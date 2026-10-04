use aifuel_app::MonitoringFacade;
use aifuel_core::{StatusCollector, StatusReport};
use serde::Serialize;
use std::io::{Read, Write};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use tiny_http::{Header, ListenAddr, Method, Request, Response, Server, StatusCode};

mod auth;
mod ui;

/// Mutation request bodies are small by contract (an integration id plus a
/// pasted key); anything larger is rejected rather than buffered.
const MAX_BODY_BYTES: u64 = 8 * 1024;

pub fn serve<C>(
    host: &str,
    port: u16,
    open_browser: bool,
    facade: MonitoringFacade<C>,
) -> Result<(), String>
where
    C: StatusCollector + 'static,
{
    let server = Server::http(format!("{host}:{port}"))
        .map_err(|error| format!("could not start dashboard server: {error}"))?;
    let address = server.server_addr().to_string();
    // A non-loopback bind is remote mode: the dashboard, credential
    // mutations, and the /v1 gateway become reachable off-host, so an
    // Admin Credential must be in force before the socket answers.
    let remote = match server.server_addr() {
        ListenAddr::IP(address) => !address.ip().is_loopback(),
        #[allow(unreachable_patterns)]
        _ => true,
    };
    if remote && !aifuel::admin::configured() {
        return Err(format!(
            "refusing to bind {address}: exposing the dashboard without a sign-in; \
             set an admin password first (`aifuel auth set-admin` or {})",
            aifuel::admin::ADMIN_PASSWORD_ENV
        ));
    }
    let url = format!("http://{address}");
    if open_browser {
        let browser_url = url.clone();
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(300));
            open_url(&browser_url);
        });
    }
    println!("aifuel dashboard: {url}");
    println!("Press Ctrl-C to stop.");
    std::io::stdout()
        .flush()
        .map_err(|error| format!("could not flush dashboard address: {error}"))?;

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start dashboard runtime: {error}"))?;

    // The `/v1` gateway shares this listener. A failure to build the
    // execution surface (for example an unreadable providers.json) degrades
    // it to an answered error - monitoring keeps working.
    let gateway = match aifuel::gateway::Gateway::new() {
        Ok(gateway) => Some(gateway),
        Err(error) => {
            eprintln!("aifuel: /v1 gateway unavailable: {error}");
            None
        }
    };

    // One thread per request: a held-open `/v1` SSE stream must never block
    // `/api/usage` or the static assets behind the same listener.
    let shared = Arc::new(Shared {
        facade,
        runtime,
        gateway,
        sessions: auth::Sessions::new(),
        remote,
    });
    for request in server.incoming_requests() {
        let shared = Arc::clone(&shared);
        let _ = thread::Builder::new()
            .name("aifuel-http".to_owned())
            .spawn(move || dispatch(&shared, request));
    }
    Ok(())
}

/// Server state shared by every request thread.
struct Shared<C: StatusCollector> {
    facade: MonitoringFacade<C>,
    runtime: tokio::runtime::Runtime,
    gateway: Option<aifuel::gateway::Gateway>,
    /// Live Dashboard Admin Sessions; empty while none have signed in.
    sessions: auth::Sessions,
    /// Whether the bound address is reachable off-host. Remote mode swaps
    /// the loopback guards for the Admin Session and the closed `/v1`
    /// posture.
    remote: bool,
}

