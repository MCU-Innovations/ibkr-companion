use chrono::NaiveDate;
use serde_json::Value;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PremiumSale {
    pub id: String,
    #[serde(default)]
    pub order_id: Option<String>,
    pub account: String,
    pub conid: i64,
    pub contract: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub quantity: f64,
    pub price: f64,
    pub proceeds: Option<f64>,
    pub currency: String,
}

impl PremiumSale {
    pub fn parse(v: &Value) -> Option<Self> {
        if !matches!(v["sec_type"].as_str()?, "OPT" | "FOP")
            || !matches!(v["side"].as_str()?.to_ascii_uppercase().as_str(), "S" | "SELL") { return None; }
        let quantity = number(v, &["size"])?.abs();
        let price = number(v, &["price"])?;
        if quantity <= 0.0 || price < 0.0 { return None; }
        let raw = crate::model::integer(&v["trade_time_r"])?;
        let at = if raw > 100_000_000_000 { chrono::DateTime::from_timestamp_millis(raw)? }
            else { chrono::DateTime::from_timestamp(raw, 0)? };
        Some(Self {
            id: v["execution_id"].as_str()?.to_string(),
            order_id: ["order_id", "orderId"].iter().find_map(|key| match &v[*key] {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()), _ => None,
            }),
            account: v["account"].as_str().or_else(|| v["accountCode"].as_str())?.into(),
            conid: crate::model::integer(&v["conid"] )?,
            contract: text(v, &["contract_description_1", "symbol"]),
            at, quantity, price,
            proceeds: number(v, &["net_amount"]).map(f64::abs),
            currency: text(v, &["currency"]),
        })
    }
}

pub fn premium_list(sales: &std::collections::HashMap<String, PremiumSale>, accounts: &[String]) -> (Vec<crate::PremiumRow>, String) {
    let mut groups = std::collections::BTreeMap::new();
    for sale in sales.values().filter(|s| accounts.contains(&s.account)) {
        // Order identifiers join partial fills; older archived executions fall
        // back to the same contract sold in the same second.
        let batch = sale.order_id.as_ref().map(|id| format!("order:{id}:{}", sale.at.date_naive()))
            .unwrap_or_else(|| format!("time:{}", sale.at.timestamp()));
        let key = (sale.account.clone(), sale.conid, sale.currency.clone(), batch);
        let group = groups.entry(key).or_insert_with(|| (sale.clone(), 0.0, 0.0, Some(0.0)));
        group.1 += sale.quantity;
        group.2 += sale.quantity * sale.price;
        group.3 = group.3.zip(sale.proceeds).map(|(sum, proceeds)| sum + proceeds);
        if sale.at > group.0.at { group.0.at = sale.at; }
    }
    let mut sales: Vec<_> = groups.into_values().map(|(mut sale, quantity, weighted, proceeds)| {
        sale.quantity = quantity;
        sale.price = weighted / quantity;
        sale.proceeds = proceeds;
        sale
    }).collect();
    sales.sort_by(|a, b| b.at.cmp(&a.at).then(a.id.cmp(&b.id)));
    let mut totals = std::collections::BTreeMap::<String, f64>::new();
    let mut missing = 0;
    let rows = sales.into_iter().map(|s| {
        if let Some(n) = s.proceeds.filter(|_| s.currency != "—") { *totals.entry(s.currency.clone()).or_default() += n; } else { missing += 1; }
        crate::PremiumRow {
            contract: s.contract.clone().into(), account: s.account.clone().into(),
            date: s.at.with_timezone(&chrono::Local).format("%d %b %y %H:%M").to_string().into(),
            quantity: s.quantity.to_string().into(), price: format!("{:.2}", s.price).into(),
            proceeds: s.proceeds.map(|n| format!("{n:.2} {}", s.currency)).unwrap_or_else(|| "—".into()).into(),
        }
    }).collect();
    let mut total = totals.into_iter().map(|(c, n)| format!("{n:.2} {c}")).collect::<Vec<_>>().join(" · ");
    if total.is_empty() { total = "—".into(); }
    if missing > 0 { total.push_str(&format!(" ({missing} unavailable)")); }
    (rows, total)
}

