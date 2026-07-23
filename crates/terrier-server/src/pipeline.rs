//! The pipeline: raw listings → normalize → upsert (+price history) →
//! search match → notify (new + price drops) → lifecycle. Notifications
//! carry €/m² and surface; wanted ads never push (ferret's noise lesson,
//! applied from day 1).

use std::collections::HashSet;

use chrono::Utc;
use terrier_domain::{
    ExtractedAttrs, Flag, Listing, ListingStatus, Moderation, RawListing, Search, normalize,
    search_matches,
};
use uuid::Uuid;

use crate::config::ScrapeConfig;
use crate::db::{Db, UpsertOutcome};
use crate::notify::Notify;

#[derive(Debug, Default, PartialEq)]
pub struct PipelineStats {
    pub new_listings: u64,
    pub updated_listings: u64,
    pub skipped: u64,
    pub notified: u64,
    pub gone: u64,
    pub suppressed: u64,
}

fn to_listing(raw: RawListing) -> Option<Listing> {
    let title = normalize::clean_title(&raw.title);
    let canonical_url = normalize::canonical_url(&raw.url)?;
    let mut flags = Vec::new();
    if normalize::is_wanted_ad(&title) {
        flags.push(Flag::WantedAd);
    }
    let now = Utc::now();
    Some(Listing {
        id: Uuid::new_v4(),
        source_id: raw.source_id,
        canonical_url,
        title,
        price_cents: raw.price_cents,
        property_type: raw.property_type,
        surface_m2: raw.surface_m2,
        rooms: raw.rooms,
        bedrooms: raw.bedrooms,
        land_m2: raw.land_m2,
        commune: raw.commune,
        postal_code: raw.postal_code,
        lat: raw.lat,
        lng: raw.lng,
        dpe: raw.dpe,
        ges: raw.ges,
        sell_type: raw.sell_type,
        description: raw.description,
        address: raw.address,
        seller: raw.seller,
        attributes: ExtractedAttrs::default(),
        flags,
        status: ListingStatus::Active,
        moderation: Moderation::None,
        first_seen: now,
        last_seen: now,
    })
}

fn format_price(cents: i64) -> String {
    format!("{} €", cents / 100)
}

fn describe(listing: &Listing) -> String {
    let mut parts = vec![listing.title.clone()];
    let mut details = Vec::new();
    if let Some(s) = listing.surface_m2 {
        details.push(format!("{s:.0} m²"));
    }
    if let Some(m2) = listing.price_per_m2_cents() {
        details.push(format!("{} €/m²", m2 / 100));
    }
    if let Some(c) = &listing.commune {
        details.push(c.clone());
    }
    if !details.is_empty() {
        parts.push(details.join(" · "));
    }
    parts.push(listing.canonical_url.clone());
    parts.join("\n")
}

fn notification_worthy(listing: &Listing) -> bool {
    !listing.flags.contains(&Flag::WantedAd)
}

