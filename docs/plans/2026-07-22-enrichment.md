# Listing Enrichment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every listing gets locally-stored images, its full description, seller identity, finer address, and LLM-extracted structured attributes — via an enrichment queue + per-source background worker, with the LLM as a fail-open refinement layer (spec: `docs/specs/2026-07-22-enrichment-design.md`).

**Architecture:** The scrape pipeline stores what the search page embeds (truncated description, image URLs, seller) and enqueues new/price-changed listings; a per-source worker drains the queue: detail-page fetch → image downloads → LLM extraction, each step independent and retried with backoff. LLM calls go to any OpenAI-compatible endpoint (llama.cpp), ported from ferret's `llm.rs`.

**Tech Stack:** Rust workspace (axum + sqlx SQLite, Leptos CSR, reqwest). No new dependencies.

**Conventions for the executor:**
- All commands run through the nix dev shell: `nix develop -c cargo test --workspace` etc., from `/projects/rust/terrier`.
- No migration files: edit `crates/terrier-server/migrations/0001_init.sql` in place (nothing is deployed). If a local `terrier.db*` exists at the repo root, delete it (`rm -f terrier.db terrier.db-shm terrier.db-wal`) — the checksum of 0001 changes.
- Commit with `git -c commit.gpgsign=false commit` (no GPG key in this environment). End commit messages with the Co-Authored-By/Claude-Session trailer used in `git log -1`.
- Spec deviation, decided here: `charges_copro_month_cents` / `taxe_fonciere_year_cents` stay in cents in the domain (consistency with `price_cents`), but the LLM answers in **euros** (`_eur` fields) and `llm.rs` converts. Reason: asking a model for cents invites ×100 errors.

---

### Task 1: Domain types (Seller, ExtractedAttrs, ListingDetail, extended Listing/RawListing)

**Files:**
- Modify: `crates/terrier-domain/src/listing.rs`
- Modify: `crates/terrier-domain/src/lib.rs`
- Modify: `crates/terrier-domain/src/search.rs` (test literal)
- Create: `crates/terrier-domain/src/llm.rs`
- Modify: `crates/terrier-domain/src/status.rs`

- [ ] **Step 1: Write failing serde tests** — append to the `tests` module of `crates/terrier-domain/src/listing.rs`:

```rust
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
        assert_eq!(serde_json::to_string(&SellerKind::Private).unwrap(), "\"private\"");
    }
```

Also refactor the existing test's inline `Listing {...}` into a shared helper in the tests module (the existing `price_per_m2_needs_a_plausible_surface` test constructs one — extract it):

```rust
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
```

and rewrite `price_per_m2_needs_a_plausible_surface` to use `let mut l = sample_listing();`.

- [ ] **Step 2: Run to verify failure**

Run: `nix develop -c cargo test -p terrier-domain`
Expected: compile FAIL (`ExtractedAttrs` not found).

- [ ] **Step 3: Implement the types** in `crates/terrier-domain/src/listing.rs` (before `pub struct Listing`):

```rust
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
}

/// What a detail-page fetch yields — everything optional, merged over the
/// stored listing (None never clears a stored value).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListingDetail {
    pub description: Option<String>,
    pub address: Option<String>,
    pub image_urls: Vec<String>,
    pub seller: Option<Seller>,
}
```

Extend `Listing` (insert after `pub sell_type: Option<String>,`):

```rust
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
```

Extend `RawListing` (after `pub sell_type: Option<String>,`):

```rust
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub image_urls: Vec<String>,
    #[serde(default)]
    pub seller: Option<Seller>,
```

Extend `ListingWithHistory`:

```rust
    #[serde(default)]
    pub images: Vec<ListingImage>,
```

- [ ] **Step 4: Create `crates/terrier-domain/src/llm.rs`** (settings/prompts types shared by server, client and UI):

```rust
//! LLM configuration surface shared by the server API, client and UI.

use serde::{Deserialize, Serialize};

/// Effective settings as shown in the UI (never carries the key itself).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmSettings {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_set: bool,
    pub from_override: bool,
}

/// UI → server: DB-stored override of the `[llm]` TOML section.
/// Empty url/model fall back to TOML; `api_key: None` keeps the stored key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmSettingsUpdate {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// Overridable system prompts (empty = built-in default).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmPrompts {
    pub extract: String,
}
```

- [ ] **Step 5: Extend `crates/terrier-domain/src/status.rs`** — add to `StatusResponse`:

```rust
    /// Listings waiting in the enrichment queue.
    #[serde(default)]
    pub enrichment_pending: i64,
    #[serde(default)]
    pub llm: Option<LlmStatus>,
```

and add below `StatusResponse`:

```rust
/// LLM liveness for the status strip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmStatus {
    pub enabled: bool,
    pub model: Option<String>,
    /// In-flight extraction calls right now.
    pub busy: u32,
}
```

- [ ] **Step 6: Export from `crates/terrier-domain/src/lib.rs`** — add `pub mod llm;` alongside the existing modules and re-export the new names the way existing types are re-exported (match the file's style, e.g. `pub use llm::{LlmPrompts, LlmSettings, LlmSettingsUpdate};` and add `SellerKind, Seller, ListingImage, ExtractedAttrs, ListingDetail` and `LlmStatus` to the existing `pub use` lists).

- [ ] **Step 7: Fix every struct literal that no longer compiles.** Sites (verified by grep):
  - `crates/terrier-domain/src/search.rs` test `fn listing()` — add `description: None, address: None, seller: None, attributes: ExtractedAttrs::default(),`
  - `crates/terrier-server/src/db.rs` — `tests_listing_helper` and tests-module `fn listing()` — same four fields. (`merged` at db.rs:242 uses `..listing.clone()`, untouched.)
  - `crates/terrier-server/src/pipeline.rs` — `to_listing` gains the real mapping:
    ```rust
        description: raw.description,
        address: raw.address,
        seller: raw.seller,
        attributes: ExtractedAttrs::default(),
    ```
    (import `ExtractedAttrs` in the use list) — note `raw.image_urls` is NOT on `Listing`; Task 4 handles it. Test helper `fn raw()` gains `description: None, address: None, image_urls: vec![], seller: None,`.
  - `crates/terrier-server/src/scrape/leboncoin.rs` `RawListing` literal — add `description: None, address: None, image_urls: vec![], seller: None,` (real values come in Task 5).
  - `crates/terrier-server/src/scrape/ouestfrance.rs` `RawListing` literal — same four defaults.

- [ ] **Step 8: Run the full workspace tests**

Run: `nix develop -c cargo test --workspace`
Expected: PASS (45 existing + 3 new).

- [ ] **Step 9: Commit**

```bash
git add -A && git -c commit.gpgsign=false commit -m "domain: seller, images, description, extracted attributes types"
```

---

### Task 2: Schema + DB mapping (columns, upsert semantics)

**Files:**
- Modify: `crates/terrier-server/migrations/0001_init.sql`
- Modify: `crates/terrier-server/src/db.rs`

- [ ] **Step 1: Write failing DB tests** — append to the tests module of `db.rs`:

```rust
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
        assert_eq!(again.description.as_deref(), Some("the full, longer description"));
        assert_eq!(again.seller.as_ref().unwrap().name.as_deref(), Some("Agence X"));
    }
```

(`set_detail` is implemented in Task 3 — for THIS task stub it so the test compiles, full version next task:)

```rust
    // in impl Db (real logic in the enrichment task)
    pub async fn set_detail(
        &self,
        id: Uuid,
        d: &terrier_domain::ListingDetail,
    ) -> Result<bool> {
        let changed = d.description.is_some();
        if let Some(desc) = &d.description {
            sqlx::query("UPDATE listings SET description = ?, enriched_at = ? WHERE id = ?")
                .bind(desc)
                .bind(Utc::now().to_rfc3339())
                .bind(id.to_string())
                .execute(&self.pool)
                .await?;
        }
        Ok(changed)
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `nix develop -c cargo test -p terrier-server db::`
Expected: FAIL (no such column: description).

- [ ] **Step 3: Extend the schema.** In `0001_init.sql`, inside `CREATE TABLE listings`, after `sell_type TEXT,` add:

```sql
    description TEXT,
    address TEXT,
    seller_name TEXT,
    seller_type TEXT,               -- 'pro' | 'private'
    siren TEXT,
    attributes TEXT NOT NULL DEFAULT '{}',  -- ExtractedAttrs JSON
    enriched_at TEXT,               -- detail fetch done (or not applicable)
    extracted_at TEXT,              -- LLM extraction done for current description
```

Append at the end of the file:

```sql
-- photos: downloaded once, kept when the listing goes gone
CREATE TABLE listing_images (
    listing_id TEXT NOT NULL REFERENCES listings (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    url TEXT NOT NULL,
    local_path TEXT,                  -- relative to images_dir once fetched
    fetched_at TEXT,
    PRIMARY KEY (listing_id, position),
    UNIQUE (listing_id, url)
);

-- listings awaiting enrichment (detail page, images, LLM extraction)
CREATE TABLE enrichment_queue (
    listing_id TEXT PRIMARY KEY REFERENCES listings (id) ON DELETE CASCADE,
    reason TEXT NOT NULL,             -- 'new' | 'price-change'
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT NOT NULL,
    last_error TEXT
);

-- one row per LLM call (ferret pattern)
CREATE TABLE llm_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    model TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    ok INTEGER NOT NULL,
    error TEXT,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    created_at TEXT NOT NULL
);
```

Run `rm -f terrier.db terrier.db-shm terrier.db-wal` (stale checksum).

- [ ] **Step 4: Extend `db.rs` row mapping.** Helpers near `moderation_str`:

```rust
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
```

In `row_to_listing`, add before `flags,`:

```rust
        description: row.get("description"),
        address: row.get("address"),
        seller: seller_of(row),
        attributes: serde_json::from_str(&row.get::<String, _>("attributes"))
            .map_err(|e| DbError::Corrupt(format!("bad attributes: {e}")))?,
```

- [ ] **Step 5: Extend `upsert_listing`.** INSERT branch — column list gains `description, address, seller_name, seller_type, siren` (after `sell_type`), with five extra `?` placeholders; bind after `.bind(&listing.sell_type)`:

```rust
                .bind(&listing.description)
                .bind(&listing.address)
                // seller
                .bind(seller_cols(&listing.seller).0)
                .bind(seller_cols(&listing.seller).1)
                .bind(seller_cols(&listing.seller).2)
```

UPDATE branch — after `sell_type = ?,` add (search data must never clobber enrichment: existing description/address win; seller freshens when the scrape carries one):

```rust
                     description = COALESCE(description, ?),
                     address = COALESCE(address, ?),
                     seller_name = COALESCE(?, seller_name),
                     seller_type = COALESCE(?, seller_type),
                     siren = COALESCE(?, siren),
```

with the same five binds in that order after `.bind(&listing.sell_type)`. The `merged` Listing at the end must reflect stored precedence — change its construction to:

```rust
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
                    seller: listing.seller.clone().or(stored.seller.clone()),
                    attributes: stored.attributes.clone(),
                    ..listing.clone()
                };
```

- [ ] **Step 6: Run tests**

Run: `nix develop -c cargo test -p terrier-server`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A && git -c commit.gpgsign=false commit -m "db: enrichment schema and listing columns"
```

