//! `openpaperdesign-mcp`: the openpaperdesign plugin's MCP server, over stdio.
//!
//! ```text
//! openpaperdesign-mcp --codex-context                the packaged Codex plugin
//! openpaperdesign-mcp [--workspace <dir>]            standalone MCP over stdio
//! openpaperdesign-mcp doctor [--workspace <dir>]     setup check, as JSON
//! openpaperdesign-mcp --version
//! ```
//!
//! The plugin pins workspace and supported permissions from Codex's per-call context.
//! Standalone mode uses the working directory (or deliberate `--workspace`). Nothing
//! is open until the agent binds a project deliberately: `bind_project` (an existing paper-rs
//! project inside the workspace) or `create_project` (a new one under `<workspace>/design/`).
//! The bound project is then served by the B2a server, in this process, on 127.0.0.1 with a
//! fresh token. Every upstream tool call goes to that server's host, with the same policy,
//! write lock, atomic saves and revisions as its HTTP `/mcp` endpoint. `open_editor` returns
//! the loopback editor URL (with the token in its fragment) to open in the host's browser;
//! the URL is never written to disk. One project per process; binding again switches.
//!
//! Switching is safe in two ways. The new project is opened and fully checked (containment,
//! `project.json`, page ids, links) before the current one is touched, so a failed bind
//! leaves the current binding, its server and its editor URL as they were. And once the
//! browser editor has been opened, switching stops the server that tab saves to, so an edit
//! there that isn't saved yet could no longer be saved: the switch then needs `force: true`,
//! and the refusal says why. Edits made through the tools are saved by each call. When this
//! process exits, the same applies to an open tab, without a prompt.
//!
//! `openpaperdesign-mcp --app-library <dir>` (what the packaged plugin starts, with Codex's
//! `${PLUGIN_DATA}`) is a separate mode: the home view's direct actions work on canvases the app
//! owns in that folder (see `crate::library` and `launcher_app.rs`), with no workspace and no
//! project tool. It can't be combined with `--codex-context` or `--workspace`.

use crate::agent::offered_tools;
use crate::{Server, bundle_file_names, new_token, router};
use paper_rs::mcp::server::to_result;
use paper_rs::mcp::{Out, ToolResult, tools};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    RequestMetaObject, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(not(test))]
include!(concat!(env!("OUT_DIR"), "/bundle.rs"));
#[cfg(test)]
use tests::BUNDLE_FILES;

#[path = "launcher_inventory.rs"]
mod inventory;
#[path = "launcher_scope.rs"]
mod scope;
use scope::{CAPABILITY, Workspace};
#[path = "launcher_app.rs"]
mod app;
#[path = "launcher_direct.rs"]
mod direct;
use crate::library::Library;
use tokio::sync::{Mutex, OwnedMutexGuard};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const SWITCH_NEEDS_FORCE: &str = "The browser editor for the bound project was opened \
    (open_editor). Switching stops the server that browser tab saves to, so any edit there that \
    isn't saved yet (the page shows an unsaved change or \"Saving…\") would be lost. Your own \
    tool edits are already saved by each call. Ask the person to let their edits save (the \
    page shows \"Saved\") or to accept losing them, then call again with force: true.";

/// The home: one global-entry tool and its static MCP App view, plus the read-only native
/// project inventory it shows. All three are answered before workspace admission because they
/// touch none of this session's state: no context, binding, project, listener or workspace
/// file. They never read or change it, so they cannot pin, inspect or retire a binding. The
/// inventory reads Codex's own project list through its CLI; a listed root is discovery, never
/// authority. Every other tool keeps per-call admission unchanged.
const HOME_TOOL: &str = "openpaperdesign_home";
const PROJECTS_TOOL: &str = "openpaperdesign_projects";
const INVENTORY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
/// One inventory child process at a time per session.
static INVENTORY_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Versioned: a view whose code changed incompatibly gets a new URI. Hosts may cache a view by
/// its URI, so a fresh tool listing or resource read gets this view under a key no older view
/// used, and a read of an earlier URI is refused. That neither evicts nor refreshes a view a host
/// already has open, nor tells this server which view version is calling it. `home-2` added the
/// in-panel editor module; `home-3` makes the home browser-first: New, Open, Import and Open
/// current canvas ask for the loopback web editor link and open it, and the in-panel editor is
/// only the optional "Edit in this view". No earlier URI is served.
const HOME_URI: &str = "ui://openpaperdesign/home-3.html";
const HOME_MIME: &str = "text/html;profile=mcp-app";
const HOME_HTML: &str = include_str!("home.html");
/// Geist Pixel Square (SIL OFL 1.1, `assets/geist-OFL.txt`), unmodified, for the home's
/// `onlinesourdough` wordmark. Served inside the view as a data URI: no font URL or service.
const WORDMARK_FONT: &[u8] = include_bytes!("../assets/geist-pixel-square.woff2");
const WORDMARK_FONT_SLOT: &str = "__OPD_WORDMARK_FONT__";
/// The view's one mode slot: `library` in an app-library process, `workspace` otherwise. It is
/// fixed per process and carries no path, id or state.
const MODE_SLOT: &str = "__OPD_MODE__";

/// The home view as served: `home.html` with the wordmark font in its one slot.
#[allow(dead_code)]
fn home_html() -> &'static str {
    home_html_for(false)
}

/// The home view for a workspace or an app-library process. An app-library view also gets
/// the in-panel editor module in its one editor slot, when this build has it (`panel_editor`);
/// otherwise the slot keeps its `null` and the view offers the browser editor link instead.
fn home_html_for(library: bool) -> &'static str {
    static WORKSPACE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    static LIBRARY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let (cell, mode) = if library {
        (&LIBRARY, "library")
    } else {
        (&WORKSPACE, "workspace")
    };
    cell.get_or_init(|| {
        let html = HOME_HTML
            .replacen(WORDMARK_FONT_SLOT, &base64(WORDMARK_FONT), 1)
            .replacen(MODE_SLOT, mode, 1);
        match panel_editor() {
            Ok(module) if library => html.replacen(PANEL_SLOT, module, 1),
            _ => html,
        }
    })
}

/// The home's in-panel editor slot: a line of its one inline module. Left as it is, the view
/// has no in-panel editor (`opdCreatePanel` is null).
const PANEL_SLOT: &str = "const opdCreatePanel = null; // __OPD_PANEL_MODULE__";
/// The packaged bundle files the in-panel editor module is made of, in this order. Only these
/// fixed, embedded files: never a path, URL or project file. The same order is assembled by
/// `crates/openpaperdesign-web/tests/panel.test.mjs`.
const PANEL_PARTS: [&str; 5] = [
    "openpaperdesign_web.js",
    "session.js",
    "page.js",
    "editor.js",
    "panel.js",
];
/// The packaged renderer, sent to the in-panel editor in bounded parts (`app::ASSET`).
const WASM_FILE: &str = "openpaperdesign_web_bg.wasm";

fn bundle_file(files: crate::EmbeddedBundle, name: &str) -> Option<&'static [u8]> {
    files
        .iter()
        .find(|(f, _)| *f == name)
        .map(|(_, b)| *b)
        .filter(|b| !b.is_empty())
}

