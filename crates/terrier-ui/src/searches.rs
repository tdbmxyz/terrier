//! Searches: structured commune + criteria form (no free text to
//! interpret), rows with live match counts, edit/pause/delete. Every
//! action answers immediately (ferret lesson).

use leptos::prelude::*;
use leptos::task::spawn_local;
use terrier_client::TerrierClient;
use terrier_domain::{PropertyType, Search, SearchRequest};

use crate::{DataVersion, format_price};

const ALL_TYPES: [PropertyType; 3] =
    [PropertyType::House, PropertyType::Apartment, PropertyType::Land];

#[derive(Clone, Copy)]
struct EditRequest(RwSignal<Option<Search>>);

#[component]
pub fn SearchesView() -> impl IntoView {
    let client: TerrierClient = expect_context();
    let version: DataVersion = expect_context();
    provide_context(EditRequest(RwSignal::new(None)));

    let searches = LocalResource::new({
        let client = client.clone();
        move || {
            version.0.track();
            let client = client.clone();
            async move { client.searches().await }
        }
    });

    view! {
        <section>
            <SearchForm/>
            {move || match searches.get() {
                None => view! { <p class="muted">"Chargement…"</p> }.into_any(),
                Some(Err(e)) => {
                    view! { <p class="error">{format!("serveur injoignable : {e}")}</p> }
                        .into_any()
                }
                Some(Ok(items)) if items.is_empty() => view! {
                    <p class="muted">"Aucune recherche — créez-en une ci-dessus."</p>
                }
                .into_any(),
                Some(Ok(items)) => view! {
                    <ul class="watches">
                        {items.into_iter().map(search_row).collect_view()}
                    </ul>
                }
                .into_any(),
            }}
        </section>
    }
}

#[component]
fn SearchForm() -> impl IntoView {
    let client: TerrierClient = expect_context();
    let version: DataVersion = expect_context();
    let edit: EditRequest = expect_context();

    let name = RwSignal::new(String::new());
    let locations = RwSignal::new(String::new());
    let max_price = RwSignal::new(String::new());
    let min_surface = RwSignal::new(String::new());
    let min_rooms = RwSignal::new(String::new());
    let types = RwSignal::new(Vec::<PropertyType>::new());
    let editing = RwSignal::new(None::<uuid::Uuid>);
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);

    // an edit request loads the search into the form
    Effect::new(move |_| {
        let Some(search) = edit.0.get() else { return };
        edit.0.set(None);
        editing.set(Some(search.id));
        name.set(search.name.clone());
        locations.set(search.locations.join(", "));
        max_price.set(
            search.max_price_cents.map(|c| (c / 100).to_string()).unwrap_or_default(),
        );
        min_surface.set(
            search.min_surface_m2.map(|s| format!("{s:.0}")).unwrap_or_default(),
        );
        min_rooms.set(search.min_rooms.map(|r| r.to_string()).unwrap_or_default());
        types.set(search.property_types.clone());
    });

    let reset = move || {
        editing.set(None);
        name.set(String::new());
        locations.set(String::new());
        max_price.set(String::new());
        min_surface.set(String::new());
        min_rooms.set(String::new());
        types.set(Vec::new());
        message.set(None);
    };

    let save = {
        let client = client.clone();
        move |_| {
            let request = SearchRequest {
                name: name.get_untracked().trim().to_string(),
                locations: locations
                    .get_untracked()
                    .split(',')
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect(),
                max_price_cents: max_price
                    .get_untracked()
                    .trim()
                    .replace([' ', '\u{202f}'], "")
                    .parse::<i64>()
                    .ok()
                    .map(|e| e * 100),
                min_surface_m2: min_surface.get_untracked().trim().parse().ok(),
                min_rooms: min_rooms.get_untracked().trim().parse().ok(),
                property_types: types.get_untracked(),
                active: true,
            };
            if request.name.is_empty() {
                message.set(Some("il faut un nom".into()));
                return;
            }
            if request.locations.is_empty() {
                message.set(Some("au moins une commune ou un code postal".into()));
                return;
            }
            busy.set(true);
            let client = client.clone();
            let update_id = editing.get_untracked();
            spawn_local(async move {
                let result = match update_id {
                    Some(id) => client.update_search(id, &request).await,
                    None => client.create_search(&request).await,
                };
                match result {
                    Ok(_) => {
                        version.0.update(|v| *v += 1);
                        reset();
                    }
                    Err(e) => message.set(Some(e.to_string())),
                }
                busy.set(false);
            });
        }
    };

    view! {
        <div class="guided">
            <div class="editor-head">
                <input placeholder="nom (ex : maison Bruz)" prop:value=name
                    on:input=move |ev| name.set(event_target_value(&ev))/>
                <input class="wide"
                    placeholder="communes / codes postaux, séparés par des virgules (ex : Bruz 35170, Rennes 35000)"
                    prop:value=locations
                    on:input=move |ev| locations.set(event_target_value(&ev))/>
            </div>
            <div class="editor-head">
                <input class="narrow" placeholder="prix max €" prop:value=max_price
                    on:input=move |ev| max_price.set(event_target_value(&ev))/>
                <input class="narrow" placeholder="surface min m²" prop:value=min_surface
                    on:input=move |ev| min_surface.set(event_target_value(&ev))/>
                <input class="narrow" placeholder="pièces min" prop:value=min_rooms
                    on:input=move |ev| min_rooms.set(event_target_value(&ev))/>
                {ALL_TYPES
                    .into_iter()
                    .map(|t| {
                        view! {
                            <label class="enum-value">
                                <input type="checkbox"
                                    prop:checked=move || types.with(|ts| ts.contains(&t))
                                    on:change=move |ev| types.update(|ts| {
                                        if event_target_checked(&ev) {
                                            if !ts.contains(&t) { ts.push(t); }
                                        } else {
                                            ts.retain(|x| *x != t);
                                        }
                                    })/>
                                {t.label()}
                            </label>
                        }
                    })
                    .collect_view()}
            </div>
            <div class="editor-actions">
                <button on:click=save disabled=move || busy.get()>
                    {move || match (busy.get(), editing.get().is_some()) {
                        (true, _) => "Enregistrement…",
                        (false, true) => "Mettre à jour",
                        (false, false) => "Créer la recherche",
                    }}
                </button>
                {move || editing.get().is_some().then(|| view! {
                    <button on:click=move |_| reset()>"Annuler"</button>
                })}
                {move || message.get().map(|m| view! { <span class="error">{m}</span> })}
            </div>
        </div>
    }
}

