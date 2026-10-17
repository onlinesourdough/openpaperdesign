//! Direct actions for the home view: no chat message, no model turn.
//!
//! Two separate pieces, neither of which grants filesystem access:
//!
//! - `openpaperdesign_choose_folder` asks the host for its directory picker (MCP elicitation,
//!   `openai/elicitation/create` with a `directory` resource input) and reports what the person
//!   chose. Like the home, it is answered before workspace admission and touches no state: no
//!   context, binding, project, listener or file. The chosen folder is never opened, listed or
//!   even looked up; it is input only. A host directory choice carries no documented access,
//!   lifetime or revocation contract, so it is not a workspace.
//! - The `openpaperdesign_ui_*` tools list, create and open canvases directly. Each call must
//!   carry its own supported Codex workspace context, exactly as a model call does
//!   (`Workspace::from_meta`), and, when the view names a folder, that folder must be exactly
//!   the admitted workspace. They reuse the model path's operations and its one bound project.
//!   A refused call never pins: an unpinned session is pinned only by an accepted action (a
//!   listing under the same lock, or a create/open in the same bind handoff that installs its
//!   binding). Arguments, the view's choice and the picker answer authorize nothing.
//!
//! Authority lifetime. Each pin and binding records whose authority admitted it (`Owner`).
//! A view call that carries the host's workspace context, where that context is invalid,
//! downgraded or names another workspace identity, is an observed revocation. When the
//! authority in force is the view's own (a binding a direct action installed, or a pin only
//! direct actions made), it retires the session exactly as a model call would: admission
//! closes and the bound server's writers and rollback drain through its fence, fencing queued
//! and late HTTP, stdio and browser writers. A view call without that context observes
//! nothing and never retires; nor does any view call retire what a model call admitted. As
//! for model calls, revocation is seen only on calls: an idle binding isn't revisited.

use super::*;
use rmcp::model::{CancelledNotificationParam, CustomRequest, ProtocolVersion, ServerRequest};
use rmcp::service::PeerRequestOptions;

pub(super) const PICK_TOOL: &str = "openpaperdesign_choose_folder";
pub(super) const UI_CANVASES: &str = "openpaperdesign_ui_canvases";
pub(super) const UI_CREATE: &str = "openpaperdesign_ui_create";
pub(super) const UI_OPEN: &str = "openpaperdesign_ui_open";
pub(super) const UI_EDITOR: &str = "openpaperdesign_ui_editor";
pub(super) const UI_TOOLS: [&str; 4] = [UI_CANVASES, UI_CREATE, UI_OPEN, UI_EDITOR];

/// The legacy method of OpenAI form elicitation, for MCP protocols before 2026-07-28.
const PICK_METHOD: &str = "openai/elicitation/create";
const PICK_FIELD: &str = "folder";
#[cfg(not(test))]
const PICK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
#[cfg(test)]
pub(super) const PICK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

const NO_CONTEXT: &str = "This view's request carries no Codex workspace context, so \
    OpenPaperDesign can't read or change project files from here. Nothing was read or changed.";