---

### Task 3: DB enrichment methods (queue, images, attributes, LLM log)

**Files:**
- Modify: `crates/terrier-server/src/db.rs`

- [ ] **Step 1: Write failing tests** (append to db.rs tests):

```rust
    #[tokio::test]
    async fn enrichment_queue_lifecycle() {
        let db = test_db().await;
        let (l, _) = db.upsert_listing(&listing("https://x/1", 30_000_000)).await.unwrap();
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
    async fn price_change_enqueue_clears_enriched_at() {
        let db = test_db().await;
        let (l, _) = db.upsert_listing(&listing("https://x/1", 30_000_000)).await.unwrap();
        db.set_detail(l.id, &terrier_domain::ListingDetail::default()).await.unwrap();
        assert!(db.enrichment_state(l.id).await.unwrap().enriched_at.is_some());
        db.enqueue_enrichment(l.id, "price-change").await.unwrap();
        assert!(db.enrichment_state(l.id).await.unwrap().enriched_at.is_none());
    }

    #[tokio::test]
    async fn detail_merge_images_and_extraction_state() {
        let db = test_db().await;
        let (l, _) = db.upsert_listing(&listing("https://x/1", 30_000_000)).await.unwrap();
        db.add_image_urls(l.id, &["https://cdn/a.jpg".into(), "https://cdn/b.jpg".into()])
            .await
            .unwrap();
        // idempotent by url, new urls append after existing positions
        db.add_image_urls(l.id, &["https://cdn/b.jpg".into(), "https://cdn/c.jpg".into()])
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
        let attrs = terrier_domain::ExtractedAttrs { fibre: Some(true), ..Default::default() };
        db.set_attributes(l.id, &attrs).await.unwrap();
        assert!(db.enrichment_state(l.id).await.unwrap().extracted_at.is_some());
        let detail = terrier_domain::ListingDetail {
            description: Some("desc v1".into()),
            ..Default::default()
        };
        assert!(db.set_detail(l.id, &detail).await.unwrap());
        let st = db.enrichment_state(l.id).await.unwrap();
        assert!(st.extracted_at.is_none(), "new description re-triggers extraction");
        assert_eq!(st.listing.attributes.fibre, Some(true), "old attrs kept meanwhile");
        db.set_attributes(l.id, &attrs).await.unwrap();
        assert!(!db.set_detail(l.id, &detail).await.unwrap(), "same description: no change");
        assert!(db.enrichment_state(l.id).await.unwrap().extracted_at.is_some());
    }
```

- [ ] **Step 2: Run to verify failure** — `nix develop -c cargo test -p terrier-server db::` → compile FAIL.

- [ ] **Step 3: Implement.** Add to `impl Db` (a `// ---- enrichment ----` section) and a row struct above the impl:

```rust
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
```

```rust
    // ---- enrichment ----

    pub async fn enqueue_enrichment(&self, id: Uuid, reason: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO enrichment_queue (listing_id, reason, attempts, next_attempt_at)
             VALUES (?, ?, 0, ?)
             ON CONFLICT (listing_id) DO UPDATE SET
                 reason = excluded.reason, next_attempt_at = excluded.next_attempt_at",
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
        rows.iter().map(|r| parse_uuid(&r.get::<String, _>("listing_id"))).collect()
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
        let backoff = 60u64.saturating_mul(2u64.saturating_pow(attempts - 1)).min(21_600);
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
        Ok(EnrichState { listing: row_to_listing(&row)?, enriched_at, extracted_at })
    }

    /// Merge a detail fetch over the listing; marks the listing enriched.
    /// Returns true when the description changed (extraction re-triggers).
    pub async fn set_detail(&self, id: Uuid, d: &terrier_domain::ListingDetail) -> Result<bool> {
        let stored: Option<String> =
            sqlx::query("SELECT description FROM listings WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?
                .ok_or(DbError::NotFound)?
                .get("description");
        let changed = d.description.is_some() && d.description != stored;
        let (name, kind, siren) = seller_cols(&d.seller);
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
        .bind(name)
        .bind(kind)
        .bind(siren)
        .bind(Utc::now().to_rfc3339())
        .bind(changed)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if !d.image_urls.is_empty() {
            self.add_image_urls(id, &d.image_urls).await?;
        }
        Ok(changed)
    }

    /// Append unknown image urls after existing positions (idempotent).
    pub async fn add_image_urls(&self, id: Uuid, urls: &[String]) -> Result<()> {
        let rows = sqlx::query(
            "SELECT url, position FROM listing_images WHERE listing_id = ? ORDER BY position",
        )
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let known: HashSet<String> = rows.iter().map(|r| r.get("url")).collect();
        let mut next = rows.iter().map(|r| r.get::<i64, _>("position")).max().map_or(0, |p| p + 1);
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
        Ok(rows.iter().map(|r| (r.get("position"), r.get("url"))).collect())
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
```

Replace the Task-2 stub of `set_detail` with this real one. `log_llm_request` references `crate::llm::LlmLogEntry` — Task 6 creates it; until then keep the method commented out OR (preferred) do Task 6's `LlmLogEntry` struct early by creating a minimal `crates/terrier-server/src/llm.rs` now containing just:

```rust
//! LLM extraction (filled in by the llm task).

/// One logged LLM call (the llm_requests table).
#[derive(Debug, Clone)]
pub struct LlmLogEntry {
    pub kind: String,
    pub model: String,
    pub duration_ms: i64,
    pub ok: bool,
    pub error: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}
```

and `mod llm;` in `main.rs`.

Also remove the `#[allow(dead_code)]` markers from `get_setting`/`put_setting` (they get real callers in Task 7/11).

- [ ] **Step 4: Run tests** — `nix develop -c cargo test -p terrier-server` → PASS. (If `get_setting`/`put_setting` now warn as dead code because callers arrive later, keep the `#[allow(dead_code)]` until Task 11 and note it.)

- [ ] **Step 5: Commit**

```bash
git add -A && git -c commit.gpgsign=false commit -m "db: enrichment queue, images, attributes, llm log"
```

---

### Task 4: Pipeline enqueues enrichment and stores baseline images

**Files:**
- Modify: `crates/terrier-server/src/pipeline.rs`

- [ ] **Step 1: Write failing tests** (pipeline.rs tests module):

```rust
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
```

- [ ] **Step 2: Run to verify failure** — `nix develop -c cargo test -p terrier-server pipeline::` → FAIL (depth 0 ≠ 1).

- [ ] **Step 3: Implement.** In `process_listings`, keep the image urls before conversion and enqueue per outcome. Replace the block from `let Some(listing) = to_listing(raw)` through the `match outcome` with:

```rust
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
```

(The existing `match outcome` only distinguished New vs rest — note `UpsertOutcome::PriceChanged` is now matched explicitly; import stays `crate::db::{Db, UpsertOutcome}`.)

- [ ] **Step 4: Run tests** — `nix develop -c cargo test -p terrier-server` → PASS.
- [ ] **Step 5: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "pipeline: enqueue enrichment on new and price change"`

---

### Task 5: Leboncoin search parser — images, body, owner

**Files:**
- Modify: `crates/terrier-server/src/scrape/leboncoin.rs`
- Modify: `crates/terrier-server/tests/fixtures/leboncoin_immo_search.html`

- [ ] **Step 1: Extend the fixture.** Read the fixture; inside the `__NEXT_DATA__` JSON, the two active ads (Penthouse + house). Merge into the first (Penthouse) ad object:

```json
"body": "Penthouse d'exception au dernier étage, vue dégagée. Copropriété avec ascenseur, charges 180 € / mois.",
"images": {"nb_images": 2, "urls_large": ["https://img.leboncoin.fr/api/v1/pent-1.jpg", "https://img.leboncoin.fr/api/v1/pent-2.jpg"]},
"owner": {"type": "pro", "name": "Agence Horizon", "siren": "123456789"}
```

and into the second (house) ad:

```json
"body": "Maison familiale, jardin clos, garage.",
"images": {"nb_images": 1, "urls_large": ["https://img.leboncoin.fr/api/v1/house-1.jpg"]},
"owner": {"type": "private", "name": "Jean"}
```

- [ ] **Step 2: Write failing test assertions** — extend `parses_immo_fixture_with_structured_attributes` in leboncoin.rs:

```rust
        assert_eq!(flat.image_urls.len(), 2);
        assert!(flat.image_urls[0].ends_with("pent-1.jpg"));
        assert!(flat.description.as_deref().unwrap().starts_with("Penthouse d'exception"));
        let seller = flat.seller.as_ref().unwrap();
        assert_eq!(seller.kind, SellerKind::Pro);
        assert_eq!(seller.name.as_deref(), Some("Agence Horizon"));
        assert_eq!(seller.siren.as_deref(), Some("123456789"));
        assert_eq!(house.seller.as_ref().unwrap().kind, SellerKind::Private);
```

(add `SellerKind` to the test imports). Run: `nix develop -c cargo test -p terrier-server leboncoin` → FAIL.

- [ ] **Step 3: Implement.** In leboncoin.rs add helpers near `attr`/`property_type` (imports gain `Seller, SellerKind` from `terrier_domain`):

```rust
fn image_urls(ad: &serde_json::Value) -> Vec<String> {
    ad["images"]["urls_large"]
        .as_array()
        .or_else(|| ad["images"]["urls"].as_array())
        .map(|a| a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect())
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
        siren: ad["owner"]["siren"].as_str().or_else(|| attr(ad, "siren")).map(str::to_string),
    })
}
```

and in the `RawListing` literal replace the Task-1 placeholders:

```rust
            description: ad["body"].as_str().map(str::to_string),
            address: None,
            image_urls: image_urls(ad),
            seller: seller(ad),
```

- [ ] **Step 4: Run tests** — `nix develop -c cargo test -p terrier-server leboncoin` → PASS.
- [ ] **Step 5: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "leboncoin: capture images, body and owner from search json"`

---

### Task 6: Leboncoin detail-page parser + `fetch_detail` on the source trait

**Files:**
- Modify: `crates/terrier-server/src/scrape/mod.rs`
- Modify: `crates/terrier-server/src/scrape/leboncoin.rs`
- Create: `crates/terrier-server/tests/fixtures/leboncoin_immo_ad.html`

- [ ] **Step 1: Create the fixture** (synthetic, modeled on the real ad-page `__NEXT_DATA__` shape — `props.pageProps.ad` carries the same ad object as search results but with the full `body` and image set; validate against a live capture when the stealth box allows, but do not block on it):

