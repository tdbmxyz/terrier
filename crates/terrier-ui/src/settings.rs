//! Settings: LLM endpoint (OpenAI-compatible, llama.cpp on zeus) and the
//! extraction prompt — ferret's panel, trimmed to the one extract pass.

use leptos::prelude::*;
use leptos::task::spawn_local;
use terrier_client::TerrierClient;
use terrier_domain::{LlmPrompts, LlmSettingsUpdate};

#[component]
pub fn SettingsView() -> impl IntoView {
    let client: TerrierClient = expect_context();

    let enabled = RwSignal::new(false);
    let base_url = RwSignal::new(String::new());
    let model = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());
    let models = RwSignal::new(Vec::<String>::new());
    let feedback = RwSignal::new(String::new());
    let prompt = RwSignal::new(String::new());

    // initial load
    {
        let client = client.clone();
        spawn_local(async move {
            if let Ok(s) = client.llm_settings().await {
                enabled.set(s.enabled);
                base_url.set(s.base_url);
                model.set(s.model);
            }
            if let Ok(p) = client.llm_prompts().await {
                prompt.set(p.extract);
            }
        });
    }

    let update = move || LlmSettingsUpdate {
        enabled: enabled.get_untracked(),
        base_url: base_url.get_untracked(),
        model: model.get_untracked(),
        api_key: {
            let k = api_key.get_untracked();
            (!k.trim().is_empty()).then(|| k.trim().to_string())
        },
    };

    let load_models = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let url = base_url.get_untracked();
            spawn_local(async move {
                match client.llm_models(&url).await {
                    Ok(m) => {
                        feedback.set(format!("{} modèle(s)", m.len()));
                        models.set(m);
                    }
                    Err(e) => feedback.set(format!("modèles : {e}")),
                }
            });
        }
    };

    let probe = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let u = update();
            feedback.set("test en cours…".into());
            spawn_local(async move {
                match client.llm_probe(&u).await {
                    Ok(()) => feedback.set("✓ le modèle répond".into()),
                    Err(e) => feedback.set(format!("échec : {e}")),
                }
            });
        }
    };

    let save = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let u = update();
            spawn_local(async move {
                match client.update_llm_settings(&u).await {
                    Ok(s) => {
                        feedback.set("enregistré".into());
                        enabled.set(s.enabled);
                        base_url.set(s.base_url);
                        model.set(s.model);
                        api_key.set(String::new());
                    }
                    Err(e) => feedback.set(format!("échec : {e}")),
                }
            });
        }
    };

    let save_prompt = {
        let client = client.clone();
        move |_| {
            let client = client.clone();
            let p = LlmPrompts {
                extract: prompt.get_untracked(),
            };
            spawn_local(async move {
                match client.update_llm_prompts(&p).await {
                    Ok(p) => {
                        feedback.set("prompt enregistré".into());
                        prompt.set(p.extract);
                    }
                    Err(e) => feedback.set(format!("échec : {e}")),
                }
            });
        }
    };

    view! {
        <section class="settings">
            <div class="settings-block">
                <span class="settings-title">"Extraction LLM (serveur)"</span>
                <label class="spec">
                    <input type="checkbox" prop:checked=enabled
                        on:change=move |ev| enabled.set(event_target_checked(&ev))/>
                    "activer l'extraction des descriptions"
                </label>
                <input prop:value=base_url placeholder="http://127.0.0.1:8080/v1"
                    on:input=move |ev| base_url.set(event_target_value(&ev))/>
                <div class="row">
                    <input prop:value=model placeholder="modèle" list="llm-models"
                        on:input=move |ev| model.set(event_target_value(&ev))/>
                    <datalist id="llm-models">
                        {move || models.get().into_iter()
                            .map(|m| view! { <option value=m></option> })
                            .collect_view()}
                    </datalist>
                    <button on:click=load_models>"lister"</button>
                </div>
                <input prop:value=api_key type="password"
                    placeholder="clé API (vide = inchangée)"
                    on:input=move |ev| api_key.set(event_target_value(&ev))/>
                <div class="row">
                    <button on:click=probe>"Tester"</button>
                    <button class="primary" on:click=save>"Enregistrer"</button>
                </div>
                <span class="muted">{move || feedback.get()}</span>
            </div>
            <div class="settings-block">
                <span class="settings-title">"Prompt d'extraction (vide = défaut)"</span>
                <textarea prop:value=prompt rows="10"
                    on:input=move |ev| prompt.set(event_target_value(&ev))></textarea>
                <button on:click=save_prompt>"Enregistrer le prompt"</button>
            </div>
        </section>
    }
}
