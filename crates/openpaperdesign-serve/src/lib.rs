//! Loopback HTTP server for editing one paper-rs project folder in a browser.
//!
//! - Serves the built `openpaperdesign-web` bundle and a small JSON API.
//! - API calls need the per-launch token (`Authorization: Bearer …`); requests with a foreign
//!   `Host` (DNS rebinding) or a cross-origin `Origin`/`Sec-Fetch-Site` are refused. One
//!   exception: a top-level document navigation to `/` from another site (a link or redirect,
//!   such as the host opening the editor URL) gets the static bootstrap page, which holds no
//!   project data. The editor's capability stays in the URL fragment, never sent over HTTP; the
//!   page's own API and MCP calls still need it as the Bearer token (see `top_level_entry`).
//! - Pages are addressed by the ids listed in the project's manifest only; the server builds
//!   every path itself, so no client input reaches the file system as a path.
//! - Symbolic links are refused, fail closed: `project.json`, `pages/`, every page file and
//!   every bundle file must be a real file or directory (and files must have a single hard
//!   link). The project root itself must stay the directory that was opened: on every access
//!   it must still be a real directory with the same canonical path and the same (device,
//!   inode). This is checked before upstream `Project::open` runs (its stale-temp cleanup must
//!   never follow a link out of the project) and again on every read and write. Atomic saves
//!   rename over the page file, so a write never lands outside `pages/`. Residual race: a
//!   directory or link swapped in between a check and the following file operation can still
//!   redirect that read or write.
//! - The web bundle (eight allowlisted files) is checked and read into memory once at startup,
//!   from a folder outside the project; static requests never touch the file system.
//! - Unix only (Linux, macOS): the root identity and hard-link checks use Unix metadata, so the
//!   server refuses to start elsewhere.
//! - Writes are revision-checked: a page's revision (ETag) is a hash of its file **as it is
//!   on disk**, `PUT` needs a matching `If-Match`, and a stale one gets `409 Conflict` with
//!   the file left untouched. Saves go through upstream `project::Project::save_page`
//!   (format + atomic write), so the native app reads them as its own.
//!
//! Boundary: only writers that go through this server are serialized and revision-checked.
//! Because the revision is computed from the file on disk, a change made by another writer
//! (e.g. a running native paper-rs) before the browser's next save makes that save fail
//! with 409 instead of overwriting it. Nothing stops the other writer from overwriting the
//! browser's saves, and a write landing between this server's check and its rename is not
//! detected. Run one writer at a time.

pub mod agent;
mod commit;
#[doc(hidden)]
pub mod launcher;
pub(crate) mod library;
mod lifecycle;
#[cfg(test)]
mod test_support;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use paper_rs::model::Document;
use paper_rs::project::format::{page_from_json, page_to_json};
use paper_rs::project::{ASSETS_DIR, MANIFEST, Manifest, PAGES_DIR, PageData, Project, now_ms};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const ASSETS: &str = ASSETS_DIR;
/// The MCP endpoint (same listener, same token).
pub const MCP_PATH: &str = "/mcp";

/// Files of the web bundle, by URL path. Nothing else is served from disk. `panel.js` is the
/// in-panel editor's own part; the launcher inlines it (with the glue, `session.js`, `page.js`
/// and `editor.js`) into the home view, and serving it here gives the browser nothing new.
const BUNDLE: &[(&str, &str, &str)] = &[
    ("/", "index.html", "text/html; charset=utf-8"),
    ("/app.js", "app.js", "text/javascript; charset=utf-8"),
    ("/page.js", "page.js", "text/javascript; charset=utf-8"),
    ("/editor.js", "editor.js", "text/javascript; charset=utf-8"),
    ("/panel.js", "panel.js", "text/javascript; charset=utf-8"),
    (
        "/session.js",
        "session.js",
        "text/javascript; charset=utf-8",
    ),
    (
        "/openpaperdesign_web.js",
        "openpaperdesign_web.js",
        "text/javascript; charset=utf-8",
    ),
    (
        "/openpaperdesign_web_bg.wasm",
        "openpaperdesign_web_bg.wasm",
        "application/wasm",
    ),
];

/// Same-origin only; wasm needs `wasm-unsafe-eval`, the page's `<style>` block needs inline
/// styles. Not embeddable in other sites' frames.
const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Revision of a page whose file does not exist yet (listed in the manifest, never saved).
pub const ABSENT: &str = "\"absent\"";
pub(crate) const MAX_PAGE_BYTES: usize = 64 << 20;

/// An extra identity check run before every read and write of a bound project, from the
/// browser API and the MCP host alike (the app library's root and lease, see `library`). Once
/// it fails it keeps failing.
pub(crate) type Guard = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// What the browser editor reported to this server: that it loaded the page and its renderer
/// is Ready, and how many of its saves were committed. Never the URL or anything else.
#[derive(Default)]
pub(crate) struct EditorState {
    ready: std::sync::atomic::AtomicBool,
    saves: std::sync::atomic::AtomicU64,
}

pub struct Config {
    /// The project folder (must contain an upstream `project.json`).
    pub project: PathBuf,
    /// Directory with the built web bundle.
    pub web_dir: PathBuf,
    /// The port the listener is bound to (used for the Host/Origin allow-list).
    pub port: u16,
    pub token: String,
}

pub struct Server {
    /// The web bundle, read once at startup: (URL path, content type, bytes). Static files are
    /// never read from disk per request.
    bundle: Bundle,
    /// Canonical project root at startup.
    dir: PathBuf,
    /// (device, inode) of the root at startup.
    root_id: (u64, u64),
    token: String,
    hosts: [String; 2],
    origins: [String; 2],
    /// Serializes revision check + write for every writer through this server: browser saves
    /// and agent calls (the MCP host thread takes the same lock).
    lifecycle: Arc<lifecycle::Lifecycle>,
    /// Tool calls for the MCP host thread.
    calls: agent::Calls,
    /// Set when a failed write couldn't be undone; every later write is refused.
    broken: Broken,
    /// Every page id the host has held, released only after retirement drains its writers.
    held: HeldPages,
    /// The app library's identity check for this project, if it is a library canvas.
    guard: Option<Guard>,
    editor: EditorState,
}

