use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub account: String,
    pub conid: i64,
    pub symbol: String,
    pub side: Side,
    pub size: f64,
    pub price: f64,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.replace(',', "").parse().ok()))
        .filter(|n| n.is_finite())
}

pub(crate) fn integer(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

impl Execution {
    pub fn parse(v: &Value) -> Option<Self> {
        if v.get("sec_type").and_then(Value::as_str)? != "STK" {
            return None;
        }
        let side = match v.get("side")?.as_str()?.to_ascii_uppercase().as_str() {
            "B" | "BUY" => Side::Buy,
            "S" | "SELL" => Side::Sell,
            _ => return None,
        };
        let account = v
            .get("account")
            .or_else(|| v.get("accountCode"))?
            .as_str()?
            .to_string();
        let conid = integer(v.get("conid")?)?;
        let symbol = v.get("symbol")?.as_str()?.trim().to_uppercase();
        let size = number(v.get("size")?)?.abs();
        let price = number(v.get("price")?)?;
        let raw_time = v.get("trade_time_r").and_then(Value::as_i64)?;
        let at = if raw_time > 100_000_000_000 {
            Utc.timestamp_millis_opt(raw_time).single()?
        } else {
            Utc.timestamp_opt(raw_time, 0).single()?
        };
        if symbol.is_empty() || size <= 0.0 || price <= 0.0 {
            return None;
        }
        let id = v
            .get("execution_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{account}:{conid}:{raw_time}:{side:?}:{size}:{price}"));
        Some(Self {
            id,
            account,
            conid,
            symbol,
            side,
            size,
            price,
            at,
        })
    }

    pub fn parse_pa_transaction(
        v: &Value,
        account: &str,
        conid: i64,
        symbol: &str,
        index: usize,
    ) -> Option<Self> {
        if v.get("acctid")?.as_str()? != account || integer(v.get("conid")?)? != conid {
            return None;
        }
        let kind = v.get("type")?.as_str()?;
        let size = number(v.get("qty")?)?;
        let side = if kind.starts_with("Buy") && size > 0.0 {
            Side::Buy
        } else if kind.starts_with("Sell") && size < 0.0 {
            Side::Sell
        } else {
            return None;
        };
        let price = number(v.get("pr")?)?;
        if price <= 0.0 {
            return None;
        }
        let date = v.get("date")?.as_str()?;
        let parts: Vec<&str> = date.split_whitespace().collect();
        if parts.len() < 6 {
            return None;
        }
        let day = NaiveDate::parse_from_str(
            &format!("{} {} {}", parts[1], parts[2], parts.last()?),
            "%b %d %Y",
        )
        .ok()?;
        let at = Utc.from_utc_datetime(
            &(day.and_hms_opt(0, 0, 0)? + chrono::Duration::seconds(index as i64)),
        );
        Some(Self {
            id: format!("pa:{account}:{conid}:{day}:{index}"),
            account: account.into(),
            conid,
            symbol: symbol.into(),
            side,
            size: size.abs(),
            price,
            at,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Position {
    pub account: String,
    pub conid: i64,
    pub quantity: f64,
    pub market_value: Option<f64>,
    pub currency: String,
}

impl Position {
    pub fn parse(v: &Value) -> Option<Self> {
        if crate::options::is_option(v) {
            return None;
        }
        Some(Self {
            account: v.get("_account")?.as_str()?.to_string(),
            conid: integer(v.get("conid")?)?,
            quantity: number(v.get("position")?)?,
            market_value: v
                .get("mktValue")
                .or_else(|| v.get("marketValue"))
                .or_else(|| v.get("market_value"))
                .and_then(number)
                .map(f64::abs),
            currency: v
                .get("currency")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|currency| !currency.is_empty())
                .unwrap_or("USD")
                .to_ascii_uppercase(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Quote {
    pub price: f64,
    pub updated_ms: i64,
    pub real_time: bool,
    pub label: String,
    pub previous_close: bool,
}

impl Quote {
    pub fn parse(v: &Value, previous: Option<&Self>) -> Option<(i64, Self)> {
        let conid = integer(v.get("conid")?)?;
        let raw = v.get("31").and_then(Value::as_str).map(str::trim);
        let prefix = raw
            .and_then(|r| r.chars().next())
            .filter(|c| *c == 'C' || *c == 'H');
        let price = match raw {
            Some(r) => r
                .trim_start_matches(['C', 'H'])
                .replace(',', "")
                .parse::<f64>()
                .ok()?,
            None => previous?.price,
        };
        if !price.is_finite() || price <= 0.0 {
            return None;
        }
        let updated_ms = if raw.is_some() {
            v.get("_updated").and_then(Value::as_i64)?
        } else {
            previous?.updated_ms
        };
        let availability = v
            .get("6509")
            .and_then(Value::as_str)
            .and_then(|s| s.chars().next())
            .or_else(|| {
                previous.map(|q| {
                    if q.label.contains("real-time") {
                        'R'
                    } else if q.label.contains("delayed frozen") {
                        'Y'
                    } else if q.label.contains("delayed") {
                        'D'
                    } else if q.label.contains("frozen") {
                        'Z'
                    } else {
                        '?'
                    }
                })
            });
        let previous_close = if raw.is_some() {
            prefix.is_some()
        } else {
            previous.is_some_and(|q| q.previous_close)
        };
        let kind = match availability {
            Some('R') => "real-time",
            Some('D') => "delayed",
            Some('Z') => "frozen",
            Some('Y') => "delayed frozen",
            Some('N') => "not subscribed",
            _ => "unverified",
        };
        let label = format!(
            "IBKR {kind}{}",
            if previous_close {
                " · prior close or halted"
            } else {
                ""
            }
        );
        Some((
            conid,
            Self {
                price,
                updated_ms,
                real_time: availability == Some('R') && !previous_close,
                label,
                previous_close,
            },
        ))
    }
    pub fn alert_usable(&self, now_ms: i64) -> bool {
        self.real_time && self.updated_ms <= now_ms + 5_000 && now_ms - self.updated_ms <= 90_000
    }
}

#[derive(Clone, Debug)]
pub struct Sale {
    pub key: String,
    pub account: String,
    pub conid: i64,
    pub symbol: String,
    pub at: DateTime<Utc>,
    pub price: f64,
    pub held: f64,
    pub average_cost: Option<f64>,
    pub best_buy: Option<f64>,
}

pub fn recent_sales(executions: &HashMap<String, Execution>, positions: &[Position]) -> Vec<Sale> {
    let mut held: HashMap<i64, f64> = HashMap::new();
    for position in positions {
        *held.entry(position.conid).or_default() += position.quantity;
    }
    let mut groups: HashMap<(String, i64), Vec<&Execution>> = HashMap::new();
    for execution in executions.values() {
        groups
            .entry((execution.account.clone(), execution.conid))
            .or_default()
            .push(execution);
    }
    let mut sales = Vec::new();
    for ((account, conid), mut fills) in groups {
        fills.sort_by_key(|fill| fill.at);
        let Some(last_sell_idx) = fills.iter().rposition(|fill| fill.side == Side::Sell) else {
            continue;
        };
        let last = fills[last_sell_idx];
        let mut start = last_sell_idx;
        while start > 0 && fills[start - 1].side == Side::Sell {
            start -= 1;
        }
        let latest_sales = &fills[start..=last_sell_idx];
        let quantity: f64 = latest_sales.iter().map(|fill| fill.size).sum();
        if quantity <= 0.0 {
            continue;
        }
        let price = latest_sales
            .iter()
            .map(|fill| fill.size * fill.price)
            .sum::<f64>()
            / quantity;

        // Infer opening inventory from the current account position and fill balance.
        // A known long purchase cycle begins at zero, including after an older,
        // incomplete cycle is fully closed.
        let account_held: f64 = positions
            .iter()
            .filter(|p| p.conid == conid && p.account == account)
            .map(|p| p.quantity)
            .sum();
        let delta: f64 = fills
            .iter()
            .map(|f| if f.side == Side::Buy { f.size } else { -f.size })
            .sum();
        let opening = account_held - delta;
        let mut balance = opening;
        let mut complete = opening.abs() < 1e-7;
        let mut buys = Vec::new();
        let mut cost_buys = Vec::new();
        for (index, fill) in fills[..=last_sell_idx].iter().enumerate() {
            match fill.side {
                Side::Buy => {
                    balance += fill.size;
                    if complete {
                        buys.push((fill.price, fill.size));
                    }
                }
                Side::Sell => {
                    balance -= fill.size;
                    if balance < -1e-7 {
                        complete = false;
                        buys.clear();
                    }
                    if index == last_sell_idx && complete {
                        cost_buys = buys.clone();
                    }
                }
            }
            if balance.abs() < 1e-7 {
                complete = true;
                buys.clear();
            }
        }
        let average_cost = if !cost_buys.is_empty() {
            let total: f64 = cost_buys.iter().map(|(_, size)| size).sum();
            Some(
                cost_buys
                    .iter()
                    .map(|(cost, size)| cost * size)
                    .sum::<f64>()
                    / total,
            )
        } else {
            None
        };
        let best_buy = cost_buys.iter().map(|(cost, _)| *cost).reduce(f64::min);
        sales.push(Sale {
            key: format!("{account}:{conid}"),
            account,
            conid,
            symbol: last.symbol.clone(),
            at: last.at,
            price,
            held: *held.get(&conid).unwrap_or(&0.0),
            average_cost,
            best_buy,
        });
    }
    sales.sort_by(|a, b| b.at.cmp(&a.at));
    sales
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Setting {
    pub pinned: bool,
    pub alert: bool,
    pub target: Option<f64>,
    pub percent: bool,
    #[serde(skip)]
    pub previous_reached: Option<bool>,
    #[serde(skip)]
    pub fired: bool,
    #[serde(skip)]
    pub snooze_until_ms: i64,
}

impl Setting {
    pub fn level(&self, exit: f64) -> Option<f64> {
        let value = self.target?;
        if !value.is_finite() || value <= 0.0 {
            return None;
        }
        if self.percent {
            if value >= 100.0 {
                return None;
            }
            Some(exit * (1.0 - value / 100.0))
        } else {
            Some(value)
        }
    }
    pub fn observe(&mut self, quote: &Quote, exit: f64, now_ms: i64) -> bool {
        let Some(level) = self.level(exit) else {
            return false;
        };
        if !self.alert || !quote.alert_usable(now_ms) || now_ms < self.snooze_until_ms {
            return false;
        }
        let reached = quote.price <= level;
        let fired = reached && self.previous_reached == Some(false) && !self.fired;
        self.previous_reached = Some(reached);
        self.fired |= fired;
        fired
    }
}

pub type Settings = BTreeMap<String, Setting>;

#[cfg(test)]
mod tests {
    use super::*;
    fn fill(id: &str, side: Side, size: f64, price: f64, t: i64) -> Execution {
        Execution {
            id: id.into(),
            account: "U1".into(),
            conid: 1,
            symbol: "ABC".into(),
            side,
            size,
            price,
            at: Utc.timestamp_opt(t, 0).unwrap(),
        }
    }
    #[test]
    fn weighted_latest_sale_and_complete_purchase_cost() {
        let executions = [
            fill("b", Side::Buy, 3.0, 10.0, 1),
            fill("s1", Side::Sell, 1.0, 14.0, 2),
            fill("s2", Side::Sell, 2.0, 17.0, 3),
        ]
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
        let sales = recent_sales(&executions, &[]);
        assert_eq!(sales.len(), 1);
        assert!((sales[0].price - 16.0).abs() < 1e-9);
        assert_eq!(sales[0].average_cost, Some(10.0));
    }
    #[test]
    fn string_position_id_preserves_holdings_and_purchase_cost() {
        let position = Position::parse(&serde_json::json!({
            "_account": "U1", "conid": "1", "position": "3"
        }))
        .unwrap();
        let executions = [
            fill("buy", Side::Buy, 5.0, 10.0, 1),
            fill("sell", Side::Sell, 2.0, 12.0, 2),
        ]
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
        let sales = recent_sales(&executions, &[position]);
        assert_eq!(sales[0].held, 3.0);
        assert_eq!(sales[0].average_cost, Some(10.0));
        assert_eq!(sales[0].best_buy, Some(10.0));
    }
    #[test]
    fn buy_average_is_weighted_by_filled_shares_and_best_is_lowest_fill() {
        let executions = [
            fill("b1", Side::Buy, 2.0, 10.0, 1),
            fill("b2", Side::Buy, 3.0, 16.0, 2),
            fill("s", Side::Sell, 5.0, 20.0, 3),
        ]
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
        let sales = recent_sales(&executions, &[]);
        assert_eq!(sales[0].average_cost, Some(13.6));
        assert_eq!(sales[0].best_buy, Some(10.0));
    }
    #[test]
    fn unknown_opening_inventory_does_not_invent_cost() {
        let executions = [
            fill("s1", Side::Sell, 2.0, 14.0, 2),
            fill("s2", Side::Sell, 3.0, 17.0, 3),
        ]
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
        let sales = recent_sales(&executions, &[]);
        assert!((sales[0].price - 15.8).abs() < 1e-9);
        assert_eq!(sales[0].average_cost, None);
        assert_eq!(sales[0].best_buy, None);
    }
    #[test]
    fn later_complete_cycle_recovers_cost_after_unknown_opening_inventory() {
        let executions = [
            fill("old-exit", Side::Sell, 2.0, 12.0, 1),
            fill("new-buy", Side::Buy, 3.0, 10.0, 2),
            fill("new-exit", Side::Sell, 3.0, 15.0, 3),
        ]
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
        let sales = recent_sales(&executions, &[]);
        assert_eq!(sales[0].average_cost, Some(10.0));
        assert_eq!(sales[0].best_buy, Some(10.0));
    }
    #[test]
    fn client_portal_transaction_parses_older_buy() {
        let row = serde_json::json!({
            "acctid": "U1",
            "conid": 1,
            "date": "Thu Sep 17 00:00:00 EDT 2026",
            "type": "Buy",
            "qty": 2.0,
            "pr": 149.33
        });
        let fill = Execution::parse_pa_transaction(&row, "U1", 1, "ORCL", 0).unwrap();
        assert_eq!(fill.side, Side::Buy);
        assert_eq!(fill.size, 2.0);
        assert_eq!(fill.price, 149.33);
        assert_eq!(fill.at.date_naive().to_string(), "2026-09-17");
        assert!(Execution::parse_pa_transaction(&row, "U2", 1, "ORCL", 0).is_none());
    }
    #[test]
    fn archived_execution_preserves_reentry_sale() {
        let executions: HashMap<String, Execution> = [
            fill("b", Side::Buy, 2.0, 10.0, 1),
            fill("s", Side::Sell, 2.0, 12.0, 2),
        ]
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
        let saved = serde_json::to_vec(&executions).unwrap();
        let restored: HashMap<String, Execution> = serde_json::from_slice(&saved).unwrap();
        let sales = recent_sales(&restored, &[]);
        assert_eq!(sales[0].price, 12.0);
        assert_eq!(sales[0].average_cost, Some(10.0));
    }
    #[test]
    fn partial_websocket_quote_keeps_price_timestamp() {
        let initial =
            serde_json::json!({"conid": 1, "31": "C100.25", "6509": "RPB", "_updated": 1000});
        let (_, quote) = Quote::parse(&initial, None).unwrap();
        assert!(!quote.real_time);
        let availability = serde_json::json!({"conid": 1, "6509": "DPB", "_updated": 2000});
        let (_, updated) = Quote::parse(&availability, Some(&quote)).unwrap();
        assert_eq!(updated.price, 100.25);
        assert_eq!(updated.updated_ms, 1000);
        assert!(!updated.real_time);
    }
    #[test]
    fn alert_requires_fresh_real_time_crossing() {
        let mut setting = Setting {
            target: Some(9.0),
            alert: true,
            ..Default::default()
        };
        let mut q = Quote {
            price: 10.0,
            updated_ms: 1_000_000,
            real_time: true,
            label: "IBKR".into(),
            previous_close: false,
        };
        assert!(!setting.observe(&q, 12.0, 1_000_000));
        q.price = 8.0;
        assert!(setting.observe(&q, 12.0, 1_000_001));
        assert!(!setting.observe(&q, 12.0, 1_000_002));
        setting.fired = false;
        q.real_time = false;
        assert!(!setting.observe(&q, 12.0, 1_000_003));
    }
}