/// App-only tools: the view calls them; they are not offered to the model.
pub(super) fn tools() -> Vec<Value> {
    let app = json!({"ui": {"visibility": ["app"]}});
    let root = json!({"type": "string", "maxLength": 4096, "description": "The folder the person chose. It must be exactly this request's admitted Codex workspace; it grants nothing."});
    vec![
        json!({
            "name": PICK_TOOL,
            "description": "Ask the host to let the person choose a folder (the host's directory \
                picker). Reports the choice only: the folder is not opened, listed or used, and \
                choosing it grants no access. Answered without workspace context.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "_meta": app,
        }),
        json!({
            "name": UI_CANVASES,
            "description": "For the home view: list the canvases under design/ of this request's \
                admitted workspace (read-only, as openpaperdesign_canvases). Needs the request's \
                own Codex workspace context.",
            "inputSchema": {"type": "object", "properties": {"root": root}, "additionalProperties": false},
            "_meta": app,
        }),
        json!({
            "name": UI_CREATE,
            "description": "For the home view: create a canvas under design/<name>/ of this \
                request's admitted workspace, bind it and return its editor link. Needs the \
                request's own Codex workspace context.",
            "inputSchema": {"type": "object", "properties": {
                "root": root,
                "name": {"type": "string", "maxLength": 60},
                "force": {"type": "boolean", "description": "Switch even though the open browser editor of the current canvas may have unsaved edits."}
            }, "required": ["name"], "additionalProperties": false},
            "_meta": app,
        }),
        json!({
            "name": UI_OPEN,
            "description": "For the home view: bind an existing canvas (design/<name>) of this \
                request's admitted workspace and return its editor link. Needs the request's own \
                Codex workspace context.",
            "inputSchema": {"type": "object", "properties": {
                "root": root,
                "project": {"type": "string", "maxLength": 4096},
                "force": {"type": "boolean", "description": "As for openpaperdesign_ui_create."}
            }, "required": ["project"], "additionalProperties": false},
            "_meta": app,
        }),
        json!({
            "name": UI_EDITOR,
            "description": "For the home view: the editor link of the canvas bound in this \
                session, if any. Needs the request's own Codex workspace context.",
            "inputSchema": {"type": "object", "properties": {"root": root}, "additionalProperties": false},
            "_meta": app,
        }),
    ]
}

/// Only the named string/boolean arguments, each of the expected type.
pub(super) fn strict<'a>(
    args: &'a Value,
    strings: &[&str],
    flags: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let map = args
        .as_object()
        .ok_or("arguments must be an object; nothing was read or changed")?;
    for (k, v) in map {
        let ok = (strings.contains(&k.as_str()) && v.is_string())
            || (flags.contains(&k.as_str()) && v.is_boolean());
        if !ok {
            return Err(format!(
                "unexpected argument `{k}`; nothing was read or changed"
            ));
        }
    }
    Ok(map)
}

/// The named folder, if any, must be exactly the admitted workspace. Compared as paths only:
/// the named folder itself is never looked up.
fn same_root(ws: &Workspace, root: Option<&str>) -> Result<(), String> {
    match root {
        None => Ok(()),
        Some(r) if Path::new(r) == ws.path.as_path() => Ok(()),
        Some(_) => Err(
            "The chosen folder isn't the workspace this request is admitted for, so \
            OpenPaperDesign won't use it. Open that folder's project in Codex. Nothing was read \
            or changed."
                .into(),
        ),
    }
}

impl Launcher {
    /// Admission for a direct view action. The call's own context must be valid and match
    /// the pinned workspace (if any). Admission itself changes nothing: it returns the
    /// workspace and whether the session is still unpinned, and the caller pins only once
    /// its action is accepted. On any failure: no pin, no retirement, no binding change.
    fn admit_direct(
        state: &LauncherState,
        meta: &RequestMetaObject,
    ) -> Result<(Workspace, bool), String> {
        if let Some(reason) = &state.invalidated {
            return Err(format!(
                "This session was retired ({reason}). Nothing was read or changed."
            ));
        }
        if !state.host_context {
            // Standalone: the deliberately configured workspace, as for every tool.
            let ws = state
                .workspace
                .clone()
                .ok_or_else(|| NO_CONTEXT.to_string())?;
            return Ok((ws, false));
        }
        let ws = Workspace::from_meta(meta).map_err(|e| format!("{NO_CONTEXT} ({e})"))?;
        match &state.workspace {
            Some(pinned) if pinned.path != ws.path || pinned.id != ws.id => Err(
                "This request's Codex workspace isn't the one this session uses. Nothing was \
                 read or changed."
                    .into(),
            ),
            Some(_) => Ok((ws, false)),
            None => Ok((ws, true)),
        }
    }

