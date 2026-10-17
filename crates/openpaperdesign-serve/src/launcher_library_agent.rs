//! Agent (model) access to the app library: the same app-owned canvas the person opens from the
//! OpenPaperDesign home (in its web editor, or the optional in-panel editor), through upstream
//! paper-rs's own agent commands.
//!
//! Two small pieces, both over existing machinery:
//!
//! - Library controls the model can see (`openpaperdesign_library_*`): list, the open canvas
//!   (its id, binding and pages), create, open and close. They reuse the home view's library
//!   operations (`app_create`, `app_open`, `app_close`, `Library::list`) with no `force` or
//!   `replace`: a canvas that has an editor which may hold unsaved changes (a mounted panel or
//!   a given browser link) is never switched or closed from here. Only
//!   `openpaperdesign_library_editor` hands out the canvas's loopback browser editor link (for
//!   Codex's built-in browser, as workspace mode's `open_editor` does), and that counts as a
//!   link given; no other result carries the URL or its token.
//! - Upstream's offered design tools (`agent::offered_tools`: the paper-rs catalogue minus
//!   export and file creation), with their own names, descriptions and schemas, plus one
//!   required `binding`: the open canvas the agent means. It is compared under the state lock
//!   (after the canvas, library and lease are re-checked), removed, and the call goes to that
//!   binding's server exactly as workspace mode sends it (`Server::call_tool` with this
//!   process's session): the same policy, read-before-write acknowledgements, one-commit save
//!   with rollback and `Broken` fence, and the lifecycle fence that refuses queued and late
//!   calls after a close or switch. A stale binding is refused with no effect.
//!
//! The binding is a stale-view check, not a credential, like everywhere else in the library.
//! Library authority stays where it is: `PLUGIN_DATA` and the per-canvas lease.

use super::*;
use crate::agent::SESSION_ARG;

pub(in crate::launcher) const AGENT_LIST: &str = "openpaperdesign_library_canvases";
pub(in crate::launcher) const AGENT_CURRENT: &str = "openpaperdesign_library_current";
pub(in crate::launcher) const AGENT_CREATE: &str = "openpaperdesign_library_create";
pub(in crate::launcher) const AGENT_OPEN: &str = "openpaperdesign_library_open";
pub(in crate::launcher) const AGENT_CLOSE: &str = "openpaperdesign_library_close";
pub(in crate::launcher) const AGENT_EDITOR: &str = "openpaperdesign_library_editor";
pub(in crate::launcher) const AGENT_TOOLS: [&str; 6] = [
    AGENT_LIST,
    AGENT_CURRENT,
    AGENT_CREATE,
    AGENT_OPEN,
    AGENT_CLOSE,
    AGENT_EDITOR,
];

const NEEDS_BINDING: &str = "Name the canvas you mean: pass `binding` from \
    openpaperdesign_library_current (or from a library create or open result). Nothing was \
    read or changed.";
const AGENT_STALE: &str = "That binding isn't the canvas open in this OpenPaperDesign window any \
    more (it was closed, or another canvas was opened). Nothing was read or changed. Call \
    openpaperdesign_library_current to see what is open.";
const EDITORS_OPEN: &str = "The open canvas has an editor (its browser tab, or an OpenPaperDesign \
    view) that may hold unsaved changes, so it can't be switched or closed from here. Work on \
    the open canvas (openpaperdesign_library_current), or ask the person to close it from the \
    OpenPaperDesign home. Nothing was changed.";
const NEXT: &str = "Read a page before you change it: get_tree_summary with nodeId \"root\", \
    its pageId and this binding (a library result like this one doesn't count as a read). \
    Every successful change is saved at once; the person's editor (browser tab or view) picks it \
    up when it has no unsaved change of its own, and one that has gets a conflict instead of \
    overwriting yours.";

/// Whether `name` is one of upstream's offered design tools.
pub(in crate::launcher) fn is_design_tool(name: &str) -> bool {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES
        .get_or_init(|| {
            offered_tools()
                .into_iter()
                .map(|t| t.name.to_string())
                .collect()
        })
        .iter()
        .any(|n| n == name)
}