/// Fully loaded but not yet active. Preparation has no effect on the layout registry.
/// The launcher retires its old server before consuming this value.
pub(crate) struct PreparedServer(Arc<Server>);

impl PreparedServer {
    /// Start with fresh layout state for this project's ids. For a replacement, the previous
    /// server must have completed `retire` first. Arbitrary concurrent hosts are unsupported.
    pub(crate) fn activate(self) -> Arc<Server> {
        self.0.lifecycle.activate(&self.0.held);
        self.0
    }
}

/// Page ids a host has held (shared between the server and its host thread).
pub type HeldPages = Arc<std::sync::Mutex<std::collections::HashSet<String>>>;

/// Fail-closed state shared by every writer of one server: once a failed multi-file write
/// could not be rolled back (see `commit`), the message naming the files is kept here and
/// every further write, browser or MCP, is refused with it. Restarting the server after
/// checking those files clears it.
pub type Broken = Arc<std::sync::Mutex<Option<String>>>;

pub(crate) fn set_broken(b: &Broken, why: &str) {
    *b.lock().unwrap() = Some(why.to_string());
}

pub(crate) fn refuse_if_broken(b: &Broken) -> Result<(), String> {
    match b.lock().unwrap().as_ref() {
        Some(files) => Err(format!(
            "writes are disabled: a failed save could not be undone, so these files may be \
             inconsistent: {files}. Check or restore them, then restart the server"
        )),
        None => Ok(()),
    }
}

#[derive(Debug)]
pub enum OpenError {
    NotAProject(PathBuf, String),
    Unsafe(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NotAProject(p, e) => {
                write!(f, "{} is not a paper-rs project folder: {e}", p.display())
            }
            OpenError::Unsafe(e) => write!(f, "refusing to serve this project: {e}"),
        }
    }
}

impl std::error::Error for OpenError {}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    File,
    Dir,
}

/// `Ok(true)` if `path` is a real file/dir of the wanted kind, `Ok(false)` if it doesn't
/// exist, and an error for a symlink, the wrong kind, or a file with more than one hard link
/// (which could alias a file outside the project).
pub(crate) fn plain(path: &Path, want: Kind) -> Result<bool, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let m = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{name}: {e}")),
    };
    let t = m.file_type();
    if t.is_symlink() {
        return Err(format!("{name} is a symbolic link"));
    }
    match want {
        Kind::Dir if !t.is_dir() => Err(format!("{name} is not a directory")),
        Kind::File if !t.is_file() => Err(format!("{name} is not a regular file")),
        Kind::File if links(&m) > 1 => Err(format!("{name} has more than one hard link")),
        _ => Ok(true),
    }
}

#[cfg(unix)]
pub(crate) fn links(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.nlink()
}

#[cfg(unix)]
pub(crate) fn dev_ino(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

// Non-Unix: `Server::open` refuses to start, so these never run; they fail closed anyway.
#[cfg(not(unix))]
pub(crate) fn links(_m: &std::fs::Metadata) -> u64 {
    u64::MAX
}

#[cfg(not(unix))]
pub(crate) fn check_root(_dir: &Path, _id: (u64, u64)) -> Result<(), String> {
    Err("only supported on Unix".into())
}

/// The root must still be the directory opened at startup: a real directory (not a link),
/// with the same canonical path (catches swapped ancestors too) and the same (device, inode).
#[cfg(unix)]
pub(crate) fn check_root(dir: &Path, id: (u64, u64)) -> Result<(), String> {
    let m = std::fs::symlink_metadata(dir).map_err(|e| format!("project folder: {e}"))?;
    if m.file_type().is_symlink() || !m.is_dir() {
        return Err("the project folder was replaced (not a real directory any more)".into());
    }
    let now = dir
        .canonicalize()
        .map_err(|e| format!("project folder: {e}"))?;
    if now != dir || dev_ino(&m) != id {
        return Err("the project folder was replaced since the server started".into());
    }
    Ok(())
}

/// Web bundle files compiled into a binary: (file name, bytes).
pub type EmbeddedBundle = &'static [(&'static str, &'static [u8])];

/// The bundle file names a complete build has (see `scripts/build-web.sh`).
pub fn bundle_file_names() -> Vec<&'static str> {
    BUNDLE.iter().map(|(_, f, _)| *f).collect()
}

enum BundleSource {
    Dir(PathBuf),
    Embedded(EmbeddedBundle),
}

/// (URL path, content type, bytes) of each bundle file found at startup.
type Bundle = Vec<(&'static str, &'static str, Vec<u8>)>;

/// The bundle folder's real path (symlinks resolved by the OS), or `None` when it doesn't
/// exist, in which case nothing is served. No lexical guessing: a path that doesn't exist and
/// contains `..` is refused, because `..` after a symlink can't be resolved without it.
fn resolve(path: &Path) -> Result<Option<PathBuf>, String> {
    match path.canonicalize() {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                Err("a web bundle folder that doesn't exist may not contain `..`".into())
            } else {
                Ok(None)
            }
        }
        Err(e) => Err(format!("web bundle folder: {e}")),
    }
}

/// Read the bundle allowlist into memory. The bundle directory must not be (or be inside) the
/// project folder, and every file present must be a plain single-link file. A missing
/// directory or file is not an error: nothing is served for it (404 "build it first").
fn load_bundle(web_dir: &Path, project: &Path) -> Result<Bundle, String> {
    let Some(web) = resolve(web_dir)? else {
        return Ok(Vec::new());
    };
    if web.starts_with(project) {
        return Err("the web bundle folder must not be inside the project folder".into());
    }
    if !plain(&web, Kind::Dir)? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for &(url, file, mime) in BUNDLE {
        let path = web.join(file);
        if plain(&path, Kind::File)? {
            let bytes = std::fs::read(&path).map_err(|e| format!("{file}: {e}"))?;
            out.push((url, mime, bytes));
        }
    }
    Ok(out)
}

