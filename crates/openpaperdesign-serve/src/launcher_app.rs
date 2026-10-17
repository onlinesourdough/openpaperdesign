//! The app library's direct actions (`openpaperdesign-mcp --app-library <dir>`): list, create,
//! open, close, reopen, import and export canvases the app owns (see `crate::library`), from
//! the home view, with no model turn and no message.
//!
//! In an app-library process only these tools, the home, its resource,
//! `openpaperdesign_status` (this mode's own report), the read-only Codex project inventory and
//! the agent's access to the same canvases (`launcher_library_agent.rs`: model-visible library
//! controls and upstream's design tools, each naming the open canvas's binding) are answered.
//! Every project route (`bind_project`, `create_project`, `open_editor`, the workspace
//! canvases list, the `_ui_*` workspace actions and the folder picker) is refused without being
//! admitted and without touching any state, so a call without workspace context can neither
//! use nor retire anything here. No Workspace is ever
//! made for the library, and no argument names a path: only a server-made canvas id, a bounded
//! name, a `force` flag, a server-made binding identity or a canvas file's bytes reach it.
//!
//! One canvas is bound per process, through the same prepare/install/retire handoff as project
//! mode; its lease moves with the binding and is released only after the old server drained.
//! Creation and import run in an owned task, so a caller that goes away can't strand a
//! half-made canvas, its cleanup or the handoff. A lost reply doesn't mean nothing happened:
//! the canvas then shows up in the list. Nothing is retried automatically.
//!
//! Several views (the sidebar, chat tabs) can share this process's one binding. Each binding
//! has its own identity (`binding`, `<canvas id>.<n>`), returned with every editor result.
//! Close, export and the readiness poll name the binding they were shown for, and a confirmed
//! switch (`force`) names the binding it confirmed replacing (`replace`); each is compared
//! under the launcher's state lock before anything happens, and a stale one is refused with
//! the current canvas, its files, URL and lease untouched. This only catches stale views: it
//! is not a credential, a path or any authority.

use super::*;
use crate::library::payload::MAX_PAYLOAD_BYTES;
use crate::library::{Lease, LeaseError, MAX_NAME_CHARS, NewDocument, payload, valid_id};

#[path = "launcher_library_agent.rs"]
mod agent_api;
pub(super) use agent_api::AGENT_TOOLS;
#[cfg(test)]
pub(super) use agent_api::{
    AGENT_CLOSE, AGENT_CREATE, AGENT_CURRENT, AGENT_EDITOR, AGENT_LIST, AGENT_OPEN,
};

pub(super) const LIST: &str = "openpaperdesign_app_canvases";
pub(super) const CREATE: &str = "openpaperdesign_app_create";
pub(super) const OPEN: &str = "openpaperdesign_app_open";
pub(super) const EDITOR: &str = "openpaperdesign_app_editor";
pub(super) const CLOSE: &str = "openpaperdesign_app_close";
pub(super) const IMPORT: &str = "openpaperdesign_app_import";
pub(super) const EXPORT: &str = "openpaperdesign_app_export";
// The in-panel editor's transport (the home view's own editor, see `panel_call`).
pub(super) const ASSET: &str = "openpaperdesign_app_editor_asset";
pub(super) const PAGE: &str = "openpaperdesign_app_page";
pub(super) const REVISION: &str = "openpaperdesign_app_page_revision";
pub(super) const SAVE: &str = "openpaperdesign_app_page_save";
pub(super) const READY: &str = "openpaperdesign_app_ready";
pub(super) const RELEASE: &str = "openpaperdesign_app_page_release";
pub(super) const PANEL_TOOLS: [&str; 6] = [ASSET, PAGE, REVISION, SAVE, READY, RELEASE];
pub(super) const TOOLS: [&str; 13] = [
    LIST, CREATE, OPEN, EDITOR, CLOSE, IMPORT, EXPORT, ASSET, PAGE, REVISION, SAVE, READY, RELEASE,
];

/// The packaged renderer goes to the in-panel editor in parts of this many bytes (the last one
/// shorter), in order; a renderer larger than `MAX_ASSET_BYTES` isn't sent at all.
pub(super) const PART_BYTES: usize = 256 * 1024;
pub(super) const MAX_ASSET_BYTES: usize = 16 * 1024 * 1024;
/// The largest page message the in-panel editor exchanges, counted as the serialized JSON
/// (UTF-8, escaping included): a page read's result, or a save's arguments. A deliberately
/// conservative choice, well below the 64 MiB page limit, because the host's message limits
/// are unknown (rmcp's own stdio codec has none by default; that says nothing about the
/// host). A larger page isn't opened in the panel, and a larger save isn't sent; neither
/// changes any file. The browser editor link still works for them.
pub(super) const MAX_PANEL_MESSAGE: usize = 4 * 1024 * 1024;
/// Room left within `MAX_PANEL_MESSAGE` for what a host adds around a tool request or result
/// it forwards (its own ids, metadata and framing). An explicit, bounded allowance, not a known
/// host limit; the view's `panel.js` uses the same number.
pub(super) const HOST_OVERHEAD: usize = 16 * 1024;

/// The size of `r` as this server answers it: `to_result` (what `call_tool` sends) inside a
/// JSON-RPC response with the widest id, every level of escaping included, plus
/// `HOST_OVERHEAD`.
pub(super) fn response_bytes(r: &ToolResult) -> usize {
    let wire = json!({"jsonrpc": "2.0", "id": u64::MAX, "result": to_result(r.clone())});
    serde_json::to_vec(&wire)
        .map_or(usize::MAX, |v| v.len())
        .saturating_add(HOST_OVERHEAD)
}

/// The size of a `tools/call` request for `name` with `args` as the view sends it (and a host
/// forwards it): a JSON-RPC request with the widest id, plus `HOST_OVERHEAD`.
pub(super) fn request_bytes(name: &str, args: &Value) -> usize {
    let wire = json!({"jsonrpc": "2.0", "id": u64::MAX, "method": "tools/call",
        "params": {"name": name, "arguments": args}});
    serde_json::to_vec(&wire)
        .map_or(usize::MAX, |v| v.len())
        .saturating_add(HOST_OVERHEAD)
}
/// JSON integers are exact up to 2^53 - 1.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

