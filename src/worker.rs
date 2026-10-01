use crate::market::{self, ClosePrices};
use crate::model::{recent_sales, Execution, Position, Quote, Sale, Settings};
use crate::portal::{Candle, Portal};
use chrono::{Datelike, Local, Months, NaiveDate, Utc};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
pub enum Command {
    Refresh,
    Select(String),
    Filter(String),
    Period(String),
    Sort(String),
    Target(String, String),
    Alert(bool),
    Pin,
    Reset,
    Snooze,
}

#[derive(Clone, Debug)]
pub struct RowView {
    pub key: String,
    pub symbol: String,
    pub date: String,
    pub sale: String,
    pub price: String,
    pub move_text: String,
    pub move_tone: i32,
    pub target: String,
    pub best_buy: String,
    pub best_buy_highlight: bool,
    pub average_buy: String,
    pub average_buy_highlight: bool,
    pub pinned: bool,
    pub selected: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ChartBarView {
    pub x: f32,
    pub width: f32,
    pub wick_top: f32,
    pub wick_height: f32,
    pub body_top: f32,
    pub body_height: f32,
    pub up: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ChartData {
    pub bars: Vec<ChartBarView>,
    pub status: String,
    pub caption: String,
    pub high: String,
    pub low: String,
    pub first_time: String,
    pub last_time: String,
}

#[derive(Clone, Debug)]
pub struct View {
    pub rows: Vec<RowView>,
    pub status: String,
    pub count: String,
    pub selected_key: String,
    pub selected_symbol: String,
    pub selected_detail: String,
    pub selected_quote: String,
    pub selected_chart: ChartData,
    pub selected_target: String,
    pub selected_mode: String,
    pub selected_alert: bool,
    pub alert_text: String,
    pub sort_column: String,
    pub sort_ascending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortColumn {
    Symbol,
    Date,
    Exit,
    Market,
    Delta,
    Target,
    BestBuy,
    AverageBuy,
}

impl SortColumn {
    fn parse(id: &str) -> Option<Self> {
        Some(match id {
            "symbol" => Self::Symbol,
            "date" => Self::Date,
            "exit" => Self::Exit,
            "market" => Self::Market,
            "delta" => Self::Delta,
            "target" => Self::Target,
            "best_buy" => Self::BestBuy,
            "average_buy" => Self::AverageBuy,
            _ => return None,
        })
    }

    fn id(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::Date => "date",
            Self::Exit => "exit",
            Self::Market => "market",
            Self::Delta => "delta",
            Self::Target => "target",
            Self::BestBuy => "best_buy",
            Self::AverageBuy => "average_buy",
        }
    }

    fn first_ascending(self) -> bool {
        matches!(self, Self::Symbol | Self::Delta)
    }
}

enum NetworkEvent {
    Snapshot(anyhow::Result<(Vec<String>, Vec<Value>, Vec<Value>)>),
    History(String, anyhow::Result<Vec<Candle>>),
    Close(i64, NaiveDate, anyhow::Result<Option<(NaiveDate, f64)>>),
    CostHistory(Vec<String>, anyhow::Result<HashMap<String, Vec<Execution>>>),
}

struct HistoricalClose {
    cycle: NaiveDate,
    date: NaiveDate,
    price: f64,
}

struct State {
    accounts: Vec<String>,
    executions: HashMap<String, Execution>,
    cost_history: HashMap<String, Vec<Execution>>,
    cost_estimates: HashMap<String, (Option<f64>, Option<f64>)>,
    cost_pending: HashSet<String>,
    cost_attempts: HashMap<String, i64>,
    positions: Vec<Position>,
    history_contracts: HashMap<String, (String, i64, String)>,
    quotes: HashMap<i64, Quote>,
    overnight_quotes: HashMap<i64, Quote>,
    closes: HashMap<i64, (NaiveDate, ClosePrices)>,
    historical_closes: HashMap<i64, HistoricalClose>,
    close_pending: HashSet<i64>,
    close_attempts: HashMap<i64, (NaiveDate, i64)>,
    close_request_times: VecDeque<i64>,
    settings: Settings,
    selected: String,
    filter: String,
    period: String,
    sort_column: Option<SortColumn>,
    sort_ascending: bool,
    status: String,
    alert_text: String,
    charts: HashMap<String, ChartData>,
    chart_requested: HashMap<String, i64>,
    chart_pending: HashSet<String>,
    refreshing: bool,
    ws_connected: bool,
    snapshot_ok: bool,
    positions_loaded: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            executions: HashMap::new(),
            cost_history: HashMap::new(),
            cost_estimates: HashMap::new(),
            cost_pending: HashSet::new(),
            cost_attempts: HashMap::new(),
            positions: Vec::new(),
            history_contracts: HashMap::new(),
            quotes: HashMap::new(),
            overnight_quotes: HashMap::new(),
            closes: HashMap::new(),
            historical_closes: HashMap::new(),
            close_pending: HashSet::new(),
            close_attempts: HashMap::new(),
            close_request_times: VecDeque::new(),
            settings: Settings::new(),
            selected: String::new(),
            filter: "All exits".into(),
            period: "Inception".into(),
            sort_column: Some(SortColumn::Delta),
            sort_ascending: true,
            status: "Connecting to Client Portal…".into(),
            alert_text: String::new(),
            charts: HashMap::new(),
            chart_requested: HashMap::new(),
            chart_pending: HashSet::new(),
            refreshing: false,
            ws_connected: false,
            snapshot_ok: false,
            positions_loaded: false,
        }
    }
}

fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ibkr-companion")
        .join("settings.json")
}

fn archive_path() -> PathBuf {
    settings_path().with_file_name("executions.json")
}

fn cost_history_path() -> PathBuf {
    settings_path().with_file_name("purchase-history.json")
}

fn cost_requests_path() -> PathBuf {
    settings_path().with_file_name("purchase-history-requests.json")
}

async fn write_json<T: Serialize>(path: PathBuf, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension("json.tmp");
    let data = serde_json::to_vec(value)?;
    tokio::fs::write(&temporary, data).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

async fn save_archive(executions: &HashMap<String, Execution>) -> anyhow::Result<()> {
    write_json(archive_path(), executions).await
}

async fn save_cost_history(history: &HashMap<String, Vec<Execution>>) -> anyhow::Result<()> {
    write_json(cost_history_path(), history).await
}

async fn save_settings(settings: Settings) -> anyhow::Result<()> {
    write_json(settings_path(), &settings).await
}

fn start_refresh(portal: &Portal, tx: &mpsc::Sender<NetworkEvent>, state: &mut State) {
    if state.refreshing {
        crate::diagnostics::debug(format_args!("Snapshot refresh already running"));
        return;
    }
    crate::diagnostics::debug(format_args!("Starting Client Portal snapshot refresh"));
    state.refreshing = true;
    state.status = "Updating Client Portal…".into();
    let portal = portal.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            let accounts = portal.accounts().await?;
            let (positions, trades) =
                tokio::try_join!(portal.positions(&accounts), portal.trades())?;
            Ok((accounts, positions, trades))
        }
        .await;
        let _ = tx.send(NetworkEvent::Snapshot(result)).await;
    });
}

