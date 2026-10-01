//! The provider's Advertised Model catalog discovery.
//!
//! `GET /provider` is the serve's own catalog route: it reports every
//! model the provider knows plus the `connected` provider ids that hold
//! credentials. A one-shot `opencode serve` answers it, then dies.
//! Older serves without `/provider` answer `GET /config/providers` with
//! the same provider/model shape, so that is the fallback - never a
//! fabricated list.

use super::protocol;
use super::serve::{self, BasicAuth, ServeHandle};
use crate::agent_execution::kill_and_wait;
use crate::model_catalog::ProviderCatalogModel;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// The overall catalog probe bound: spawn, readiness, and the provider
/// listing share it. The serve answers in milliseconds once listening.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(75);

/// A catalog discovery snapshot: advertised models and the provider
/// ids the serve reported as connected.
pub(crate) struct CatalogResult {
    pub models: Vec<ProviderCatalogModel>,
    pub connected: BTreeSet<String>,
}

impl CatalogResult {
    fn empty() -> Self {
        Self {
            models: Vec::new(),
            connected: BTreeSet::new(),
        }
    }
}

/// Discover the advertised catalog once. Discovery failures surface as
/// an empty result, never a guessed list. The probe runs on a
/// short-lived runtime on a scoped thread, the same pattern the
/// execution adapter's discovery uses.
pub(super) fn discover() -> CatalogResult {
    let result = thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .ok()?;
                runtime.block_on(discover_async())
            })
            .join()
    });
    result.ok().flatten().unwrap_or_else(CatalogResult::empty)
}

async fn discover_async() -> Option<CatalogResult> {
    let cwd = std::env::temp_dir();
    let deadline = Instant::now() + CATALOG_TIMEOUT;
    // The catalog probe is not an instance run: it spawns with the
    // inherited environment only. Instance overlays belong to the session
    // spawn path (`session.create`), not to model listing.
    let mut serve = serve::spawn_serve(&cwd, &std::collections::BTreeMap::new()).ok()?;
    let result = probe(&mut serve, &cwd, deadline).await;
    if let Some(child) = serve.child.as_mut() {
        let _ = kill_and_wait(child).await;
    }
    result
}

/// Wait for the serve to answer HTTP, then read the provider list.
async fn probe(serve: &mut ServeHandle, cwd: &Path, deadline: Instant) -> Option<CatalogResult> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    wait_ready(&client, serve, cwd, deadline).await?;
    for path in ["/provider", "/config/providers"] {
        let url = protocol::endpoint(&serve.base_url, path, cwd).ok()?;
        let response = authed(&client, serve.auth.as_ref(), url).send().await;
        let Ok(response) = response else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.json::<Value>().await else {
            continue;
        };
        let result = parse_catalog(&body);
        if !result.models.is_empty() {
            return Some(result);
        }
    }
    None
}

/// Any HTTP response proves the serve is listening; TCP-level errors
/// retry until the deadline, an early exit fails fast.
async fn wait_ready(
    client: &reqwest::Client,
    serve: &mut ServeHandle,
    cwd: &Path,
    deadline: Instant,
) -> Option<()> {
    let url = protocol::endpoint(&serve.base_url, protocol::SESSIONS_PATH, cwd).ok()?;
    loop {
        if serve
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .is_some()
        {
            return None;
        }
        match authed(client, serve.auth.as_ref(), url.clone())
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(_) => return Some(()),
            Err(_) if Instant::now() >= deadline => return None,
            Err(_) => tokio::time::sleep(READY_POLL).await,
        }
    }
}

fn authed(
    client: &reqwest::Client,
    auth: Option<&BasicAuth>,
    url: reqwest::Url,
) -> reqwest::RequestBuilder {
    let request = client.get(url);
    match auth {
        Some(auth) => request.basic_auth(&auth.username, Some(&auth.password)),
        None => request,
    }
}

/// Read the provider catalog response: `/provider` answers
/// `{all, default, connected}`, `/config/providers` answers
/// `{providers, default}` with the same model shape.
fn parse_catalog(body: &Value) -> CatalogResult {
    let mut models = Vec::new();
    let providers = body["all"]
        .as_array()
        .or_else(|| body["providers"].as_array());
    for provider in providers.into_iter().flatten() {
        let Some(provider_id) = provider["id"].as_str() else {
            continue;
        };
        let Some(entries) = provider["models"].as_object() else {
            continue;
        };
        for (model_id, model) in entries {
            models.push(ProviderCatalogModel {
                model_id: format!("{provider_id}/{model_id}"),
                display_label: model["name"].as_str().map(str::to_owned),
                // OpenCode reports a `reasoning` capability flag, not a
                // selectable effort ladder; effort stays unreported.
                default_effort: None,
                supported_efforts: None,
            });
        }
    }
    let connected = body["connected"]
        .as_array()
        .map(|connected| {
            connected
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    CatalogResult { models, connected }
}
