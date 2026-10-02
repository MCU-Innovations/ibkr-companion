use crate::backend_settings::{self, BackendSettings, Provider};
use chrono::Utc;
use reqwest::{header, Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

const FRESH_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_PARALLEL: usize = 4;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct RateLedger {
    yahoo_retry_at: i64,
    fmp_retry_at: i64,
    fmp_calls: VecDeque<i64>,
}

#[derive(Default)]
struct RateState {
    yahoo_starts: VecDeque<tokio::time::Instant>,
    fmp_starts: VecDeque<tokio::time::Instant>,
    ledger: RateLedger,
    path: Option<PathBuf>,
    fmp_paused: bool,
}

#[derive(Clone, Default)]
struct RateLimiter(Arc<Mutex<RateState>>);

impl RateLimiter {
    async fn configure(&self, path: PathBuf) -> anyhow::Result<()> {
        let ledger = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RateLedger::default(),
            Err(error) => return Err(error.into()),
        };
        let mut state = self.0.lock().await;
        state.ledger = ledger;
        state.path = Some(path);
        Ok(())
    }

    async fn acquire(&self, provider: Provider) -> Result<(), Failure> {
        loop {
            let mut state = self.0.lock().await;
            let instant = tokio::time::Instant::now();
            let now = Utc::now().timestamp_millis();
            if provider == Provider::Fmp && state.fmp_paused {
                return Err(Failure {
                    pause: true,
                    ..Failure::new("FMP access is paused; check the API key and plan")
                });
            }
            let retry_at = if provider == Provider::Yahoo {
                state.ledger.yahoo_retry_at
            } else {
                state.ledger.fmp_retry_at
            };
            let mut wait = Duration::from_millis(retry_at.saturating_sub(now).max(0) as u64);
            // Yahoo's unofficial service has no fixed published quota. Use a
            // conservative shared ceiling and honor every server cooldown.
            let per_minute = if provider == Provider::Yahoo { 120 } else { 60 };
            let starts = if provider == Provider::Yahoo {
                &mut state.yahoo_starts
            } else {
                &mut state.fmp_starts
            };
            while starts
                .front()
                .is_some_and(|start| instant.duration_since(*start) >= Duration::from_secs(60))
            {
                starts.pop_front();
            }
            if starts.len() >= per_minute {
                wait = wait.max(
                    Duration::from_secs(60)
                        .saturating_sub(instant.duration_since(*starts.front().unwrap())),
                );
            }
            if starts.len() >= 4 {
                let fourth_last = starts[starts.len() - 4];
                wait = wait.max(
                    Duration::from_secs(1).saturating_sub(instant.duration_since(fourth_last)),
                );
            }
            if provider == Provider::Fmp {
                while state
                    .ledger
                    .fmp_calls
                    .front()
                    .is_some_and(|at| now.saturating_sub(*at) >= FRESH_MS)
                {
                    state.ledger.fmp_calls.pop_front();
                }
                // Keep the free-plan budget across restarts, using a rolling
                // 24-hour window so a midnight reset cannot double the allowance.
                if state.ledger.fmp_calls.len() >= 250 {
                    let millis = state.ledger.fmp_calls[0]
                        .saturating_add(FRESH_MS)
                        .saturating_sub(now)
                        .max(0) as u64;
                    return Err(Failure {
                        message: "FMP daily request budget exhausted; using cached fundamentals"
                            .into(),
                        retry_seconds: millis.div_ceil(1000),
                        global: true,
                        pause: false,
                    });
                }
            }
            if !wait.is_zero() {
                drop(state);
                tokio::time::sleep(wait).await;
                continue;
            }
            if provider == Provider::Yahoo {
                state.yahoo_starts.push_back(instant);
            } else {
                state.ledger.fmp_calls.push_back(now);
                persist_ledger(&state)
                    .await
                    .map_err(|_| Failure::new("Could not save FMP API usage budget"))?;
                state.fmp_starts.push_back(tokio::time::Instant::now());
            }
            return Ok(());
        }
    }

    async fn failed(&self, provider: Provider, error: &Failure) {
        let mut state = self.0.lock().await;
        if error.pause && provider == Provider::Fmp {
            state.fmp_paused = true;
        }
        if error.global {
            let retry_at = Utc::now().timestamp_millis().saturating_add(
                error
                    .retry_seconds
                    .saturating_mul(1000)
                    .min(i64::MAX as u64) as i64,
            );
            let retry = if provider == Provider::Yahoo {
                &mut state.ledger.yahoo_retry_at
            } else {
                &mut state.ledger.fmp_retry_at
            };
            *retry = (*retry).max(retry_at);
            if persist_ledger(&state).await.is_err() {
                crate::diagnostics::warn(format_args!(
                    "Could not persist fundamentals rate-limit cooldown"
                ));
            }
        }
    }
}

