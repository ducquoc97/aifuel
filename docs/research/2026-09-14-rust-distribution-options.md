# Rust distribution options for AI Fuel

- Date: 2026-09-14
- Research issue: [#9](https://github.com/ducquoc97/aifuel/issues/9)
- Architecture decision: [#17](https://github.com/ducquoc97/aifuel/issues/17)
- Status: evidence and recommendation only. This note does not approve an architecture.

## Question and constraints

The revised issue asks which Rust distribution options can ship one AI Fuel binary per target, embed the dashboard, and avoid a separately installed language runtime while allowing build-time crates.

The explicit prerequisites are an installed and authenticated Codex or Claude CLI for agent launching, and an installed browser for the dashboard. The binary should not install, bundle, or authenticate either prerequisite.

"Zero dependency" is interpreted here as zero separately installed application runtime dependencies. A Cargo dependency compiled into the release binary is allowed. Literal zero third-party crates would require reimplementing TLS, JSON, SQLite access, and HTTP parsing, which increases security and maintenance risk rather than reducing it.

## Recommendation for issue #17

Evaluate this minimal synchronous distribution as the leading option:

1. Ship one native CLI binary per target. Keep the provider catalog statically compiled and use a stable JSON output contract. Do not use dynamic plugins or shared-library providers.
2. Use `ureq` with its blocking API and Rustls. Configure the platform verifier on common desktop operating systems. Keep static `webpki-roots` as an explicit alternative for environments that require a fixed root set, with a scheduled root update.
3. Use `serde` and `serde_json` for typed provider responses and local JSON credential stores.
4. Use Rust standard library filesystem and process APIs for known provider paths and direct CLI execution. Add a small directory-convention crate only if native app cache/config locations are required.
5. Use `rusqlite` with its `bundled` feature only for the existing provider adapters that must read SQLite stores. Keep SQLite read-only, disable extension-loading features, and accept the extra C build toolchain cost.
6. Embed HTML, CSS, and icons with `include_str!` and `include_bytes!`. Serve fixed loopback routes with `tiny_http`, or a carefully bounded `std::net::TcpListener` implementation if minimizing the dependency graph is more important than avoiding a custom HTTP parser.
7. Distribute archives containing the binary, license/notice files, and checksums first. Add signed platform installers later. Do not make a WebView runtime, Python, Node, Rust, SQLite, or provider CLI part of the application distribution.

This recommendation is intentionally conditional. Issue #17 must decide whether current provider parity requires SQLite and OS keychain adapters, whether a browser dashboard is sufficient instead of a tray shell, and which target matrix is required.

## Option comparison

| Option | What is compiled into the binary | Strengths | Costs and failure modes | Assessment |
| --- | --- | --- | --- | --- |
| Rust standard library only | `std::net`, `std::fs`, `std::process`, hand-written JSON/TLS/HTTP | Small conceptual surface; no third-party license inventory | The standard library does not provide HTTPS/TLS or JSON. Reimplementing certificate validation, JSON parsing, and HTTP parsing creates high security and maintenance risk. SQLite support would still need an FFI or parser. | Reject for the real feature set. |
| Minimal synchronous crates | `ureq` + Rustls, `serde`/`serde_json`, a small local server, optional bundled SQLite | Matches the current synchronous polling model; no async runtime; no OpenSSL install; simple cross-platform process and file model | `ureq` still has a dependency graph and Rustls crypto provider. Bundled SQLite adds a C build and larger binaries. Platform certificate behavior differs by OS. | Recommended baseline to evaluate. |
| Async web stack | `reqwest` or `hyper`, `tokio`, `axum`, `serde` | Strong routing, concurrency, timeouts, streaming, and long-running task support | Much larger graph and runtime model. `reqwest` brings Hyper/Tokio layers even for a small dashboard. Async process supervision is useful only if concurrent long-running agent jobs become a primary feature. | Keep as a later option for a real job orchestrator or streaming UI. |
| Desktop WebView shell | Tauri or another native shell plus web assets and platform integration | Tray, notifications, settings windows, and native lifecycle | The shell can require WebView2 on Windows and WebKit/GTK system facilities on Linux. The Windows reference explicitly bootstraps WebView2 and VC++ runtimes. This violates the strict no-runtime-install goal unless those are accepted prerequisites. | Not the baseline. Revisit only for a native tray product. |
| External runtime helpers | Python/Node/SQLite/curl/browser helpers | Reuses existing tools quickly | Reintroduces installation and version drift, weakens one-binary claims, and makes credential/process behavior less predictable. | Reject except for the explicit installed authenticated agent CLIs and browser. |

The Rust target model supports building for a target triple with Cargo's `--target` option, and Cargo's `--locked` mode makes the dependency graph deterministic for release builds. See the [Cargo build documentation](https://doc.rust-lang.org/cargo/commands/cargo-build.html) and [Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html).

## HTTPS and TLS

### Candidate clients

- `ureq` is a blocking HTTP client, is implemented in Rust, forbids unsafe code, and keeps dependencies smaller by avoiding an async runtime. Its current documentation exposes Rustls, native-tls, platform-verifier, root-certificate, proxy, timeout, and JSON features: [ureq API and feature documentation](https://docs.rs/ureq/latest/ureq/).
- `reqwest` has a richer high-level API and a blocking mode, but its current manifest includes Hyper, Tokio, Tower, and multiple optional TLS layers. Its current default TLS feature is Rustls, while native-tls can use system TLS on Windows/macOS and OpenSSL on Linux: [reqwest TLS documentation](https://docs.rs/reqwest/latest/reqwest/tls/) and [reqwest manifest](https://raw.githubusercontent.com/seanmonstar/reqwest/master/Cargo.toml).
- The standard library has TCP sockets but no TLS client. `std::net::TcpListener` is enough for a loopback server, not for authenticated provider HTTPS: [TcpListener](https://doc.rust-lang.org/stable/std/net/struct.TcpListener.html).

For the current polling workload, blocking `ureq` is a better fit than adding Tokio. Provider calls can be bounded with an agent timeout and performed in a fixed number of worker threads. If issue #17 turns the product into a concurrent translation job runner with streaming output, the async option should be reconsidered rather than added speculatively.

### Certificate roots

There are two valid trust-store policies:

| Policy | Evidence | Security and maintenance trade-off |
| --- | --- | --- |
| Platform verifier | `rustls-platform-verifier` uses Windows certificate verification, macOS Security.framework, and the system CA bundle on Linux. Its documentation describes OS trust decisions, enterprise CA integration, and revocation support on Windows/macOS: [platform verifier documentation](https://docs.rs/rustls-platform-verifier/latest/rustls_platform_verifier/). | Best default for a desktop application. It follows local enterprise trust and OS-managed updates. Linux uses the system CA bundle through a WebPKI fallback and does not provide revocation checking, so a missing or stale Linux CA bundle must be reported clearly. It adds platform-specific APIs to the compiled dependency graph but no user-installed library. |
| Static Mozilla roots | `webpki-roots` is a static root bundle and the ureq documentation says it does not update automatically and does not provide SCT or CRL revocation behavior: [ureq root certificate guidance](https://docs.rs/ureq/latest/ureq/#root-certificates) and [webpki-roots manifest](https://raw.githubusercontent.com/rustls/webpki-roots/main/webpki-roots/Cargo.toml). | Most reproducible and independent of the host CA store. The release process must update and audit the root bundle. It does not honor enterprise-local roots and can continue trusting a root that the OS has distrusted. It is useful for controlled or frequently rebuilt container images, less ideal as the only desktop policy. |
| Native TLS/OpenSSL | `native-tls` delegates to OS TLS on Windows/macOS and commonly OpenSSL on Linux: [reqwest TLS backends](https://docs.rs/reqwest/latest/reqwest/tls/). | Familiar platform behavior, but Linux OpenSSL availability and cross-compilation make the binary less self-contained. Vendoring OpenSSL increases build and vulnerability-update responsibility. Do not select it for the baseline. |

The baseline should never disable certificate or hostname verification. Reqwest's client builder documentation explicitly warns that accepting invalid certificates or hostnames creates man-in-the-middle vulnerabilities: [reqwest TLS builder warnings](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html).

Every provider adapter should use HTTPS, a fixed endpoint allowlist, explicit timeouts, bounded response bodies, and redacted error logging. Authorization headers, cookies, refresh tokens, and child-process output containing credentials must not enter dashboard JSON or logs. ureq's own documentation warns that trace logging is not redacted: [ureq logging guidance](https://docs.rs/ureq/latest/ureq/#log-levels).

## JSON

Rust's standard library does not parse JSON. `serde` provides typed serialization and deserialization, and `serde_json` handles JSON values, typed structures, files, and streams. Both are permissively dual-licensed MIT or Apache-2.0 in their upstream manifests: [serde manifest](https://raw.githubusercontent.com/serde-rs/serde/master/serde/Cargo.toml), [serde_json manifest](https://raw.githubusercontent.com/serde-rs/json/master/Cargo.toml), and [serde_json documentation](https://docs.rs/serde_json/latest/serde_json/).

Use typed structs for stable provider fields and a bounded `serde_json::Value` adapter only where a provider schema is known to vary. Do not enable unbounded JSON recursion for provider-controlled responses without another depth or size guard. The `serde_json` manifest documents that its unbounded-depth feature can overflow the stack: [serde_json feature documentation](https://raw.githubusercontent.com/serde-rs/json/master/Cargo.toml).

The ureq `json` feature integrates with Serde, but using `serde_json` directly remains useful for local credential files and for a stable output serializer shared by the dashboard, text mode, and non-interactive callers.

## Local stores and credentials

The current repository reads provider-owned JSON files, a SQLite `state.vscdb` store, and a macOS Keychain item. That means the distribution decision is not only about an app-owned cache.

### File paths and JSON files

Use `std::fs`, `PathBuf`, and `std::env::home_dir` for known provider locations. Rust documents `std::fs` as cross-platform filesystem operations and documents the platform-specific home directory behavior, including `USERPROFILE` and `HOME`: [std::fs](https://doc.rust-lang.org/stable/std/fs/index.html) and [std::env::home_dir](https://doc.rust-lang.org/std/env/fn.home_dir.html).

For app-owned cache/config directories, either implement the small XDG/APPDATA/Application Support mapping locally or use a small directory-convention crate. `dirs` documents the mapping and is MIT or Apache-2.0: [dirs documentation](https://docs.rs/dirs/latest/dirs/). Adding it is a build-time dependency only; hand-maintaining the mapping saves a crate but creates platform maintenance work.

Provider discovery should stay side-effect-free and read only known paths. Token refresh may write back to the provider-owned file using an atomic replace, preserving permissions where the OS permits. Never copy provider tokens into an AI Fuel database or into dashboard responses.

### SQLite

`rusqlite` with `bundled` compiles SQLite into the application, avoiding a user-installed SQLite library. Its upstream documentation calls this the normal choice for applications that control their own database and notes that the feature compiles SQLite from source: [rusqlite README](https://github.com/rusqlite/rusqlite) and [rusqlite manifest](https://raw.githubusercontent.com/rusqlite/rusqlite/master/Cargo.toml). SQLite itself is public domain: [SQLite copyright](https://www.sqlite.org/copyright.html).

Trade-offs:

- Advantages: reads the existing store format, no SQLite runtime install, mature query engine, and permissive licensing.
- Costs: a native C build is now part of every target build; the current rusqlite manifest has a higher MSRV than the HTTP/JSON crates; the release CI must build and test each target with the bundled feature.
- Security boundary: open provider databases read-only, use a fixed parameterized query, do not enable SQLite extension loading, bound returned text before JSON parsing, and treat database content as untrusted local input.

If issue #17 decides that the SQLite-backed provider is out of parity scope, removing `rusqlite` materially simplifies cross-compilation. If parity is required, bundling is safer for distribution than linking to an unknown system SQLite.

### OS credential stores

Do not add a generic cross-platform secret manager just to make the binary look self-contained. The `keyring` documentation itself says applications that need precise control over which stores they use should link the specific stores instead of the all-in-one crate: [keyring documentation](https://docs.rs/keyring/latest/keyring/).

For a provider that requires an OS store, use a target-specific adapter: macOS Security.framework, Windows DPAPI/Credential Manager, or the provider's existing file store. Apple's documentation describes Keychain Services as an encrypted database for small sensitive values: [Apple Keychain Services](https://developer.apple.com/documentation/security/keychain-services). Windows DPAPI binds protected data to the user's logon credentials by default and authenticates the protected blob: [Microsoft CryptProtectData](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata).

This keeps the baseline provider adapters narrow. It also makes permissions visible: a keychain lookup can prompt or fail, while a file lookup can be denied by ACLs. Discovery must report those states as provider-specific discovery failures, not silently treat them as missing credentials.

## Dashboard assets and local server

Rust's `include_str!` embeds a UTF-8 file as a `&'static str`, and `include_bytes!` embeds arbitrary file bytes at compile time: [include_str!](https://doc.rust-lang.org/stable/core/macro.include_str.html) and [include_bytes!](https://doc.rust-lang.org/stable/core/macro.include_bytes.html). These macros remove the need to ship an HTML/CSS resource directory beside the binary.

For the current read-only dashboard, fixed routes are enough: `/`, the embedded stylesheet/icon routes, and a JSON usage endpoint. Bind only to `127.0.0.1` (or an explicitly selected loopback address), use a random port when possible, reject methods and paths outside the fixed route table, and never expose an arbitrary filesystem path. No HTTPS is needed for the loopback server if it is strictly loopback and does not serve credentials; provider HTTPS remains mandatory.

Two server choices are reasonable:

- `std::net::TcpListener` has no dependency, but a custom HTTP parser must be bounded and maintained. This is a security risk if the route set grows.
- `tiny_http` handles connections, request parsing, and transfers, and is MIT or Apache-2.0. Its upstream documentation says routing and application policy remain the caller's responsibility: [tiny_http documentation](https://docs.rs/tiny_http/latest/tiny_http/) and [tiny_http manifest](https://raw.githubusercontent.com/tiny-http/tiny-http/master/Cargo.toml).

Prefer `tiny_http` if build-time crates are accepted. Prefer a fixed std-only responder only if the human decision explicitly values the smallest dependency graph over parser reuse. `axum` is a capable alternative, but its own documentation says it is designed around Tokio and Hyper: [axum documentation](https://docs.rs/axum/latest/axum/). That is unnecessary overhead for a local polling dashboard.

Opening the browser can use the platform's existing handler through a small `std::process::Command` adapter, or the program can print the URL and avoid launching anything. The browser remains an explicit user prerequisite.

## Agent CLI process management

The dashboard and provider polling do not need to bundle Codex or Claude. For the non-interactive translation workflow, use direct process execution:

- Construct `Command::new("codex")` or `Command::new("claude")` and pass each argument separately. Rust documents that arguments are not shell-expanded and warns about non-standard Windows command decoders: [std::process::Command](https://doc.rust-lang.org/std/process/struct.Command.html).
- Set `current_dir` to the explicit project path, such as `~/Developer/envi` after safe home expansion. Do not pass a whole command string to a shell, and do not interpolate user-controlled paths into `sh -c`, `cmd /C`, or PowerShell.
- Capture stdout and stderr with size limits, redact known credential patterns, record exit status, and enforce a deadline. `Command::output` collects output and waits, while `Child` supports explicit process control: [Command output](https://doc.rust-lang.org/std/process/struct.Command.html) and [Child](https://doc.rust-lang.org/stable/std/process/struct.Child.html).
- Report `not_installed`, `not_authenticated`, `permission_denied`, timeout, and non-zero exit as distinct statuses. Do not infer authentication from a general `gh` login or from a provider unrelated to the CLI being launched.
- For one or a few short jobs, `std::thread` plus `Command` is enough. For many concurrent long-running jobs with streaming output, Tokio process support may justify the async option, but it should be a deliberate issue17 decision.

The CodexBar and Win-CodexBar repositories demonstrate the scale problem: the macOS project lists many providers and the Windows project advertises 56 providers with different authentication and local-store sources. This supports a static provider adapter interface and bounded parallel collection, not dynamic shared-library plugins. Each adapter should own its discovery, credential source, endpoint, parser, refresh policy, and output mapping. The central collector should preserve the existing contract: static supported providers, a discovered-provider set recalculated before collection, visible provider errors, and stable JSON for all consumers.

Provider count and process concurrency are separate concerns. A user may have many available providers without asking AI Fuel to launch many agent jobs. The collector can use a small bounded worker pool; the translation runner can later expose an explicit job limit and cancellation policy.

Sources: [CodexBar](https://github.com/steipete/CodexBar) and [Win-CodexBar](https://github.com/nesszer/Win-CodexBar).

## Targets and cross-compilation

The initial release matrix should be explicit rather than claiming all Rust targets:

| Platform | Target triple | Rust support evidence | Distribution note |
| --- | --- | --- | --- |
| Windows x64 | `x86_64-pc-windows-msvc` | Tier 1 target; Windows 10+/Server 2016+ baseline in the Rust table | Portable `.zip` first; signed installer later. |
| Windows ARM64 | `aarch64-pc-windows-msvc` | Tier 1 target in the Rust table | Build and smoke-test on an ARM64 environment before publishing. |
| macOS Intel | `x86_64-apple-darwin` | Tier 1 target; macOS 10.12+ note | CLI archive or signed/notarized app bundle. |
| macOS Apple Silicon | `aarch64-apple-darwin` | Tier 1 target; macOS 11+ note | CLI archive or signed/notarized app bundle. |
| Linux x64 glibc | `x86_64-unknown-linux-gnu` | Tier 1 target; glibc 2.17 and kernel 3.2+ note | Broad distro compatibility, but depends on the host glibc and CA bundle. |
| Linux ARM64 glibc | `aarch64-unknown-linux-gnu` | Tier 1 target; glibc 2.17 and kernel 4.1+ note | Native ARM64 build or a verified cross linker. |
| Linux x64/ARM64 musl | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` | Tier 2 targets | Optional more self-contained Linux artifacts; test bundled SQLite and TLS on real systems. |

These tiers and minimum OS notes come from the [Rust platform support table](https://doc.rust-lang.org/rustc/platform-support.html). Cargo can build each target with `--target`; release CI should use `--release --locked` and run a native smoke test for every published artifact. The `rusqlite` bundled C build and the Rustls crypto provider are the main reasons to use target-aware CI rather than assuming a host build proves all targets.

## Installers and release artifacts

| Distribution form | Benefits | Cost or prerequisite | Recommendation |
| --- | --- | --- | --- |
| Raw binary archive plus checksum | No runtime install; easiest to audit; works for CLI and browser dashboard | User must put the binary on `PATH`; checksum alone is not a signature | First release format for all targets. Include `LICENSE`, notices, and a version manifest. |
| Shell/PowerShell copy installer | Convenient PATH setup; still only copies the binary | Script permissions and shell policy; must never download or execute an unpinned helper | Optional convenience wrapper after archives work. |
| Windows MSI/MSIX/EXE | Start Menu, uninstall, update, enterprise deployment | Signing, installer maintenance, and package identity. MSIX has its own packaging model: [Microsoft MSIX overview](https://learn.microsoft.com/en-us/windows/msix/overview). | Later, only if a native desktop/tray experience is approved. |
| macOS `.app`/DMG | Native launch and tray integration | Developer ID signing, hardened runtime, and notarization for a trustworthy outside-App-Store experience: [Apple distribution guidance](https://help.apple.com/xcode/mac/current/en.lproj/dev033e997ca.html) and [Apple notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution) | Later, only for a native macOS app. A CLI archive is simpler. |
| Linux package | Desktop integration and update channels | Per-distribution metadata and signing; glibc/musl variants remain | Later. Start with glibc and optional musl archives. |

The `dist` project, formerly `cargo-dist`, can generate shippable binaries, tarballs, installers, manifests, and CI workflows: [dist project](https://github.com/axodotdev/cargo-dist). It is useful build/release automation, not an end-user runtime dependency. It should be evaluated after the target matrix and signing policy are decided.

The Win-CodexBar reference is useful evidence about the boundary: its README describes installer and portable builds but also WebView2 and VC++ runtime bootstrapping. That is a valid Windows desktop product choice, but it is not a strict one-binary/no-runtime-install choice: [Win-CodexBar installation notes](https://github.com/nesszer/Win-CodexBar#install).

## Licenses and notice obligations

The repository is MIT. Candidate components have compatible permissive licenses, but the release must preserve notices and audit transitive dependencies:

| Component | Upstream license evidence | Distribution note |
| --- | --- | --- |
| `ureq` | MIT OR Apache-2.0 in its manifest | Include the selected license notice in the release inventory. |
| `rustls` | Apache-2.0 OR ISC OR MIT in its manifest | Check the resolved version, because the transitive graph can change. |
| `rustls-platform-verifier` | MIT OR Apache-2.0 in its manifest | Uses platform APIs on Windows/macOS and system CA discovery on Linux. |
| `serde`, `serde_json` | MIT OR Apache-2.0 in upstream manifests | Proc-macro crates are build-time but still part of source/license audit. |
| `tiny_http` | MIT OR Apache-2.0 in its manifest | No TLS feature is needed for the strictly loopback dashboard. |
| `rusqlite` and `libsqlite3-sys` | MIT in upstream project; bundled SQLite is public domain | Keep the rusqlite and SQLite notices with the source/release inventory. |
| `webpki-roots` | CDLA-Permissive-2.0 in its manifest | This is a separate CA-data license and must not be omitted if the static-root feature is used. |

Rust's own official projects are generally dual-licensed MIT or Apache-2.0: [Rust licensing policy](https://rust-lang.org/policies/licenses/). License compatibility is not the only maintenance concern: a pinned `Cargo.lock`, reproducible release builds, a dependency audit, and a documented update cadence for TLS roots are required.

## Security and maintenance summary

- **Credential boundary:** read provider-owned stores only; do not centralize tokens. Keep discovery side-effect-free and preserve provider-specific failure states.
- **Network boundary:** use Rustls with hostname and certificate verification, allowlisted provider endpoints, timeouts, bounded bodies, and redacted logs. Never add a "skip TLS verification" fallback.
- **Local server boundary:** loopback only, fixed routes, no arbitrary file serving, no token-bearing URLs, and no browser-facing write endpoints unless CSRF/auth design is approved.
- **Process boundary:** direct executable plus separate arguments, explicit working directory, restricted inherited environment where appropriate, bounded output, deadlines, cancellation, and distinct helper status values.
- **Provider scale:** static adapters and stable JSON make adding many providers reviewable. Dynamic plugins would add loading, signing, ABI, and search-path risks without helping a self-contained release.
- **Build boundary:** `ureq`/Rustls are simpler to cross-compile than native OpenSSL, while bundled SQLite and OS credential adapters require target-specific CI. Build on native runners when a target's system framework or C linker matters.
- **Release boundary:** archives first, checksums and eventually signatures, then platform installers only after signing/notarization and runtime prerequisites are explicit.

## Evidence conclusion

The minimal synchronous Rust option provides the best balance for the current dashboard and quota-collection workload. It can produce one binary per target with no Python, Node, Rust, SQLite, WebView, or provider-CLI installation required by the binary itself. It does not remove system facilities: platform certificate stores, browser, authenticated agent CLIs, and optional OS credential stores remain explicit prerequisites or integration points.

This is a recommendation for the human architecture discussion in issue #17. It is not an architecture approval and does not resolve provider feasibility, permission policy, interface contracts, or parity scope.