#[derive(Clone, Debug, Default)]
pub struct OptionView {
    pub key: String,
    pub symbol: String,
    pub contract_short: String,
    pub market_price: String,
    pub market_status: String,
    pub moneyness: String,
    pub market_price_number: Option<f64>,
    pub share_cost: String,
    pub share_cost_number: Option<f64>,
    pub expiry_short: String,
    pub dte: String,
    pub quantity: String,
    pub mark: String,
    pub pnl_value: String,
    pub pnl_percent: String,
    pub delta: String,
    pub gamma: String,
    pub theta: String,
    pub vega: String,
    pub quote_status: String,
    pub currency: String,
    pub pnl_tone: i32,
    pub expiry_day: Option<NaiveDate>,
    pub quantity_number: f64,
    pub strike_number: Option<f64>,
    pub mark_number: Option<f64>,
    pub pnl_number: Option<f64>,
    pub return_number: Option<f64>,
    pub delta_number: Option<f64>,
    pub gamma_number: Option<f64>,
    pub theta_number: Option<f64>,
    pub vega_number: Option<f64>,
    pub contract: String,
    pub account: String,
    pub expiry: String,
    pub position: String,
    pub pricing: String,
    pub pnl: String,
    pub greeks: String,
    pub liquidity: String,
    pub urgent: bool,
}

// IBKR may embed the OCC code inside a display description, e.g.
// "BE OCT2026 295 C [BE  261002C00295000 100]". Search ASCII digit runs
// instead of assuming the last 15 characters of the description are the code.
fn occ_details(description: &str) -> Option<(NaiveDate, &'static str, f64)> {
    let bytes = description.as_bytes();
    for (start, code) in bytes.windows(15).enumerate() {
        if (start > 0 && bytes[start - 1].is_ascii_digit())
            || bytes.get(start + 15).is_some_and(u8::is_ascii_digit)
            || !code[..6].iter().all(u8::is_ascii_digit)
            || !matches!(code[6], b'C' | b'P')
            || !code[7..].iter().all(u8::is_ascii_digit)
        {
            continue;
        }
        let date = std::str::from_utf8(&code[..6])
            .ok()
            .and_then(|s| NaiveDate::parse_from_str(s, "%y%m%d").ok());
        if let Some(date) = date {
            let strike = std::str::from_utf8(&code[7..]).ok()?.parse::<u64>().ok()? as f64 / 1000.0;
            return Some((date, if code[6] == b'C' { "Call" } else { "Put" }, strike));
        }
    }
    None
}

fn compare_optional<T: PartialOrd>(
    a: Option<T>,
    b: Option<T>,
    ascending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(a), Some(b)) => if ascending {
            a.partial_cmp(&b)
        } else {
            b.partial_cmp(&a)
        }
        .unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

pub fn sort(rows: &mut [OptionView], column: &str, ascending: bool) {
    rows.sort_by(|a, b| {
        let order = match column {
            "symbol" => compare_optional(Some(&a.symbol), Some(&b.symbol), ascending),
            "contract" => compare_optional(a.strike_number, b.strike_number, ascending),
            "market" => compare_optional(a.market_price_number, b.market_price_number, ascending),
            "share-cost" => compare_optional(a.share_cost_number, b.share_cost_number, ascending),
            "moneyness" => compare_optional(Some(&a.moneyness), Some(&b.moneyness), ascending),
            "position" => {
                compare_optional(Some(a.quantity_number), Some(b.quantity_number), ascending)
            }
            "mark" => compare_optional(a.mark_number, b.mark_number, ascending),
            "pnl" => compare_optional(a.pnl_number, b.pnl_number, ascending),
            "return" => compare_optional(a.return_number, b.return_number, ascending),
            "delta" => compare_optional(a.delta_number, b.delta_number, ascending),
            "gamma" => compare_optional(a.gamma_number, b.gamma_number, ascending),
            "theta" => compare_optional(a.theta_number, b.theta_number, ascending),
            "vega" => compare_optional(a.vega_number, b.vega_number, ascending),
            _ => compare_optional(a.expiry_day, b.expiry_day, ascending),
        };
        order.then(a.symbol.cmp(&b.symbol)).then(a.key.cmp(&b.key))
    });
}

