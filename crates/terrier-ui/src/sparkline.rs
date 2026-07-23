//! Inline SVG price sparkline for the expanded deal card. Single series on
//! the card surface: 2px accent line, endpoint + minimum markers, value
//! labels in muted ink (text never wears the series color). One
//! observation renders as a dot, not a line.

use leptos::prelude::*;
use terrier_domain::PricePoint;

const W: f64 = 220.0;
const H: f64 = 48.0;
const PAD: f64 = 5.0;

/// Scale price points into `x,y` pairs for an SVG polyline (y inverted,
/// padded). Flat series render mid-height. Pure — unit-tested.
pub(crate) fn polyline_points(prices: &[i64]) -> String {
    let n = prices.len();
    if n == 0 {
        return String::new();
    }
    let (min, max) = prices
        .iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), &p| (lo.min(p), hi.max(p)));
    let span = (max - min).max(1) as f64;
    let step = if n > 1 {
        (W - 2.0 * PAD) / (n - 1) as f64
    } else {
        0.0
    };
    prices
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            let x = PAD + step * i as f64;
            let y = if max == min {
                H / 2.0
            } else {
                PAD + (H - 2.0 * PAD) * (1.0 - (p - min) as f64 / span)
            };
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[component]
pub fn Sparkline(prices: Vec<PricePoint>, currency: String) -> impl IntoView {
    if prices.is_empty() {
        return view! { <p class="muted">"No price history yet."</p> }.into_any();
    }
    let values: Vec<i64> = prices.iter().map(|p| p.price_cents).collect();
    let points = polyline_points(&values);
    let last = *values.last().expect("non-empty");
    let min = *values.iter().min().expect("non-empty");
    let last_xy = points.split(' ').next_back().unwrap_or("0,0").to_string();
    let (lx, ly) = last_xy.split_once(',').unwrap_or(("0", "0"));
    let (lx, ly) = (lx.to_string(), ly.to_string());
    let title: String = prices
        .iter()
        .map(|p| format!("{}: {:.2} {currency}", p.day, p.price_cents as f64 / 100.0))
        .collect::<Vec<_>>()
        .join("\n");
    let single = values.len() == 1;

    view! {
        <div class="spark">
            <svg viewBox=format!("0 0 {W} {H}") width=W height=H role="img"
                aria-label="price history">
                <title>{title}</title>
                {(!single)
                    .then(|| view! {
                        <polyline points=points.clone() fill="none" stroke="var(--accent)"
                            stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>
                    })}
                <circle cx=lx cy=ly r="3.5" fill="var(--accent)"/>
            </svg>
            <span class="muted">
                {format!(
                    "{} pt{} · min {:.2} · now {:.2} {currency}",
                    values.len(),
                    if single { "" } else { "s" },
                    min as f64 / 100.0,
                    last as f64 / 100.0,
                )}
            </span>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_into_padded_viewbox() {
        let pts = polyline_points(&[100, 200, 150]);
        let coords: Vec<(f64, f64)> = pts
            .split(' ')
            .map(|p| {
                let (x, y) = p.split_once(',').unwrap();
                (x.parse().unwrap(), y.parse().unwrap())
            })
            .collect();
        assert_eq!(coords.len(), 3);
        // max price → top pad, min price → bottom pad
        assert_eq!(coords[1].1, PAD);
        assert_eq!(coords[0].1, H - PAD);
        // x spans the padded width
        assert_eq!(coords[0].0, PAD);
        assert_eq!(coords[2].0, W - PAD);
    }

    #[test]
    fn flat_and_single_series() {
        // flat series sits mid-height instead of dividing by zero
        assert!(polyline_points(&[500, 500]).contains(&format!("{:.1}", H / 2.0)));
        // single point → one coordinate pair
        assert_eq!(polyline_points(&[500]).split(' ').count(), 1);
        assert_eq!(polyline_points(&[]), "");
    }
}