    pub(super) async fn direct(
        &self,
        name: &str,
        args: Value,
        meta: &RequestMetaObject,
    ) -> ToolResult {
        let (strings, flags): (&[&str], &[&str]) = match name {
            UI_CREATE => (&["root", "name"], &["force"]),
            UI_OPEN => (&["root", "project"], &["force"]),
            _ => (&["root"], &[]),
        };
        let map = strict(&args, strings, flags)?;
        let get = |k: &str| map.get(k).and_then(Value::as_str);
        let force = map.get("force").and_then(Value::as_bool).unwrap_or(false);
        let mut state = self.state.clone().lock_owned().await;
        let (ws, pin) = match Self::admit_direct(&state, meta) {
            Ok(admitted) => admitted,
            Err(why) => {
                let observed = state.host_context && meta.contains_key(CAPABILITY);
                let owned = match &state.bound {
                    Some(b) => b.owner == Owner::Direct,
                    None => state.workspace.is_some() && state.pinned_by == Owner::Direct,
                };
                if observed && owned && state.invalidated.is_none() {
                    Self::retire_session(state, why.clone()).await?;
                    return Err(format!(
                        "{why} This view's access to the folder changed, so its canvas was \
                         closed: nothing more can be saved there from this session."
                    ));
                }
                return Err(why);
            }
        };
        same_root(&ws, get("root"))?;
        // These actions end in the editor: without it, refuse before changing anything.
        if name != UI_CANVASES && !bundle_complete() {
            return Err(
                "This openpaperdesign-mcp was built without the browser editor, so a \
                canvas can't be opened from here. Nothing was changed."
                    .into(),
            );
        }
        let (action, dir) = match name {
            UI_CANVASES => {
                let bound = state.bound.as_ref().map(|b| b.server.dir().to_path_buf());
                let out = ws.canvases(bound.as_deref())?;
                if pin {
                    state.workspace = Some(ws); // accepted, still under the same lock
                    state.pinned_by = Owner::Direct;
                }
                return Ok(out);
            }
            UI_EDITOR => {
                if state.bound.is_none() {
                    return Err("No canvas is open in this session yet. Open one from a \
                        project's canvases. Nothing was changed."
                        .into());
                }
                let mut v = Self::editor(&mut state, &ws)?;
                if pin {
                    state.workspace = Some(ws); // accepted, still under the same lock
                    state.pinned_by = Owner::Direct;
                }
                v["action"] = json!("reopened");
                return text(v);
            }
            UI_CREATE => {
                let name = get("name")
                    .filter(|s| !s.trim().is_empty())
                    .ok_or("a canvas name is required; nothing was created")?;
                let (dir, _) =
                    Self::create(state, ws.clone(), name, force, pin, Owner::Direct).await?;
                ("created", dir)
            }
            UI_OPEN => {
                let project = get("project").ok_or("a canvas is required; nothing was opened")?;
                // Only a canvas folder directly under design/, as listed.
                let rest = project
                    .strip_prefix("design/")
                    .filter(|r| !r.is_empty() && !r.contains(['/', '\\']) && *r != "." && *r != "..")
                    .ok_or("only a canvas folder directly under design/ can be opened here; nothing was opened")?;
                let dir = Self::existing(&ws, &format!("design/{rest}"))?;
                Self::bind(state, ws.clone(), dir.clone(), force, pin, Owner::Direct).await?;
                ("opened", dir)
            }
            _ => return Err(format!("unknown tool {name}")),
        };
        // The bind handoff released the lock. Give the link only for exactly that canvas.
        let mut state = self.state.clone().lock_owned().await;
        if state.invalidated.is_some()
            || state.bound.as_ref().map(|b| b.server.dir()) != Some(dir.as_path())
        {
            return Err(format!(
                "The canvas was {action}, but this session's canvas changed before it could be \
                 opened. Open it again from the list."
            ));
        }
        let mut v = Self::editor(&mut state, &ws)?;
        v["action"] = json!(action);
        text(v)
    }
}

