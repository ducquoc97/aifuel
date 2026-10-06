//! The compiled OAuth profile set: declarative `OAuthFlowSpec` values the
//! flow mechanics in this module run against. Config selects a profile by
//! `OAuthProfileId`; it cannot invent flow behavior - the set is closed at
//! compile time per `docs/specs/provider-integrations.md`.

use super::http;
use crate::credentials::{OAuthTokens, env_override};
use aifuel_core::OAuthProfileId;
use serde::Deserialize;

/// One compiled OAuth flow profile: the endpoints, client identity, and
/// grant mechanics of a provider's public OAuth client. AI Fuel reuses the
/// provider CLI's public `client_id`, the same convention opencode and
/// codex-rs follow, because these providers do not register third-party
/// OAuth clients.
pub struct OAuthFlowSpec {
    /// The profile identity `AuthBinding::OAuth.profile` selects.
    pub profile: &'static str,
    /// The catalog provider the grant belongs to.
    pub provider: &'static str,
    /// The integration the minted grant is bound to at store time.
    pub integration: &'static str,
    /// The provider's public OAuth client id.
    pub client_id: &'static str,
    /// The token endpoint serving code exchange and refresh grants.
    pub token_url: &'static str,
    /// Refresh grant encoding: GitHub expects a form, OpenAI's backend
    /// expects JSON (as codex-rs sends it).
    pub refresh_form: bool,
    /// Extracts a Provider Account identity from a JWT `id_token`, when
    /// the flow issues one.
    pub account_from_id_token: Option<fn(&str) -> Option<String>>,
    /// The interactive login flows this profile can run; the first is the
    /// default, alternatives are selected explicitly (e.g. `--device`).
    pub flows: &'static [OAuthFlow],
}

/// One interactive grant flow a profile can run.
pub enum OAuthFlow {
    /// RFC 8628 device authorization (GitHub's flavor).
    Device {
        device_url: &'static str,
        scope: &'static str,
        /// Headers GitHub's device and token endpoints expect from a
        /// first-party editor client.
        headers: &'static [(&'static str, &'static str)],
    },
    /// Authorization-code + PKCE on a loopback listener (RFC 8252 + 7636).
    Loopback {
        authorize_url: &'static str,
        scope: &'static str,
        extra_params: &'static [(&'static str, &'static str)],
        /// Redirect ports the provider's allowlist accepts, tried in order.
        ports: &'static [u16],
    },
    /// OpenAI's headless device grant: `{issuer}/api/accounts/deviceauth`
    /// returns the PKCE pair alongside the authorization code, so no
    /// loopback listener is needed.
    DeviceAuth { issuer: &'static str },
}

/// The Codex profile: ChatGPT subscription OAuth behind `codex:oauth`.
pub(crate) static CODEX: OAuthFlowSpec = OAuthFlowSpec {
    profile: "codex",
    provider: "codex",
    integration: "codex:oauth",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    token_url: "https://auth.openai.com/oauth/token",
    refresh_form: false,
    account_from_id_token: Some(codex_account_id),
    flows: &[
        OAuthFlow::Loopback {
            authorize_url: "https://auth.openai.com/oauth/authorize",
            scope: "openid profile email offline_access api.connectors.read api.connectors.invoke",
            extra_params: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "aifuel"),
            ],
            ports: &[1455, 1457],
        },
        OAuthFlow::DeviceAuth {
            issuer: "https://auth.openai.com",
        },
    ],
};

/// The Copilot profile: GitHub's device authorization grant behind
/// `copilot:oauth`. The issued grant is a plain GitHub OAuth token; the
/// adapter exchanges it for a short-lived Copilot session per run.
pub(crate) static COPILOT: OAuthFlowSpec = OAuthFlowSpec {
    profile: "copilot",
    provider: "copilot",
    integration: "copilot:oauth",
    client_id: "Iv1.b507a08c87ecfe98",
    token_url: "https://github.com/login/oauth/access_token",
    refresh_form: true,
    account_from_id_token: None,
    flows: &[OAuthFlow::Device {
        device_url: "https://github.com/login/device/code",
        scope: "read:user",
        headers: &[
            ("accept", "application/json"),
            ("editor-version", "Neovim/0.6.1"),
            ("editor-plugin-version", "copilot.lua"),
            ("user-agent", "GithubCopilot/1.155.0"),
        ],
    }],
};

/// The compiled profile set `OAuthProfileId` values resolve against.
const PROFILES: &[&OAuthFlowSpec] = &[&CODEX, &COPILOT];

/// Look up a compiled OAuth profile by identity.
pub fn profile(id: &OAuthProfileId) -> Option<&'static OAuthFlowSpec> {
    PROFILES
        .iter()
        .copied()
        .find(|spec| spec.profile == id.as_str())
}

/// Every compiled profile, for `auth login` help and listing surfaces.
pub fn profiles() -> impl Iterator<Item = &'static OAuthFlowSpec> {
    PROFILES.iter().copied()
}

