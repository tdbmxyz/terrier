//! Leboncoin ventes_immobilières (category 9): searches per location and
//! parses the embedded `__NEXT_DATA__` JSON — `price_cents` direct plus
//! structured attributes (surface, rooms, DPE, land, city/zipcode).
//! Behind DataDome: plain HTTP can get 403 while curl passes, so 403/429
//! falls back to a curl subprocess with the same headers (proven in
//! ferret against the same host).

use terrier_domain::{ExtractedAttrs, ListingDetail, PropertyType, RawListing, Seller, SellerKind};
use url::Url;

use crate::config::LeboncoinConfig;
use crate::politeness::ScrapeClient;
use crate::scrape::{ImmoSource, location_slug};

use tower::{Service, ServiceExt};

pub const SOURCE_ID: &str = "leboncoin-immo";
const SEARCH_URL: &str = "https://www.leboncoin.fr/recherche";
/// The ad HTML page is DataDome-walled; this JSON endpoint isn't, and returns
/// the ad object at the top level (not nested under props.pageProps.ad).
const DETAIL_API: &str = "https://api.leboncoin.fr/finder/classified";

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const ACCEPT_LANGUAGE: &str = "fr-FR,fr;q=0.9";
const ACCEPT: &str = "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8";

pub fn search_url(location: &str, page: u32) -> String {
    let slug: String =
        url::form_urlencoded::byte_serialize(location_slug(location).as_bytes()).collect();
    if page <= 1 {
        format!("{SEARCH_URL}?category=9&locations={slug}")
    } else {
        format!("{SEARCH_URL}?category=9&locations={slug}&page={page}")
    }
}

pub fn curl_args(url: &str) -> Vec<String> {
    vec![
        "-sL".into(),
        "--fail".into(),
        "-m".into(),
        "30".into(),
        "--compressed".into(),
        "-H".into(),
        format!("User-Agent: {USER_AGENT}"),
        "-H".into(),
        format!("Accept-Language: {ACCEPT_LANGUAGE}"),
        "-H".into(),
        format!("Accept: {ACCEPT}"),
        url.into(),
    ]
}

fn attr<'a>(ad: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    ad["attributes"].as_array()?.iter().find_map(|a| {
        if a["key"].as_str() == Some(key) {
            a["value"].as_str()
        } else {
            None
        }
    })
}

/// The human-readable `value_label` of an attribute (e.g. "Bon état"),
/// where the raw `value` is an opaque code.
fn attr_label<'a>(ad: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    ad["attributes"].as_array()?.iter().find_map(|a| {
        if a["key"].as_str() == Some(key) {
            a["value_label"].as_str()
        } else {
            None
        }
    })
}

fn attr_i64(ad: &serde_json::Value, key: &str) -> Option<i64> {
    attr(ad, key).and_then(|s| s.parse().ok())
}

/// Map Leboncoin's English enum codes to the same lowercase French vocabulary
/// the LLM prompt produces, so structured and extracted values render alike.
fn heating_type_fr(code: &str) -> Option<String> {
    Some(
        match code {
            "individual" => "individuel",
            "collective" => "collectif",
            _ => return None,
        }
        .into(),
    )
}

fn heating_energy_fr(code: &str) -> Option<String> {
    Some(
        match code {
            "gas" => "gaz",
            "electric" => "electrique",
            "fuel" | "oil" => "fioul",
            "wood" => "bois",
            "heat_pump" => "pompe à chaleur",
            _ => return None,
        }
        .into(),
    )
}

fn orientation_fr(code: &str) -> Option<String> {
    Some(
        match code {
            "north" => "nord",
            "south" => "sud",
            "east" => "est",
            "west" => "ouest",
            "north_east" => "nord-est",
            "north_west" => "nord-ouest",
            "south_east" => "sud-est",
            "south_west" => "sud-ouest",
            _ => return None,
        }
        .into(),
    )
}