fn search_row(search: Search) -> impl IntoView {
    let client: TerrierClient = expect_context();
    let version: DataVersion = expect_context();
    let edit: EditRequest = expect_context();
    let status: crate::status::StatusResource = expect_context();
    let search_id = search.id;
    let match_count = move || {
        status
            .0
            .get()
            .flatten()
            .and_then(|s| s.search_matches.get(&search_id).copied())
            .unwrap_or(0)
    };

    let mut criteria: Vec<String> = vec![search.locations.join(", ")];
    if let Some(max) = search.max_price_cents {
        criteria.push(format!("≤ {}", format_price(max)));
    }
    if let Some(min) = search.min_surface_m2 {
        criteria.push(format!("≥ {min:.0} m²"));
    }
    if let Some(min) = search.min_rooms {
        criteria.push(format!("≥ {min} pièces"));
    }
    if !search.property_types.is_empty() {
        criteria.push(
            search.property_types.iter().map(|t| t.label()).collect::<Vec<_>>().join("/"),
        );
    }

    let toggle = {
        let client = client.clone();
        let search = search.clone();
        move |_| {
            let client = client.clone();
            let request = SearchRequest {
                name: search.name.clone(),
                locations: search.locations.clone(),
                max_price_cents: search.max_price_cents,
                min_surface_m2: search.min_surface_m2,
                min_rooms: search.min_rooms,
                property_types: search.property_types.clone(),
                active: !search.active,
            };
            let id = search.id;
            spawn_local(async move {
                let _ = client.update_search(id, &request).await;
                version.0.update(|v| *v += 1);
            });
        }
    };
    let start_edit = {
        let search = search.clone();
        move |_| edit.0.set(Some(search.clone()))
    };
    let delete = {
        let client = client.clone();
        let id = search.id;
        move |_| {
            let client = client.clone();
            spawn_local(async move {
                let _ = client.delete_search(id).await;
                version.0.update(|v| *v += 1);
            });
        }
    };

    view! {
        <li class="watch" class:inactive=!search.active>
            <div class="watch-main">
                <span class="watch-name">
                    {search.name.clone()}
                    " "
                    <span class="badge ok">{move || format!("{} annonces", match_count())}</span>
                </span>
                <span class="muted">{criteria.join(" · ")}</span>
            </div>
            <div class="watch-actions">
                <button on:click=start_edit>"éditer"</button>
                <button on:click=toggle>
                    {if search.active { "pause" } else { "reprendre" }}
                </button>
                <button class="danger" on:click=delete>"supprimer"</button>
            </div>
        </li>
    }
}