/// The project's own entries must be real: `project.json` (required) and `pages/` (optional).
pub(crate) fn check_project_dir(dir: &Path) -> Result<(), String> {
    if !plain(&dir.join(MANIFEST), Kind::File)? {
        return Err(format!("{MANIFEST} is missing"));
    }
    plain(&dir.join(PAGES_DIR), Kind::Dir)?;
    plain(&dir.join(ASSETS), Kind::Dir)?;
    Ok(())
}

/// A page id that can safely name a file under `pages/`: `[A-Za-z0-9_-]{1,64}` (no
/// separators, dots or absolute paths). Upstream ids are 16 hex digits.
/// A page id's node-id prefix (upstream `<first six characters>:<n>`), case-folded.
pub(crate) fn page_prefix(id: &str) -> String {
    id.chars().take(6).collect::<String>().to_ascii_lowercase()
}

pub(crate) fn valid_page_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The manifest, read only from a real `project.json`, with every page id checked (and no
/// duplicates) before any page is touched.
pub(crate) fn read_manifest_checked(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join(MANIFEST);
    if !plain(&path, Kind::File)? {
        return Err(format!("{MANIFEST} is missing"));
    }
    let m = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| Manifest::from_json(&s).map_err(|e| e.to_string()))?;
    let mut seen = std::collections::HashSet::new();
    let mut prefixes = std::collections::HashSet::new();
    for p in &m.pages {
        if !valid_page_id(&p.id) {
            return Err(format!("{MANIFEST} lists an unsafe page id {:?}", p.id));
        }
        // Compared case-folded: on a case-insensitive file system (macOS APFS by default)
        // `CasePage` and `casepage` are the same page file.
        if !seen.insert(p.id.to_ascii_lowercase()) {
            return Err(format!(
                "{MANIFEST} lists page id {:?} twice (page ids must differ in more than case)",
                p.id
            ));
        }
        // Node ids name their page by its first six characters (`<prefix>:<n>`), so two
        // pages sharing them would be ambiguous to every tool.
        if !prefixes.insert(page_prefix(&p.id)) {
            return Err(format!(
                "{MANIFEST} lists page id {:?}, whose first six characters (ignoring case) \
                 are another page's; node ids couldn't tell the pages apart",
                p.id
            ));
        }
    }
    Ok(m)
}

fn forbidden(e: String) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, format!("refused: {e}"))
}

impl Server {
    /// Open an existing project folder. Nothing is created: a folder without a valid
    /// `project.json` is refused.
    pub fn open(cfg: Config) -> Result<Arc<Server>, OpenError> {
        #[cfg(not(unix))]
        return Err(OpenError::Unsafe(
            "only supported on Unix (Linux, macOS): the project-root identity and hard-link \
             checks need Unix metadata"
                .into(),
        ));
        #[cfg(unix)]
        Self::open_unix(
            cfg.project,
            BundleSource::Dir(cfg.web_dir),
            cfg.port,
            cfg.token,
            None,
        )
        .map(PreparedServer::activate)
    }

    /// Like [`Server::open`], with the web bundle compiled into the binary (`files` = file
    /// name and bytes; only the allowlisted names are served). Nothing is read from disk for
    /// static files, at startup or later.
    pub fn open_embedded(
        project: PathBuf,
        files: EmbeddedBundle,
        port: u16,
        token: String,
    ) -> Result<Arc<Server>, OpenError> {
        #[cfg(not(unix))]
        return Err(OpenError::Unsafe(
            "only supported on Unix (Linux, macOS)".into(),
        ));
        #[cfg(unix)]
        Self::prepare_embedded(project, files, port, token).map(PreparedServer::activate)
    }

    /// Validate and load a replacement without changing the active binding's layout state.
    /// The returned server admits no calls until `activate` consumes it.
    pub(crate) fn prepare_embedded(
        project: PathBuf,
        files: EmbeddedBundle,
        port: u16,
        token: String,
    ) -> Result<PreparedServer, OpenError> {
        #[cfg(not(unix))]
        return Err(OpenError::Unsafe(
            "only supported on Unix (Linux, macOS)".into(),
        ));
        #[cfg(unix)]
        Self::open_unix(project, BundleSource::Embedded(files), port, token, None)
    }

    /// Like [`Server::prepare_embedded`], for an app-library canvas: `guard` (its library and
    /// lease check, see `library::guard`) runs before upstream `Project::open` and then before
    /// every browser and MCP read and write.
    pub(crate) fn prepare_library(
        project: PathBuf,
        files: EmbeddedBundle,
        port: u16,
        token: String,
        guard: Guard,
    ) -> Result<PreparedServer, OpenError> {
        #[cfg(not(unix))]
        return Err(OpenError::Unsafe(
            "only supported on Unix (Linux, macOS)".into(),
        ));
        #[cfg(unix)]
        Self::open_unix(
            project,
            BundleSource::Embedded(files),
            port,
            token,
            Some(guard),
        )
    }

