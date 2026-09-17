use aifuel_core::StatusReport;
use aifuel_providers::{CollectionConfig, UsageService};
use std::io::Write;
use std::process::Command;
use std::thread;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const INDEX_HTML: &str = include_str!("../../../src/index.html");
const DASHBOARD_CSS: &str = include_str!("../../../src/dashboard.css");

pub fn serve(host: &str, port: u16, open_browser: bool) -> Result<(), String> {
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

    let context = aifuel_providers::DiscoveryContext::from_environment()
        .map_err(|error| error.to_string())?;
    let service = UsageService::new(context.home_dir(), CollectionConfig::from_environment())?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start dashboard runtime: {error}"))?;

    for request in server.incoming_requests() {
        if !is_local_request(&request) {
            respond(
                request,
                403,
                "forbidden: cross-origin request rejected",
                "text/plain",
            );
            continue;
        }
        handle_request(request, &service, &runtime);
    }
    Ok(())
}

fn handle_request(request: Request, service: &UsageService, runtime: &tokio::runtime::Runtime) {
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
        (&Method::Get, "/api/usage") => {
            let report = runtime.block_on(service.status(force));
            respond_json(request, &report);
        }
        (&Method::Get, "/api/usage/stream") => {
            let report = runtime.block_on(service.status(force));
            respond_stream(request, &report);
        }
        _ => respond(request, 404, "not found", "text/plain"),
    }
}

fn respond_json(request: Request, report: &StatusReport) {
    match serde_json::to_string_pretty(report) {
        Ok(body) => respond(request, 200, body, "application/json; charset=utf-8"),
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
    let host = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Host"))
        .map(|header| host_without_port(header.value.as_str()))
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        return false;
    }
    if let Some(origin) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Origin"))
        .map(|header| header.value.as_str())
    {
        let origin_host = origin
            .split_once("://")
            .map(|(_, value)| host_without_port(value))
            .unwrap_or_default();
        if origin != "null" && origin_host != host {
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
