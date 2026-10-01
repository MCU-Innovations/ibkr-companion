use crate::model::Quote;
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketPhase {
    Regular,
    Extended,
    Overnight,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveSource {
    Regular,
    Extended,
    Overnight,
}

impl LiveSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Regular => "IBKR live · regular",
            Self::Extended => "IBKR live · extended",
            Self::Overnight => "IBKR live · overnight",
        }
    }
}

fn first_sunday(year: i32, month: u32) -> u32 {
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    1 + (7 - first.weekday().num_days_from_sunday()) % 7
}

/// US Eastern time, including the DST transitions used by US equity sessions.
fn eastern(now: DateTime<Utc>) -> DateTime<FixedOffset> {
    let year = now.year();
    let march_second_sunday = first_sunday(year, 3) + 7;
    let november_first_sunday = first_sunday(year, 11);
    // 02:00 Eastern is 07:00 UTC on the spring change and 06:00 UTC in autumn.
    let dst_start = Utc
        .with_ymd_and_hms(year, 3, march_second_sunday, 7, 0, 0)
        .single()
        .unwrap();
    let dst_end = Utc
        .with_ymd_and_hms(year, 11, november_first_sunday, 6, 0, 0)
        .single()
        .unwrap();
    let hours_west = if now >= dst_start && now < dst_end {
        4
    } else {
        5
    };
    now.with_timezone(&FixedOffset::west_opt(hours_west * 3600).unwrap())
}

pub fn eastern_time(now: DateTime<Utc>) -> DateTime<FixedOffset> {
    eastern(now)
}

pub fn overnight_subscription_active(now: DateTime<Utc>) -> bool {
    let et = eastern(now);
    let minute = et.hour() * 60 + et.minute();
    match et.weekday() {
        Weekday::Sun | Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu => {
            minute >= 18 * 60 || (et.weekday() != Weekday::Sun && minute < 4 * 60)
        }
        Weekday::Fri => minute < 4 * 60,
        Weekday::Sat => false,
    }
}

pub fn eastern_date(now: DateTime<Utc>) -> NaiveDate {
    eastern(now).date_naive()
}

pub fn close_cycle_date(now: DateTime<Utc>) -> NaiveDate {
    let et = eastern(now);
    let mut date = et.date_naive();
    if !matches!(
        et.weekday(),
        Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
    ) || et.hour() < 16
    {
        date = date.pred_opt().unwrap();
    }
    while matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
        date = date.pred_opt().unwrap();
    }
    date
}

pub fn phase(now: DateTime<Utc>) -> MarketPhase {
    let et = eastern(now);
    let minute = et.hour() * 60 + et.minute();
    let weekday = matches!(
        et.weekday(),
        Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
    );
    if weekday && (9 * 60 + 30..16 * 60).contains(&minute) {
        MarketPhase::Regular
    } else if overnight_subscription_active(now) && (minute >= 20 * 60 || minute < 3 * 60 + 50) {
        MarketPhase::Overnight
    } else if (weekday
        && ((4 * 60..9 * 60 + 30).contains(&minute) || (16 * 60..20 * 60).contains(&minute)))
        || (et.weekday() == Weekday::Sun && (18 * 60..20 * 60).contains(&minute))
    {
        MarketPhase::Extended
    } else {
        MarketPhase::Closed
    }
}