    #[cfg(unix)]
    fn open_unix(
        project: PathBuf,
        bundle: BundleSource,
        port: u16,
        token: String,
        guard: Option<Guard>,
    ) -> Result<PreparedServer, OpenError> {
        if let Some(g) = &guard {
            g().map_err(OpenError::Unsafe)?;
        }
        let dir = project
            .canonicalize()
            .map_err(|e| OpenError::NotAProject(project.clone(), e.to_string()))?;
        let root_id = std::fs::symlink_metadata(&dir)
            .map(|m| dev_ino(&m))
            .map_err(|e| OpenError::NotAProject(dir.clone(), e.to_string()))?;
        // Before upstream `Project::open`: its stale-temp cleanup deletes files in `pages/`, so
        // that must not be a link to somewhere else.
        check_project_dir(&dir).map_err(OpenError::Unsafe)?;
        // Same check (and stale temp-file cleanup) as when the native app opens a project.
        Project::open(&dir).map_err(|e| OpenError::NotAProject(dir.clone(), e.to_string()))?;
        let bundle = match bundle {
            BundleSource::Dir(web_dir) => load_bundle(&web_dir, &dir).map_err(OpenError::Unsafe)?,
            BundleSource::Embedded(files) => BUNDLE
                .iter()
                .filter_map(|&(url, file, mime)| {
                    files
                        .iter()
                        .find(|(name, _)| *name == file)
                        .map(|(_, bytes)| (url, mime, bytes.to_vec()))
                })
                .collect(),
        };
        let lifecycle = Arc::new(lifecycle::Lifecycle::default());
        let broken = Broken::default();
        let held = HeldPages::default();
        let calls = agent::spawn_host_guarded(
            dir.clone(),
            root_id,
            lifecycle.clone(),
            broken.clone(),
            held.clone(),
            guard.clone(),
        )
        .map_err(|e| OpenError::NotAProject(dir.clone(), e))?;
        let p = port;
        Ok(PreparedServer(Arc::new(Server {
            dir,
            root_id,
            bundle,
            token,
            hosts: [format!("127.0.0.1:{p}"), format!("localhost:{p}")],
            origins: [
                format!("http://127.0.0.1:{p}"),
                format!("http://localhost:{p}"),
            ],
            lifecycle,
            calls,
            broken,
            held,
            guard,
            editor: EditorState::default(),
        })))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Permanently close admission, wait for the current writer (including rollback), and
    /// release this host's page ids. Queued and late HTTP/stdio calls cannot execute, and
    /// browser saves check the same fence. Retained handles stay retired; this is idempotent.
    pub async fn retire(&self) {
        self.lifecycle.retire(&self.held).await;
        self.calls.stop();
    }

    /// Run a tool call against this project's host, exactly as `/mcp` does (same policy,
    /// lock, save, revision and stale-write check). `session` is the caller's MCP session,
    /// from [`agent::new_session`].
    pub async fn call_tool(
        &self,
        session: u64,
        name: &str,
        args: serde_json::Value,
    ) -> paper_rs::mcp::ToolResult {
        agent::dispatch(&self.calls, session, name.to_string(), args).await
    }

    /// How many bundle files were found at startup (0 = not built).
    pub fn bundle_files(&self) -> usize {
        self.bundle.len()
    }

    /// The bound project's manifest, behind the same boundary as every API read: the root is
    /// still the directory that was bound (a real directory with the same canonical path and
    /// device/inode, so a replaced or linked root is refused) and its own entries are real.
    /// Reading it acknowledges no page.
    pub(crate) fn bound_manifest(&self) -> Result<Manifest, String> {
        check_root(&self.dir, self.root_id)?;
        self.check_guard()?;
        check_project_dir(&self.dir)?;
        read_manifest_checked(&self.dir)
    }

    fn check_guard(&self) -> Result<(), String> {
        self.guard.as_ref().map_or(Ok(()), |g| g())
    }

    /// Root identity, the library guard (for a library canvas) and the project's own entries;
    /// run before every read and write.
    fn check(&self) -> Result<(), ApiError> {
        check_root(&self.dir, self.root_id).map_err(forbidden)?;
        self.check_guard().map_err(forbidden)?;
        check_project_dir(&self.dir).map_err(forbidden)
    }

    /// Whether the browser editor reported its page loaded and Ready, and how many browser
    /// saves were committed. A navigation request never sets these.
    pub(crate) fn editor_state(&self) -> (bool, u64) {
        use std::sync::atomic::Ordering;
        (
            self.editor.ready.load(Ordering::Acquire),
            self.editor.saves.load(Ordering::Acquire),
        )
    }

    /// The saved canvas as a portable file (`library::payload`): one coherent snapshot of the
    /// files on disk, read under this binding's writer fence, so no browser or MCP save is
    /// half-written in it (it waits for one under way). Edits still only in a browser tab are
    /// not on disk and not in it. The fence is released before the text is handed on.
    pub(crate) async fn saved_snapshot(&self) -> Result<library::payload::Exported, String> {
        let _write = self.lifecycle.write.lock().await;
        self.lifecycle.check_active()?;
        check_root(&self.dir, self.root_id)?;
        self.check_guard()?;
        check_project_dir(&self.dir)?;
        library::payload::export(&self.dir)
    }

    fn project(&self) -> Result<Project, ApiError> {
        // Re-read per request: the manifest may change while we run (e.g. pages renamed
        // natively). Not `Project::open`, which also deletes other writers' temp files.
        self.check()?;
        let manifest = read_manifest_checked(&self.dir)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(Project {
            dir: self.dir.clone(),
            manifest,
        })
    }
}

/// A random per-launch API token (32 bytes, hex).
pub fn new_token() -> Result<String, getrandom::Error> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Revision of page file bytes: 64-bit SipHash (std `DefaultHasher`, fixed keys) of the
/// exact bytes. A change detector for honest writers, not a cryptographic digest.
pub fn etag(bytes: &[u8]) -> String {
    let mut h = std::hash::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

fn error(status: StatusCode, msg: &str) -> Response {
    ApiError::new(status, msg).into_response()
}

/// An API failure: status, message and (for 409) the page's current revision. The in-panel
/// page tools (`launcher_app.rs`) report the same three.
pub(crate) struct ApiError {
    status: StatusCode,
    msg: String,
    etag: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> ApiError {
        ApiError {
            status,
            msg: msg.into(),
            etag: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.msg }).to_string();
        let mut r = (
            self.status,
            [(header::CONTENT_TYPE, "application/json")],
            Body::from(body),
        )
            .into_response();
        if let Some(e) = self.etag.and_then(|e| HeaderValue::from_str(&e).ok()) {
            r.headers_mut().insert(header::ETAG, e);
        }
        r
    }
}

fn header_str(h: &HeaderMap, name: impl header::AsHeaderName) -> Option<&str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The one request from another site that is let through: the browser's top-level navigation
/// to the editor page itself, as when a link or redirect on the host's own site (its open-link)
/// opens the editor URL. Only `GET /`, marked by the browser as a navigation
/// (`Sec-Fetch-Mode: navigate`) of a top-level document (`Sec-Fetch-Dest: document`) from a
/// same-site or cross-site page. It returns the static bootstrap page only: no project data,
/// file operation or state. The editor's capability is the URL fragment (`#token=…`), which the
/// browser never sends, so nothing here checks or needs it; every API and MCP call still needs
/// it as a Bearer token from the page's own origin. Frames, fetches, subresources, other paths
/// and methods stay refused, and a foreign `Origin` or `Host` is refused before this.
fn top_level_entry(req: &Request) -> bool {
    let h = req.headers();
    req.method() == axum::http::Method::GET
        && req.uri().path() == "/"
        && header_str(h, "sec-fetch-mode") == Some("navigate")
        && header_str(h, "sec-fetch-dest") == Some("document")
        && matches!(
            header_str(h, "sec-fetch-site"),
            Some("same-site" | "cross-site")
        )
}

/// Host / Origin / fetch-site checks for every request, token check for `/api/`.
async fn guard(State(s): State<Arc<Server>>, req: Request, next: Next) -> Response {
    let h = req.headers();
    match header_str(h, header::HOST) {
        Some(host) if s.hosts.iter().any(|a| a == host) => {}
        _ => return error(StatusCode::FORBIDDEN, "unexpected Host"),
    }
    if let Some(origin) = h.get(header::ORIGIN)
        && !s.origins.iter().any(|a| origin == a.as_str())
    {
        return error(StatusCode::FORBIDDEN, "cross-origin request");
    }
    if let Some(site) = header_str(h, "sec-fetch-site")
        && !matches!(site, "same-origin" | "none")
        && !top_level_entry(&req)
    {
        return error(StatusCode::FORBIDDEN, "cross-site request");
    }
    let path = req.uri().path();
    if path.starts_with("/api/") || path == MCP_PATH || path.starts_with("/mcp/") {
        let ok = header_str(h, header::AUTHORIZATION)
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| same_bytes(t.as_bytes(), s.token.as_bytes()));
        if !ok {
            return error(StatusCode::UNAUTHORIZED, "missing or wrong token");
        }
    }
    if let Err(e) = s.lifecycle.check_active() {
        return error(StatusCode::SERVICE_UNAVAILABLE, &e);
    }
    next.run(req).await
}