```html
<!DOCTYPE html><html><head><title>Penthouse 5 pièces 139 m² - Rennes</title></head><body>
<div id="__next">…</div>
<script id="__NEXT_DATA__" type="application/json">
{"props":{"pageProps":{"ad":{
  "list_id": 2900000001,
  "subject": "Penthouse 5 pièces 139 m²",
  "body": "Penthouse d'exception au dernier étage avec ascenseur.\nCopropriété de 24 lots, charges 180 € par mois. Taxe foncière 2100 €.\nChauffage individuel gaz, fibre optique. Exposition sud-ouest.\nRue de la Monnaie, quartier Centre.",
  "url": "https://www.leboncoin.fr/ad/ventes_immobilieres/2900000001",
  "price": [1285000], "price_cents": 128500000, "status": "active",
  "attributes": [
    {"key": "real_estate_type", "value": "2"},
    {"key": "square", "value": "139"},
    {"key": "rooms", "value": "5"},
    {"key": "energy_rate", "value": "c"}
  ],
  "location": {"city": "Rennes", "zipcode": "35000", "lat": 48.111, "lng": -1.681,
               "street": "Rue de la Monnaie"},
  "images": {"nb_images": 3, "urls_large": [
    "https://img.leboncoin.fr/api/v1/pent-1.jpg",
    "https://img.leboncoin.fr/api/v1/pent-2.jpg",
    "https://img.leboncoin.fr/api/v1/pent-3.jpg"]},
  "owner": {"type": "pro", "name": "Agence Horizon", "siren": "123456789"}
}}}}
</script></body></html>
```

- [ ] **Step 2: Write failing tests** (leboncoin.rs tests):

```rust
    #[test]
    fn parses_ad_detail_page() {
        let html = include_str!("../../tests/fixtures/leboncoin_immo_ad.html");
        let d = parse_ad_page(html).unwrap();
        assert!(d.description.as_deref().unwrap().contains("Copropriété de 24 lots"));
        assert_eq!(d.image_urls.len(), 3);
        assert_eq!(d.address.as_deref(), Some("Rue de la Monnaie"));
        assert_eq!(d.seller.as_ref().unwrap().siren.as_deref(), Some("123456789"));
    }

    #[test]
    fn blocked_ad_page_is_a_hard_error() {
        assert!(parse_ad_page("<html>datadome says no</html>").is_err());
        // page with __NEXT_DATA__ but no ad (layout change) is also an error
        let html = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{}}</script></script>"#;
        assert!(parse_ad_page(html).is_err());
    }
```

Run: `nix develop -c cargo test -p terrier-server leboncoin` → compile FAIL.

- [ ] **Step 3: Implement.** In leboncoin.rs, first extract the `__NEXT_DATA__` slicing out of `parse_search_page` into a shared helper (the existing lines from `let start_tag` through `serde_json::from_str`):

```rust
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
```

then `parse_search_page` starts with `let data = next_data(html)?;`, and add:

```rust
/// The ad detail page: full body, complete image set, seller, street.
pub fn parse_ad_page(html: &str) -> anyhow::Result<terrier_domain::ListingDetail> {
    let data = next_data(html)?;
    let ad = &data["props"]["pageProps"]["ad"];
    anyhow::ensure!(ad.is_object(), "no props.pageProps.ad (blocked page or new layout)");
    Ok(terrier_domain::ListingDetail {
        description: ad["body"].as_str().map(str::to_string),
        address: ad["location"]["street"]
            .as_str()
            .or_else(|| ad["location"]["address"].as_str())
            .map(str::to_string),
        image_urls: image_urls(ad),
        seller: seller(ad),
    })
}
```

- [ ] **Step 4: Add `fetch_detail` to the trait.** In `scrape/mod.rs`:

```rust
use terrier_domain::ListingDetail;

#[async_trait::async_trait]
pub trait ImmoSource: Send + Sync {
    fn id(&self) -> &str;
    /// One full fetch over every configured location/page.
    async fn fetch(&self) -> anyhow::Result<Vec<RawListing>>;
    /// One listing's detail page; `Ok(None)` when the source has no
    /// detail support (the enricher then marks the listing enriched as-is).
    async fn fetch_detail(&self, _url: &str) -> anyhow::Result<Option<ListingDetail>> {
        Ok(None)
    }
}
```

and in `impl ImmoSource for LeboncoinSource`:

```rust
    async fn fetch_detail(
        &self,
        url: &str,
    ) -> anyhow::Result<Option<terrier_domain::ListingDetail>> {
        let html = self.fetch_page(url).await?;
        Ok(Some(parse_ad_page(&html)?))
    }
```

- [ ] **Step 5: Run tests** — `nix develop -c cargo test -p terrier-server` → PASS.
- [ ] **Step 6: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "leboncoin: ad detail-page parser and fetch_detail hook"`

---

### Task 7: LLM extraction module (ferret's llm.rs, trimmed to extraction)

**Files:**
- Modify: `crates/terrier-server/src/llm.rs` (replace the Task-3 stub file)
- Modify: `crates/terrier-server/src/config.rs`

Reference implementation: `/projects/rust/ferret/crates/ferret-server/src/llm.rs` — port faithfully; what follows is the terrier adaptation.

- [ ] **Step 1: Add `LlmConfig` and `EnrichmentConfig` to `config.rs`:**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    /// OpenAI-compatible base url (llama.cpp: http://host:8080/v1).
    pub base_url: String,
    pub model: String,
    pub api_key_file: Option<PathBuf>,
    pub timeout_secs: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://127.0.0.1:8080/v1".into(),
            model: String::new(),
            api_key_file: None,
            timeout_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrichmentConfig {
    /// Queue poll cadence per source.
    pub poll_seconds: u64,
    pub max_attempts: u32,
    /// Images downloaded per listing at most.
    pub max_images: usize,
    /// Where images land; relative paths resolve against the CWD.
    pub images_dir: PathBuf,
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            poll_seconds: 60,
            max_attempts: 8,
            max_images: 10,
            images_dir: "images".into(),
        }
    }
}
```

Add `pub llm: LlmConfig,` and `pub enrichment: EnrichmentConfig,` fields to `Config` + its `Default`. Extend `defaults_are_sane` test: `assert!(!config.llm.enabled);`.

- [ ] **Step 2: Write the module with tests.** Full `crates/terrier-server/src/llm.rs`:

```rust
//! LLM extraction of structured facts from listing descriptions, via any
//! OpenAI-compatible chat-completions API (llama.cpp on zeus by default).
//! Ported from ferret's llm.rs: one structured-output call per listing,
//! every error fail-open — the LLM is a refinement layer, never a
//! dependency.

use std::time::Duration;

use serde::Deserialize;
use terrier_domain::{ExtractedAttrs, LlmPrompts, LlmSettings, LlmSettingsUpdate, LlmStatus};

use crate::config::LlmConfig;

/// What the extractor sees about a listing.
pub struct ExtractInput<'a> {
    pub title: &'a str,
    pub price_cents: i64,
    pub property_type: &'a str,
    pub surface_m2: Option<f64>,
    pub rooms: Option<i64>,
    pub description: &'a str,
}

#[async_trait::async_trait]
pub trait LlmExtract: Send + Sync {
    async fn extract(&self, input: &ExtractInput<'_>) -> anyhow::Result<ExtractedAttrs>;
}

/// One logged LLM call (the llm_requests table).
#[derive(Debug, Clone)]
pub struct LlmLogEntry {
    pub kind: String,
    pub model: String,
    pub duration_ms: i64,
    pub ok: bool,
    pub error: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}

pub struct OpenAiExtractor {
    http: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    prompts: LlmPrompts,
    /// In-flight call count, shared with `/api/status` via the runtime.
    busy: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Request log sink (None in unit tests).
    db: Option<crate::db::Db>,
}

// ---- runtime configuration: TOML base + DB override, hot-swappable ----

pub const LLM_SETTINGS_KEY: &str = "llm";
pub const PROMPTS_SETTINGS_KEY: &str = "prompts";

/// Fully resolved LLM configuration, ready to build clients from.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveLlm {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
    pub api_key: Option<String>,
    pub from_override: bool,
    pub override_key_set: bool,
}

/// Merge the TOML base with an optional DB override. The key file is only
/// read when the result is enabled — a broken path never blocks startup of
/// a disabled pass.
pub fn effective(base: &LlmConfig, o: Option<&LlmSettingsUpdate>) -> anyhow::Result<EffectiveLlm> {
    let pick = |over: &str, conf: &str| {
        if over.trim().is_empty() { conf.to_string() } else { over.trim().to_string() }
    };
    let (enabled, base_url, model) = match o {
        Some(o) => (o.enabled, pick(&o.base_url, &base.base_url), pick(&o.model, &base.model)),
        None => (base.enabled, base.base_url.clone(), base.model.clone()),
    };
    let override_key = o.and_then(|o| o.api_key.clone()).filter(|k| !k.is_empty());
    let override_key_set = override_key.is_some();
    let api_key = match (&override_key, enabled) {
        (Some(key), _) => Some(key.clone()),
        (None, true) => match &base.api_key_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("reading llm api key {}: {e}", path.display()))?
                    .trim()
                    .to_string(),
            ),
            None => None,
        },
        (None, false) => None,
    };
    Ok(EffectiveLlm {
        enabled,
        base_url,
        model,
        timeout_secs: base.timeout_secs,
        api_key,
        from_override: o.is_some(),
        override_key_set,
    })
}

/// The live LLM layer, swapped in place when settings change so the
/// enrichment workers and API handlers pick the new backend up without a
/// restart.
#[derive(Clone, Default)]
pub struct LlmRuntime {
    pub extractor: Option<std::sync::Arc<dyn LlmExtract>>,
    pub status: LlmStatus,
    pub settings: LlmSettings,
    pub prompts: LlmPrompts,
    pub busy: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

pub type LlmHandle = std::sync::Arc<tokio::sync::RwLock<LlmRuntime>>;

pub fn build_runtime(eff: EffectiveLlm, prompts: LlmPrompts, db: Option<crate::db::Db>) -> LlmRuntime {
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let extractor = eff.enabled.then(|| {
        std::sync::Arc::new(OpenAiExtractor {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(eff.timeout_secs))
                .user_agent(concat!("terrier/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("building llm http client"),
            url: format!("{}/chat/completions", eff.base_url.trim_end_matches('/')),
            model: eff.model.clone(),
            api_key: eff.api_key.clone(),
            prompts: effective_prompts(Some(&prompts)),
            busy: busy.clone(),
            db,
        }) as std::sync::Arc<dyn LlmExtract>
    });
    LlmRuntime {
        extractor,
        busy,
        status: LlmStatus {
            enabled: eff.enabled,
            model: eff.enabled.then(|| eff.model.clone()),
            busy: 0,
        },
        settings: LlmSettings {
            enabled: eff.enabled,
            base_url: eff.base_url,
            model: eff.model,
            api_key_set: eff.override_key_set,
            from_override: eff.from_override,
        },
        prompts,
    }
}

pub async fn load_override(db: &crate::db::Db) -> Option<LlmSettingsUpdate> {
    let raw = db.get_setting(LLM_SETTINGS_KEY).await.ok()??;
    serde_json::from_str(&raw)
        .map_err(|e| tracing::warn!(error = %e, "ignoring corrupt llm settings override"))
        .ok()
}

pub async fn load_prompts(db: &crate::db::Db) -> Option<LlmPrompts> {
    let raw = db.get_setting(PROMPTS_SETTINGS_KEY).await.ok()??;
    serde_json::from_str(&raw)
        .map_err(|e| tracing::warn!(error = %e, "ignoring corrupt prompt override"))
        .ok()
}

// ---- system prompt: default here, user-overridable via settings ----

pub const EXTRACT_PROMPT: &str =
    "You extract structured facts from a French real-estate SALE listing for \
     a price tracker. Fill ONLY what the text explicitly states — never \
     guess, never infer from what is typical; absent from the text means \
     null (empty list for notes).\n\
     - annee_construction: the build year when stated.\n\
     - travaux: \"a-prevoir\" (works needed), \"rafraichissement\" (light \
       refresh), \"aucun\" ONLY if the text says none are needed / recently \
       renovated.\n\
     - chauffage_type (individuel/collectif/pompe à chaleur/poêle…), \
       chauffage_energie (gaz/electrique/fioul/bois…).\n\
     - charges_copro_month_eur: MONTHLY copropriété charges in euros \
       (convert if given per year/quarter). taxe_fonciere_year_eur: YEARLY \
       property tax in euros.\n\
     - etage: the apartment's floor (0 = rez-de-chaussée).\n\
     - orientation: main exposure when stated.\n\
     - notes: short French phrases for notable facts the other fields don't \
       cover (servitude, locataire en place, viager occupé, DPE vierge, \
       zone inondable, travaux de copropriété votés…).\n\
     Answer only with the JSON object.";

pub fn default_prompts() -> LlmPrompts {
    LlmPrompts { extract: EXTRACT_PROMPT.into() }
}

/// Stored override merged over the defaults (empty field = default).
pub fn effective_prompts(stored: Option<&LlmPrompts>) -> LlmPrompts {
    let defaults = default_prompts();
    let Some(stored) = stored else { return defaults };
    LlmPrompts {
        extract: if stored.extract.trim().is_empty() {
            defaults.extract
        } else {
            stored.extract.trim().to_string()
        },
    }
}

/// The JSON schema the model must answer with (strict structured output).
/// Money is asked in EUROS — models mangle cents; conversion happens here.
fn response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "annee_construction": { "type": ["integer", "null"] },
            "travaux": { "type": ["string", "null"],
                "enum": ["a-prevoir", "rafraichissement", "aucun", null] },
            "chauffage_type": { "type": ["string", "null"] },
            "chauffage_energie": { "type": ["string", "null"] },
            "fibre": { "type": ["boolean", "null"] },
            "charges_copro_month_eur": { "type": ["number", "null"] },
            "taxe_fonciere_year_eur": { "type": ["number", "null"] },
            "etage": { "type": ["integer", "null"] },
            "ascenseur": { "type": ["boolean", "null"] },
            "jardin": { "type": ["boolean", "null"] },
            "garage_parking": { "type": ["boolean", "null"] },
            "piscine": { "type": ["boolean", "null"] },
            "orientation": { "type": ["string", "null"] },
            "mitoyenne": { "type": ["boolean", "null"] },
            "notes": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["annee_construction", "travaux", "chauffage_type",
            "chauffage_energie", "fibre", "charges_copro_month_eur",
            "taxe_fonciere_year_eur", "etage", "ascenseur", "jardin",
            "garage_parking", "piscine", "orientation", "mitoyenne", "notes"],
        "additionalProperties": false
    })
}

/// The model's answer shape (euros); converted into the domain type.
#[derive(Debug, Deserialize)]
struct RawExtraction {
    annee_construction: Option<i64>,
    travaux: Option<String>,
    chauffage_type: Option<String>,
    chauffage_energie: Option<String>,
    fibre: Option<bool>,
    charges_copro_month_eur: Option<f64>,
    taxe_fonciere_year_eur: Option<f64>,
    etage: Option<i64>,
    ascenseur: Option<bool>,
    jardin: Option<bool>,
    garage_parking: Option<bool>,
    piscine: Option<bool>,
    orientation: Option<String>,
    mitoyenne: Option<bool>,
    #[serde(default)]
    notes: Vec<String>,
}

impl From<RawExtraction> for ExtractedAttrs {
    fn from(r: RawExtraction) -> Self {
        let cents = |eur: Option<f64>| eur.map(|e| (e * 100.0).round() as i64);
        ExtractedAttrs {
            annee_construction: r.annee_construction,
            travaux: r.travaux,
            chauffage_type: r.chauffage_type,
            chauffage_energie: r.chauffage_energie,
            fibre: r.fibre,
            charges_copro_month_cents: cents(r.charges_copro_month_eur),
            taxe_fonciere_year_cents: cents(r.taxe_fonciere_year_eur),
            etage: r.etage,
            ascenseur: r.ascenseur,
            jardin: r.jardin,
            garage_parking: r.garage_parking,
            piscine: r.piscine,
            orientation: r.orientation,
            mitoyenne: r.mitoyenne,
            notes: r.notes,
        }
    }
}

pub(crate) fn request_body(
    model: &str,
    input: &ExtractInput<'_>,
    system: &str,
) -> serde_json::Value {
    let listing = serde_json::json!({
        "title": input.title,
        "price": format!("{:.0} EUR", input.price_cents as f64 / 100.0),
        "property_type": input.property_type,
        "surface_m2": input.surface_m2,
        "rooms": input.rooms,
        "description": input.description,
    });
    serde_json::json!({
        "model": model,
        "temperature": 0,
        // explicit budget with room for chain-of-thought: reasoning models
        // think first and the thoughts count against max_tokens; ollama-style
        // backends would otherwise cap at ~128 and truncate the JSON
        "max_tokens": 4000,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": listing.to_string() }
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": { "name": "extraction", "strict": true, "schema": response_schema() }
        }
    })
}

/// The assistant text of a chat-completions response body.
pub(crate) fn content_of(body: &str) -> anyhow::Result<String> {
    let v: serde_json::Value = serde_json::from_str(body)?;
    let choice = &v["choices"][0];
    if let Some(content) = choice["message"]["content"].as_str()
        && !content.trim().is_empty()
    {
        return Ok(content.to_string());
    }
    // llama.cpp reasoning models put thoughts in reasoning_content; when
    // the token budget runs out mid-think, content stays empty
    if let Some(reasoning) = choice["message"]["reasoning_content"].as_str()
        && !reasoning.trim().is_empty()
    {
        let finish = choice["finish_reason"].as_str().unwrap_or("?");
        if finish == "stop" && reasoning.contains('{') {
            return Ok(reasoning.to_string());
        }
        anyhow::bail!(
            "the model spent its whole token budget reasoning without answering \
             (finish_reason={finish}) — thinking should be disabled for this call"
        );
    }
    anyhow::bail!("no choices[0].message.content in llm response")
}

/// Models love wrapping JSON in ```fences``` or prose despite instructions —
/// cut the answer down to its outermost object before parsing.
pub(crate) fn extract_json(content: &str) -> &str {
    let content = match content.rfind("</think>") {
        Some(i) => &content[i + "</think>".len()..],
        None => content,
    };
    match (content.find('{'), content.rfind('}')) {
        (Some(start), Some(end)) if end > start => &content[start..=end],
        _ => content,
    }
}

impl OpenAiExtractor {
    /// Returns the assistant content plus token usage when reported.
    async fn post_chat(
        &self,
        body: &serde_json::Value,
        usage: &mut Option<(i64, i64)>,
    ) -> anyhow::Result<String> {
        let mut request = self.http.post(&self.url).json(body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{status}: {}", text.chars().take(300).collect::<String>().trim());
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && let (Some(p), Some(c)) =
                (v["usage"]["prompt_tokens"].as_i64(), v["usage"]["completion_tokens"].as_i64())
        {
            *usage = Some((p, c));
        }
        content_of(&text)
    }

    /// One structured chat call, resilient to backends that reject OR
    /// silently mangle `response_format`: any failure on the structured
    /// attempt gets one plain retry — the prompt already demands a bare
    /// JSON object. Every call is timed and logged.
    async fn chat_json<T: serde::de::DeserializeOwned>(
        &self,
        kind: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<T> {
        use std::sync::atomic::Ordering;
        self.busy.fetch_add(1, Ordering::SeqCst);
        let start = std::time::Instant::now();
        let mut usage = None;
        let result = self.chat_json_inner(body, &mut usage).await;
        self.busy.fetch_sub(1, Ordering::SeqCst);
        if let Some(db) = &self.db {
            let entry = LlmLogEntry {
                kind: kind.to_string(),
                model: self.model.clone(),
                duration_ms: start.elapsed().as_millis() as i64,
                ok: result.is_ok(),
                error: result.as_ref().err().map(|e| e.to_string()),
                prompt_tokens: usage.map(|(p, _)| p),
                completion_tokens: usage.map(|(_, c)| c),
            };
            let db = db.clone();
            // fire-and-forget: the log must never slow or fail the call
            tokio::spawn(async move {
                if let Err(e) = db.log_llm_request(&entry).await {
                    tracing::debug!(error = %e, "llm request log failed");
                }
            });
        }
        result
    }

    async fn chat_json_inner<T: serde::de::DeserializeOwned>(
        &self,
        mut body: serde_json::Value,
        usage: &mut Option<(i64, i64)>,
    ) -> anyhow::Result<T> {
        fn parse<T: serde::de::DeserializeOwned>(content: &str) -> anyhow::Result<T> {
            Ok(serde_json::from_str(extract_json(content))?)
        }
        let first = match self.post_chat(&body, usage).await {
            Ok(content) => match parse(&content) {
                Ok(v) => return Ok(v),
                Err(e) => anyhow::anyhow!(
                    "{e} (content: {})",
                    content.chars().take(120).collect::<String>()
                ),
            },
            Err(e) => e,
        };
        if body.get("response_format").is_none() {
            return Err(first);
        }
        tracing::debug!(error = %first, "structured attempt failed — retrying plain");
        body.as_object_mut().expect("chat body is an object").remove("response_format");
        let content = self
            .post_chat(&body, usage)
            .await
            .map_err(|e| anyhow::anyhow!("{first}; plain retry: {e}"))?;
        parse(&content).map_err(|e| anyhow::anyhow!("{first}; plain retry: {e}"))
    }
}

#[async_trait::async_trait]
impl LlmExtract for OpenAiExtractor {
    async fn extract(&self, input: &ExtractInput<'_>) -> anyhow::Result<ExtractedAttrs> {
        let raw: RawExtraction = self
            .chat_json("extract", request_body(&self.model, input, &self.prompts.extract))
            .await?;
        Ok(raw.into())
    }
}

// ---- endpoint discovery & probing (settings UI helpers) ----

fn probe_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(concat!("terrier/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("building llm probe client")
}

/// `GET {base_url}/models` — the standard OpenAI-compatible catalog.
pub async fn list_models(base_url: &str, api_key: Option<&str>) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = probe_client(10).get(&url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{status}: {}", text.chars().take(300).collect::<String>().trim());
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let mut models: Vec<String> = v["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no data[] in {url} response"))?
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect();
    models.sort();
    Ok(models)
}

/// One tiny real completion against the endpoint — the settings panel's
/// "Test" button. Errors carry the backend's message verbatim.
pub async fn probe(base_url: &str, model: &str, api_key: Option<&str>) -> anyhow::Result<()> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 2048,
        "messages": [{ "role": "user", "content": "Reply with the single word: ok" }],
    });
    // a reasoning model may think before its one-word answer
    let mut request = probe_client(90).post(&url).json(&body);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{status}: {}", text.chars().take(300).collect::<String>().trim());
    }
    content_of(&text).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ExtractInput<'static> {
        ExtractInput {
            title: "Maison 5 pièces Bruz",
            price_cents: 32_000_000,
            property_type: "maison",
            surface_m2: Some(110.0),
            rooms: Some(5),
            description: "Maison familiale, charges 45 € par mois, taxe foncière 1200 €. \
                          Chauffage gaz. Fibre. Travaux de rafraîchissement à prévoir.",
        }
    }

    fn parse_response(body: &str) -> anyhow::Result<ExtractedAttrs> {
        let content = content_of(body)?;
        let raw: RawExtraction = serde_json::from_str(extract_json(&content))?;
        Ok(raw.into())
    }

    #[test]
    fn parses_extraction_and_converts_euros_to_cents() {
        let body = r#"{"choices": [{ "message": { "role": "assistant", "content":
            "{\"annee_construction\": 1998, \"travaux\": \"rafraichissement\", \"chauffage_type\": \"individuel\", \"chauffage_energie\": \"gaz\", \"fibre\": true, \"charges_copro_month_eur\": 45, \"taxe_fonciere_year_eur\": 1200.5, \"etage\": null, \"ascenseur\": null, \"jardin\": true, \"garage_parking\": null, \"piscine\": null, \"orientation\": null, \"mitoyenne\": null, \"notes\": [\"locataire en place\"]}"
        }}]}"#;
        let a = parse_response(body).unwrap();
        assert_eq!(a.annee_construction, Some(1998));
        assert_eq!(a.charges_copro_month_cents, Some(4500));
        assert_eq!(a.taxe_fonciere_year_cents, Some(120_050));
        assert_eq!(a.travaux.as_deref(), Some("rafraichissement"));
        assert_eq!(a.notes, vec!["locataire en place"]);
    }

    #[test]
    fn extract_json_strips_fences_prose_and_think_blocks() {
        assert_eq!(extract_json("{\"a\": 1}"), "{\"a\": 1}");
        assert_eq!(extract_json("```json\n{\"a\": 1}\n```"), "{\"a\": 1}");
        assert_eq!(
            extract_json("Sure! Here: {\"a\": {\"b\": 2}} hope it helps"),
            "{\"a\": {\"b\": 2}}"
        );
        assert_eq!(extract_json("<think>Hmm {tricky}</think>\n{\"a\": 1}"), "{\"a\": 1}");
        assert_eq!(extract_json("no json at all"), "no json at all");
    }

    #[test]
    fn empty_content_with_reasoning_is_a_clear_error() {
        let body = r#"{"choices": [{"finish_reason": "length", "message":
            {"role": "assistant", "content": "", "reasoning_content": "Let me think..."}}]}"#;
        let err = content_of(body).unwrap_err().to_string();
        assert!(err.contains("token budget reasoning"), "got: {err}");

        let body = r#"{"choices": [{"finish_reason": "stop", "message":
            {"role": "assistant", "content": "", "reasoning_content": "here: {\"a\": 1}"}}]}"#;
        assert!(content_of(body).unwrap().contains("{\"a\": 1}"));
    }

    #[test]
    fn request_carries_listing_strict_schema_and_token_room() {
        let body = request_body("qwen3", &input(), EXTRACT_PROMPT);
        assert_eq!(body["model"], "qwen3");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert!(body["max_tokens"].as_u64().unwrap() >= 4000, "room for CoT");
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("Maison 5 pièces Bruz"));
        assert!(user.contains("taxe foncière 1200"));
        assert!(user.contains("320000 EUR"));
    }

    #[test]
    fn rejects_malformed_responses() {
        assert!(parse_response("not json").is_err());
        assert!(parse_response(r#"{"choices": []}"#).is_err());
        let bad = r#"{"choices": [{"message": {"content": "{\"travaux\": 42}"}}]}"#;
        assert!(parse_response(bad).is_err(), "wrong field type rejected");
    }

    #[test]
    fn disabled_config_builds_no_extractor() {
        let eff = effective(&LlmConfig::default(), None).unwrap();
        let runtime = build_runtime(eff, default_prompts(), None);
        assert!(runtime.extractor.is_none());
        assert!(!runtime.status.enabled && runtime.status.model.is_none());
    }

    #[test]
    fn override_supersedes_config_blank_fields_fall_back() {
        let base = LlmConfig { model: "conf-model".into(), ..Default::default() };
        let o = LlmSettingsUpdate {
            enabled: true,
            base_url: "http://zeus:8080/v1".into(),
            model: String::new(),
            api_key: Some("sk-x".into()),
        };
        let eff = effective(&base, Some(&o)).unwrap();
        assert!(eff.enabled, "override enables a config-disabled pass");
        assert_eq!(eff.base_url, "http://zeus:8080/v1");
        assert_eq!(eff.model, "conf-model", "blank override field falls back to TOML");
        assert_eq!(eff.api_key.as_deref(), Some("sk-x"));

        let runtime = build_runtime(eff, default_prompts(), None);
        assert!(runtime.extractor.is_some());
        assert_eq!(runtime.status.model.as_deref(), Some("conf-model"));
        assert!(runtime.settings.api_key_set && runtime.settings.from_override);
    }

    #[test]
    fn prompt_override_merges_over_default() {
        let stored = LlmPrompts { extract: "  ".into() };
        assert_eq!(effective_prompts(Some(&stored)).extract, EXTRACT_PROMPT);
        let stored = LlmPrompts { extract: "custom".into() };
        assert_eq!(effective_prompts(Some(&stored)).extract, "custom");
    }
}
```

Note: `terrier_domain::LlmSettingsUpdate` replaces ferret's local `LlmOverride` — it is stored as the `llm` settings JSON.

- [ ] **Step 3: Run tests** — `nix develop -c cargo test -p terrier-server llm` → PASS (and workspace compiles).
- [ ] **Step 4: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "llm: openai-compatible extraction module (ferret port)"`

---

### Task 8: Enrichment worker

**Files:**
- Create: `crates/terrier-server/src/enrich.rs`
- Modify: `crates/terrier-server/src/main.rs` (add `mod enrich;`)

- [ ] **Step 1: Write the module with tests:**

```rust
//! The enrichment worker: drains the per-source queue — detail page →
//! image downloads → LLM extraction — each step independent and fail-open,
//! with the queue's backoff handling retries. One worker task per source,
//! sharing the source's politeness budget via its `fetch_detail`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::config::EnrichmentConfig;
use crate::db::Db;
use crate::llm::{ExtractInput, LlmHandle};
use crate::scrape::ImmoSource;

/// Image downloads mocked in tests; the real one is a browser-UA client.
#[async_trait::async_trait]
pub trait ImageFetch: Send + Sync {
    async fn fetch(&self, url: &str) -> anyhow::Result<Vec<u8>>;
}

pub struct HttpImageFetch {
    client: reqwest::Client,
}

impl HttpImageFetch {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
                )
                .build()
                .expect("building image http client"),
        }
    }
}

#[async_trait::async_trait]
impl ImageFetch for HttpImageFetch {
    async fn fetch(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let response = self.client.get(url).send().await?.error_for_status()?;
        Ok(response.bytes().await?.to_vec())
    }
}

/// ".jpg" from a CDN url, default jpg; query strings stripped.
fn extension_of(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('.').next() {
        Some(ext) if ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) => ext,
        _ => "jpg",
    }
}

/// One queue item, end to end. Any error propagates so the caller records
/// the failure with backoff; every step is idempotent for the retry.
pub async fn process_one(
    db: &Db,
    source: &dyn ImmoSource,
    images: &dyn ImageFetch,
    llm: &LlmHandle,
    config: &EnrichmentConfig,
    id: Uuid,
) -> anyhow::Result<()> {
    let state = db.enrichment_state(id).await?;

    // 1. detail page (skipped once enriched; price-change cleared the mark)
    if state.enriched_at.is_none() {
        let detail = source
            .fetch_detail(&state.listing.canonical_url)
            .await?
            .unwrap_or_default();
        // an empty detail still marks the listing enriched — sources
        // without detail support are done after images + extraction
        db.set_detail(id, &detail).await?;
    }

    // 2. images: fetch once each, capped
    for (position, url) in db.pending_images(id, config.max_images as i64).await? {
        let bytes = images.fetch(&url).await?;
        let dir = config.images_dir.join(id.to_string());
        std::fs::create_dir_all(&dir)?;
        let file = format!("{position}.{}", extension_of(&url));
        std::fs::write(dir.join(&file), &bytes)?;
        db.mark_image_saved(id, position, &format!("{id}/{file}")).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 3. LLM extraction over whatever description we have
    let state = db.enrichment_state(id).await?;
    if state.extracted_at.is_none()
        && let Some(description) = &state.listing.description
    {
        let extractor = llm.read().await.extractor.clone();
        if let Some(extractor) = extractor {
            let attrs = extractor
                .extract(&ExtractInput {
                    title: &state.listing.title,
                    price_cents: state.listing.price_cents,
                    property_type: state.listing.property_type.label(),
                    surface_m2: state.listing.surface_m2,
                    rooms: state.listing.rooms,
                    description,
                })
                .await?;
            db.set_attributes(id, &attrs).await?;
        }
        // llm disabled: fine — the listing is done without attributes
    }

    db.enrichment_done(id).await
}

/// The forever loop for one source.
pub async fn run_source_enricher(
    source: Arc<dyn ImmoSource>,
    db: Db,
    config: EnrichmentConfig,
    llm: LlmHandle,
) {
    let images = HttpImageFetch::new();
    loop {
        match db.due_enrichment(source.id(), 5).await {
            Ok(due) => {
                for id in due {
                    if let Err(e) =
                        process_one(&db, source.as_ref(), &images, &llm, &config, id).await
                    {
                        tracing::warn!(source = source.id(), %id, error = %e, "enrichment failed");
                        match db.enrichment_failed(id, &e.to_string(), config.max_attempts).await {
                            Ok(true) => {
                                tracing::warn!(%id, "enrichment given up after max attempts")
                            }
                            Ok(false) => {}
                            Err(e) => tracing::error!(error = %e, "recording enrichment failure"),
                        }
                    }
                }
            }
            Err(e) => tracing::error!(source = source.id(), error = %e, "reading enrichment queue"),
        }
        tokio::time::sleep(Duration::from_secs(config.poll_seconds)).await;
    }
}

/// Resolve where a saved image lives on disk (also used by main to serve).
pub fn images_root(config: &EnrichmentConfig) -> PathBuf {
    Path::new(&config.images_dir).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as FsPath;
    use std::sync::Mutex;

    use terrier_domain::{ExtractedAttrs, ListingDetail, RawListing};

    struct FakeSource {
        detail: Option<ListingDetail>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl ImmoSource for FakeSource {
        fn id(&self) -> &str {
            "src"
        }
        async fn fetch(&self) -> anyhow::Result<Vec<RawListing>> {
            Ok(vec![])
        }
        async fn fetch_detail(&self, _url: &str) -> anyhow::Result<Option<ListingDetail>> {
            if self.fail {
                anyhow::bail!("blocked");
            }
            Ok(self.detail.clone())
        }
    }

    struct FakeImages;
    #[async_trait::async_trait]
    impl ImageFetch for FakeImages {
        async fn fetch(&self, _url: &str) -> anyhow::Result<Vec<u8>> {
            Ok(vec![0xFF, 0xD8, 0xFF])
        }
    }

    struct FakeLlm {
        calls: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl crate::llm::LlmExtract for FakeLlm {
        async fn extract(&self, input: &ExtractInput<'_>) -> anyhow::Result<ExtractedAttrs> {
            *self.calls.lock().unwrap() += 1;
            assert!(input.description.contains("full description"));
            Ok(ExtractedAttrs { fibre: Some(true), ..Default::default() })
        }
    }

    async fn setup(detail: Option<ListingDetail>) -> (Db, Uuid) {
        let db = Db::connect(FsPath::new(":memory:")).await.unwrap();
        let (l, _) = db
            .upsert_listing(&crate::db::tests_listing_helper("https://x/1", 30_000_000))
            .await
            .unwrap();
        db.enqueue_enrichment(l.id, "new").await.unwrap();
        let _ = detail;
        (db, l.id)
    }

    fn llm_handle(extractor: Option<Arc<dyn crate::llm::LlmExtract>>) -> LlmHandle {
        Arc::new(tokio::sync::RwLock::new(crate::llm::LlmRuntime {
            extractor,
            ..Default::default()
        }))
    }

    fn config(dir: &FsPath) -> EnrichmentConfig {
        EnrichmentConfig { images_dir: dir.to_path_buf(), ..Default::default() }
    }

    #[tokio::test]
    async fn full_enrichment_detail_images_extraction() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let detail = ListingDetail {
            description: Some("the full description".into()),
            image_urls: vec!["https://cdn/a.jpg".into(), "https://cdn/b.webp?rule=x".into()],
            ..Default::default()
        };
        let (db, id) = setup(None).await;
        let source = FakeSource { detail: Some(detail), fail: false };
        let llm_impl = Arc::new(FakeLlm { calls: Mutex::new(0) });
        let llm = llm_handle(Some(llm_impl.clone()));

        process_one(&db, &source, &FakeImages, &llm, &config(&tmp), id).await.unwrap();

        let st = db.enrichment_state(id).await.unwrap();
        assert!(st.enriched_at.is_some() && st.extracted_at.is_some());
        assert_eq!(st.listing.description.as_deref(), Some("the full description"));
        assert_eq!(st.listing.attributes.fibre, Some(true));
        assert_eq!(*llm_impl.calls.lock().unwrap(), 1);
        assert!(db.pending_images(id, 10).await.unwrap().is_empty());
        assert!(tmp.join(id.to_string()).join("0.jpg").exists());
        assert!(tmp.join(id.to_string()).join("1.webp").exists(), "query string stripped");
        assert_eq!(db.enrichment_depth().await.unwrap(), 0, "dequeued");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn no_detail_support_and_no_llm_still_completes() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let (db, id) = setup(None).await;
        let source = FakeSource { detail: None, fail: false };
        process_one(&db, &source, &FakeImages, &llm_handle(None), &config(&tmp), id)
            .await
            .unwrap();
        let st = db.enrichment_state(id).await.unwrap();
        assert!(st.enriched_at.is_some(), "marked enriched even without detail");
        assert!(st.extracted_at.is_none(), "no llm: no extraction claim");
        assert_eq!(db.enrichment_depth().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn detail_failure_propagates_for_retry() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let (db, id) = setup(None).await;
        let source = FakeSource { detail: None, fail: true };
        let err = process_one(&db, &source, &FakeImages, &llm_handle(None), &config(&tmp), id)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked"));
        assert_eq!(db.enrichment_depth().await.unwrap(), 1, "stays queued for retry");
    }

    #[test]
    fn extension_default_and_stripping() {
        assert_eq!(extension_of("https://cdn/a.jpg"), "jpg");
        assert_eq!(extension_of("https://cdn/a.webp?rule=classified"), "webp");
        assert_eq!(extension_of("https://cdn/no-extension"), "jpg");
        assert_eq!(extension_of("https://cdn/x.superlongext"), "jpg");
    }
}
```

Note: `PropertyType::label()` exists and returns the French label — used as `property_type` for the LLM.

- [ ] **Step 2: Add `mod enrich;` to `main.rs`** (module list).
- [ ] **Step 3: Run tests** — `nix develop -c cargo test -p terrier-server enrich` → PASS.
- [ ] **Step 4: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "enrich: per-source queue worker (detail, images, extraction)"`

---

### Task 9: Wiring — main.rs, AppState, status, /images static serving

**Files:**
- Modify: `crates/terrier-server/src/main.rs`
- Modify: `crates/terrier-server/src/state.rs`
- Modify: `crates/terrier-server/src/api.rs` (status handler + AppState construction in tests)

- [ ] **Step 1: Extend `AppState`** in state.rs:

```rust
    pub llm: crate::llm::LlmHandle,
    /// TOML base for the [llm] section (settings PUT merges over it).
    pub llm_base: crate::config::LlmConfig,
```

(add `use` as needed). Update both test constructions in api.rs — add:

```rust
            llm: Default::default(),
            llm_base: Default::default(),
```

(`LlmHandle` is `Arc<RwLock<LlmRuntime>>`, both Default; `LlmConfig` derives no Default automatically — it has a manual impl, fine.)

- [ ] **Step 2: main.rs wiring.** After `db` is connected and before sources are built:

```rust
    let llm_override = llm::load_override(&db).await;
    let prompts = llm::effective_prompts(llm::load_prompts(&db).await.as_ref());
    let llm_eff = llm::effective(&config.llm, llm_override.as_ref())
        .context("resolving llm configuration")?;
    if llm_eff.enabled {
        tracing::info!(model = %llm_eff.model, url = %llm_eff.base_url, "llm extraction enabled");
    }
    let llm_handle: llm::LlmHandle = Arc::new(tokio::sync::RwLock::new(llm::build_runtime(
        llm_eff,
        prompts,
        Some(db.clone()),
    )));
```

After `scheduler::spawn_all(...)` add the enrichers (note: `spawn_all` takes `sources` by value — clone the Arc list first):

```rust
    // keep a handle on each source for its enrichment worker
    let enrich_sources: Vec<Arc<dyn ImmoSource>> =
        sources.iter().map(|(s, _)| s.clone()).collect();
```

(place this line BEFORE the `scheduler::spawn_all(sources, ...)` call), then after it:

```rust
    for source in enrich_sources {
        tokio::spawn(enrich::run_source_enricher(
            source,
            db.clone(),
            config.enrichment.clone(),
            llm_handle.clone(),
        ));
    }
```

AppState construction gains `llm: llm_handle.clone(), llm_base: config.llm.clone(),`. After the router is built, before `static_dir` handling, serve images:

```rust
    std::fs::create_dir_all(&config.enrichment.images_dir).ok();
    app = app.nest_service(
        "/images",
        tower_http::services::ServeDir::new(&config.enrichment.images_dir),
    );
```

(`let mut app = ...` already exists.) Add `mod llm;` / `mod enrich;` to the module list if not already done.

- [ ] **Step 3: Status handler.** In api.rs `status()`:

```rust
async fn status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let mut sources: Vec<_> = state.statuses.read().await.values().cloned().collect();
    sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    let search_matches = state.db.count_matches().await?;
    let enrichment_pending = state.db.enrichment_depth().await?;
    let llm = {
        let runtime = state.llm.read().await;
        let mut status = runtime.status.clone();
        status.busy = runtime.busy.load(std::sync::atomic::Ordering::SeqCst);
        Some(status)
    };
    Ok(Json(terrier_domain::StatusResponse {
        sources,
        search_matches,
        enrichment_pending,
        llm,
    })
    .into_response())
}
```

- [ ] **Step 4: Build + tests** — `nix develop -c cargo test --workspace` → PASS.
- [ ] **Step 5: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "server: wire llm runtime, enrichment workers, /images"`

