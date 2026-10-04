//! Dashboard Admin Sessions and their public endpoints.
//!
//! Three routes sit outside the management tier:
//!
//! - `POST /api/login` checks the Admin Credential (`aifuel::admin`) and
//!   answers an `aifuel_session` cookie: `HttpOnly`, `Path=/`,
//!   `SameSite=Lax`, and `Secure` when the request arrived behind TLS
//!   (`X-Forwarded-Proto: https`, the documented reverse-proxy shape).
//! - `POST /api/logout` revokes the presented session and expires the
//!   cookie.
//! - `GET /api/auth/session` reports `{configured, authenticated}` so the
//!   shell knows whether to show its sign-out control.
//!
//! Sessions are random tokens in process memory with a sliding 30-day
//! expiry - restart signs everyone out, which is the documented posture.
//! `/v1` never sees them; that surface stays on Gateway Keys.

use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Method, Request};

/// Cookie carrying the session token.
const COOKIE_NAME: &str = "aifuel_session";
/// Sliding session lifetime; activity renews it.
const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Pause before answering a bad password - a cheap brute-force dampener
/// for a single-user credential.
const FAILURE_DELAY: Duration = Duration::from_millis(400);
const MAX_BODY_BYTES: u64 = 8 * 1024;

/// The in-memory session table shared by every request thread.
pub(crate) struct Sessions {
    inner: Mutex<HashMap<String, u64>>,
}

impl Sessions {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a session token after a verified login.
    fn issue(&self) -> String {
        let token = aifuel::admin::fresh_token();
        self.inner
            .lock()
            .expect("session table mutex")
            .insert(token.clone(), expiry());
        token
    }

    /// Whether `token` names a live session; a hit slides the expiry
    /// forward so an active operator is never signed out mid-work.
    pub(crate) fn check(&self, token: &str) -> bool {
        let now = now_unix();
        let mut sessions = self.inner.lock().expect("session table mutex");
        match sessions.get_mut(token) {
            Some(expires) if *expires > now => {
                *expires = now + SESSION_TTL.as_secs();
                true
            }
            Some(_) => {
                sessions.remove(token);
                false
            }
            None => false,
        }
    }

    /// End one session; unknown tokens are already gone.
    fn revoke(&self, token: &str) {
        self.inner
            .lock()
            .expect("session table mutex")
            .remove(token);
    }
}

/// Whether `(method, path)` is one of the public auth routes - the
/// caller checks before handing the request to [`handle`].
pub(crate) fn claims(method: &Method, path: &str) -> bool {
    matches!(
        (method, path),
        (&Method::Post, "/api/login")
            | (&Method::Post, "/api/logout")
            | (&Method::Get, "/api/auth/session")
    )
}

/// Serve one claimed public auth route.
pub(crate) fn handle(request: Request, sessions: &Sessions) {
    let path = request.url().split('?').next().unwrap_or(request.url());
    match (request.method(), path) {
        (&Method::Post, "/api/login") => login(request, sessions),
        (&Method::Post, "/api/logout") => logout(request, sessions),
        (&Method::Get, "/api/auth/session") => session_state(request, sessions),
        _ => unreachable!("dispatch claims only the three auth routes"),
    }
}

/// Whether the request carries a live session cookie.
pub(crate) fn authenticated(request: &Request, sessions: &Sessions) -> bool {
    session_token(request).is_some_and(|token| sessions.check(&token))
}

fn login(mut request: Request, sessions: &Sessions) {
    #[derive(serde::Deserialize)]
    struct LoginBody {
        password: String,
    }
    let body: LoginBody = match read_json_body(&mut request) {
        Ok(body) => body,
        Err(error) => return respond_json(request, 400, &json!({"error": error})),
    };
    if !aifuel::admin::configured() {
        return respond_json(
            request,
            503,
            &json!({"error": "no admin password is configured; run `aifuel auth set-admin`"}),
        );
    }
    if !aifuel::admin::verify(&body.password) {
        std::thread::sleep(FAILURE_DELAY);
        return respond_json(request, 401, &json!({"error": "invalid password"}));
    }
    let token = sessions.issue();
    let cookie = session_cookie(&request, &token);
    respond_json_with_cookie(request, 200, &json!({"ok": true}), Some(cookie));
}

fn logout(request: Request, sessions: &Sessions) {
    if let Some(token) = session_token(&request) {
        sessions.revoke(&token);
    }
    let cookie = expired_cookie(&request);
    respond_json_with_cookie(request, 200, &json!({"ok": true}), Some(cookie));
}