/// On every response, including refusals from `guard`, unmatched paths and body-limit
/// errors.
async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    res
}

/// Streamable HTTP clients must accept JSON and SSE; add the header when a client omits it
/// (as upstream's MCP server does).
async fn fix_accept(mut req: Request, next: Next) -> Response {
    if req.method() == axum::http::Method::POST {
        let accept = header_str(req.headers(), header::ACCEPT).unwrap_or("");
        if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
            req.headers_mut().insert(
                header::ACCEPT,
                HeaderValue::from_static("application/json, text/event-stream"),
            );
        }
    }
    next.run(req).await
}

pub fn router(s: Arc<Server>) -> Router {
    let calls = s.calls.clone();
    let config = StreamableHttpServerConfig::default()
        .with_allowed_origins(s.origins.clone())
        .enforce_origin_validation()
        .with_max_request_body_bytes(MAX_PAGE_BYTES);
    let mcp: StreamableHttpService<agent::AgentServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(agent::AgentServer::new(calls.clone())),
            Default::default(),
            config,
        );
    let mut r = Router::new()
        .route("/api/project", get(get_project))
        .route("/api/pages/{id}", get(get_page).put(put_page))
        .route("/api/pages/{id}/revision", get(get_revision))
        .route("/api/ready", axum::routing::post(post_ready))
        .nest_service(
            MCP_PATH,
            tower::ServiceBuilder::new()
                .layer(middleware::from_fn(fix_accept))
                .service(mcp),
        );
    for &(url, file, mime) in BUNDLE {
        r = r.route(
            url,
            get(move |State(s): State<Arc<Server>>| async move {
                match s.bundle.iter().find(|(u, _, _)| *u == url) {
                    Some((_, _, b)) => ([(header::CONTENT_TYPE, mime)], b.clone()).into_response(),
                    None => error(
                        StatusCode::NOT_FOUND,
                        &format!("web bundle file {file} not found; build it first"),
                    ),
                }
            }),
        );
    }
    r.fallback(|| async { error(StatusCode::NOT_FOUND, "not found") })
        .layer(middleware::from_fn_with_state(s.clone(), guard))
        .layer(DefaultBodyLimit::max(MAX_PAGE_BYTES))
        .layer(middleware::map_response(security_headers))
        .with_state(s)
}

