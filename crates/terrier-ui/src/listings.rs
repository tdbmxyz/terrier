//! Listings view: the history IS the landing page. Every card shows its
//! sparkline and Δ since first seen without a click (the history arrives
//! inline with the listing — no N+1). Sort by biggest drop / €/m² /
//! price / recency; filter by search; dismiss/ban moderation.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use terrier_client::TerrierClient;
use terrier_domain::{Flag, ListingStatus, ListingWithHistory, Moderation, Search};
use uuid::Uuid;

use crate::{DataVersion, format_price};

const REFRESH: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Recent,
    BiggestDrop,
    PricePerM2,
    Price,
}

#[component]
pub fn ListingsView() -> impl IntoView {
    let client: TerrierClient = expect_context();
    let version: DataVersion = expect_context();
    let filter = RwSignal::new(None::<Uuid>);
    let sort = RwSignal::new(Sort::Recent);
    let show_hidden = RwSignal::new(false);

    let tick = RwSignal::new(0u32);
    if let Ok(handle) = set_interval_with_handle(move || tick.update(|n| *n += 1), REFRESH) {
        on_cleanup(move || handle.clear());
    }

    let searches = LocalResource::new({
        let client = client.clone();
        move || {
            version.0.track();
            let client = client.clone();
            async move { client.searches().await.unwrap_or_default() }
        }
    });
    let listings = LocalResource::new(move || {
        tick.track();
        version.0.track();
        let client = client.clone();
        let search_id = filter.get();
        let hidden = show_hidden.get();
        async move { client.listings(search_id, hidden).await }
    });

    let sorted = move |mut items: Vec<ListingWithHistory>| {
        match sort.get() {
            Sort::Recent => {} // server order: last_seen desc
            Sort::BiggestDrop => items.sort_by_key(drop_pct_milli),
            Sort::PricePerM2 => {
                items.sort_by_key(|l| l.listing.price_per_m2_cents().unwrap_or(i64::MAX))
            }
            Sort::Price => items.sort_by_key(|l| l.listing.price_cents),
        }
        items
    };

    view! {
        <section>
            <crate::status::SourcesStrip/>
            <div class="toolbar">
                <select on:change=move |ev| {
                    filter.set(Uuid::parse_str(&event_target_value(&ev)).ok());
                }>
                    <option value="">"toutes les recherches"</option>
                    {move || {
                        searches
                            .get()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|s: Search| {
                                view! { <option value=s.id.to_string()>{s.name}</option> }
                            })
                            .collect_view()
                    }}
                </select>
                <select on:change=move |ev| {
                    sort.set(match event_target_value(&ev).as_str() {
                        "drop" => Sort::BiggestDrop,
                        "m2" => Sort::PricePerM2,
                        "price" => Sort::Price,
                        _ => Sort::Recent,
                    });
                }>
                    <option value="recent">"récentes d'abord"</option>
                    <option value="drop">"plus grosses baisses"</option>
                    <option value="m2">"€/m² croissant"</option>
                    <option value="price">"prix croissant"</option>
                </select>
                <label class="spec">
                    <input type="checkbox" prop:checked=show_hidden
                        on:change=move |ev| show_hidden.set(event_target_checked(&ev))/>
                    "masquées"
                </label>
            </div>
            {move || match listings.get() {
                None => view! { <p class="muted">"Chargement…"</p> }.into_any(),
                Some(Err(e)) => {
                    view! { <p class="error">{format!("serveur injoignable : {e}")}</p> }
                        .into_any()
                }
                Some(Ok(items)) if items.is_empty() && show_hidden.get() => {
                    view! { <p class="muted">"Rien de masqué ni banni."</p> }.into_any()
                }
                Some(Ok(items)) if items.is_empty() => view! {
                    <p class="muted">
                        "Aucune annonce — elles arrivent dès qu'une recherche active \
                         alimente les sources."
                    </p>
                }
                .into_any(),
                Some(Ok(items)) => view! {
                    <ul class="deals">
                        {sorted(items)
                            .into_iter()
                            .map(|l| view! { <ListingCard item=l/> })
                            .collect_view()}
                    </ul>
                }
                .into_any(),
            }}
        </section>
    }
}

