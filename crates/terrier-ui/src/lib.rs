//! Shared Leptos UI, mounted by the web bundle. Mobile-first from day 1.

mod about;
mod communes;
mod listings;
mod searches;
mod settings;
mod sparkline;
mod status;

use leptos::prelude::*;
use terrier_client::TerrierClient;
use url::Url;

#[derive(Clone)]
pub struct AppConfig {
    pub api_base: Url,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Listings,
    Searches,
    Communes,
    Settings,
    About,
}

/// Bumped after every mutation so list resources reload.
#[derive(Clone, Copy)]
pub(crate) struct DataVersion(pub(crate) RwSignal<u32>);

const API_BASE_KEY: &str = "terrier-api-base";

#[component]
pub fn App(config: AppConfig) -> impl IntoView {
    provide_context(TerrierClient::new(config.api_base.clone()));
    provide_context(DataVersion(RwSignal::new(0)));
    let status_tick = RwSignal::new(0u32);
    if let Ok(handle) = set_interval_with_handle(
        move || status_tick.update(|n| *n += 1),
        std::time::Duration::from_secs(30),
    ) {
        on_cleanup(move || handle.clear());
    }
    status::provide_status(status_tick);
    let tab = RwSignal::new(Tab::Listings);
    let show_connect = RwSignal::new(false);
    let server = RwSignal::new(config.api_base.to_string());

    let tab_button = move |target: Tab, label: &'static str| {
        view! {
            <button
                class:active=move || tab.get() == target
                on:click=move |_| tab.set(target)
            >
                {label}
            </button>
        }
    };

    let save_server = move |_| {
        let value = server.get_untracked();
        let Some(window) = web_sys::window() else {
            return;
        };
        if let Ok(Some(storage)) = window.local_storage() {
            if value.trim().is_empty() || Url::parse(value.trim()).is_err() {
                let _ = storage.remove_item(API_BASE_KEY);
            } else {
                let _ = storage.set_item(API_BASE_KEY, value.trim());
            }
            let _ = window.location().reload();
        }
    };

    view! {
        <header class="topbar">
            <span class="brand">"terrier"</span>
            <nav>
                {tab_button(Tab::Listings, "Annonces")}
                {tab_button(Tab::Searches, "Recherches")}
                {tab_button(Tab::Communes, "Communes")}
                {tab_button(Tab::Settings, "Réglages")}
                {tab_button(Tab::About, "À propos")}
            </nav>
            <button class="connect-toggle" title="server address"
                on:click=move |_| show_connect.update(|s| *s = !*s)>
                "⚙"
            </button>
        </header>
        {move || show_connect.get().then(|| view! {
            <div class="connect">
                <div class="settings-block">
                    <span class="settings-title">"Server (this device)"</span>
                    <input prop:value=server placeholder="http://zeus:4810"
                        on:input=move |ev| server.set(event_target_value(&ev))/>
                    <button on:click=save_server>"Save & reload"</button>
                    <span class="muted">"empty = back to automatic"</span>
                </div>
            </div>
        })}
        <main>
            <div style:display=move || if tab.get() == Tab::Listings { "" } else { "none" }>
                <listings::ListingsView/>
            </div>
            <div style:display=move || if tab.get() == Tab::Searches { "" } else { "none" }>
                <searches::SearchesView/>
            </div>
            <div style:display=move || if tab.get() == Tab::Communes { "" } else { "none" }>
                <communes::CommunesView/>
            </div>
            <div style:display=move || if tab.get() == Tab::Settings { "" } else { "none" }>
                <settings::SettingsView/>
            </div>
            <div style:display=move || if tab.get() == Tab::About { "" } else { "none" }>
                <about::AboutView/>
            </div>
        </main>
    }
}

/// "32000000 cents" → "320 000 €" (immo prices are whole euros; thin
/// group spacing for readability).
pub(crate) fn format_price(cents: i64) -> String {
    let euros = cents / 100;
    let s = euros.abs().to_string();
    let mut grouped = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            grouped.push('\u{202f}');
        }
        grouped.push(c);
    }
    let sign = if euros < 0 { "-" } else { "" };
    format!("{sign}{grouped} €")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_formatting_groups_thousands() {
        assert_eq!(format_price(32_000_000), "320\u{202f}000 €");
        assert_eq!(format_price(128_500_000), "1\u{202f}285\u{202f}000 €");
        assert_eq!(format_price(99_900), "999 €");
    }
}
