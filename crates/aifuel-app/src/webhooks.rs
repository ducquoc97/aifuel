//! Quota event webhooks.
//!
//! After each monitoring collection, every configured endpoint receives a
//! JSON POST for each quota window that changed state: `threshold_crossed`
//! when a window's consumed percentage reaches its configured threshold and
//! `quota_reset` when a window that was over threshold comes back under it
//! (the window reset or quota was replenished). Notification state is kept
//! in `webhook-state.json` next to `webhooks.json` so one crossing notifies
//! once instead of on every refresh.
//!
//! Delivery follows the MCP Gateway's trust rules: endpoints must use HTTPS
//! except for loopback HTTP, redirects are not followed, and failures are
//! logged without ever breaking collection.

use aifuel_core::{ProviderStatus, ProviderUsage, QuotaWindow, StatusReport};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The webhook configuration file inside the AI Fuel user config directory.
pub const WEBHOOKS_FILE_NAME: &str = "webhooks.json";
const STATE_FILE_NAME: &str = "webhook-state.json";
const STATE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_THRESHOLD_PERCENT: f64 = 90.0;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Delivers quota events produced by monitoring collections to the
/// `webhooks.json` endpoint set.
pub struct WebhookNotifier {
    endpoints: Vec<ResolvedEndpoint>,
    state_path: PathBuf,
    /// Serializes read-modify-write of the dedupe state so two concurrent
    /// collections cannot observe the same pre-transition snapshot and
    /// deliver a crossing twice.
    state_lock: Mutex<()>,
}

impl WebhookNotifier {
    /// Load `webhooks.json` from the AI Fuel config directory. An absent file
    /// disables delivery (`None`); a malformed file is a configuration error
    /// the caller reports while collection continues without notifications.
    /// Entries that parse but are not deliverable are skipped with a warning.
    pub fn load(config_dir: &Path) -> Result<Option<Self>, WebhookConfigError> {
        let path = config_dir.join(WEBHOOKS_FILE_NAME);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(WebhookConfigError::Io(error)),
        };
        let file: WebhooksFile = serde_json::from_slice(&bytes)?;
        let mut endpoints = Vec::new();
        for entry in &file.webhooks {
            match resolve_entry(entry, &file.defaults) {
                Ok(endpoint) => endpoints.push(endpoint),
                Err(reason) => eprintln!("aifuel: webhook entry skipped: {reason}"),
            }
        }
        Ok(Some(Self {
            endpoints,
            state_path: config_dir.join(STATE_FILE_NAME),
            state_lock: Mutex::new(()),
        }))
    }

    /// Evaluate a freshly collected report and POST each resulting event to
    /// the endpoints subscribed to it. State is persisted before delivery so
    /// a failed send never replays the same crossing; every failure is
    /// logged and swallowed because notification must not break collection.
    pub async fn deliver(&self, report: &StatusReport) {
        if self.endpoints.is_empty() {
            return;
        }
        let pending = {
            let _guard = self.state_lock.lock().expect("webhook state mutex");
            let mut state = WebhookState::load(&self.state_path);
            let mut dirty = false;
            let mut pending = Vec::new();
            for provider in &report.providers {
                if provider.status != ProviderStatus::Ok {
                    continue;
                }
                let anchor = provider
                    .windows
                    .iter()
                    .map(|window| period_rank(&window.period))
                    .min()
                    .unwrap_or(u8::MAX);
                for window in &provider.windows {
                    let Some(used) = window.used_percent else {
                        continue;
                    };
                    for (index, endpoint) in self.endpoints.iter().enumerate() {
                        let key = state_key(provider, window, endpoint.threshold_percent);
                        let was_above = state
                            .windows
                            .get(&key)
                            .map(|mark| mark.above)
                            .unwrap_or(false);
                        let above = used >= endpoint.threshold_percent;
                        if was_above != above {
                            state.windows.insert(key, WindowMark { above });
                            dirty = true;
                        }
                        let kind = match (was_above, above) {
                            (false, true) => Some(EventKind::ThresholdCrossed),
                            (true, false) => Some(EventKind::QuotaReset),
                            _ => None,
                        };
                        if let Some(kind) = kind
                            && endpoint.events.contains(&kind)
                        {
                            pending.push((
                                index,
                                WebhookPayload::new(
                                    kind,
                                    provider,
                                    window,
                                    period_rank(&window.period) == anchor,
                                    endpoint.threshold_percent,
                                    report.generated_at,
                                ),
                            ));
                        }
                    }
                }
            }
            if dirty && let Err(error) = state.save(&self.state_path) {
                eprintln!("aifuel: webhook state could not be saved: {error}");
            }
            pending
        };
        for (index, payload) in pending {
            deliver_one(&self.endpoints[index], &payload).await;
        }
    }
}