pub(super) const NOT_LIBRARY: &str = "This OpenPaperDesign server wasn't started for the app \
    library (it serves workspace projects), so app-library actions aren't available here. \
    Nothing was read or changed.";
const LIBRARY_ONLY: &str = "This OpenPaperDesign server runs the app library: its canvases \
    live in OpenPaperDesign's own data folder, and project tools aren't available here. Nothing \
    was read or changed. Workspace projects need a separately started `openpaperdesign-mcp \
    --codex-context` (or `--workspace <dir>`) server.";
const NO_CANVAS: &str = "No canvas is open in this OpenPaperDesign window. Open one from the \
    library. Nothing was changed.";
const BUSY: &str = "This canvas is open in another OpenPaperDesign window (another Codex chat \
    or panel). Close it there first. Nothing was changed here.";
const CLOSE_NEEDS_FORCE: &str = "The editor of this canvas was opened (its browser tab, or the \
    editor in another OpenPaperDesign view). Closing stops the server it saves to, so an edit \
    there that isn't saved yet (the page shows an unsaved change or \"Saving…\") would be lost. \
    Let it show \"Saved\", or accept losing it, then close again with force: true.";
const UNMOUNTED: &str = "This in-panel editor isn't open on the canvas any more (it was left, \
    or the canvas was closed or replaced). Nothing was read or changed.";
const STALE: &str = "The canvas this view shows isn't the one open in this OpenPaperDesign \
    window any more: another view changed it. Nothing was done, and the open canvas is \
    unchanged. Go back to your canvases to see what is open.";

/// A new binding's identity: its canvas id and a number unique in this process.
pub(super) fn binding_id(doc: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{doc}.{n}")
}

