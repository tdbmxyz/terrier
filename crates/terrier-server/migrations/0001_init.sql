-- terrier schema v1 — fresh project, single migration (a ferret lesson:
-- design the moderation/settings/history columns in from the start).

CREATE TABLE searches (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    locations TEXT NOT NULL,          -- JSON array of raw location strings
    max_price_cents INTEGER,
    min_surface_m2 REAL,
    min_rooms INTEGER,
    property_types TEXT NOT NULL,     -- JSON array
    active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL
);

CREATE TABLE listings (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    canonical_url TEXT NOT NULL,
    title TEXT NOT NULL,
    price_cents INTEGER NOT NULL,
    property_type TEXT NOT NULL,
    surface_m2 REAL,
    rooms INTEGER,
    bedrooms INTEGER,
    land_m2 REAL,
    commune TEXT,
    postal_code TEXT,
    lat REAL,
    lng REAL,
    dpe TEXT,
    ges TEXT,
    sell_type TEXT,
    description TEXT,
    address TEXT,
    seller_name TEXT,
    seller_type TEXT,               -- 'pro' | 'private'
    siren TEXT,
    attributes TEXT NOT NULL DEFAULT '{}',  -- ExtractedAttrs JSON
    enriched_at TEXT,               -- detail fetch done (or not applicable)
    extracted_at TEXT,              -- LLM extraction done for current description
    flags TEXT NOT NULL DEFAULT '[]', -- JSON array
    status TEXT NOT NULL DEFAULT 'active',
    moderation TEXT NOT NULL DEFAULT 'none',
    first_seen TEXT NOT NULL,
    last_seen TEXT NOT NULL,
    UNIQUE (source_id, canonical_url)
);
CREATE INDEX idx_listings_commune ON listings (commune, status);
CREATE INDEX idx_listings_status ON listings (status, moderation, last_seen);

-- every price ever observed: one row per listing per day, latest wins
CREATE TABLE listing_prices (
    listing_id TEXT NOT NULL REFERENCES listings (id) ON DELETE CASCADE,
    day TEXT NOT NULL,                -- ISO date (UTC)
    price_cents INTEGER NOT NULL,
    PRIMARY KEY (listing_id, day)
);

CREATE TABLE search_matches (
    search_id TEXT NOT NULL REFERENCES searches (id) ON DELETE CASCADE,
    listing_id TEXT NOT NULL REFERENCES listings (id) ON DELETE CASCADE,
    matched_at TEXT NOT NULL,
    notified_price_cents INTEGER,
    PRIMARY KEY (search_id, listing_id)
);

-- runtime-editable settings (key → JSON), ferret pattern
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

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