/// The packaged renderer and its identity (a hash of its bytes), if this build has it. The
/// in-panel module carries the same identity, so parts of another build are refused.
fn wasm() -> Option<(&'static [u8], &'static str)> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let bytes = bundle_file(BUNDLE_FILES, WASM_FILE)?;
    let id = ID.get_or_init(|| Some(crate::etag(bytes))).as_deref()?;
    Some((bytes, id))
}

/// The in-panel editor module of this build, or why there is none.
fn panel_editor() -> Result<&'static str, &'static str> {
    static MODULE: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    MODULE
        .get_or_init(|| {
            let (_, id) = wasm().ok_or("this build has no packaged renderer")?;
            panel_module(BUNDLE_FILES, id)
        })
        .as_ref()
        .map(String::as_str)
        .map_err(String::as_str)
}

/// One inline ES module from the fixed parts (`PANEL_PARTS`), after a line naming the renderer
/// identity. Each part must be present, UTF-8 and free of `import` declarations (they have
/// none; a module inlined in the view can't import), and the whole must be safe inside the
/// view's `<script type="module">`: no `</script`, `<script` or `<!--` anywhere (case-
/// insensitive). Nothing is evaluated, fetched or loaded by URL: the module's own code calls
/// the glue's init with the renderer bytes it was sent.
fn panel_module(files: crate::EmbeddedBundle, identity: &str) -> Result<String, String> {
    let mut out = format!(
        "// The in-panel editor (assembled by openpaperdesign-mcp from its packaged files).\n\
         const OPD_WASM_IDENTITY = {};\n",
        serde_json::to_string(identity).expect("a string serializes")
    );
    for name in PANEL_PARTS {
        let bytes =
            bundle_file(files, name).ok_or_else(|| format!("{name} isn't in this build"))?;
        let text = std::str::from_utf8(bytes).map_err(|_| format!("{name} isn't UTF-8"))?;
        if text.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("import ") || t.starts_with("import{") || t.starts_with("import(")
        }) {
            return Err(format!(
                "{name} has an import, which an inline module can't load"
            ));
        }
        out.push_str(&format!("\n// ---- {name} ----\n"));
        out.push_str(text);
        out.push('\n');
    }
    let lower = out.to_ascii_lowercase();
    for bad in ["</script", "<script", "<!--"] {
        if lower.contains(bad) {
            return Err(format!(
                "the editor code contains `{bad}`, so it can't be put inside the view"
            ));
        }
    }
    if !out.contains("function opdCreatePanel(") {
        return Err("panel.js doesn't define opdCreatePanel".into());
    }
    Ok(out)
}

const NOT_BOUND: &str = "No design project is bound yet. Call bind_project with the path of an \
    existing paper-rs project folder inside this workspace, or create_project to make a new one \
    under design/.";

fn bundle_complete() -> bool {
    let names = bundle_file_names();
    names
        .iter()
        .all(|n| BUNDLE_FILES.iter().any(|(f, _)| f == n))
}

/// Why this workspace can't be used for binding, if it can't: never the filesystem root or
/// the user's home folder itself (that would put every project of the user in reach).
fn workspace_problem(ws: &Path) -> Option<String> {
    if ws.parent().is_none() {
        return Some("the workspace is the filesystem root".into());
    }
    let home = std::env::var_os("HOME").and_then(|h| PathBuf::from(h).canonicalize().ok());
    if home.as_deref() == Some(ws) {
        return Some("the workspace is your home folder".into());
    }
    // Neither standalone cwd nor a host-selected scope may use the plugin folder.
    if ws.join("mcp.json").is_file() && ws.join("skills/openpaperdesign/SKILL.md").is_file() {
        return Some("the workspace is the openpaperdesign plugin folder, not your project".into());
    }
    None
}

/// A directory's identity: (device, inode).
#[cfg(unix)]
fn identity(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}
#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// `path` is a real directory (not a symlink to one).
fn real_dir(path: &Path) -> Result<std::fs::Metadata, String> {
    let m = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if m.file_type().is_symlink() || !m.is_dir() {
        return Err(format!("{} is not a real directory", path.display()));
    }
    Ok(m)
}

struct Bound {
    server: Arc<Server>,
    port: u16,
    token: String,
    task: tokio::task::JoinHandle<()>,
    /// `open_editor` gave out this server's URL, so a browser tab may be editing it.
    editor_opened: bool,
    /// Whose admitted authority installed this binding.
    owner: Owner,
    /// An app-library canvas: its id, and its lease. The binding owns the lease only to hold it
    /// (it is never read): the lock is released when this binding is dropped, which every
    /// switch and close does only after the server was retired.
    doc: Option<String>,
    _lease: Option<Arc<crate::library::Lease>>,
    /// An app-library binding's own identity (`<canvas id>.<n>`, unique in this process), which
    /// views send back so a stale action or confirmation is refused. Not a credential and not
    /// a path; it authorizes nothing.
    binding: Option<String>,
    /// In-panel editors of this app-library binding that read its page and haven't been
    /// released (Back, Close or the view's teardown release them; a view that vanished never
    /// does, which only makes Close and Switch ask for confirmation). Like `binding`, a mount
    /// number is a stale-view check, never a credential. Each mount has its lifetime flag: a
    /// release clears it (under the state lock), and a save admitted for that mount checks it
    /// again under the writer fence (`Server::save_page_while`).
    mounts: std::collections::BTreeMap<u64, Arc<std::sync::atomic::AtomicBool>>,
    next_mount: u64,
}

/// The origin of admitted authority: a model call's context, or a direct home-view action's
/// own context. A binding (or a pin without one) the view admitted is closed when a view call
/// observes that context revoked; app input never retires what a model call admitted.
/// `Library` is an app-library canvas, in an app-library process only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Model,
    Direct,
    Library,
}

impl Drop for Bound {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct Launcher {
    state: Arc<Mutex<LauncherState>>,
    /// The one MCP client on stdio, as a session of the bound project's host.
    session: u64,
    /// Set in an app-library process (`--app-library`): then only the library's own actions,
    /// the home and the read-only inventory are answered.
    library: Option<Arc<Library>>,
}

struct LauncherState {
    host_context: bool,
    workspace: Option<Workspace>,
    /// Who pinned `workspace`; any successful model admission makes it `Model`.
    pinned_by: Owner,
    invalidated: Option<String>,
    bound: Option<Bound>,
    /// Set by [`Launcher::shutdown`] at the end of this process's service: nothing may create
    /// a project or install a binding any more.
    shut_down: bool,
}

/// A creation or binding that starts after the service ended (a request still in flight when
/// the connection closed) is refused before it writes or installs anything.
const SHUT_DOWN: &str = "This OpenPaperDesign server is shutting down; nothing was created, \
    opened or changed.";

fn refuse_if_shut_down(state: &LauncherState) -> Result<(), String> {
    if state.shut_down {
        Err(SHUT_DOWN.into())
    } else {
        Ok(())
    }
}

/// What one project creation made, as established when it happened: each folder this call
/// created (its identity, taken right after `create_dir`) and each file this call finished
/// writing (its identity and content revision, taken after the write). A failure before the
/// binding is installed removes only entries that are still exactly those, deepest first: a
/// file with the same identity and bytes, a folder with the same identity at its canonical path
/// and only when empty. Anything replaced, changed, moved or not known to be this call's stays
/// and is named. The check-then-remove window is the remaining path race; this is not crash
/// atomic and doesn't serialize other writers.
#[derive(Default)]
struct Made {
    dirs: Vec<(PathBuf, Option<(u64, u64)>)>,
    files: Vec<(PathBuf, Option<FileState>)>,
}

/// A file's (device, inode) identity and content revision.
type FileState = ((u64, u64), String);

impl Made {
    /// Record a folder this call just created.
    fn dir(&mut self, path: PathBuf) {
        let id = real_dir(&path).ok().map(|m| identity(&m));
        self.dirs.push((path, id));
    }

