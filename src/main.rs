#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod diagnostics;
mod market;
mod model;
mod portal;
mod worker;

#[cfg(all(windows, target_env = "msvc"))]
const _: () = {
    extern "C" fn attach_parent_console_early() {
        let _ = diagnostics::attach_parent_console();
    }

    #[used]
    #[link_section = ".CRT$XCU"]
    static ATTACH_PARENT_CONSOLE: extern "C" fn() = attach_parent_console_early;
};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;
use worker::{Command, View};

slint::include_modules!();

fn apply_view(ui: &AppWindow, view: View) {
    let selection_changed = ui.get_selected_key().as_str() != view.selected_key;
    let chart = view.selected_chart;
    let rows: Vec<TradeRow> = view
        .rows
        .into_iter()
        .map(|row| TradeRow {
            key: row.key.into(),
            symbol: row.symbol.into(),
            date: row.date.into(),
            sale: row.sale.into(),
            price: row.price.into(),
            delta: row.move_text.into(),
            delta_tone: row.move_tone,
            target: row.target.into(),
            best_buy: row.best_buy.into(),
            best_buy_highlight: row.best_buy_highlight,
            average_buy: row.average_buy.into(),
            average_buy_highlight: row.average_buy_highlight,
            pinned: row.pinned,
            selected: row.selected,
        })
        .collect();
    let current = ui.get_rows();
    if let Some(model) = current.as_any().downcast_ref::<VecModel<TradeRow>>() {
        let same_order = model.row_count() == rows.len()
            && rows
                .iter()
                .enumerate()
                .all(|(index, row)| model.row_data(index).is_some_and(|old| old.key == row.key));
        if same_order {
            for (index, row) in rows.into_iter().enumerate() {
                if model.row_data(index).is_some_and(|old| old != row) {
                    model.set_row_data(index, row);
                }
            }
        } else {
            model.set_vec(rows);
        }
    } else {
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
    }
    ui.set_connection_status(SharedString::from(view.status));
    ui.set_count_text(SharedString::from(view.count));
    ui.set_selected_symbol(SharedString::from(view.selected_symbol));
    ui.set_selected_detail(SharedString::from(view.selected_detail));
    ui.set_selected_quote(SharedString::from(view.selected_quote));
    let bars: Vec<CandleBar> = chart
        .bars
        .into_iter()
        .map(|bar| CandleBar {
            x: bar.x,
            width: bar.width,
            wick_top: bar.wick_top,
            wick_height: bar.wick_height,
            body_top: bar.body_top,
            body_height: bar.body_height,
            up: bar.up,
        })
        .collect();
    let current_bars = ui.get_chart_bars();
    if let Some(model) = current_bars.as_any().downcast_ref::<VecModel<CandleBar>>() {
        let same = model.row_count() == bars.len()
            && bars
                .iter()
                .enumerate()
                .all(|(index, bar)| model.row_data(index).as_ref() == Some(bar));
        if !same {
            model.set_vec(bars);
        }
    } else {
        ui.set_chart_bars(ModelRc::new(VecModel::from(bars)));
    }
    ui.set_chart_status(SharedString::from(chart.status));
    ui.set_chart_caption(SharedString::from(chart.caption));
    ui.set_chart_high(SharedString::from(chart.high));
    ui.set_chart_low(SharedString::from(chart.low));
    ui.set_chart_first_time(SharedString::from(chart.first_time));
    ui.set_chart_last_time(SharedString::from(chart.last_time));
    ui.set_alert_text(SharedString::from(view.alert_text));
    ui.set_sort_column(SharedString::from(view.sort_column));
    ui.set_sort_ascending(view.sort_ascending);
    if selection_changed {
        ui.set_selected_key(SharedString::from(view.selected_key));
        ui.set_selected_target(SharedString::from(view.selected_target));
        ui.set_selected_mode(SharedString::from(view.selected_mode));
        ui.set_selected_alert(view.selected_alert);
    }
}

fn main() -> anyhow::Result<()> {
    let _ = diagnostics::attach_parent_console();
    diagnostics::init();
    diagnostics::info(format_args!("Starting IBKR Companion"));
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Rustls crypto provider was already initialized"))?;
    let portal = portal::Portal::from_env()?;
    diagnostics::info(format_args!("Client Portal Gateway configured"));
    diagnostics::debug(format_args!(
        "TLS verification: {}; account filter: {} account(s)",
        if portal.verify_ssl { "on" } else { "off" },
        portal.allowed_accounts.len()
    ));
    let mut args = std::env::args_os().skip(1);
    if args
        .next()
        .is_some_and(|arg| arg == "--recover-client-portal-history")
    {
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("Expected captured Client Portal JSON path"))?;
        return tokio::runtime::Runtime::new()?
            .block_on(worker::recover_client_portal_history(portal, path.into()));
    }
    let ui = AppWindow::new()?;
    ui.set_rows(ModelRc::new(VecModel::<TradeRow>::default()));
    ui.set_chart_bars(ModelRc::new(VecModel::<CandleBar>::default()));
    let (tx, rx) = mpsc::unbounded_channel::<Command>();

    let sender = tx.clone();
    ui.on_refresh(move || {
        let _ = sender.send(Command::Refresh);
    });
    let sender = tx.clone();
    ui.on_select(move |key| {
        let _ = sender.send(Command::Select(key.to_string()));
    });
    let sender = tx.clone();
    ui.on_filter_changed(move |text| {
        let _ = sender.send(Command::Filter(text.to_string()));
    });
    let sender = tx.clone();
    ui.on_period_changed(move |text| {
        let _ = sender.send(Command::Period(text.to_string()));
    });
    let sender = tx.clone();
    ui.on_sort_changed(move |column| {
        let _ = sender.send(Command::Sort(column.to_string()));
    });
    let sender = tx.clone();
    ui.on_target_saved(move |mode, value| {
        let _ = sender.send(Command::Target(mode.to_string(), value.to_string()));
    });
    let sender = tx.clone();
    ui.on_alert_changed(move |value| {
        let _ = sender.send(Command::Alert(value));
    });
    let sender = tx.clone();
    ui.on_pin_selected(move || {
        let _ = sender.send(Command::Pin);
    });
    let sender = tx.clone();
    ui.on_reset_alert(move || {
        let _ = sender.send(Command::Reset);
    });
    let sender = tx.clone();
    ui.on_snooze_alert(move || {
        let _ = sender.send(Command::Snooze);
    });

    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Tokio runtime");
        runtime.block_on(worker::run(portal, rx, weak));
    });
    ui.run()?;
    diagnostics::info(format_args!("Window closed"));
    drop(tx);
    Ok(())
}