pub fn live_quote<'a>(
    now: DateTime<Utc>,
    smart: Option<&'a Quote>,
    overnight: Option<&'a Quote>,
) -> Option<(&'a Quote, LiveSource)> {
    let current_phase = phase(now);
    let usable = |quote: &&Quote, source: LiveSource| {
        let et = eastern(now);
        let (date, hour, minute) = match source {
            LiveSource::Regular => (et.date_naive(), 9, 30),
            LiveSource::Extended => (et.date_naive(), if et.hour() >= 16 { 16 } else { 4 }, 0),
            LiveSource::Overnight => (
                if et.hour() < 4 {
                    et.date_naive().pred_opt().unwrap()
                } else {
                    et.date_naive()
                },
                20,
                0,
            ),
        };
        let start = et
            .offset()
            .from_local_datetime(&date.and_hms_opt(hour, minute, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        !quote.previous_close
            && (quote.real_time || quote.label == "IBKR delayed")
            && quote.updated_ms >= start
            && quote.updated_ms <= now.timestamp_millis() + 5_000
    };
    let smart_source = if current_phase == MarketPhase::Regular {
        LiveSource::Regular
    } else {
        LiveSource::Extended
    };
    let smart = smart.filter(|quote| usable(quote, smart_source));
    let overnight = overnight.filter(|quote| usable(quote, LiveSource::Overnight));
    match phase(now) {
        MarketPhase::Regular => smart.map(|quote| (quote, LiveSource::Regular)),
        MarketPhase::Overnight => overnight.map(|quote| (quote, LiveSource::Overnight)),
        MarketPhase::Extended => smart.map(|quote| (quote, LiveSource::Extended)),
        MarketPhase::Closed => None,
    }
}

pub fn is_overnight_message(value: &Value) -> bool {
    ["conidEx", "topic"]
        .into_iter()
        .filter_map(|field| value.get(field).and_then(Value::as_str))
        .any(|text| text.to_ascii_uppercase().contains("@OVERNIGHT"))
}

#[derive(Clone, Debug, Default)]
pub struct ClosePrices {
    today: Option<f64>,
    prior: Option<f64>,
    prefixed_last: Option<f64>,
}

impl ClosePrices {
    pub fn update(&mut self, value: &Value) {
        if let Some(price) = value.get("7296").and_then(price_field) {
            self.today = Some(price);
        }
        if let Some(price) = value.get("7741").and_then(price_field) {
            self.prior = Some(price);
        }
        if let Some(price) = value
            .get("31")
            .and_then(Value::as_str)
            .and_then(|raw| raw.strip_prefix('C'))
            .and_then(|raw| price_field(&Value::String(raw.into())))
        {
            self.prefixed_last = Some(price);
        }
    }

    pub fn display(&self) -> Option<(f64, &'static str)> {
        self.today.map(|price| (price, "IBKR close")).or_else(|| {
            self.prior
                .or(self.prefixed_last)
                .map(|price| (price, "IBKR prior close"))
        })
    }
}

pub fn latest_regular_close(response: &Value, cycle_date: NaiveDate) -> Option<(NaiveDate, f64)> {
    response
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|bar| {
            let timestamp = bar.get("t")?.as_i64()?;
            let at = DateTime::<Utc>::from_timestamp_millis(timestamp)?;
            let date = eastern_date(at);
            let price = bar.get("c").and_then(price_field)?;
            (date <= cycle_date).then_some((date, timestamp, price))
        })
        .max_by_key(|(_, timestamp, _)| *timestamp)
        .map(|(date, _, price)| (date, price))
}

fn price_field(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.replace(',', "").parse::<f64>().ok())
        })
        .filter(|price| price.is_finite() && *price > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eastern_session_boundaries_and_dst() {
        let at = |year, month, day, hour, minute| {
            Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
                .single()
                .unwrap()
        };
        assert_eq!(phase(at(2026, 9, 30, 13, 29)), MarketPhase::Extended);
        assert_eq!(phase(at(2026, 9, 30, 13, 30)), MarketPhase::Regular);
        assert_eq!(phase(at(2026, 9, 30, 20, 0)), MarketPhase::Extended);
        assert_eq!(phase(at(2026, 10, 1, 0, 0)), MarketPhase::Overnight);
        assert_eq!(phase(at(2026, 10, 1, 7, 50)), MarketPhase::Closed);
        assert_eq!(phase(at(2026, 12, 1, 14, 30)), MarketPhase::Regular);
        assert_eq!(phase(at(2026, 10, 3, 15, 0)), MarketPhase::Closed);
        assert!(overnight_subscription_active(at(2026, 10, 4, 22, 0)));
    }

    #[test]
    fn overnight_uses_only_fresh_overnight_and_close_is_available() {
        let now = Utc
            .with_ymd_and_hms(2026, 10, 1, 0, 30, 0)
            .single()
            .unwrap();
        let smart = Quote {
            price: 100.0,
            updated_ms: now.timestamp_millis(),
            real_time: true,
            label: "IBKR real-time".into(),
            previous_close: false,
        };
        assert!(live_quote(now, Some(&smart), None).is_none());
        let overnight = Quote {
            price: 101.0,
            ..smart.clone()
        };
        assert_eq!(
            live_quote(now, Some(&smart), Some(&overnight))
                .unwrap()
                .0
                .price,
            101.0
        );
        let mut close = ClosePrices::default();
        close.update(&serde_json::json!({"7296": "99.50", "7741": "98.70"}));
        assert_eq!(close.display().map(|(price, _)| price), Some(99.5));
        assert_eq!(close.display(), Some((99.5, "IBKR close")));
        assert!(is_overnight_message(&serde_json::json!({
            "conid": 1,
            "conidEx": "1@OVERNIGHT",
            "topic": "smd+1@OVERNIGHT"
        })));
    }

    #[test]
    fn completed_close_ignores_partial_regular_session_bar() {
        let at = |month, day, hour| {
            Utc.with_ymd_and_hms(2026, month, day, hour, 30, 0)
                .single()
                .unwrap()
        };
        let response = serde_json::json!({"data": [
            {"t": at(9, 29, 13).timestamp_millis(), "c": 99.0},
            {"t": at(9, 30, 13).timestamp_millis(), "c": 101.0}
        ]});
        assert_eq!(
            close_cycle_date(at(9, 30, 14)),
            NaiveDate::from_ymd_opt(2026, 9, 29).unwrap()
        );
        assert_eq!(
            latest_regular_close(&response, close_cycle_date(at(9, 30, 14)))
                .unwrap()
                .1,
            99.0
        );
        assert_eq!(
            latest_regular_close(&response, close_cycle_date(at(9, 30, 21)))
                .unwrap()
                .1,
            101.0
        );
    }

    #[test]
    fn session_quote_stays_visible_without_relaxing_alert_freshness() {
        let now = Utc.with_ymd_and_hms(2026, 9, 30, 22, 0, 0).unwrap();
        let mut quote = Quote {
            price: 101.0,
            updated_ms: (now - chrono::Duration::minutes(10)).timestamp_millis(),
            real_time: true,
            label: "IBKR real-time".into(),
            previous_close: false,
        };
        assert!(live_quote(now, Some(&quote), None).is_some());
        assert!(!quote.alert_usable(now.timestamp_millis()));
        quote.real_time = false;
        quote.label = "IBKR delayed".into();
        assert!(live_quote(now, Some(&quote), None).is_some());
        assert!(!quote.alert_usable(now.timestamp_millis()));
        quote.updated_ms = (now - chrono::Duration::hours(3)).timestamp_millis();
        assert!(live_quote(now, Some(&quote), None).is_none());
    }
}
