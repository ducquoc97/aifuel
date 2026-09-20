use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, Url, redirect::Policy};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub(super) struct RemoteEndpoint {
    pub(super) url: Url,
    pub(super) client: Client,
}

pub(super) async fn connect_endpoint(
    value: &str,
    bearer_token: Option<&str>,
    connect_timeout: Duration,
) -> Result<RemoteEndpoint, &'static str> {
    let url = Url::parse(value).map_err(|_| "remote MCP endpoint URL is invalid")?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("remote MCP endpoint URL cannot contain user information or a fragment");
    }

    let host = url
        .host_str()
        .ok_or("remote MCP endpoint URL must include a host")?;
    let default_headers = if let Some(token) = bearer_token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "remote MCP bearer token could not be configured")?;
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value);
        Some(headers)
    } else {
        None
    };
    let mut builder = Client::builder()
        .connect_timeout(connect_timeout)
        .redirect(Policy::none());
    if let Some(default_headers) = default_headers {
        builder = builder.default_headers(default_headers);
    }

    match url.scheme() {
        "https" => {}
        "http" => {
            builder = builder.no_proxy();
            if host.eq_ignore_ascii_case("localhost") {
                let port = url
                    .port_or_known_default()
                    .ok_or("remote MCP loopback endpoint has no port")?;
                let addresses: Vec<SocketAddr> =
                    tokio::time::timeout(connect_timeout, tokio::net::lookup_host((host, port)))
                        .await
                        .map_err(|_| "remote MCP loopback endpoint resolution timed out")?
                        .map_err(|_| "remote MCP loopback endpoint could not be resolved")?
                        .collect();
                if addresses.is_empty() || addresses.iter().any(|addr| !addr.ip().is_loopback()) {
                    return Err("remote MCP HTTP endpoint must resolve only to loopback addresses");
                }
                let pinned: Vec<_> = addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address.ip(), 0))
                    .collect();
                builder = builder.resolve_to_addrs(host, &pinned);
            } else if !host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
            {
                return Err("remote MCP HTTP endpoint must use a loopback address");
            }
        }
        _ => return Err("remote MCP endpoint must use HTTPS or loopback HTTP"),
    }

    let client = builder
        .build()
        .map_err(|_| "remote MCP HTTP client could not be created")?;
    Ok(RemoteEndpoint { url, client })
}