/// `global_condition`'s label → the `travaux` enum ("a-prevoir" | ...).
fn condition_to_travaux(label: &str) -> Option<String> {
    let l = label.to_lowercase();
    if l.contains("rénov") || l.contains("refaire") || l.contains("travaux") {
        Some("a-prevoir".into())
    } else if l.contains("rafraîch") || l.contains("rafraich") {
        Some("rafraichissement".into())
    } else if l.contains("bon état") || l.contains("neuf") {
        Some("aucun".into())
    } else {
        None
    }
}

fn garage_parking(ad: &serde_json::Value) -> Option<bool> {
    if attr_i64(ad, "nb_parkings").unwrap_or(0) > 0 {
        return Some(true);
    }
    let spec = attr_label(ad, "specificities")?.to_lowercase();
    (spec.contains("garage") || spec.contains("parking")).then_some(true)
}

/// The facts Leboncoin exposes structurally in the ad's `attributes` array —
/// more reliable than LLM prose extraction, so these win over it. Only the
/// unambiguous fields are taken; genuinely prose-only facts (fibre, piscine,
/// mitoyenne, jardin, notes) are left to the extractor.
fn ad_attributes(ad: &serde_json::Value) -> ExtractedAttrs {
    ExtractedAttrs {
        annee_construction: attr_i64(ad, "building_year"),
        travaux: attr_label(ad, "global_condition").and_then(condition_to_travaux),
        chauffage_type: attr(ad, "heating_type").and_then(heating_type_fr),
        chauffage_energie: attr(ad, "heating_mode").and_then(heating_energy_fr),
        // annual_charges is in euros/year; ExtractedAttrs stores cents/month
        charges_copro_month_cents: attr_i64(ad, "annual_charges").map(|y| y * 100 / 12),
        taxe_fonciere_year_cents: attr_i64(ad, "property_tax").map(|e| e * 100),
        etage: attr_i64(ad, "floor_number"),
        // elevator: "1" = Oui, "2" = Non
        ascenseur: attr(ad, "elevator").map(|v| v == "1"),
        garage_parking: garage_parking(ad),
        orientation: attr(ad, "orientation").and_then(orientation_fr),
        ..Default::default()
    }
}

fn property_type(ad: &serde_json::Value) -> PropertyType {
    // real_estate_type: 1=maison, 2=appartement, 3=terrain (observed)
    match attr(ad, "real_estate_type") {
        Some("1") => PropertyType::House,
        Some("2") => PropertyType::Apartment,
        Some("3") => PropertyType::Land,
        _ => PropertyType::Other,
    }
}

