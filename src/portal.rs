use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_LENGTH};
use reqwest::Client;
use reqwest::StatusCode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{
    connect_async, connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, Message},
    Connector,
};

#[derive(Clone)]
pub struct Portal {
    pub base: String,
    pub verify_ssl: bool,
    pub allowed_accounts: Vec<String>,
    client: Client,
}

#[derive(Clone, Debug)]
pub struct Candle {
    pub at_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

pub fn parse_candles(value: &Value) -> Vec<Candle> {
    let mut candles: Vec<Candle> = value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bar| {
            let at_ms = bar.get("t")?.as_i64()?;
            chrono::DateTime::<Utc>::from_timestamp_millis(at_ms)?;
            let read = |key| {
                bar.get(key)
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite() && *v > 0.0)
            };
            let open = read("o")?;
            let high = read("h")?;
            let low = read("l")?;
            let close = read("c")?;
            (high >= low && high >= open.max(close) && low <= open.min(close)).then_some(Candle {
                at_ms,
                open,
                high,
                low,
                close,
            })
        })
        .collect();
    candles.sort_by_key(|bar| bar.at_ms);
    candles.dedup_by_key(|bar| bar.at_ms);
    candles
}

impl Portal {
    pub fn from_env() -> Result<Self> {
        let base = std::env::var("IBKR_GATEWAY_URL")
            .unwrap_or_else(|_| "https://localhost:5000/v1/api".into())
            .trim_end_matches('/')
            .to_string();
        let url = url::Url::parse(&base).context("invalid IBKR_GATEWAY_URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
            || url.path().trim_end_matches('/') != "/v1/api"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("IBKR_GATEWAY_URL must be a loopback Client Portal URL");
        }
        let verify_ssl = std::env::var("IBKR_VERIFY_SSL")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        let client = gateway_client(verify_ssl)?;
        let allowed_accounts = std::env::var("IBKR_ACCOUNT_IDS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Ok(Self {
            base,
            verify_ssl,
            allowed_accounts,
            client,
        })
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        let started = Instant::now();
        crate::diagnostics::debug(format_args!("GET {path}"));
        let response = self
            .client
            .get(format!("{}/{}", self.base, path))
            .send()
            .await
            .context("Client Portal Gateway is unreachable")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let detail = gateway_error_detail(&body);
            crate::diagnostics::warn(format_args!("GET {path}: HTTP {status}"));
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                && self.brokerage_authenticated().await == Some(false)
            {
                bail!("Client Portal Gateway brokerage session is not authenticated; complete the Gateway browser login");
            }
            if detail.is_empty() {
                bail!("Client Portal {path}: HTTP {status}");
            }
            bail!("Client Portal {path}: HTTP {status}: {detail}");
        }
        crate::diagnostics::debug(format_args!(
            "GET {path}: HTTP {} in {} ms",
            response.status(),
            started.elapsed().as_millis()
        ));
        let value: Value = response
            .json()
            .await
            .context("Client Portal returned non-JSON data")?;
        if value.get("error").is_some() {
            bail!("Client Portal: {}", value["error"]);
        }
        Ok(value)
    }

    async fn brokerage_authenticated(&self) -> Option<bool> {
        let response = self
            .client
            .post(format!("{}/iserver/auth/status", self.base))
            .header(CONTENT_LENGTH, "0")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            crate::diagnostics::debug(format_args!(
                "Brokerage authentication check: HTTP {}",
                response.status()
            ));
            return None;
        }
        let authenticated = response
            .json::<Value>()
            .await
            .ok()?
            .get("authenticated")
            .and_then(Value::as_bool);
        crate::diagnostics::debug(format_args!(
            "Brokerage authentication check: {authenticated:?}"
        ));
        authenticated
    }

    pub async fn accounts(&self) -> Result<Vec<String>> {
        let value = self.get("portfolio/accounts").await?;
        let list = value
            .as_array()
            .context("Client Portal accounts response is invalid")?;
        let accounts: Vec<String> = list
            .iter()
            .filter_map(|v| {
                v.get("accountId")
                    .or_else(|| v.get("id"))
                    .and_then(Value::as_str)
            })
            .filter(|id| {
                self.allowed_accounts.is_empty() || self.allowed_accounts.iter().any(|a| a == id)
            })
            .map(str::to_string)
            .collect();
        if accounts.is_empty() {
            bail!("No selected accounts were returned by Client Portal");
        }
        let brokerage = self.get("iserver/accounts").await?;
        if !brokerage
            .get("accounts")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            bail!("Client Portal brokerage session is not authenticated; complete the Gateway browser login");
        }
        Ok(accounts)
    }

    pub async fn positions(&self, accounts: &[String]) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        for account in accounts {
            let path = format!("portfolio2/{account}/positions");
            let rows = match self.get(&path).await {
                Ok(value) => value
                    .as_array()
                    .context("Client Portal positions response is invalid")?
                    .clone(),
                Err(error) => {
                    crate::diagnostics::warn(format_args!("Uncached positions unavailable ({error}); trying paginated Client Portal positions"));
                    let mut rows = Vec::new();
                    let mut page = 0;
                    loop {
                        let value = self
                            .get(&format!("portfolio/{account}/positions/{page}"))
                            .await?;
                        let batch = value
                            .as_array()
                            .context("Client Portal positions page is invalid")?;
                        rows.extend(batch.iter().cloned());
                        if batch.len() < 100 {
                            break;
                        }
                        page += 1;
                    }
                    rows
                }
            };
            for mut item in rows {
                if item
                    .get("secType")
                    .or_else(|| item.get("assetClass"))
                    .and_then(Value::as_str)
                    == Some("STK")
                {
                    item["_account"] = Value::String(account.clone());
                    if crate::model::Position::parse(&item).is_none() {
                        bail!("Client Portal returned an invalid stock position; holdings cannot be established");
                    }
                    all.push(item);
                }
            }
        }
        Ok(all)
    }

    pub async fn trades(&self) -> Result<Vec<Value>> {
        let value = self.get("iserver/account/trades?days=7").await?;
        Ok(value
            .as_array()
            .context("Client Portal trades response is invalid")?
            .clone())
    }

    pub async fn transactions(&self, account: &str, conids: &[i64]) -> Result<Vec<Value>> {
        let path = "pa/transactions";
        let started = Instant::now();
        crate::diagnostics::debug(format_args!("POST {path} for {} contracts", conids.len()));
        let response = self
            .client
            .post(format!("{}/{}", self.base, path))
            .json(&serde_json::json!({
                "acctIds": [account],
                "conids": conids,
                "currency": "USD",
                // Request all dated transactions, not a rolling one-year window.
                "days": (Utc::now().date_naive() - NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days() + 1
            }))
            .send()
            .await
            .context("Client Portal Gateway is unreachable")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let detail = gateway_error_detail(&body);
            crate::diagnostics::warn(format_args!("POST {path}: HTTP {status}"));
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                && self.brokerage_authenticated().await == Some(false)
            {
                bail!("Client Portal Gateway brokerage session is not authenticated; complete the Gateway browser login");
            }
            bail!("Client Portal {path}: HTTP {status}: {detail}");
        }
        crate::diagnostics::debug(format_args!(
            "POST {path}: HTTP {status} in {} ms",
            started.elapsed().as_millis()
        ));
        let value: Value = response
            .json()
            .await
            .context("Client Portal returned non-JSON transaction data")?;
        if value.get("error").is_some() {
            bail!("Client Portal: {}", value["error"]);
        }
        Ok(value
            .get("transactions")
            .and_then(Value::as_array)
            .context("Client Portal transaction history response is invalid")?
            .clone())
    }

    pub async fn history(&self, conid: i64) -> Result<Vec<Candle>> {
        let value = self
            .get(&format!(
                "iserver/marketdata/history?conid={conid}&period=1w&bar=15min&outsideRTH=false&source=trades"
            ))
            .await?;
        Ok(parse_candles(&value))
    }

    pub async fn regular_close(
        &self,
        conid: i64,
        cycle_date: NaiveDate,
    ) -> Result<Option<(NaiveDate, f64)>> {
        let value = self
            .get(&format!(
                "iserver/marketdata/history?conid={conid}&period=2w&bar=1d&outsideRTH=false&source=trades"
            ))
            .await?;
        Ok(crate::market::latest_regular_close(&value, cycle_date))
    }

    fn ws_url(&self) -> String {
        format!(
            "{}/ws",
            self.base
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1)
        )
    }

    pub async fn stream(
        self,
        mut subscriptions: watch::Receiver<Vec<i64>>,
        tx: mpsc::Sender<Value>,
    ) {
        let mut backoff = 1;
        loop {
            let url = self.ws_url();
            crate::diagnostics::info(format_args!("Connecting Client Portal WebSocket"));
            let session = async {
                let value: Value = self
                    .client
                    .post(format!("{}/tickle", self.base))
                    .header(CONTENT_LENGTH, "0")
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let token = value
                    .get("session")
                    .and_then(Value::as_str)
                    .context("Client Portal did not return a WebSocket session")?;
                let mut request = url.as_str().into_client_request()?;
                request
                    .headers_mut()
                    .insert("Cookie", format!("api={token}").parse()?);
                request.headers_mut().insert(
                    "Origin",
                    url::Url::parse(&self.base)?
                        .origin()
                        .ascii_serialization()
                        .parse()?,
                );
                request
                    .headers_mut()
                    .insert("User-Agent", "ibkr-companion/0.1".parse()?);
                Ok::<_, anyhow::Error>(request)
            }
            .await;
            let request = match session {
                Ok(request) => request,
                Err(error) => {
                    crate::diagnostics::warn(format_args!(
                        "WebSocket session unavailable: {error}"
                    ));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }
            };
            let connected = if url.starts_with("wss:") && !self.verify_ssl {
                let config = ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(LoopbackCertVerifier))
                    .with_no_client_auth();
                connect_async_tls_with_config(
                    request,
                    None,
                    false,
                    Some(Connector::Rustls(Arc::new(config))),
                )
                .await
            } else {
                connect_async(request).await
            };
            match connected {
                Ok((socket, _)) => {
                    crate::diagnostics::info(format_args!(
                        "Client Portal WebSocket transport connected"
                    ));
                    backoff = 1;
                    let (mut writer, mut reader) = socket.split();
                    // The Gateway ignores subscriptions sent before its authenticated sts event.
                    let ready = tokio::time::timeout(Duration::from_secs(15), async {
                        while let Some(message) = reader.next().await {
                            match message? {
                                message @ (Message::Text(_) | Message::Binary(_)) => {
                                    let value = decode_stream_message(&message)
                                        .context("Invalid WebSocket JSON during authentication")?;
                                    crate::diagnostics::debug(format_args!(
                                        "WebSocket initialization topic: {}",
                                        value
                                            .get("topic")
                                            .and_then(Value::as_str)
                                            .unwrap_or("unknown")
                                    ));
                                    if value.get("topic").and_then(Value::as_str) == Some("sts") {
                                        if value
                                            .pointer("/args/authenticated")
                                            .and_then(Value::as_bool)
                                            == Some(true)
                                        {
                                            return Ok::<_, anyhow::Error>(());
                                        }
                                        bail!("Gateway WebSocket is not authenticated");
                                    }
                                }
                                Message::Ping(data) => writer.send(Message::Pong(data)).await?,
                                Message::Close(_) => {
                                    bail!("Gateway closed WebSocket during authentication")
                                }
                                _ => {}
                            }
                        }
                        bail!("Gateway ended WebSocket during authentication")
                    })
                    .await;
                    if !matches!(ready, Ok(Ok(()))) {
                        crate::diagnostics::warn(format_args!(
                            "WebSocket authentication failed or timed out"
                        ));
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    crate::diagnostics::info(format_args!("Client Portal WebSocket authenticated"));
                    if let Err(error) = writer
                        .send(Message::Text(
                            "str+{\"realtimeUpdatesOnly\":true,\"days\":1}".into(),
                        ))
                        .await
                    {
                        crate::diagnostics::warn(format_args!(
                            "Trade stream subscription failed: {error}"
                        ));
                        continue;
                    }
                    let mut current = Vec::<String>::new();
                    let wanted = subscriptions.borrow().clone();
                    if let Err(error) = sync_subscriptions(
                        &mut writer,
                        &mut current,
                        &wanted,
                        crate::market::overnight_subscription_active(Utc::now()),
                    )
                    .await
                    {
                        crate::diagnostics::warn(format_args!(
                            "Market data subscription failed: {error}"
                        ));
                        continue;
                    }
                    if tx
                        .send(serde_json::json!({"_ws": "connected"}))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let mut renew = tokio::time::interval_at(
                        tokio::time::Instant::now() + Duration::from_secs(9 * 60),
                        Duration::from_secs(9 * 60),
                    );
                    let mut source_timer = tokio::time::interval_at(
                        tokio::time::Instant::now() + Duration::from_secs(30),
                        Duration::from_secs(30),
                    );
                    'connected: loop {
                        tokio::select! {
                            message = reader.next() => match message {
                                Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                                    if let Some(value) = decode_stream_message(&message) {
                                        if value.get("topic").and_then(Value::as_str) == Some("sts")
                                            && value.pointer("/args/authenticated").and_then(Value::as_bool) == Some(false) {
                                            crate::diagnostics::warn(format_args!("WebSocket authentication expired"));
                                            break;
                                        }
                                        if tx.send(value).await.is_err() { return; }
                                    }
                                }
                                Some(Ok(Message::Ping(data))) => { if let Err(error) = writer.send(Message::Pong(data)).await {
                                    crate::diagnostics::warn(format_args!("WebSocket pong failed: {error}")); break;
                                } }
                                Some(Ok(Message::Close(frame))) => {
                                    crate::diagnostics::info(format_args!("Client Portal WebSocket closed: {frame:?}")); break;
                                }
                                Some(Err(error)) => {
                                    crate::diagnostics::warn(format_args!("Client Portal WebSocket read failed: {error}")); break;
                                }
                                None => {
                                    crate::diagnostics::info(format_args!("Client Portal WebSocket stream ended")); break;
                                }
                                _ => {}
                            },
                            changed = subscriptions.changed() => {
                                if changed.is_err() { return; }
                                let wanted = subscriptions.borrow().clone();
                                if let Err(error) = sync_subscriptions(&mut writer, &mut current, &wanted, crate::market::overnight_subscription_active(Utc::now())).await {
                                    crate::diagnostics::warn(format_args!("Market data subscription update failed: {error}")); break;
                                }
                            }
                            _ = source_timer.tick() => {
                                let wanted = subscriptions.borrow().clone();
                                if let Err(error) = sync_subscriptions(&mut writer, &mut current, &wanted, crate::market::overnight_subscription_active(Utc::now())).await {
                                    crate::diagnostics::warn(format_args!("Market data session switch failed: {error}")); break;
                                }
                            }
                            _ = renew.tick() => {
                                for target in &current {
                                    if let Err(error) = writer.send(subscription_message(target)).await {
                                        crate::diagnostics::warn(format_args!("Market data subscription renewal failed: {error}")); break 'connected;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(error) => crate::diagnostics::warn(format_args!(
                    "Client Portal WebSocket connection failed: {error}"
                )),
            }
            if tx
                .send(serde_json::json!({"_ws": "reconnecting"}))
                .await
                .is_err()
            {
                return;
            }
            crate::diagnostics::info(format_args!("WebSocket reconnecting in {backoff} s"));
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    }
}

fn gateway_client(verify_ssl: bool) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    Ok(Client::builder()
        .user_agent(concat!("ibkr-companion/", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .danger_accept_invalid_certs(!verify_ssl)
        .timeout(Duration::from_secs(12))
        .build()?)
}

fn gateway_error_detail(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| {
            ["error", "message", "detail"]
                .into_iter()
                .filter_map(|key| value.get(key))
                .find_map(Value::as_str)
        })
        .unwrap_or_else(|| {
            if parsed.is_some() || body.trim_start().starts_with('<') {
                ""
            } else {
                body
            }
        });
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(240).collect()
}

fn decode_stream_message(message: &Message) -> Option<Value> {
    match message {
        Message::Text(body) => serde_json::from_str(body).ok(),
        Message::Binary(body) => serde_json::from_slice(body).ok(),
        _ => None,
    }
}

fn subscription_message(target: &str) -> Message {
    let fields = if target.ends_with("@OVERNIGHT") {
        "[\"31\",\"6509\"]"
    } else {
        "[\"31\",\"6509\",\"7296\",\"7741\"]"
    };
    Message::Text(format!("smd+{target}+{{\"fields\":{fields}}}").into())
}

async fn sync_subscriptions<S>(
    writer: &mut S,
    current: &mut Vec<String>,
    wanted: &[i64],
    overnight: bool,
) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let targets: Vec<String> = wanted
        .iter()
        .map(ToString::to_string)
        .chain(
            wanted
                .iter()
                .filter(|_| overnight)
                .map(|conid| format!("{conid}@OVERNIGHT")),
        )
        .collect();
    let removed = current.iter().filter(|id| !targets.contains(id)).count();
    let added = targets.iter().filter(|id| !current.contains(id)).count();
    for target in current.iter().filter(|id| !targets.contains(id)) {
        writer
            .send(Message::Text(format!("umd+{target}+{{}}").into()))
            .await?;
    }
    for target in targets.iter().filter(|id| !current.contains(id)) {
        writer.send(subscription_message(target)).await?;
    }
    *current = targets;
    if added > 0 || removed > 0 {
        crate::diagnostics::info(format_args!(
            "Market data subscriptions: +{added} -{removed}, {} active",
            current.len()
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct LoopbackCertVerifier;

impl ServerCertVerifier for LoopbackCertVerifier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
        ]
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stream_accepts_gateway_binary_json_and_text_json() {
        let payload =
            r#"{"topic":"smd+1@OVERNIGHT","conid":1,"31":"101.25","6509":"RP","_updated":1000}"#;
        let text = super::decode_stream_message(&super::Message::Text(payload.into())).unwrap();
        let binary = super::decode_stream_message(&super::Message::Binary(
            payload.as_bytes().to_vec().into(),
        ))
        .unwrap();
        assert_eq!(text, binary);
        assert_eq!(
            crate::model::Quote::parse(&binary, None).unwrap().1.price,
            101.25
        );
    }
    use super::*;

    #[test]
    fn history_parser_keeps_valid_ohlc_and_orders_by_time() {
        let data = serde_json::json!({"data": [
            {"t": 1790775900000_i64, "o": 101.0, "h": 104.0, "l": 100.0, "c": 103.0},
            {"t": 1790775000000_i64, "o": 100.0, "h": 102.0, "l": 99.0, "c": 101.0},
            {"t": 1790776800000_i64, "o": 103.0, "h": 102.0, "l": 99.0, "c": 100.0}
        ]});
        let bars = parse_candles(&data);
        assert_eq!(bars.len(), 2);
        assert!(bars[0].at_ms < bars[1].at_ms);
        assert_eq!(
            (bars[0].open, bars[0].high, bars[0].low, bars[0].close),
            (100.0, 102.0, 99.0, 101.0)
        );
    }
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[tokio::test]
    async fn gateway_requests_send_required_headers_and_explain_expired_login() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = 0;
            let mut headers_ok = true;
            while requests < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut buffer = [0_u8; 2048];
                        let size = stream.read(&mut buffer).unwrap();
                        let request = String::from_utf8_lossy(&buffer[..size]);
                        let lower = request.to_ascii_lowercase();
                        headers_ok &= lower.contains("user-agent: ibkr-companion/")
                            && lower.contains("accept: */*");
                        if request.starts_with("POST ") {
                            headers_ok &= lower.contains("content-length: 0");
                        }
                        let (status, body) =
                            if request.starts_with("GET /v1/api/portfolio/accounts ") {
                                ("403 Forbidden", r#"{"error":"access denied"}"#)
                            } else if request.starts_with("POST /v1/api/iserver/auth/status ") {
                                ("200 OK", r#"{"authenticated":false}"#)
                            } else {
                                panic!("unexpected Gateway request: {request}");
                            };
                        let reply = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(reply.as_bytes()).unwrap();
                        requests += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock Gateway failed: {error}"),
                }
            }
            (requests, headers_ok)
        });
        let base = format!("http://localhost:{port}/v1/api");
        let client = gateway_client(false).unwrap();
        let portal = Portal {
            base,
            verify_ssl: false,
            allowed_accounts: Vec::new(),
            client,
        };
        let error = portal.get("portfolio/accounts").await.unwrap_err();
        assert!(error
            .to_string()
            .contains("brokerage session is not authenticated"));
        assert_eq!(server.join().unwrap(), (2, true));
    }
}