    /// Record a file this call just finished writing.
    fn file(&mut self, path: PathBuf) {
        let owned = Self::file_state(&path);
        self.files.push((path, owned));
    }

    /// A regular file's identity and content revision, read at its canonical path.
    fn file_state(path: &Path) -> Option<FileState> {
        let m = std::fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.is_file())?;
        if path.canonicalize().ok().as_deref() != Some(path) {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        Some((identity(&m), crate::etag(&bytes)))
    }

    /// Remove what is still exactly what was made; returns the paths that stay. A path counts
    /// as gone only when it is observed missing (NotFound): a failed observation (permission
    /// denied, I/O error) leaves it uncertain, so it is kept and named, never assumed absent.
    fn undo(&self) -> Vec<PathBuf> {
        let mut left = vec![];
        let gone = |p: &Path| matches!(std::fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        for (f, owned) in self.files.iter().rev() {
            let ours = owned.is_some() && Self::file_state(f) == *owned;
            let removed = ours && std::fs::remove_file(f).is_ok();
            if !(removed || gone(f)) {
                left.push(f.clone());
            }
        }
        for (d, id) in self.dirs.iter().rev() {
            let ours = id.is_some()
                && real_dir(d).ok().map(|m| identity(&m)) == *id
                && d.canonicalize().ok().as_deref() == Some(d.as_path());
            let removed = ours && std::fs::remove_dir(d).is_ok();
            if !(removed || gone(d)) {
                left.push(d.clone());
            }
        }
        left
    }

    /// The error for a failed creation, after undoing it: says whether anything stayed.
    fn fail(&self, error: String) -> String {
        let left = self.undo();
        if left.is_empty() {
            format!("{error}. Nothing was created.")
        } else {
            let names: Vec<String> = left.iter().map(|p| p.display().to_string()).collect();
            format!(
                "{error}. The new project could not be fully removed; these paths remain (not \
                 empty, changed, or not known to be this call's): {}",
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
use tests::Listener;
#[cfg(not(test))]
use tokio::net::TcpListener as Listener;

/// A binding prepared up to the infallible handoff.
struct Install {
    listener: Listener,
    port: u16,
    token: String,
    rel: String,
    prepared: crate::PreparedServer,
    /// For an app-library canvas: its id and lease, moved into the installed binding.
    doc: Option<String>,
    lease: Option<Arc<crate::library::Lease>>,
}

fn text(v: Value) -> ToolResult {
    Ok(vec![Out::Text(serde_json::to_string_pretty(&v).unwrap())])
}

fn own_tools() -> Vec<Tool> {
    [
        json!({
            "name": "bind_project",
            "description": "Bind the design project to edit: an existing paper-rs project folder \
                (it has a project.json) inside this workspace, as a path relative to the \
                workspace or absolute. Paths outside the workspace, symlinks leading out of it, \
                and the home folder are refused. Binding again switches projects; a failed bind \
                keeps the current one. If the browser editor was opened, switching needs \
                force: true, because unsaved edits in that browser tab would be lost. Required \
                before any editing tool.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Project folder, e.g. design/landing"},
                "force": {"type": "boolean", "description": "Switch even though the browser editor of the current project was opened; unsaved edits in that tab are lost."}
            }, "required": ["path"]}
        }),
        json!({
            "name": "create_project",
            "description": "Create a new, empty paper-rs project under <workspace>/design/<name>/ \
                and bind it. Refuses a name whose folder already exists (bind that instead). \
                Switching away from a project whose browser editor was opened needs force: true, \
                as for bind_project.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"},
                "force": {"type": "boolean", "description": "As for bind_project."}
            }, "required": ["name"]}
        }),
        json!({
            "name": "open_editor",
            "description": "Start (if needed) and return the editable canvas URL for the bound \
                project: http://127.0.0.1:<port>/#token=<token>. Open it in the host's in-app \
                browser; the person's edits there and your tool edits land in the same project \
                and show up in each other live. The URL holds a per-session token: don't write \
                it into files.",
            "inputSchema": {"type": "object", "properties": {}},
            "_meta": result_view()
        }),
        json!({
            "name": "openpaperdesign_status",
            "description": "Report what is set up and what is bound: version, workspace, bound \
                project, workspace provenance/permission admission or refusal reason, and \
                whether the browser editor is available.",
            "inputSchema": {"type": "object", "properties": {}},
            "_meta": result_view()
        }),
        json!({
            "name": HOME_TOOL,
            "title": "OpenPaperDesign",
            "description": "Open the OpenPaperDesign home: a static view (the Codex sidebar entry). \
                It reads no workspace, project or binding and changes nothing. To work on a \
                canvas use openpaperdesign_status, then bind_project or create_project, then \
                open_editor.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "icons": icons(),
            "_meta": {
                "ui": {"resourceUri": HOME_URI},
                // Global: the sidebar. Thread: a tab inside a chat, whose model calls carry
                // that chat's own workspace context. Neither entry grants a workspace.
                "openai/ui": {"entrypoints": [{"type": "global"}, {"type": "thread"}]}
            }
        }),
        json!({
            "name": "openpaperdesign_canvases",
            "description": "List the existing design canvases (paper-rs projects) directly under \
                this workspace's design/ folder: project path, canvas name and id, page count, \
                and which one is bound. Read-only: it creates nothing (not even design/), binds \
                nothing, follows no links and counts as reading no page. Needs the same \
                workspace context as every other tool.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "_meta": result_view()
        }),
        json!({
            "name": PROJECTS_TOOL,
            "description": "List the Codex projects saved on this computer (read-only, from the \
                Codex CLI's app-server: project/list, or project/read with projectId), each \
                with its id and every root folder. Discovery only: a listed root is not this \
                chat's workspace and grants nothing; openpaperdesign_status shows the \
                workspace you can use. Reports available: false with a reason when the list \
                can't be read.",
            "inputSchema": {"type": "object", "properties": {
                "projectId": {"type": "string", "maxLength": 256, "description": "Read just this project (an id from the list)."}
            }, "additionalProperties": false}
        }),
    ]
    .into_iter()
    .chain(direct::tools())
    .map(|t| serde_json::from_value(t).expect("valid tool"))
    .collect()
}

