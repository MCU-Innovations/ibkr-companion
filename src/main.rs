#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod backend_settings;
mod diagnostics;
mod fundamentals;
mod heatmap;
mod market;
mod model;
mod options;
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

#[cfg(test)]
mod navigation_tests {
    use super::*;
    struct TestPlatform(std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>);
    impl slint::platform::Platform for TestPlatform {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    #[test]
    fn settings_back_and_escape_preserve_drawer_and_workspace() {
        let window = slint::platform::software_renderer::MinimalSoftwareWindow::new(
            slint::platform::software_renderer::RepaintBufferType::NewBuffer,
        );
        slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.show().unwrap();
        ui.window().set_size(slint::PhysicalSize::new(1240, 790));
        ui.set_current_page_index(1);
        ui.set_drawer_open(true);
        ui.invoke_open_settings();
        assert!(ui.get_settings_open());
        ui.invoke_dismiss_top_page();
        assert!(!ui.get_settings_open());
        assert!(ui.get_drawer_open());
        assert_eq!(ui.get_current_page_index(), 1);
        ui.invoke_open_settings();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1240 * 790];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1240);
        });
        if let Ok(path) = std::env::var("IBKR_SETTINGS_PREVIEW") {
            let mut data = b"P6\n1240 790\n255\n".to_vec();
            for pixel in &pixels {
                data.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(path, data).unwrap();
        }
        // Focus the API-key input before Escape; capture must run before input handling.
        let position = slint::LogicalPosition::new(850.0, 575.0);
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
        assert!(!ui.get_settings_open());
        assert!(ui.get_drawer_open());
        assert_eq!(ui.get_current_page_index(), 1);
        ui.set_drawer_open(false);
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1240);
        });
        let width = ui.get_heatmap_width();
        let height = ui.get_heatmap_height();
        ui.set_heat_cells(ModelRc::new(VecModel::from(vec![HeatCell {
            tile: HoldingTile {
                symbol: "AAPL".into(),
                symbol_length: 4,
                classification: "Technology / Consumer Electronics".into(),
                value: "$1000".into(),
                market_cap_label: "$4.82T cap".into(),
                change: "+1.25%".into(),
                change_tone: 1,
                ..Default::default()
            },
            x: width - 24.0,
            y: height - 20.0,
            w: 24.0,
            h: 20.0,
            header: false,
        }])));
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1240);
        });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(
                    ui.get_heatmap_origin_x() + width - 12.0,
                    ui.get_heatmap_origin_y() + height - 10.0,
                ),
            });
        assert!(ui.get_heatmap_tooltip_visible());
        assert_eq!(ui.get_hovered_stock().symbol, "AAPL");
        assert!(ui.get_heatmap_tooltip_x() >= 0.0 && ui.get_heatmap_tooltip_y() >= 0.0);
        assert!(ui.get_heatmap_tooltip_x() + ui.get_heatmap_tooltip_width() <= width);
        assert!(ui.get_heatmap_tooltip_y() + ui.get_heatmap_tooltip_height() <= height);
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1240);
        });
        if let Ok(path) = std::env::var("IBKR_HEATMAP_PREVIEW") {
            let mut data = b"P6\n1240 790\n255\n".to_vec();
            for pixel in &pixels {
                data.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(path, data).unwrap();
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerExited);
        assert!(!ui.get_heatmap_tooltip_visible());
        ui.set_current_page_index(2);
        let option_fixtures = [
            (
                "BE OCT2026 295 C [BE  261002C00295000 100]",
                -1.0,
                122.31,
                56.97,
                0.56,
                "0.091",
            ),
            (
                "CELH OCT2026 29 C [CELH  261009C00029000 100]",
                -1.0,
                31.96,
                3.66,
                0.26,
                "0.228",
            ),
            (
                "NVTS OCT2026 13 C [NVTS  261009C00013000 100]",
                -1.0,
                38.50,
                12.35,
                0.25,
                "0.310",
            ),
            (
                "AAPL NOV2026 250 P [AAPL  261120P00250000 100]",
                2.0,
                400.0,
                -84.0,
                3.58,
                "-0.420",
            ),
            (
                "MSFT DEC2026 500 C [MSFT  261218C00500000 100]",
                1.0,
                1000.0,
                125.0,
                11.25,
                "0.560",
            ),
        ];
        let mut option_views: Vec<_> = option_fixtures.iter().enumerate().map(|(index, (desc, qty, cost, pnl, mark, delta))| {
            options::row(&serde_json::json!({"assetClass":"OPT", "contractDesc":desc, "conid":index+1,
                "_account":"U123", "currency":"USD", "multiplier":100, "position":qty,
                "_underlyingPrice": ([290.5, 31.2, 13.0, 248.0, 501.5][index]), "_underlyingStatus":"frozen",
                "avgCost":cost, "unrealizedPnl":pnl, "realizedPnl":0, "mktValue":qty*mark*100.0}),
                None, Some(&serde_json::json!({"7635":mark, "31":mark, "84":mark-0.03, "86":mark+0.03,
                    "6509":"Z", "7308":delta, "7309":"0.013", "7310":"-0.518", "7311":"0.026", "7633":"85.6%", "87":"3.45K", "7638":"735"})),
                chrono::NaiveDate::from_ymd_opt(2026,10,1).unwrap()).unwrap()
        }).collect();
        options::sort(&mut option_views, "expiry", true);
        ui.set_options_status("5 open positions / 1 expiring within 7 days".into());
        ui.set_options_pnl(options::pnl_summary(&option_views).into());
        options::apply_rows(&ui, option_views.clone());
        ui.set_drawer_open(true);
        ui.invoke_open_settings();
        ui.invoke_dismiss_top_page();
        assert_eq!(ui.get_current_page_index(), 2);
        assert!(ui.get_drawer_open());
        ui.set_drawer_open(false);
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1240);
        });
        if let Ok(path) = std::env::var("IBKR_OPTIONS_PREVIEW") {
            let mut data = b"P6\n1240 790\n255\n".to_vec();
            for pixel in &pixels {
                data.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(path, data).unwrap();
        }
        ui.invoke_select_option(ui.get_option_rows().row_data(0).unwrap());
        assert!(ui.get_option_detail_visible());
        let selected_key = ui.get_selected_option().key;
        option_views[0].pnl_value = "+60.00 USD".into();
        option_views.reverse();
        options::apply_rows(&ui, option_views.clone());
        assert_eq!(ui.get_selected_option().key, selected_key);
        assert_eq!(ui.get_selected_option().pnl_value, "+60.00 USD");
        option_views
            .iter_mut()
            .find(|r| r.key == selected_key.as_str())
            .unwrap()
            .pnl_value = "+56.97 USD".into();
        options::sort(&mut option_views, "expiry", true);
        options::apply_rows(&ui, option_views.clone());
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1240);
        });
        if let Ok(path) = std::env::var("IBKR_OPTIONS_DETAIL_PREVIEW") {
            let mut data = b"P6\n1240 790\n255\n".to_vec();
            for pixel in &pixels {
                data.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(path, data).unwrap();
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
        assert!(!ui.get_option_detail_visible());
        ui.invoke_select_option(ui.get_option_rows().row_data(0).unwrap());
        ui.window().set_size(slint::PhysicalSize::new(1020, 580));
        let mut small_pixels = vec![slint::Rgb8Pixel::default(); 1020 * 580];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut small_pixels, 1020);
        });
        if let Ok(path) = std::env::var("IBKR_OPTIONS_SMALL_PREVIEW") {
            let mut data = b"P6\n1020 580\n255\n".to_vec();
            for pixel in &small_pixels {
                data.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(path, data).unwrap();
        }
        options::apply_rows(&ui, Vec::new());
        assert!(!ui.get_option_detail_visible());
        assert!(ui.get_selected_option().key.is_empty());
        ui.hide().unwrap();
    }
}

