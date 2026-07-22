//! About: client + server versions with commits — "what am I running?".

use leptos::prelude::*;
use terrier_client::TerrierClient;

#[component]
pub fn AboutView() -> impl IntoView {
    let client: TerrierClient = expect_context();
    let server_url = client.base().to_string();

    let health = LocalResource::new({
        let client = client.clone();
        move || {
            let client = client.clone();
            async move { client.health().await }
        }
    });

    view! {
        <section class="about">
            <p>
                <span class="muted">"Client : "</span>
                {format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("TERRIER_COMMIT"))}
            </p>
            <p>
                <span class="muted">"Serveur : "</span>
                {move || match health.get() {
                    None => "vérification…".to_string(),
                    Some(Ok(h)) => format!(
                        "{} ({}) — {server_url}",
                        h.version,
                        h.commit.unwrap_or_else(|| "unknown".into()),
                    ),
                    Some(Err(e)) => format!("injoignable ({e})"),
                }}
            </p>
            <p class="muted">
                "terrier creuse les annonces immobilières et garde tous les prix. "
                <a href="https://github.com/tdbmxyz/terrier" target="_blank" rel="noreferrer">
                    "Source"
                </a>
            </p>
        </section>
    }
}