/// The plugin's mark, from its packaged assets (`plugin/openpaperdesign/assets`), as static
/// SVG data URIs: dark ink for light backgrounds, light ink for dark ones. Used for the home
/// entry (tool `icons`) and as the server icon fallback (`serverInfo.icons`); no URL, file
/// access or listener is involved.
const ICON_LIGHT: &[u8] = include_bytes!("../../../plugin/openpaperdesign/assets/icon.svg");
const ICON_DARK: &[u8] = include_bytes!("../../../plugin/openpaperdesign/assets/icon-dark.svg");

fn icons() -> Value {
    let icon = |svg: &[u8], theme: &str| {
        json!({
            "src": format!("data:image/svg+xml;base64,{}", base64(svg)),
            "mimeType": "image/svg+xml",
            "sizes": ["any"],
            "theme": theme,
        })
    };
    json!([icon(ICON_LIGHT, "light"), icon(ICON_DARK, "dark")])
}

/// Standard base64 with padding (RFC 4648), for the icon data URIs.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The home view also presents the model's own status and editor results. Model-only: the
/// view itself never calls these tools (it has no workspace context and would be refused).
fn result_view() -> Value {
    json!({"ui": {"resourceUri": HOME_URI, "visibility": ["model"]}})
}

/// `openpaperdesign_home`: answered without workspace admission and without touching state.
fn home(args: Option<&serde_json::Map<String, Value>>) -> ToolResult {
    if args.is_some_and(|a| !a.is_empty()) {
        return Err(format!(
            "{HOME_TOOL} takes no arguments; it only opens the home view."
        ));
    }
    text(json!({
        "surface": "home",
        "version": VERSION,
        "note": "Static home view. Nothing was read or changed. Use openpaperdesign_status to check this chat's workspace.",
    }))
}

/// `openpaperdesign_projects`: the native inventory, answered without workspace admission and
/// without touching state. Only `projectId` is accepted. `cancelled` is the MCP request's
/// cancellation: the inventory child is stopped and waited for before the inventory is free
/// again (a cancelled request's reply is not sent).
async fn projects(
    args: Option<&serde_json::Map<String, Value>>,
    cancelled: impl std::future::Future<Output = ()>,
) -> ToolResult {
    let empty = serde_json::Map::new();
    let args = args.unwrap_or(&empty);
    if let Some(k) = args.keys().find(|k| *k != "projectId") {
        return Err(format!(
            "{PROJECTS_TOOL} accepts only projectId, not `{k}`."
        ));
    }
    let id = match args.get("projectId") {
        None => None,
        Some(Value::String(s))
            if !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control) =>
        {
            Some(s.as_str())
        }
        Some(_) => return Err("projectId must be a non-empty project id string.".into()),
    };
    use std::sync::atomic::Ordering;
    if INVENTORY_BUSY.swap(true, Ordering::AcqRel) {
        return Err("The Codex project list is already being read; try again.".into());
    }
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            INVENTORY_BUSY.store(false, Ordering::Release);
        }
    }
    let _release = Release;
    let query = id.map_or(inventory::Query::List, inventory::Query::Read);
    // The normal command route: `codex` on this process's PATH. No fixed install path.
    let read = inventory::read(
        std::ffi::OsStr::new("codex"),
        query,
        INVENTORY_DEADLINE,
        cancelled,
    );
    match read.await {
        Ok(v) => text(v),
        Err(why) => text(why.to_json()),
    }
}

impl Workspace {
    fn relative(&self, dir: &Path) -> String {
        let rel = dir
            .strip_prefix(&self.path)
            .unwrap_or(dir)
            .display()
            .to_string();
        if rel.is_empty() { ".".into() } else { rel }
    }

    /// The workspace is still the directory recorded at start.
    fn check_workspace(&self) -> Result<(), String> {
        let replaced = || {
            format!(
                "the workspace {} was moved or replaced since this server started; refusing. \
                 Restart the agent in the project folder.",
                self.path.display()
            )
        };
        let m = real_dir(&self.path).map_err(|_| replaced())?;
        if identity(&m) != self.id
            || self.path.canonicalize().ok().as_deref() != Some(self.path.as_path())
        {
            return Err(replaced());
        }
        Ok(())
    }

    /// The existing paper-rs projects directly under `<workspace>/design/`, read-only. Runs only
    /// after workspace admission. Each candidate must be a real directory on the workspace's
    /// file system (links are reported, never followed) with a real `project.json` that passes
    /// the same checks as binding. Nothing is created: no `design/`, project, server, layout
    /// entry or page-read acknowledgement. The scan is bounded and says when it stopped early.
    fn canvases(&self, bound: Option<&Path>) -> ToolResult {
        const MAX_SCAN: usize = 1000;
        const MAX_CANVASES: usize = 200;
        self.check_workspace()?;
        if let Some(p) = workspace_problem(&self.path) {
            return Err(format!("refusing to list canvases: {p}"));
        }
        let design = self.path.join("design");
        self.project_path(&design)?;
        let mut out = serde_json::Map::new();
        out.insert("workspace".into(), json!(self.path));
        let present = crate::plain(&design, crate::Kind::Dir)
            .map_err(|e| format!("design/ can't be listed: {e}"))?;
        out.insert("design_present".into(), json!(present));
        let (mut canvases, mut skipped, mut complete) = (vec![], vec![], true);
        if present {
            let design_dev = identity(&real_dir(&design)?).0;
            let entries = std::fs::read_dir(&design).map_err(|e| format!("design/: {e}"))?;
            for (n, entry) in entries.enumerate() {
                if n >= MAX_SCAN || canvases.len() >= MAX_CANVASES {
                    complete = false;
                    break;
                }
                let entry = entry.map_err(|e| format!("design/: {e}"))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    skipped.push(
                        json!({"project": null, "problem": "a folder name that isn't valid UTF-8"}),
                    );
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                let rel = format!("design/{name}");
                let dir = design.join(name);
                let mut skip =
                    |problem: String| skipped.push(json!({"project": rel, "problem": problem}));
                if let Err(e) = self.project_path(&dir) {
                    skip(e);
                    continue;
                }
                let m = match std::fs::symlink_metadata(&dir) {
                    Ok(m) => m,
                    Err(e) => {
                        skip(e.to_string());
                        continue;
                    }
                };
                if m.file_type().is_symlink() {
                    skip("a symbolic link (not followed)".into());
                    continue;
                }
                if !m.is_dir() {
                    continue;
                }
                if identity(&m).0 != design_dev {
                    skip("on another file system (not followed)".into());
                    continue;
                }
                match crate::plain(&dir.join(paper_rs::project::MANIFEST), crate::Kind::File) {
                    Ok(false) => continue, // not a paper-rs project
                    Err(e) => {
                        skip(e);
                        continue;
                    }
                    Ok(true) => {}
                }
                let checked = crate::check_project_dir(&dir)
                    .and_then(|()| crate::read_manifest_checked(&dir));
                match checked {
                    Ok(manifest) => canvases.push(json!({
                        "project": rel,
                        "path": dir,
                        "bound": bound == Some(dir.as_path()),
                        "canvas": {
                            "id": manifest.id,
                            "name": manifest.name,
                            "pages": manifest.pages.len(),
                            "modified": manifest.modified,
                        },
                    })),
                    Err(e) => skip(e),
                }
            }
        }
        canvases.sort_by(|a, b| a["project"].as_str().cmp(&b["project"].as_str()));
        skipped.sort_by(|a, b| a["project"].as_str().cmp(&b["project"].as_str()));
        out.insert("complete".into(), json!(complete));
        out.insert("canvases".into(), json!(canvases));
        out.insert("skipped".into(), json!(skipped));
        out.insert(
            "note".into(),
            json!(
                "Read-only discovery: nothing was created or bound, and no page counts as read. \
                   To open one, bind_project its project path, read the page you will change \
                   (get_tree_summary root with its pageId), then call open_editor."
            ),
        );
        text(Value::Object(out))
    }