fn apply_view(ui: &AppWindow, view: View) {
    ui.set_premium_rows(ModelRc::new(VecModel::from(view.premium_rows)));
    ui.set_premium_total(view.premium_total.into());
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
            open_trend: row.open_trend.into(),
            open_trend_tone: row.open_trend_tone,
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
    ui.set_options_status(view.options_status.into());
    ui.set_options_pnl(view.options_pnl.into());
    ui.set_option_sort_column(view.option_sort_column.into());
    ui.set_option_sort_ascending(view.option_sort_ascending);
    options::apply_rows(ui, view.options);
    ui.set_connection_status(SharedString::from(view.status));
    ui.set_count_text(SharedString::from(view.count));
    ui.set_fundamentals_status(view.fundamentals_status.into());
    ui.set_portfolio_total(SharedString::from(view.portfolio_total));
    ui.set_portfolio_count(SharedString::from(view.portfolio_count));
    ui.set_sector_slices(ModelRc::new(VecModel::from(
        view.sector_slices
            .into_iter()
            .map(|slice| SectorSlice {
                label: slice.label.into(),
                amount: slice.amount.into(),
                start: slice.start,
                fraction: slice.fraction,
                palette: slice.palette,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_holding_tiles(ModelRc::new(VecModel::from(
        view.holding_tiles
            .into_iter()
            .map(|holding| HoldingTile {
                symbol_length: holding.symbol.chars().count() as i32,
                sector: holding.sector.into(),
                portfolio_weight: holding.portfolio_weight,
                market_cap: holding.market_cap,
                market_cap_label: holding.market_cap_label.into(),
                market_cap_stale: holding.market_cap_stale,
                symbol: holding.symbol.into(),
                classification: holding.classification.into(),
                value: holding.value.into(),
                change: holding.change.into(),
                change_tone: holding.change_tone,
            })
            .collect::<Vec<_>>(),
    )));
    heatmap::update(ui);
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
    let first_arg = args.next();
    if first_arg.as_deref() == Some(std::ffi::OsStr::new("--check-fundamentals")) {
        let symbol = args
            .next()
            .unwrap_or_else(|| "AAPL".into())
            .to_string_lossy()
            .to_string();
        let profile = tokio::runtime::Runtime::new()?.block_on(async {
            fundamentals::FundamentalsClient::new()?
                .check_yahoo(&symbol)
                .await
        })?;
        let json = serde_json::to_string_pretty(&profile)?;
        if let Some(path) = args.next() {
            std::fs::write(path, &json)?;
        }
        println!("{json}");
        return Ok(());
    }
    if first_arg.is_some_and(|arg| arg == "--recover-client-portal-history") {
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("Expected captured Client Portal JSON path"))?;
        return tokio::runtime::Runtime::new()?
            .block_on(worker::recover_client_portal_history(portal, path.into()));
    }
    let ui = AppWindow::new()?;
    ui.set_rows(ModelRc::new(VecModel::<TradeRow>::default()));
    ui.set_chart_bars(ModelRc::new(VecModel::<CandleBar>::default()));
    ui.set_sector_slices(ModelRc::new(VecModel::<SectorSlice>::default()));
    ui.set_holding_tiles(ModelRc::new(VecModel::<HoldingTile>::default()));
    let weak_heatmap = ui.as_weak();
    ui.on_layout_heatmap(move || {
        if let Some(ui) = weak_heatmap.upgrade() {
            heatmap::update(&ui);
        }
    });
    let (tx, rx) = mpsc::unbounded_channel::<Command>();
    let sender = tx.clone();
    ui.on_fundamentals_demand(move |active| {
        let _ = sender.send(Command::FundamentalsDemand(active));
    });
    let sender = tx.clone();
    ui.on_backend_saved(move |provider, enabled, key, clear_key| {
        let _ = sender.send(Command::BackendSave {
            provider,
            enabled,
            key: key.to_string(),
            clear_key,
        });
    });
    let sender = tx.clone();
    ui.on_fundamentals_refresh(move || {
        let _ = sender.send(Command::FundamentalsRefresh);
    });

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
    ui.on_option_sort(move |column| {
        let _ = sender.send(Command::OptionSort(column.to_string()));
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

    // The Material title bar is frameless. These Winit calls are the same
    // native frame integration used by ECU Workbench.
    #[cfg(windows)]
    wire_windows_frame_callbacks(&ui);

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

#[cfg(windows)]
fn wire_windows_frame_callbacks(window: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;

    let drag_window = window.as_weak();
    window.on_start_window_drag(move || {
        let handle = drag_window.clone();
        let _ = slint::spawn_local(async move {
            if let Some(window) = handle.upgrade() {
                if let Ok(native) = window.window().winit_window().await {
                    let _ = native.drag_window();
                }
            }
        });
    });

    let minimize_window = window.as_weak();
    window.on_minimize_window(move || {
        let handle = minimize_window.clone();
        let _ = slint::spawn_local(async move {
            if let Some(window) = handle.upgrade() {
                if let Ok(native) = window.window().winit_window().await {
                    native.set_minimized(true);
                }
            }
        });
    });

    let maximize_window = window.as_weak();
    window.on_toggle_maximize_window(move || {
        let handle = maximize_window.clone();
        let _ = slint::spawn_local(async move {
            if let Some(window) = handle.upgrade() {
                if let Ok(native) = window.window().winit_window().await {
                    native.set_maximized(!native.is_maximized());
                }
            }
        });
    });

    let close_window = window.as_weak();
    window.on_close_window(move || {
        if let Some(window) = close_window.upgrade() {
            let _ = window.window().hide();
        }
    });
}