fn dispatch<C>(shared: &Shared<C>, mut request: Request)
where
    C: StatusCollector,
{
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or(request.url())
        .to_owned();

    // The `/v1` tier speaks bearer Gateway Keys on every bind, never
    // Admin Sessions. Loopback keeps the DNS-rebinding and loopback-Origin
    // guards for local browser apps; remote mode drops them - non-browser
    // clients and proxied traffic are the intended callers - but fails
    // closed while no usable key exists, so the anonymous posture never
    // reaches a public socket.
    if path.starts_with("/v1") {
        if shared.remote {
            if !aifuel::gateway::has_active_keys() {
                drain_body(&mut request);
                aifuel::gateway::respond_error(
                    request,
                    503,
                    "no gateway keys are issued yet; sign in to the dashboard and create one under API Keys",
                    "server_error",
                );
                return;
            }
        } else if !(is_loopback_host(&request) && has_loopback_origin(&request)) {
            drain_body(&mut request);
            respond(
                request,
                403,
                "forbidden: cross-origin request rejected",
                "text/plain",
            );
            return;
        }
        aifuel::gateway::handle(
            request,
            shared.gateway.as_ref(),
            &shared.facade,
            &shared.runtime,
        );
        return;
    }

    // Public tier: the Admin Session endpoints answer before any
    // credential check - they are how a session is earned.
    if auth::claims(request.method(), &path) {
        auth::handle(request, &shared.sessions);
        return;
    }
    if request.method() == &Method::Get && path == "/healthz" {
        respond(
            request,
            200,
            "{\"ok\":true}",
            "application/json; charset=utf-8",
        );
        return;
    }
    // Compiled static assets carry no secrets and the sign-in page needs
    // them before a session exists, so /ui/ stays public on every bind.
    if request.method() == &Method::Get && path.starts_with("/ui/") {
        match ui::asset(&path) {
            Some(asset) => respond_bytes(request, 200, asset.body, asset.content_type),
            None => {
                drain_body(&mut request);
                respond(request, 404, "not found", "text/plain");
            }
        }
        return;
    }

    // Management tier: pages, /api/*, everything else. Loopback keeps the
    // strict local-request guard; remote mode drops the loopback-Host
    // rule but still rejects cross-site browser traffic, and the Admin
    // Session is the real boundary on a public bind.
    let permitted = if shared.remote {
        same_site_request(&request)
    } else {
        is_local_request(&request)
    };
    if !permitted {
        drain_body(&mut request);
        respond(
            request,
            403,
            "forbidden: cross-origin request rejected",
            "text/plain",
        );
        return;
    }
    if aifuel::admin::configured() && !auth::authenticated(&request, &shared.sessions) {
        if request.method() == &Method::Get && ui::page(&path).is_some() {
            respond(request, 200, ui::login_page(), "text/html; charset=utf-8");
        } else {
            let mut request = request;
            drain_body(&mut request);
            respond_json(
                request,
                401,
                &serde_json::json!({"error": "admin sign-in required"}),
            );
        }
        return;
    }
    if path.starts_with("/api/gateway") {
        // The gateway admin surface mutates local state (downstream
        // keys, routes) and reads request logs, so it lives in the
        // management tier rather than the relaxed `/v1` policy.
        aifuel::gateway::handle_admin(request, shared.gateway.as_ref());
        return;
    }
    handle_request(request, &shared.facade, &shared.runtime);
}