/// Price move since first observation, in thousandths (negative = drop),
/// missing history sorts last.
fn drop_pct_milli(l: &ListingWithHistory) -> i64 {
    match l.history.first() {
        Some(first) if first.price_cents > 0 => {
            ((l.listing.price_cents - first.price_cents) * 1000) / first.price_cents
        }
        _ => i64::MAX,
    }
}

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

#[component]
fn ListingCard(item: ListingWithHistory) -> impl IntoView {
    let listing = item.listing;
    let history = item.history;
    let images = item.images;
    let gone = listing.status == ListingStatus::Gone;
    let listing_id = listing.id;

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
    let has_gallery = gallery.len() > 1;
    let description = listing.description.clone();
    let has_description = description.is_some();
    let notes = listing.attributes.notes.clone();
    let attr_line = attr_chips(&listing.attributes).join(" · ");
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

    let mut chips: Vec<String> = vec![listing.property_type.label().to_string()];
    if let Some(s) = listing.surface_m2 {
        chips.push(format!("{s:.0} m²"));
    }
    if let Some(r) = listing.rooms {
        chips.push(format!("{r} p."));
    }
    if let Some(land) = listing.land_m2 {
        chips.push(format!("terrain {land:.0} m²"));
    }
    if let (Some(commune), cp) = (&listing.commune, &listing.postal_code) {
        chips.push(match cp {
            Some(cp) => format!("{commune} ({cp})"),
            None => commune.clone(),
        });
    }
    chips.push(listing.source_id.clone());

    let delta = history.first().and_then(|first| {
        if first.price_cents > 0 && first.price_cents != listing.price_cents {
            Some((listing.price_cents - first.price_cents) as f64 / first.price_cents as f64
                * 100.0)
        } else {
            None
        }
    });

    let moderation = listing.moderation;

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
                        {(moderation == Moderation::Dismissed).then(|| view! {
                            <span class="badge muted">"masquée"</span>
                        })}
                        {(moderation == Moderation::Banned).then(|| view! {
                            <span class="badge bad">"bannie"</span>
                        })}
                        {gone.then(|| view! { <span class="badge muted">"disparue"</span> })}
                    </div>
                    {(!attr_line.is_empty()).then(|| view! {
                        <div class="deal-meta">
                            <span class="muted attrs">{attr_line.clone()}</span>
                        </div>
                    })}
                </div>
            </div>
            {(history.len() > 1).then(|| view! {
                <crate::sparkline::Sparkline prices=history.clone() currency="EUR".to_string()/>
            })}
            {(has_description || has_gallery).then(|| view! {
                <button class="expand" on:click=move |_| expanded.update(|e| *e = !*e)>
                    {move || if expanded.get() { "réduire" } else { "détails" }}
                </button>
            })}
            {move || expanded.get().then(|| {
                let description = description.clone();
                let notes = notes.clone();
                let gallery = gallery.clone();
                view! {
                    <div class="deal-detail">
                        {description.map(|d| view! { <p class="description">{d}</p> })}
                        {(!notes.is_empty()).then(|| view! {
                            <p class="muted">{format!("À noter : {}", notes.join(" · "))}</p>
                        })}
                        <div class="gallery">
                            {gallery.into_iter().map(|src| view! {
                                <img src=src loading="lazy"/>
                            }).collect_view()}
                        </div>
                    </div>
                }
            })}
            <ModerationButtons listing_id=listing_id current=moderation/>
        </li>
    }
}

#[component]
fn ModerationButtons(listing_id: Uuid, current: Moderation) -> impl IntoView {
    let client: TerrierClient = expect_context();
    let version: DataVersion = expect_context();
    let set = move |moderation: Moderation| {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            spawn_local(async move {
                let _ = client.set_moderation(listing_id, moderation).await;
                version.0.update(|v| *v += 1);
            });
        }
    };
    view! {
        <div class="watch-actions deal-actions">
            {(current != Moderation::Dismissed).then(|| view! {
                <button title="masquer — revient si l'annonce disparaît puis est repostée"
                    on:click=set(Moderation::Dismissed)>
                    "masquer"
                </button>
            })}
            {(current != Moderation::Banned).then(|| view! {
                <button class="danger" title="ne plus jamais voir cette annonce"
                    on:click=set(Moderation::Banned)>
                    "bannir"
                </button>
            })}
            {(current != Moderation::None).then(|| view! {
                <button on:click=set(Moderation::None)>"restaurer"</button>
            })}
        </div>
    }
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
