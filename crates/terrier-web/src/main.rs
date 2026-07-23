use leptos::prelude::*;
use terrier_ui::{App, AppConfig};
use url::Url;

/// Where the terrier API lives, resolved in order:
///
/// 1. `window.TERRIER_API_BASE` — set by a hosting shell (the Tauri shell
///    injects it).
/// 2. `localStorage["terrier-api-base"]` — manual override, survives reloads.
/// 3. The page origin, when it can actually be the server — the
///    served-by-terrier-server case (and trunk's dev proxy). Tauri's own
///    origins are the app bundle, never the API.
/// 4. The default local server.
fn resolve() -> Url {
    let fallback = Url::parse("http://127.0.0.1:4810").expect("valid fallback url");
    let Some(window) = web_sys::window() else {
        return fallback;
    };

    let injected = js_sys::Reflect::get(&window, &"TERRIER_API_BASE".into())
        .ok()
        .and_then(|v| v.as_string());
    let stored = window
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item("terrier-api-base").ok().flatten());
    if let Some(url) = [injected, stored]
        .into_iter()
        .flatten()
        .find_map(|raw| Url::parse(&raw).ok())
    {
        return url;
    }

    match window.location().origin().ok().map(|o| Url::parse(&o)) {
        Some(Ok(url))
            if (url.scheme() == "http" || url.scheme() == "https")
                && url.host_str() != Some("tauri.localhost") =>
        {
            url
        }
        _ => fallback,
    }
}

fn main() {
    console_error_panic_hook::set_once();
    let config = AppConfig {
        api_base: resolve(),
    };
    mount_to_body(move || view! { <App config=config.clone()/> });
}