pub fn pnl_summary(rows: &[OptionView]) -> String {
    let mut currencies = std::collections::BTreeMap::<&str, f64>::new();
    for row in rows {
        if let Some(pnl) = row.pnl_number {
            *currencies.entry(&row.currency).or_default() += pnl;
        }
    }
    let missing = rows.iter().filter(|r| r.pnl_number.is_none()).count();
    let mut result = currencies
        .into_iter()
        .map(|(c, n)| format!("{n:+.2} {c}"))
        .collect::<Vec<_>>()
        .join("  ·  ");
    if result.is_empty() {
        result = "—".into();
    }
    if missing > 0 {
        result.push_str(&format!(" ({missing} unavailable)"));
    }
    result
}

impl From<OptionView> for crate::OptionRow {
    fn from(o: OptionView) -> Self {
        Self {
            key: o.key.into(),
            symbol: o.symbol.into(),
            contract_short: o.contract_short.into(),
            market_price: o.market_price.into(),
            share_cost: o.share_cost.into(),
            market_status: o.market_status.into(),
            moneyness: o.moneyness.into(),
            expiry_short: o.expiry_short.into(),
            dte: o.dte.into(),
            quantity: o.quantity.into(),
            mark: o.mark.into(),
            pnl_value: o.pnl_value.into(),
            pnl_percent: o.pnl_percent.into(),
            delta: o.delta.into(),
            gamma: o.gamma.into(),
            theta: o.theta.into(),
            vega: o.vega.into(),
            quote_status: o.quote_status.into(),
            pnl_tone: o.pnl_tone,
            contract: o.contract.into(),
            account: o.account.into(),
            expiry: o.expiry.into(),
            position: o.position.into(),
            pricing: o.pricing.into(),
            pnl: o.pnl.into(),
            greeks: o.greeks.into(),
            liquidity: o.liquidity.into(),
            urgent: o.urgent,
        }
    }
}

pub fn apply_rows(ui: &crate::AppWindow, rows: Vec<OptionView>) {
    use slint::{Model, ModelRc, VecModel};
    let rows: Vec<crate::OptionRow> = rows.into_iter().map(Into::into).collect();
    let selected = ui.get_selected_option().key;
    if let Some(row) = rows.iter().find(|r| r.key == selected) {
        ui.set_selected_option(row.clone());
    } else {
        ui.set_selected_option(crate::OptionRow::default());
        ui.set_option_detail_visible(false);
    }
    let current = ui.get_option_rows();
    if let Some(model) = current
        .as_any()
        .downcast_ref::<VecModel<crate::OptionRow>>()
    {
        let same_order = model.row_count() == rows.len()
            && rows
                .iter()
                .enumerate()
                .all(|(i, row)| model.row_data(i).is_some_and(|old| old.key == row.key));
        if same_order {
            for (i, row) in rows.into_iter().enumerate() {
                if model.row_data(i).is_some_and(|old| old != row) {
                    model.set_row_data(i, row);
                }
            }
        } else {
            model.set_vec(rows);
        }
    } else {
        ui.set_option_rows(ModelRc::new(VecModel::from(rows)));
    }
}

pub fn is_option(v: &Value) -> bool {
    ["secType", "assetClass", "type", "instrumentType"]
        .iter()
        .any(|key| {
            v[*key]
                .as_str()
                .is_some_and(|s| matches!(s.trim().to_ascii_uppercase().as_str(), "OPT" | "FOP"))
        })
}