    /// Create `design/<folder>/pages/` without following links: each component is made with
    /// `create_dir` (which never follows a final symlink) or must already be a real
    /// directory, and the result must still be inside the unchanged workspace. Returns the
    /// folders this call made, so a later failure can remove exactly those; a failure here
    /// removes them itself.
    fn create_folders(&self, folder: &str) -> Result<(PathBuf, Made), String> {
        self.check_workspace()?;
        self.project_path(&self.path.join("design").join(folder))?;
        let mut made = Made::default();
        let design = self.path.join("design");
        match std::fs::create_dir(&design) {
            Ok(()) => made.dir(design.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("design/: {e}")),
        }
        real_dir(&design)
            .map_err(|_| made.fail("design/ is not a real folder (a link?); refusing".into()))?;
        let dir = design.join(folder);
        match std::fs::create_dir(&dir) {
            Ok(()) => made.dir(dir.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(made.fail(format!(
                    "design/{folder} already exists; bind it with bind_project"
                )));
            }
            Err(e) => return Err(made.fail(format!("design/{folder}: {e}"))),
        }
        let pages = dir.join(paper_rs::project::PAGES_DIR);
        std::fs::create_dir(&pages)
            .map_err(|e| made.fail(format!("design/{folder}/pages: {e}")))?;
        made.dir(pages.clone());
        // Verify what was made before writing any file into it.
        self.check_workspace().map_err(|e| made.fail(e))?;
        for d in [&design, &dir, &pages] {
            real_dir(d).map_err(|e| made.fail(e))?;
        }
        if dir.canonicalize().ok().as_deref() != Some(dir.as_path()) {
            return Err(
                made.fail("the new project folder isn't where it was made; refusing".into())
            );
        }
        Ok((dir, made))
    }

    fn resolve_inside(&self, path: &str) -> Result<PathBuf, String> {
        self.check_workspace()?;
        if let Some(p) = workspace_problem(&self.path) {
            return Err(format!(
                "refusing to bind: {p}. Start the agent in a project folder instead."
            ));
        }
        let p = Path::new(path.trim());
        self.project_path(p)?;
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.path.join(p)
        };
        let real = joined
            .canonicalize()
            .map_err(|e| format!("{}: {e}", joined.display()))?;
        if !real.starts_with(&self.path) {
            return Err(format!(
                "{} is outside the workspace {} (symlinks are followed before this check)",
                joined.display(),
                self.path.display()
            ));
        }
        self.project_path(&real)?;
        if workspace_problem(&real).is_some() {
            return Err("refusing to bind the home folder or filesystem root".into());
        }
        Ok(real)
    }
}

impl Launcher {
    fn new(workspace: Option<Workspace>) -> Self {
        Self {
            state: Arc::new(Mutex::new(LauncherState {
                host_context: workspace.is_none(),
                workspace,
                pinned_by: Owner::Model,
                invalidated: None,
                bound: None,
                shut_down: false,
            })),
            session: crate::agent::new_session(),
            library: None,
        }
    }

    /// An app-library process: no workspace, no host context, and only library actions.
    fn with_library(library: Library) -> Self {
        let mut l = Self::new(None);
        l.library = Some(Arc::new(library));
        // Never consulted in library mode (no call reaches workspace admission); false keeps
        // that true even if one did.
        l.state = Arc::new(Mutex::new(LauncherState {
            host_context: false,
            workspace: None,
            pinned_by: Owner::Library,
            invalidated: None,
            bound: None,
            shut_down: false,
        }));
        l
    }

    /// The normal end of this process's service (stdio closed, or the service failed): from now
    /// on nothing creates a project or installs a binding, and the final binding is retired as
    /// Close and Switch retire one: admission closes, a started browser save or MCP call and
    /// its rollback drain, queued and late HTTP, MCP and browser writers are refused, and only
    /// then are the binding and its lease dropped. A request still running when the connection
    /// closed finishes first (it holds the state lock). Calling it again is harmless.
    async fn shutdown(&self) {
        if let Err(e) = self.begin_shutdown().await {
            eprintln!("openpaperdesign-mcp: shutdown stopped unexpectedly: {e}");
        }
    }