/// The model-visible library controls, then upstream's offered design tools with the binding
/// envelope added to their own schemas.
pub(super) fn agent_tools() -> Vec<Value> {
    let none = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let binding = json!({"type": "string", "pattern": "^[0-9a-f]{16}\\.[0-9]{1,20}$",
        "description": "The open canvas you mean: its binding from \
            openpaperdesign_library_current (or a library create or open result). A stale one \
            is refused with no effect."});
    let id = json!({"type": "string", "pattern": "^[0-9a-f]{16}$",
        "description": "A canvas id from openpaperdesign_library_canvases."});
    let mut out = vec![
        json!({"name": AGENT_LIST, "inputSchema": none,
            "annotations": {"readOnlyHint": true},
            "description": "List the canvases in OpenPaperDesign's app library (the ones the \
                person sees in the OpenPaperDesign home), and which one is open here. \
                Read-only: nothing is created, opened or locked."}),
        json!({"name": AGENT_CURRENT, "inputSchema": none,
            "annotations": {"readOnlyHint": true},
            "description": "The canvas open in this OpenPaperDesign window: its id, name, \
                binding (pass it to the design tools), pages and whether an editor is open on \
                it. Reading it counts as reading no page."}),
        json!({"name": AGENT_CREATE,
            "annotations": {"readOnlyHint": false, "destructiveHint": false},
            "description": "Create a new, empty canvas in the app library and open it here \
                (no design system or direction is applied). Refused while the open canvas has \
                an editor that may hold unsaved changes.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": MAX_NAME_CHARS}
            }, "required": ["name"], "additionalProperties": false}}),
        json!({"name": AGENT_OPEN,
            "annotations": {"readOnlyHint": false, "destructiveHint": false},
            "description": "Open a library canvas here, so the design tools work on it. Opening \
                the canvas already open is a no-op. Refused while the open canvas has an editor \
                that may hold unsaved changes, or when another OpenPaperDesign window has the \
                canvas open.",
            "inputSchema": {"type": "object", "properties": {"id": id},
                "required": ["id"], "additionalProperties": false}}),
        json!({"name": AGENT_CLOSE,
            "annotations": {"readOnlyHint": false, "destructiveHint": false},
            "description": "Close the open canvas (named by its binding). Everything saved \
                stays in the library. Refused while it has an editor that may hold unsaved \
                changes.",
            "inputSchema": {"type": "object", "properties": {"binding": binding},
                "required": ["binding"], "additionalProperties": false}}),
        json!({"name": AGENT_EDITOR,
            "annotations": {"readOnlyHint": false, "destructiveHint": false},
            "description": "The loopback browser editor link of the open canvas \
                (http://127.0.0.1:<port>/#token=…), to open in Codex's built-in browser or a \
                browser on this computer. The person and you then edit the same canvas: your \
                changes show there, and an unsaved change there is never overwritten (its save \
                is refused as a conflict). The token is for this window only: never write it \
                into files. Once a link was given, the canvas can't be switched or closed from \
                here.",
            "inputSchema": {"type": "object", "properties": {"binding": binding},
                "required": ["binding"], "additionalProperties": false}}),
    ];
    for t in offered_tools() {
        let mut v = serde_json::to_value(t).expect("tool serializes");
        if let Some(schema) = v.get_mut("inputSchema").and_then(Value::as_object_mut) {
            if let Some(props) = schema
                .entry("properties")
                .or_insert_with(|| json!({}))
                .as_object_mut()
            {
                props.insert("binding".into(), binding.clone());
            }
            if let Some(required) = schema
                .entry("required")
                .or_insert_with(|| json!([]))
                .as_array_mut()
            {
                required.push(json!("binding"));
            }
        }
        if let Some(Value::String(d)) = v.get_mut("description") {
            d.push_str(
                "\n\nOpenPaperDesign app library: also pass `binding`, the open canvas you mean \
                 (openpaperdesign_library_current). Changes are saved at once.",
            );
        }
        out.push(v);
    }
    out
}

/// A library refusal phrased for the agent: never ask it for a confirmation it can't give.
fn agent_refusal(e: String) -> String {
    if e.contains("force: true") {
        EDITORS_OPEN.into()
    } else if e == STALE {
        AGENT_STALE.into()
    } else {
        e
    }
}