/// `run_lifecycle` must be true only for a full scheduled fetch.
pub async fn process_listings(
    db: &Db,
    scrape: &ScrapeConfig,
    source_id: &str,
    listings: Vec<RawListing>,
    notifier: &dyn Notify,
    run_lifecycle: bool,
) -> anyhow::Result<PipelineStats> {
    let mut stats = PipelineStats::default();
    let searches: Vec<Search> = db.list_searches().await?;
    let mut seen_urls: HashSet<String> = HashSet::new();

    for raw in listings {
        let image_urls = raw.image_urls.clone();
        let Some(listing) = to_listing(raw) else {
            stats.skipped += 1;
            continue;
        };
        seen_urls.insert(listing.canonical_url.clone());

        let (stored, outcome) = db.upsert_listing(&listing).await?;
        if !image_urls.is_empty() {
            db.add_image_urls(stored.id, &image_urls).await?;
        }
        match outcome {
            UpsertOutcome::New => {
                stats.new_listings += 1;
                db.enqueue_enrichment(stored.id, "new").await?;
            }
            UpsertOutcome::PriceChanged { .. } => {
                stats.updated_listings += 1;
                db.enqueue_enrichment(stored.id, "price-change").await?;
            }
            UpsertOutcome::Unchanged => stats.updated_listings += 1,
        }

        if stored.moderation != Moderation::None {
            continue;
        }
        for search in &searches {
            if !search_matches(search, &stored) {
                continue;
            }
            let fresh = db.insert_match(stored.id, search.id).await?;
            if fresh && !notification_worthy(&stored) {
                stats.suppressed += 1;
                continue;
            }
            if fresh {
                notifier
                    .send(
                        &format!("{}: {}", search.name, format_price(stored.price_cents)),
                        &describe(&stored),
                        "house_with_garden",
                        "default",
                    )
                    .await;
                db.mark_notified(stored.id, search.id, stored.price_cents).await?;
                stats.notified += 1;
            } else if let Some(prev) = db.notified_price(stored.id, search.id).await?
                && (stored.price_cents as f64)
                    <= (prev as f64) * (1.0 - scrape.renotify_drop_pct / 100.0)
                && notification_worthy(&stored)
            {
                notifier
                    .send(
                        &format!(
                            "{}: {} → {}",
                            search.name,
                            format_price(prev),
                            format_price(stored.price_cents)
                        ),
                        &format!("Baisse de prix\n{}", describe(&stored)),
                        "chart_with_downwards_trend",
                        "default",
                    )
                    .await;
                db.mark_notified(stored.id, search.id, stored.price_cents).await?;
                stats.notified += 1;
            }
        }
    }

    if run_lifecycle {
        stats.gone = db.mark_gone(source_id, &seen_urls).await?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    use terrier_domain::{PropertyType, SearchRequest};

    struct MockNotifier {
        sent: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl Notify for MockNotifier {
        async fn send(&self, title: &str, message: &str, _tags: &str, _priority: &str) {
            self.sent.lock().unwrap().push((title.to_string(), message.to_string()));
        }
    }

    fn raw(url: &str, price: i64, title: &str) -> RawListing {
        RawListing {
            source_id: "leboncoin-immo".into(),
            url: url.into(),
            title: title.into(),
            price_cents: price,
            property_type: PropertyType::House,
            surface_m2: Some(100.0),
            rooms: Some(5),
            bedrooms: Some(3),
            land_m2: None,
            commune: Some("Bruz".into()),
            postal_code: Some("35170".into()),
            lat: None,
            lng: None,
            dpe: Some("c".into()),
            ges: None,
            sell_type: Some("old".into()),
            description: None,
            address: None,
            image_urls: vec![],
            seller: None,
        }
    }

    async fn setup() -> (Db, MockNotifier) {
        let db = Db::connect(Path::new(":memory:")).await.unwrap();
        db.create_search(&SearchRequest {
            name: "maison bruz".into(),
            locations: vec!["Bruz 35170".into()],
            max_price_cents: Some(40_000_000),
            min_surface_m2: Some(80.0),
            min_rooms: None,
            property_types: vec![PropertyType::House],
            active: true,
        })
        .await
        .unwrap();
        (db, MockNotifier { sent: Mutex::new(Vec::new()) })
    }

    async fn run(db: &Db, listings: Vec<RawListing>, notifier: &MockNotifier) -> PipelineStats {
        process_listings(db, &ScrapeConfig::default(), "leboncoin-immo", listings, notifier, true)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn new_match_notifies_with_m2_price() {
        let (db, notifier) = setup().await;
        let stats =
            run(&db, vec![raw("https://x/1", 32_000_000, "Maison 5 pièces Bruz")], &notifier)
                .await;
        assert_eq!(stats.new_listings, 1);
        assert_eq!(stats.notified, 1);
        let sent = notifier.sent.lock().unwrap();
        assert!(sent[0].0.contains("320000 €"), "title: {}", sent[0].0);
        assert!(sent[0].1.contains("3200 €/m²"), "€/m² in the push: {}", sent[0].1);
        assert!(sent[0].1.contains("Bruz"));
    }

    #[tokio::test]
    async fn price_drop_renotifies_and_records_history() {
        let (db, notifier) = setup().await;
        run(&db, vec![raw("https://x/1", 32_000_000, "Maison 5 pièces Bruz")], &notifier).await;
        let stats =
            run(&db, vec![raw("https://x/1", 30_000_000, "Maison 5 pièces Bruz")], &notifier)
                .await;
        assert_eq!(stats.notified, 1, "6% drop re-notifies");
        let sent = notifier.sent.lock().unwrap();
        assert!(sent[1].0.contains("320000 € → 300000 €"), "{}", sent[1].0);
        assert!(sent[1].1.contains("Baisse de prix"));
    }

    #[tokio::test]
    async fn tiny_price_wiggle_stays_silent() {
        let (db, notifier) = setup().await;
        run(&db, vec![raw("https://x/1", 32_000_000, "Maison 5 pièces Bruz")], &notifier).await;
        // -0.3% < renotify_drop_pct (1%) → recorded, not pushed
        let stats =
            run(&db, vec![raw("https://x/1", 31_900_000, "Maison 5 pièces Bruz")], &notifier)
                .await;
        assert_eq!(stats.notified, 0);
        assert_eq!(notifier.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn wanted_ads_match_silently() {
        let (db, notifier) = setup().await;
        let stats =
            run(&db, vec![raw("https://x/1", 30_000_000, "Recherche maison Bruz")], &notifier)
                .await;
        assert_eq!(stats.suppressed, 1);
        assert!(notifier.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn new_and_price_change_enqueue_enrichment() {
        let (db, notifier) = setup().await;
        let mut r = raw("https://x/1", 32_000_000, "Maison 5 pièces Bruz");
        r.image_urls = vec!["https://cdn/a.jpg".into()];
        run(&db, vec![r.clone()], &notifier).await;
        assert_eq!(db.enrichment_depth().await.unwrap(), 1, "new listing enqueued");
        // baseline image urls stored from the search page
        let listings = db.list_listings(None, false).await.unwrap();
        let images = db.images_for(&[listings[0].id]).await.unwrap();
        assert_eq!(images[&listings[0].id].len(), 1);

        db.enrichment_done(listings[0].id).await.unwrap();
        run(&db, vec![r.clone()], &notifier).await;
        assert_eq!(db.enrichment_depth().await.unwrap(), 0, "unchanged: not re-enqueued");

        r.price_cents = 30_000_000;
        run(&db, vec![r], &notifier).await;
        assert_eq!(db.enrichment_depth().await.unwrap(), 1, "price change re-enqueued");
    }

    #[tokio::test]
    async fn unseen_listing_goes_gone_and_revives_silently() {
        let (db, notifier) = setup().await;
        run(&db, vec![raw("https://x/1", 32_000_000, "Maison 5 pièces Bruz")], &notifier).await;
        let stats = run(&db, vec![], &notifier).await;
        assert_eq!(stats.gone, 1);
        let stats =
            run(&db, vec![raw("https://x/1", 32_000_000, "Maison 5 pièces Bruz")], &notifier)
                .await;
        assert_eq!(stats.gone, 0);
        assert_eq!(stats.notified, 0, "revival is not a fresh match");
    }
}