    /// The shutdown as an owned task, spawned before anything waits (not even for the state
    /// lock): a caller cancelled at any point, including while a request in flight still holds
    /// the state, leaves it running, so the lease is still kept until the drain is done.
    fn begin_shutdown(&self) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut state = state.lock_owned().await;
            state.shut_down = true;
            if let Some(b) = state.bound.as_ref() {
                b.server.retire().await;
            }
            state.bound = None;
        })
    }

    fn status(state: &LauncherState, problem: Option<&str>) -> ToolResult {
        let current_problem = problem.map(str::to_owned).or_else(|| {
            state.workspace.as_ref().and_then(|ws| {
                ws.check_workspace()
                    .err()
                    .or_else(|| workspace_problem(&ws.path))
            })
        });
        text(json!({
            "version": VERSION,
            "workspace": state.workspace.as_ref().map(|ws| &ws.path),
            "workspace_source": state.workspace.as_ref().map_or(CAPABILITY, |ws| ws.source),
            "workspace_state": if state.invalidated.is_some() { "retired" } else if state.workspace.is_some() { "pinned" } else { "unresolved" },
            "workspace_usable": state.workspace.is_some() && current_problem.is_none() && state.invalidated.is_none(),
            "workspace_permission_mode": state.workspace.as_ref().filter(|_| current_problem.is_none() && state.invalidated.is_none()).map(|ws| ws.permission),
            "workspace_problem": current_problem,
            "bound_project": state.bound.as_ref().map(|b| b.server.dir()),
            "editor_opened": state.bound.as_ref().is_some_and(|b| b.editor_opened),
            "browser_editor": if bundle_complete() { "built in" } else { "not built (open_editor unavailable)" },
        }))
    }

    /// Refuse to switch away from a project whose browser editor was opened, unless forced.
    fn may_switch(bound: &Option<Bound>, to: Option<&Path>, force: bool) -> Result<(), String> {
        match bound {
            Some(b) if b.editor_opened && !force && to != Some(b.server.dir()) => {
                Err(SWITCH_NEEDS_FORCE.into())
            }
            _ => Ok(()),
        }
    }

    /// Close this session's admission (when it has a workspace), then drain the bound server's
    /// writers and rollback through its fence and drop the binding. The drain is owned by a
    /// spawned task, so cancelling the caller can't stop it halfway; admission stays locked
    /// until it completes. Returns the still-held state.
    async fn retire_session(
        mut state: OwnedMutexGuard<LauncherState>,
        reason: String,
    ) -> Result<OwnedMutexGuard<LauncherState>, String> {
        if state.workspace.is_some() {
            state.invalidated = Some(reason);
        }
        tokio::spawn(async move {
            if let Some(old) = state.bound.as_ref() {
                old.server.retire().await;
            }
            state.bound = None;
            state
        })
        .await
        .map_err(|e| format!("workspace retirement failed: {e}"))
    }

    /// The fallible network resources of a binding: a loopback listener and a fresh token.
    /// Acquired before anything is written, so a failure here leaves no files.
    async fn listen(key: &Path) -> Result<(Listener, u16, String), String> {
        #[cfg(test)]
        if crate::test_support::fault(key, "listen_fails") {
            return Err("cannot listen on 127.0.0.1: injected test failure".into());
        }
        let _ = key;
        let listener = Listener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("cannot listen on 127.0.0.1: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let token = new_token().map_err(|e| e.to_string())?;
        Ok((listener, port, token))
    }

    /// Open and check the project completely, without touching the current binding: an error
    /// here leaves the current one (and its server) unchanged.
    fn prepare_install(
        ws: &Workspace,
        dir: PathBuf,
        (listener, port, token): (Listener, u16, String),
    ) -> Result<Install, String> {
        let rel = ws.relative(&dir);
        let prepared = Server::prepare_embedded(dir, BUNDLE_FILES, port, token.clone())
            .map_err(|e| e.to_string())?;
        Ok(Install {
            listener,
            port,
            token,
            rel,
            prepared,
            doc: None,
            lease: None,
        })
    }

    /// Install a prepared binding: no fallible preparation remains. The transition is owned
    /// independently of the request, so cancelling a response can't strand the current binding
    /// halfway retired. With `pin`, an unpinned session is pinned in this same step (a direct
    /// view action whose admission didn't pin), so pin and binding appear together or not at all.
    async fn install(
        mut state: OwnedMutexGuard<LauncherState>,
        install: Install,
        pin: Option<Workspace>,
        owner: Owner,
    ) -> ToolResult {
        // Never a new binding after the service ended; the prepared server is never activated.
        refuse_if_shut_down(&state)?;
        let Install {
            listener,
            port,
            token,
            rel,
            prepared,
            doc,
            lease,
        } = install;
        tokio::spawn(async move {
            if let Some(old) = state.bound.as_ref() {
                old.server.retire().await;
                old.task.abort();
            }
            // The old binding (and its lease, if any) is dropped below, after this drain.
            // Every old writer/rollback has finished; queued and retained old handles are
            // permanently fenced. Only now may the replacement reset its same-id entries.
            let server = prepared.activate();
            let app = router(server.clone());
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            if let Some(ws) = pin
                && state.workspace.is_none()
            {
                state.workspace = Some(ws);
                state.pinned_by = owner;
            }
            state.bound = Some(Bound {
                server: server.clone(),
                port,
                token,
                task,
                editor_opened: false,
                owner,
                binding: doc.as_deref().map(app::binding_id),
                doc,
                _lease: lease,
                mounts: Default::default(),
                next_mount: 0,
            });
            // This summary is not the agent's read: its session must still read each page.
            let info = server
                .call_tool(crate::agent::new_session(), "get_basic_info", json!({}))
                .await;
            let mut out = vec![Out::Text(
                json!({
                    "bound": rel,
                    "next": "before changing a page, read it: get_tree_summary with nodeId \"root\" \
                        and its pageId (this summary doesn't count); get_basic_info before renaming, \
                        adding or removing pages. Then edit with the design tools; call open_editor \
                        for the browser canvas.",
                })
                .to_string(),
            )];
            if let Ok(more) = info {
                out.extend(more);
            }
            Ok(out)
        })
        .await
        .unwrap_or_else(|e| Err(format!("binding handoff failed unexpectedly: {e}")))
    }

    /// Bind `dir` (see `install` for `pin`); `owner` is whose authority admitted it.
    async fn bind(
        state: OwnedMutexGuard<LauncherState>,
        ws: Workspace,
        dir: PathBuf,
        force: bool,
        pin: bool,
        owner: Owner,
    ) -> ToolResult {
        refuse_if_shut_down(&state)?;
        if let Some(b) = state.bound.as_ref()
            && b.server.dir() == dir
        {
            return text(
                json!({"bound": ws.relative(&dir), "note": "already bound; nothing changed"}),
            );
        }
        Self::may_switch(&state.bound, Some(&dir), force)?;
        let listening = Self::listen(&ws.path).await?;
        let install = Self::prepare_install(&ws, dir, listening)?;
        Self::install(state, install, pin.then_some(ws), owner).await
    }

    /// An existing paper-rs project inside the admitted workspace (as `bind_project` takes it).
    fn existing(ws: &Workspace, path: &str) -> Result<PathBuf, String> {
        let dir = ws.resolve_inside(path)?;
        if !dir.join(paper_rs::project::MANIFEST).is_file() {
            return Err(format!(
                "{} is not a paper-rs project (no project.json). Use create_project for a new one.",
                dir.display()
            ));
        }
        Ok(dir)
    }

    /// `create_project`: a new project under `<workspace>/design/<folder>/`, then bound.
    /// Returns the new project's folder with the bind result. Nothing is written until the
    /// binding's listener exists, and a later failure before the binding is installed removes
    /// exactly what this call made (under the same admission lock), or names what it couldn't.
    async fn create(
        state: OwnedMutexGuard<LauncherState>,
        ws: Workspace,
        name: &str,
        force: bool,
        pin: bool,
        owner: Owner,
    ) -> Result<(PathBuf, Vec<Out>), String> {
        // Before creating anything.
        refuse_if_shut_down(&state)?;
        Self::may_switch(&state.bound, None, force)?;
        ws.check_workspace()?;
        if let Some(p) = workspace_problem(&ws.path) {
            return Err(format!("refusing to create a project: {p}"));
        }
        let listening = Self::listen(&ws.path).await?;
        let folder = paper_rs::project::folder_name(name);
        let (dir, mut made) = ws.create_folders(&folder)?;
        // As upstream `Store::create`, but into the folder made and checked above.
        let manifest = paper_rs::project::Manifest::new(name.trim());
        let page = paper_rs::project::PageData {
            id: manifest.pages[0].id.clone(),
            name: manifest.pages[0].name.clone(),
            doc: paper_rs::model::Document {
                name: manifest.pages[0].name.clone(),
                ..Default::default()
            },
            camera: None,
        };
        let p = paper_rs::project::Project { dir, manifest };
        // A file becomes this call's only once its write has completed.
        p.save_page(&page).map_err(|e| made.fail(e.to_string()))?;
        made.file(p.page_path(&page.id));
        p.write_manifest().map_err(|e| made.fail(e.to_string()))?;
        made.file(p.dir.join(paper_rs::project::MANIFEST));
        #[cfg(test)]
        {
            crate::test_support::hit(&ws.path, "create_after_files");
            if crate::test_support::fault(&ws.path, "create_fails") {
                return Err(made.fail("injected test failure".into()));
            }
        }
        let dir = p.dir.clone();
        let install = Self::prepare_install(&ws, p.dir, listening).map_err(|e| made.fail(e))?;
        let out = Self::install(state, install, pin.then_some(ws), owner).await?;
        Ok((dir, out))
    }

    /// `open_editor`: the bound project's editor URL and identity.
    fn editor(state: &mut LauncherState, ws: &Workspace) -> Result<Value, String> {
        let Some(b) = state.bound.as_mut() else {
            return Err(NOT_BOUND.into());
        };
        if !bundle_complete() {
            return Err(
                "This openpaperdesign-mcp was built without the browser editor. Build \
                 the web bundle first (./scripts/build-web.sh), then reinstall \
                 (cargo install --locked --path crates/openpaperdesign-serve). Editing \
                 through the tools works without it."
                    .into(),
            );
        }
        // Self-contained identity from this admitted call: the validated workspace and
        // the bound project. Its manifest is read only after the bound root's identity
        // and entries are checked again (a replaced or linked root is refused), and
        // reading it acknowledges no page. Nothing counts as opened unless this passes.
        let dir = b.server.dir().to_path_buf();
        let manifest = b.server.bound_manifest().map_err(|e| {
            format!(
                "The bound project {} can't be opened: {e}. Nothing was opened; bind \
                 a project again.",
                ws.relative(&dir)
            )
        })?;
        b.editor_opened = true;
        Ok(json!({
            "url": format!("http://127.0.0.1:{}/#token={}", b.port, b.token),
            "workspace": ws.path,
            "project": ws.relative(&dir),
            "project_path": dir,
            "canvas": {
                "id": manifest.id,
                "name": manifest.name,
                "pages": manifest.pages.len(),
            },
            "open_in": "the host's in-app browser (loopback only)",
            "note": "The token in the URL is for this session only; don't write it into files.",
        }))
    }

    async fn run(&self, name: &str, args: Value, meta: &RequestMetaObject) -> ToolResult {
        let mut state = self.state.clone().lock_owned().await;
        if state.host_context {
            let context = if let Some(reason) = &state.invalidated {
                Err(reason.clone())
            } else {
                Workspace::from_meta(meta).and_then(|ws| {
                    if let Some(pinned) = &state.workspace
                        && (ws.path != pinned.path || ws.id != pinned.id)
                    {
                        return Err("Codex workspace changed or was moved/replaced; this session cannot change workspace".into());
                    }
                    Ok(ws)
                })
            };
            match context {
                Ok(ws) => {
                    state.workspace = Some(ws);
                    state.pinned_by = Owner::Model;
                }
                Err(reason) => {
                    let status = name == "openpaperdesign_status";
                    let state = Self::retire_session(state, reason.clone()).await?;
                    return if status {
                        Self::status(&state, Some(&reason))
                    } else {
                        Err(format!(
                            "{reason}. No call was run. If this session had a workspace, it is retired; start a fresh supported host session."
                        ))
                    };
                }
            }
        }
        if name == "openpaperdesign_status" {
            return Self::status(&state, None);
        }
        let ws = state
            .workspace
            .as_ref()
            .expect("admitted workspace")
            .clone();
        let arg = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| format!("missing required string argument `{k}`"))
        };
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        match name {
            "bind_project" => {
                let dir = Self::existing(&ws, arg("path")?)?;
                Self::bind(state, ws, dir, force, false, Owner::Model).await
            }
            "create_project" => Self::create(state, ws, arg("name")?, force, false, Owner::Model)
                .await
                .map(|(_, out)| out),
            "open_editor" => text(Self::editor(&mut state, &ws)?),
            "openpaperdesign_canvases" => {
                if args.as_object().is_some_and(|a| !a.is_empty()) {
                    return Err("openpaperdesign_canvases takes no arguments; it lists this workspace's design/ folder.".into());
                }
                let bound = state.bound.as_ref().map(|b| b.server.dir().to_path_buf());
                ws.canvases(bound.as_deref())
            }
            _ => {
                let server = {
                    match state.bound.as_ref() {
                        Some(b) => b.server.clone(),
                        None => return Err(NOT_BOUND.into()),
                    }
                };
                drop(state);
                server.call_tool(self.session, name, args).await
            }
        }
    }
}

