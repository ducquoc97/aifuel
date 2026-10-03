use aifuel_app::MonitoringFacade;
use aifuel_core::{StatusCollector, StatusReport};
use serde::Serialize;
use std::io::{Read, Write};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const INDEX_HTML: &str = include_str!("../../../src/index.html");
const DASHBOARD_CSS: &str = include_str!("../../../src/dashboard.css");
const CONNECT_CSS: &str = include_str!("../../../src/connect.css");
const CONNECT_JS: &str = include_str!("../../../src/connect.js");

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
}

fn dispatch<C>(shared: &Shared<C>, mut request: Request)
where
    C: StatusCollector,
{
    // The Host check is the DNS-rebinding boundary every route shares.
    if !is_loopback_host(&request) {
        drain_body(&mut request);
        respond(
            request,
            403,
            "forbidden: cross-origin request rejected",
            "text/plain",
        );
        return;
    }
    let path = request.url().split('?').next().unwrap_or(request.url());
    if path.starts_with("/v1") {
        // `/v1` is an API surface for local apps: browser clients on other
        // loopback ports send a loopback Origin, so the dashboard's
        // same-origin rule would reject the intended traffic. Loopback
        // origins pass; remote origins are still rejected. Bearer shape is
        // enforced by the gateway itself, never ambient cookies, so there
        // is no dashboard credential to forge here.
        if !has_loopback_origin(&request) {
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
    if path.starts_with("/api/gateway") {
        // The gateway admin surface mutates local state (downstream keys)
        // and reads request logs, so it keeps the dashboard's strict
        // same-origin guard rather than the relaxed `/v1` policy.
        if !is_local_request(&request) {
            drain_body(&mut request);
            respond(
                request,
                403,
                "forbidden: cross-origin request rejected",
                "text/plain",
            );
            return;
        }
        aifuel::gateway::handle_admin(request, shared.gateway.as_ref());
        return;
    }
    if !is_local_request(&request) {
        drain_body(&mut request);
        respond(
            request,
            403,
            "forbidden: cross-origin request rejected",
            "text/plain",
        );
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
    let path = request.url().split('?').next().unwrap_or(request.url());
    match (request.method(), path) {
        (&Method::Get, "/") => respond(request, 200, INDEX_HTML, "text/html; charset=utf-8"),
        (&Method::Get, "/dashboard.css") => {
            respond(request, 200, DASHBOARD_CSS, "text/css; charset=utf-8")
        }
        (&Method::Get, "/connect.css") => {
            respond(request, 200, CONNECT_CSS, "text/css; charset=utf-8")
        }
        (&Method::Get, "/connect.js") => respond(
            request,
            200,
            CONNECT_JS,
            "application/javascript; charset=utf-8",
        ),
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

fn is_local_request(request: &Request) -> bool {
    let host = request_host(request);
    if !is_loopback_host(&request) {
        return false;
    }
    if let Some(origin) = origin_host(request) {
        if origin != "null" && origin != host {
            return false;
        }
    }
    if request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Sec-Fetch-Site"))
        .is_some_and(|header| matches!(header.value.as_str(), "cross-site" | "same-site"))
    {
        return false;
    }
    true
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