async fn persist_ledger(state: &RateState) -> anyhow::Result<()> {
    if let Some(path) = &state.path {
        write_cache_file(path, &state.ledger).await?;
    }
    Ok(())
}

async fn write_cache_file(path: &std::path::Path, value: &impl Serialize) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, serde_json::to_vec(value)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub symbol: String,
    pub currency: String,
    pub market_cap: Option<f64>,
    pub sector: String,
    pub industry: String,
    pub updated_ms: i64,
}

impl Profile {
    pub fn fresh(&self, now: i64) -> bool {
        self.updated_ms <= now && now - self.updated_ms < FRESH_MS
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub symbol: String,
    pub ticker: String,
}

// Keys are kept out of Debug output, request URLs, and UI view snapshots.
pub enum Command {
    Demand(Vec<Request>),
    Save {
        provider: Provider,
        enabled: bool,
        replacement_key: Option<String>,
        clear_key: bool,
    },
    Refresh,
}

pub enum Event {
    Settings {
        provider: i32,
        enabled: bool,
        key_set: bool,
    },
    Profile(Profile),
    Reset,
    Status(String),
}

#[derive(Clone, Debug)]
struct Failure {
    message: String,
    retry_seconds: u64,
    pause: bool,
    global: bool,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_seconds: 15 * 60,
            pause: false,
            global: false,
        }
    }
}

#[derive(Default)]
struct YahooSession {
    cookies: BTreeMap<String, String>,
    crumb: String,
}

#[derive(Clone)]
pub struct FundamentalsClient {
    http: Client,
    yahoo: Arc<Mutex<YahooSession>>,
    yahoo_base: String,
    crumb_url: String,
    cookie_url: String,
    fmp_url: String,
    limiter: RateLimiter,
}

