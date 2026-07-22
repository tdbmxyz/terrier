# terrier

Self-hosted immobilier price tracker — ferret's sibling for real estate.
terrier scrapes ventes immobilières, records **every price of every
listing**, and shows the history: per-listing sparklines, Δ since
publication, and per-commune median €/m². Alerts (ntfy) on new matches
and on any price drop.

## Layout

- `crates/terrier-domain` — types + matching, pure logic
- `crates/terrier-server` — axum API, SQLite storage, scraper scheduler
- `crates/terrier-client` — typed API client (native + wasm)
- `crates/terrier-ui` — Leptos components (mobile-first)
- `crates/terrier-web` — trunk entrypoint

## Development

```
nix develop
cargo test --workspace
cargo run -p terrier-server         # reads terrier.toml / $TERRIER_CONFIG
cd crates/terrier-web && trunk serve  # dev frontend on :8082
```

Config reference: `crates/terrier-server/terrier.example.toml`.
NixOS: `docs/zeus-config-example.nix` (module `nixosModules.terrier`,
port 4810).

## Sources

- **leboncoin-immo**: category 9 search per location; structured
  attributes (surface, pièces, DPE, terrain) parsed from the embedded
  page JSON; curl fallback when DataDome blocks the plain client.
- **ouestfrance-immo** (experimental): bot-walled — runs only through
  `fetch_command` (stealth browser wrapper).

Searches are structured (communes + prix max + surface min + pièces +
types); their locations join the scrape rotation automatically. Matching
fails closed on missing attributes, wanted ads ("Recherche maison…")
never notify, and dismiss/ban moderation is built in.
