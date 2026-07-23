//! SQLite persistence. All row ↔ domain mapping happens here and only
//! here. Single-connection WAL, same conventions as ferret.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use terrier_domain::{
    Flag, Listing, ListingStatus, Moderation, PricePoint, PropertyType, Search, SearchRequest,
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("not found")]
    NotFound,
    #[error("invalid stored data: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    New,
    PriceChanged { old_price_cents: i64 },
    Unchanged,
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

fn property_type_str(t: PropertyType) -> &'static str {
    match t {
        PropertyType::House => "house",
        PropertyType::Apartment => "apartment",
        PropertyType::Land => "land",
        PropertyType::Other => "other",
    }
}

fn property_type_from(s: &str) -> PropertyType {
    match s {
        "house" => PropertyType::House,
        "apartment" => PropertyType::Apartment,
        "land" => PropertyType::Land,
        _ => PropertyType::Other,
    }
}

fn moderation_str(m: Moderation) -> &'static str {
    match m {
        Moderation::None => "none",
        Moderation::Dismissed => "dismissed",
        Moderation::Banned => "banned",
    }
}

fn seller_of(row: &sqlx::sqlite::SqliteRow) -> Option<terrier_domain::Seller> {
    let kind = match row.get::<Option<String>, _>("seller_type").as_deref() {
        Some("pro") => terrier_domain::SellerKind::Pro,
        Some("private") => terrier_domain::SellerKind::Private,
        _ => return None,
    };
    Some(terrier_domain::Seller {
        kind,
        name: row.get("seller_name"),
        siren: row.get("siren"),
    })
}

fn seller_cols(s: &Option<terrier_domain::Seller>) -> (Option<&str>, Option<&str>, Option<&str>) {
    match s {
        Some(s) => (
            s.name.as_deref(),
            Some(match s.kind {
                terrier_domain::SellerKind::Pro => "pro",
                terrier_domain::SellerKind::Private => "private",
            }),
            s.siren.as_deref(),
        ),
        None => (None, None, None),
    }
}

/// One listing_images row as the API needs it.
#[derive(Debug, Clone)]
pub struct DbImage {
    pub position: i64,
    pub url: String,
    pub local_path: Option<String>,
}

/// What the enrichment worker needs to decide its steps.
#[derive(Debug, Clone)]
pub struct EnrichState {
    pub listing: Listing,
    pub enriched_at: Option<DateTime<Utc>>,
    pub extracted_at: Option<DateTime<Utc>>,
}