---

### Task 10: API endpoints (listings images, LLM settings/probe/models/prompts) + client

**Files:**
- Modify: `crates/terrier-server/src/api.rs`
- Modify: `crates/terrier-client/src/lib.rs`

- [ ] **Step 1: Write failing API test** (api.rs tests):

```rust
    #[tokio::test]
    async fn listings_carry_images_with_local_urls() {
        let db = Db::connect(FsPath::new(":memory:")).await.unwrap();
        let l = crate::db::tests_listing_helper("https://x/1", 30_000_000);
        let (stored, _) = db.upsert_listing(&l).await.unwrap();
        db.add_image_urls(stored.id, &["https://cdn/a.jpg".into(), "https://cdn/b.jpg".into()])
            .await
            .unwrap();
        db.mark_image_saved(stored.id, 0, &format!("{}/0.jpg", stored.id)).await.unwrap();
        let app = router(AppState {
            db,
            notifier: Arc::new(crate::notify::NoopNotifier),
            statuses: Arc::new(tokio::sync::RwLock::new(Default::default())),
            shared_locations: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            location_cap: 20,
            llm: Default::default(),
            llm_base: Default::default(),
        });
        let resp = app
            .oneshot(Request::get("/api/listings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let listings: Vec<ListingWithHistory> = body_json(resp).await;
        assert_eq!(listings[0].images.len(), 2);
        assert_eq!(listings[0].images[0].url, format!("/images/{}/0.jpg", stored.id));
        assert_eq!(listings[0].images[1].url, "https://cdn/b.jpg", "not yet local: CDN url");
    }

    #[tokio::test]
    async fn llm_settings_roundtrip() {
        let app = app().await;
        let resp = app
            .clone()
            .oneshot(Request::get("/api/settings/llm").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let s: terrier_domain::LlmSettings = body_json(resp).await;
        assert!(!s.enabled);

        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/settings/llm")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"enabled": true, "base_url": "http://zeus:8080/v1",
                            "model": "qwen3", "api_key": null}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let s: terrier_domain::LlmSettings = body_json(resp).await;
        assert!(s.enabled && s.from_override);
        assert_eq!(s.model, "qwen3");
    }
```