impl FundamentalsClient {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .build()?,
            yahoo: Arc::new(Mutex::new(YahooSession::default())),
            yahoo_base: "https://query2.finance.yahoo.com/v10/finance/quoteSummary/".into(),
            crumb_url: "https://query1.finance.yahoo.com/v1/test/getcrumb".into(),
            cookie_url: "https://fc.yahoo.com".into(),
            fmp_url: "https://financialmodelingprep.com/stable/profile".into(),
            limiter: RateLimiter::default(),
        })
    }

    async fn bootstrap(&self, session: &mut YahooSession) -> Result<(), Failure> {
        self.limiter.acquire(Provider::Yahoo).await?;
        let cookie = self
            .http
            .get(&self.cookie_url)
            .send()
            .await
            .map_err(|_| Failure::new("Yahoo Finance connection failed"))?;
        // fc.yahoo.com intentionally returns 404 while setting its session cookie.
        if cookie.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(http_failure(&cookie, Provider::Yahoo));
        }
        collect_cookies(session, &cookie);
        self.limiter.acquire(Provider::Yahoo).await?;
        let response = self
            .http
            .get(&self.crumb_url)
            .header(header::COOKIE, cookie_header(session))
            .send()
            .await
            .map_err(|_| Failure::new("Yahoo Finance session request failed"))?;
        if !response.status().is_success() {
            return Err(http_failure(&response, Provider::Yahoo));
        }
        collect_cookies(session, &response);
        let crumb = response
            .text()
            .await
            .map_err(|_| Failure::new("Yahoo Finance session response unreadable"))?;
        let crumb = crumb.trim();
        if crumb.is_empty()
            || crumb.len() > 256
            || crumb.contains('<')
            || crumb.contains("Too Many")
            || crumb.chars().any(char::is_whitespace)
        {
            return Err(Failure::new(
                "Yahoo Finance did not provide a valid session",
            ));
        }
        session.crumb = crumb.to_string();
        Ok(())
    }

    async fn fetch(
        &self,
        provider: Provider,
        request: &Request,
        key: &str,
    ) -> Result<Profile, Failure> {
        let result = self.fetch_inner(provider, request, key).await;
        if let Err(error) = &result {
            self.limiter.failed(provider, error).await;
        }
        result
    }

    async fn fetch_inner(
        &self,
        provider: Provider,
        request: &Request,
        key: &str,
    ) -> Result<Profile, Failure> {
        if provider == Provider::Fmp {
            let mut token = header::HeaderValue::from_str(key)
                .map_err(|_| Failure::new("FMP API key is invalid"))?;
            token.set_sensitive(true);
            self.limiter.acquire(provider).await?;
            let response = self
                .http
                .get(&self.fmp_url)
                .query(&[("symbol", &request.ticker)])
                .header("apikey", token)
                .send()
                .await
                .map_err(|_| Failure::new("FMP connection failed"))?;
            if !response.status().is_success() {
                return Err(http_failure(&response, provider));
            }
            let data: Value = response
                .json()
                .await
                .map_err(|_| Failure::new("FMP returned an unreadable profile"))?;
            return parse_fmp(&data, request, Utc::now().timestamp_millis());
        }
        for attempt in 0..2 {
            let (cookies, crumb) = {
                let mut session = self.yahoo.lock().await;
                if session.crumb.is_empty() {
                    self.bootstrap(&mut session).await?;
                }
                (cookie_header(&session), session.crumb.clone())
            };
            let mut url = reqwest::Url::parse(&self.yahoo_base)
                .map_err(|_| Failure::new("Invalid Yahoo Finance URL"))?;
            url.path_segments_mut()
                .map_err(|_| Failure::new("Invalid Yahoo Finance URL"))?
                .pop_if_empty()
                .push(&request.ticker);
            self.limiter.acquire(provider).await?;
            let response = self
                .http
                .get(url)
                .header(header::COOKIE, cookies)
                .query(&[
                    ("modules", "price,assetProfile,summaryDetail"),
                    ("formatted", "false"),
                    ("crumb", &crumb),
                ])
                .send()
                .await
                .map_err(|_| Failure::new("Yahoo Finance connection failed"))?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                let mut session = self.yahoo.lock().await;
                // Another parallel request may already have renewed this session.
                if session.crumb == crumb {
                    *session = YahooSession::default();
                }
                continue;
            }
            if !response.status().is_success() {
                return Err(http_failure(&response, provider));
            }
            let data: Value = response
                .json()
                .await
                .map_err(|_| Failure::new("Yahoo Finance returned an unreadable profile"))?;
            return parse_yahoo(&data, request, Utc::now().timestamp_millis());
        }
        Err(Failure::new("Yahoo Finance session expired"))
    }

    pub async fn check_yahoo(&self, symbol: &str) -> anyhow::Result<Profile> {
        self.fetch(
            Provider::Yahoo,
            &Request {
                symbol: symbol.into(),
                ticker: yahoo_ticker(symbol),
            },
            "",
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.message))
    }
}

pub fn yahoo_ticker(symbol: &str) -> String {
    // Yahoo uses hyphens for US share classes; preserve exchange suffixes.
    let symbol = symbol.trim().to_uppercase();
    if symbol.ends_with(".A") || symbol.ends_with(".B") {
        symbol.replace('.', "-")
    } else {
        symbol.replace(' ', "-")
    }
}

fn collect_cookies(session: &mut YahooSession, response: &Response) {
    for cookie in response.headers().get_all(header::SET_COOKIE) {
        if let Ok(cookie) = cookie.to_str() {
            if let Some((name, value)) = cookie
                .split(';')
                .next()
                .and_then(|item| item.split_once('='))
            {
                session.cookies.insert(name.to_string(), value.to_string());
            }
        }
    }
}
fn cookie_header(session: &YahooSession) -> String {
    session
        .cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}
fn http_failure(response: &Response, provider: Provider) -> Failure {
    let status = response.status();
    let mut error = Failure::new(format!(
        "{} request failed (HTTP {})",
        provider.name(),
        status.as_u16()
    ));
    match status.as_u16() {
        401 | 402 | 403 if provider == Provider::Fmp => {
            error.message =
                "FMP rejected access; check the API key and endpoint access for your plan".into();
            error.pause = true;
        }
        429 => {
            error.message = format!(
                "{} rate limit reached; waiting before retrying",
                provider.name()
            );
            error.retry_seconds = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(retry_after_seconds)
                .unwrap_or(300)
                .max(60);
            error.global = true;
        }
        404 => {
            error.retry_seconds = 24 * 3600;
        }
        401 | 403 | 500..=599 => {
            error.retry_seconds = 300;
            error.global = true;
        }
        _ => {}
    }
    error
}

