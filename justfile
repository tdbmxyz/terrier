# terrier task runner. `just --list` for the menu.

# Run the full test suite
test:
    cargo test --workspace

# Lint everything, warnings are errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the server (reads terrier.toml / $TERRIER_CONFIG)
serve:
    cargo run -p terrier-server

# Dev frontend on :8082 with /api proxied to a local server on :4810
web:
    cd crates/terrier-web && trunk serve
