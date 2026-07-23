//! Searches: the immo equivalent of ferret's watches — structured
//! commune + criteria, no free-text interpretation. Active searches'
//! locations drive the scrape rotation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::listing::{Listing, ListingStatus, Moderation, PropertyType};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Search {
    pub id: Uuid,
    pub name: String,
    /// Raw location strings as typed ("Rennes 35000", "35235", "Bruz").
    /// Each source maps them to its own URL format.
    pub locations: Vec<String>,
    pub max_price_cents: Option<i64>,
    pub min_surface_m2: Option<f64>,
    pub min_rooms: Option<i64>,
    /// Empty = every type matches.
    pub property_types: Vec<PropertyType>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub name: String,
    #[serde(default)]
    pub locations: Vec<String>,
    #[serde(default)]
    pub max_price_cents: Option<i64>,
    #[serde(default)]
    pub min_surface_m2: Option<f64>,
    #[serde(default)]
    pub min_rooms: Option<i64>,
    #[serde(default)]
    pub property_types: Vec<PropertyType>,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

/// One search location matches a listing when its postal code or commune
/// name appears in the location string (case-insensitive). "Rennes 35000"
/// therefore matches both by name and by code; a bare "35000" matches by
/// code only.
fn location_matches(location: &str, listing: &Listing) -> bool {
    let location = location.to_lowercase();
    if let Some(cp) = &listing.postal_code
        && !cp.is_empty()
        && location.contains(cp.as_str())
    {
        return true;
    }
    if let Some(commune) = &listing.commune
        && !commune.is_empty()
        && location.contains(&commune.to_lowercase())
    {
        return true;
    }
    false
}

/// Does a listing satisfy a search? Criteria the search sets fail CLOSED
/// on listings missing that attribute: a min-surface search never matches
/// a listing without a known surface (precision over recall — the noise
/// lesson from ferret).
pub fn search_matches(search: &Search, listing: &Listing) -> bool {
    if !search.active
        || listing.status != ListingStatus::Active
        || listing.moderation != Moderation::None
    {
        return false;
    }
    if !search.locations.is_empty()
        && !search.locations.iter().any(|l| location_matches(l, listing))
    {
        return false;
    }
    if let Some(max) = search.max_price_cents
        && listing.price_cents > max
    {
        return false;
    }
    if let Some(min) = search.min_surface_m2 {
        match listing.surface_m2 {
            Some(s) if s >= min => {}
            _ => return false,
        }
    }
    if let Some(min) = search.min_rooms {
        match listing.rooms {
            Some(r) if r >= min => {}
            _ => return false,
        }
    }
    if !search.property_types.is_empty()
        && !search.property_types.contains(&listing.property_type)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::ExtractedAttrs;

    fn listing() -> Listing {
        Listing {
            id: Uuid::nil(),
            source_id: "leboncoin-immo".into(),
            canonical_url: "https://x/1".into(),
            title: "Maison 5 pièces".into(),
            price_cents: 30_000_000,
            property_type: PropertyType::House,
            surface_m2: Some(110.0),
            rooms: Some(5),
            bedrooms: Some(3),
            land_m2: Some(400.0),
            commune: Some("Bruz".into()),
            postal_code: Some("35170".into()),
            lat: None,
            lng: None,
            dpe: Some("c".into()),
            ges: None,
            sell_type: Some("old".into()),
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

    fn search() -> Search {
        Search {
            id: Uuid::nil(),
            name: "maison bruz".into(),
            locations: vec!["Bruz 35170".into()],
            max_price_cents: Some(35_000_000),
            min_surface_m2: Some(90.0),
            min_rooms: Some(4),
            property_types: vec![PropertyType::House],
            active: true,
            created_at: chrono::DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn full_criteria_match() {
        assert!(search_matches(&search(), &listing()));
    }

    #[test]
    fn location_matches_by_code_or_name() {
        let mut s = search();
        s.locations = vec!["35170".into()];
        assert!(search_matches(&s, &listing()), "postal code alone");
        s.locations = vec!["bruz".into()];
        assert!(search_matches(&s, &listing()), "commune name, any case");
        s.locations = vec!["Rennes 35000".into()];
        assert!(!search_matches(&s, &listing()), "different commune");
        s.locations = vec![];
        assert!(search_matches(&s, &listing()), "no location = anywhere");
    }

    #[test]
    fn criteria_fail_closed_on_missing_attributes() {
        let mut l = listing();
        l.surface_m2 = None;
        assert!(!search_matches(&search(), &l), "min-surface set, surface unknown");
        let mut s = search();
        s.min_surface_m2 = None;
        assert!(search_matches(&s, &l), "criterion unset — missing attr is fine");
    }

    #[test]
    fn price_type_and_state_gates() {
        let s = search();
        let mut l = listing();
        l.price_cents = 40_000_000;
        assert!(!search_matches(&s, &l), "over budget");
        let mut l = listing();
        l.property_type = PropertyType::Apartment;
        assert!(!search_matches(&s, &l), "wrong type");
        let mut l = listing();
        l.status = ListingStatus::Gone;
        assert!(!search_matches(&s, &l), "gone listings never match");
        let mut l = listing();
        l.moderation = Moderation::Dismissed;
        assert!(!search_matches(&s, &l), "moderated listings never match");
        let mut s = search();
        s.active = false;
        assert!(!search_matches(&s, &listing()), "paused search");
    }
}