pub fn underlying_conid(v: &Value) -> Option<i64> {
    [
        "underlyingConid",
        "underlying_conid",
        "underlying_con_id",
        "underConid",
        "undConid",
        "6457",
    ]
    .iter()
    .find_map(|key| crate::model::integer(&v[*key]).filter(|id| *id > 0))
}

fn moneyness(price: Option<f64>, strike: Option<f64>, right: &str) -> &'static str {
    let Some((price, strike)) = price.zip(strike) else {
        return "—";
    };
    if !matches!(right, "Call" | "Put") {
        return "—";
    }
    let difference = (price * 100.0).round() - (strike * 100.0).round();
    if difference == 0.0 {
        "ATM"
    } else if (right == "Call" && difference > 0.0) || (right == "Put" && difference < 0.0) {
        "ITM"
    } else {
        "OTM"
    }
}

fn text(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| match &v[*k] {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "—".into())
}

fn number(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| {
            v[*k]
                .as_f64()
                .or_else(|| v[*k].as_str()?.replace(',', "").parse().ok())
        })
        .filter(|n| n.is_finite())
}

fn amount(v: &Value, keys: &[&str]) -> String {
    number(v, keys)
        .map(|n| format!("{n:.2}"))
        .unwrap_or_else(|| "—".into())
}

// IBKR sends partial ticks. A bid/ask/last tick must not leave an older
// cached mark on screen when field 7635 is absent from that update.
pub fn merge_ticks(cached: &mut Value, update: &Value) {
    let (Some(old), Some(new)) = (cached.as_object_mut(), update.as_object()) else { return; };
    old.extend(new.iter().filter(|(_, value)| !value.is_null()).map(|(key, value)| (key.clone(), value.clone())));
    if number(update, &["7635"]).is_none() && ["31", "84", "86"].iter().any(|key| update.get(*key).is_some()) {
        let values = Value::Object(old.clone());
        if let Some(last) = number(&values, &["31"]) {
            let mark = number(&values, &["86"]).filter(|ask| *ask < last)
                .or_else(|| number(&values, &["84"]).filter(|bid| *bid > last)).unwrap_or(last);
            old.insert("7635".into(), serde_json::json!(mark));
        }
    }
}

pub fn share_cost(positions: &[Value], account: &str, underlying: i64, currency: &str) -> Option<f64> {
    let p = positions.iter().find(|p| {
        !is_option(p)
            && text(p, &["_account"]) == account
            && crate::model::integer(&p["conid"]) == Some(underlying)
            && text(p, &["currency"]).eq_ignore_ascii_case(currency)
            && number(p, &["position"]).is_some_and(|n| n > 1e-7)
    })?;
    number(p, &["avgPrice"])
        .or_else(|| {
            let cost = number(p, &["avgCost"])?;
            let multiplier = number(p, &["multiplier"]).filter(|n| *n > 0.0).unwrap_or(1.0);
            Some(cost / multiplier)
        })
        .filter(|n| *n >= 0.0)
}