fn image_urls(ad: &serde_json::Value) -> Vec<String> {
    ad["images"]["urls_large"]
        .as_array()
        .or_else(|| ad["images"]["urls"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn seller(ad: &serde_json::Value) -> Option<Seller> {
    let kind = match ad["owner"]["type"].as_str() {
        Some("pro") => SellerKind::Pro,
        Some("private") => SellerKind::Private,
        _ => return None,
    };
    Some(Seller {
        kind,
        name: ad["owner"]["name"].as_str().map(str::to_string),
        siren: ad["owner"]["siren"]
            .as_str()
            .or_else(|| attr(ad, "siren"))
            .map(str::to_string),
    })
}

/// The parsed `__NEXT_DATA__` JSON of any Leboncoin page. Missing tag =
/// blocked page or new layout → hard error so backoff/alerting kicks in.
fn next_data(html: &str) -> anyhow::Result<serde_json::Value> {
    let start_tag = r#"<script id="__NEXT_DATA__""#;
    let start = html
        .find(start_tag)
        .ok_or_else(|| anyhow::anyhow!("__NEXT_DATA__ not found (blocked page or new layout)"))?;
    let json_start = html[start..]
        .find('>')
        .map(|i| start + i + 1)
        .ok_or_else(|| anyhow::anyhow!("malformed __NEXT_DATA__ tag"))?;
    let json_end = html[json_start..]
        .find("</script>")
        .map(|i| json_start + i)
        .ok_or_else(|| anyhow::anyhow!("unterminated __NEXT_DATA__ tag"))?;
    Ok(serde_json::from_str(&html[json_start..json_end])?)
}

/// Missing `ads` = no results; missing `__NEXT_DATA__` = blocked or
/// restructured page → hard error so backoff/alerting kicks in instead of
/// silently marking everything gone.
pub fn parse_search_page(html: &str) -> anyhow::Result<Vec<RawListing>> {
    let data = next_data(html)?;

    let ads = match data["props"]["pageProps"]["searchData"].get("ads") {
        Some(serde_json::Value::Array(ads)) => ads.as_slice(),
        _ => return Ok(Vec::new()),
    };

    let mut listings = Vec::new();
    for ad in ads {
        if ad["status"].as_str().unwrap_or("active") != "active" {
            continue;
        }
        let (Some(title), Some(url)) = (ad["subject"].as_str(), ad["url"].as_str()) else {
            continue;
        };
        let cents = ad["price_cents"]
            .as_i64()
            .or_else(|| ad["price"][0].as_f64().map(|e| (e * 100.0).round() as i64));
        let Some(price_cents) = cents else {
            continue;
        };
        let location = &ad["location"];
        listings.push(RawListing {
            source_id: SOURCE_ID.into(),
            url: url.to_string(),
            title: title.to_string(),
            price_cents,
            property_type: property_type(ad),
            surface_m2: attr(ad, "square").and_then(|s| s.parse().ok()),
            rooms: attr(ad, "rooms").and_then(|s| s.parse().ok()),
            bedrooms: attr(ad, "bedrooms").and_then(|s| s.parse().ok()),
            land_m2: attr(ad, "land_plot_surface").and_then(|s| s.parse().ok()),
            commune: location["city"].as_str().map(str::to_string),
            postal_code: location["zipcode"].as_str().map(str::to_string),
            lat: location["lat"].as_f64(),
            lng: location["lng"].as_f64(),
            dpe: attr(ad, "energy_rate")
                .filter(|v| {
                    ["a", "b", "c", "d", "e", "f", "g"].contains(&v.to_lowercase().as_str())
                })
                .map(|v| v.to_lowercase()),
            ges: attr(ad, "ges")
                .filter(|v| {
                    ["a", "b", "c", "d", "e", "f", "g"].contains(&v.to_lowercase().as_str())
                })
                .map(|v| v.to_lowercase()),
            sell_type: attr(ad, "immo_sell_type").map(str::to_string),
            description: ad["body"].as_str().map(str::to_string),
            address: None,
            image_urls: image_urls(ad),
            seller: seller(ad),
        });
    }
    Ok(listings)
}

/// Build a `ListingDetail` from a top-level ad object (finder API shape).
fn ad_detail(ad: &serde_json::Value) -> ListingDetail {
    ListingDetail {
        description: ad["body"].as_str().map(str::to_string),
        // The API carries no street; the district is the finest relative
        // address available (e.g. "Bourg l'Év. la Touche").
        address: ad["location"]["district"]
            .as_str()
            .or_else(|| ad["location"]["city_label"].as_str())
            .map(str::to_string),
        image_urls: image_urls(ad),
        seller: seller(ad),
        attributes: ad_attributes(ad),
    }
}

/// Parse the JSON body of `api.leboncoin.fr/finder/classified/<id>`, whose ad
/// object sits at the top level — unlike the DataDome-walled HTML ad page,
/// which nested it under `props.pageProps.ad`. Full body, complete image set,
/// seller, relative address, and structured attributes.
pub fn parse_ad_json(body: &str) -> anyhow::Result<ListingDetail> {
    let ad: serde_json::Value = serde_json::from_str(body)?;
    // A DataDome block or error payload lacks both — treat as a hard error so
    // backoff/alerting kicks in instead of storing an empty detail.
    anyhow::ensure!(
        ad["list_id"].is_number() || ad["body"].is_string(),
        "not a finder ad payload (blocked or unexpected shape)"
    );
    Ok(ad_detail(&ad))
}

/// The trailing numeric path segment of a Leboncoin ad URL is its list_id.
fn list_id_of(url: &str) -> Option<&str> {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn detail_api_url(list_id: &str) -> String {
    format!("{DETAIL_API}/{list_id}")
}

pub struct LeboncoinSource {
    config: LeboncoinConfig,
    client: ScrapeClient,
    /// live search locations merged in at fetch time
    extra: Option<crate::state::SharedLocations>,
}

impl LeboncoinSource {
    pub fn new(
        config: LeboncoinConfig,
        client: ScrapeClient,
        extra: Option<crate::state::SharedLocations>,
    ) -> Self {
        Self {
            config,
            client,
            extra,
        }
    }

    async fn fetch_page(&self, url: &str) -> anyhow::Result<String> {
        let parsed = Url::parse(url)?;
        let mut request = reqwest::Request::new(reqwest::Method::GET, parsed);
        let headers = request.headers_mut();
        headers.insert(reqwest::header::USER_AGENT, USER_AGENT.parse()?);
        headers.insert(reqwest::header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE.parse()?);
        headers.insert(reqwest::header::ACCEPT, ACCEPT.parse()?);

        let mut client = self.client.clone();
        // DataDome blocks in two ways: a 403/429 answer, or a dropped TLS
        // handshake (transport error). Both fall back to curl, which passes.
        let response = match client.ready().await?.call(request).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, url, "leboncoin transport error, retrying via curl");
                return fetch_via_curl(url).await;
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            tracing::debug!(%status, url, "leboncoin fingerprint-blocked, retrying via curl");
            return fetch_via_curl(url).await;
        }
        Ok(response.error_for_status()?.text().await?)
    }
}

async fn fetch_via_curl(url: &str) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("curl")
        .args(curl_args(url))
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning curl: {e}"))?;
    anyhow::ensure!(
        output.status.success(),
        "curl failed with {} on {url}",
        output.status
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[async_trait::async_trait]
impl ImmoSource for LeboncoinSource {
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
        tracing::info!(?locations, "leboncoin-immo fetch");
        let mut all = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for location in &locations {
            for page in 1..=self.config.pages_per_location.max(1) {
                let html = self.fetch_page(&search_url(location, page)).await?;
                let listings = parse_search_page(&html)?;
                let count = listings.len();
                all.extend(listings.into_iter().filter(|l| seen.insert(l.url.clone())));
                // 35 ads per full page — a short page is the last one
                if count < 35 {
                    break;
                }
            }
        }
        Ok(all)
    }

    async fn fetch_detail(&self, url: &str) -> anyhow::Result<Option<ListingDetail>> {
        let Some(list_id) = list_id_of(url) else {
            tracing::warn!(url, "leboncoin: no list_id in ad url, skipping detail");
            return Ok(None);
        };
        let body = self.fetch_page(&detail_api_url(list_id)).await?;
        Ok(Some(parse_ad_json(&body)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terrier_domain::SellerKind;

    #[test]
    fn search_url_builds_category_9_with_location() {
        assert_eq!(
            search_url("Rennes 35000", 1),
            "https://www.leboncoin.fr/recherche?category=9&locations=Rennes_35000"
        );
        assert_eq!(
            search_url("Rennes 35000", 2),
            "https://www.leboncoin.fr/recherche?category=9&locations=Rennes_35000&page=2"
        );
    }

    #[test]
    fn parses_immo_fixture_with_structured_attributes() {
        // fixture built from a REAL live capture (2026-07-22)
        let html = include_str!("../../tests/fixtures/leboncoin_immo_search.html");
        let listings = parse_search_page(html).unwrap();
        assert_eq!(listings.len(), 2, "sold ad skipped");

        let flat = &listings[0];
        assert_eq!(flat.title, "Penthouse 5 pièces 139 m²");
        assert_eq!(flat.price_cents, 128_500_000);
        assert_eq!(flat.property_type, PropertyType::Apartment);
        assert_eq!(flat.surface_m2, Some(139.0));
        assert_eq!(flat.rooms, Some(5));
        assert_eq!(flat.bedrooms, Some(3));
        assert_eq!(flat.commune.as_deref(), Some("Rennes"));
        assert_eq!(flat.postal_code.as_deref(), Some("35000"));
        assert_eq!(flat.dpe.as_deref(), Some("c"));
        assert_eq!(flat.sell_type.as_deref(), Some("old"));
        assert_eq!(flat.image_urls.len(), 2);
        assert!(flat.image_urls[0].ends_with("pent-1.jpg"));
        assert!(
            flat.description
                .as_deref()
                .unwrap()
                .starts_with("Penthouse d'exception")
        );
        let seller = flat.seller.as_ref().unwrap();
        assert_eq!(seller.kind, SellerKind::Pro);
        assert_eq!(seller.name.as_deref(), Some("Agence Horizon"));
        assert_eq!(seller.siren.as_deref(), Some("123456789"));

        let house = &listings[1];
        assert_eq!(house.property_type, PropertyType::House);
        assert_eq!(house.land_m2, Some(600.0));
        assert_eq!(house.dpe.as_deref(), Some("a"));
        assert_eq!(house.seller.as_ref().unwrap().kind, SellerKind::Private);
    }

    #[test]
    fn no_results_is_empty_not_error() {
        let html = include_str!("../../tests/fixtures/leboncoin_immo_empty.html");
        assert_eq!(parse_search_page(html).unwrap(), Vec::new());
    }

    #[test]
    fn blocked_page_is_a_hard_error() {
        assert!(parse_search_page("<html>datadome says no</html>").is_err());
    }

    #[test]
    fn parses_ad_detail_json() {
        // fixture mirrors a REAL api.leboncoin.fr/finder/classified capture
        let json = include_str!("../../tests/fixtures/leboncoin_immo_ad.json");
        let d = parse_ad_json(json).unwrap();
        assert!(
            d.description
                .as_deref()
                .unwrap()
                .contains("Copropriété de 24 lots")
        );
        assert_eq!(d.image_urls.len(), 3);
        assert!(d.image_urls[0].ends_with("ad-1.jpg"));
        assert_eq!(d.address.as_deref(), Some("Bourg l'Év. la Touche"));
        let seller = d.seller.as_ref().unwrap();
        assert_eq!(seller.kind, SellerKind::Pro);
        assert_eq!(seller.siren.as_deref(), Some("833292865"));

        // structured attributes, mapped to the LLM's French vocabulary
        let a = &d.attributes;
        assert_eq!(a.annee_construction, Some(1960));
        assert_eq!(a.travaux.as_deref(), Some("aucun"));
        assert_eq!(a.chauffage_type.as_deref(), Some("individuel"));
        assert_eq!(a.chauffage_energie.as_deref(), Some("gaz"));
        assert_eq!(a.charges_copro_month_cents, Some(1200 * 100 / 12));
        assert_eq!(a.taxe_fonciere_year_cents, Some(60_000));
        assert_eq!(a.etage, Some(2));
        assert_eq!(a.ascenseur, Some(true));
        assert_eq!(a.orientation.as_deref(), Some("sud-est"));
        assert_eq!(a.garage_parking, Some(true), "from specificities label");
        // prose-only facts stay for the extractor
        assert_eq!(a.fibre, None);
        assert_eq!(a.piscine, None);
        assert!(a.notes.is_empty());
    }

    #[test]
    fn blocked_or_malformed_ad_json_is_a_hard_error() {
        assert!(parse_ad_json("<html>datadome says no</html>").is_err());
        // valid JSON but not an ad payload (e.g. an error object)
        assert!(parse_ad_json(r#"{"error": "forbidden"}"#).is_err());
    }

    #[test]
    fn list_id_extracted_from_ad_url() {
        assert_eq!(
            list_id_of("https://www.leboncoin.fr/ad/ventes_immobilieres/3138407746"),
            Some("3138407746")
        );
        assert_eq!(
            list_id_of("https://www.leboncoin.fr/ad/ventes_immobilieres/3138407746/"),
            Some("3138407746")
        );
        assert_eq!(
            list_id_of("https://www.leboncoin.fr/recherche?foo=bar"),
            None
        );
        assert_eq!(
            detail_api_url("3138407746"),
            "https://api.leboncoin.fr/finder/classified/3138407746"
        );
    }

    #[test]
    fn image_urls_falls_back_to_plain_urls() {
        let ad = serde_json::json!({"images": {"urls": ["https://cdn/x.jpg"]}});
        assert_eq!(image_urls(&ad), vec!["https://cdn/x.jpg".to_string()]);
    }
}