Run: `nix develop -c cargo test -p terrier-server api::` → FAIL.

- [ ] **Step 2: Implement listings images.** In `list_listings`:

```rust
async fn list_listings(
    State(state): State<AppState>,
    Query(q): Query<ListingsQuery>,
) -> Result<Response, ApiError> {
    let listings = state.db.list_listings(q.search_id, q.hidden).await?;
    let ids: Vec<Uuid> = listings.iter().map(|l| l.id).collect();
    let mut histories = state.db.prices_for(&ids).await?;
    let mut images = state.db.images_for(&ids).await?;
    let out: Vec<ListingWithHistory> = listings
        .into_iter()
        .map(|listing| ListingWithHistory {
            history: histories.remove(&listing.id).unwrap_or_default(),
            images: images
                .remove(&listing.id)
                .unwrap_or_default()
                .into_iter()
                .map(|i| terrier_domain::ListingImage {
                    position: i.position,
                    url: match i.local_path {
                        Some(p) => format!("/images/{p}"),
                        None => i.url,
                    },
                })
                .collect(),
            listing,
        })
        .collect();
    Ok(Json(out).into_response())
}
```

- [ ] **Step 3: Implement the LLM endpoints.** Routes:

```rust
        .route("/api/settings/llm", get(get_llm_settings).put(put_llm_settings))
        .route("/api/settings/prompts", get(get_prompts).put(put_prompts))
        .route("/api/llm/models", get(llm_models))
        .route("/api/llm/probe", axum::routing::post(llm_probe))
```

Handlers:

