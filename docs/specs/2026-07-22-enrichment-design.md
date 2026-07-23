# terrier — listing enrichment (images, description, seller, LLM attributes)

Fills the v1 gaps except new sources (deferred): every listing gets its
images stored locally, its full description, its seller identity, a finer
address when the source provides one, and structured attributes extracted
from the description by a local LLM. Scrapers are adapted to capture the
new data.

Decisions (user): images downloaded locally on first sight; detail page
fetched on NEW listings and on PRICE CHANGES (never on plain re-scrapes);
LLM extraction via any OpenAI-compatible endpoint (llama.cpp on zeus,
ferret's proven `llm.rs` pattern) — configurable, fail-open, retry later;
no migration file — schema changes go straight into `0001_init.sql`
(nothing is deployed yet).

## Architecture

A separate **enrichment queue + background worker** (option B):
the scrape pipeline stores whatever the search page already embeds
(truncated description, image URLs, seller) and enqueues the listing;
a per-source worker drains the queue through the same politeness budget
as scraping. Steps are independent and each fails open:

1. detail-page fetch → full description, complete image set, seller,
   finer address, missing attributes
2. image download → `<data_dir>/images/<listing_id>/<n>.<ext>`
3. LLM extraction → `attributes` JSON (runs on the truncated
   description if the detail fetch failed; re-runs when the stored
   description changes)

Failures retry with a 60s→6h exponential backoff per queue item; at the
attempt cap the item is dropped (a later price change re-enqueues it
with a fresh budget). The LLM and the detail fetch are refinement layers,
never dependencies — scraping and matching keep working without them.

## Schema (edits to 0001_init.sql)

- `listings` gains: `description TEXT`, `address TEXT`,
  `seller_name TEXT`, `seller_type TEXT` ('pro'|'private'),
  `siren TEXT`, `attributes TEXT NOT NULL DEFAULT '{}'` (JSON),
  `enriched_at TEXT`, `extracted_at TEXT`.
- `listing_images(listing_id, position, url, local_path, fetched_at,
  PRIMARY KEY (listing_id, position))` — files kept when a listing goes
  gone (that history is the point).
- `enrichment_queue(listing_id PRIMARY KEY, reason, attempts,
  next_attempt_at, last_error)` — reason 'new' | 'price-change'.
- `llm_requests` log table (ferret's shape: kind, model, duration_ms,
  ok, error, prompt/completion tokens, created_at).

## Domain

- `Listing` gains `description`, `address`, `seller: Option<Seller>`
  (`name`, `kind: SellerKind{Pro,Private}`, `siren`),
  `attributes: ExtractedAttrs`, and the API shape carries
  `images: Vec<ListingImage>` (position, url, local url).
- `ExtractedAttrs` — all `Option`, serialized as the `attributes` JSON:
  `annee_construction: i64`, `travaux` ("a-prevoir" | "rafraichissement"
  | "aucun"), `chauffage_type`, `chauffage_energie`, `fibre: bool`,
  `charges_copro_month_cents`, `taxe_fonciere_year_cents`, `etage`,
  `ascenseur: bool`, `jardin: bool`, `garage_parking: bool`,
  `piscine: bool`, `orientation`, `mitoyenne: bool`,
  `notes: Vec<String>` (servitude, locataire en place, viager occupé…).
  The prompt forbids guessing: absent in the text ⇒ null.
- `RawListing` gains optional `description`, `address`, `image_urls`,
  `seller_name`, `seller_type`, `siren` — sources fill what they have.

## Scrapers

- **leboncoin search parser**: additionally read `images` (URL list),
  `body` (truncated description) and `owner` (type pro/private, name,
  siren) from each ad's JSON, so baseline data exists even if the detail
  fetch never succeeds.
- **leboncoin detail parser** (new): `__NEXT_DATA__` of an ad page,
  built against a live-captured fixture — full description, complete
  image set, full seller block, finer address when present, attributes
  the search page lacked. Fetched via the same `ScrapeClient` +
  curl-on-403/429 fallback.
- **ouestfrance / generic**: gain the same optional slots (CSS selectors
  for description/images/seller in the generic engine) so they fill them
  when enabled. No new sources in this iteration.

## Enrichment worker

- Pipeline enqueues on `UpsertOutcome::New` and on any price change.
- One worker task per source, sharing that source's politeness layer so
  scrape + enrichment stay under the per-host budget together.
- Image downloads use a separate browser-UA client against the CDN host
  (distinct from the scrape host) with a fixed 500 ms spacing; images are fetched
  once (a row with `local_path` set is never re-fetched); count capped
  per listing (config, default 10).
- Queue depth and LLM busy state surface in the status strip.

## LLM extraction (port of ferret's llm.rs)

- `[llm]` config: `enabled` (default false), `base_url`, `model`,
  `api_key_file`, `timeout_secs` — plus ferret's DB settings override,
  prompt override, model-list (`GET /models`) and probe ("Test" button),
  request logging, busy counter, structured-output call with strict
  `json_schema` and one plain retry, reasoning-model content salvage.
- One call per extraction: system prompt (French real-estate extraction,
  answer only the JSON object, never guess) + user JSON of title, price,
  known structured attributes, description. Response parsed into
  `ExtractedAttrs`.

## API / UI

- `GET /api/listings` includes the new fields and image list; the server
  serves `<data_dir>/images` at `/images/...`.
- Cards: cover thumbnail, seller badge (pro/particulier + name),
  attribute chips beside the existing surface/DPE chips; expanding a
  card shows the description and the image gallery.
- New Settings tab: LLM enable/endpoint/model/test + prompt override
  (ferret's panel).

## Testing

- Live-captured fixture for the LBC ad detail page; parser unit tests.
- Ported llm.rs tests (request shape, strict schema, fence/think
  stripping, budget-exhausted reasoning error, plain retry).
- Enrichment queue tests on in-memory SQLite: enqueue on new and on
  price change, backoff/attempt caps, image idempotency.
- Pipeline tests asserting no enqueue on unchanged re-scrape.