fn retry_after_seconds(value: &str) -> Option<u64> {
    value.trim().parse().ok().or_else(|| {
        let deadline = chrono::DateTime::parse_from_rfc2822(value).ok()?;
        let ms = deadline
            .timestamp_millis()
            .saturating_sub(Utc::now().timestamp_millis())
            .max(0) as u64;
        Some(ms.div_ceil(1000))
    })
}
fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    let value = value.get("raw").unwrap_or(value);
    value
        .as_f64()
        .or_else(|| value.as_str()?.replace(',', "").parse().ok())
        .filter(|n| n.is_finite() && *n > 0.0 && *n <= f32::MAX as f64)
}
fn string(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}
fn parse_yahoo(data: &Value, request: &Request, now: i64) -> Result<Profile, Failure> {
    let row = data
        .pointer("/quoteSummary/result/0")
        .ok_or_else(|| Failure::new("Yahoo Finance has no profile for this symbol"))?;
    let price = &row["price"];
    if !string(price, "symbol").eq_ignore_ascii_case(&request.ticker) {
        return Err(Failure::new("Yahoo Finance returned a different symbol"));
    }
    let profile = Profile {
        symbol: request.symbol.clone(),
        currency: string(price, "currency").to_uppercase(),
        market_cap: number(price.get("marketCap"))
            .or_else(|| number(row["summaryDetail"].get("marketCap"))),
        sector: string(&row["assetProfile"], "sector"),
        industry: string(&row["assetProfile"], "industry"),
        updated_ms: now,
    };
    if profile.market_cap.is_none() && profile.sector.is_empty() && profile.industry.is_empty() {
        return Err(Failure::new(
            "Yahoo Finance has no fundamentals for this symbol",
        ));
    }
    Ok(profile)
}
fn parse_fmp(data: &Value, request: &Request, now: i64) -> Result<Profile, Failure> {
    if data.get("Error Message").is_some() || data.get("error").is_some() {
        return Err(Failure {
            message: "FMP rejected access; check the API key and endpoint access for your plan"
                .into(),
            pause: true,
            ..Failure::new("")
        });
    }
    let row = data
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| string(row, "symbol").eq_ignore_ascii_case(&request.ticker))
        })
        .ok_or_else(|| Failure::new("FMP has no profile for this symbol"))?;
    Ok(Profile {
        symbol: request.symbol.clone(),
        currency: string(row, "currency").to_uppercase(),
        market_cap: number(row.get("marketCap")),
        sector: string(row, "sector"),
        industry: string(row, "industry"),
        updated_ms: now,
    })
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Cache {
    yahoo: HashMap<String, Profile>,
    fmp: HashMap<String, Profile>,
    yahoo_retry: HashMap<String, i64>,
    fmp_retry: HashMap<String, i64>,
}
impl Cache {
    fn retry(&self, provider: Provider) -> &HashMap<String, i64> {
        if provider == Provider::Yahoo {
            &self.yahoo_retry
        } else {
            &self.fmp_retry
        }
    }
    fn retry_mut(&mut self, provider: Provider) -> &mut HashMap<String, i64> {
        if provider == Provider::Yahoo {
            &mut self.yahoo_retry
        } else {
            &mut self.fmp_retry
        }
    }
    fn profiles(&self, provider: Provider) -> &HashMap<String, Profile> {
        if provider == Provider::Yahoo {
            &self.yahoo
        } else {
            &self.fmp
        }
    }
    fn profiles_mut(&mut self, provider: Provider) -> &mut HashMap<String, Profile> {
        if provider == Provider::Yahoo {
            &mut self.yahoo
        } else {
            &mut self.fmp
        }
    }
}

pub async fn run(
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    settings_path: PathBuf,
) {
    let client = match FundamentalsClient::new() {
        Ok(client) => client,
        Err(_) => {
            let _ = events.send(Event::Status(
                "Could not initialize fundamentals backend".into(),
            ));
            return;
        }
    };
    run_with_client(commands, events, settings_path, client).await;
}