#[derive(Debug, Deserialize)]
struct WebhooksFile {
    #[serde(default)]
    webhooks: Vec<WebhookEntry>,
    #[serde(default)]
    defaults: WebhookDefaults,
}

#[derive(Debug, Deserialize)]
struct WebhookEntry {
    url: String,
    #[serde(default)]
    events: Option<Vec<String>>,
    #[serde(default)]
    threshold_percent: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct WebhookDefaults {
    #[serde(default)]
    events: Option<Vec<String>>,
    #[serde(default)]
    threshold_percent: Option<f64>,
}

#[derive(Debug)]
struct ResolvedEndpoint {
    url: Url,
    /// Scheme and authority only; the path may carry a secret token and is
    /// never logged.
    display: String,
    events: Vec<EventKind>,
    threshold_percent: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EventKind {
    ThresholdCrossed,
    QuotaReset,
}

impl EventKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "threshold_crossed" => Some(Self::ThresholdCrossed),
            "quota_reset" => Some(Self::QuotaReset),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize)]
struct WebhookPayload {
    event: EventKind,
    provider: String,
    provider_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    window: String,
    window_period: String,
    /// Whether this window carries the provider's longest-period quota - the
    /// same anchor the dashboard ranks on (monthly, weekly, daily, 5h).
    authoritative: bool,
    percent_used: f64,
    percent_remaining: Option<f64>,
    threshold_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_at: Option<f64>,
    checked_at: f64,
}

impl WebhookPayload {
    fn new(
        event: EventKind,
        provider: &ProviderUsage,
        window: &QuotaWindow,
        authoritative: bool,
        threshold_percent: f64,
        checked_at: f64,
    ) -> Self {
        Self {
            event,
            provider: provider.key.as_str().to_owned(),
            provider_name: provider.name,
            account_id: provider.account_id.clone(),
            window: window.label.clone(),
            window_period: window.period.clone(),
            authoritative,
            percent_used: window.used_percent.unwrap_or(0.0),
            percent_remaining: window.remaining_percent,
            threshold_percent,
            reset_at: window.resets_at,
            checked_at,
        }
    }
}

/// The dedupe record: for each watched window and threshold pair, whether it
/// was already over the line at the previous collection.
#[derive(Debug, Serialize, Deserialize)]
struct WebhookState {
    version: u32,
    #[serde(default)]
    windows: BTreeMap<String, WindowMark>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct WindowMark {
    above: bool,
}

impl Default for WebhookState {
    fn default() -> Self {
        Self {
            version: STATE_SCHEMA_VERSION,
            windows: BTreeMap::new(),
        }
    }
}

impl WebhookState {
    /// Load prior marks; a missing file starts empty and a corrupt or
    /// incompatible file is discarded so delivery degrades to a one-time
    /// re-notification rather than wedging.
    fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(state) if state.version == STATE_SCHEMA_VERSION => state,
            Ok(_) => {
                eprintln!("aifuel: webhook state schema changed; notification history reset");
                Self::default()
            }
            Err(error) => {
                eprintln!("aifuel: webhook state file is unreadable and was ignored: {error}");
                Self::default()
            }
        }
    }

    fn save(&self, path: &Path) -> Result<(), io::Error> {
        let serialized = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_path(path);
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = options.open(&temporary)?;
            set_private_permissions(&file)?;
            file.write_all(&serialized)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn resolve_entry(
    entry: &WebhookEntry,
    defaults: &WebhookDefaults,
) -> Result<ResolvedEndpoint, String> {
    let url = validate_url(&entry.url)?;
    let host = url.host_str().unwrap_or("unknown host");
    let display = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    let names = entry
        .events
        .clone()
        .or_else(|| defaults.events.clone())
        .unwrap_or_else(|| vec!["threshold_crossed".to_owned(), "quota_reset".to_owned()]);
    let mut events = Vec::new();
    for name in &names {
        match EventKind::parse(name) {
            Some(kind) if !events.contains(&kind) => events.push(kind),
            Some(_) => {}
            None => eprintln!("aifuel: webhook event name {name:?} is not recognized"),
        }
    }
    if events.is_empty() {
        return Err(format!(
            "webhook {} subscribes to no known events",
            entry.url
        ));
    }
    let threshold_percent = entry
        .threshold_percent
        .or(defaults.threshold_percent)
        .unwrap_or(DEFAULT_THRESHOLD_PERCENT);
    if !threshold_percent.is_finite() || !(0.0..=100.0).contains(&threshold_percent) {
        return Err(format!(
            "webhook {} threshold_percent must be between 0 and 100",
            entry.url
        ));
    }
    Ok(ResolvedEndpoint {
        url,
        display,
        events,
        threshold_percent,
    })
}

/// Apply the same trust rule as remote MCP endpoints: HTTPS everywhere,
/// plain HTTP only on loopback. `localhost` resolves lazily at delivery so
/// config loading stays synchronous; literal non-loopback addresses are
/// rejected here.
fn validate_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| format!("webhook URL {value:?} is invalid"))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(format!(
            "webhook URL {value:?} cannot contain user information or a fragment"
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("webhook URL {value:?} must include a host"))?;
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback_host(host) => Ok(url),
        _ => Err(format!(
            "webhook URL {value:?} must use HTTPS or loopback HTTP"
        )),
    }
}