/// The shape of [`binding_id`]: a canvas id, a dot and 1 to 20 digits.
fn valid_binding(s: &str) -> bool {
    s.split_once('.').is_some_and(|(id, n)| {
        valid_id(id) && !n.is_empty() && n.len() <= 20 && n.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The current library binding's identity, if a canvas is open.
fn current_binding(state: &LauncherState) -> Option<&str> {
    state.bound.as_ref().and_then(|b| b.binding.as_deref())
}

/// Whether an editor may hold unsaved edits of the open canvas: its browser link was given, or
/// an in-panel editor is mounted on it (in this view or another).
fn editors_in_use(b: &Bound) -> bool {
    b.editor_opened || !b.mounts.is_empty()
}

/// A switch away from the open canvas (create, import or open of another one). Once its editor
/// link was given, or while an in-panel editor is mounted on it, it needs `force` together with
/// `replace`, the binding the person confirmed losing; the refusal names the current binding so
/// a view can confirm exactly that one. A `replace` that isn't the current binding is stale and
/// refused.
fn library_switch(state: &LauncherState, force: bool, replace: Option<&str>) -> Result<(), String> {
    let current = current_binding(state);
    if let Some(r) = replace
        && current != Some(r)
    {
        return Err(STALE.into());
    }
    match state.bound.as_ref() {
        Some(b) if editors_in_use(b) && (!force || replace.is_none()) => Err(format!(
            "{SWITCH_NEEDS_FORCE} (open canvas: {})",
            current.unwrap_or("none")
        )),
        _ => Ok(()),
    }
}

/// An action on the canvas a view shows (`binding`): there must be an open library canvas, and
/// it must be exactly that binding.
fn expect_binding(state: &LauncherState, binding: Option<&str>) -> Result<(), String> {
    match current_binding(state) {
        None => Err(NO_CANVAS.into()),
        Some(c) if binding == Some(c) => Ok(()),
        Some(_) => Err(STALE.into()),
    }
}

pub(super) const INSTRUCTIONS: &str = "openpaperdesign (app library): the OpenPaperDesign home \
    in the Codex sidebar creates, opens, closes and reopens canvases that OpenPaperDesign keeps \
    in its own data folder, and hands each to its loopback web editor (Codex's built-in \
    browser, or a browser on this computer; editing inside the view is optional). Those \
    actions are the person's, from the view; they need no model call.\n\n\
    You can work on the same canvases: openpaperdesign_library_current shows the canvas open \
    here (id, binding, pages); openpaperdesign_library_canvases, _create, _open and _close pick \
    one. The paper-rs design tools below edit it; pass the open canvas's `binding` with every \
    call (a stale one is refused with no effect). Read a page before changing it \
    (get_tree_summary with nodeId \"root\" and its pageId); changes to an unread or \
    since-changed page are refused as conflicts. Each successful change is saved at once and \
    shows in the person's editor when it has no unsaved change of its own. A canvas whose editor may hold unsaved changes \
    can't be switched or closed from here: ask the person. In this server list_resources and \
    open_file only name the open canvas; create_file and export aren't offered; images must be \
    data: URIs. Workspace design projects (design/<name>/ in a repository) need a separately \
    started `openpaperdesign-mcp --codex-context` server; don't claim this one can bind or edit \
    them.";

/// The tools of an app-library process: the home, this mode's status, the inventory, the
/// app-only library actions, and the agent's library controls and design tools.
pub(super) fn listed_tools() -> Vec<Tool> {
    let none = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let mut out = vec![
        json!({
            "name": HOME_TOOL,
            "title": "OpenPaperDesign",
            "description": "Open the OpenPaperDesign home (the Codex sidebar entry): the app's \
                own canvas library. The person creates, opens and closes canvases there \
                directly. Calling this changes nothing.",
            "inputSchema": none,
            "icons": icons(),
            "_meta": {
                "ui": {"resourceUri": HOME_URI},
                "openai/ui": {"entrypoints": [{"type": "global"}, {"type": "thread"}]}
            }
        }),
        json!({
            "name": "openpaperdesign_status",
            "description": "Report what this OpenPaperDesign server serves: the app library \
                (its folder and the canvas open in this window), and that it has no workspace \
                or workspace project tools.",
            "inputSchema": none,
        }),
    ];
    out.extend(
        own_tools()
            .into_iter()
            .filter(|t| t.name == PROJECTS_TOOL)
            .map(|t| serde_json::to_value(t).expect("tool serializes")),
    );
    out.extend(tools());
    out.extend(agent_api::agent_tools());
    out.into_iter()
        .map(|t| serde_json::from_value(t).expect("valid tool"))
        .collect()
}

/// The app-only library actions. Their schemas match what the server enforces.
fn tools() -> Vec<Value> {
    let app = json!({"ui": {"visibility": ["app"]}});
    let none = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let force = json!({"type": "boolean", "description": "Go ahead even though the open \
        canvas's browser tab may still have unsaved edits; they are lost. A switch also needs \
        `replace`."});
    let id = json!({"type": "string", "pattern": "^[0-9a-f]{16}$", "description": "A canvas id from the library list."});
    let binding = json!({"type": "string", "pattern": "^[0-9a-f]{16}\\.[0-9]{1,20}$",
        "description": "The binding this view was shown (from an editor result). A stale one is refused."});
    let replace = json!({"type": "string", "pattern": "^[0-9a-f]{16}\\.[0-9]{1,20}$",
        "description": "With force: the binding the person confirmed replacing. A stale one is refused."});
    let panel = json!({"type": "boolean", "description": "The view edits the canvas in its own \
        panel: the result has no browser editor link, and none counts as given."});
    let page_id = json!({"type": "string", "pattern": "^[A-Za-z0-9_-]{1,64}$",
        "description": "The page this in-panel editor read first (from its first page result)."});
    let mount = json!({"type": "integer", "minimum": 1, "maximum": MAX_SAFE_INTEGER,
        "description": "This in-panel editor's number (from its first page result). A released one is refused."});
    let doc = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required, "additionalProperties": false});
    vec![
        json!({"name": LIST, "_meta": app, "inputSchema": none,
            "description": "For the home view: the canvases in OpenPaperDesign's app library \
                (read-only: nothing is created, opened or locked)."}),
        json!({"name": CREATE, "_meta": app,
            "description": "For the home view: create a new, empty canvas in the app library \
                and open it in the editor. No design system or direction is applied.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": MAX_NAME_CHARS},
                "force": force,
                "replace": replace,
                "panel": panel
            }, "required": ["name"], "additionalProperties": false}}),
        json!({"name": OPEN, "_meta": app,
            "description": "For the home view: open a library canvas in the editor.",
            "inputSchema": {"type": "object", "properties": {"id": id, "force": force, "replace": replace, "panel": panel},
                "required": ["id"], "additionalProperties": false}}),
        json!({"name": EDITOR, "_meta": app,
            "description": "For the home view: the editor link and state of the canvas open \
                in this window; with `binding`, only if that is still the one open.",
            "inputSchema": {"type": "object", "properties": {"binding": binding, "panel": panel},
                "additionalProperties": false}}),
        json!({"name": ASSET, "_meta": app,
            "description": "For the home view's own editor: one part of this build's packaged \
                renderer (WebAssembly), base64. Parts are 262144 bytes, numbered from 0. Reads \
                no canvas and no file.",
            "inputSchema": doc(json!({"part": {"type": "integer", "minimum": 0, "maximum": 63}}), &["part"])}),
        json!({"name": PAGE, "_meta": app,
            "description": "For the home view's own editor: the page of the canvas this view \
                shows. Without page_id: its active page (else its first), and the result names \
                this editor's mount; then with page_id and mount: that exact page again.",
            "inputSchema": doc(json!({"binding": binding, "page_id": page_id, "mount": mount}), &["binding"])}),
        json!({"name": REVISION, "_meta": app,
            "description": "For the home view's own editor: the current revision of its page.",
            "inputSchema": doc(json!({"binding": binding, "page_id": page_id, "mount": mount}), &["binding", "page_id", "mount"])}),
        json!({"name": SAVE, "_meta": app,
            "description": "For the home view's own editor: save its page (an upstream page \
                file), only if if_match is the page's revision now. A stale one is refused \
                and nothing is written.",
            "inputSchema": doc(json!({"binding": binding, "page_id": page_id, "mount": mount,
                "if_match": {"type": "string", "maxLength": 64}, "text": {"type": "string"}}),
                &["binding", "page_id", "mount", "if_match", "text"])}),
        json!({"name": READY, "_meta": app,
            "description": "For the home view's own editor: report that it loaded its page and \
                its renderer is Ready. Changes no file.",
            "inputSchema": doc(json!({"binding": binding, "mount": mount}), &["binding", "mount"])}),
        json!({"name": RELEASE, "_meta": app,
            "description": "For the home view's own editor: it was left (Back, Close or the \
                view going away). The canvas stays open; nothing is saved or changed.",
            "inputSchema": doc(json!({"binding": binding, "mount": mount}), &["binding", "mount"])}),
        json!({"name": CLOSE, "_meta": app,
            "description": "For the home view: close the canvas this view shows (its editor \
                server stops). Saved edits stay in the library; closing saves nothing.",
            "inputSchema": {"type": "object", "properties": {"binding": binding, "force": force},
                "required": ["binding"], "additionalProperties": false}}),
        json!({"name": IMPORT, "_meta": app,
            "description": "For the home view: import an OpenPaperDesign canvas file (its \
                text, chosen by the person) as a new library canvas, and open it. The file is \
                checked completely first; nothing existing is overwritten or merged.",
            "inputSchema": {"type": "object", "properties": {
                "payload": {"type": "string", "maxLength": MAX_PAYLOAD_BYTES},
                "force": force,
                "replace": replace,
                "panel": panel
            }, "required": ["payload"], "additionalProperties": false}}),
        json!({"name": EXPORT, "_meta": app,
            "description": "For the home view: the canvas this view shows, as last saved, as \
                a canvas file (canvas only: no design direction or tokens).",
            "inputSchema": {"type": "object", "properties": {"binding": binding},
                "required": ["binding"], "additionalProperties": false}}),
    ]
}

