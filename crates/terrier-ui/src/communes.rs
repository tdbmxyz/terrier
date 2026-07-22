//! Communes dashboard: median €/m² per commune, now vs 30 days ago.

use leptos::prelude::*;
use terrier_client::TerrierClient;

use crate::DataVersion;

#[component]
pub fn CommunesView() -> impl IntoView {
    let client: TerrierClient = expect_context();
    let version: DataVersion = expect_context();
    let stats = LocalResource::new(move || {
        version.0.track();
        let client = client.clone();
        async move { client.communes().await }
    });

    view! {
        <section>
            <p class="muted">
                "Médiane du €/m² sur les annonces actives, par commune — la \
                 colonne « il y a 30 j » n'apparaît qu'avec assez d'historique."
            </p>
            {move || match stats.get() {
                None => view! { <p class="muted">"Chargement…"</p> }.into_any(),
                Some(Err(e)) => {
                    view! { <p class="error">{format!("serveur injoignable : {e}")}</p> }
                        .into_any()
                }
                Some(Ok(rows)) if rows.is_empty() => view! {
                    <p class="muted">"Pas encore de données — les annonces arrivent d'abord."</p>
                }
                .into_any(),
                Some(Ok(rows)) => view! {
                    <div class="table-scroll">
                        <table class="communes">
                            <thead>
                                <tr>
                                    <th>"Commune"</th>
                                    <th>"Annonces"</th>
                                    <th>"€/m² médian"</th>
                                    <th>"il y a 30 j"</th>
                                    <th>"Δ"</th>
                                </tr>
                            </thead>
                            <tbody>
                                {rows.into_iter().map(|s| {
                                    let now = s.median_m2_cents;
                                    let old = s.median_m2_cents_30d;
                                    let delta = match (now, old) {
                                        (Some(n), Some(o)) if o > 0 => {
                                            Some((n - o) as f64 / o as f64 * 100.0)
                                        }
                                        _ => None,
                                    };
                                    view! {
                                        <tr>
                                            <td>{match &s.postal_code {
                                                Some(cp) => format!("{} ({cp})", s.commune),
                                                None => s.commune.clone(),
                                            }}</td>
                                            <td>{s.listings}</td>
                                            <td>{now.map(|c| format!("{} €", c / 100))
                                                .unwrap_or_else(|| "—".into())}</td>
                                            <td>{old.map(|c| format!("{} €", c / 100))
                                                .unwrap_or_else(|| "—".into())}</td>
                                            <td class=delta.map(|d| if d <= 0.0 { "ok" } else { "warn" })
                                                .unwrap_or("muted")>
                                                {delta.map(|d| format!("{d:+.1}%"))
                                                    .unwrap_or_else(|| "—".into())}
                                            </td>
                                        </tr>
                                    }
                                }).collect_view()}
                            </tbody>
                        </table>
                    </div>
                }
                .into_any(),
            }}
        </section>
    }
}
