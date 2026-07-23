//! A persisted real-estate listing: one advert on one source. Listings
//! are never deduplicated across sources (user decision) — the same
//! property on Leboncoin and Ouest France Immo stays two rows, each with
//! its own price history.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PropertyType {
    House,
    Apartment,
    Land,
    #[default]
    Other,
}

impl PropertyType {
    pub fn label(self) -> &'static str {
        match self {
            Self::House => "maison",
            Self::Apartment => "appartement",
            Self::Land => "terrain",
            Self::Other => "autre",
        }
    }
}

/// Lifecycle on the source: never deleted, `gone` when a successful
/// scrape no longer sees it, revived if it reappears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListingStatus {
    #[default]
    Active,
    Gone,
}

/// User verdict, orthogonal to lifecycle. Moderated listings never match
/// searches and never notify (same semantics as ferret).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Moderation {
    #[default]
    None,
    /// Hidden for now — clears if the listing goes gone and is re-acquired.
    Dismissed,
    /// Never show or match again.
    Banned,
}

/// Warning flags. They gate notifications, never visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Flag {
    /// A buy request ("Recherche maison…"), not an offer.
    WantedAd,
}

/// One dated price observation — at most one per day, latest wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricePoint {
    /// ISO date (UTC) of the observation.
    pub day: String,
    pub price_cents: i64,
}

/// Who is selling: agency/notary (pro) or an individual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SellerKind {
    Pro,
    Private,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seller {
    pub kind: SellerKind,
    #[serde(default)]
    pub name: Option<String>,
    /// SIREN of the agency when the source exposes it.
    #[serde(default)]
    pub siren: Option<String>,
}

/// One photo, as the UI should load it: `/images/<id>/<n>.<ext>` once
/// downloaded locally, the source CDN URL until then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListingImage {
    pub position: i64,
    pub url: String,
}

/// Facts extracted from the description by the LLM. Everything optional:
/// the prompt forbids guessing — absent from the text means null.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractedAttrs {
    pub annee_construction: Option<i64>,
    /// "a-prevoir" | "rafraichissement" | "aucun"
    pub travaux: Option<String>,
    pub chauffage_type: Option<String>,
    pub chauffage_energie: Option<String>,
    pub fibre: Option<bool>,
    pub charges_copro_month_cents: Option<i64>,
    pub taxe_fonciere_year_cents: Option<i64>,
    /// Floor of an apartment; 0 = rez-de-chaussée.
    pub etage: Option<i64>,
    pub ascenseur: Option<bool>,
    pub jardin: Option<bool>,
    pub garage_parking: Option<bool>,
    pub piscine: Option<bool>,
    pub orientation: Option<String>,
    pub mitoyenne: Option<bool>,
    /// Notable free-form facts (servitude, locataire en place, viager…).
    pub notes: Vec<String>,
}

impl ExtractedAttrs {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fill this value's gaps from `other`: for every `Option` field that is
    /// `None` here, adopt `other`'s; `notes` is taken from `other` only when
    /// empty here. `self` always wins where it already has a value — used to
    /// let authoritative structured attributes take precedence over the LLM's
    /// prose extraction while still adopting the fields structure didn't cover.
    pub fn fill_gaps_from(&mut self, other: &ExtractedAttrs) {
        macro_rules! fill {
            ($($f:ident),+ $(,)?) => { $(
                if self.$f.is_none() {
                    self.$f = other.$f.clone();
                }
            )+ };
        }
        fill!(
            annee_construction,
            travaux,
            chauffage_type,
            chauffage_energie,
            fibre,
            charges_copro_month_cents,
            taxe_fonciere_year_cents,
            etage,
            ascenseur,
            jardin,
            garage_parking,
            piscine,
            orientation,
            mitoyenne,
        );
        if self.notes.is_empty() {
            self.notes = other.notes.clone();
        }
    }
}

/// What a detail-page fetch yields — everything optional, merged over the
/// stored listing (None never clears a stored value). `attributes` carries
/// facts the source exposes structurally (e.g. Leboncoin's `attributes`
/// array), which are more reliable than LLM prose extraction and win over it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListingDetail {
    pub description: Option<String>,
    pub address: Option<String>,
    pub image_urls: Vec<String>,
    pub seller: Option<Seller>,
    pub attributes: ExtractedAttrs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub id: Uuid,
    pub source_id: String,
    pub canonical_url: String,
    pub title: String,
    pub price_cents: i64,
    pub property_type: PropertyType,
    /// Living surface in m² when the source provides it.
    pub surface_m2: Option<f64>,
    pub rooms: Option<i64>,
    pub bedrooms: Option<i64>,
    /// Plot surface in m² (terrains, maisons).
    pub land_m2: Option<f64>,
    pub commune: Option<String>,
    pub postal_code: Option<String>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    /// Energy rating A–G (lowercase), when stated.
    pub dpe: Option<String>,
    pub ges: Option<String>,
    /// "old" | "new" | "viager" when the source distinguishes.
    pub sell_type: Option<String>,
    /// Full description once enriched; the search page's truncated body
    /// until then.
    #[serde(default)]
    pub description: Option<String>,
    /// Street/quartier when a source gives finer than commune.
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub seller: Option<Seller>,
    #[serde(default)]
    pub attributes: ExtractedAttrs,
    pub flags: Vec<Flag>,
    pub status: ListingStatus,
    pub moderation: Moderation,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