fn handle_request<C>(
    request: Request,
    facade: &MonitoringFacade<C>,
    runtime: &tokio::runtime::Runtime,
) where
    C: StatusCollector,
{
    let force = request
        .url()
        .split('?')
        .nth(1)
        .is_some_and(|query| query.split('&').any(|part| part == "force=1"));
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or(request.url())
        .to_owned();
    // GET pages render the shared shell; GET /ui/* serves the embedded
    // static assets (styles, scripts, fonts). Both lookups answer with
    // compiled-in content - a miss falls through to the API table.
    if matches!(request.method(), Method::Get) {
        if let Some(html) = ui::page(&path) {
            respond(request, 200, html, "text/html; charset=utf-8");
            return;
        }
        if let Some(asset) = ui::asset(&path) {
            respond_bytes(request, 200, asset.body, asset.content_type);
            return;
        }
        if path.starts_with("/ui/") {
            let mut request = request;
            drain_body(&mut request);
            respond(request, 404, "not found", "text/plain");
            return;
        }
    }
    match (request.method(), path.as_str()) {
        (&Method::Get, "/api/usage") => {
            let report = runtime.block_on(facade.status(force));
            respond_json(request, 200, &report);
        }
        (&Method::Get, "/api/usage/stream") => {
            let report = runtime.block_on(facade.status(force));
            respond_stream(request, &report);
        }
        // The Connect panel mirrors `aifuel auth`: it reports each API-key
        // integration's credential source and mutates the local Credential
        // Store. Mutations are POST-only; the server binds loopback by
        // default and `is_local_request` above rejects cross-origin and
        // cross-site browser traffic, which is the CSRF boundary for these
        // endpoints. Binding a non-loopback host (`--host 0.0.0.0`) widens
        // who can reach the form - only the loopback Host/Origin checks
        // stand between a reachable socket and the credential store, so
        // treat any non-loopback bind as remote exposure of key submission.
        (&Method::Get, "/api/auth") => match aifuel::connect::entries() {
            Ok(integrations) => respond_json(
                request,
                200,
                &serde_json::json!({ "integrations": integrations }),
            ),
            Err(error) => respond_json(request, 500, &serde_json::json!({ "error": error })),
        },
        (&Method::Post, "/api/auth/set-key") => handle_set_key(request),
        (&Method::Post, "/api/auth/remove") => handle_remove(request),
        // The sidebar's Quit button. Answer first, then exit on a short
        // delay so the response reaches the browser before the socket dies.
        (&Method::Post, "/api/shutdown") => {
            respond(request, 200, "{\"ok\":true}", "application/json; charset=utf-8");
            thread::spawn(|| {
                thread::sleep(std::time::Duration::from_millis(120));
                std::process::exit(0);
            });
        }
        _ => {
            let mut request = request;
            drain_body(&mut request);
            respond(request, 404, "not found", "text/plain")
        }
    }
}

#[derive(serde::Deserialize)]
struct SetKeyBody {
    integration: String,
    key: String,
}

#[derive(serde::Deserialize)]
struct RemoveBody {
    credential: String,
}

/// `POST /api/auth/set-key`: store the submitted key as the integration's
/// Managed Credential. The key travels in the request body only - responses
/// never carry it back.
fn handle_set_key(mut request: Request) {
    let body: SetKeyBody = match read_json_body(&mut request) {
        Ok(body) => body,
        Err(error) => {
            return respond_json(request, 400, &serde_json::json!({ "error": error }));
        }
    };
    match aifuel::connect::store_key(&body.integration, &body.key) {
        Ok(()) => respond_json(request, 200, &serde_json::json!({ "ok": true })),
        Err(error) => respond_json(request, 400, &serde_json::json!({ "error": error })),
    }
}

/// `POST /api/auth/remove`: delete the named Managed Credential, answering
/// the same warnings `aifuel auth remove` prints.
fn handle_remove(mut request: Request) {
    let body: RemoveBody = match read_json_body(&mut request) {
        Ok(body) => body,
        Err(error) => {
            return respond_json(request, 400, &serde_json::json!({ "error": error }));
        }
    };
    match aifuel::connect::remove_credential(&body.credential) {
        Ok(warnings) => respond_json(
            request,
            200,
            &serde_json::json!({ "ok": true, "warnings": warnings }),
        ),
        Err(error) => respond_json(request, 400, &serde_json::json!({ "error": error })),
    }
}

/// Decode a JSON mutation body. Requiring `application/json` is part of the
/// CSRF posture: a cross-site HTML form can only post form-encodings, so
/// forged submissions fail here even before the origin checks.
fn read_json_body<T: serde::de::DeserializeOwned>(request: &mut Request) -> Result<T, String> {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str().to_string())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        drain_body(request);
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

/// Consume a rejected request's body (bounded) before the response: closing
/// a socket while the kernel still holds unread request bytes can RST the
/// connection before the client reads the rejection.
fn drain_body(request: &mut Request) {
    let mut reader = request.as_reader().take(MAX_BODY_BYTES + 1);
    let _ = std::io::copy(&mut reader, &mut std::io::sink());
}