/// A new canvas's name: 1 to 60 characters after trimming, without control characters.
fn canvas_name(name: Option<&str>) -> Result<String, String> {
    let n = name.map(str::trim).unwrap_or("");
    if n.is_empty() || n.chars().count() > MAX_NAME_CHARS || n.chars().any(char::is_control) {
        return Err(format!(
            "A canvas name of 1 to {MAX_NAME_CHARS} characters, without control characters, is \
             required. Nothing was created."
        ));
    }
    Ok(n.to_string())
}

impl Launcher {
    /// Every call in an app-library process.
    pub(super) async fn library_call(
        &self,
        lib: &Arc<Library>,
        name: &str,
        args: Option<serde_json::Map<String, Value>>,
        context: &RequestContext<RoleServer>,
    ) -> ToolResult {
        match name {
            HOME_TOOL => {
                if args.as_ref().is_some_and(|a| !a.is_empty()) {
                    return Err(format!(
                        "{HOME_TOOL} takes no arguments; it only opens the home view."
                    ));
                }
                text(json!({
                    "surface": "home",
                    "version": VERSION,
                    "mode": "app-library",
                    "note": "Static home view. Nothing was read or changed.",
                }))
            }
            PROJECTS_TOOL => {
                let ct = context.ct.clone();
                projects(args.as_ref(), async move { ct.cancelled().await }).await
            }
            "openpaperdesign_status" => {
                if args.as_ref().is_some_and(|a| !a.is_empty()) {
                    return Err("openpaperdesign_status takes no arguments.".into());
                }
                let state = self.state.lock().await;
                let problem = lib.check().err();
                let open = state.bound.as_ref().and_then(|b| b.doc.clone());
                text(json!({
                    "version": VERSION,
                    "mode": "app-library",
                    "library": lib.root(),
                    "library_usable": problem.is_none(),
                    "library_problem": problem,
                    "open_canvas": open,
                    "editor_opened": state.bound.as_ref().is_some_and(|b| b.editor_opened),
                    "panel_editors": state.bound.as_ref().map_or(0, |b| b.mounts.len()),
                    "browser_editor": if bundle_complete() { "built in" } else { "not built" },
                    "panel_editor": match panel_editor() {
                        Ok(_) => "built in".to_string(),
                        Err(why) => format!("not available: {why}"),
                    },
                    "binding": state.bound.as_ref().and_then(|b| b.binding.clone()),
                    "agent_tools": "openpaperdesign_library_* and the paper-rs design tools, \
                        each naming the open canvas's binding",
                    "workspace": null,
                    "workspace_usable": false,
                    "workspace_problem": "This server runs the app library and has no workspace \
                        or workspace project tools; workspace projects need a separately started \
                        openpaperdesign-mcp --codex-context server.",
                }))
            }
            n if PANEL_TOOLS.contains(&n) => {
                let args = Value::Object(args.unwrap_or_default());
                // rmcp cancels `ct` on `notifications/cancelled` but keeps running this handler.
                let ct = context.ct.clone();
                self.panel_call(n, &args, async move { ct.cancelled().await })
                    .await
            }
            n if TOOLS.contains(&n) => {
                self.app(lib, n, Value::Object(args.unwrap_or_default()))
                    .await
            }
            n if AGENT_TOOLS.contains(&n) => {
                self.agent_library(lib, n, Value::Object(args.unwrap_or_default()))
                    .await
            }
            n if agent_api::is_design_tool(n) => self.agent_design(n, args).await,
            _ => Err(LIBRARY_ONLY.into()),
        }
    }