async fn get_project(State(s): State<Arc<Server>>) -> Response {
    let p = match s.project() {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let m = &p.manifest;
    let body = serde_json::json!({
        "name": m.name,
        "pages": m.pages.iter().map(|r| serde_json::json!({"id": r.id, "name": r.name})).collect::<Vec<_>>(),
        "active_page": m.active_page,
    });
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// The browser editor's own report that it loaded its page and its renderer is Ready (token
/// and origin checked like every API call). It carries nothing and changes no file.
async fn post_ready(State(s): State<Arc<Server>>) -> Response {
    match s.mark_ready() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// How long a save may wait for the writer fence before it is given up, unrun (as for MCP
/// calls, see `agent`).
const SAVE_START: std::time::Duration = std::time::Duration::from_secs(120);

/// One page as a reader gets it: its manifest identity, the canvas name, its revision and its
/// file bytes (a listed page without a file reads as upstream's empty page).
pub(crate) struct PageRead {
    pub id: String,
    pub name: String,
    pub project: String,
    pub revision: String,
    pub bytes: Vec<u8>,
}

/// The page operations shared by the browser's HTTP API and the app's in-panel page tools
/// (`launcher_app.rs`). Each one checks admission (an active binding) itself, so no caller
/// relies on the HTTP middleware for it; the page reads and the save also check the root
/// identity, the library guard and the project's own entries. `mark_ready` reads and writes no
/// file and checks only admission (the in-panel caller re-checks the canvas, library and lease
/// first). HTTP keeps its own Host/Origin/token checks in front of these.
impl Server {
    fn admit(&self) -> Result<(), ApiError> {
        self.lifecycle
            .check_active()
            .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e))
    }

    /// Page `id` of the manifest, or with `None` the manifest's active page (its first page
    /// when none is set or the set one isn't listed), as the browser editor picks it.
    pub(crate) fn page(&self, id: Option<&str>) -> Result<PageRead, ApiError> {
        self.admit()?;
        let p = self.project()?;
        let id = match id {
            Some(id) => id.to_string(),
            None => p
                .manifest
                .active_page
                .clone()
                .filter(|a| p.manifest.pages.iter().any(|r| &r.id == a))
                .or_else(|| p.manifest.pages.first().map(|r| r.id.clone()))
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "this canvas has no pages"))?,
        };
        let name = page_ref(&p, &id)?;
        let bytes = read_page(&p, &id)?;
        let rev = revision(&bytes);
        let bytes = match bytes {
            Some(b) => b,
            // Same as upstream `Project::load_pages`: a listed page without a file is empty.
            None => page_to_json(&PageData {
                id: id.clone(),
                name: name.clone(),
                doc: Document {
                    name: name.clone(),
                    ..Default::default()
                },
                camera: None,
            })
            .into_bytes(),
        };
        Ok(PageRead {
            id,
            name,
            project: p.manifest.name.clone(),
            revision: rev,
            bytes,
        })
    }

    /// The current revision of page `id` (cheap polling).
    pub(crate) fn page_revision(&self, id: &str) -> Result<String, ApiError> {
        self.admit()?;
        let p = self.project()?;
        page_ref(&p, id)?;
        Ok(revision(&read_page(&p, id)?))
    }

    /// Save page `id` from `body` (an upstream page file), only if `if_match` is its revision
    /// on disk now. The one save transaction of every browser and in-panel save: the page and
    /// the manifest (its `modified` time only; the page list, names, order, active page, other
    /// pages and assets as they are on disk now) are one commit, rolled back on any error. A
    /// failed rollback fences every later write (`Broken`). The wait for the writer fence is
    /// bounded (`SAVE_START`): a save that couldn't start, or whose caller stopped waiting
    /// before it started, never runs. Once it holds the fence nothing awaits any more, so a
    /// started save always finishes its commit or rollback, and retirement waits for it.
    /// Returns the revision of the bytes written.
    pub(crate) async fn save_page(
        &self,
        id: &str,
        if_match: &str,
        body: &str,
    ) -> Result<String, ApiError> {
        self.save_page_within(SAVE_START, None, id, if_match, body)
            .await
    }

    /// [`Server::save_page`] for an editor whose lifetime can be revoked (`live`, an in-panel
    /// editor's mount): it is checked again under the writer fence, so a save of an editor
    /// that was released before the save started never runs. Revoke it, then
    /// [`Server::drain_writer`], to know no save of that editor is still under way.
    pub(crate) async fn save_page_while(
        &self,
        live: &std::sync::atomic::AtomicBool,
        id: &str,
        if_match: &str,
        body: &str,
    ) -> Result<String, ApiError> {
        self.save_page_within(SAVE_START, Some(live), id, if_match, body)
            .await
    }

    /// Wait until no writer (a started save, its commit or rollback, or an agent call) holds
    /// the writer fence any more. Changes nothing.
    pub(crate) async fn drain_writer(&self) {
        drop(self.lifecycle.write.lock().await);
    }

    /// [`Server::save_page`] with its own bound on the wait for the writer fence and an
    /// optional revocable editor lifetime (see [`Server::save_page_while`]).
    pub(crate) async fn save_page_within(
        &self,
        start_within: std::time::Duration,
        live: Option<&std::sync::atomic::AtomicBool>,
        id: &str,
        if_match: &str,
        body: &str,
    ) -> Result<String, ApiError> {
        if body.len() > MAX_PAGE_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the page is larger than {MAX_PAGE_BYTES} bytes; nothing was written"),
            ));
        }
        #[cfg(test)]
        test_support::hit(&self.dir, "browser_queued");
        let _write = tokio::time::timeout(start_within, self.lifecycle.write.lock())
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the save could not start in time (the canvas was busy); it was not run and \
                     nothing was written",
                )
            })?;
        // Under the fence: admission again (a close may have retired the binding meanwhile),
        // and the editor's own lifetime (it may have been released meanwhile).
        self.admit()?;
        if live.is_some_and(|l| !l.load(std::sync::atomic::Ordering::Acquire)) {
            return Err(ApiError::new(
                StatusCode::GONE,
                "this editor was left (released) before its save could start; it was not run \
                 and nothing was written",
            ));
        }
        // Started: from here on it runs to its commit or rollback, and retirement and a
        // release (`drain_writer`) wait for it.
        #[cfg(test)]
        test_support::hit(&self.dir, "save_started");
        let mut p = self.project()?;
        let name = page_ref(&p, id)?;
        // Strict: the save must be based on the revision on disk now (whoever wrote it: the
        // agent through this server, or anything else). A stale one is refused, unwritten.
        let current = revision(&read_page(&p, id)?);
        if if_match != current {
            return Err(ApiError {
                etag: Some(current),
                ..ApiError::new(
                    StatusCode::CONFLICT,
                    "the page changed since it was loaded; nothing was written",
                )
            });
        }
        let mut page = page_from_json(body)
            .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
        if page.id != id {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "page id in the body does not match the URL or page_id",
            ));
        }
        // The manifest owns page names (as in upstream `Project::load_page`).
        page.name = name.clone();
        page.doc.name = name;
        // Checked again just before writing (`read_page` above checked the page file).
        self.check()?;
        refuse_if_broken(&self.broken)
            .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e))?;
        // Like the native session: a page write bumps the project's modified time. The page
        // and the manifest are written as one commit: on an error neither changed.
        p.manifest.modified = now_ms();
        let writes = [
            commit::Write {
                path: p.page_path(&page.id),
                data: page_to_json(&page).into_bytes(),
            },
            commit::Write {
                path: p.dir.join(MANIFEST),
                data: p.manifest.to_json().into_bytes(),
            },
        ];
        commit::commit(&writes).map_err(|e| {
            if let commit::CommitError::Broken { files, .. } = &e {
                set_broken(&self.broken, &files.join(", "));
            }
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.describe())
        })?;
        // Committed: the revision is the hash of the bytes just written. Nothing that can
        // fail comes after the commit, so a saved page is never reported as not saved.
        let rev = etag(&writes[0].data);
        self.editor
            .saves
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(rev)
    }

    /// An editor of this binding (the browser tab or an in-panel editor) reports that it loaded
    /// its page and its renderer is Ready. Changes no file.
    pub(crate) fn mark_ready(&self) -> Result<(), ApiError> {
        self.admit()?;
        self.editor
            .ready
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

/// The manifest's entry for `id`, or an error response. Ids are checked against the
/// manifest (and for a plain character set), never used as client-chosen paths.
fn page_ref(p: &Project, id: &str) -> Result<String, ApiError> {
    if !valid_page_id(id) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid page id"));
    }
    p.manifest
        .pages
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.name.clone())
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such page in this project"))
}

