//! Embedded dashboard UI: the shared shell, per-page fragments, and
//! static assets under /ui/. Everything the browser downloads is
//! compiled into the binary - no runtime file reads, no CDN.

const SHELL_HTML: &str = include_str!("../../../../src/ui/shell.html");
const FONTS_CSS: &str = include_str!("../../../../src/ui/fonts.css");
const APP_CSS: &str = include_str!("../../../../src/ui/app.css");
const APP_JS: &str = include_str!("../../../../src/ui/app.js");
const USAGE_HTML: &str = include_str!("../../../../src/ui/usage.html");
const USAGE_CSS: &str = include_str!("../../../../src/ui/usage.css");
const USAGE_JS: &str = include_str!("../../../../src/ui/usage.js");
const CREDENTIALS_HTML: &str = include_str!("../../../../src/ui/credentials.html");
const CREDENTIALS_CSS: &str = include_str!("../../../../src/ui/credentials.css");
const CREDENTIALS_JS: &str = include_str!("../../../../src/ui/credentials.js");
const GATEWAY_HTML: &str = include_str!("../../../../src/ui/gateway.html");
const GATEWAY_CSS: &str = include_str!("../../../../src/ui/gateway.css");
const LOGIN_HTML: &str = include_str!("../../../../src/ui/login.html");

/// The gateway page's scripts, loaded in order under /ui/gateway/.
/// Shared helpers first, section modules next, the entry point last.
const GATEWAY_SCRIPTS: &[(&str, &str)] = &[
    ("shared.js", include_str!("../../../../src/ui/gateway/shared.js")),
    ("connect.js", include_str!("../../../../src/ui/gateway/connect.js")),
    ("providers.js", include_str!("../../../../src/ui/gateway/providers.js")),
    ("picker.js", include_str!("../../../../src/ui/gateway/picker.js")),
    ("routes.js", include_str!("../../../../src/ui/gateway/routes.js")),
    ("keys.js", include_str!("../../../../src/ui/gateway/keys.js")),
    ("logs.js", include_str!("../../../../src/ui/gateway/logs.js")),
    ("main.js", include_str!("../../../../src/ui/gateway/main.js")),
];

const GATEWAY_PAGE_SCRIPTS: &str = r#"<script src="/ui/gateway/shared.js" defer></script>
<script src="/ui/gateway/connect.js" defer></script>
<script src="/ui/gateway/providers.js" defer></script>
<script src="/ui/gateway/picker.js" defer></script>
<script src="/ui/gateway/routes.js" defer></script>
<script src="/ui/gateway/keys.js" defer></script>
<script src="/ui/gateway/logs.js" defer></script>
<script src="/ui/gateway/main.js" defer></script>"#;

/// Bundled fonts served under /ui/fonts/. Be Vietnam Pro is the app
/// typeface; ms-outlined is a pyftsubset'd Material Symbols subset.
const FONTS: &[(&str, &[u8])] = &[
    ("bevietnampro-400-latin.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-400-latin.woff2")),
    ("bevietnampro-400-latin-ext.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-400-latin-ext.woff2")),
    ("bevietnampro-400-vietnamese.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-400-vietnamese.woff2")),
    ("bevietnampro-500-latin.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-500-latin.woff2")),
    ("bevietnampro-500-latin-ext.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-500-latin-ext.woff2")),
    ("bevietnampro-500-vietnamese.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-500-vietnamese.woff2")),
    ("bevietnampro-600-latin.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-600-latin.woff2")),
    ("bevietnampro-600-latin-ext.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-600-latin-ext.woff2")),
    ("bevietnampro-600-vietnamese.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-600-vietnamese.woff2")),
    ("bevietnampro-700-latin.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-700-latin.woff2")),
    ("bevietnampro-700-latin-ext.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-700-latin-ext.woff2")),
    ("bevietnampro-700-vietnamese.woff2", include_bytes!("../../../../src/ui/fonts/bevietnampro-700-vietnamese.woff2")),
    ("ms-outlined.woff2", include_bytes!("../../../../src/ui/fonts/ms-outlined.woff2")),
];

/// Header action buttons injected into the shell per page.
const USAGE_ACTIONS: &str = r#"<span class="status-text" id="updated" aria-live="polite">loading&hellip;</span>
<label class="toggle-wrap" aria-label="Auto-refresh">
  <input type="checkbox" id="auto-refresh-btn" onchange="toggleAutoRefresh()">
  <span class="toggle-track"><span class="toggle-thumb"></span></span>
  Auto-refresh
</label>
<button class="btn" id="refresh-btn" onclick="load(true, false)" aria-label="Refresh data now">
  <span class="ms ms-refresh" aria-hidden="true"></span>Refresh
</button>"#;

const GATEWAY_ACTIONS: &str = r#"<span class="status-text" id="updated" aria-live="polite">loading&hellip;</span>
<button class="btn" id="refresh-btn" onclick="refreshSection()" aria-label="Refresh this page">
  <span class="ms ms-refresh" aria-hidden="true"></span>Refresh
</button>"#;

const TEXT_CSS: &str = "text/css; charset=utf-8";
const TEXT_JS: &str = "application/javascript; charset=utf-8";

/// A static asset's embedded bytes and its Content-Type.
pub struct Asset {
    pub body: &'static [u8],
    pub content_type: &'static str,
}

