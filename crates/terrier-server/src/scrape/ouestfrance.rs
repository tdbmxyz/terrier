//! Ouest France Immo — EXPERIMENTAL. The site is bot-walled (plain HTTP
//! and curl-impersonate both get decoy pages; listings load through an
//! internal SPA API), so this plugin only works through `fetch_command`:
//! an external argv (stealth browser wrapper, `{url}` substituted) that
//! returns the RENDERED page HTML.
//!
//! Parsing reads schema.org JSON-LD blocks when present and falls back to
//! a hard error naming the problem — never a silent empty page (which
//! would mark everything gone).

use terrier_domain::{PropertyType, RawListing};

use crate::config::OuestFranceConfig;
use crate::scrape::ImmoSource;

pub const SOURCE_ID: &str = "ouestfrance-immo";

/// "Rennes 35000" → the `ville-dep-cp` slug OFI uses ("rennes-35-35000").
pub fn location_slug(raw: &str) -> String {
    let cleaned = raw.trim().to_lowercase().replace('_', " ");
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    let cp = match words.last() {
        Some(w) if w.chars().all(|c| c.is_ascii_digit()) && w.len() == 5 => words.pop(),
        _ => None,
    };
    let city = words.join("-");
    match cp {
        Some(cp) => format!("{city}-{}-{cp}", &cp[..2]),
        None => city,
    }
}

pub fn search_url(location: &str) -> String {
    format!(
        "https://www.ouestfrance-immo.com/acheter/{}/",
        location_slug(location)
    )
}

/// Extract listings from schema.org JSON-LD blocks in a rendered page.
pub fn parse_page(html: &str) -> anyhow::Result<Vec<RawListing>> {
    let mut listings = Vec::new();
    let mut rest = html;
    let mut found_ld = false;
    while let Some(start) = rest.find(r#"application/ld+json"#) {
        let after = &rest[start..];
        let Some(json_start) = after.find('>') else {
            break;
        };
        let Some(json_end) = after.find("</script>") else {
            break;
        };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&after[json_start + 1..json_end]) {
            found_ld = true;
            collect_offers(&v, &mut listings);
        }
        rest = &after[json_end..];
    }
    if !found_ld {
        anyhow::bail!(
            "no JSON-LD found — blocked page or layout change; \
             capture a rendered fixture and adapt the parser"
        );
    }
    Ok(listings)
}

fn collect_offers(v: &serde_json::Value, out: &mut Vec<RawListing>) {
    match v {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_offers(item, out);
            }
        }
        serde_json::Value::Object(_) => {
            if let Some(listing) = offer_to_listing(v) {
                out.push(listing);
            }
            for key in ["itemListElement", "@graph", "item"] {
                if let Some(inner) = v.get(key) {
                    collect_offers(inner, out);
                }
            }
        }
        _ => {}
    }
}

fn offer_to_listing(v: &serde_json::Value) -> Option<RawListing> {
    let ty = v["@type"].as_str()?;
    if !matches!(
        ty,
        "Product" | "Offer" | "House" | "Apartment" | "SingleFamilyResidence"
    ) {
        return None;
    }
    let name = v["name"].as_str()?;
    let url = v["url"].as_str().or_else(|| v["offers"]["url"].as_str())?;
    let price = v["offers"]["price"]
        .as_f64()
        .or_else(|| v["offers"]["price"].as_str().and_then(|s| s.parse().ok()))
        .or_else(|| v["price"].as_f64())?;
    let property_type = match ty {
        "House" | "SingleFamilyResidence" => PropertyType::House,
        "Apartment" => PropertyType::Apartment,
        _ if name.to_lowercase().contains("maison") => PropertyType::House,
        _ if name.to_lowercase().contains("appartement") => PropertyType::Apartment,
        _ if name.to_lowercase().contains("terrain") => PropertyType::Land,
        _ => PropertyType::Other,
    };
    Some(RawListing {
        source_id: SOURCE_ID.into(),
        url: url.to_string(),
        title: name.to_string(),
        price_cents: (price * 100.0).round() as i64,
        property_type,
        surface_m2: v["floorSize"]["value"].as_f64(),
        rooms: v["numberOfRooms"].as_i64(),
        bedrooms: None,
        land_m2: None,
        commune: v["address"]["addressLocality"].as_str().map(str::to_string),
        postal_code: v["address"]["postalCode"].as_str().map(str::to_string),
        lat: None,
        lng: None,
        dpe: None,
        ges: None,
        sell_type: None,
        description: None,
        address: None,
        image_urls: vec![],
        seller: None,
    })
}

pub struct OuestFranceSource {
    config: OuestFranceConfig,
    extra: Option<crate::state::SharedLocations>,
}

impl OuestFranceSource {
    pub fn new(config: OuestFranceConfig, extra: Option<crate::state::SharedLocations>) -> Self {
        Self { config, extra }
    }

    async fn fetch_page(&self, url: &str) -> anyhow::Result<String> {
        anyhow::ensure!(
            !self.config.fetch_command.is_empty(),
            "ouestfrance-immo needs fetch_command (stealth browser wrapper) — \
             the site blocks plain HTTP clients"
        );
        let argv: Vec<String> = self
            .config
            .fetch_command
            .iter()
            .map(|a| a.replace("{url}", url))
            .collect();
        let output = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("spawning fetch_command {:?}: {e}", argv[0]))?;
        anyhow::ensure!(
            output.status.success(),
            "fetch_command failed with {} on {url}",
            output.status
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[async_trait::async_trait]
impl ImmoSource for OuestFranceSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    async fn fetch(&self) -> anyhow::Result<Vec<RawListing>> {
        let mut locations = self.config.locations.clone();
        if let Some(extra) = &self.extra {
            for l in extra.read().await.iter() {
                if !locations.contains(l) {
                    locations.push(l.clone());
                }
            }
        }
        let mut all = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for location in &locations {
            tokio::time::sleep(std::time::Duration::from_millis(self.config.delay_ms)).await;
            let html = self.fetch_page(&search_url(location)).await?;
            all.extend(
                parse_page(&html)?
                    .into_iter()
                    .filter(|l| seen.insert(l.url.clone())),
            );
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_builds_ville_dep_cp() {
        assert_eq!(location_slug("Rennes 35000"), "rennes-35-35000");
        assert_eq!(location_slug("Saint-Malo 35400"), "saint-malo-35-35400");
        assert_eq!(location_slug("bruz"), "bruz");
    }

    #[test]
    fn parses_json_ld_offers() {
        let html = r#"<html><script type="application/ld+json">
        {"@type":"ItemList","itemListElement":[
          {"@type":"ListItem","item":{"@type":"House","name":"Maison 5 pièces Bruz",
           "url":"https://www.ouestfrance-immo.com/annonce/1",
           "offers":{"price":"320000"},
           "floorSize":{"value":110},"numberOfRooms":5,
           "address":{"addressLocality":"Bruz","postalCode":"35170"}}}
        ]}</script></html>"#;
        let listings = parse_page(html).unwrap();
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].price_cents, 32_000_000);
        assert_eq!(listings[0].property_type, PropertyType::House);
        assert_eq!(listings[0].surface_m2, Some(110.0));
        assert_eq!(listings[0].commune.as_deref(), Some("Bruz"));
    }

    #[test]
    fn page_without_json_ld_is_a_hard_error() {
        assert!(parse_page("<html>bot wall</html>").is_err());
    }
}