pub fn row(
    position: &Value,
    info: Option<&Value>,
    ticks: Option<&Value>,
    today: NaiveDate,
) -> Option<OptionView> {
    if !is_option(position) {
        return None;
    }
    let qty = number(position, &["position"])?;
    if qty.abs() < 1e-7 {
        return None;
    }
    // Metadata cannot overwrite account position values or live market data.
    let mut v = info.cloned().unwrap_or_else(|| serde_json::json!({}));
    let object = v.as_object_mut()?;
    object.extend(
        position
            .as_object()?
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    if let Some(ticks) = ticks.and_then(Value::as_object) {
        object.extend(ticks.clone());
    }
    let description = text(
        &v,
        &[
            "contractDesc",
            "description",
            "localSymbol",
            "symbol",
            "ticker",
        ],
    );
    let occ = ["localSymbol", "contractDesc", "description"]
        .iter()
        .filter_map(|key| v[*key].as_str())
        .find_map(occ_details);
    let raw_expiry = text(
        &v,
        &[
            "expiry",
            "expirationDate",
            "lastTradeDateOrContractMonth",
            "maturityDate",
        ],
    );
    let expiry = ["%Y%m%d", "%Y-%m-%d", "%Y%m%d-%H:%M:%S"]
        .iter()
        .find_map(|f| NaiveDate::parse_from_str(&raw_expiry, f).ok())
        .or(occ.map(|o| o.0));
    let days = expiry.map(|d| (d - today).num_days());
    let currency = text(&v, &["currency"]);
    let mut right = text(&v, &["putOrCall", "right"]);
    if right == "—" {
        if let Some((_, kind, _)) = occ {
            right = kind.into();
        }
    }
    right = match right.as_str() {
        "C" | "CALL" => "Call".into(),
        "P" | "PUT" => "Put".into(),
        _ => right,
    };
    let strike_number = number(&v, &["strike", "strikePrice"]).or(occ.map(|o| o.2));
    let market_price_number = number(&v, &["_underlyingPrice"]).filter(|n| *n > 0.0);
    let strike = strike_number
        .map(|n| format!("{n:.2}"))
        .unwrap_or_else(|| "—".into());
    let symbol = text(&v, &["ticker", "symbol", "underlyingSymbol"]);
    let symbol = if symbol == "—" {
        description
            .split_whitespace()
            .next()
            .unwrap_or("—")
            .to_string()
    } else {
        symbol
    };
    let multiplier = text(&v, &["multiplier"]);
    let quote_kind = match text(&v, &["6509"]).chars().next() {
        Some('R') => "real-time",
        Some('D' | 'Y') => "delayed",
        Some('Z') => "frozen",
        _ => "snapshot",
    };
    let updated = number(&v, &["_updated"])
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".into());
    let mark_number = number(&v, &["7635", "mktPrice"]);
    let pnl_number = number(&v, &["unrealizedPnl"]);
    let return_number = pnl_number
        .zip(number(&v, &["avgCost"]))
        .filter(|(_, cost)| cost.abs() > 1e-9)
        .map(|(pnl, cost)| 100.0 * pnl / (cost * qty).abs());
    Some(OptionView {
        share_cost: amount(&v, &["_shareCost"]),
        share_cost_number: number(&v, &["_shareCost"]),
        market_status: text(&v, &["_underlyingStatus"]),
        market_price: market_price_number
            .map(|n| format!("{n:.2}"))
            .unwrap_or_else(|| "—".into()),
        moneyness: moneyness(market_price_number, strike_number, &right).into(),
        market_price_number,
        key: format!("{}:{}", text(&v, &["_account"]), text(&v, &["conid"])),
        symbol,
        contract_short: format!(
            "{} {right}",
            strike.trim_end_matches('0').trim_end_matches('.')
        ),
        expiry_short: expiry
            .map(|d| d.format("%d %b %y").to_string())
            .unwrap_or_else(|| "—".into()),
        dte: days
            .map(|d| {
                if d < 0 {
                    "Expired".into()
                } else if d == 0 {
                    "Today".into()
                } else {
                    format!("{d}d")
                }
            })
            .unwrap_or_else(|| "—".into()),
        quantity: format!("{} {}", if qty > 0.0 { "Long" } else { "Short" }, qty.abs()),
        mark: amount(&v, &["7635", "mktPrice"]),
        pnl_value: pnl_number
            .map(|n| format!("{n:+.2} {currency}"))
            .unwrap_or_else(|| "—".into()),
        pnl_percent: return_number
            .map(|n| format!("{n:+.1}%"))
            .unwrap_or_else(|| "—".into()),
        delta: text(&v, &["7308"]),
        gamma: text(&v, &["7309"]),
        theta: text(&v, &["7310"]),
        vega: text(&v, &["7311"]),
        quote_status: if number(&v, &["7635"]).is_some() {
            quote_kind.into()
        } else {
            "snapshot".into()
        },
        currency: currency.clone(),
        pnl_tone: pnl_number
            .map(|p| {
                if p > 0.0 {
                    1
                } else if p < 0.0 {
                    -1
                } else {
                    0
                }
            })
            .unwrap_or(0),
        expiry_day: expiry,
        quantity_number: qty,
        strike_number,
        mark_number,
        pnl_number,
        return_number,
        delta_number: number(&v, &["7308"]),
        gamma_number: number(&v, &["7309"]),
        theta_number: number(&v, &["7310"]),
        vega_number: number(&v, &["7311"]),
        contract: format!("{description}  ·  {right}  ·  strike {strike}"),
        account: format!(
            "Account {}  ·  {currency}  ·  multiplier {multiplier}",
            text(&v, &["_account"])
        ),
        expiry: expiry
            .map(|d| {
                let days = days.unwrap();
                if days < 0 {
                    format!("Expired {d}")
                } else if days == 0 {
                    format!("Expires today · {d}")
                } else {
                    format!(
                        "Expires {d}  ·  {days} {}",
                        if days == 1 { "day" } else { "days" }
                    )
                }
            })
            .unwrap_or_else(|| format!("Expiry {raw_expiry}")),
        urgent: days.is_some_and(|d| d <= 7),
        position: format!(
            "{} {} contracts  ·  Market value {} {currency}",
            if qty > 0.0 { "Long" } else { "Short" },
            qty.abs(),
            amount(&v, &["mktValue", "marketValue"])
        ),
        pricing: format!(
            "Bid {}   Ask {}   Last {}   Mark {}   ·   {quote_kind} {updated}",
            amount(&v, &["84"]),
            amount(&v, &["86"]),
            amount(&v, &["31", "mktPrice"]),
            amount(&v, &["7635"])
        ),
        pnl: format!(
            "Avg cost {}   ·   Unrealized P/L {} {currency}   ·   Realized P/L {} {currency}",
            amount(&v, &["avgCost"]),
            amount(&v, &["unrealizedPnl"]),
            amount(&v, &["realizedPnl"])
        ),
        greeks: format!(
            "Delta {}   Gamma {}   Theta {}   Vega {}   IV {}",
            text(&v, &["7308"]),
            text(&v, &["7309"]),
            text(&v, &["7310"]),
            text(&v, &["7311"]),
            text(&v, &["7633"])
        ),
        liquidity: format!(
            "Volume {}   ·   Open interest {}",
            text(&v, &["87"]),
            text(&v, &["7638"])
        ),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn premium_rows_combine_partial_fills_and_preserve_separate_sales() {
        let fill = serde_json::json!({"execution_id":"a", "order_id":42, "sec_type":"OPT",
            "side":"S", "account":"U1", "conid":123, "trade_time_r":1790956800000_i64,
            "size":1, "price":"1.00", "net_amount":100, "currency":"USD"});
        let mut next = fill.clone();
        next["execution_id"] = serde_json::json!("b");
        next["size"] = serde_json::json!(2);
        next["price"] = serde_json::json!("1.30");
        next["net_amount"] = serde_json::json!(260);
        next["trade_time_r"] = serde_json::json!(1790956805000_i64);
        let mut sales = std::collections::HashMap::new();
        for value in [&fill, &next] { let sale = PremiumSale::parse(value).unwrap(); sales.insert(sale.id.clone(), sale); }
        let accounts = vec!["U1".into()];
        let (rows, total) = premium_list(&sales, &accounts);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].quantity, "3");
        assert_eq!(rows[0].price, "1.20");
        assert_eq!(rows[0].proceeds, "360.00 USD");
        assert_eq!(total, "360.00 USD");
        next["execution_id"] = serde_json::json!("c");
        next["order_id"] = serde_json::json!(43);
        let sale = PremiumSale::parse(&next).unwrap(); sales.insert(sale.id.clone(), sale);
        assert_eq!(premium_list(&sales, &accounts).0.len(), 2);
        for sale in sales.values_mut() { sale.order_id = None; sale.at = chrono::DateTime::from_timestamp(1790956800, 0).unwrap(); }
        assert_eq!(premium_list(&sales, &accounts).0.len(), 1);
    }
    #[test]
    fn position_underlying_and_partial_option_ticks_keep_prices_live() {
        assert_eq!(underlying_conid(&serde_json::json!({"undConid":123})), Some(123));
        let mut cached = serde_json::json!({"31":"2.10", "84":"2.00", "86":"2.20", "7635":"2.10", "7308":"0.5"});
        merge_ticks(&mut cached, &serde_json::json!({"84":"2.15", "_updated":1000}));
        assert_eq!(number(&cached, &["7635"]), Some(2.15));
        assert_eq!(number(&cached, &["7308"]), Some(0.5));
        merge_ticks(&mut cached, &serde_json::json!({"86":"2.05", "_updated":2000}));
        assert_eq!(number(&cached, &["7635"]), Some(2.05));
        merge_ticks(&mut cached, &serde_json::json!({"7635":"2.07", "84":"2.06"}));
        assert_eq!(number(&cached, &["7635"]), Some(2.07));
    }
    #[test]
    fn share_cost_matches_account_underlying_and_currency() {
        let mut positions = vec![serde_json::json!({"assetClass":"STK", "_account":"U1",
            "conid":"123", "currency":"USD", "position":200, "avgPrice":"185.25", "avgCost":999})];
        assert_eq!(share_cost(&positions, "U1", 123, "USD"), Some(185.25));
        assert_eq!(share_cost(&positions, "U2", 123, "USD"), None);
        assert_eq!(share_cost(&positions, "U1", 456, "USD"), None);
        assert_eq!(share_cost(&positions, "U1", 123, "EUR"), None);
        positions[0]["avgPrice"] = Value::Null;
        positions[0]["avgCost"] = serde_json::json!(190.5);
        positions[0]["multiplier"] = serde_json::json!(0);
        assert_eq!(share_cost(&positions, "U1", 123, "USD"), Some(190.5));
        positions[0]["avgCost"] = Value::Null;
        assert_eq!(share_cost(&positions, "U1", 123, "USD"), None);
        positions[0]["avgPrice"] = serde_json::json!(185.25);
        positions[0]["position"] = serde_json::json!(-200);
        assert_eq!(share_cost(&positions, "U1", 123, "USD"), None);
    }
    #[test]
    fn classifies_calls_and_puts_using_underlying_price() {
        for (right, price, expected) in [
            ("Call", 201.0, "ITM"),
            ("Call", 199.0, "OTM"),
            ("Put", 201.0, "OTM"),
            ("Put", 199.0, "ITM"),
            ("Call", 200.004, "ATM"),
            ("Put", 200.0, "ATM"),
        ] {
            let position = serde_json::json!({"assetClass":"OPT", "position":-1,
                "strike":200, "right":right, "_underlyingPrice":price, "mktPrice":5});
            let result = super::row(
                &position,
                None,
                None,
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            )
            .unwrap();
            assert_eq!(result.moneyness, expected);
            assert_eq!(result.market_price, format!("{price:.2}"));
        }
        assert_eq!(super::moneyness(None, Some(200.0), "Call"), "—");
        assert_eq!(super::moneyness(Some(201.0), None, "Put"), "—");
        assert_eq!(super::moneyness(Some(201.0), Some(200.0), "—"), "—");
        assert_eq!(
            super::underlying_conid(&serde_json::json!({"6457":"123"})),
            Some(123)
        );
    }
    use super::*;
    #[test]
    fn parses_wrapped_ibkr_codes_and_calculates_signed_return() {
        let position = serde_json::json!({"assetClass":"OPT", "conid":123, "_account":"U1", "position":-1,
            "contractDesc":"BE OCT2026 295 C [BE  261002C00295000 100]", "currency":"USD", "expiry":null,
            "avgCost":122.31, "unrealizedPnl":56.97, "mktValue":-65.34});
        let r = row(
            &position,
            None,
            None,
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
        )
        .unwrap();
        assert_eq!(r.symbol, "BE");
        assert_eq!(r.contract_short, "295 Call");
        assert_eq!(r.expiry_short, "02 Oct 26");
        assert_eq!(r.dte, "1d");
        assert_eq!(r.pnl_percent, "+46.6%");
        assert_eq!(r.pnl_value, "+56.97 USD");
        assert_eq!(r.key, "U1:123");
        assert!(r.urgent);
        assert!(occ_details("No expiry ✓ 261332C00295000").is_none());
    }

    #[test]
    fn sorts_numerically_and_keeps_unknowns_last_in_both_directions() {
        let mut rows = vec![
            OptionView {
                symbol: "A".into(),
                pnl_number: Some(9.0),
                ..Default::default()
            },
            OptionView {
                symbol: "B".into(),
                pnl_number: Some(100.0),
                ..Default::default()
            },
            OptionView {
                symbol: "C".into(),
                ..Default::default()
            },
        ];
        sort(&mut rows, "pnl", false);
        assert_eq!(
            rows.iter().map(|r| r.symbol.as_str()).collect::<Vec<_>>(),
            ["B", "A", "C"]
        );
        sort(&mut rows, "pnl", true);
        assert_eq!(
            rows.iter().map(|r| r.symbol.as_str()).collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
    }

    #[test]
    fn totals_keep_currencies_separate_and_identify_missing_pnl() {
        let rows = vec![
            OptionView {
                currency: "USD".into(),
                pnl_number: Some(9.0),
                ..Default::default()
            },
            OptionView {
                currency: "EUR".into(),
                pnl_number: Some(-100.0),
                ..Default::default()
            },
            OptionView {
                currency: "USD".into(),
                pnl_number: Some(1.0),
                ..Default::default()
            },
            OptionView::default(),
        ];
        assert_eq!(
            pnl_summary(&rows),
            "-100.00 EUR  ·  +10.00 USD (1 unavailable)"
        );
    }

    #[test]
    fn preserves_short_sign_and_live_greeks_and_flags_expiry() {
        let p = serde_json::json!({"assetClass":"OPT", "position":-2, "contractDesc":"AAPL CALL", "_account":"U1", "currency":"USD", "expiry":"20261002", "strike":200, "mktValue":-420, "unrealizedPnl":35});
        let tick = serde_json::json!({"7308":"0.42", "84":"2.0", "86":"2.2"});
        let r = row(
            &p,
            None,
            Some(&tick),
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
        )
        .unwrap();
        assert!(r.position.contains("Short 2"));
        assert!(r.position.contains("-420.00"));
        assert!(r.greeks.contains("0.42"));
        assert!(r.urgent);
        assert!(r.expiry.contains("1 day"));
    }
    #[test]
    fn decodes_occ_symbol_without_assuming_multiplier() {
        let r = row(&serde_json::json!({"assetClass":"OPT","position":1,"contractDesc":"AAPL  261002P00200000"}),None,None,NaiveDate::from_ymd_opt(2026,10,1).unwrap()).unwrap();
        assert!(r.contract.contains("Put  ·  strike 200.00"));
        assert!(r.expiry.contains("2026-10-02"));
        assert!(r.account.contains("multiplier —"));
    }
    #[test]
    fn excludes_stocks_and_closed_options_and_does_not_invent_data() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert!(row(
            &serde_json::json!({"assetClass":"STK","position":2}),
            None,
            None,
            day
        )
        .is_none());
        assert!(row(
            &serde_json::json!({"assetClass":"OPT","position":0}),
            None,
            None,
            day
        )
        .is_none());
        let r = row(
            &serde_json::json!({"assetClass":"FOP","position":1}),
            None,
            None,
            day,
        )
        .unwrap();
        assert!(r.greeks.contains("Delta —"));
        assert!(!r.urgent);
    }
}