/// `openpaperdesign_choose_folder`: the host's directory picker, as input only.
pub(super) async fn choose_folder(
    args: Option<&serde_json::Map<String, Value>>,
    context: &RequestContext<RoleServer>,
) -> ToolResult {
    if args.is_some_and(|a| !a.is_empty()) {
        return Err(format!("{PICK_TOOL} takes no arguments; nothing was asked"));
    }
    let unsupported = |reason: &str| text(json!({"status": "unsupported", "reason": reason}));
    let Some(info) = context.peer.peer_info() else {
        return unsupported("the host hasn't initialized this connection");
    };
    let form = info
        .capabilities
        .extensions
        .as_ref()
        .and_then(|e| e.get("openai/elicitation"))
        .and_then(|e| e.get("form"));
    if form.is_none() {
        return unsupported("this host doesn't offer its folder picker to apps");
    }
    // From 2026-07-28 OpenAI forms use multi-round-trip requests, which this adapter doesn't
    // implement; the legacy method is for earlier protocols only.
    if info.protocol_version.as_str() >= ProtocolVersion::V_2026_07_28.as_str() {
        return unsupported("this host's protocol needs a newer picker request");
    }
    let params = json!({
        "mode": "form",
        "message": "Choose the project folder to open in OpenPaperDesign.",
        "requestedSchema": {
            "type": "object",
            "properties": {PICK_FIELD: {
                "type": "string",
                "format": "uri",
                "title": "Project folder",
                "x-openai-input": {"type": "resource", "options": [], "userOptions": {"kind": "directory"}}
            }},
            "required": [PICK_FIELD]
        }
    });
    let request = ServerRequest::CustomRequest(CustomRequest::new(PICK_METHOD, Some(params)));
    let handle = context
        .peer
        .send_request_with_option(request, PeerRequestOptions::with_timeout(PICK_TIMEOUT))
        .await
        .map_err(|e| format!("the folder picker couldn't be asked: {e}"))?;
    let id = handle.id.clone();
    let reply = tokio::select! {
        r = handle.await_response() => r,
        () = context.ct.cancelled() => {
            // Withdraw the pending picker; a late answer is then dropped unmatched.
            let reason = Some("the tool call was cancelled".to_string());
            let _ = context
                .peer
                .notify_cancelled(CancelledNotificationParam::new(Some(id), reason))
                .await;
            return Err("cancelled; nothing was selected".into());
        }
    };
    let reply = match reply {
        Ok(r) => serde_json::to_value(r).map_err(|e| e.to_string())?,
        Err(rmcp::ServiceError::Timeout { .. }) => return text(json!({"status": "timeout"})),
        Err(rmcp::ServiceError::McpError(e)) => {
            return text(json!({"status": "unavailable", "code": e.code.0}));
        }
        Err(e) => return Err(format!("the folder picker failed: {e}")),
    };
    text(classify(&reply)?)
}

/// Strictly read the host's answer. Only an accepted local `file://` directory URI becomes a
/// selection, decoded without touching the filesystem.
fn classify(reply: &Value) -> Result<Value, String> {
    let malformed =
        || "The host's picker answer wasn't a valid folder choice; nothing was selected.";
    let obj = reply.as_object().ok_or_else(malformed)?;
    if obj
        .keys()
        .any(|k| !["action", "content", "_meta"].contains(&k.as_str()))
    {
        return Err(malformed().into());
    }
    let content = obj.get("content").filter(|c| !c.is_null());
    match obj.get("action").and_then(Value::as_str) {
        Some("decline") if content.is_none() => return Ok(json!({"status": "declined"})),
        Some("cancel") if content.is_none() => return Ok(json!({"status": "cancelled"})),
        Some("accept") => {}
        _ => return Err(malformed().into()),
    }
    let content = content.and_then(Value::as_object).ok_or_else(malformed)?;
    if content.len() != 1 {
        return Err(malformed().into());
    }
    let uri = content
        .get(PICK_FIELD)
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    if !uri.starts_with("file:") {
        // A host reference that isn't a local path (or no URI at all): unusable here.
        let scheme = uri.split_once(':').map(|(s, _)| s).filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        });
        return match scheme {
            Some(_) => Ok(json!({"status": "opaque"})),
            None => Err(malformed().into()),
        };
    }
    let path = scope::file_uri(uri).map_err(|_| malformed())?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    Ok(json!({
        "status": "selected",
        "folder": {"path": path, "name": name},
        "access": "none",
        "note": "Input only: the folder was not opened, listed or checked, and choosing it grants no access. A project action works only where this request also carries Codex workspace context for exactly this folder.",
    }))
}

#[cfg(test)]
#[path = "launcher_direct_tests.rs"]
mod tests;