fn session_state(request: Request, sessions: &Sessions) {
    let authenticated = authenticated(&request, sessions);
    respond_json(
        request,
        200,
        &json!({
            "configured": aifuel::admin::configured(),
            "authenticated": authenticated,
        }),
    );
}

/// `aifuel_session=<token>` from the Cookie header, when present.
fn session_token(request: &Request) -> Option<String> {
    let header = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Cookie"))?
        .value
        .as_str();
    for part in header.split(';') {
        if let Some((name, value)) = part.trim().split_once('=')
            && name == COOKIE_NAME
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn session_cookie(request: &Request, token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; HttpOnly; Path=/; SameSite=Lax{}",
        secure_suffix(request)
    )
}

fn expired_cookie(request: &Request) -> String {
    format!(
        "{COOKIE_NAME}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0{}",
        secure_suffix(request)
    )
}

/// `; Secure` when the request reports a TLS front - the documented
/// reverse-proxy shape - so the cookie never travels plain HTTP there.
/// Direct-HTTP deployments omit it or browsers would refuse to store it.
fn secure_suffix(request: &Request) -> &'static str {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("X-Forwarded-Proto"))
        .filter(|header| header.value.as_str().eq_ignore_ascii_case("https"))
        .map(|_| "; Secure")
        .unwrap_or("")
}

fn respond_json(request: Request, status: u16, value: &impl Serialize) {
    respond_json_with_cookie(request, status, value, None)
}

fn respond_json_with_cookie(
    request: Request,
    status: u16,
    value: &impl Serialize,
    cookie: Option<String>,
) {
    let body = match serde_json::to_vec_pretty(value) {
        Ok(body) => body,
        Err(_) => b"{\"error\": \"could not serialize the response\"}".to_vec(),
    };
    let mut response =
        tiny_http::Response::from_data(body).with_status_code(tiny_http::StatusCode(status));
    response.add_header(
        tiny_http::Header::from_bytes("Content-Type", "application/json; charset=utf-8")
            .expect("static content type is valid"),
    );
    response.add_header(
        tiny_http::Header::from_bytes("Cache-Control", "no-store").expect("static header is valid"),
    );
    if let Some(cookie) = cookie
        && let Ok(header) = tiny_http::Header::from_bytes("Set-Cookie", cookie)
    {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

/// Decode a JSON mutation body bounded by `MAX_BODY_BYTES` - the same
/// contract the dashboard's other admin mutations enforce, including the
/// `application/json` requirement.
fn read_json_body<T: serde::de::DeserializeOwned>(request: &mut Request) -> Result<T, String> {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str().to_string())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        let mut reader = request.as_reader().take(MAX_BODY_BYTES + 1);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        return Err("expected an application/json request body".to_owned());
    }
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|error| format!("could not read the request body: {error}"))?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err("the request body is too large".to_owned());
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid JSON body: {error}"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn expiry() -> u64 {
    now_unix() + SESSION_TTL.as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_issued_session_checks_out_until_revoked() {
        let sessions = Sessions::new();
        let token = sessions.issue();
        assert!(sessions.check(&token));
        sessions.revoke(&token);
        assert!(
            !sessions.check(&token),
            "a revoked session must not authorize"
        );
    }

    #[test]
    fn unknown_and_forged_tokens_never_authorize() {
        let sessions = Sessions::new();
        assert!(!sessions.check("aifuel_session_does_not_exist"));
        assert!(!sessions.check(""));
    }

    #[test]
    fn checking_slides_the_expiry_forward() {
        let sessions = Sessions::new();
        let token = sessions.issue();
        // Push the stored expiry close to now; the check must both pass
        // and renew it to the full TTL.
        sessions
            .inner
            .lock()
            .expect("session table mutex")
            .insert(token.clone(), now_unix() + 10);
        assert!(sessions.check(&token));
        let after = *sessions
            .inner
            .lock()
            .expect("session table mutex")
            .get(&token)
            .expect("the session exists");
        assert!(after >= now_unix() + SESSION_TTL.as_secs() - 1);
    }

    #[test]
    fn an_expired_session_is_dropped_not_extended() {
        let sessions = Sessions::new();
        let token = sessions.issue();
        sessions
            .inner
            .lock()
            .expect("session table mutex")
            .insert(token.clone(), now_unix() - 1);
        assert!(
            !sessions.check(&token),
            "a stale session must not authorize"
        );
        assert!(
            !sessions
                .inner
                .lock()
                .expect("session table mutex")
                .contains_key(&token),
            "the stale entry should be evicted"
        );
    }
}