impl Listing {
    /// €/m² in cents, when a plausible surface is known.
    pub fn price_per_m2_cents(&self) -> Option<i64> {
        match self.surface_m2 {
            Some(s) if s >= 1.0 => Some((self.price_cents as f64 / s).round() as i64),
            _ => None,
        }
    }
}

/// API shape of `GET /api/listings`: the listing plus its full price
/// history INLINE — the UI's always-visible sparklines must not need one
/// request per card (an N+1 ferret would have made).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListingWithHistory {
    #[serde(flatten)]
    pub listing: Listing,
    #[serde(default)]
    pub history: Vec<PricePoint>,
    #[serde(default)]
    pub images: Vec<ListingImage>,
}

/// What a source plugin hands the pipeline: already-structured where the
/// source is structured (Leboncoin), minimal for CSS-scraped sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawListing {
    pub source_id: String,
    pub url: String,
    pub title: String,
    pub price_cents: i64,
    #[serde(default)]
    pub property_type: PropertyType,
    #[serde(default)]
    pub surface_m2: Option<f64>,
    #[serde(default)]
    pub rooms: Option<i64>,
    #[serde(default)]
    pub bedrooms: Option<i64>,
    #[serde(default)]
    pub land_m2: Option<f64>,
    #[serde(default)]
    pub commune: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lng: Option<f64>,
    #[serde(default)]
    pub dpe: Option<String>,
    #[serde(default)]
    pub ges: Option<String>,
    #[serde(default)]
    pub sell_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub image_urls: Vec<String>,
    #[serde(default)]
    pub seller: Option<Seller>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_gaps_keeps_own_values_and_adopts_missing() {
        // structured (authoritative): heating + orientation known, no prose facts
        let mut structured = ExtractedAttrs {
            chauffage_energie: Some("gaz".into()),
            orientation: Some("sud".into()),
            etage: Some(2),
            ..Default::default()
        };
        // LLM: disagrees on orientation (loses), adds prose-only fibre/notes
        let llm = ExtractedAttrs {
            chauffage_energie: Some("electrique".into()),
            orientation: Some("nord".into()),
            fibre: Some(true),
            notes: vec!["locataire en place".into()],
            ..Default::default()
        };
        structured.fill_gaps_from(&llm);

        assert_eq!(structured.chauffage_energie.as_deref(), Some("gaz"));
        assert_eq!(structured.orientation.as_deref(), Some("sud"));
        assert_eq!(structured.etage, Some(2));
        assert_eq!(structured.fibre, Some(true), "adopted from llm");
        assert_eq!(structured.notes, vec!["locataire en place".to_string()]);
    }

    #[test]
    fn fill_gaps_does_not_replace_existing_notes() {
        let mut a = ExtractedAttrs {
            notes: vec!["servitude".into()],
            ..Default::default()
        };
        a.fill_gaps_from(&ExtractedAttrs {
            notes: vec!["other".into()],
            ..Default::default()
        });
        assert_eq!(a.notes, vec!["servitude".to_string()]);
    }

    fn sample_listing() -> Listing {
        Listing {
            id: Uuid::nil(),
            source_id: "s".into(),
            canonical_url: "https://x".into(),
            title: "t".into(),
            price_cents: 30_000_000,
            property_type: PropertyType::House,
            surface_m2: Some(100.0),
            rooms: None,
            bedrooms: None,
            land_m2: None,
            commune: None,
            postal_code: None,
            lat: None,
            lng: None,
            dpe: None,
            ges: None,
            sell_type: None,
            description: None,
            address: None,
            seller: None,
            attributes: ExtractedAttrs::default(),
            flags: vec![],
            status: ListingStatus::Active,
            moderation: Moderation::None,
            first_seen: chrono::DateTime::UNIX_EPOCH,
            last_seen: chrono::DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn price_per_m2_needs_a_plausible_surface() {
        let mut l = sample_listing();
        assert_eq!(l.price_per_m2_cents(), Some(300_000)); // 3 000 €/m²
        l.surface_m2 = Some(0.0);
        assert_eq!(l.price_per_m2_cents(), None, "zero surface = no ratio");
        l.surface_m2 = None;
        assert_eq!(l.price_per_m2_cents(), None);
    }

    #[test]
    fn extracted_attrs_defaults_and_is_empty() {
        let attrs: ExtractedAttrs = serde_json::from_str("{}").unwrap();
        assert!(attrs.is_empty());
        let attrs: ExtractedAttrs =
            serde_json::from_str(r#"{"fibre": true, "notes": ["locataire en place"]}"#).unwrap();
        assert!(!attrs.is_empty());
        assert_eq!(attrs.fibre, Some(true));
        assert_eq!(attrs.notes, vec!["locataire en place"]);
    }

    #[test]
    fn old_listing_json_still_deserializes() {
        // a pre-enrichment Listing serialization must load (serde defaults)
        let mut v = serde_json::to_value(sample_listing()).unwrap();
        let obj = v.as_object_mut().unwrap();
        for key in ["description", "address", "seller", "attributes"] {
            obj.remove(key);
        }
        let l: Listing = serde_json::from_value(v).unwrap();
        assert!(l.description.is_none() && l.seller.is_none());
        assert!(l.attributes.is_empty());
    }

    #[test]
    fn seller_kind_serializes_kebab_case() {
        assert_eq!(serde_json::to_string(&SellerKind::Pro).unwrap(), "\"pro\"");
        assert_eq!(
            serde_json::to_string(&SellerKind::Private).unwrap(),
            "\"private\""
        );
    }
}