```rust
async fn get_llm_settings(State(state): State<AppState>) -> Response {
    Json(state.llm.read().await.settings.clone()).into_response()
}

async fn put_llm_settings(
    State(state): State<AppState>,
    Json(mut update): Json<terrier_domain::LlmSettingsUpdate>,
) -> Result<Response, ApiError> {
    // api_key None = keep the previously stored key
    if update.api_key.is_none()
        && let Some(prev) = crate::llm::load_override(&state.db).await
    {
        update.api_key = prev.api_key;
    }
    state
        .db
        .put_setting(
            crate::llm::LLM_SETTINGS_KEY,
            &serde_json::to_string(&update).expect("settings serialize"),
        )
        .await?;
    let eff = crate::llm::effective(&state.llm_base, Some(&update))
        .map_err(|e| ApiError(DbError::Corrupt(e.to_string())))?;
    let prompts = state.llm.read().await.prompts.clone();
    let runtime = crate::llm::build_runtime(eff, prompts, Some(state.db.clone()));
    let settings = runtime.settings.clone();
    *state.llm.write().await = runtime;
    Ok(Json(settings).into_response())
}

async fn get_prompts(State(state): State<AppState>) -> Response {
    Json(state.llm.read().await.prompts.clone()).into_response()
}

async fn put_prompts(
    State(state): State<AppState>,
    Json(prompts): Json<terrier_domain::LlmPrompts>,
) -> Result<Response, ApiError> {
    state
        .db
        .put_setting(
            crate::llm::PROMPTS_SETTINGS_KEY,
            &serde_json::to_string(&prompts).expect("prompts serialize"),
        )
        .await?;
    let merged = crate::llm::effective_prompts(Some(&prompts));
    let (eff_settings, override_) =
        (state.llm.read().await.settings.clone(), crate::llm::load_override(&state.db).await);
    let _ = eff_settings;
    let eff = crate::llm::effective(&state.llm_base, override_.as_ref())
        .map_err(|e| ApiError(DbError::Corrupt(e.to_string())))?;
    let runtime = crate::llm::build_runtime(eff, merged.clone(), Some(state.db.clone()));
    *state.llm.write().await = runtime;
    Ok(Json(merged).into_response())
}

#[derive(Deserialize)]
struct LlmProbeRequest {
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key: Option<String>,
}

async fn llm_models(Query(q): Query<LlmProbeRequest>) -> Response {
    match crate::llm::list_models(&q.base_url, q.api_key.as_deref()).await {
        Ok(models) => Json(models).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

async fn llm_probe(Json(q): Json<LlmProbeRequest>) -> Response {
    match crate::llm::probe(&q.base_url, &q.model, q.api_key.as_deref()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}
```

- [ ] **Step 4: Client methods** (terrier-client/src/lib.rs; extend the domain import with `LlmPrompts, LlmSettings, LlmSettingsUpdate`):

```rust
    pub async fn llm_settings(&self) -> Result<LlmSettings> {
        self.send(self.http.get(self.url("api/settings/llm")?), DATA_TIMEOUT).await
    }

    pub async fn update_llm_settings(&self, update: &LlmSettingsUpdate) -> Result<LlmSettings> {
        self.send(self.http.put(self.url("api/settings/llm")?).json(update), DATA_TIMEOUT)
            .await
    }

    pub async fn llm_prompts(&self) -> Result<LlmPrompts> {
        self.send(self.http.get(self.url("api/settings/prompts")?), DATA_TIMEOUT).await
    }

    pub async fn update_llm_prompts(&self, prompts: &LlmPrompts) -> Result<LlmPrompts> {
        self.send(self.http.put(self.url("api/settings/prompts")?).json(prompts), DATA_TIMEOUT)
            .await
    }

    pub async fn llm_models(&self, base_url: &str) -> Result<Vec<String>> {
        let path = format!(
            "api/llm/models?base_url={}",
            url::form_urlencoded::byte_serialize(base_url.as_bytes()).collect::<String>()
        );
        self.send(self.http.get(self.url(&path)?), DATA_TIMEOUT).await
    }

    /// The settings panel's "Test": one tiny completion (slow local models).
    pub async fn llm_probe(&self, update: &LlmSettingsUpdate) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            base_url: &'a str,
            model: &'a str,
            api_key: &'a Option<String>,
        }
        let mut request = self
            .http
            .post(self.url("api/llm/probe")?)
            .json(&Body {
                base_url: &update.base_url,
                model: &update.model,
                api_key: &update.api_key,
            })
            .build()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        *request.timeout_mut() = Some(Duration::from_secs(120));
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(ClientError::Api { status: status.as_u16(), message });
        }
        Ok(())
    }
```

- [ ] **Step 5: Run tests** — `nix develop -c cargo test --workspace` → PASS.
- [ ] **Step 6: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "api: listing images inline, llm settings/probe/models endpoints"`

---

### Task 11: UI — cards (thumbnail, seller, attributes, gallery) and Settings tab

**Files:**
- Modify: `crates/terrier-ui/src/listings.rs`
- Create: `crates/terrier-ui/src/settings.rs`
- Modify: `crates/terrier-ui/src/lib.rs`
- Modify: `crates/terrier-web/styles.css`

UI has no test harness beyond `cargo check`/unit tests — verify with `nix develop -c cargo check -p terrier-ui` after each step, and unit-test the pure chip builder.

- [ ] **Step 1: Attribute chips helper + test** in listings.rs:

```rust
/// French chips for extracted attributes, in card order.
fn attr_chips(a: &terrier_domain::ExtractedAttrs) -> Vec<String> {
    let mut chips = Vec::new();
    if let Some(y) = a.annee_construction {
        chips.push(format!("constr. {y}"));
    }
    match a.travaux.as_deref() {
        Some("a-prevoir") => chips.push("travaux à prévoir".into()),
        Some("rafraichissement") => chips.push("rafraîchissement".into()),
        Some("aucun") => chips.push("sans travaux".into()),
        _ => {}
    }
    match (&a.chauffage_type, &a.chauffage_energie) {
        (Some(t), Some(e)) => chips.push(format!("chauffage {t} {e}")),
        (Some(t), None) => chips.push(format!("chauffage {t}")),
        (None, Some(e)) => chips.push(format!("chauffage {e}")),
        (None, None) => {}
    }
    if a.fibre == Some(true) {
        chips.push("fibre".into());
    }
    if let Some(c) = a.charges_copro_month_cents {
        chips.push(format!("copro {} €/mois", c / 100));
    }
    if let Some(t) = a.taxe_fonciere_year_cents {
        chips.push(format!("TF {} €/an", t / 100));
    }
    if let Some(e) = a.etage {
        chips.push(if a.ascenseur == Some(true) {
            format!("ét. {e} asc.")
        } else {
            format!("ét. {e}")
        });
    } else if a.ascenseur == Some(true) {
        chips.push("ascenseur".into());
    }
    if a.jardin == Some(true) {
        chips.push("jardin".into());
    }
    if a.garage_parking == Some(true) {
        chips.push("garage/parking".into());
    }
    if a.piscine == Some(true) {
        chips.push("piscine".into());
    }
    if let Some(o) = &a.orientation {
        chips.push(format!("expo {o}"));
    }
    if a.mitoyenne == Some(true) {
        chips.push("mitoyenne".into());
    }
    chips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_chips_render_french_labels() {
        let a = terrier_domain::ExtractedAttrs {
            annee_construction: Some(1998),
            travaux: Some("a-prevoir".into()),
            charges_copro_month_cents: Some(18_000),
            etage: Some(3),
            ascenseur: Some(true),
            fibre: Some(true),
            ..Default::default()
        };
        let chips = attr_chips(&a);
        assert!(chips.contains(&"constr. 1998".to_string()));
        assert!(chips.contains(&"travaux à prévoir".to_string()));
        assert!(chips.contains(&"copro 180 €/mois".to_string()));
        assert!(chips.contains(&"ét. 3 asc.".to_string()));
        assert!(attr_chips(&Default::default()).is_empty());
    }
}
```

Run `nix develop -c cargo test -p terrier-ui` → PASS once the helper compiles.

- [ ] **Step 2: Extend `ListingCard`.** Changes inside the component (keep the rest intact):

At the top after `let listing_id = listing.id;`:

```rust
    let images = item.images;
    let client: TerrierClient = expect_context();
    let expanded = RwSignal::new(false);
    // relative /images/... urls need the API host when the UI is served
    // from another origin (dev, Trunk)
    let base = client.base().clone();
    let img_src = move |url: &str| -> String {
        if url.starts_with('/') {
            base.join(url).map(|u| u.to_string()).unwrap_or_else(|_| url.to_string())
        } else {
            url.to_string()
        }
    };
    let cover = images.first().map(|i| img_src(&i.url));
    let gallery: Vec<String> = images.iter().map(|i| img_src(&i.url)).collect();
    let description = listing.description.clone();
    let notes = listing.attributes.notes.clone();
    let seller_badge = listing.seller.as_ref().map(|s| {
        let kind = match s.kind {
            terrier_domain::SellerKind::Pro => "pro",
            terrier_domain::SellerKind::Private => "particulier",
        };
        match &s.name {
            Some(name) => format!("{kind} · {name}"),
            None => kind.to_string(),
        }
    });
```

Add attribute chips to the existing chips: after the `chips.push(listing.source_id.clone());` line insert nothing — attribute chips render as badges instead. In the `view!`, restructure the card body: wrap the existing content in a flex row with the thumbnail:

```rust
    view! {
        <li class="deal" class:gone=gone>
            <div class="deal-row">
                {cover.map(|src| view! {
                    <img class="thumb" src=src loading="lazy"
                        on:click=move |_| expanded.update(|e| *e = !*e)/>
                })}
                <div class="deal-body">
                    <div class="deal-main">
                        <a href=listing.canonical_url.clone() target="_blank" rel="noreferrer">
                            {listing.title.clone()}
                        </a>
                        <span class="price-block">
                            <span class="price">{format_price(listing.price_cents)}</span>
                            {listing.price_per_m2_cents().map(|m2| view! {
                                <span class="badge m2">{format!("{} €/m²", m2 / 100)}</span>
                            })}
                        </span>
                    </div>
                    <div class="deal-meta">
                        <span class="muted">{chips.join(" · ")}</span>
                        {seller_badge.map(|s| view! { <span class="badge seller">{s}</span> })}
                        {listing.dpe.clone().map(|d| view! {
                            <span class=format!("badge dpe dpe-{d}")>{format!("DPE {}", d.to_uppercase())}</span>
                        })}
                        {delta.map(|d| view! {
                            <span class=if d < 0.0 { "badge ok" } else { "badge warn" }>
                                {format!("{d:+.1}% depuis publication")}
                            </span>
                        })}
                        {listing.flags.contains(&Flag::WantedAd).then(|| view! {
                            <span class="badge muted">"recherche (pas une offre)"</span>
                        })}
                        {(listing.moderation == Moderation::Dismissed).then(|| view! {
                            <span class="badge muted">"masquée"</span>
                        })}
                        {(listing.moderation == Moderation::Banned).then(|| view! {
                            <span class="badge bad">"bannie"</span>
                        })}
                        {gone.then(|| view! { <span class="badge muted">"disparue"</span> })}
                    </div>
                    <div class="deal-meta">
                        <span class="muted attrs">{attr_chips(&listing.attributes).join(" · ")}</span>
                    </div>
                </div>
            </div>
            {(history.len() > 1).then(|| view! {
                <crate::sparkline::Sparkline prices=history.clone() currency="EUR".to_string()/>
            })}
            {(description.is_some() || gallery.len() > 1).then(|| view! {
                <button class="expand" on:click=move |_| expanded.update(|e| *e = !*e)>
                    {move || if expanded.get() { "réduire" } else { "détails" }}
                </button>
            })}
            {move || expanded.get().then(|| view! {
                <div class="deal-detail">
                    {description.clone().map(|d| view! { <p class="description">{d}</p> })}
                    {(!notes.is_empty()).then(|| view! {
                        <p class="muted">{format!("À noter : {}", notes.join(" · "))}</p>
                    })}
                    <div class="gallery">
                        {gallery.iter().map(|src| view! {
                            <img src=src.clone() loading="lazy"/>
                        }).collect_view()}
                    </div>
                </div>
            })}
            <ModerationButtons listing_id=listing_id current=listing.moderation/>
        </li>
    }