impl ServerHandler for Launcher {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        if self.library.is_some() {
            // No workspace context is used or asked for in an app-library process.
            return ServerConfig::new(capabilities)
                .with_server_info(
                    Implementation::new("openpaperdesign", VERSION)
                        .with_icons(serde_json::from_value(icons()).expect("valid icons")),
                )
                .with_instructions(format!("{}\n\n{}", app::INSTRUCTIONS, tools::INSTRUCTIONS));
        }
        // Safe to advertise in standalone mode too; only --codex-context consumes it.
        capabilities.experimental = Some([(CAPABILITY.to_string(), Default::default())].into());
        ServerConfig::new(capabilities)
            .with_server_info(
                Implementation::new("openpaperdesign", VERSION)
                    .with_icons(serde_json::from_value(icons()).expect("valid icons")),
            )
            .with_instructions(format!(
                "openpaperdesign: an editable design canvas (paper-rs) for this workspace. \
                 Check openpaperdesign_status first. In packaged Codex mode every call except \
                 the static openpaperdesign_home view and the read-only openpaperdesign_projects \
                 list needs host-generated workspace and supported permission context; tool \
                 arguments cannot supply it, and a listed Codex project folder is not the \
                 workspace. If workspace_usable is false, report workspace_problem \
                 and stop editing. First \
                 bind a project deliberately (bind_project for an existing one inside the \
                 workspace, or create_project under design/). Before changing a page, read \
                 that page in this session (get_tree_summary with nodeId \"root\" and its \
                 pageId); changes to unread or since-changed pages are refused as conflicts. \
                 Then edit with the tools below, and call open_editor to get the canvas URL for \
                 the in-app browser, where the \
                 person can edit too. Export and project switching outside the workspace are \
                 disabled; images must be data: URIs.\n\n{}",
                tools::INSTRUCTIONS
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if self.library.is_some() {
            return Ok(ListToolsResult::with_all_items(app::listed_tools()));
        }
        let mut all = own_tools();
        all.extend(offered_tools());
        Ok(ListToolsResult::with_all_items(all))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let home = serde_json::from_value(json!({
            "uri": HOME_URI,
            "name": "openpaperdesign-home",
            "title": "OpenPaperDesign",
            "mimeType": HOME_MIME,
        }))
        .expect("valid resource");
        Ok(ListResourcesResult::with_all_items(vec![home]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        // Static and exact: no other URI is resolved, and no state or file is read.
        if request.uri != HOME_URI {
            return Err(ErrorData::resource_not_found("unknown resource", None));
        }
        let contents = serde_json::from_value(json!({
            "uri": HOME_URI,
            "mimeType": HOME_MIME,
            "text": home_html_for(self.library.is_some()),
            "_meta": {"openai/ui": {"availableDisplayModes": ["inline", "fullscreen"]}},
        }))
        .expect("valid resource contents");
        Ok(ReadResourceResult::new(vec![contents]).into())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if let Some(library) = &self.library {
            // Every call in an app-library process: never workspace admission or retirement.
            let r = self
                .library_call(library, &request.name, request.arguments, &context)
                .await;
            return Ok(to_result(r).into());
        }
        if app::TOOLS.contains(&request.name.as_ref())
            || app::AGENT_TOOLS.contains(&request.name.as_ref())
        {
            // Refused without reading or changing any state (no admission, no retirement).
            return Ok(to_result(Err(app::NOT_LIBRARY.to_string())).into());
        }
        if request.name == HOME_TOOL {
            return Ok(to_result(home(request.arguments.as_ref())).into());
        }
        if request.name == direct::PICK_TOOL {
            return Ok(to_result(
                direct::choose_folder(request.arguments.as_ref(), &context).await,
            )
            .into());
        }
        if direct::UI_TOOLS.contains(&request.name.as_ref()) {
            let args = Value::Object(request.arguments.unwrap_or_default());
            return Ok(to_result(self.direct(&request.name, args, &context.meta).await).into());
        }
        if request.name == PROJECTS_TOOL {
            // rmcp cancels `ct` on `notifications/cancelled` but keeps running this handler.
            let ct = context.ct.clone();
            let cancelled = async move { ct.cancelled().await };
            return Ok(to_result(projects(request.arguments.as_ref(), cancelled).await).into());
        }
        let args = Value::Object(request.arguments.unwrap_or_default());
        Ok(to_result(self.run(&request.name, args, &context.meta).await).into())
    }
}

fn workspace(args: &mut Vec<String>) -> Result<Option<Workspace>, String> {
    if args.iter().any(|a| a == "--codex-context") {
        if args.iter().filter(|a| *a == "--codex-context").count() != 1
            || args.iter().any(|a| a == "--workspace")
        {
            return Err("--codex-context cannot be repeated or combined with --workspace".into());
        }
        args.retain(|a| a != "--codex-context");
        return Ok(None);
    }
    let (path, source) = match args.iter().position(|a| a == "--workspace") {
        Some(i) => {
            let v = args
                .get(i + 1)
                .cloned()
                .filter(|v| !v.starts_with("--"))
                .ok_or("--workspace needs a value")?;
            args.drain(i..=i + 1);
            (PathBuf::from(v), "explicit --workspace")
        }
        None => (
            std::env::current_dir().map_err(|e| e.to_string())?,
            "process cwd",
        ),
    };
    Workspace::new(&path, source).map(Some)
}

const LIBRARY_FLAG: &str = "--app-library";

/// How this process runs: a workspace launcher (`--codex-context`, `--workspace` or cwd), or
/// the app library (`--app-library <dir>`).
enum Mode {
    Workspace(Option<Workspace>),
    Library(Library),
}

fn mode(args: &mut Vec<String>) -> Result<Mode, String> {
    let n = args.iter().filter(|a| *a == LIBRARY_FLAG).count();
    if n == 0 {
        return workspace(args).map(Mode::Workspace);
    }
    if n != 1
        || args
            .iter()
            .any(|a| a == "--codex-context" || a == "--workspace")
    {
        return Err(
            "--app-library cannot be repeated or combined with --codex-context or --workspace"
                .into(),
        );
    }
    let i = args
        .iter()
        .position(|a| a == LIBRARY_FLAG)
        .expect("counted");
    let v = args
        .get(i + 1)
        .cloned()
        .filter(|v| !v.starts_with("--"))
        .ok_or(
            "--app-library needs the library folder (the packaged plugin passes ${PLUGIN_DATA})",
        )?;
    args.drain(i..=i + 1);
    Library::open(&v).map(Mode::Library)
}

/// Entrypoint for the stdio launcher binary.
pub async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("openpaperdesign-mcp {VERSION}");
        return;
    }
    let mode = mode(&mut args).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let ws = match &mode {
        Mode::Workspace(ws) => ws.clone(),
        Mode::Library(_) => None,
    };
    match args.first().map(String::as_str) {
        Some("doctor") => {
            let missing: Vec<&str> = bundle_file_names()
                .into_iter()
                .filter(|n| !BUNDLE_FILES.iter().any(|(f, _)| f == n))
                .collect();
            if let Mode::Library(lib) = &mode {
                let problem = lib.check().err();
                let report = json!({
                    "version": VERSION,
                    "mode": "app-library",
                    "library": lib.root(),
                    "library_usable": problem.is_none(),
                    "library_problem": problem,
                    "workspace": null,
                    "browser_editor": if missing.is_empty() { "built in" } else { "missing" },
                    "missing_bundle_files": missing,
                    "platform_supported": cfg!(unix),
                });
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
                let ok = missing.is_empty() && cfg!(unix) && problem.is_none();
                std::process::exit(if ok { 0 } else { 1 });
            }
            let report = json!({
                "version": VERSION,
                "mode": "workspace",
                "workspace": ws.as_ref().map(|w| &w.path),
                "workspace_source": ws.as_ref().map_or(CAPABILITY, |w| w.source),
                "workspace_usable": ws.as_ref().is_some_and(|w| workspace_problem(&w.path).is_none()),
                "workspace_problem": ws.as_ref().map_or_else(|| Some("awaiting per-call Codex workspace and permission context".to_string()), |w| workspace_problem(&w.path)),
                "browser_editor": if missing.is_empty() { "built in" } else { "missing" },
                "missing_bundle_files": missing,
                "platform_supported": cfg!(unix),
            });
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            let ok = missing.is_empty() && cfg!(unix);
            std::process::exit(if ok { 0 } else { 1 });
        }
        None => {}
        Some(other) => {
            eprintln!(
                "unknown argument {other:?}\nusage: openpaperdesign-mcp [--workspace <dir> | --codex-context | --app-library <dir>] [doctor] | --version"
            );
            std::process::exit(2);
        }
    }
    // MCP over stdio. Nothing else may write to stdout.
    let launcher = match mode {
        Mode::Workspace(ws) => Launcher::new(ws),
        Mode::Library(lib) => Launcher::with_library(lib),
    };
    // The service consumes its handler; this clone shares the state so the end can drain it.
    let owner = launcher.clone();
    match launcher.serve(rmcp::transport::stdio()).await {
        Ok(running) => {
            let _ = running.waiting().await;
            owner.shutdown().await;
        }
        Err(e) => {
            eprintln!("openpaperdesign-mcp: {e}");
            owner.shutdown().await;
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[path = "launcher_tests.rs"]
mod tests;