fn respond_json(request: Request, status: u16, value: &impl Serialize) {
    match serde_json::to_string_pretty(value) {
        Ok(body) => respond(request, status, body, "application/json; charset=utf-8"),
        Err(error) => respond(request, 500, error.to_string(), "text/plain"),
    }
}

fn respond_stream(request: Request, report: &StatusReport) {
    let mut body = String::new();
    body.push_str(
        &serde_json::json!({
            "providers_expected": report.providers.iter().map(|provider| provider.key.as_str()).collect::<Vec<_>>(),
            "discovery_errors": report.discovery_errors,
        })
        .to_string(),
    );
    body.push('\n');
    for provider in &report.providers {
        body.push_str(&serde_json::json!({"provider": provider}).to_string());
        body.push('\n');
    }
    body.push_str(
        &serde_json::json!({
            "done": true,
            "generated_at": report.generated_at,
        })
        .to_string(),
    );
    body.push('\n');
    respond(request, 200, body, "application/x-ndjson; charset=utf-8");
}

fn respond(request: Request, status: u16, body: impl Into<String>, content_type: &str) {
    let body = body.into();
    let mut response = Response::from_data(body.into_bytes()).with_status_code(StatusCode(status));
    response.add_header(
        Header::from_bytes("Content-Type", content_type).expect("static content type is valid"),
    );
    response.add_header(
        Header::from_bytes("Cache-Control", "no-store").expect("static header is valid"),
    );
    let _ = request.respond(response);
}

fn respond_bytes(request: Request, status: u16, body: &[u8], content_type: &str) {
    let mut response = Response::from_data(body.to_vec()).with_status_code(StatusCode(status));
    response.add_header(
        Header::from_bytes("Content-Type", content_type).expect("static content type is valid"),
    );
    response.add_header(
        Header::from_bytes("Cache-Control", "no-store").expect("static header is valid"),
    );
    let _ = request.respond(response);
}

fn is_local_request(request: &Request) -> bool {
    is_loopback_host(request) && same_site_request(request)
}

/// Whether a browser-marked request is same-site relative to its target:
/// an absent, `null`, or host-matching Origin, and no cross-site or
/// same-site `Sec-Fetch-Site` marker. Host-agnostic - the caller picks
/// whether the Host itself must also be loopback.
fn same_site_request(request: &Request) -> bool {
    let host = request_host(request);
    if let Some(origin) = origin_host(request) {
        if origin != "null" && origin != host {
            return false;
        }
    }
    !request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Sec-Fetch-Site"))
        .is_some_and(|header| matches!(header.value.as_str(), "cross-site" | "same-site"))
}

/// Whether the request targets the loopback listener: the Host header is a
/// loopback name or absent. This is the DNS-rebinding boundary.
fn is_loopback_host(request: &Request) -> bool {
    matches!(
        request_host(request).as_str(),
        "127.0.0.1" | "localhost" | "[::1]" | "::1"
    )
}

/// Whether the request's Origin header is absent or a loopback origin -
/// local browser apps (Open WebUI, NextChat) served from another loopback
/// port, and non-browser clients that send no Origin at all.
fn has_loopback_origin(request: &Request) -> bool {
    origin_host(request).is_none_or(|origin| {
        origin == "null" || matches!(origin.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1")
    })
}

fn request_host(request: &Request) -> String {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Host"))
        .map(|header| host_without_port(header.value.as_str()))
        .unwrap_or_else(|| "127.0.0.1".to_owned())
}

fn origin_host(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Origin"))
        .map(|header| header.value.as_str())
        .map(|origin| {
            origin
                .split_once("://")
                .map(|(_, value)| host_without_port(value))
                .unwrap_or_default()
        })
}

fn host_without_port(value: &str) -> String {
    if let Some(value) = value.strip_prefix('[') {
        return value.split(']').next().unwrap_or("").to_owned();
    }
    value.split(':').next().unwrap_or("").to_owned()
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let command = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    let _ = Command::new(command.0).args(command.1).spawn();
}