    pub(super) async fn app(&self, lib: &Arc<Library>, name: &str, args: Value) -> ToolResult {
        let (strings, flags): (&[&str], &[&str]) = match name {
            CREATE => (&["name", "replace"], &["force", "panel"]),
            OPEN => (&["id", "replace"], &["force", "panel"]),
            EDITOR => (&["binding"], &["panel"]),
            EXPORT => (&["binding"], &[]),
            CLOSE => (&["binding"], &["force"]),
            IMPORT => (&["payload", "replace"], &["force", "panel"]),
            _ => (&[], &[]),
        };
        let map = direct::strict(&args, strings, flags)?;
        let get = |k: &str| map.get(k).and_then(Value::as_str);
        let force = map.get("force").and_then(Value::as_bool).unwrap_or(false);
        // The view edits in its own panel: no browser link is handed out by this result.
        let panel = map.get("panel").and_then(Value::as_bool).unwrap_or(false);
        // A request for the view's own editor is refused before anything is locked, created,
        // imported, opened or switched when this build can't serve that editor.
        if panel {
            Self::panel_available(lib)?;
        }
        for k in ["binding", "replace"] {
            if get(k).is_some_and(|b| !valid_binding(b)) {
                return Err(format!(
                    "`{k}` isn't a binding from an editor result; nothing was read or changed"
                ));
            }
        }
        let (binding, replace) = (get("binding"), get("replace"));
        match name {
            LIST => {
                let state = self.state.lock().await;
                let open = state.bound.as_ref().and_then(|b| b.doc.clone());
                drop(state);
                text(lib.list(open.as_deref())?)
            }
            CREATE => {
                let name = canvas_name(get("name"))?;
                let doc = NewDocument::blank(&name);
                self.app_create(lib, doc, force, replace, "created", panel)
                    .await
            }
            OPEN => {
                let id = get("id")
                    .filter(|s| valid_id(s))
                    .ok_or("A canvas id from the library list is required. Nothing was opened.")?;
                self.app_open(lib, id, force, replace, panel).await
            }
            EDITOR => {
                let state = self.state.clone().lock_owned().await;
                let mut state = Self::fence_library(state).await?;
                // Without `binding`: whichever canvas is open (the menu's "Open current canvas").
                if binding.is_some() {
                    expect_binding(&state, binding)?;
                }
                text(Self::library_editor(&mut state, lib, "reopened", panel)?)
            }
            CLOSE => self.app_close(binding, force).await,
            IMPORT => {
                let file =
                    get("payload").ok_or("A canvas file is required. Nothing was imported.")?;
                // Checked completely before anything is locked or written.
                let doc = payload::validate(file)?;
                self.app_create(lib, doc, force, replace, "imported", panel)
                    .await
            }
            EXPORT => self.app_export(binding).await,
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// An open canvas whose library or lease identity no longer checks out is closed through
    /// the retirement fence before anything else happens, and the action is refused.
    async fn fence_library(
        state: OwnedMutexGuard<LauncherState>,
    ) -> Result<OwnedMutexGuard<LauncherState>, String> {
        let invalid = state
            .bound
            .as_ref()
            .filter(|b| b.doc.is_some())
            .and_then(|b| b.server.bound_manifest().err());
        match invalid {
            None => Ok(state),
            Some(why) => {
                Self::retire_session(state, why.clone()).await?;
                Err(format!(
                    "The open canvas can't be used any more ({why}), so it was closed: nothing \
                     more is read or saved through it. Nothing else was changed."
                ))
            }
        }
    }

    /// The open library canvas's editor link and identity, from this same binding. Reading its
    /// manifest first re-checks the canvas, the library and the lease. With `panel`, the view
    /// edits it in its own panel: the result carries no link and gives none out (Close and
    /// Switch then count the panel's mount instead, see `editors_in_use`).
    fn library_editor(
        state: &mut LauncherState,
        lib: &Library,
        action: &str,
        panel: bool,
    ) -> Result<Value, String> {
        let Some(b) = state.bound.as_mut() else {
            return Err(NO_CANVAS.into());
        };
        let Some(doc) = b.doc.clone() else {
            return Err(NO_CANVAS.into());
        };
        let manifest = b.server.bound_manifest().map_err(|e| {
            format!("The open canvas can't be shown: {e}. Nothing was opened; close it and open it again.")
        })?;
        let (loaded, saves) = b.server.editor_state();
        let mut out = json!({
            "action": action,
            "storage": "app-library",
            "browser_editor": if bundle_complete() { "built in" } else { "not built" },
            "canvas": {
                "id": doc,
                "name": manifest.name,
                "pages": manifest.pages.len(),
                "modified": manifest.modified,
            },
            "binding": b.binding,
            "location": b.server.dir(),
            "library": lib.root(),
            "editor_loaded": loaded,
            "browser_saves": saves,
        });
        if panel {
            out["panel"] = json!(true);
            out["panel_editor"] = json!(match panel_editor() {
                Ok(_) => "built in".to_string(),
                Err(why) => format!("not available: {why}"),
            });
            out["open_in"] = json!("this view");
            out["note"] = json!("Saved edits are kept in OpenPaperDesign's app library.");
        } else {
            b.editor_opened = true;
            out["url"] = json!(format!("http://127.0.0.1:{}/#token={}", b.port, b.token));
            out["open_in"] = json!("the host's in-app browser (loopback only)");
            out["note"] = json!(
                "Saved edits are kept in OpenPaperDesign's app library. The token in the URL \
                 is for this window only; don't write it into files."
            );
        }
        Ok(out)
    }

    /// Create (or import) a canvas and open it. Fallible preparation (listener, lease, files,
    /// checks, server) finishes before the open canvas, if any, is retired; any failure before
    /// the handoff keeps the open canvas, its URL and lease, and removes exactly what this call
    /// made (or names what stays). The work runs as an owned task.
    async fn app_create(
        &self,
        lib: &Arc<Library>,
        doc: NewDocument,
        force: bool,
        replace: Option<&str>,
        action: &'static str,
        panel: bool,
    ) -> ToolResult {
        let state = self.state.clone().lock_owned().await;
        refuse_if_shut_down(&state)?;
        let state = Self::fence_library(state).await?;
        library_switch(&state, force, replace)?;
        let lib2 = lib.clone();
        let id = tokio::spawn(async move {
            let lib = lib2;
            let listening = Self::listen(lib.root())
                .await
                .map_err(|e| format!("{e}. Nothing was created."))?;
            let lease = Lease::fresh(&lib)?;
            let id = lease.doc.clone();
            let (dir, created) = lib.create_document(&lease, &doc.fresh())?;
            lib.check_document(&id).map_err(|e| created.fail(&e))?;
            #[cfg(test)]
            if crate::test_support::fault(lib.root(), "app_prepare_fails") {
                return Err(created.fail("injected test failure"));
            }
            let lease = Arc::new(lease);
            let guard = crate::library::guard(lib.clone(), &lease);
            let install = Self::prepare_library_install(dir, &id, listening, lease, guard)
                .map_err(|e| created.fail(&e))?;
            Self::install(state, install, None, Owner::Library).await?;
            Ok::<_, String>(id)
        })
        .await
        .map_err(|e| {
            format!("creating the canvas stopped unexpectedly ({e}); check the library list")
        })??;
        let mut state = self.state.clone().lock_owned().await;
        if state.bound.as_ref().and_then(|b| b.doc.as_deref()) != Some(id.as_str()) {
            return Err(format!(
                "The canvas was {action}, but this window's open canvas changed before it could \
                 be shown. It is in the library list."
            ));
        }
        text(Self::library_editor(&mut state, lib, action, panel)?)
    }

    /// Open a library canvas. Opening the canvas that is already open here is a checked no-op;
    /// a busy, invalid or failing target leaves the open canvas, its URL and lease as they were.
    async fn app_open(
        &self,
        lib: &Arc<Library>,
        id: &str,
        force: bool,
        replace: Option<&str>,
        panel: bool,
    ) -> ToolResult {
        let state = self.state.clone().lock_owned().await;
        refuse_if_shut_down(&state)?;
        let mut state = Self::fence_library(state).await?;
        if replace.is_some_and(|r| current_binding(&state) != Some(r)) {
            // A confirmation shown for another binding never acts on this one.
            return Err(STALE.into());
        }
        if state.bound.as_ref().and_then(|b| b.doc.as_deref()) == Some(id) {
            return text(Self::library_editor(
                &mut state,
                lib,
                "already-open",
                panel,
            )?);
        }
        // Checked before a lease file is made for it: only an existing library canvas.
        lib.read_document(id)
            .map_err(|e| format!("This canvas can't be opened: {e}. Nothing was changed."))?;
        library_switch(&state, force, replace)?;
        let listening = Self::listen(lib.root())
            .await
            .map_err(|e| format!("{e}. Nothing was opened; the open canvas is unchanged."))?;
        let lease = match Lease::acquire(lib, id) {
            Ok(l) => l,
            Err(LeaseError::Busy) => return Err(BUSY.into()),
            Err(LeaseError::Failed(e)) => {
                return Err(format!(
                    "{e}. Nothing was opened; the open canvas is unchanged."
                ));
            }
        };
        // Under the lease, before upstream `Project::open` and its stale-temp cleanup.
        lib.check_document(id)
            .map_err(|e| format!("This canvas can't be opened: {e}. Nothing was changed."))?;
        let lease = Arc::new(lease);
        let guard = crate::library::guard(lib.clone(), &lease);
        let install =
            Self::prepare_library_install(lib.document_dir(id), id, listening, lease, guard)?;
        Self::install(state, install, None, Owner::Library).await?;
        let mut state = self.state.clone().lock_owned().await;
        if state.bound.as_ref().and_then(|b| b.doc.as_deref()) != Some(id) {
            return Err(
                "The canvas was opened, but this window's open canvas changed before it could be \
                 shown. Open it again from the library."
                    .into(),
            );
        }
        text(Self::library_editor(&mut state, lib, "opened", panel)?)
    }

    fn prepare_library_install(
        dir: PathBuf,
        doc: &str,
        (listener, port, token): (Listener, u16, String),
        lease: Arc<Lease>,
        guard: crate::Guard,
    ) -> Result<Install, String> {
        let prepared = Server::prepare_library(dir, BUNDLE_FILES, port, token.clone(), guard)
            .map_err(|e| e.to_string())?;
        Ok(Install {
            listener,
            port,
            token,
            rel: doc.to_string(),
            prepared,
            doc: Some(doc.to_string()),
            lease: Some(lease),
        })
    }

    /// Close the canvas the view shows (`binding`, checked under the state lock): admission
    /// closes, started writers and rollback drain, then the binding and its lease go. Owned, so
    /// a caller leaving can't stop it halfway. Closing saves nothing that is only in a browser
    /// tab. Its `force` confirmation is bound to the same `binding`.
    async fn app_close(&self, binding: Option<&str>, force: bool) -> ToolResult {
        let state = self.state.clone().lock_owned().await;
        expect_binding(&state, binding)?;
        let Some(b) = state.bound.as_ref() else {
            return Err(NO_CANVAS.into());
        };
        let Some(doc) = b.doc.clone() else {
            return Err(NO_CANVAS.into());
        };
        if editors_in_use(b) && !force {
            return Err(CLOSE_NEEDS_FORCE.into());
        }
        tokio::spawn(async move {
            let mut state = state;
            if let Some(b) = state.bound.as_ref() {
                b.server.retire().await;
            }
            state.bound = None;
        })
        .await
        .map_err(|e| format!("closing stopped unexpectedly: {e}"))?;
        text(json!({
            "action": "closed",
            "canvas": {"id": doc},
            "note": "The canvas's editor server was stopped. Everything saved before this is in \
                the library; an edit still unsaved in a browser tab was not saved by closing.",
        }))
    }

    /// The open canvas as last saved, as a canvas file. The snapshot is taken under its lease
    /// and writer fence (it waits for a save under way); the fence is released before the text
    /// is returned. Only the open canvas the view shows (`binding`) can be exported; a binding
    /// replaced meanwhile is retired, so its snapshot is refused rather than read from the
    /// replacement.
    async fn app_export(&self, binding: Option<&str>) -> ToolResult {
        let (server, doc, current) = {
            let state = self.state.lock().await;
            let b = state
                .bound
                .as_ref()
                .filter(|b| b.doc.is_some())
                .ok_or("Open the canvas to export first. Nothing was exported.")?;
            expect_binding(&state, binding)?;
            (
                b.server.clone(),
                b.doc.clone().unwrap_or_default(),
                b.binding.clone(),
            )
        };
        let e = server
            .saved_snapshot()
            .await
            .map_err(|e| format!("Not exported: {e}"))?;
        let stem: String = e
            .name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .take(40)
            .collect();
        let stem = stem.trim_matches('-');
        text(json!({
            "action": "exported",
            "binding": current,
            "canvas": {"id": doc, "name": e.name},
            "content": payload::CANVAS_ONLY,
            "file_name": format!("{}.opdcanvas.json", if stem.is_empty() { "canvas" } else { stem }),
            "bytes": e.text.len(),
            "payload": e.text,
            "note": payload::NOTE,
        }))
    }
}

/// Only the named arguments, each of its type: `strings`, and `ints` (whole numbers from 0 to
/// 2^53 - 1; a fraction, a sign, a float such as `1.0` or a numeric string is refused). For
/// the in-panel editor's tools, whose numbers `direct::strict` (strings and booleans) can't
/// check.
fn panel_args<'a>(
    args: &'a Value,
    strings: &[&str],
    ints: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let map = args
        .as_object()
        .ok_or("arguments must be an object; nothing was read or changed")?;
    for (k, v) in map {
        let ok = (strings.contains(&k.as_str()) && v.is_string())
            || (ints.contains(&k.as_str()) && v.as_u64().is_some_and(|n| n <= MAX_SAFE_INTEGER));
        if !ok {
            return Err(format!(
                "unexpected argument `{k}`, or not of its type; nothing was read or changed"
            ));
        }
    }
    Ok(map)
}

/// Part `part` of the packaged renderer: exactly `PART_BYTES` bytes from `part * PART_BYTES`
/// (the last part shorter), base64, with the renderer's identity, total size and part count
/// so the editor can check each answer belongs to the same renderer and comes in order. No
/// other file is ever sent, and no argument names one.
pub(super) fn asset_part(bytes: &[u8], identity: &str, part: u64) -> Result<Value, String> {
    if bytes.is_empty() || bytes.len() > MAX_ASSET_BYTES {
        return Err(format!(
            "this build's renderer is {} bytes, outside what the in-panel editor accepts (1 to \
             {MAX_ASSET_BYTES}); nothing was sent",
            bytes.len()
        ));
    }
    let parts = bytes.len().div_ceil(PART_BYTES);
    let i = usize::try_from(part)
        .ok()
        .filter(|i| *i < parts)
        .ok_or_else(|| {
            format!(
                "there is no renderer part {part}: it has {parts} parts, 0 to {}; nothing was sent",
                parts - 1
            )
        })?;
    let start = i * PART_BYTES;
    let end = (start + PART_BYTES).min(bytes.len());
    Ok(json!({
        "asset": WASM_FILE,
        "identity": identity,
        "bytes": bytes.len(),
        "parts": parts,
        "part": i,
        "offset": start,
        "part_bytes": end - start,
        "data": base64(&bytes[start..end]),
    }))
}

/// An admitted in-panel call: the open canvas's server and, for a call naming a mount, that
/// mount's lifetime flag.
type Admitted = (Arc<Server>, Option<Arc<std::sync::atomic::AtomicBool>>);

fn panel_unavailable(why: &str) -> String {
    format!(
        "The in-panel editor isn't available in this OpenPaperDesign build ({why}), so nothing \
         was created, imported, opened or changed. Rebuild OpenPaperDesign with its editor \
         (./scripts/build-web.sh), then reinstall it."
    )
}

/// An in-panel page call's refusal, as JSON the editor reads: the message, an HTTP-like status
/// (409 with the page's current revision for a conflict, 410 when the binding or mount isn't
/// current any more, 413 when it is too large for the panel, 400 for bad arguments, 503 when
/// it couldn't run) and nothing else. Every refusal means nothing was written.
fn panel_refusal(status: u16, error: &str, revision: Option<&str>) -> String {
    json!({"error": error, "status": status, "revision": revision}).to_string()
}

fn api_refusal(e: crate::ApiError) -> String {
    panel_refusal(e.status.as_u16(), &e.msg, e.etag.as_deref())
}

impl Launcher {
    /// Whether this build serves the in-panel editor (its module and renderer, see
    /// `panel_editor`).
    fn panel_available(lib: &Library) -> Result<(), String> {
        #[cfg(test)]
        if crate::test_support::fault(lib.root(), "panel_unavailable") {
            return Err(panel_unavailable("injected test failure"));
        }
        let _ = lib;
        panel_editor().map(|_| ()).map_err(panel_unavailable)
    }