impl Db {
    pub async fn connect(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    // ---- searches ----

    pub async fn create_search(&self, req: &SearchRequest) -> Result<Search> {
        let search = Search {
            id: Uuid::new_v4(),
            name: req.name.clone(),
            locations: req.locations.clone(),
            max_price_cents: req.max_price_cents,
            min_surface_m2: req.min_surface_m2,
            min_rooms: req.min_rooms,
            property_types: req.property_types.clone(),
            active: req.active,
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO searches (id, name, locations, max_price_cents, min_surface_m2,
             min_rooms, property_types, active, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(search.id.to_string())
        .bind(&search.name)
        .bind(serde_json::to_string(&search.locations).expect("locations serialize"))
        .bind(search.max_price_cents)
        .bind(search.min_surface_m2)
        .bind(search.min_rooms)
        .bind(serde_json::to_string(&search.property_types).expect("types serialize"))
        .bind(search.active)
        .bind(search.created_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(search)
    }

    pub async fn list_searches(&self) -> Result<Vec<Search>> {
        let rows = sqlx::query("SELECT * FROM searches ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_search).collect()
    }

    pub async fn update_search(&self, id: Uuid, req: &SearchRequest) -> Result<Search> {
        let result = sqlx::query(
            "UPDATE searches SET name = ?, locations = ?, max_price_cents = ?,
             min_surface_m2 = ?, min_rooms = ?, property_types = ?, active = ? WHERE id = ?",
        )
        .bind(&req.name)
        .bind(serde_json::to_string(&req.locations).expect("locations serialize"))
        .bind(req.max_price_cents)
        .bind(req.min_surface_m2)
        .bind(req.min_rooms)
        .bind(serde_json::to_string(&req.property_types).expect("types serialize"))
        .bind(req.active)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        let row = sqlx::query("SELECT * FROM searches WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?;
        row_to_search(&row)
    }

    pub async fn delete_search(&self, id: Uuid) -> Result<()> {
        let result = sqlx::query("DELETE FROM searches WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    // ---- listings ----

    /// Insert, or update mutable fields for a known (source, url) pair —
    /// reviving gone listings, clearing a dismissal on re-acquire (bans
    /// stick), and recording every price change in listing_prices.
    pub async fn upsert_listing(&self, listing: &Listing) -> Result<(Listing, UpsertOutcome)> {
        let existing =
            sqlx::query("SELECT * FROM listings WHERE source_id = ? AND canonical_url = ?")
                .bind(&listing.source_id)
                .bind(&listing.canonical_url)
                .fetch_optional(&self.pool)
                .await?;
        match existing {
            None => {
                let (s_name, s_type, s_siren) = seller_cols(&listing.seller);
                sqlx::query(
                    "INSERT INTO listings (id, source_id, canonical_url, title, price_cents,
                     property_type, surface_m2, rooms, bedrooms, land_m2, commune, postal_code,
                     lat, lng, dpe, ges, sell_type, description, address, seller_name,
                     seller_type, siren, attributes, flags, status, moderation, first_seen,
                     last_seen)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                             'active', 'none', ?, ?)",
                )
                .bind(listing.id.to_string())
                .bind(&listing.source_id)
                .bind(&listing.canonical_url)
                .bind(&listing.title)
                .bind(listing.price_cents)
                .bind(property_type_str(listing.property_type))
                .bind(listing.surface_m2)
                .bind(listing.rooms)
                .bind(listing.bedrooms)
                .bind(listing.land_m2)
                .bind(&listing.commune)
                .bind(&listing.postal_code)
                .bind(listing.lat)
                .bind(listing.lng)
                .bind(&listing.dpe)
                .bind(&listing.ges)
                .bind(&listing.sell_type)
                .bind(&listing.description)
                .bind(&listing.address)
                .bind(s_name)
                .bind(s_type)
                .bind(s_siren)
                .bind(serde_json::to_string(&listing.attributes).expect("attrs serialize"))
                .bind(serde_json::to_string(&listing.flags).expect("flags serialize"))
                .bind(listing.first_seen.to_rfc3339())
                .bind(listing.last_seen.to_rfc3339())
                .execute(&self.pool)
                .await?;
                self.record_price(listing.id, listing.price_cents).await?;
                let stored = Listing {
                    status: ListingStatus::Active,
                    ..listing.clone()
                };
                Ok((stored, UpsertOutcome::New))
            }
            Some(row) => {
                let stored = row_to_listing(&row)?;
                let merged_seller = listing.seller.clone().or(stored.seller.clone());
                let (s_name, s_type, s_siren) = seller_cols(&merged_seller);
                sqlx::query(
                    "UPDATE listings SET title = ?, price_cents = ?, property_type = ?,
                     surface_m2 = ?, rooms = ?, bedrooms = ?, land_m2 = ?, commune = ?,
                     postal_code = ?, lat = ?, lng = ?, dpe = ?, ges = ?, sell_type = ?,
                     description = COALESCE(description, ?),
                     address = COALESCE(address, ?),
                     seller_name = ?, seller_type = ?, siren = ?,
                     flags = ?, status = 'active',
                     moderation = CASE
                         WHEN status = 'gone' AND moderation = 'dismissed' THEN 'none'
                         ELSE moderation END,
                     last_seen = ? WHERE id = ?",
                )
                .bind(&listing.title)
                .bind(listing.price_cents)
                .bind(property_type_str(listing.property_type))
                .bind(listing.surface_m2)
                .bind(listing.rooms)
                .bind(listing.bedrooms)
                .bind(listing.land_m2)
                .bind(&listing.commune)
                .bind(&listing.postal_code)
                .bind(listing.lat)
                .bind(listing.lng)
                .bind(&listing.dpe)
                .bind(&listing.ges)
                .bind(&listing.sell_type)
                .bind(&listing.description)
                .bind(&listing.address)
                .bind(s_name)
                .bind(s_type)
                .bind(s_siren)
                .bind(serde_json::to_string(&listing.flags).expect("flags serialize"))
                .bind(listing.last_seen.to_rfc3339())
                .bind(stored.id.to_string())
                .execute(&self.pool)
                .await?;
                let outcome = if stored.price_cents != listing.price_cents {
                    self.record_price(stored.id, listing.price_cents).await?;
                    UpsertOutcome::PriceChanged {
                        old_price_cents: stored.price_cents,
                    }
                } else {
                    UpsertOutcome::Unchanged
                };
                let merged = Listing {
                    id: stored.id,
                    first_seen: stored.first_seen,
                    status: ListingStatus::Active,
                    moderation: match (stored.status, stored.moderation) {
                        (ListingStatus::Gone, Moderation::Dismissed) => Moderation::None,
                        _ => stored.moderation,
                    },
                    description: stored.description.clone().or(listing.description.clone()),
                    address: stored.address.clone().or(listing.address.clone()),
                    seller: merged_seller,
                    attributes: stored.attributes.clone(),
                    ..listing.clone()
                };
                Ok((merged, outcome))
            }
        }
    }

    async fn record_price(&self, listing_id: Uuid, price_cents: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO listing_prices (listing_id, day, price_cents) VALUES (?, ?, ?)
             ON CONFLICT (listing_id, day) DO UPDATE SET price_cents = excluded.price_cents",
        )
        .bind(listing_id.to_string())
        .bind(Utc::now().date_naive().to_string())
        .bind(price_cents)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `hidden = false` excludes moderated listings; `true` lists ONLY them.
    pub async fn list_listings(
        &self,
        search_id: Option<Uuid>,
        hidden: bool,
    ) -> Result<Vec<Listing>> {
        let filter = if hidden {
            "l.moderation != 'none'"
        } else {
            "l.moderation = 'none'"
        };
        let rows = match search_id {
            Some(s) => {
                sqlx::query(&format!(
                    "SELECT l.* FROM listings l
                     JOIN search_matches m ON m.listing_id = l.id
                     WHERE m.search_id = ? AND {filter} ORDER BY l.last_seen DESC",
                ))
                .bind(s.to_string())
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(&format!(
                    "SELECT l.* FROM listings l WHERE {filter} ORDER BY l.last_seen DESC"
                ))
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(row_to_listing).collect()
    }

    #[allow(dead_code)] // future per-listing detail endpoint
    pub async fn listing_prices(&self, listing_id: Uuid) -> Result<Vec<PricePoint>> {
        let rows = sqlx::query(
            "SELECT day, price_cents FROM listing_prices WHERE listing_id = ? ORDER BY day",
        )
        .bind(listing_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| PricePoint {
                day: r.get("day"),
                price_cents: r.get("price_cents"),
            })
            .collect())
    }

    /// Price history for many listings at once (the inline sparklines) —
    /// one query, grouped in memory.
    pub async fn prices_for(&self, listing_ids: &[Uuid]) -> Result<HashMap<Uuid, Vec<PricePoint>>> {
        let mut map: HashMap<Uuid, Vec<PricePoint>> = HashMap::new();
        // chunk to stay under SQLite's bind limit
        for chunk in listing_ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT listing_id, day, price_cents FROM listing_prices
                 WHERE listing_id IN ({placeholders}) ORDER BY day"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id.to_string());
            }
            for row in q.fetch_all(&self.pool).await? {
                let id = parse_uuid(&row.get::<String, _>("listing_id"))?;
                map.entry(id).or_default().push(PricePoint {
                    day: row.get("day"),
                    price_cents: row.get("price_cents"),
                });
            }
        }
        Ok(map)
    }

    pub async fn mark_gone(&self, source_id: &str, seen: &HashSet<String>) -> Result<u64> {
        let rows = sqlx::query(
            "SELECT id, canonical_url FROM listings WHERE source_id = ? AND status = 'active'",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        let mut gone = 0;
        for row in rows {
            let url: String = row.get("canonical_url");
            if !seen.contains(&url) {
                sqlx::query("UPDATE listings SET status = 'gone' WHERE id = ?")
                    .bind(row.get::<String, _>("id"))
                    .execute(&self.pool)
                    .await?;
                gone += 1;
            }
        }
        Ok(gone)
    }

    pub async fn set_moderation(&self, listing_id: Uuid, moderation: Moderation) -> Result<()> {
        let result = sqlx::query("UPDATE listings SET moderation = ? WHERE id = ?")
            .bind(moderation_str(moderation))
            .bind(listing_id.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        if moderation != Moderation::None {
            sqlx::query("DELETE FROM search_matches WHERE listing_id = ?")
                .bind(listing_id.to_string())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    // ---- matches ----

    /// True when the match is new.
    pub async fn insert_match(&self, listing_id: Uuid, search_id: Uuid) -> Result<bool> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO search_matches (search_id, listing_id, matched_at)
             VALUES (?, ?, ?)",
        )
        .bind(search_id.to_string())
        .bind(listing_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_notified(
        &self,
        listing_id: Uuid,
        search_id: Uuid,
        price_cents: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE search_matches SET notified_price_cents = ?
             WHERE search_id = ? AND listing_id = ?",
        )
        .bind(price_cents)
        .bind(search_id.to_string())
        .bind(listing_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn notified_price(&self, listing_id: Uuid, search_id: Uuid) -> Result<Option<i64>> {
        let row = sqlx::query(
            "SELECT notified_price_cents FROM search_matches
             WHERE search_id = ? AND listing_id = ?",
        )
        .bind(search_id.to_string())
        .bind(listing_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| r.get::<Option<i64>, _>("notified_price_cents")))
    }

    pub async fn count_matches(&self) -> Result<HashMap<Uuid, i64>> {
        let rows =
            sqlx::query("SELECT search_id, COUNT(*) AS n FROM search_matches GROUP BY search_id")
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    parse_uuid(&r.get::<String, _>("search_id"))?,
                    r.get::<i64, _>("n"),
                ))
            })
            .collect()
    }

    // ---- commune stats ----

    /// Median €/m² per commune, now and ≥30 days ago, over active
    /// unmoderated listings with a known surface.
    pub async fn commune_stats(&self) -> Result<Vec<terrier_domain::CommuneStat>> {
        let rows = sqlx::query(
            "SELECT id, commune, postal_code, price_cents, surface_m2 FROM listings
             WHERE status = 'active' AND moderation = 'none'
               AND commune IS NOT NULL AND surface_m2 >= 1.0",
        )
        .fetch_all(&self.pool)
        .await?;
        let cutoff = (Utc::now() - chrono::Duration::days(30))
            .date_naive()
            .to_string();
        let old_rows = sqlx::query(
            "SELECT l.commune, p.price_cents, l.surface_m2 FROM listing_prices p
             JOIN listings l ON l.id = p.listing_id
             WHERE p.day <= ? AND l.commune IS NOT NULL AND l.surface_m2 >= 1.0",
        )
        .bind(&cutoff)
        .fetch_all(&self.pool)
        .await?;

        let mut now: HashMap<String, (Option<String>, Vec<i64>)> = HashMap::new();
        for r in &rows {
            let commune: String = r.get("commune");
            let m2 = (r.get::<i64, _>("price_cents") as f64 / r.get::<f64, _>("surface_m2")).round()
                as i64;
            let e = now
                .entry(commune)
                .or_insert_with(|| (r.get("postal_code"), Vec::new()));
            e.1.push(m2);
        }
        let mut old: HashMap<String, Vec<i64>> = HashMap::new();
        for r in &old_rows {
            let commune: String = r.get("commune");
            let m2 = (r.get::<i64, _>("price_cents") as f64 / r.get::<f64, _>("surface_m2")).round()
                as i64;
            old.entry(commune).or_default().push(m2);
        }

        fn median(v: &mut [i64]) -> Option<i64> {
            if v.is_empty() {
                return None;
            }
            v.sort_unstable();
            Some(v[(v.len() - 1) / 2])
        }

        let mut stats: Vec<terrier_domain::CommuneStat> = now
            .into_iter()
            .map(
                |(commune, (postal_code, mut m2s))| terrier_domain::CommuneStat {
                    listings: m2s.len() as i64,
                    median_m2_cents: median(&mut m2s),
                    median_m2_cents_30d: old.get(&commune).and_then(|v| median(&mut v.clone())),
                    commune,
                    postal_code,
                },
            )
            .collect();
        stats.sort_by(|a, b| b.listings.cmp(&a.listings).then(a.commune.cmp(&b.commune)));
        Ok(stats)
    }

    // ---- settings ----

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("value")))
    }

    pub async fn put_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- enrichment ----

    pub async fn enqueue_enrichment(&self, id: Uuid, reason: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO enrichment_queue (listing_id, reason, attempts, next_attempt_at)
             VALUES (?, ?, 0, ?)
             ON CONFLICT (listing_id) DO UPDATE SET
                 reason = excluded.reason, next_attempt_at = excluded.next_attempt_at,
                 attempts = 0, last_error = NULL",
        )
        .bind(id.to_string())
        .bind(reason)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        if reason == "price-change" {
            // force a detail re-fetch: the description may have changed
            sqlx::query("UPDATE listings SET enriched_at = NULL WHERE id = ?")
                .bind(id.to_string())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Queue items ready now for one source, oldest first.
    pub async fn due_enrichment(&self, source_id: &str, limit: i64) -> Result<Vec<Uuid>> {
        let rows = sqlx::query(
            "SELECT q.listing_id FROM enrichment_queue q
             JOIN listings l ON l.id = q.listing_id
             WHERE l.source_id = ? AND q.next_attempt_at <= ?
             ORDER BY q.next_attempt_at LIMIT ?",
        )
        .bind(source_id)
        .bind(Utc::now().to_rfc3339())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| parse_uuid(&r.get::<String, _>("listing_id")))
            .collect()
    }

    pub async fn enrichment_done(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM enrichment_queue WHERE listing_id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Exponential backoff (60s base, 6h cap); deletes at the attempt cap.
    /// Returns true when the item was given up on.
    pub async fn enrichment_failed(
        &self,
        id: Uuid,
        error: &str,
        max_attempts: u32,
    ) -> Result<bool> {
        let row = sqlx::query("SELECT attempts FROM enrichment_queue WHERE listing_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(true) };
        let attempts: u32 = row.get::<i64, _>("attempts") as u32 + 1;
        if attempts >= max_attempts {
            self.enrichment_done(id).await?;
            return Ok(true);
        }
        let backoff = 60u64
            .saturating_mul(2u64.saturating_pow(attempts - 1))
            .min(21_600);
        sqlx::query(
            "UPDATE enrichment_queue SET attempts = ?, next_attempt_at = ?, last_error = ?
             WHERE listing_id = ?",
        )
        .bind(attempts as i64)
        .bind((Utc::now() + chrono::Duration::seconds(backoff as i64)).to_rfc3339())
        .bind(error)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(false)
    }

    pub async fn enrichment_depth(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM enrichment_queue")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("n"))
    }

    pub async fn enrichment_state(&self, id: Uuid) -> Result<EnrichState> {
        let row = sqlx::query("SELECT * FROM listings WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(DbError::NotFound)?;
        let enriched_at = row
            .get::<Option<String>, _>("enriched_at")
            .map(|s| parse_ts(&s))
            .transpose()?;
        let extracted_at = row
            .get::<Option<String>, _>("extracted_at")
            .map(|s| parse_ts(&s))
            .transpose()?;
        Ok(EnrichState {
            listing: row_to_listing(&row)?,
            enriched_at,
            extracted_at,
        })
    }

    /// Merge a detail fetch over the listing; marks the listing enriched.
    /// Returns true when the description changed (extraction re-triggers).
    pub async fn set_detail(&self, id: Uuid, d: &terrier_domain::ListingDetail) -> Result<bool> {
        let stored: Option<String> = sqlx::query("SELECT description FROM listings WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(DbError::NotFound)?
            .get("description");
        let changed = d.description.is_some() && d.description != stored;
        if !d.image_urls.is_empty() {
            self.add_image_urls(id, &d.image_urls).await?;
        }
        let (s_name, s_type, s_siren) = seller_cols(&d.seller);
        sqlx::query(
            "UPDATE listings SET
                 description = COALESCE(?, description),
                 address = COALESCE(?, address),
                 seller_name = COALESCE(?, seller_name),
                 seller_type = COALESCE(?, seller_type),
                 siren = COALESCE(?, siren),
                 enriched_at = ?,
                 extracted_at = CASE WHEN ? THEN NULL ELSE extracted_at END
             WHERE id = ?",
        )
        .bind(&d.description)
        .bind(&d.address)
        .bind(s_name)
        .bind(s_type)
        .bind(s_siren)
        .bind(Utc::now().to_rfc3339())
        .bind(changed)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(changed)
    }

    /// Append unknown image urls after existing positions (idempotent;
    /// never reorders — UNIQUE(listing_id, url) depends on that).
    pub async fn add_image_urls(&self, id: Uuid, urls: &[String]) -> Result<()> {
        let rows = sqlx::query(
            "SELECT url, position FROM listing_images WHERE listing_id = ? ORDER BY position",
        )
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let known: HashSet<String> = rows.iter().map(|r| r.get("url")).collect();
        let mut next = rows
            .iter()
            .map(|r| r.get::<i64, _>("position"))
            .max()
            .map_or(0, |p| p + 1);
        for url in urls {
            if known.contains(url) {
                continue;
            }
            sqlx::query(
                "INSERT OR IGNORE INTO listing_images (listing_id, position, url) VALUES (?, ?, ?)",
            )
            .bind(id.to_string())
            .bind(next)
            .bind(url)
            .execute(&self.pool)
            .await?;
            next += 1;
        }
        Ok(())
    }

    pub async fn pending_images(&self, id: Uuid, limit: i64) -> Result<Vec<(i64, String)>> {
        let rows = sqlx::query(
            "SELECT position, url FROM listing_images
             WHERE listing_id = ? AND local_path IS NULL ORDER BY position LIMIT ?",
        )
        .bind(id.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("position"), r.get("url")))
            .collect())
    }

    pub async fn mark_image_saved(&self, id: Uuid, position: i64, local_path: &str) -> Result<()> {
        sqlx::query(
            "UPDATE listing_images SET local_path = ?, fetched_at = ?
             WHERE listing_id = ? AND position = ?",
        )
        .bind(local_path)
        .bind(Utc::now().to_rfc3339())
        .bind(id.to_string())
        .bind(position)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// All images for many listings at once (the API's inline image lists).
    pub async fn images_for(&self, ids: &[Uuid]) -> Result<HashMap<Uuid, Vec<DbImage>>> {
        let mut map: HashMap<Uuid, Vec<DbImage>> = HashMap::new();
        for chunk in ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT listing_id, position, url, local_path FROM listing_images
                 WHERE listing_id IN ({placeholders}) ORDER BY position"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id.to_string());
            }
            for row in q.fetch_all(&self.pool).await? {
                let id = parse_uuid(&row.get::<String, _>("listing_id"))?;
                map.entry(id).or_default().push(DbImage {
                    position: row.get("position"),
                    url: row.get("url"),
                    local_path: row.get("local_path"),
                });
            }
        }
        Ok(map)
    }

    pub async fn set_attributes(
        &self,
        id: Uuid,
        attrs: &terrier_domain::ExtractedAttrs,
    ) -> Result<()> {
        sqlx::query("UPDATE listings SET attributes = ?, extracted_at = ? WHERE id = ?")
            .bind(serde_json::to_string(attrs).expect("attrs serialize"))
            .bind(Utc::now().to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn log_llm_request(&self, e: &crate::llm::LlmLogEntry) -> Result<()> {
        sqlx::query(
            "INSERT INTO llm_requests (kind, model, duration_ms, ok, error,
             prompt_tokens, completion_tokens, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.kind)
        .bind(&e.model)
        .bind(e.duration_ms)
        .bind(e.ok)
        .bind(&e.error)
        .bind(e.prompt_tokens)
        .bind(e.completion_tokens)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Canned listing for tests in other modules.
#[cfg(test)]
pub fn tests_listing_helper(url: &str, price: i64) -> Listing {
    Listing {
        id: Uuid::new_v4(),
        source_id: "src".into(),
        canonical_url: url.into(),
        title: "Maison 5 pièces Bruz".into(),
        price_cents: price,
        property_type: PropertyType::House,
        surface_m2: Some(110.0),
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
        seller: None,
        attributes: Default::default(),
        flags: vec![],
        status: ListingStatus::Active,
        moderation: Moderation::None,
        first_seen: Utc::now(),
        last_seen: Utc::now(),
    }
}

fn parse_uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| DbError::Corrupt(format!("bad uuid {s:?}: {e}")))
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| DbError::Corrupt(format!("bad timestamp {s:?}: {e}")))
}

fn row_to_search(row: &sqlx::sqlite::SqliteRow) -> Result<Search> {
    Ok(Search {
        id: parse_uuid(&row.get::<String, _>("id"))?,
        name: row.get("name"),
        locations: serde_json::from_str(&row.get::<String, _>("locations"))
            .map_err(|e| DbError::Corrupt(format!("bad locations: {e}")))?,
        max_price_cents: row.get("max_price_cents"),
        min_surface_m2: row.get("min_surface_m2"),
        min_rooms: row.get("min_rooms"),
        property_types: serde_json::from_str(&row.get::<String, _>("property_types"))
            .map_err(|e| DbError::Corrupt(format!("bad property_types: {e}")))?,
        active: row.get("active"),
        created_at: parse_ts(&row.get::<String, _>("created_at"))?,
    })
}

fn row_to_listing(row: &sqlx::sqlite::SqliteRow) -> Result<Listing> {
    let status = match row.get::<String, _>("status").as_str() {
        "gone" => ListingStatus::Gone,
        _ => ListingStatus::Active,
    };
    let moderation = match row.get::<String, _>("moderation").as_str() {
        "dismissed" => Moderation::Dismissed,
        "banned" => Moderation::Banned,
        _ => Moderation::None,
    };
    let flags: Vec<Flag> = serde_json::from_str(&row.get::<String, _>("flags"))
        .map_err(|e| DbError::Corrupt(format!("bad flags: {e}")))?;
    Ok(Listing {
        id: parse_uuid(&row.get::<String, _>("id"))?,
        source_id: row.get("source_id"),
        canonical_url: row.get("canonical_url"),
        title: row.get("title"),
        price_cents: row.get("price_cents"),
        property_type: property_type_from(&row.get::<String, _>("property_type")),
        surface_m2: row.get("surface_m2"),
        rooms: row.get("rooms"),
        bedrooms: row.get("bedrooms"),
        land_m2: row.get("land_m2"),
        commune: row.get("commune"),
        postal_code: row.get("postal_code"),
        lat: row.get("lat"),
        lng: row.get("lng"),
        dpe: row.get("dpe"),
        ges: row.get("ges"),
        sell_type: row.get("sell_type"),
        description: row.get("description"),
        address: row.get("address"),
        seller: seller_of(row),
        attributes: serde_json::from_str(&row.get::<String, _>("attributes"))
            .map_err(|e| DbError::Corrupt(format!("bad attributes: {e}")))?,
        flags,
        status,
        moderation,
        first_seen: parse_ts(&row.get::<String, _>("first_seen"))?,
        last_seen: parse_ts(&row.get::<String, _>("last_seen"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use terrier_domain::PropertyType;

    async fn test_db() -> Db {
        Db::connect(Path::new(":memory:")).await.unwrap()
    }

    fn listing(url: &str, price: i64) -> Listing {
        Listing {
            id: Uuid::new_v4(),
            source_id: "src".into(),
            canonical_url: url.into(),
            title: "Maison 5 pièces Bruz".into(),
            price_cents: price,
            property_type: PropertyType::House,
            surface_m2: Some(110.0),
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
            seller: None,
            attributes: Default::default(),
            flags: vec![],
            status: ListingStatus::Active,
            moderation: Moderation::None,
            first_seen: Utc::now(),
            last_seen: Utc::now(),
        }
    }

    #[tokio::test]
    async fn upsert_records_price_history_and_revives() {
        let db = test_db().await;
        let (l1, o1) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        assert_eq!(o1, UpsertOutcome::New);

        // same day, same price → unchanged, one price row
        let (_, o2) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        assert_eq!(o2, UpsertOutcome::Unchanged);

        // price drop recorded (same day → latest wins)
        let (_, o3) = db
            .upsert_listing(&listing("https://x/1", 29_000_000))
            .await
            .unwrap();
        assert_eq!(
            o3,
            UpsertOutcome::PriceChanged {
                old_price_cents: 30_000_000
            }
        );
        let prices = db.listing_prices(l1.id).await.unwrap();
        assert_eq!(prices.len(), 1, "one row per day, latest wins");
        assert_eq!(prices[0].price_cents, 29_000_000);

        // gone / revive
        assert_eq!(db.mark_gone("src", &HashSet::new()).await.unwrap(), 1);
        let (l4, _) = db
            .upsert_listing(&listing("https://x/1", 29_000_000))
            .await
            .unwrap();
        assert_eq!(l4.status, ListingStatus::Active);
    }

    #[tokio::test]
    async fn moderation_semantics_match_ferret() {
        let db = test_db().await;
        let (l, _) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        let s = db
            .create_search(&SearchRequest {
                name: "s".into(),
                locations: vec![],
                max_price_cents: None,
                min_surface_m2: None,
                min_rooms: None,
                property_types: vec![],
                active: true,
            })
            .await
            .unwrap();
        db.insert_match(l.id, s.id).await.unwrap();

        db.set_moderation(l.id, Moderation::Dismissed)
            .await
            .unwrap();
        assert!(db.list_listings(None, false).await.unwrap().is_empty());
        assert_eq!(db.list_listings(None, true).await.unwrap().len(), 1);
        assert!(
            db.count_matches().await.unwrap().is_empty(),
            "matches dropped"
        );

        // dismissal clears on gone + re-acquire; ban survives it
        db.mark_gone("src", &HashSet::new()).await.unwrap();
        let (l2, _) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        assert_eq!(l2.moderation, Moderation::None);
        db.set_moderation(l.id, Moderation::Banned).await.unwrap();
        db.mark_gone("src", &HashSet::new()).await.unwrap();
        let (l3, _) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        assert_eq!(l3.moderation, Moderation::Banned);
    }

    #[tokio::test]
    async fn commune_stats_median() {
        let db = test_db().await;
        // 3 listings in Bruz: 110m²@300k, 100m²@350k, 50m²@200k
        db.upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        let mut l2 = listing("https://x/2", 35_000_000);
        l2.surface_m2 = Some(100.0);
        db.upsert_listing(&l2).await.unwrap();
        let mut l3 = listing("https://x/3", 20_000_000);
        l3.surface_m2 = Some(50.0);
        db.upsert_listing(&l3).await.unwrap();

        let stats = db.commune_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].commune, "Bruz");
        assert_eq!(stats[0].listings, 3);
        // €/m²: 2727, 3500, 4000 → median 3500 €/m² = 350_000 cents... in
        // cents/m²: 272_727, 350_000, 400_000 → median 350_000
        assert_eq!(stats[0].median_m2_cents, Some(350_000));
    }

    #[tokio::test]
    async fn upsert_keeps_enriched_description_and_updates_seller() {
        let db = test_db().await;
        let mut l = listing("https://x/1", 30_000_000);
        l.description = Some("truncated body".into());
        l.seller = Some(terrier_domain::Seller {
            kind: terrier_domain::SellerKind::Pro,
            name: Some("Agence X".into()),
            siren: Some("123456789".into()),
        });
        let (stored, _) = db.upsert_listing(&l).await.unwrap();
        assert_eq!(stored.description.as_deref(), Some("truncated body"));

        // enrichment stores the full description…
        db.set_detail(
            stored.id,
            &terrier_domain::ListingDetail {
                description: Some("the full, longer description".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // …and a later re-scrape with the truncated body must NOT clobber it
        let (again, _) = db.upsert_listing(&l).await.unwrap();
        assert_eq!(
            again.description.as_deref(),
            Some("the full, longer description")
        );
        assert_eq!(
            again.seller.as_ref().unwrap().name.as_deref(),
            Some("Agence X")
        );

        // a later re-scrape with a *different* seller (private, no siren)
        // must replace the stored seller atomically — not leave a stale
        // pro siren behind because only the non-null columns were COALESCEd.
        let mut l2 = l.clone();
        l2.seller = Some(terrier_domain::Seller {
            kind: terrier_domain::SellerKind::Private,
            name: Some("Jean".into()),
            siren: None,
        });
        db.upsert_listing(&l2).await.unwrap();
        let stored_listings = db.list_listings(None, false).await.unwrap();
        let stored = stored_listings.iter().find(|x| x.id == again.id).unwrap();
        let seller = stored.seller.as_ref().expect("seller present");
        assert_eq!(seller.kind, terrier_domain::SellerKind::Private);
        assert_eq!(seller.name.as_deref(), Some("Jean"));
        assert_eq!(
            seller.siren, None,
            "stale pro siren must not survive a seller change"
        );
    }

    #[tokio::test]
    async fn enrichment_queue_lifecycle() {
        let db = test_db().await;
        let (l, _) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        db.enqueue_enrichment(l.id, "new").await.unwrap();
        assert_eq!(db.enrichment_depth().await.unwrap(), 1);
        assert_eq!(db.due_enrichment("src", 10).await.unwrap(), vec![l.id]);

        // failure backs off into the future — no longer due
        let gave_up = db.enrichment_failed(l.id, "boom", 8).await.unwrap();
        assert!(!gave_up);
        assert!(db.due_enrichment("src", 10).await.unwrap().is_empty());

        // attempts cap deletes the item
        for _ in 0..7 {
            db.enrichment_failed(l.id, "boom", 8).await.unwrap();
        }
        assert_eq!(db.enrichment_depth().await.unwrap(), 0);

        // done removes it too
        db.enqueue_enrichment(l.id, "new").await.unwrap();
        db.enrichment_done(l.id).await.unwrap();
        assert_eq!(db.enrichment_depth().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn reenqueue_resets_attempt_budget() {
        let db = test_db().await;
        let (l, _) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        db.enqueue_enrichment(l.id, "new").await.unwrap();
        for _ in 0..7 {
            db.enrichment_failed(l.id, "boom", 8).await.unwrap();
        }
        // a fresh trigger gets a fresh budget
        db.enqueue_enrichment(l.id, "new").await.unwrap();
        assert!(!db.due_enrichment("src", 10).await.unwrap().is_empty());
        let gave_up = db.enrichment_failed(l.id, "x", 8).await.unwrap();
        assert!(!gave_up, "attempts should have restarted from 0");
        assert_eq!(db.enrichment_depth().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn price_change_enqueue_clears_enriched_at() {
        let db = test_db().await;
        let (l, _) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        db.set_detail(l.id, &terrier_domain::ListingDetail::default())
            .await
            .unwrap();
        assert!(
            db.enrichment_state(l.id)
                .await
                .unwrap()
                .enriched_at
                .is_some()
        );
        db.enqueue_enrichment(l.id, "price-change").await.unwrap();
        assert!(
            db.enrichment_state(l.id)
                .await
                .unwrap()
                .enriched_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn detail_merge_images_and_extraction_state() {
        let db = test_db().await;
        let (l, _) = db
            .upsert_listing(&listing("https://x/1", 30_000_000))
            .await
            .unwrap();
        db.add_image_urls(
            l.id,
            &["https://cdn/a.jpg".into(), "https://cdn/b.jpg".into()],
        )
        .await
        .unwrap();
        // idempotent by url, new urls append after existing positions
        db.add_image_urls(
            l.id,
            &["https://cdn/b.jpg".into(), "https://cdn/c.jpg".into()],
        )
        .await
        .unwrap();
        let pending = db.pending_images(l.id, 10).await.unwrap();
        assert_eq!(
            pending,
            vec![
                (0, "https://cdn/a.jpg".into()),
                (1, "https://cdn/b.jpg".into()),
                (2, "https://cdn/c.jpg".into())
            ]
        );
        db.mark_image_saved(l.id, 0, "xx/0.jpg").await.unwrap();
        assert_eq!(db.pending_images(l.id, 10).await.unwrap().len(), 2);
        let by_listing = db.images_for(&[l.id]).await.unwrap();
        assert_eq!(by_listing[&l.id].len(), 3);
        assert_eq!(by_listing[&l.id][0].local_path.as_deref(), Some("xx/0.jpg"));

        // a changed description resets extraction; same description doesn't
        let attrs = terrier_domain::ExtractedAttrs {
            fibre: Some(true),
            ..Default::default()
        };
        db.set_attributes(l.id, &attrs).await.unwrap();
        assert!(
            db.enrichment_state(l.id)
                .await
                .unwrap()
                .extracted_at
                .is_some()
        );
        let detail = terrier_domain::ListingDetail {
            description: Some("desc v1".into()),
            ..Default::default()
        };
        assert!(db.set_detail(l.id, &detail).await.unwrap());
        let st = db.enrichment_state(l.id).await.unwrap();
        assert!(
            st.extracted_at.is_none(),
            "new description re-triggers extraction"
        );
        assert_eq!(
            st.listing.attributes.fibre,
            Some(true),
            "old attrs kept meanwhile"
        );
        db.set_attributes(l.id, &attrs).await.unwrap();
        assert!(
            !db.set_detail(l.id, &detail).await.unwrap(),
            "same description: no change"
        );
        assert!(
            db.enrichment_state(l.id)
                .await
                .unwrap()
                .extracted_at
                .is_some()
        );
    }
}