/// The profile a selector names: a profile id, a catalog provider id, or
/// the integration the profile's grant binds to.
pub fn profile_for(selector: &str) -> Option<&'static OAuthFlowSpec> {
    PROFILES.iter().copied().find(|spec| {
        spec.profile == selector || spec.provider == selector || spec.integration == selector
    })
}

/// The resolved parameters of one flow: spec constants with `AIFUEL_OAUTH_*`
/// overrides applied. Tests exercising flow mechanics build these values
/// directly rather than mutating the process environment.
pub enum FlowParams {
    Device(DeviceFlowParams),
    Loopback(LoopbackFlowParams),
    DeviceAuth(DeviceAuthParams),
}

/// The endpoints and form fields of an RFC 8628 device grant.
pub struct DeviceFlowParams {
    pub device_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scope: String,
    pub headers: Vec<(String, String)>,
}

/// The endpoints and form fields of a loopback authorization-code flow.
pub struct LoopbackFlowParams {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scope: String,
    pub extra_params: Vec<(String, String)>,
    pub ports: Vec<u16>,
}

/// The endpoints of OpenAI's `deviceauth` grant.
pub struct DeviceAuthParams {
    pub issuer: String,
    pub token_url: String,
    pub client_id: String,
}

/// Materialize a flow's wire parameters, honoring `AIFUEL_OAUTH_*`
/// endpoint overrides (`DEVICE_URL`, `AUTHORIZE_URL`, `TOKEN_URL`,
/// `ISSUER`, `CLIENT_ID`, `LOOPBACK_PORTS`).
pub fn flow_params(spec: &OAuthFlowSpec, flow: &OAuthFlow) -> FlowParams {
    let client_id =
        env_override("AIFUEL_OAUTH_CLIENT_ID").unwrap_or_else(|| spec.client_id.to_owned());
    let token_url =
        env_override("AIFUEL_OAUTH_TOKEN_URL").unwrap_or_else(|| spec.token_url.to_owned());
    match flow {
        OAuthFlow::Device {
            device_url,
            scope,
            headers,
        } => FlowParams::Device(DeviceFlowParams {
            device_url: env_override("AIFUEL_OAUTH_DEVICE_URL")
                .unwrap_or_else(|| (*device_url).to_owned()),
            token_url,
            client_id,
            scope: (*scope).to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }),
        OAuthFlow::Loopback {
            authorize_url,
            scope,
            extra_params,
            ports,
        } => FlowParams::Loopback(LoopbackFlowParams {
            authorize_url: env_override("AIFUEL_OAUTH_AUTHORIZE_URL")
                .unwrap_or_else(|| (*authorize_url).to_owned()),
            token_url,
            client_id,
            scope: (*scope).to_owned(),
            extra_params: extra_params
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            ports: env_override("AIFUEL_OAUTH_LOOPBACK_PORTS")
                .map(|list| {
                    list.split(',')
                        .filter_map(|port| port.trim().parse().ok())
                        .collect()
                })
                .unwrap_or_else(|| ports.to_vec()),
        }),
        OAuthFlow::DeviceAuth { issuer } => FlowParams::DeviceAuth(DeviceAuthParams {
            issuer: env_override("AIFUEL_OAUTH_ISSUER").unwrap_or_else(|| (*issuer).to_owned()),
            token_url,
            client_id,
        }),
    }
}

/// The shape every compiled flow's token endpoint answers with. GitHub's
/// device grant returns only `access_token`; OpenAI's endpoints add
/// `refresh_token`, `expires_in`, and the `id_token` carrying the account.
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub id_token: Option<String>,
    /// GitHub answers even pending/denied device polls with HTTP 200 and
    /// the RFC 8628 error code in the body - the poll loop reads this
    /// rather than the status.
    #[serde(default)]
    pub error: Option<String>,
}

/// Convert a token endpoint's response into store-ready grant material:
/// expiry from the JWT `exp` claim first, then `expires_in`; account
/// identity from the spec's `id_token` extractor.
pub(crate) fn tokens_from_response(spec: &OAuthFlowSpec, response: TokenResponse) -> OAuthTokens {
    let expires = http::jwt_exp(&response.access_token)
        .map(|at| at as i64)
        .or_else(|| {
            response
                .expires_in
                .map(|seconds| http::unix_now() as i64 + seconds as i64)
        });
    OAuthTokens {
        access: response.access_token,
        refresh: response.refresh_token,
        expires,
        account_id: spec
            .account_from_id_token
            .and_then(|extract| response.id_token.as_deref().and_then(extract)),
        destination: None,
    }
}

/// The ChatGPT account identity inside a Codex `id_token`: the
/// `https://api.openai.com/auth` namespaced claim that names the billing
/// account, as codex-rs reads it.
fn codex_account_id(id_token: &str) -> Option<String> {
    http::jwt_payload(id_token)?
        .get("https://api.openai.com/auth")
        .and_then(|claims| claims.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}