async fn run_with_client(
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    settings_path: PathBuf,
    client: FundamentalsClient,
) {
    let mut settings = match backend_settings::load(&settings_path).await {
        Ok(settings) => settings,
        Err(_) => {
            let _ = events.send(Event::Status(
                "Could not load backend settings; using Yahoo Finance defaults".into(),
            ));
            BackendSettings::default()
        }
    };
    let cache_path = settings_path.with_file_name("fundamentals-cache.json");
    let mut cache: Cache = tokio::fs::read(&cache_path)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if client
        .limiter
        .configure(settings_path.with_file_name("fundamentals-rate-limits.json"))
        .await
        .is_err()
    {
        let _ = events.send(Event::Status(
            "Could not load fundamentals rate limits; fetching paused".into(),
        ));
        return;
    }
    publish_settings(&events, &settings);
    for profile in cache.profiles(settings.provider).values() {
        let _ = events.send(Event::Profile(profile.clone()));
    }
    let mut desired: Vec<Request> = Vec::new();
    let mut attempts = cache.retry(settings.provider).clone();
    let mut paused = false;
    let mut cooldown = 0_i64;
    let mut generation = 0_u64;
    let mut missing_key_reported = false;
    let (result_tx, mut result_rx) =
        mpsc::unbounded_channel::<(u64, String, Result<Profile, Failure>)>();
    let mut pending: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(Command::Demand(requests)) => { desired = requests; }
                Some(Command::Refresh) => {
                    // Manual refresh must not bypass the server's rate-limit cooldown.
                    attempts.retain(|_, retry_at| Utc::now().timestamp_millis() < *retry_at);

                    let _ = events.send(Event::Status(format!("{} daily cache checked; only missing or expired profiles will load", settings.provider.name())));
                }
                Some(Command::Save { provider, enabled, replacement_key, clear_key }) => {
                    let mut next = settings.clone(); next.provider = provider; next.enabled = enabled;
                    if clear_key { next.api_keys.remove("fmp"); }
                    if let Some(key) = replacement_key {
                        let key = key.trim().to_string();
                        if !key.is_empty() {
                            if header::HeaderValue::from_str(&key).is_err() {
                                let _ = events.send(Event::Status("Enter a valid API key".into())); continue;
                            }
                            next.api_keys.insert("fmp".into(), key);
                        }
                    }
                    match backend_settings::save(&settings_path, &next).await {
                        Ok(()) => {
                            for (_, task) in pending.drain() { task.abort(); }
                            generation += 1;
                            missing_key_reported = false;
                            let access_changed = settings.provider != next.provider || settings.api_keys != next.api_keys;
                            if access_changed { paused = false; client.limiter.0.lock().await.fmp_paused = false; }
                            attempts = cache.retry(next.provider).clone();
                            settings = next;
                            let _ = events.send(Event::Reset);
                            publish_settings(&events, &settings);
                            for profile in cache.profiles(settings.provider).values() { let _ = events.send(Event::Profile(profile.clone())); }
                            let _ = events.send(Event::Status(if settings.enabled { format!("Settings saved · {} ready", settings.provider.name()) } else { "Settings saved · fundamentals disabled".into() }));
                        }
                        Err(_) => { let _ = events.send(Event::Status("Could not save backend settings; previous settings remain active".into())); }
                    }
                }
                None => break,
            },
            result = result_rx.recv() => {
                if let Some((result_generation, symbol, result)) = result {
                    if result_generation != generation { continue; }
                    pending.remove(&symbol);
                    match result {
                        Ok(profile) => {
                            cache.profiles_mut(settings.provider).insert(symbol.clone(), profile.clone());
                            attempts.remove(&symbol); cache.retry_mut(settings.provider).remove(&symbol);
                            let _ = events.send(Event::Profile(profile));
                            let count = desired.iter().filter(|r| cache.profiles(settings.provider).get(&r.symbol).is_some_and(|p| p.fresh(Utc::now().timestamp_millis()))).count();
                            let _ = events.send(Event::Status(format!("{} · {count}/{} profiles ready", settings.provider.name(), desired.len())));

                        }
                        Err(error) => {
                            let retry_ms = error.retry_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64;
                            let retry_at = Utc::now().timestamp_millis().saturating_add(retry_ms);
                            attempts.insert(symbol.clone(), retry_at);
                            cache.retry_mut(settings.provider).insert(symbol.clone(), retry_at);
                            if error.global { cooldown = cooldown.max(retry_at); }
                            paused |= error.pause;
                            let _ = events.send(Event::Status(format!("{symbol}: {}", error.message)));
                        }
                    }
                    if write_cache_file(&cache_path, &cache).await.is_err() {
                        let _ = events.send(Event::Status("Fundamentals loaded, but daily cache could not be saved".into()));
                    }
                }
            },
            _ = ticker.tick() => {
                let now = Utc::now().timestamp_millis();
                if pending.len() >= MAX_PARALLEL || !settings.enabled || paused || now < cooldown { continue; }
                let key = settings.api_keys.get("fmp").cloned().unwrap_or_default();
                if settings.provider == Provider::Fmp && key.is_empty() {
                    if !desired.is_empty() && !missing_key_reported {
                        let _ = events.send(Event::Status("Add an FMP API key in Settings, or select Yahoo Finance".into()));
                        missing_key_reported = true;
                    }
                    continue;
                }
                while pending.len() < MAX_PARALLEL {
                let Some(request) = desired.iter().find(|r| {
                    !pending.contains_key(&r.symbol) && !cache.profiles(settings.provider).get(&r.symbol).is_some_and(|p| p.fresh(now))
                        && !attempts.get(&r.symbol).is_some_and(|retry| now < *retry)
                }) else { break; };
                    let mut request = request.clone();
                    if let Some(alias) = settings.symbol_aliases.get(&request.symbol) { request.ticker = alias.clone(); }
                    else if settings.provider == Provider::Yahoo { request.ticker = yahoo_ticker(&request.ticker); }
                    let client = client.clone(); let tx = result_tx.clone(); let provider = settings.provider;
                    let _ = events.send(Event::Status(format!("{} · loading {}", provider.name(), request.symbol)));
                    let symbol = request.symbol.clone();
                    let key = key.clone();
                    pending.insert(symbol, tokio::spawn(async move {
                        let result = client.fetch(provider, &request, &key).await;
                        let _ = tx.send((generation, request.symbol, result));
                    }));
                }
            }
        }
    }
    for (_, task) in pending {
        task.abort();
    }
}