fn start_close_refresh(portal: &Portal, tx: &mpsc::Sender<NetworkEvent>, state: &mut State) {
    if !state.positions_loaded {
        return;
    }
    let now = Utc::now();
    let cycle = market::close_cycle_date(now);
    let now_ms = now.timestamp_millis();
    while state
        .close_request_times
        .front()
        .is_some_and(|at| now_ms - at >= 60_000)
    {
        state.close_request_times.pop_front();
    }
    let sales = observed_sales(state);
    let mut conids = Vec::new();
    let mut seen = HashSet::new();
    for sale in &sales {
        if seen.insert(sale.conid) {
            conids.push(sale.conid);
        }
    }
    if let Some(selected) = sales.iter().find(|sale| sale.key == state.selected) {
        if let Some(index) = conids.iter().position(|conid| *conid == selected.conid) {
            conids.swap(0, index);
        }
    }
    let available = 30usize.saturating_sub(state.close_request_times.len());
    let mut scheduled = 0;
    for conid in conids {
        if scheduled >= available {
            break;
        }
        if state.close_pending.contains(&conid)
            || state
                .historical_closes
                .get(&conid)
                .is_some_and(|close| close.cycle == cycle)
            || state
                .close_attempts
                .get(&conid)
                .is_some_and(|(last_cycle, at)| *last_cycle == cycle && now_ms - *at < 5 * 60_000)
        {
            continue;
        }
        state.close_pending.insert(conid);
        state.close_attempts.insert(conid, (cycle, now_ms));
        state
            .close_request_times
            .push_back(now_ms + scheduled as i64 * 350);
        let tx = tx.clone();
        let portal = portal.clone();
        let delay = Duration::from_millis(scheduled as u64 * 350);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let result = portal.regular_close(conid, cycle).await;
            let _ = tx.send(NetworkEvent::Close(conid, cycle, result)).await;
        });
        scheduled += 1;
    }
    if scheduled > 0 {
        crate::diagnostics::debug(format_args!(
            "Scheduled {scheduled} Client Portal regular-close lookups"
        ));
    }
}

fn start_chart_refresh(portal: &Portal, tx: &mpsc::Sender<NetworkEvent>, state: &mut State) {
    let key = state.selected.clone();
    if key.is_empty() || state.chart_pending.contains(&key) {
        return;
    }
    let now_ms = Utc::now().timestamp_millis();
    if state
        .chart_requested
        .get(&key)
        .is_some_and(|at| now_ms - *at < 2 * 60_000)
    {
        return;
    }
    let Some(sale) = observed_sales(state)
        .into_iter()
        .find(|sale| sale.key == key)
    else {
        return;
    };
    state.chart_requested.insert(key.clone(), now_ms);
    state.chart_pending.insert(key.clone());
    state
        .charts
        .entry(key.clone())
        .or_insert_with(|| ChartData {
            status: "Loading IBKR candlesticks…".into(),
            ..Default::default()
        });
    let portal = portal.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = portal.history(sale.conid).await;
        let _ = tx.send(NetworkEvent::History(key, result)).await;
    });
}

fn chart_from_candles(candles: &[Candle]) -> ChartData {
    let latest_date = candles
        .iter()
        .filter_map(|bar| {
            chrono::DateTime::<Utc>::from_timestamp_millis(bar.at_ms).map(market::eastern_date)
        })
        .max();
    let Some(latest_date) = latest_date else {
        return ChartData {
            status: "No IBKR regular-session candles available".into(),
            ..Default::default()
        };
    };
    let latest: Vec<&Candle> = candles
        .iter()
        .filter(|bar| {
            chrono::DateTime::<Utc>::from_timestamp_millis(bar.at_ms)
                .is_some_and(|at| market::eastern_date(at) == latest_date)
        })
        .collect();
    let high = latest
        .iter()
        .map(|bar| bar.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let low = latest
        .iter()
        .map(|bar| bar.low)
        .fold(f64::INFINITY, f64::min);
    let spread = (high - low).max(high * 0.0005).max(0.01);
    let top = high + spread * 0.06;
    let scale = (high - low) + spread * 0.12;
    let y = |price: f64| ((top - price) / scale) as f32;
    let count = latest.len() as f32;
    let bars = latest
        .iter()
        .enumerate()
        .map(|(index, bar)| ChartBarView {
            x: (index as f32 + 0.16) / count,
            width: 0.68 / count,
            wick_top: y(bar.high),
            wick_height: y(bar.low) - y(bar.high),
            body_top: y(bar.open.max(bar.close)),
            body_height: (y(bar.open) - y(bar.close)).abs(),
            up: bar.close >= bar.open,
        })
        .collect();
    let time = |bar: &&Candle| {
        let at = chrono::DateTime::<Utc>::from_timestamp_millis(bar.at_ms).unwrap();
        market::eastern_time(at).format("%H:%M").to_string()
    };
    ChartData {
        bars,
        status: format!(
            "Open {:.2} · Close {:.2}",
            latest[0].open,
            latest.last().unwrap().close
        ),
        caption: format!(
            "{} · 15-minute regular session",
            latest_date.format("%b %d")
        ),
        high: format!("{high:.2}"),
        low: format!("{low:.2}"),
        first_time: time(&latest[0]),
        last_time: time(latest.last().unwrap()),
    }
}

fn ingest_trades(state: &mut State, rows: &[Value]) -> bool {
    let mut changed = false;
    let allowed: HashSet<&str> = state.accounts.iter().map(String::as_str).collect();
    for row in rows {
        if let Some(fill) = Execution::parse(row) {
            if !allowed.is_empty() && !allowed.contains(fill.account.as_str()) {
                continue;
            }
            if !state.executions.contains_key(&fill.id) {
                state.executions.insert(fill.id.clone(), fill);
                changed = true;
            }
        }
    }
    changed
}

fn merged_executions(state: &State) -> HashMap<String, Execution> {
    let mut earliest = HashMap::<(String, i64), NaiveDate>::new();
    for fill in state.executions.values() {
        let key = (fill.account.clone(), fill.conid);
        let day = market::eastern_date(fill.at);
        earliest
            .entry(key)
            .and_modify(|current| *current = (*current).min(day))
            .or_insert(day);
    }
    let mut merged = state.executions.clone();
    for history in state.cost_history.values() {
        for fill in history {
            let key = (fill.account.clone(), fill.conid);
            if earliest
                .get(&key)
                .is_none_or(|first_day| fill.at.date_naive() < *first_day)
            {
                let mut fill = fill.clone();
                // PA dates have no execution time; normalize old midnight cache entries.
                fill.at += chrono::Duration::hours(12);
                merged.insert(fill.id.clone(), fill);
            }
        }
    }
    merged
}

fn rebuild_cost_estimates(state: &mut State) {
    // All purchases across all cycles, independent of the exit-date filter.
    let mut totals = HashMap::<String, (f64, f64, f64)>::new();
    for fill in merged_executions(state).values() {
        if fill.side != crate::model::Side::Buy || fill.size <= 0.0 {
            continue;
        }
        let total = totals
            .entry(format!("{}:{}", fill.account, fill.conid))
            .or_insert((0.0, 0.0, f64::INFINITY));
        total.0 += fill.size;
        total.1 += fill.size * fill.price;
        total.2 = total.2.min(fill.price);
    }
    state.cost_estimates = totals
        .into_iter()
        .map(|(key, (quantity, cost, best))| (key, (Some(cost / quantity), Some(best))))
        .collect();
}

fn start_cost_refresh(portal: &Portal, tx: &mpsc::Sender<NetworkEvent>, state: &mut State) {
    if !state.positions_loaded {
        return;
    }
    let now_ms = Utc::now().timestamp_millis();
    // Client Portal limits /pa/transactions to one request per 15 minutes.
    if state
        .cost_attempts
        .values()
        .any(|last| now_ms - last < 15 * 60 * 1000)
    {
        return;
    }
    let mut candidates = state.history_contracts.clone();
    for fill in state
        .executions
        .values()
        .chain(state.cost_history.values().flatten())
    {
        candidates.insert(
            format!("{}:{}", fill.account, fill.conid),
            (fill.account.clone(), fill.conid, fill.symbol.clone()),
        );
    }
    let mut missing: Vec<_> = candidates
        .into_iter()
        .filter(|(key, (account, _, _))| {
            state.accounts.contains(account)
                && !state.cost_pending.contains(key)
                && !state
                    .cost_attempts
                    .get(key)
                    .is_some_and(|last| now_ms - last < 6 * 60 * 60 * 1000)
        })
        .collect();
    // Oldest request first prevents a large portfolio starving later contracts.
    missing.sort_by_key(|(key, (_, conid, _))| {
        (
            state.cost_history.contains_key(key),
            state
                .positions
                .iter()
                .any(|p| p.conid == *conid && p.quantity.abs() > 1e-7),
            state.cost_attempts.get(key).copied().unwrap_or(0),
            key.clone(),
        )
    });
    let Some(account) = missing.first().map(|(_, (account, _, _))| account.clone()) else {
        return;
    };
    // This Gateway supports multiple conids per request (verified with 19 symbols).
    // Keep requests globally paced; batch per account rather than pacing each stock.
    let batch: HashMap<_, _> = missing
        .into_iter()
        .filter(|(_, (candidate, _, _))| candidate == &account)
        .take(19)
        .collect();
    let keys: Vec<_> = batch.keys().cloned().collect();
    let conids: Vec<_> = batch.values().map(|(_, conid, _)| *conid).collect();
    for key in &keys {
        state.cost_pending.insert(key.clone());
        state.cost_attempts.insert(key.clone(), now_ms);
    }
    let tx = tx.clone();
    let portal = portal.clone();
    let attempts = state.cost_attempts.clone();
    crate::diagnostics::info(format_args!(
        "Loading Client Portal purchase/sale history for {} stocks",
        conids.len()
    ));
    tokio::spawn(async move {
        if let Err(error) = write_json(cost_requests_path(), &attempts).await {
            crate::diagnostics::warn(format_args!(
                "Could not save history request timing: {error}"
            ));
        }
        let result = portal
            .transactions(&account, &conids)
            .await
            .and_then(|rows| parse_transaction_batch(&rows, &batch));
        let _ = tx.send(NetworkEvent::CostHistory(keys, result)).await;
    });
}

fn parse_transaction_batch(
    rows: &[Value],
    contracts: &HashMap<String, (String, i64, String)>,
) -> anyhow::Result<HashMap<String, Vec<Execution>>> {
    let mut result: HashMap<String, Vec<Execution>> = contracts
        .keys()
        .map(|key| (key.clone(), Vec::new()))
        .collect();
    for (index, row) in rows.iter().enumerate() {
        let kind = row.get("type").and_then(Value::as_str).unwrap_or("");
        if !kind.starts_with("Buy") && !kind.starts_with("Sell") {
            continue;
        }
        let account = row
            .get("acctid")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("History transaction is missing account"))?;
        let conid = row
            .get("conid")
            .and_then(crate::model::integer)
            .ok_or_else(|| anyhow::anyhow!("History transaction is missing contract"))?;
        let key = format!("{account}:{conid}");
        let (_, _, symbol) = contracts
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("History returned an unrequested stock: {conid}"))?;
        let execution = Execution::parse_pa_transaction(row, account, conid, symbol, index)
            .ok_or_else(|| anyhow::anyhow!("Invalid {kind} history record for {symbol}"))?;
        result.get_mut(&key).unwrap().push(execution);
    }
    Ok(result)
}

