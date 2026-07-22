# terrier — self-hosted immobilier price tracker

ferret's sibling for real estate (ventes only, v1). The mission: record
EVERY price of every listing and make the history visible — per listing
and per commune — with alerts on new matches and price drops.

Decisions (user): sales only; searches are structured commune + criteria
(no LLM interpretation); listings stay separate per source (no
cross-source property dedupe); name "terrier".

## Architecture

ferret's proven skeleton, trimmed: Rust workspace — terrier-domain
(pure logic), terrier-server (axum + sqlx SQLite WAL, port 4810),
terrier-client (dual native/wasm reqwest), terrier-ui (Leptos CSR),
terrier-web (Trunk) — nix flake + NixOS module. Web-only v1, built
mobile-first; no Tauri shell yet. No LLM in v1: Leboncoin's immo
category carries structured attributes, so the parsing noise that
forced ferret's LLM gate does not exist here. The settings-table
pattern ships anyway for future runtime knobs.

## Domain

- `Listing`: source_id, canonical_url, title, price_cents,
  surface_m2 (f64), rooms, bedrooms, property_type
  (house/apartment/land/other), commune, postal_code, lat/lng,
  dpe + ges (A–G), land_m2, sell_type (old/new/viager),
  status (active/gone with revive), moderation (none/dismissed/banned),
  first_seen/last_seen. Derived: price_per_m2.
- `listing_prices`: one row per listing per day, latest wins — every
  change recorded. The UI receives a short history INLINE with each
  listing (no N+1 fetches; the sparkline is always visible).
- `Search`: name, locations (raw "Rennes 35000"-style strings, mapped
  to each source's URL format), max_price_cents, min_surface_m2,
  min_rooms, property_types (set), active. Active searches' locations
  feed the scrape rotation (deduped, capped) exactly like ferret's
  watch queries.
- Matching: location (postal code or commune name, case-insensitive)
  + criteria; missing listing attributes fail closed for criteria the
  search sets (a listing without surface doesn't match a min-surface
  search).
- Wanted-ad flag (leading recherche/cherche/achat) suppresses pushes.

## Sources

- **leboncoin-immo** (flagship, validated live 2026-07-22: 200 +
  __NEXT_DATA__ with ferret's headers): category=9 search per location,
  `price_cents` direct, attributes square/rooms/bedrooms/
  real_estate_type/energy_rate/ges/land_plot_surface/immo_sell_type,
  location city/zipcode/lat/lng. curl fallback on 403/429 as in ferret.
- **ouestfrance-immo**: bot-walled (403; curl-impersonate gets decoy
  410 shells; data loads via internal SPA API). Ships as a plugin
  behind ferret's proven `fetch_command` stealth hook, disabled by
  default; parser built against a captured fixture once the stealth
  fetcher runs on zeus.
- **generic** CSS-selector engine inherited from ferret for future
  static-HTML sources.

## Notifications

ntfy: new match (title, price, €/m², surface, commune) and ANY price
drop ≥ configurable pct (old → new shown). Wanted ads never push.

## UI (ferret's lessons applied from day 1)

- Mobile-first: wrapping layouts, min-width: 0, no horizontal scroll.
- Landing = the history: listing cards always show the price sparkline
  and Δ% since first seen, plus €/m² badge, surface/rooms/commune/DPE
  chips. Sort by biggest drop / €/m² / price / recency; filter by search.
- Searches tab: structured form (communes, max €, min m², min rooms,
  type checkboxes), rows with live match counts, edit/pause/delete.
- Communes tab: per-commune median €/m² (current vs 30 days ago) and
  listing counts.
- Status strip (sources ticking), busy states, dismiss/ban moderation
  with "hidden only" review, in-app About (versions + git commits),
  settings table plumbing.

## Deployment

flake packages terrier-server / terrier-web, nixosModules.terrier
(port 4810), docs/zeus-config-example.nix with Leboncoin enabled and
baseline locations.
