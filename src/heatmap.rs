use crate::{AppWindow, HeatCell, HoldingTile};
use slint::{Model, ModelRc, VecModel};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

// Recursively split near half the total weight along the longest edge.
// Every leaf receives exactly its share of the available rectangle.
fn partition(weights: &[f64], rect: Rect) -> Vec<Rect> {
    if weights.is_empty() {
        return vec![];
    }
    if weights.len() == 1 {
        return vec![rect];
    }
    let total: f64 = weights.iter().sum();
    let mut sum = weights[0];
    let mut split = 1;
    while split < weights.len() - 1
        && (sum + weights[split] - total / 2.0).abs() < (sum - total / 2.0).abs()
    {
        sum += weights[split];
        split += 1;
    }
    let fraction = (sum / total) as f32;
    let (first, second) = if rect.w >= rect.h {
        let w = rect.w * fraction;
        (
            Rect { w, ..rect },
            Rect {
                x: rect.x + w,
                w: rect.w - w,
                ..rect
            },
        )
    } else {
        let h = rect.h * fraction;
        (
            Rect { h, ..rect },
            Rect {
                y: rect.y + h,
                h: rect.h - h,
                ..rect
            },
        )
    };
    let mut result = partition(&weights[..split], first);
    result.extend(partition(&weights[split..], second));
    result
}

pub fn update(ui: &AppWindow) {
    let market_cap = ui.get_heatmap_scale() == 1;
    let mut groups: BTreeMap<String, Vec<(HoldingTile, f64)>> = BTreeMap::new();
    let mut missing = Vec::new();
    // A stock held in multiple accounts is a single cell. Market cap counts once.
    let mut stocks: BTreeMap<(String, String), HoldingTile> = BTreeMap::new();
    for tile in ui.get_holding_tiles().iter() {
        let key = (tile.sector.to_string(), tile.symbol.to_string());
        if let Some(existing) = stocks.get_mut(&key) {
            existing.portfolio_weight += tile.portfolio_weight;
            existing.value = format!("${:.0}", existing.portfolio_weight).into();
        } else {
            stocks.insert(key, tile);
        }
    }
    for tile in stocks.into_values() {
        let weight = if market_cap {
            tile.market_cap
        } else {
            tile.portfolio_weight
        } as f64;
        if weight.is_finite() && weight > 0.0 {
            groups
                .entry(tile.sector.to_string())
                .or_default()
                .push((tile, weight));
        } else {
            missing.push(tile);
        }
    }
    let missing_count = missing.len();
    let width = ui.get_heatmap_width().max(1.0);
    let height = ui.get_heatmap_height().max(1.0);
    let mut cells = Vec::new();
    let mut add_group = |name: &str, items: &[(HoldingTile, f64)], rect: Rect| {
        let header_height = rect.h.min(22.0);
        cells.push(HeatCell {
            tile: HoldingTile {
                sector: name.into(),
                ..Default::default()
            },
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: header_height,
            header: true,
        });
        let body = Rect {
            x: rect.x,
            y: rect.y + header_height,
            w: rect.w,
            h: (rect.h - header_height).max(0.0),
        };
        let weights: Vec<_> = items.iter().map(|(_, weight)| *weight).collect();
        for ((tile, _), r) in items.iter().zip(partition(&weights, body)) {
            cells.push(HeatCell {
                tile: tile.clone(),
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                header: false,
            });
        }
    };
    let missing_height = if missing_count == 0 {
        0.0
    } else if groups.is_empty() {
        height
    } else {
        (height * 0.18).max(80.0).min(height * 0.4)
    };
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(name, mut items)| {
            items.sort_by(|a, b| {
                b.1.total_cmp(&a.1)
                    .then_with(|| a.0.symbol.cmp(&b.0.symbol))
            });
            let weight: f64 = items.iter().map(|(_, weight)| weight).sum();
            (name, items, weight)
        })
        .collect();
    groups.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let weights: Vec<_> = groups.iter().map(|(_, _, weight)| *weight).collect();
    let available = Rect {
        x: 0.0,
        y: 0.0,
        w: width,
        h: height - missing_height,
    };
    for ((name, items, _), rect) in groups.iter().zip(partition(&weights, available)) {
        add_group(name, items, rect);
    }
    if !missing.is_empty() {
        let mut by_sector: BTreeMap<String, Vec<(HoldingTile, f64)>> = BTreeMap::new();
        for tile in missing {
            by_sector
                .entry(tile.sector.to_string())
                .or_default()
                .push((tile, 1.0));
        }
        let groups: Vec<_> = by_sector.into_iter().collect();
        let weights: Vec<_> = groups.iter().map(|(_, items)| items.len() as f64).collect();
        for ((name, items), rect) in groups.iter().zip(partition(
            &weights,
            Rect {
                x: 0.0,
                y: height - missing_height,
                w: width,
                h: missing_height,
            },
        )) {
            add_group(
                &format!(
                    "{name} · {} unavailable",
                    if market_cap { "market cap" } else { "value" }
                ),
                items,
                rect,
            );
        }
    }
    ui.set_heat_cells(ModelRc::new(VecModel::from(cells)));
    let basis = if market_cap {
        "market capitalization"
    } else {
        "portfolio value (USD)"
    };
    let stale = market_cap
        && ui
            .get_holding_tiles()
            .iter()
            .any(|tile| tile.market_cap_stale);
    let suffix = if stale {
        " - includes stale cached caps while refreshing"
    } else {
        ""
    };
    ui.set_heatmap_note(if ui.get_holding_tiles().row_count() == 0 {
        "Waiting for held stocks".into()
    } else if missing_count > 0 {
        format!(
            "Area by {basis} · {missing_count} missing values shown separately with equal sizes{suffix}"
        )
        .into()
    } else {
        format!("Area by {basis} · color shows IBKR day change{suffix}").into()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weighted_rectangles_cover_space_without_overlap() {
        for (w, h) in [(900.0, 500.0), (300.0, 800.0)] {
            let weights = [100.0, 25.0, 10.0, 1.0];
            let rects = partition(
                &weights,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
            );
            for (i, r) in rects.iter().enumerate() {
                let expected = w * h * (weights[i] / 136.0) as f32;
                assert!((r.w * r.h - expected).abs() < 0.1);
                assert!(
                    r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= w + 0.001 && r.y + r.h <= h + 0.001
                );
                for other in &rects[..i] {
                    let overlap_w = (r.x + r.w).min(other.x + other.w) - r.x.max(other.x);
                    let overlap_h = (r.y + r.h).min(other.y + other.h) - r.y.max(other.y);
                    assert!(overlap_w <= 0.001 || overlap_h <= 0.001);
                }
            }
        }
    }
}