    /// The home view's own editor (the in-panel editor): the renderer's parts, and the page,
    /// revision, save, Ready and release calls of the canvas the view shows. The renderer part
    /// is static packaged data. Every page call names the view's `binding` and, after its first
    /// read, the `mount` that read returned; both are compared under the state lock before
    /// anything is read or written (`panel_server`, which also re-checks the canvas, library and
    /// lease). The `page_id` is checked as a page of the canvas's manifest; a mount is not tied
    /// to one page (the view keeps to the page it first read). The calls run through the same
    /// `Server` operations as the browser's HTTP API (`Server::page`, `page_revision`,
    /// `save_page_while`, `mark_ready`), with the same conflict, commit and rollback rules,
    /// never through a loopback request.
    ///
    /// A save is tied to its mount's lifetime: a release revokes it, and a save not started by
    /// then never runs; a save whose request is cancelled before it starts never runs either
    /// (`cancelled` is observed before admission, while waiting for it and before the writer
    /// fence is taken). A started save always finishes its commit or rollback.
    pub(super) async fn panel_call(
        &self,
        name: &str,
        args: &Value,
        cancelled: impl std::future::Future<Output = ()>,
    ) -> ToolResult {
        tokio::pin!(cancelled);
        if name == ASSET {
            let map = panel_args(args, &[], &["part"])?;
            let part = map
                .get("part")
                .and_then(Value::as_u64)
                .ok_or("`part` (a whole number) is required; nothing was sent")?;
            let (bytes, id) = wasm().ok_or(
                "This OpenPaperDesign command was built without the browser editor, so it has \
                 no renderer to send.",
            )?;
            return text(asset_part(bytes, id, part)?);
        }
        let (strings, ints, required): (&[&str], &[&str], &[&str]) = match name {
            PAGE => (&["binding", "page_id"], &["mount"], &["binding"]),
            REVISION => (
                &["binding", "page_id"],
                &["mount"],
                &["binding", "page_id", "mount"],
            ),
            SAVE => (
                &["binding", "page_id", "if_match", "text"],
                &["mount"],
                &["binding", "page_id", "mount", "if_match", "text"],
            ),
            READY | RELEASE => (&["binding"], &["mount"], &["binding", "mount"]),
            _ => return Err(format!("unknown tool {name}")),
        };
        let bad = |status: u16, msg: &str| panel_refusal(status, msg, None);
        let map = panel_args(args, strings, ints).map_err(|e| bad(400, &e))?;
        if let Some(k) = required.iter().find(|k| !map.contains_key(**k)) {
            return Err(bad(
                400,
                &format!("`{k}` is required; nothing was read or changed"),
            ));
        }
        let get = |k: &str| map.get(k).and_then(Value::as_str);
        let binding = get("binding").filter(|b| valid_binding(b)).ok_or_else(|| {
            bad(
                400,
                "`binding` isn't a binding from an editor result; nothing was read or changed",
            )
        })?;
        let mount = map.get("mount").and_then(Value::as_u64);
        let page_id = get("page_id");
        if page_id.is_some_and(|p| !crate::valid_page_id(p)) {
            return Err(bad(
                400,
                "`page_id` isn't a page id; nothing was read or changed",
            ));
        }
        if name == PAGE && page_id.is_some() != mount.is_some() {
            return Err(bad(
                400,
                "the first page read names neither page_id nor mount, and every later one names \
                 both; nothing was read or changed",
            ));
        }
        // No editor is mounted from a build that can't serve one.
        if name == PAGE && mount.is_none() {
            panel_editor().map_err(|why| bad(503, &panel_unavailable(why)))?;
        }
        // A save too large for the panel is refused before anything else, as the request it
        // came in (see `request_bytes`).
        if name == SAVE {
            let size = request_bytes(SAVE, args);
            if size > MAX_PANEL_MESSAGE {
                return Err(bad(
                    413,
                    &format!(
                        "this save is {size} bytes as a message, more than the in-panel editor's \
                         {MAX_PANEL_MESSAGE}; nothing was written. Open the canvas in the browser \
                         editor to save a page this large."
                    ),
                ));
            }
        }
        if name == RELEASE {
            let released = {
                let mut state = self.state.lock().await;
                expect_binding(&state, Some(binding)).map_err(|e| bad(410, &e))?;
                let b = state.bound.as_mut().ok_or_else(|| bad(410, NO_CANVAS))?;
                // Revoked under the state lock: no later call is admitted for this mount, and
                // a save admitted earlier finds it revoked under the writer fence.
                mount.and_then(|m| b.mounts.remove(&m)).map(|live| {
                    live.store(false, std::sync::atomic::Ordering::Release);
                    b.server.clone()
                })
            };
            // Without the state lock: a save of this editor that had already started finishes
            // its commit or rollback before the release is acknowledged. The binding stays.
            if let Some(server) = &released {
                server.drain_writer().await;
            }
            return text(json!({"binding": binding, "released": released.is_some()}));
        }
        const CANCELLED: &str = "the save was cancelled before it started; nothing was written";
        let admitted = if name == SAVE {
            // An already cancelled save, or one cancelled while waiting for admission, never
            // starts.
            tokio::select! {
                biased;
                () = &mut cancelled => return Err(bad(503, CANCELLED)),
                r = self.panel_server(binding, mount) => r,
            }
        } else {
            self.panel_server(binding, mount).await
        };
        let (server, live) = admitted.map_err(|e| bad(410, &e))?;
        match name {
            PAGE => {
                let page = server.page(page_id).map_err(api_refusal)?;
                let body = String::from_utf8(page.bytes).map_err(|_| {
                    bad(
                        422,
                        "the page file isn't UTF-8 text, so it can't be edited here; nothing was \
                         changed",
                    )
                })?;
                let mut out = json!({
                    "binding": binding,
                    "canvas": {"name": page.project},
                    "page": {"id": page.id, "name": page.name},
                    "revision": page.revision,
                    "text": body,
                    // The widest mount number, for the measurement below.
                    "mount": MAX_SAFE_INTEGER,
                });
                // Measured as it would be sent: the tool result and its JSON-RPC response,
                // every level of escaping included, and the host allowance. Refused before a
                // mount is registered.
                let size = response_bytes(&text(out.clone()));
                if size > MAX_PANEL_MESSAGE {
                    return Err(bad(
                        413,
                        &format!(
                            "this page is {size} bytes as a message, more than the in-panel \
                             editor's {MAX_PANEL_MESSAGE}, so it wasn't opened here; nothing was \
                             changed. Open it in the browser editor instead."
                        ),
                    ));
                }
                let m = match mount {
                    Some(m) => m,
                    None => {
                        // A new mount, only while this binding is still the open one.
                        let mut state = self.state.lock().await;
                        expect_binding(&state, Some(binding)).map_err(|e| bad(410, &e))?;
                        let b = state.bound.as_mut().ok_or_else(|| bad(410, NO_CANVAS))?;
                        b.next_mount += 1;
                        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
                        b.mounts.insert(b.next_mount, live);
                        b.next_mount
                    }
                };
                out["mount"] = json!(m);
                text(out)
            }
            REVISION => {
                let page_id = page_id.unwrap_or_default();
                let revision = server.page_revision(page_id).map_err(api_refusal)?;
                text(json!({"binding": binding, "page_id": page_id, "revision": revision}))
            }
            SAVE => {
                let page_id = page_id.unwrap_or_default();
                let if_match = get("if_match").unwrap_or_default();
                let body = get("text").unwrap_or_default();
                let live = live.ok_or_else(|| bad(410, UNMOUNTED))?;
                // Cancellation is checked first on every poll, so a cancelled save never takes
                // even a free writer fence. Once the save holds the fence it runs to its commit
                // or rollback within that same poll, without waiting again
                // (`Server::save_page_while`), so it can't be stopped halfway.
                let revision = tokio::select! {
                    biased;
                    () = &mut cancelled => return Err(bad(503, CANCELLED)),
                    r = server.save_page_while(&live, page_id, if_match, body) => {
                        r.map_err(api_refusal)?
                    }
                };
                text(json!({
                    "binding": binding,
                    "page_id": page_id,
                    "saved": true,
                    "revision": revision,
                }))
            }
            READY => {
                server.mark_ready().map_err(api_refusal)?;
                text(json!({"binding": binding, "ready": true}))
            }
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// The open canvas's server for an in-panel call naming `binding` (and `mount`, after the
    /// first page read), compared under the state lock, with that mount's lifetime flag. The
    /// lock is released before the server is used: a save then waits for the binding's writer
    /// fence without it. Close holds the state lock while it drains that fence, so this order is
    /// never reversed.
    async fn panel_server(&self, binding: &str, mount: Option<u64>) -> Result<Admitted, String> {
        let state = self.state.clone().lock_owned().await;
        refuse_if_shut_down(&state)?;
        let state = Self::fence_library(state).await?;
        expect_binding(&state, Some(binding))?;
        let b = state.bound.as_ref().ok_or(NO_CANVAS)?;
        let live = match mount {
            Some(m) => Some(b.mounts.get(&m).cloned().ok_or(UNMOUNTED)?),
            None => None,
        };
        Ok((b.server.clone(), live))
    }
}