/// Recover a captured Client Portal response using the same parser as live batches.
/// The UI must be closed so that it cannot overwrite the recovered cache.
pub async fn recover_client_portal_history(portal: Portal, path: PathBuf) -> anyhow::Result<()> {
    let data: Value = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    let contracts: HashMap<String, (String, i64, String)> =
        serde_json::from_value(data["contracts"].clone())?;
    let rows = data["transactions"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing transactions"))?;
    let recovered = parse_transaction_batch(rows, &contracts)?;
    let mut state = State::default();
    state.accounts = portal.accounts().await?;
    let positions = portal.positions(&state.accounts).await?;
    state.positions = positions.iter().filter_map(Position::parse).collect();
    state.positions_loaded = true;
    if let Ok(bytes) = tokio::fs::read(archive_path()).await {
        state.executions = serde_json::from_slice(&bytes)?;
    }
    if let Ok(bytes) = tokio::fs::read(cost_history_path()).await {
        state.cost_history = serde_json::from_slice(&bytes)?;
        let backup = cost_history_path().with_file_name(format!(
            "purchase-history-backup-{}.json",
            Utc::now().timestamp_millis()
        ));
        tokio::fs::write(backup, bytes).await?;
    }
    state.cost_history.extend(recovered);
    rebuild_cost_estimates(&mut state);
    let rendered = view(&state);
    for (_, (_, _, symbol)) in &contracts {
        anyhow::ensure!(
            rendered.rows.iter().any(|row| row.symbol == *symbol),
            "Recovered stock {symbol} is not visible; cache has not been changed"
        );
    }
    if let Ok(bytes) = tokio::fs::read(cost_requests_path()).await {
        state.cost_attempts = serde_json::from_slice(&bytes)?;
    }
    let fetched = data["fetched_at"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing response timestamp"))?;
    for key in contracts.keys() {
        state.cost_attempts.insert(key.clone(), fetched);
    }
    let registry_path = settings_path().with_file_name("client-portal-contracts.json");
    if let Ok(bytes) = tokio::fs::read(&registry_path).await {
        state.history_contracts = serde_json::from_slice(&bytes)?;
    }
    state.history_contracts.extend(contracts.clone());
    save_cost_history(&state.cost_history).await?;
    write_json(cost_requests_path(), &state.cost_attempts).await?;
    write_json(registry_path, &state.history_contracts).await?;
    let mut symbols: Vec<_> = contracts
        .values()
        .map(|(_, _, symbol)| symbol.as_str())
        .collect();
    symbols.sort();
    crate::diagnostics::info(format_args!(
        "Verified {} recovered stocks in rendered exit rows: {}",
        symbols.len(),
        symbols.join(", ")
    ));
    crate::diagnostics::info(format_args!(
        "{} total unheld exits; {} returned history records recovered",
        rendered.rows.len(),
        rows.len()
    ));
    Ok(())
}

fn observed_sales(state: &State) -> Vec<Sale> {
    if !state.positions_loaded {
        return Vec::new();
    }
    recent_sales(&merged_executions(state), &state.positions)
        .into_iter()
        .map(|mut sale| {
            if let Some((average, best)) = state.cost_estimates.get(&sale.key) {
                sale.average_cost = *average;
                sale.best_buy = *best;
            }
            sale
        })
        .filter(|sale| {
            state
                .accounts
                .iter()
                .any(|account| account == &sale.account)
        })
        .collect()
}

fn change_sort(state: &mut State, id: &str) {
    let Some(column) = SortColumn::parse(id) else {
        return;
    };
    if state.sort_column == Some(column) {
        state.sort_ascending = !state.sort_ascending;
    } else {
        state.sort_column = Some(column);
        state.sort_ascending = column.first_ascending();
    }
}

fn compare_price_option(left: Option<f64>, right: Option<f64>, ascending: bool) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => {
            let result = left.total_cmp(&right);
            if ascending {
                result
            } else {
                result.reverse()
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn sort_sales(sales: &mut [Sale], state: &State, now: chrono::DateTime<Utc>) {
    sales.sort_by(|left, right| {
        let Some(column) = state.sort_column else {
            let left_pin = state.settings.get(&left.key).is_some_and(|s| s.pinned);
            let right_pin = state.settings.get(&right.key).is_some_and(|s| s.pinned);
            return right_pin
                .cmp(&left_pin)
                .then_with(|| right.at.cmp(&left.at))
                .then_with(|| left.key.cmp(&right.key));
        };
        let ascending = state.sort_ascending;
        let result = match column {
            SortColumn::Symbol => {
                let result = left.symbol.cmp(&right.symbol);
                if ascending {
                    result
                } else {
                    result.reverse()
                }
            }
            SortColumn::Date => {
                let result = left.at.cmp(&right.at);
                if ascending {
                    result
                } else {
                    result.reverse()
                }
            }
            SortColumn::Exit => {
                compare_price_option(Some(left.price), Some(right.price), ascending)
            }
            SortColumn::Market => compare_price_option(
                display_price(state, left.conid, now).map(|quote| quote.price),
                display_price(state, right.conid, now).map(|quote| quote.price),
                ascending,
            ),
            SortColumn::Delta => compare_price_option(
                display_price(state, left.conid, now)
                    .map(|quote| (left.price - quote.price) / left.price * 100.0),
                display_price(state, right.conid, now)
                    .map(|quote| (right.price - quote.price) / right.price * 100.0),
                ascending,
            ),
            SortColumn::Target => compare_price_option(
                state
                    .settings
                    .get(&left.key)
                    .and_then(|s| s.level(left.price)),
                state
                    .settings
                    .get(&right.key)
                    .and_then(|s| s.level(right.price)),
                ascending,
            ),
            SortColumn::BestBuy => compare_price_option(left.best_buy, right.best_buy, ascending),
            SortColumn::AverageBuy => {
                compare_price_option(left.average_cost, right.average_cost, ascending)
            }
        };
        result
            .then_with(|| right.at.cmp(&left.at))
            .then_with(|| left.key.cmp(&right.key))
    });
}

fn observed_alerts(state: &mut State, conid: i64) {
    let now = Utc::now();
    let Some((quote, _)) = market::live_quote(
        now,
        state.quotes.get(&conid),
        state.overnight_quotes.get(&conid),
    ) else {
        return;
    };
    let now = now.timestamp_millis();
    for sale in observed_sales(state)
        .into_iter()
        .filter(|sale| sale.conid == conid)
    {
        let setting = state.settings.entry(sale.key.clone()).or_default();
        if setting.observe(quote, sale.price, now) {
            let level = setting.level(sale.price).unwrap_or_default();
            state.alert_text = format!(
                "{} reached target {:.2} at IBKR {:.2}.",
                sale.symbol, level, quote.price
            );
        }
    }
}

struct DisplayPrice {
    price: f64,
    label: String,
    updated_ms: Option<i64>,
}

fn display_price(state: &State, conid: i64, now: chrono::DateTime<Utc>) -> Option<DisplayPrice> {
    market::live_quote(
        now,
        state.quotes.get(&conid),
        state.overnight_quotes.get(&conid),
    )
    .map(|(quote, source)| DisplayPrice {
        price: quote.price,
        label: if quote.real_time {
            source.label().into()
        } else {
            source.label().replace("live", "delayed")
        },
        updated_ms: Some(quote.updated_ms),
    })
    .or_else(|| {
        state.historical_closes.get(&conid).and_then(|close| {
            (close.cycle == market::close_cycle_date(now)).then(|| DisplayPrice {
                price: close.price,
                label: format!("IBKR regular close {}", close.date.format("%b %d")),
                updated_ms: None,
            })
        })
    })
    .or_else(|| {
        state
            .closes
            .get(&conid)
            .filter(|(cycle, _)| *cycle == market::close_cycle_date(now))
            .and_then(|(_, prices)| prices.display())
            .map(|(price, label)| DisplayPrice {
                price,
                label: label.into(),
                updated_ms: None,
            })
    })
}

fn period_start(period: &str, today: NaiveDate) -> Option<NaiveDate> {
    match period {
        "1 Week" => Some(today - chrono::Duration::days(6)),
        "1 Month" => today.checked_sub_months(Months::new(1)),
        "1 Year" => today.checked_sub_months(Months::new(12)),
        "Year to Date" => NaiveDate::from_ymd_opt(today.year(), 1, 1),
        _ => None,
    }
}

fn view(state: &State) -> View {
    view_at(state, Utc::now())
}

fn view_at(state: &State, now: chrono::DateTime<Utc>) -> View {
    let mut sales = observed_sales(state);
    sort_sales(&mut sales, state, now);
    let total = sales.iter().filter(|sale| sale.held.abs() <= 1e-7).count();
    let today = market::eastern_date(now);
    let eligible: Vec<&Sale> = sales
        .iter()
        .filter(|sale| {
            if period_start(&state.period, today)
                .is_some_and(|start| market::eastern_date(sale.at) < start)
            {
                return false;
            }
            let setting = state.settings.get(&sale.key);
            let quote = display_price(state, sale.conid, now);
            match state.filter.as_str() {
                "Below exit" => quote.as_ref().is_some_and(|q| q.price < sale.price),
                "Target reached" => quote.as_ref().zip(setting).is_some_and(|(q, s)| {
                    s.level(sale.price).is_some_and(|level| q.price <= level)
                }),
                "Pinned" => setting.is_some_and(|s| s.pinned),
                _ => true,
            }
        })
        .collect();
    let visible: Vec<&Sale> = eligible
        .into_iter()
        .filter(|sale| sale.held.abs() <= 1e-7)
        .collect();
    let rows =
        visible
            .iter()
            .map(|sale| {
                let quote = display_price(state, sale.conid, now);
                let setting = state.settings.get(&sale.key);
                let price = quote
                    .as_ref()
                    .map(|q| format!("{:.2}", q.price))
                    .unwrap_or_else(|| "—".into());
                let move_text = quote
                    .as_ref()
                    .map(|q| format!("{:+.2}%", (sale.price - q.price) / sale.price * 100.0))
                    .unwrap_or_else(|| "—".into());
                let move_tone = quote.as_ref().map_or(0, |q| {
                    if q.price < sale.price {
                        1
                    } else if q.price > sale.price {
                        -1
                    } else {
                        0
                    }
                });
                let target = setting
                    .and_then(|s| s.level(sale.price))
                    .map(|v| format!("{v:.2}"))
                    .unwrap_or_else(|| "—".into());
                RowView {
                    key: sale.key.clone(),
                    symbol: sale.symbol.clone(),
                    date: if state.executions.values().any(|f| {
                        f.account == sale.account && f.conid == sale.conid && f.at == sale.at
                    }) {
                        sale.at
                            .with_timezone(&Local)
                            .format("%b %d %H:%M")
                            .to_string()
                    } else {
                        sale.at.format("%b %d %Y").to_string()
                    },
                    sale: format!("{:.2}", sale.price),
                    price,
                    move_text,
                    move_tone,
                    target,
                    best_buy: sale
                        .best_buy
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "—".into()),
                    best_buy_highlight: quote
                        .as_ref()
                        .zip(sale.best_buy)
                        .is_some_and(|(market, buy)| market.price < buy),
                    average_buy: sale
                        .average_cost
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "—".into()),
                    average_buy_highlight: quote
                        .as_ref()
                        .zip(sale.average_cost)
                        .is_some_and(|(market, buy)| market.price < buy),
                    pinned: setting.is_some_and(|s| s.pinned),
                    selected: sale.key == state.selected,
                }
            })
            .collect();
    let selected = sales.iter().find(|s| s.key == state.selected);
    let selected_quote = selected.and_then(|sale| display_price(state, sale.conid, now));
    let selected_setting = selected.and_then(|sale| state.settings.get(&sale.key));
    let detail = selected
        .map(|sale| {
            let mut text = format!(
                "Exited at {:.2} on {}.\nShares still held: {:.4}.",
                sale.price,
                sale.at.with_timezone(&Local).format("%b %d %H:%M"),
                sale.held
            );
            if let Some(cost) = sale.average_cost {
                text.push_str(&format!("\nVWAP buy: {cost:.2}."));
            }
            if let Some(best) = sale.best_buy {
                text.push_str(&format!(" Best observed buy: {best:.2}."));
            }
            text
        })
        .unwrap_or_else(|| "Select a stock to inspect its exit and set a target.".into());
    View {
        rows,
        status: state.status.clone(),
        count: format!(
            "{}: {} shown / {} loaded exits",
            state.period,
            visible.len(),
            total
        ),
        selected_key: state.selected.clone(),
        selected_symbol: selected
            .map(|s| s.symbol.clone())
            .unwrap_or_else(|| "Select a stock".into()),
        selected_detail: detail,
        selected_quote: selected_quote
            .map(|q| {
                let timestamp = q
                    .updated_ms
                    .and_then(chrono::DateTime::<Utc>::from_timestamp_millis)
                    .map(|t| format!(" · updated {}", t.with_timezone(&Local).format("%H:%M:%S")))
                    .unwrap_or_default();
                format!("{} · {:.2}{timestamp}", q.label, q.price)
            })
            .unwrap_or_else(|| "Waiting for a Client Portal live quote or close".into()),
        selected_chart: state
            .charts
            .get(&state.selected)
            .cloned()
            .unwrap_or_else(|| ChartData {
                status: "Select a stock to load IBKR candlesticks".into(),
                ..Default::default()
            }),
        selected_target: selected_setting
            .and_then(|s| s.target)
            .map(|v| format!("{v}"))
            .unwrap_or_default(),
        selected_mode: if selected_setting.is_some_and(|s| s.percent) {
            "% below exit"
        } else {
            "Price"
        }
        .into(),
        selected_alert: selected_setting.is_some_and(|s| s.alert),
        alert_text: state.alert_text.clone(),
        sort_column: state.sort_column.map(SortColumn::id).unwrap_or("").into(),
        sort_ascending: state.sort_ascending,
    }
}

pub async fn run(
    portal: Portal,
    mut commands: mpsc::UnboundedReceiver<Command>,
    ui: slint::Weak<crate::AppWindow>,
) {
    let mut state = State::default();
    if let Ok(bytes) = tokio::fs::read(settings_path()).await {
        if let Ok(settings) = serde_json::from_slice(&bytes) {
            state.settings = settings;
        }
    }
    if let Ok(bytes) = tokio::fs::read(archive_path()).await {
        if let Ok(executions) = serde_json::from_slice(&bytes) {
            state.executions = executions;
        }
    }
    if let Ok(bytes) = tokio::fs::read(cost_history_path()).await {
        if let Ok(history) = serde_json::from_slice(&bytes) {
            state.cost_history = history;
        }
    }
    if let Ok(bytes) = tokio::fs::read(cost_requests_path()).await {
        if let Ok(attempts) = serde_json::from_slice(&bytes) {
            state.cost_attempts = attempts;
        }
    }
    // Optional migration contains only contract IDs observed in Client Portal snapshots.
    let registry_path = settings_path().with_file_name("client-portal-contracts.json");
    let mut registry_paths = vec![registry_path.clone()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(directory) = exe.parent() {
            registry_paths.push(directory.join("client-portal-contracts.json"));
        }
    }
    for path in registry_paths {
        if let Ok(bytes) = tokio::fs::read(&path).await {
            match serde_json::from_slice::<HashMap<String, (String, i64, String)>>(&bytes) {
                Ok(contracts) => state.history_contracts.extend(contracts),
                Err(error) => crate::diagnostics::warn(format_args!(
                    "Invalid saved contract registry: {error}"
                )),
            }
        }
    }
    crate::diagnostics::info(format_args!(
        "Recovered {} previously observed Client Portal stock identifiers",
        state.history_contracts.len()
    ));
    crate::diagnostics::info(format_args!(
        "Loaded {} saved targets, {} archived executions, and {} purchase histories",
        state.settings.len(),
        state.executions.len(),
        state.cost_history.len()
    ));
    let (sub_tx, sub_rx) = watch::channel(Vec::<i64>::new());
    let (stream_tx, mut stream_rx) = mpsc::channel::<Value>(256);
    tokio::spawn(portal.clone().stream(sub_rx, stream_tx));
    let (network_tx, mut network_rx) = mpsc::channel::<NetworkEvent>(16);
    start_refresh(&portal, &network_tx, &mut state);
    let mut timer = tokio::time::interval(Duration::from_secs(30));
    loop {
        let mut save = false;
        let mut archive_changed = false;
        let mut cost_history_changed = false;
        let mut contracts_changed = false;
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Refresh) => {
                    crate::diagnostics::info(format_args!("Manual refresh requested"));
                    start_refresh(&portal, &network_tx, &mut state);
                    state.chart_requested.remove(&state.selected);
                    start_chart_refresh(&portal, &network_tx, &mut state);
                }
                Some(Command::Select(key)) => {
                    state.selected = key;
                    start_chart_refresh(&portal, &network_tx, &mut state);
                    start_close_refresh(&portal, &network_tx, &mut state);
                }
                Some(Command::Filter(value)) => state.filter = value,
                Some(Command::Period(value)) => state.period = value,
                Some(Command::Sort(column)) => change_sort(&mut state, &column),
                Some(Command::Target(mode, text)) => {
                    if !state.selected.is_empty() {
                        let parsed = text.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0 && (mode != "% below exit" || *v < 100.0));
                        let setting = state.settings.entry(state.selected.clone()).or_default();
                        if text.trim().is_empty() || parsed.is_some() {
                            setting.target = parsed; setting.percent = mode == "% below exit";
                            setting.fired = false; setting.previous_reached = None; save = true;
                            state.alert_text = "Target saved locally.".into();
                        } else { state.alert_text = "Enter a positive price or a percentage below 100.".into(); }
                    }
                }
                Some(Command::Alert(value)) => { if !state.selected.is_empty() {
                    let setting = state.settings.entry(state.selected.clone()).or_default();
                    setting.alert = value; setting.fired = false; setting.previous_reached = None; save = true;
                } }
                Some(Command::Pin) => { if !state.selected.is_empty() {
                    let setting = state.settings.entry(state.selected.clone()).or_default(); setting.pinned = !setting.pinned; save = true;
                } }
                Some(Command::Reset) => { if let Some(setting) = state.settings.get_mut(&state.selected) {
                    setting.fired = false; setting.previous_reached = Some(false); setting.snooze_until_ms = 0;
                    state.alert_text = "Alert re-armed. The next fresh quote will check the target.".into();
                } }
                Some(Command::Snooze) => { if let Some(setting) = state.settings.get_mut(&state.selected) {
                    setting.snooze_until_ms = Utc::now().timestamp_millis() + 15 * 60 * 1000;
                    setting.fired = false; setting.previous_reached = Some(false);
                    state.alert_text = "Alert snoozed for 15 minutes.".into();
                } }
                None => break,
            },
            event = network_rx.recv() => match event {
                Some(NetworkEvent::Snapshot(result)) => {
                    state.refreshing = false;
                    match result {
                        Ok((accounts, positions, trades)) => {
                            state.snapshot_ok = true;
                            crate::diagnostics::info(format_args!(
                                "Snapshot refreshed: {} accounts, {} stock positions, {} trade records",
                                accounts.len(), positions.len(), trades.len()
                            ));
                            state.accounts = accounts;
                            state.positions = positions.iter().filter_map(Position::parse).collect();
                            state.positions_loaded = true;
                            for row in &positions {
                                if let Some(position) = Position::parse(row) {
                                    if let Some(symbol) = row.get("ticker").or_else(|| row.get("contractDesc"))
                                        .or_else(|| row.get("description")).and_then(Value::as_str) {
                                        state.history_contracts.insert(format!("{}:{}", position.account, position.conid),
                                            (position.account, position.conid, symbol.to_string()));
                                    }
                                }
                            }
                            contracts_changed = true;
                            archive_changed = ingest_trades(&mut state, &trades);
                            rebuild_cost_estimates(&mut state);
                            if archive_changed {
                                crate::diagnostics::info(format_args!(
                                    "Trade archive now contains {} executions",
                                    state.executions.len()
                                ));
                            }
                            let ids: Vec<i64> = observed_sales(&state).iter().map(|s| s.conid).collect::<HashSet<_>>().into_iter().collect();
                            let _ = sub_tx.send(ids);
                            start_close_refresh(&portal, &network_tx, &mut state);
                            start_cost_refresh(&portal, &network_tx, &mut state);
                            state.status = format!("Client Portal connected · stream {} · {}",
                                if state.ws_connected { "live" } else { "reconnecting" }, Local::now().format("%H:%M:%S"));
                        }
                        Err(error) => {
                            state.snapshot_ok = false;
                            crate::diagnostics::error(format_args!("Snapshot refresh failed: {error:#}"));
                            state.status = format!("Client Portal unavailable: {error}");
                        }
                    }
                }
                Some(NetworkEvent::History(key, result)) => {
                    state.chart_pending.remove(&key);
                    match &result {
                        Ok(bars) => crate::diagnostics::debug(format_args!("Loaded {} OHLC bars", bars.len())),
                        Err(error) => crate::diagnostics::warn(format_args!("Candlesticks failed: {error:#}")),
                    }
                    match result {
                        Ok(bars) => { state.charts.insert(key, chart_from_candles(&bars)); }
                        Err(error) => {
                            let chart = state.charts.entry(key).or_default();
                            chart.status = format!("IBKR candles unavailable: {error}");
                        }
                    }
                }
                Some(NetworkEvent::Close(conid, cycle, result)) => {
                    state.close_pending.remove(&conid);
                    match result {
                        Ok(Some((date, price))) => {
                            state.historical_closes.insert(conid, HistoricalClose { cycle, date, price });
                            crate::diagnostics::debug(format_args!("Loaded regular close for {conid} from {date}"));
                        }
                        Ok(None) => crate::diagnostics::debug(format_args!("No completed regular close in Client Portal history for {conid}")),
                        Err(error) => crate::diagnostics::warn(format_args!("Regular close lookup failed for {conid}: {error:#}")),
                    }
                }
                Some(NetworkEvent::CostHistory(keys, result)) => {
                    for key in &keys { state.cost_pending.remove(key); }
                    match result {
                        Ok(history) => {
                            crate::diagnostics::debug(format_args!(
                                "Loaded Client Portal history for {} stocks ({} transactions)",
                                history.len(), history.values().map(Vec::len).sum::<usize>()
                            ));
                            state.cost_history.extend(history);
                            rebuild_cost_estimates(&mut state);
                            cost_history_changed = true;
                            let ids = observed_sales(&state).iter().map(|s| s.conid)
                                .collect::<HashSet<_>>().into_iter().collect();
                            let _ = sub_tx.send(ids);
                            start_close_refresh(&portal, &network_tx, &mut state);
                        }
                        Err(error) => crate::diagnostics::warn(format_args!(
                            "Client Portal transaction history unavailable for {} stocks: {error:#}", keys.len()
                        )),
                    }
                }
                None => break,
            },
            event = stream_rx.recv() => match event {
                Some(value) => {
                    if let Some(status) = value.get("_ws").and_then(Value::as_str) {
                        state.ws_connected = status == "connected";
                        if state.snapshot_ok {
                            state.status = format!("Client Portal connected · stream {}",
                                if state.ws_connected { "live" } else { "reconnecting" });
                        }
                    } else {
                        let topic = value.get("topic").and_then(Value::as_str).unwrap_or("");
                        if topic.starts_with("str") {
                            let rows = value.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
                            if ingest_trades(&mut state, &rows) {
                                rebuild_cost_estimates(&mut state);
                                crate::diagnostics::info(format_args!("Trade stream updated the archive ({} executions)", state.executions.len()));
                                archive_changed = true;
                                start_refresh(&portal, &network_tx, &mut state);
                                start_cost_refresh(&portal, &network_tx, &mut state);
                            }
                        } else if topic.starts_with("smd") {
                            if let Some(conid) = value.get("conid").and_then(crate::model::integer) {
                                if market::is_overnight_message(&value) {
                                    let previous = state.overnight_quotes.get(&conid);
                                    if let Some((_, quote)) = Quote::parse(&value, previous) {
                                        state.overnight_quotes.insert(conid, quote);
                                        observed_alerts(&mut state, conid);
                                    }
                                } else {
                                    let cycle = market::close_cycle_date(Utc::now());
                                    let close = state.closes.entry(conid).or_insert_with(|| (cycle, ClosePrices::default()));
                                    if close.0 != cycle {
                                        *close = (cycle, ClosePrices::default());
                                    }
                                    close.1.update(&value);
                                    let previous = state.quotes.get(&conid);
                                    if let Some((_, quote)) = Quote::parse(&value, previous) {
                                        state.quotes.insert(conid, quote);
                                        observed_alerts(&mut state, conid);
                                    }
                                }
                            }
                        }
                    }
                }
                None => break,
            },
            _ = timer.tick() => {
                start_refresh(&portal, &network_tx, &mut state);
                start_close_refresh(&portal, &network_tx, &mut state);
                start_chart_refresh(&portal, &network_tx, &mut state);
            },
        }
        if contracts_changed {
            if let Err(error) = write_json(registry_path.clone(), &state.history_contracts).await {
                crate::diagnostics::warn(format_args!(
                    "Could not save historical contract registry: {error}"
                ));
            }
        }
        if save {
            if let Err(error) = save_settings(state.settings.clone()).await {
                crate::diagnostics::error(format_args!("Could not save local settings: {error:#}"));
                state.alert_text = format!("Could not save local settings: {error}");
            }
        }
        if archive_changed {
            if let Err(error) = save_archive(&state.executions).await {
                crate::diagnostics::error(format_args!("Could not save trade archive: {error:#}"));
                state.status =
                    format!("Client Portal connected · trade archive could not be saved: {error}");
            }
        }
        if cost_history_changed {
            if let Err(error) = save_cost_history(&state.cost_history).await {
                crate::diagnostics::error(format_args!(
                    "Could not save purchase history: {error:#}"
                ));
            }
        }
        let next = view(&state);
        if ui
            .upgrade_in_event_loop(move |ui| crate::apply_view(&ui, next))
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn real_gateway_batch_shape_recovers_all_nineteen_stocks() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/client_portal_multi_stock_history.json"
        ))
        .unwrap();
        let contracts = serde_json::from_value(fixture["contracts"].clone()).unwrap();
        let histories =
            parse_transaction_batch(fixture["transactions"].as_array().unwrap(), &contracts)
                .unwrap();
        assert_eq!(histories.len(), 19);
        assert_eq!(histories.values().map(Vec::len).sum::<usize>(), 63);
        let mut state = State::default();
        state.accounts.push("U1".into());
        state.positions_loaded = true;
        state.cost_history = histories;
        rebuild_cost_estimates(&mut state);
        let result = view_at(&state, Utc.with_ymd_and_hms(2026, 10, 1, 16, 0, 0).unwrap());
        let mut symbols: Vec<_> = result.rows.iter().map(|r| r.symbol.as_str()).collect();
        symbols.sort();
        assert_eq!(symbols, "AMD APH CRDO CRM CRWD CRWV DDOG DELL HIMS INOD LULU MRK NBIS NFLX RGTI SAIC SKHY TSLA WDAY".split_whitespace().collect::<Vec<_>>());
        assert!(result
            .rows
            .iter()
            .all(|r| r.best_buy == "100.00" && r.average_buy == "100.00"));
        assert_eq!(
            state
                .cost_history
                .values()
                .flatten()
                .map(|fill| fill.at.date_naive())
                .min(),
            NaiveDate::from_ymd_opt(2026, 8, 17)
        );
    }

    #[tokio::test]
    #[ignore = "requires an authenticated local Client Portal Gateway"]
    async fn live_gateway_positions_costs_and_stream() {
        crate::diagnostics::init();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let portal = Portal::from_env().unwrap();
        let mut state = State::default();
        state.accounts = portal.accounts().await.unwrap();
        let positions = portal.positions(&state.accounts).await.unwrap();
        state.positions = positions
            .iter()
            .map(|v| Position::parse(v).unwrap())
            .collect();
        state.positions_loaded = true;
        state.executions =
            serde_json::from_slice(&tokio::fs::read(archive_path()).await.unwrap()).unwrap();
        if let Ok(bytes) = tokio::fs::read(cost_history_path()).await {
            state.cost_history = serde_json::from_slice(&bytes).unwrap();
        }
        ingest_trades(&mut state, &portal.trades().await.unwrap());
        rebuild_cost_estimates(&mut state);
        let sales = observed_sales(&state);
        let missing: Vec<_> = sales
            .iter()
            .filter(|s| s.held.abs() <= 1e-7 && s.average_cost.is_none())
            .map(|s| s.symbol.as_str())
            .collect();
        println!(
            "Positions parsed: {}; held exits: {}; visible exits missing cost: {:?}",
            state.positions.len(),
            sales.iter().filter(|s| s.held.abs() > 1e-7).count(),
            missing
        );
        assert_eq!(
            view(&state).rows.len(),
            sales.iter().filter(|s| s.held.abs() <= 1e-7).count()
        );
        let conid = sales.iter().find(|s| s.symbol == "AAPL").unwrap().conid;
        let (_sub_tx, sub_rx) = watch::channel(vec![conid]);
        let (tx, mut rx) = mpsc::channel(256);
        let task = tokio::spawn(portal.stream(sub_rx, tx));
        let quote = tokio::time::timeout(Duration::from_secs(25), async {
            while let Some(value) = rx.recv().await {
                if market::is_overnight_message(&value) {
                    if let Some((_, quote)) = Quote::parse(&value, None) {
                        return quote;
                    }
                }
            }
            panic!("stream ended without a quote");
        })
        .await;
        task.abort();
        let quote = quote.expect("no overnight quote received");
        assert!(market::live_quote(Utc::now(), None, Some(&quote)).is_some());
        println!(
            "Authenticated overnight quote received and selected: {}",
            quote.price
        );
    }

    #[test]
    fn current_holdings_are_always_excluded() {
        let mut state = State::default();
        state.accounts.push("U1".into());
        state.positions_loaded = true;
        state.positions.push(Position {
            account: "U1".into(),
            conid: 2,
            quantity: 3.0,
        });
        for conid in [1, 2] {
            let fill = Execution {
                id: format!("sell-{conid}"),
                account: "U1".into(),
                conid,
                symbol: format!("S{conid}"),
                side: crate::model::Side::Sell,
                size: 1.0,
                price: 10.0,
                at: Utc::now(),
            };
            state.executions.insert(fill.id.clone(), fill);
        }
        assert_eq!(view(&state).rows.len(), 1);
        assert_eq!(view(&state).rows[0].symbol, "S1");
        state.period = "1 Week".into();
        assert_eq!(view(&state).rows.len(), 1);
        state.positions.clear();
        assert_eq!(view(&state).rows.len(), 2);
    }

    #[test]
    fn older_client_portal_buy_fills_missing_cost_without_adding_old_exit_row() {
        let mut state = State::default();
        state.accounts.push("U1".into());
        state.positions_loaded = true;
        let sale = Execution {
            id: "sale".into(),
            account: "U1".into(),
            conid: 1,
            symbol: "ORCL".into(),
            side: crate::model::Side::Sell,
            size: 2.0,
            price: 133.87,
            at: Utc.with_ymd_and_hms(2026, 9, 24, 14, 0, 0).unwrap(),
        };
        state.executions.insert(sale.id.clone(), sale);
        let older_buy = Execution::parse_pa_transaction(
            &serde_json::json!({
                "acctid": "U1", "conid": 1,
                "date": "Thu Sep 17 00:00:00 EDT 2026",
                "type": "Buy", "qty": 2.0, "pr": 149.33
            }),
            "U1",
            1,
            "ORCL",
            0,
        )
        .unwrap();
        let duplicated_sale = Execution::parse_pa_transaction(
            &serde_json::json!({
                "acctid": "U1", "conid": 1,
                "date": "Thu Sep 24 00:00:00 EDT 2026",
                "type": "Sell", "qty": -2.0, "pr": 133.87
            }),
            "U1",
            1,
            "ORCL",
            1,
        )
        .unwrap();
        state
            .cost_history
            .insert("U1:1".into(), vec![older_buy, duplicated_sale]);
        rebuild_cost_estimates(&mut state);
        let sales = observed_sales(&state);
        assert_eq!(sales.len(), 1);
        assert_eq!(sales[0].best_buy, Some(149.33));
        assert_eq!(sales[0].average_cost, Some(149.33));
        assert_eq!(view(&state).rows[0].best_buy, "149.33");
    }

    #[test]
    fn lifetime_buys_include_closed_cycles_and_ignore_period_filter() {
        let mut state = State::default();
        state.accounts.push("U1".into());
        state.positions_loaded = true;
        let history: Vec<_> = [
            ("Buy", 2.0, 10.0),
            ("Sell", -2.0, 15.0),
            ("Buy", 6.0, 20.0),
            ("Sell", -6.0, 25.0),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (kind, qty, price))| {
            Execution::parse_pa_transaction(
                &serde_json::json!({
                    "acctid": "U1", "conid": 1,
                    "date": format!("Wed Jan {:02} 00:00:00 EST 2025", index + 1),
                    "type": kind, "qty": qty, "pr": price
                }),
                "U1",
                1,
                "TEST",
                index,
            )
            .unwrap()
        })
        .collect();
        state.cost_history.insert("U1:1".into(), history);
        rebuild_cost_estimates(&mut state);
        let sales = observed_sales(&state);
        assert_eq!(sales.len(), 1); // An old exit with no seven-day execution.
        assert_eq!(sales[0].best_buy, Some(10.0));
        assert_eq!(sales[0].average_cost, Some(17.5));
        state.period = "1 Week".into();
        assert!(view(&state).rows.is_empty());
        assert_eq!(observed_sales(&state)[0].average_cost, Some(17.5));
        state.period = "Inception".into();
        assert_eq!(view(&state).rows.len(), 1);
    }

    #[test]
    fn period_selection_changes_visible_exit_rows() {
        let mut state = State::default();
        state.accounts.push("U1".into());
        state.positions_loaded = true;
        let now = Utc.with_ymd_and_hms(2026, 10, 1, 16, 0, 0).unwrap();
        for (index, (year, month, day)) in [
            (2026, 9, 30),
            (2026, 9, 10),
            (2026, 2, 1),
            (2025, 12, 1),
            (2024, 1, 1),
        ]
        .into_iter()
        .enumerate()
        {
            let fill = Execution {
                id: index.to_string(),
                account: "U1".into(),
                conid: index as i64,
                symbol: format!("S{index}"),
                side: crate::model::Side::Sell,
                size: 1.0,
                price: 10.0,
                at: Utc.with_ymd_and_hms(year, month, day, 16, 0, 0).unwrap(),
            };
            state.executions.insert(fill.id.clone(), fill);
        }
        for (period, expected) in [
            ("1 Week", 1),
            ("1 Month", 2),
            ("1 Year", 4),
            ("Year to Date", 3),
            ("Inception", 5),
            ("1 Week", 1),
        ] {
            state.period = period.into();
            let result = view_at(&state, now);
            assert_eq!(result.rows.len(), expected, "{period}");
            assert!(result.count.starts_with(period));
        }
    }

    #[test]
    fn exit_periods_use_calendar_boundaries() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        assert_eq!(
            period_start("1 Week", day),
            NaiveDate::from_ymd_opt(2024, 3, 25)
        );
        assert_eq!(
            period_start("1 Month", day),
            NaiveDate::from_ymd_opt(2024, 2, 29)
        );
        assert_eq!(
            period_start("1 Year", day),
            NaiveDate::from_ymd_opt(2023, 3, 31)
        );
        assert_eq!(
            period_start("Year to Date", day),
            NaiveDate::from_ymd_opt(2024, 1, 1)
        );
        assert_eq!(period_start("Inception", day), None);
    }

    #[test]
    fn candlestick_chart_uses_latest_session_ohlc() {
        let at = |day, hour, minute| {
            Utc.with_ymd_and_hms(2026, 9, day, hour, minute, 0)
                .unwrap()
                .timestamp_millis()
        };
        let candles = vec![
            Candle {
                at_ms: at(29, 13, 30),
                open: 80.0,
                high: 90.0,
                low: 79.0,
                close: 89.0,
            },
            Candle {
                at_ms: at(30, 13, 30),
                open: 100.0,
                high: 103.0,
                low: 99.0,
                close: 102.0,
            },
            Candle {
                at_ms: at(30, 13, 45),
                open: 102.0,
                high: 104.0,
                low: 100.0,
                close: 101.0,
            },
        ];
        let chart = chart_from_candles(&candles);
        assert_eq!(chart.bars.len(), 2);
        assert!(chart.bars[0].up);
        assert!(!chart.bars[1].up);
        assert!(chart
            .bars
            .iter()
            .all(|bar| bar.wick_top < bar.body_top && bar.wick_height > 0.0));
        assert_eq!(chart.high, "104.00");
        assert_eq!(chart.low, "99.00");
        assert_eq!(chart.first_time, "09:30");
        assert_eq!(chart.last_time, "09:45");
        assert!(chart.caption.contains("Sep 30"));
    }

    #[test]
    fn columns_sort_numeric_values_and_keep_missing_prices_last() {
        let now = Utc.with_ymd_and_hms(2026, 9, 30, 21, 0, 0).unwrap();
        let sale = |conid, symbol: &str, price| Sale {
            key: format!("U1:{conid}"),
            account: "U1".into(),
            conid,
            symbol: symbol.into(),
            at: now,
            price,
            held: 0.0,
            average_cost: None,
            best_buy: None,
        };
        let mut rows = vec![
            sale(1, "LOW", 9.0),
            sale(2, "HIGH", 100.0),
            sale(3, "NONE", 50.0),
        ];
        let mut state = State::default();
        change_sort(&mut state, "exit");
        sort_sales(&mut rows, &state, now);
        assert_eq!(
            rows.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(),
            ["HIGH", "NONE", "LOW"]
        );
        change_sort(&mut state, "exit");
        sort_sales(&mut rows, &state, now);
        assert_eq!(
            rows.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(),
            ["LOW", "NONE", "HIGH"]
        );

        let cycle = market::close_cycle_date(now);
        state.historical_closes.insert(
            1,
            HistoricalClose {
                cycle,
                date: cycle,
                price: 8.0,
            },
        );
        state.historical_closes.insert(
            2,
            HistoricalClose {
                cycle,
                date: cycle,
                price: 110.0,
            },
        );
        change_sort(&mut state, "market");
        sort_sales(&mut rows, &state, now);
        assert_eq!(
            rows.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(),
            ["HIGH", "LOW", "NONE"]
        );
        change_sort(&mut state, "market");
        sort_sales(&mut rows, &state, now);
        assert_eq!(
            rows.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(),
            ["LOW", "HIGH", "NONE"]
        );
        rows[0].best_buy = Some(11.0);
        rows[0].average_cost = Some(8.0);
        rows[1].best_buy = Some(7.0);
        rows[1].average_cost = Some(12.0);
        change_sort(&mut state, "best_buy");
        sort_sales(&mut rows, &state, now);
        assert_eq!(
            rows.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(),
            ["LOW", "HIGH", "NONE"]
        );
        change_sort(&mut state, "average_buy");
        sort_sales(&mut rows, &state, now);
        assert_eq!(
            rows.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(),
            ["HIGH", "LOW", "NONE"]
        );
    }

    #[test]
    fn overnight_price_prefers_live_source_then_completed_close() {
        let now = Utc.with_ymd_and_hms(2026, 10, 1, 0, 30, 0).unwrap();
        let mut state = State::default();
        let smart = Quote {
            price: 102.0,
            updated_ms: now.timestamp_millis(),
            real_time: true,
            label: "IBKR real-time".into(),
            previous_close: false,
        };
        state.quotes.insert(1, smart.clone());
        state.historical_closes.insert(
            1,
            HistoricalClose {
                cycle: market::close_cycle_date(now),
                date: market::close_cycle_date(now),
                price: 100.0,
            },
        );
        assert_eq!(display_price(&state, 1, now).unwrap().price, 100.0);
        state.overnight_quotes.insert(
            1,
            Quote {
                price: 101.0,
                ..smart
            },
        );
        let price = display_price(&state, 1, now).unwrap();
        assert_eq!(price.price, 101.0);
        assert_eq!(price.label, "IBKR live · overnight");
        assert_eq!(
            display_price(&state, 1, now + chrono::Duration::minutes(2))
                .unwrap()
                .price,
            101.0
        );
        assert_eq!(
            display_price(&state, 1, now + chrono::Duration::hours(8))
                .unwrap()
                .price,
            100.0
        );
    }

    #[test]
    fn close_field_is_labeled_and_expires_with_session_cycle() {
        let now = Utc.with_ymd_and_hms(2026, 9, 30, 21, 0, 0).unwrap();
        let mut state = State::default();
        let mut prices = ClosePrices::default();
        prices.update(&serde_json::json!({"7741": "99.25"}));
        state
            .closes
            .insert(1, (market::close_cycle_date(now), prices));
        assert_eq!(
            display_price(&state, 1, now).unwrap().label,
            "IBKR prior close"
        );
        let tomorrow = now + chrono::Duration::days(1);
        assert!(display_price(&state, 1, tomorrow).is_none());
    }

    #[tokio::test]
    async fn atomic_json_write_replaces_existing_file() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("atomic-json-test.json");
        write_json(path.clone(), &vec![1, 2]).await.unwrap();
        write_json(path.clone(), &vec![3]).await.unwrap();
        let saved = tokio::fs::read(&path).await.unwrap();
        assert_eq!(serde_json::from_slice::<Vec<i32>>(&saved).unwrap(), vec![3]);
        tokio::fs::remove_file(path).await.unwrap();
    }
}