/// `localhost` (resolved at delivery time) or a literal loopback address,
/// with the brackets `host_str` keeps on IPv6 literals stripped.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

async fn deliver_one(endpoint: &ResolvedEndpoint, payload: &WebhookPayload) {
    let client = match endpoint_client(endpoint).await {
        Ok(client) => client,
        Err(reason) => {
            eprintln!(
                "aifuel: webhook {} delivery failed: {reason}",
                endpoint.display
            );
            return;
        }
    };
    match client.post(endpoint.url.clone()).json(payload).send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => eprintln!(
            "aifuel: webhook {} rejected the event with HTTP {}",
            endpoint.display,
            response.status()
        ),
        Err(error) => eprintln!(
            "aifuel: webhook {} delivery failed: {error}",
            endpoint.display
        ),
    }
}

/// Build the per-endpoint HTTP client, pinning `localhost` to the addresses
/// it resolves to so a name that answers with a non-loopback address cannot
/// satisfy the loopback exception.
async fn endpoint_client(endpoint: &ResolvedEndpoint) -> Result<Client, &'static str> {
    let mut builder = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DELIVERY_TIMEOUT)
        .redirect(Policy::none());
    let url = &endpoint.url;
    if url.scheme() == "http" {
        builder = builder.no_proxy();
        if let Some(host) = url.host_str()
            && host.eq_ignore_ascii_case("localhost")
        {
            let port = url
                .port_or_known_default()
                .ok_or("loopback webhook URL has no port")?;
            let addresses: Vec<SocketAddr> =
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
                    .await
                    .map_err(|_| "webhook loopback endpoint resolution timed out")?
                    .map_err(|_| "webhook loopback endpoint could not be resolved")?
                    .collect();
            if addresses.is_empty() || addresses.iter().any(|addr| !addr.ip().is_loopback()) {
                return Err("webhook HTTP endpoint must resolve only to loopback addresses");
            }
            let pinned: Vec<_> = addresses
                .into_iter()
                .map(|address| SocketAddr::new(address.ip(), 0))
                .collect();
            builder = builder.resolve_to_addrs(host, &pinned);
        }
    }
    builder
        .build()
        .map_err(|_| "webhook HTTP client could not be created")
}

/// The dedupe key identifies one window under one threshold; account and
/// label are part of the key so a provider's separate windows transition
/// independently.
fn state_key(provider: &ProviderUsage, window: &QuotaWindow, threshold: f64) -> String {
    format!(
        "{}|{}|{}|{}",
        provider.key,
        provider.account_id.as_deref().unwrap_or(""),
        window.label,
        threshold
    )
}

/// The same longest-period-first ranking the dashboard applies when it marks
/// a window as the provider's anchor period.
fn period_rank(period: &str) -> u8 {
    match period {
        "monthly" => 0,
        "weekly" => 1,
        "daily" => 2,
        "5h" => 3,
        _ => 4,
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(STATE_FILE_NAME);
    path.with_file_name(format!(".{name}.tmp-{}-{suffix}", std::process::id()))
}

fn set_private_permissions(_file: &fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn replace_file(temporary_path: &Path, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary_path, path)
}

#[derive(Debug)]
pub enum WebhookConfigError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for WebhookConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "webhook configuration I/O failed: {error}"),
            Self::Json(error) => write!(f, "invalid webhook configuration JSON: {error}"),
        }
    }
}

impl std::error::Error for WebhookConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

impl From<io::Error> for WebhookConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for WebhookConfigError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
#[path = "webhooks_tests.rs"]
mod tests;