```

(Closures capturing `expanded`/`description`/`gallery`/`notes` in `move ||` positions: clone what the borrow checker demands — `description`/`notes`/`gallery` are used both in the `.then()` gate and inside; if the compiler complains, clone into shadowed variables before the `view!`.)

- [ ] **Step 3: Settings tab.** Create `crates/terrier-ui/src/settings.rs`:

```rust
//! Settings: LLM endpoint (OpenAI-compatible, llama.cpp on zeus) and the
//! extraction prompt — ferret's panel, trimmed to the one extract pass.

use leptos::prelude::*;
use leptos::task::spawn_local;
use terrier_client::TerrierClient;
use terrier_domain::{LlmPrompts, LlmSettingsUpdate};

#[component]
pub fn SettingsView() -> impl IntoView {
    let client: TerrierClient = expect_context();

    let enabled = RwSignal::new(false);
    let base_url = RwSignal::new(String::new());
    let model = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());
    let models = RwSignal::new(Vec::<String>::new());
    let feedback = RwSignal::new(String::new());
    let prompt = RwSignal::new(String::new());

    // initial load
    {
        let client = client.clone();
        spawn_local(async move {
            if let Ok(s) = client.llm_settings().await {
                enabled.set(s.enabled);
                base_url.set(s.base_url);
                model.set(s.model);
            }
            if let Ok(p) = client.llm_prompts().await {
                prompt.set(p.extract);
            }
        });
    }

    let update = move || LlmSettingsUpdate {
        enabled: enabled.get_untracked(),
        base_url: base_url.get_untracked(),
        model: model.get_untracked(),
        api_key: {
            let k = api_key.get_untracked();
            (!k.trim().is_empty()).then(|| k.trim().to_string())
        },
    };

    let load_models = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let url = base_url.get_untracked();
            spawn_local(async move {
                match client.llm_models(&url).await {
                    Ok(m) => {
                        feedback.set(format!("{} modèle(s)", m.len()));
                        models.set(m);
                    }
                    Err(e) => feedback.set(format!("modèles : {e}")),
                }
            });
        }
    };

    let probe = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let u = update();
            feedback.set("test en cours…".into());
            spawn_local(async move {
                match client.llm_probe(&u).await {
                    Ok(()) => feedback.set("✓ le modèle répond".into()),
                    Err(e) => feedback.set(format!("échec : {e}")),
                }
            });
        }
    };

    let save = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let u = update();
            spawn_local(async move {
                match client.update_llm_settings(&u).await {
                    Ok(s) => {
                        feedback.set("enregistré".into());
                        enabled.set(s.enabled);
                        base_url.set(s.base_url);
                        model.set(s.model);
                        api_key.set(String::new());
                    }
                    Err(e) => feedback.set(format!("échec : {e}")),
                }
            });
        }
    };

    let save_prompt = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let p = LlmPrompts { extract: prompt.get_untracked() };
            spawn_local(async move {
                match client.update_llm_prompts(&p).await {
                    Ok(p) => {
                        feedback.set("prompt enregistré".into());
                        prompt.set(p.extract);
                    }
                    Err(e) => feedback.set(format!("échec : {e}")),
                }
            });
        }
    };

    view! {
        <section class="settings">
            <div class="settings-block">
                <span class="settings-title">"Extraction LLM (serveur)"</span>
                <label class="spec">
                    <input type="checkbox" prop:checked=enabled
                        on:change=move |ev| enabled.set(event_target_checked(&ev))/>
                    "activer l'extraction des descriptions"
                </label>
                <input prop:value=base_url placeholder="http://127.0.0.1:8080/v1"
                    on:input=move |ev| base_url.set(event_target_value(&ev))/>
                <div class="row">
                    <input prop:value=model placeholder="modèle" list="llm-models"
                        on:input=move |ev| model.set(event_target_value(&ev))/>
                    <datalist id="llm-models">
                        {move || models.get().into_iter()
                            .map(|m| view! { <option value=m/> })
                            .collect_view()}
                    </datalist>
                    <button on:click=load_models>"lister"</button>
                </div>
                <input prop:value=api_key type="password"
                    placeholder="clé API (vide = inchangée)"
                    on:input=move |ev| api_key.set(event_target_value(&ev))/>
                <div class="row">
                    <button on:click=probe>"Tester"</button>
                    <button class="primary" on:click=save>"Enregistrer"</button>
                </div>
                <span class="muted">{move || feedback.get()}</span>
            </div>
            <div class="settings-block">
                <span class="settings-title">"Prompt d'extraction (vide = défaut)"</span>
                <textarea prop:value=prompt rows="10"
                    on:input=move |ev| prompt.set(event_target_value(&ev))/>
                <button on:click=save_prompt>"Enregistrer le prompt"</button>
            </div>
        </section>
    }
}
```

- [ ] **Step 4: Register the tab** in lib.rs: `mod settings;`, add `Settings` to the `Tab` enum, a `{tab_button(Tab::Settings, "Réglages")}` in the nav, and the corresponding `<div style:display=...><settings::SettingsView/></div>` in `<main>`.

- [ ] **Step 5: CSS.** Append to `crates/terrier-web/styles.css` (match the file's existing variables/conventions — read it first):

```css
/* --- enrichment: thumbnails, gallery, attribute chips, settings --- */
.deal-row { display: flex; gap: 0.6rem; min-width: 0; }
.deal-body { flex: 1; min-width: 0; }
.thumb {
  width: 74px; height: 74px; object-fit: cover; border-radius: 6px;
  flex-shrink: 0; cursor: pointer; background: rgba(127, 127, 127, 0.15);
}
.badge.seller { opacity: 0.85; }
.attrs { font-size: 0.85em; }
button.expand { font-size: 0.8em; padding: 0.15rem 0.5rem; }
.deal-detail .description { white-space: pre-line; font-size: 0.9em; }
.gallery { display: flex; gap: 0.4rem; overflow-x: auto; padding-bottom: 0.3rem; }
.gallery img { height: 110px; border-radius: 6px; flex-shrink: 0; }
.settings textarea { width: 100%; font-family: monospace; font-size: 0.85em; }
.settings .row { display: flex; gap: 0.4rem; flex-wrap: wrap; }
```

- [ ] **Step 6: Check + tests**

Run: `nix develop -c cargo check -p terrier-ui -p terrier-web && nix develop -c cargo test --workspace`
Expected: PASS. (Leptos view-macro borrow errors are the likely friction — fix by cloning captured values into shadowed locals before `view!`.)

- [ ] **Step 7: Commit** — `git add -A && git -c commit.gpgsign=false commit -m "ui: thumbnails, seller, attribute chips, gallery, llm settings tab"`

---

### Task 12: Status strip, docs, example config, final verification

**Files:**
- Modify: `crates/terrier-ui/src/status.rs`
- Modify: `crates/terrier-server/terrier.example.toml`
- Modify: `docs/zeus-config-example.nix`
- Modify: `README.md`

- [ ] **Step 1: Status strip.** Read `crates/terrier-ui/src/status.rs`; wherever the `SourcesStrip` renders per-source chips, append after them (the status resource already holds a `StatusResponse`):

```rust
    // after the per-source chips, from the same StatusResponse `s`:
    {(s.enrichment_pending > 0).then(|| view! {
        <span class="badge muted">{format!("enrichissement : {}", s.enrichment_pending)}</span>
    })}
    {s.llm.as_ref().filter(|l| l.enabled).map(|l| view! {
        <span class="badge muted">
            {if l.busy > 0 { format!("LLM ⚙ {}", l.model.clone().unwrap_or_default()) }
             else { format!("LLM {}", l.model.clone().unwrap_or_default()) }}
        </span>
    })}
```

Adapt names to the actual local variables in that file; `cargo check -p terrier-ui` must pass.

- [ ] **Step 2: Example config.** Append to `crates/terrier-server/terrier.example.toml`:

```toml
[enrichment]
# queue poll cadence per source; detail pages go through the same
# politeness budget as scraping
poll_seconds = 60
max_attempts = 8
max_images = 10
images_dir = "images"

[llm]
# structured extraction from descriptions via any OpenAI-compatible
# endpoint (llama.cpp). Fail-open: scraping never depends on it.
enabled = false
base_url = "http://127.0.0.1:8080/v1"
model = ""
# api_key_file = "/run/secrets/llm-key"
timeout_secs = 120
```

Mirror the same two sections in `docs/zeus-config-example.nix` (follow that file's existing attribute-set style, with the zeus llama.cpp URL `http://127.0.0.1:8080/v1` and `enabled = true`).

- [ ] **Step 3: README.** Add a short "Enrichment" paragraph to `README.md` under the feature list: images stored locally under `images_dir` and served at `/images`, detail pages fetched on new listings and price changes, LLM extraction of French listing attributes via a local OpenAI-compatible endpoint, configured in `[llm]` or from the Réglages tab.

- [ ] **Step 4: Full verification**

```bash
nix develop -c cargo fmt --all
nix develop -c cargo test --workspace
nix develop -c cargo clippy --workspace --all-targets 2>&1 | tail -5
```

Expected: fmt clean, all tests PASS, no new clippy errors. Then exercise the real flow once (run skill / manual): `nix develop -c cargo run -p terrier-server` with a test config pointing at the llama.cpp server, confirm `/api/status` shows `llm` and `enrichment_pending`, and `PUT /api/settings/llm` + probe round-trip against the live endpoint.

- [ ] **Step 5: Commit**

```bash
git add -A && git -c commit.gpgsign=false commit -m "enrichment: status strip, example config, docs"
```

---

## Self-review results (already applied)

- Spec coverage: schema→T2, queue/worker→T3/T8, scrapers→T5/T6, LLM→T7, API/UI→T10/T11, config/docs→T12. Ouest France: RawListing gains defaulted fields in T1 (its parser fills them when the source comes online — spec's "same optional slots"); the spec's "generic CSS engine" does not exist in the tree yet, nothing to do.
- Type consistency: `LlmSettingsUpdate` (not ferret's `LlmOverride`) everywhere; `ListingDetail` merge semantics defined once in `Db::set_detail`; image URL rewriting only in the API layer.
- The Task-3 `log_llm_request` forward-reference is resolved by creating the minimal `llm.rs` stub in Task 3 and replacing it in Task 7.