impl Launcher {
    /// The model-visible library controls (see the module docs).
    pub(super) async fn agent_library(
        &self,
        lib: &Arc<Library>,
        name: &str,
        args: Value,
    ) -> ToolResult {
        let strings: &[&str] = match name {
            AGENT_CREATE => &["name"],
            AGENT_OPEN => &["id"],
            AGENT_CLOSE | AGENT_EDITOR => &["binding"],
            _ => &[],
        };
        let map = direct::strict(&args, strings, &[])?;
        let get = |k: &str| map.get(k).and_then(Value::as_str);
        match name {
            AGENT_LIST => {
                let open = self
                    .state
                    .lock()
                    .await
                    .bound
                    .as_ref()
                    .and_then(|b| b.doc.clone());
                text(lib.list(open.as_deref())?)
            }
            AGENT_CURRENT => {
                let state = self.state.clone().lock_owned().await;
                let state = Self::fence_library(state).await?;
                text(Self::agent_view(&state, "current")?)
            }
            AGENT_CREATE => {
                let name = canvas_name(get("name"))?;
                let r = self
                    .app_create(lib, NewDocument::blank(&name), false, None, "created", true)
                    .await;
                self.agent_result(r, "created").await
            }
            AGENT_OPEN => {
                let id = get("id")
                    .filter(|s| valid_id(s))
                    .ok_or("A canvas id from the library list is required. Nothing was opened.")?;
                let r = self.app_open(lib, id, false, None, true).await;
                self.agent_result(r, "opened").await
            }
            AGENT_CLOSE => {
                let binding = get("binding")
                    .filter(|b| valid_binding(b))
                    .ok_or(NEEDS_BINDING)?;
                self.app_close(Some(binding), false)
                    .await
                    .map_err(agent_refusal)
            }
            AGENT_EDITOR => {
                let binding = get("binding")
                    .filter(|b| valid_binding(b))
                    .ok_or(NEEDS_BINDING)?;
                if !bundle_complete() {
                    return Err(
                        "This OpenPaperDesign command was built without the browser editor, so \
                         there is no editor link. The design tools still work."
                            .into(),
                    );
                }
                let state = self.state.clone().lock_owned().await;
                let mut state = Self::fence_library(state).await?;
                expect_binding(&state, Some(binding)).map_err(agent_refusal)?;
                // The same link the home view's browser action gets: it counts as given.
                text(Self::library_editor(&mut state, lib, "link", false)?)
            }
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// The canvas a create or open result named, as the agent sees it, if it is still open.
    async fn agent_result(&self, r: ToolResult, action: &str) -> ToolResult {
        let outs = r.map_err(agent_refusal)?;
        let v: Value = outs
            .iter()
            .find_map(|o| match o {
                Out::Text(t) => serde_json::from_str(t).ok(),
                _ => None,
            })
            .ok_or(
                "The library answered without a canvas. Check openpaperdesign_library_current.",
            )?;
        let binding = v["binding"].as_str().unwrap_or_default().to_string();
        let state = self.state.clone().lock_owned().await;
        expect_binding(&state, Some(&binding)).map_err(agent_refusal)?;
        text(Self::agent_view(&state, action)?)
    }

    /// The open canvas for the agent: identity, binding, pages and editor state. Reading its
    /// manifest re-checks the canvas, the library and the lease; it acknowledges no page.
    fn agent_view(state: &LauncherState, action: &str) -> Result<Value, String> {
        let b = state
            .bound
            .as_ref()
            .filter(|b| b.doc.is_some())
            .ok_or(NO_CANVAS)?;
        let manifest = b.server.bound_manifest()?;
        let pages: Vec<Value> = manifest
            .pages
            .iter()
            .map(|p| json!({"id": p.id, "name": p.name}))
            .collect();
        Ok(json!({
            "action": action,
            "storage": "app-library",
            "canvas": {"id": b.doc, "name": manifest.name, "modified": manifest.modified},
            "binding": b.binding,
            "pages": pages,
            "active_page": manifest.active_page,
            "editors": {
                "panel_editors": b.mounts.len(),
                "browser_link_given": b.editor_opened,
            },
            "next": NEXT,
        }))
    }

    /// One upstream design tool on the open canvas named by `binding` (see the module docs).
    pub(super) async fn agent_design(
        &self,
        name: &str,
        args: Option<serde_json::Map<String, Value>>,
    ) -> ToolResult {
        let mut args = args.unwrap_or_default();
        // The session is this process's, set by the dispatcher; never the caller's.
        if args.contains_key(SESSION_ARG) {
            return Err(format!(
                "unexpected argument `{SESSION_ARG}`; nothing was read or changed"
            ));
        }
        let binding = match args.remove("binding") {
            Some(Value::String(b)) if valid_binding(&b) => b,
            Some(_) => {
                return Err(
                    "`binding` isn't a binding from openpaperdesign_library_current; nothing \
                     was read or changed"
                        .into(),
                );
            }
            None => return Err(NEEDS_BINDING.into()),
        };
        let server = {
            let state = self.state.clone().lock_owned().await;
            refuse_if_shut_down(&state)?;
            let state = Self::fence_library(state).await?;
            expect_binding(&state, Some(&binding)).map_err(agent_refusal)?;
            state.bound.as_ref().ok_or(NO_CANVAS)?.server.clone()
        };
        // The state lock is released: the call waits for the binding's writer fence on its own.
        server
            .call_tool(self.session, name, Value::Object(args))
            .await
    }
}