/// Current file bytes of a page (None when it was never written).
fn read_page(p: &Project, id: &str) -> Result<Option<Vec<u8>>, ApiError> {
    let path = p.page_path(id);
    if !plain(&p.dir.join(PAGES_DIR), Kind::Dir).map_err(forbidden)?
        || !plain(&path, Kind::File).map_err(forbidden)?
    {
        return Ok(None);
    }
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
        )),
    }
}

fn revision(bytes: &Option<Vec<u8>>) -> String {
    bytes.as_deref().map_or_else(|| ABSENT.to_string(), etag)
}

async fn get_page(State(s): State<Arc<Server>>, UrlPath(id): UrlPath<String>) -> Response {
    match s.page(Some(&id)) {
        Ok(page) => (
            [
                (header::CONTENT_TYPE, "application/json".to_string()),
                (header::ETAG, page.revision),
            ],
            page.bytes,
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// The page's current revision only (cheap polling for the browser).
async fn get_revision(State(s): State<Arc<Server>>, UrlPath(id): UrlPath<String>) -> Response {
    match s.page_revision(&id) {
        Ok(rev) => (
            [(header::ETAG, rev.clone())],
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({ "revision": rev }).to_string(),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn put_page(
    State(s): State<Arc<Server>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(if_match) = header_str(&headers, header::IF_MATCH).map(str::to_string) else {
        return error(
            StatusCode::PRECONDITION_REQUIRED,
            "If-Match with the page revision is required",
        );
    };
    match s.save_page(&id, &if_match, &body).await {
        Ok(rev) => (
            StatusCode::OK,
            [(header::ETAG, rev.clone())],
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({ "revision": rev }).to_string(),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    const TOKEN: &str = "unit-test-token-0123456789abcdef";

    async fn put(s: &Arc<Server>, id: &str, tag: &str, body: String) -> (StatusCode, String) {
        use http_body_util::BodyExt;
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/pages/{id}"))
            .header(header::HOST, "127.0.0.1:1")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::IF_MATCH, tag)
            .body(Body::from(body))
            .unwrap();
        let res = router(s.clone()).oneshot(req).await.unwrap();
        let st = res.status();
        let b = res.into_body().collect().await.unwrap().to_bytes();
        (st, String::from_utf8_lossy(&b).into_owned())
    }

    /// Review round 4, F1, browser side: the page and the manifest are one commit, so a
    /// failure at any step leaves both byte-identical; after an undoable failure, every
    /// later save is refused.
    #[tokio::test(flavor = "current_thread")]
    async fn browser_saves_are_all_or_nothing_and_fail_closed() {
        use std::hash::{BuildHasher, RandomState};
        let root = std::env::temp_dir().join(format!(
            "opd-unit-put-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (p, _) = paper_rs::project::Store::open(&root)
            .unwrap()
            .create("Put")
            .unwrap();
        let id = p.manifest.pages[0].id.clone();
        let s = Server::open(Config {
            project: p.dir.clone(),
            web_dir: root.join("no-bundle"),
            port: 1,
            token: TOKEN.into(),
        })
        .unwrap();
        let files = || {
            (
                std::fs::read(p.page_path(&id)).unwrap(),
                std::fs::read(p.dir.join(MANIFEST)).unwrap(),
            )
        };
        let mut page = p.load_page(&id).unwrap();
        page.doc.next_id += 1;
        let body = page_to_json(&page);
        // Steps: stage page (0), stage manifest (1), rename page (2), rename manifest (3),
        // sync pages/ (4), sync the project folder (5).
        for at in 0..6 {
            let before = files();
            let tag = etag(&before.0);
            commit::fault::fail_at(&[at]);
            let (st, msg) = put(&s, &id, &tag, body.clone()).await;
            commit::fault::fail_at(&[]);
            assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "step {at}");
            assert!(msg.contains("nothing was written"), "step {at}: {msg}");
            assert_eq!(files(), before, "step {at}: nothing written");
        }
        // Rename of the manifest fails, and restoring the page fails too: fail closed, and
        // the answer says the page may be inconsistent (review round 5, F3).
        let tag = etag(&files().0);
        commit::fault::fail_at(&[3, 4]);
        let (st, msg) = put(&s, &id, &tag, body.clone()).await;
        commit::fault::fail_at(&[]);
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            msg.contains("may now be inconsistent")
                && msg.contains(&format!("{id}.json"))
                && msg.contains("disabled")
                && !msg.contains("nothing was written"),
            "{msg}"
        );
        assert!(s.broken.lock().unwrap().is_some());
        let before = files();
        let (st, msg) = put(&s, &id, &etag(&before.0), body).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert!(msg.contains(&format!("{id}.json")), "{msg}");
        assert_eq!(files(), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Operator Mac finding on cffeec6: a server that stops for good releases the stretch
    /// entries of every page it held, and only those.
    #[tokio::test]
    async fn releasing_a_server_drops_only_its_pages_layout_state() {
        use std::hash::{BuildHasher, RandomState};
        let root = std::env::temp_dir().join(format!(
            "opd-unit-release-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (p, _) = paper_rs::project::Store::open(&root)
            .unwrap()
            .create("Release")
            .unwrap();
        let id = p.manifest.pages[0].id.clone();
        let s = Server::open(Config {
            project: p.dir.clone(),
            web_dir: root.join("no-bundle"),
            port: 1,
            token: TOKEN.into(),
        })
        .unwrap();
        let other = format!("other-{id}");
        let n = paper_rs::model::NodeId(7);
        paper_rs::mcp::exec::with_no_stretch(|set| {
            set.insert((id.clone(), n));
            set.insert((other.clone(), n));
        });
        s.retire().await;
        let (mine, theirs) = paper_rs::mcp::exec::with_no_stretch(|set| {
            (
                set.contains(&(id.clone(), n)),
                set.contains(&(other.clone(), n)),
            )
        });
        assert!(!mine && theirs);
        paper_rs::mcp::exec::with_no_stretch(|set| set.remove(&(other.clone(), n)));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Retirement releases the lifetime union, including pages no longer in the manifest.
    /// The manifest edit is a sequential reload fixture, not a concurrent external writer.
    #[tokio::test]
    async fn retirement_includes_created_then_removed_pages_and_preserves_other_keys() {
        use paper_rs::mcp::Out;
        use paper_rs::model::NodeId;
        use serde_json::{Value, json};
        fn value(r: paper_rs::mcp::ToolResult) -> Value {
            r.unwrap()
                .iter()
                .find_map(|o| match o {
                    Out::Text(t) => serde_json::from_str(t).ok(),
                    _ => None,
                })
                .unwrap()
        }
        use std::hash::{BuildHasher, RandomState};
        let root = std::env::temp_dir().join(format!(
            "opd-held-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (p, _) = paper_rs::project::Store::open(&root)
            .unwrap()
            .create("Held")
            .unwrap();
        let first = p.manifest.pages[0].id.clone();
        let other = format!("other-{first}");
        let node = NodeId(991);
        paper_rs::mcp::exec::with_no_stretch(|s| {
            s.insert((first.clone(), node));
            s.insert((other.clone(), node));
        });
        let server = Server::open(Config {
            project: p.dir.clone(),
            web_dir: root.join("no-web"),
            port: 1,
            token: TOKEN.into(),
        })
        .unwrap();
        assert!(paper_rs::mcp::exec::with_no_stretch(|s| !s
            .contains(&(first.clone(), node))
            && s.contains(&(other.clone(), node))));
        let session = agent::new_session();
        server
            .call_tool(session, "get_basic_info", json!({}))
            .await
            .unwrap();
        let added = value(
            server
                .call_tool(session, "create_page", json!({"name":"Temporary"}))
                .await,
        )["pageId"]
            .as_str()
            .unwrap()
            .to_string();
        server
            .call_tool(
                session,
                "get_tree_summary",
                json!({"nodeId":"root","pageId":added}),
            )
            .await
            .unwrap();
        server.call_tool(session,"create_artboard",json!({"pageId":added,"styles":{"display":"flex","flexDirection":"column","alignItems":"flex-start","width":"450px","height":"200px"}})).await.unwrap();
        assert!(paper_rs::mcp::exec::with_no_stretch(|s| s
            .iter()
            .any(|(p, _)| p == &added)));
        let manifest = p.dir.join(MANIFEST);
        let mut m: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        m["pages"]
            .as_array_mut()
            .unwrap()
            .retain(|v| v["id"] != added);
        std::fs::write(&manifest, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
        let now = value(server.call_tool(session, "get_basic_info", json!({})).await);
        assert_eq!(now["pages"].as_array().unwrap().len(), 1);
        paper_rs::mcp::exec::with_no_stretch(|s| {
            s.insert((first.clone(), node));
        });
        server.retire().await;
        paper_rs::mcp::exec::with_no_stretch(|s| {
            assert!(!s.iter().any(|(p, _)| p == &first || p == &added));
            assert!(s.remove(&(other, node)));
        });
        let _ = std::fs::remove_dir_all(root);
    }

    /// Final review, P2 2: once both files are committed the save is reported as saved, with
    /// the revision of the bytes written; no fallible step follows the commit (a failure
    /// injected right after it, step 6, has nothing to hit).
    #[tokio::test(flavor = "current_thread")]
    async fn a_completed_browser_save_is_reported_as_saved() {
        use std::hash::{BuildHasher, RandomState};
        let root = std::env::temp_dir().join(format!(
            "opd-unit-put2-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (mut p, _) = paper_rs::project::Store::open(&root)
            .unwrap()
            .create("Put")
            .unwrap();
        // Make the before timestamp distinct without depending on clock resolution.
        p.manifest.modified = 0;
        p.write_manifest().unwrap();
        let id = p.manifest.pages[0].id.clone();
        let s = Server::open(Config {
            project: p.dir.clone(),
            web_dir: root.join("no-bundle"),
            port: 1,
            token: TOKEN.into(),
        })
        .unwrap();
        let before = (
            std::fs::read(p.page_path(&id)).unwrap(),
            std::fs::read(p.dir.join(MANIFEST)).unwrap(),
        );
        let mut page = p.load_page(&id).unwrap();
        page.doc.next_id += 1;
        commit::fault::fail_at(&[6]);
        let (st, msg) = put(&s, &id, &etag(&before.0), page_to_json(&page)).await;
        commit::fault::fail_at(&[]);
        let page_now = std::fs::read(p.page_path(&id)).unwrap();
        assert_ne!(page_now, before.0);
        assert_ne!(std::fs::read(p.dir.join(MANIFEST)).unwrap(), before.1);
        assert_eq!(st, StatusCode::OK, "{msg}");
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["revision"], serde_json::json!(etag(&page_now)), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
