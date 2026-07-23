//! Normalization of raw scraped values: price text → integer cents,
//! listing URL → canonical form, title whitespace cleanup.

use std::sync::LazyLock;

use regex::Regex;
use url::Url;

/// Query params that never identify the product — stripped during
/// canonicalization so retargeted URLs dedupe to one listing.
const TRACKING_PARAMS: &[&str] = &[
    "ref", "referrer", "aff", "affid", "tag", "fbclid", "gclid", "mc_cid", "mc_eid",
];

static PRICE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // number with optional thousands separators and optional decimal part.
    // The plain-digit run branch MUST come first: alternation is
    // leftmost-first, and the separator branch would otherwise truncate
    // "7000.00" to "700" (it matches 3 digits, finds no separator, stops —
    // and no backtracking into the other branch ever happens).
    Regex::new(r"(\d{4,}|\d{1,3}(?:[ \u{a0}\u{202f},.]\d{3})*)(?:[.,](\d{1,2}))?").unwrap()
});

/// Parse a scraped price string into `(cents, currency)`.
///
/// Handles EU ("1 234,56 €") and US ("$1,234.56") styles. The currency is
/// inferred from the symbol/code found in the text; defaults to EUR when a
/// number is present but no currency marker is (self-hosted EU bias, and
/// sources can be assumed single-currency).
pub fn parse_price(text: &str) -> Option<(i64, String)> {
    let currency = if text.contains('€') || text.to_uppercase().contains("EUR") {
        "EUR"
    } else if text.contains('$') || text.to_uppercase().contains("USD") {
        "USD"
    } else if text.contains('£') || text.to_uppercase().contains("GBP") {
        "GBP"
    } else {
        "EUR"
    };
    let caps = PRICE_RE.captures(text)?;
    let whole: String = caps[1].chars().filter(|c| c.is_ascii_digit()).collect();
    let whole: i64 = whole.parse().ok()?;
    let cents = match caps.get(2) {
        Some(frac) => {
            let f: i64 = frac.as_str().parse().ok()?;
            if frac.as_str().len() == 1 { f * 10 } else { f }
        }
        None => 0,
    };
    Some((whole * 100 + cents, currency.to_string()))
}

/// Canonicalize a listing URL: lowercase host, drop the fragment, drop
/// tracking query params (`utm_*` and the known list), keep the rest.
pub fn canonical_url(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && !TRACKING_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept).finish();
    }
    Some(url.to_string())
}

static WANTED_RE: LazyLock<Regex> = LazyLock::new(|| {
    // buy requests, French marketplace style: the intent word leads the title
    Regex::new(r"(?i)^\W*(je\s+)?(recherche|cherche|rech\.?|achat|ach[eè]te)\b").unwrap()
});

/// True when the title is a buy request ("Recherche RTX 5090"), not an
/// offer — matched on the leading words only to avoid false positives.
pub fn is_wanted_ad(title: &str) -> bool {
    WANTED_RE.is_match(title)
}

/// Collapse all whitespace runs (including nbsp) to single spaces and trim.
pub fn clean_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eu_style_price() {
        // French marketplaces: "1 234,56 €" (narrow nbsp or space separators)
        assert_eq!(parse_price("1 234,56 €"), Some((123_456, "EUR".into())));
        assert_eq!(parse_price("49,99€"), Some((4_999, "EUR".into())));
    }

    #[test]
    fn parses_us_style_price() {
        assert_eq!(parse_price("$1,234.56"), Some((123_456, "USD".into())));
        assert_eq!(parse_price("USD 12.00"), Some((1_200, "USD".into())));
    }

    #[test]
    fn parses_bare_integer_price() {
        assert_eq!(parse_price("120 €"), Some((12_000, "EUR".into())));
    }

    #[test]
    fn wanted_ads_are_detected_by_leading_intent_words() {
        assert!(is_wanted_ad("Recherche RTX 5090"));
        assert!(is_wanted_ad("recherche rtx 5090 sous garantie"));
        assert!(is_wanted_ad("Je cherche une RTX 4090"));
        assert!(is_wanted_ad("ACHAT rtx 3090"));
        assert!(is_wanted_ad("Achète carte graphique"));
        // selling listings that merely contain the words elsewhere
        assert!(!is_wanted_ad(
            "RTX 4090 état neuf, recherche nouveau proprio"
        ));
        assert!(!is_wanted_ad("PC gamer rtx 5090"));
    }

    #[test]
    fn four_plus_digit_prices_without_separator_keep_all_digits() {
        // regression: a real 7000€ Leboncoin ad was parsed (and notified!)
        // as 700.00€ — the separator branch matched "700" and stopped
        assert_eq!(parse_price("7000.00 €"), Some((700_000, "EUR".into())));
        assert_eq!(parse_price("1234 €"), Some((123_400, "EUR".into())));
        assert_eq!(parse_price("1299.99 €"), Some((129_999, "EUR".into())));
        assert_eq!(parse_price("12345,50"), Some((1_234_550, "EUR".into())));
    }

    #[test]
    fn separator_styles_still_parse_after_the_fix() {
        assert_eq!(parse_price("7 000,99 €"), Some((700_099, "EUR".into())));
        assert_eq!(parse_price("7.000 €"), Some((700_000, "EUR".into())));
        assert_eq!(parse_price("$12,345.67"), Some((1_234_567, "USD".into())));
        assert_eq!(parse_price("1.23 €"), Some((123, "EUR".into())));
    }

    #[test]
    fn rejects_priceless_text() {
        assert_eq!(parse_price("Contact seller"), None);
        assert_eq!(parse_price(""), None);
    }

    #[test]
    fn canonical_url_strips_tracking_and_fragment() {
        assert_eq!(
            canonical_url("https://ex.com/item/42?utm_source=x&ref=abc&id=7#frag").unwrap(),
            "https://ex.com/item/42?id=7"
        );
    }

    #[test]
    fn canonical_url_lowercases_host_keeps_path_case() {
        assert_eq!(
            canonical_url("https://Ex.COM/Item/42").unwrap(),
            "https://ex.com/Item/42"
        );
    }

    #[test]
    fn canonical_url_rejects_garbage() {
        assert!(canonical_url("not a url").is_none());
    }

    #[test]
    fn clean_title_collapses_whitespace() {
        assert_eq!(clean_title("  RTX\u{a0}3080\n 10GB  "), "RTX 3080 10GB");
    }
}