/// Rendered HTML for a page route, or None when the path is not a page.
/// The shell is constant; each route fills its {{PLACEHOLDER}} slots.
pub fn page(path: &str) -> Option<String> {
    page_for(path).map(|page| render_page(&page))
}

/// The standalone sign-in page served for management GETs while an Admin
/// Credential is configured and the caller holds no session. It is not a
/// shell page - no sidebar, no nav - and reloading after login serves
/// the real page for the same URL.
pub fn login_page() -> String {
    LOGIN_HTML.replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
}

/// The embedded body for a /ui/ asset path, or None (404).
pub fn asset(path: &str) -> Option<Asset> {
    let (body, content_type): (&[u8], &str) = match path {
        "/ui/fonts.css" => (FONTS_CSS.as_bytes(), TEXT_CSS),
        "/ui/app.css" => (APP_CSS.as_bytes(), TEXT_CSS),
        "/ui/app.js" => (APP_JS.as_bytes(), TEXT_JS),
        "/ui/usage.css" => (USAGE_CSS.as_bytes(), TEXT_CSS),
        "/ui/usage.js" => (USAGE_JS.as_bytes(), TEXT_JS),
        "/ui/credentials.css" => (CREDENTIALS_CSS.as_bytes(), TEXT_CSS),
        "/ui/credentials.js" => (CREDENTIALS_JS.as_bytes(), TEXT_JS),
        "/ui/gateway.css" => (GATEWAY_CSS.as_bytes(), TEXT_CSS),
        p if p.starts_with("/ui/gateway/") => GATEWAY_SCRIPTS
            .iter()
            .find(|(name, _)| *name == &p["/ui/gateway/".len()..])
            .map(|(_, src)| (src.as_bytes(), TEXT_JS))?,
        p if p.starts_with("/ui/fonts/") => FONTS
            .iter()
            .find(|(name, _)| *name == &p["/ui/fonts/".len()..])
            .map(|(_, data)| (*data, "font/woff2"))?,
        _ => return None,
    };
    Some(Asset { body, content_type })
}

/// One renderable page: the shell is constant, these fields fill its
/// {{PLACEHOLDER}} slots. `section` names the visible gateway section.
struct Page {
    title: &'static str,
    icon: &'static str,
    description: &'static str,
    active: &'static str,
    section: &'static str,
    header_actions: &'static str,
    content: &'static str,
    page_css: &'static str,
    /// Ready-made <script> markup; a page with several module files
    /// lists each tag here in load order.
    page_js: &'static str,
}

fn render_page(page: &Page) -> String {
    let page_css = if page.page_css.is_empty() {
        String::new()
    } else {
        format!(r#"<link rel="stylesheet" href="{}"/>"#, page.page_css)
    };
    SHELL_HTML
        .replace("{{TITLE}}", page.title)
        .replace("{{ICON}}", page.icon)
        .replace("{{DESCRIPTION}}", page.description)
        .replace("{{ACTIVE}}", page.active)
        .replace("{{SECTION}}", page.section)
        .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
        .replace("{{HEADER_ACTIONS}}", page.header_actions)
        .replace("{{PAGE_CSS}}", &page_css)
        .replace("{{PAGE_JS}}", page.page_js)
        .replace("{{CONTENT}}", page.content)
}

/// The five gateway console pages share one HTML fragment; the route picks
/// which section is visible and which sidebar item is active.
fn gateway_page(
    section: &'static str,
    title: &'static str,
    icon: &'static str,
    description: &'static str,
) -> Page {
    Page {
        title,
        icon,
        description,
        active: section,
        section,
        header_actions: GATEWAY_ACTIONS,
        content: GATEWAY_HTML,
        page_css: "/ui/gateway.css",
        page_js: GATEWAY_PAGE_SCRIPTS,
    }
}

fn page_for(path: &str) -> Option<Page> {
    Some(match path {
        "/" => Page {
            title: "Usage",
            icon: "speed",
            description: "Remaining quota across your AI coding providers, soonest reset first.",
            active: "usage",
            section: "",
            header_actions: USAGE_ACTIONS,
            content: USAGE_HTML,
            page_css: "/ui/usage.css",
            page_js: r#"<script src="/ui/usage.js" defer></script>"#,
        },
        "/credentials" => Page {
            title: "Credentials",
            icon: "key",
            description: "Store API keys or session credentials for integrations.",
            active: "credentials",
            section: "",
            header_actions: "",
            content: CREDENTIALS_HTML,
            page_css: "/ui/credentials.css",
            page_js: r#"<script src="/ui/credentials.js" defer></script>"#,
        },
        "/gateway" | "/gateway/connect" => gateway_page(
            "connect",
            "Connect",
            "api",
            "Point OpenAI- or Anthropic-compatible clients at this gateway.",
        ),
        "/gateway/providers" => gateway_page(
            "providers",
            "Providers",
            "dns",
            "Executable integrations the gateway can route requests to.",
        ),
        "/gateway/routes" => gateway_page(
            "routes",
            "Routes",
            "route",
            "Named aliases and failover combos stored in gateway.json.",
        ),
        "/gateway/keys" => gateway_page(
            "keys",
            "API Keys",
            "vpn_key",
            "Gateway keys that authenticate clients, with model allowlists.",
        ),
        "/gateway/logs" => gateway_page(
            "logs",
            "Request Log",
            "receipt_long",
            "Latest gateway requests, newest first.",
        ),
        _ => return None,
    })
}
