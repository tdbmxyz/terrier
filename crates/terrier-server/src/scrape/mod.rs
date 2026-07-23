//! Source plugins. Each source turns its pages into `RawListing`s; the
//! pipeline owns everything downstream.

pub mod leboncoin;
pub mod ouestfrance;

use terrier_domain::{ListingDetail, RawListing};

#[async_trait::async_trait]
pub trait ImmoSource: Send + Sync {
    fn id(&self) -> &str;
    /// One full fetch over every configured location/page.
    async fn fetch(&self) -> anyhow::Result<Vec<RawListing>>;

    /// One listing's detail page; `Ok(None)` when the source has no
    /// detail support (the enricher then marks the listing enriched as-is).
    // caller arrives with the enrichment worker task
    #[allow(dead_code)]
    async fn fetch_detail(&self, _url: &str) -> anyhow::Result<Option<ListingDetail>> {
        Ok(None)
    }
}

/// "Rennes 35000", "rennes_35000" or "Saint-Malo 35400" → the
/// `Ville_CP` form Leboncoin's `locations=` parameter wants.
pub fn location_slug(raw: &str) -> String {
    let cleaned = raw.trim().replace('_', " ");
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    let cp = match words.last() {
        Some(w) if w.chars().all(|c| c.is_ascii_digit()) && w.len() == 5 => words.pop(),
        _ => None,
    };
    let city: String = words
        .iter()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    match (city.is_empty(), cp) {
        (false, Some(cp)) => format!("{city}_{cp}"),
        (false, None) => city,
        (true, Some(cp)) => cp.to_string(),
        (true, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_slug_normalizes_user_input() {
        assert_eq!(location_slug("Rennes 35000"), "Rennes_35000");
        assert_eq!(location_slug("rennes 35000"), "Rennes_35000");
        assert_eq!(location_slug("Rennes_35000"), "Rennes_35000");
        assert_eq!(location_slug("Bruz"), "Bruz");
        assert_eq!(location_slug("35170"), "35170");
        assert_eq!(location_slug("saint-malo 35400"), "Saint-malo_35400");
    }
}
