//! Embedded dashboard UI: the Vite/React single-page app built into
//! ui/dist. Everything the browser downloads is compiled into the
//! binary - no runtime file reads, no CDN.
//!
//! Rebuild the bundle with `cd ui && pnpm build` before `cargo build`;
//! build.rs produces it automatically when ui/dist is missing.

use rust_embed::RustEmbed;
use std::borrow::Cow;

/// The built Vite bundle: index.html, hashed assets under assets/, and
/// the font files under fonts/. Paths inside the archive have no
/// leading slash.
#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
struct Dist;

/// A static asset's embedded bytes and its Content-Type.
pub struct Asset {
    pub body: Cow<'static, [u8]>,
    pub content_type: Cow<'static, str>,
}

/// The SPA entry document for a dashboard page route, or None when the
/// path is not a page. Every app route serves the same index.html -
/// React Router picks the view client-side, including the sign-in
/// screen when the session check fails.
pub fn page(path: &str) -> Option<String> {
    match path {
        "/" | "/login" | "/credentials" | "/gateway" | "/gateway/connect"
        | "/gateway/providers" | "/gateway/routes" | "/gateway/keys" | "/gateway/logs" => {
            Some(index_html())
        }
        _ => None,
    }
}

/// Served for management page GETs while an Admin Credential is
/// configured and the caller holds no session. It is the same SPA
/// document - the app lands on its /login route once the session check
/// returns 401.
pub fn login_page() -> String {
    index_html()
}

/// The embedded body for a /ui/ asset path, or None (404). Vite emits
/// with base /ui/, so /ui/assets/… and /ui/fonts/… map onto the archive
/// with the prefix stripped.
pub fn asset(path: &str) -> Option<Asset> {
    let rel = path.strip_prefix("/ui/")?;
    let file = Dist::get(rel)?;
    Some(Asset {
        body: file.data,
        content_type: file.metadata.mimetype().to_owned().into(),
    })
}

/// index.html with the package version stamped into its meta tag; the
/// shell reads it for the sidebar footer.
fn index_html() -> String {
    let file = Dist::get("index.html").expect("ui/dist/index.html is embedded");
    let html = std::str::from_utf8(&file.data).expect("index.html is utf-8");
    html.replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
}