fn publish_settings(events: &mpsc::UnboundedSender<Event>, settings: &BackendSettings) {
    let _ = events.send(Event::Settings {
        provider: settings.provider.index(),
        enabled: settings.enabled,
        key_set: settings
            .api_keys
            .get("fmp")
            .is_some_and(|key| !key.is_empty()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn yahoo_parses_raw_cap_currency_and_classification() {
        let data = serde_json::json!({"quoteSummary":{"result":[{"price":{"symbol":"AAPL","currency":"USD","marketCap":{"raw":3500000000000_u64}},"assetProfile":{"sector":"Technology","industry":"Consumer Electronics"}}],"error":null}});
        let profile = parse_yahoo(
            &data,
            &Request {
                symbol: "AAPL".into(),
                ticker: "AAPL".into(),
            },
            100,
        )
        .unwrap();
        assert_eq!(profile.market_cap, Some(3500000000000.0));
        assert_eq!(profile.sector, "Technology");
        assert_eq!(profile.currency, "USD");
        assert!(profile.fresh(101));
        assert!(!profile.fresh(100 + FRESH_MS));
        assert!(!profile.fresh(99));
    }
    #[test]
    fn missing_or_mismatched_profiles_are_not_invented() {
        let req = Request {
            symbol: "AAPL".into(),
            ticker: "AAPL".into(),
        };
        assert!(parse_yahoo(
            &serde_json::json!({"quoteSummary":{"result":null}}),
            &req,
            0
        )
        .is_err());
        assert!(parse_yahoo(&serde_json::json!({"quoteSummary":{"result":[{"price":{"symbol":"MSFT","marketCap":123}}]}}), &req, 0).is_err());
        assert_eq!(number(Some(&serde_json::json!(-10))), None);
        assert!(
            parse_fmp(&serde_json::json!({"Error Message":"invalid key"}), &req, 0)
                .unwrap_err()
                .pause
        );
    }
    #[test]
    fn share_class_translation_preserves_international_suffixes() {
        assert_eq!(yahoo_ticker("BRK.B"), "BRK-B");
        assert_eq!(yahoo_ticker("000660.KS"), "000660.KS");
        assert_eq!(yahoo_ticker(" pbr "), "PBR");
    }

    #[tokio::test]
    async fn rolling_rate_limit_applies_across_parallel_requests() {
        let limiter = RateLimiter::default();
        let start = tokio::time::Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let limiter = limiter.clone();
            tasks.push(tokio::spawn(async move {
                limiter.acquire(Provider::Yahoo).await.unwrap();
                tokio::time::Instant::now()
            }));
        }
        let mut times = Vec::new();
        for task in tasks {
            times.push(task.await.unwrap());
        }
        times.sort();
        assert!(times[4].duration_since(start) >= Duration::from_millis(950));
        assert!(times[3].duration_since(start) < Duration::from_millis(950));
    }

    #[tokio::test]
    async fn cooldown_and_fmp_daily_budget_survive_restart() {
        let directory = std::env::temp_dir().join(format!(
            "ibkr-rate-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let path = directory.join("rates.json");
        let limiter = RateLimiter::default();
        limiter.configure(path.clone()).await.unwrap();
        limiter
            .failed(
                Provider::Yahoo,
                &Failure {
                    message: "limited".into(),
                    retry_seconds: 120,
                    pause: false,
                    global: true,
                },
            )
            .await;
        {
            let mut state = limiter.0.lock().await;
            state.ledger.fmp_calls =
                std::iter::repeat_n(Utc::now().timestamp_millis(), 250).collect();
            persist_ledger(&state).await.unwrap();
        }
        let restarted = RateLimiter::default();
        restarted.configure(path.clone()).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            restarted.acquire(Provider::Yahoo)
        )
        .await
        .is_err());
        let quota = restarted.acquire(Provider::Fmp).await.unwrap_err();
        assert!(quota.global);
        assert!(quota.retry_seconds > 86000);
        let deadline = (Utc::now() + chrono::Duration::seconds(120)).to_rfc2822();
        assert!(retry_after_seconds(&deadline).unwrap() >= 119);
        assert_eq!(retry_after_seconds("180"), Some(180));
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[tokio::test]
    async fn worker_runs_parallel_requests_without_duplicates_and_with_a_shared_limit() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let peak = Arc::new(AtomicUsize::new(0));
        let peak_server = peak.clone();
        let server = std::thread::spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            let mut handlers = Vec::new();
            for _ in 0..8 {
                let (mut socket, _) = listener.accept().unwrap();
                let active = active.clone();
                let peak = peak_server.clone();
                handlers.push(std::thread::spawn(move || {
                    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut data = Vec::new();
                    loop {
                        let mut buffer = [0; 1024];
                        let read = socket.read(&mut buffer).unwrap();
                        assert!(read > 0); data.extend_from_slice(&buffer[..read]);
                        if data.windows(4).any(|part| part == b"\r\n\r\n") { break; }
                    }
                    let at = std::time::Instant::now();
                    let request = String::from_utf8(data).unwrap();
                    let symbol = request.lines().next().unwrap().split_whitespace().nth(1).unwrap()
                        .split('?').next().unwrap().rsplit('/').next().unwrap().to_string();
                    peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(350));
                    let body = YAHOO_AAPL.replace("AAPL", &symbol);
                    write!(socket, "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
                    active.fetch_sub(1, Ordering::SeqCst);
                    (symbol, at)
                }));
            }
            handlers
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        let client = mock_client(&base);
        // Test transport begins with a ready session so only profile calls are counted.
        client.yahoo.lock().await.crumb = "test-crumb".into();
        let directory = std::env::temp_dir().join(format!(
            "ibkr-parallel-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let (tx, rx) = mpsc::unbounded_channel();
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_with_client(
            rx,
            event_tx,
            directory.join("backends.dat"),
            client,
        ));
        let requests: Vec<_> = (0..8)
            .map(|i| Request {
                symbol: format!("STOCK{i}"),
                ticker: format!("STOCK{i}"),
            })
            .collect();
        tx.send(Command::Demand(
            requests.iter().chain(&requests).cloned().collect(),
        ))
        .unwrap();
        tokio::time::timeout(Duration::from_secs(6), async {
            let mut loaded = std::collections::HashSet::new();
            while loaded.len() < 8 {
                if let Some(Event::Profile(profile)) = events.recv().await {
                    assert!(loaded.insert(profile.symbol));
                }
            }
        })
        .await
        .unwrap();
        drop(tx);
        task.await.unwrap();
        let mut received = server.join().unwrap();
        assert!((2..=MAX_PARALLEL).contains(&peak.load(Ordering::SeqCst)));
        received.sort_by_key(|(_, at)| *at);
        assert!(received[4].1.duration_since(received[0].1) >= Duration::from_millis(900));
        let symbols: std::collections::HashSet<_> =
            received.into_iter().map(|(symbol, _)| symbol).collect();
        assert_eq!(symbols.len(), 8);
        tokio::fs::remove_file(directory.join("fundamentals-cache.json"))
            .await
            .unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    fn mock_http(
        responses: Vec<(&'static str, &'static str, &'static str)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, extra_headers, body) in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let read = socket.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    data.extend_from_slice(&buffer[..read]);
                    if data.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(data).unwrap());
                write!(socket, "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{extra_headers}\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (base, thread)
    }

    fn mock_client(base: &str) -> FundamentalsClient {
        let mut client = FundamentalsClient::new().unwrap();
        client.cookie_url = format!("{base}/cookie");
        client.crumb_url = format!("{base}/crumb");
        client.yahoo_base = format!("{base}/summary/");
        client.fmp_url = format!("{base}/profile");
        client
    }

    const YAHOO_AAPL: &str = r#"{"quoteSummary":{"result":[{"price":{"symbol":"AAPL","currency":"USD","marketCap":{"raw":3500000000000}},"assetProfile":{"sector":"Technology","industry":"Consumer Electronics"}}],"error":null}}"#;

    #[tokio::test]
    async fn yahoo_reuses_session_and_renews_expired_crumb_once() {
        let (base, server) = mock_http(vec![
            (
                "404 Not Found",
                "Set-Cookie: A3=test-session; Path=/\r\n",
                "",
            ),
            ("200 OK", "", "old-crumb"),
            ("401 Unauthorized", "", "{}"),
            (
                "404 Not Found",
                "Set-Cookie: A3=new-session; Path=/\r\n",
                "",
            ),
            ("200 OK", "", "new-crumb"),
            ("200 OK", "Content-Type: application/json\r\n", YAHOO_AAPL),
            ("200 OK", "Content-Type: application/json\r\n", YAHOO_AAPL),
        ]);
        let client = mock_client(&base);
        let request = Request {
            symbol: "AAPL".into(),
            ticker: "AAPL".into(),
        };
        client.fetch(Provider::Yahoo, &request, "").await.unwrap();
        client.fetch(Provider::Yahoo, &request, "").await.unwrap();
        let requests = server.join().unwrap();
        assert!(requests[5].contains("crumb=new-crumb"));
        assert!(requests[6]
            .to_lowercase()
            .contains("cookie: a3=new-session"));
    }

    #[tokio::test]
    async fn fmp_key_is_sent_in_header_and_rate_limits_back_off() {
        let (base, server) = mock_http(vec![("429 Too Many Requests", "Retry-After: 120\r\n", "")]);
        let client = mock_client(&base);
        let error = client
            .fetch(
                Provider::Fmp,
                &Request {
                    symbol: "AAPL".into(),
                    ticker: "AAPL".into(),
                },
                "test-secret",
            )
            .await
            .unwrap_err();
        assert!(error.global);
        assert_eq!(error.retry_seconds, 120);
        let requests = server.join().unwrap();
        assert!(requests[0].to_lowercase().contains("apikey: test-secret"));
        assert!(!requests[0].lines().next().unwrap().contains("test-secret"));
    }

    #[tokio::test]
    async fn worker_caches_requests_and_reloads_profiles_without_network() {
        let (base, server) = mock_http(vec![
            ("404 Not Found", "Set-Cookie: A3=session; Path=/\r\n", ""),
            ("200 OK", "", "crumb"),
            ("200 OK", "Content-Type: application/json\r\n", YAHOO_AAPL),
        ]);
        let directory = std::env::temp_dir().join(format!(
            "ibkr-fundamentals-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let path = directory.join("backends.dat");
        let (tx, rx) = mpsc::unbounded_channel();
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_with_client(
            rx,
            events_tx,
            path.clone(),
            mock_client(&base),
        ));
        tx.send(Command::Demand(vec![Request {
            symbol: "AAPL".into(),
            ticker: "AAPL".into(),
        }]))
        .unwrap();
        let profile = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Event::Profile(profile)) = events_rx.recv().await {
                    break profile;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(profile.sector, "Technology");
        // Await the status/cache write by closing the command channel cleanly.
        drop(tx);
        task.await.unwrap();
        server.join().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        // No server is listening: the restarted worker must use its persisted cache.
        let task = tokio::spawn(run_with_client(rx, events_tx, path, mock_client(&base)));
        tx.send(Command::Demand(vec![Request {
            symbol: "AAPL".into(),
            ticker: "AAPL".into(),
        }]))
        .unwrap();
        let loaded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Event::Profile(profile)) = events_rx.recv().await {
                    break profile;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(loaded.market_cap, profile.market_cap);
        tx.send(Command::Refresh).unwrap();
        // Even an explicit cache check must not refetch today's profiles.
        assert!(tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                if let Some(Event::Status(status)) = events_rx.recv().await {
                    assert!(
                        !status.contains("loading"),
                        "fresh profile was fetched again: {status}"
                    );
                }
            }
        })
        .await
        .is_err());
        tx.send(Command::Save {
            provider: Provider::Yahoo,
            enabled: false,
            replacement_key: None,
            clear_key: false,
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Event::Settings { enabled: false, .. }) = events_rx.recv().await {
                    break;
                }
            }
        })
        .await
        .unwrap();
        drop(tx);
        task.await.unwrap();
        // This is a unique temporary directory created by this test.
        tokio::fs::remove_file(directory.join("fundamentals-cache.json"))
            .await
            .unwrap();
        tokio::fs::remove_file(directory.join("backends.dat"))
            .await
            .unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }
}
