//! The folder picker over a real rmcp client/server pair (in-memory duplex): the client is a
//! synthetic host that records the request it is sent and answers it. Its envelopes are
//! shaped from the operator's Codex receipts; this is not evidence of the installed host.
use super::*;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ContentBlock, CustomResult, ErrorCode,
    Implementation,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ClientHandler, RoleClient};
use std::io::Write as _;
use std::sync::Mutex as StdMutex;

use super::super::tests::context;

#[derive(Clone)]
enum Answer {
    Reply(Value),
    Error(i32),
    /// Answers only after the server stopped waiting.
    Late(Value),
}

/// Every request the host was sent: method and params.
type Seen = Arc<StdMutex<Vec<(String, Option<Value>)>>>;

#[derive(Clone)]
struct Host {
    extensions: Option<Value>,
    answer: Answer,
    seen: Seen,
}

impl Host {
    fn new(answer: Answer) -> Self {
        Self {
            extensions: Some(json!({"openai/elicitation": {"form": {}}, "openai/form": {}})),
            answer,
            seen: Default::default(),
        }
    }
}

impl ClientHandler for Host {
    fn get_info(&self) -> ClientConfig {
        let mut caps = ClientCapabilities::default();
        caps.extensions = self
            .extensions
            .clone()
            .map(|e| serde_json::from_value(e).unwrap());
        ClientConfig::new(caps, Implementation::new("synthetic-host", "0"))
            .with_protocol_version(ProtocolVersion::V_2025_06_18)
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleClient>,
    ) -> Result<CustomResult, ErrorData> {
        self.seen
            .lock()
            .unwrap()
            .push((request.method.clone(), request.params.clone()));
        match &self.answer {
            Answer::Reply(v) => Ok(CustomResult(v.clone())),
            Answer::Error(code) => Err(ErrorData::new(ErrorCode(*code), "no picker", None)),
            Answer::Late(v) => {
                tokio::time::sleep(PICK_TIMEOUT * 3).await;
                Ok(CustomResult(v.clone()))
            }
        }
    }
}

struct Session {
    client: RunningService<RoleClient, Host>,
    launcher: Launcher,
    seen: Seen,
}

async fn connect(host: Host) -> Session {
    let launcher = Launcher::new(None);
    let (client, server) = tokio::io::duplex(1 << 20);
    let l = launcher.clone();
    tokio::spawn(async move {
        let _ = l.serve(server).await.unwrap().waiting().await;
    });
    let seen = host.seen.clone();
    let client = host.serve(client).await.unwrap();
    Session {
        client,
        launcher,
        seen,
    }
}

async fn call(s: &Session, name: &str, args: Value, meta: RequestMetaObject) -> (bool, Value) {
    let mut p = CallToolRequestParams::new(name.to_string());
    p.arguments = args.as_object().cloned();
    p.meta = Some(meta);
    let r = tokio::time::timeout(std::time::Duration::from_secs(15), s.client.call_tool(p))
        .await
        .expect("bounded")
        .unwrap();
    let text = r
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (r.is_error != Some(true), v)
}

/// The launcher's whole session state, comparable before and after.
async fn state(s: &Session) -> Value {
    let st = s.launcher.state.lock().await;
    json!({
        "workspace": st.workspace.as_ref().map(|w| (&w.path, w.id)),
        "invalidated": st.invalidated,
        "bound": st.bound.as_ref().map(|b| (b.server.dir().to_path_buf(), b.port, b.editor_opened)),
    })
}

/// The app's call: what the host was observed to send from an app view (no workspace context).
fn app_meta() -> RequestMetaObject {
    json!({"thread_id": "t", "threadId": "t"})
        .as_object()
        .unwrap()
        .clone()
        .into()
}

fn scratch(tag: &str) -> PathBuf {
    use std::hash::{BuildHasher, RandomState};
    let dir = std::env::temp_dir().join(format!(
        "opd-direct-{tag}-{:016x}",
        RandomState::new().hash_one(std::time::SystemTime::now())
    ));
    std::fs::create_dir(&dir).unwrap();
    dir.canonicalize().unwrap()
}

const NOWHERE: &str = "file:///nowhere/opd%20pick%20%C3%A9";

#[tokio::test]
async fn pick_sends_the_documented_directory_request_and_reports_input_only() {
    let s = connect(Host::new(Answer::Reply(
        json!({"action": "accept", "content": {"folder": NOWHERE}}),
    )))
    .await;
    let before = state(&s).await;
    let (ok, v) = call(&s, PICK_TOOL, json!({}), app_meta()).await;
    assert!(ok, "{v}");
    assert_eq!(v["status"], "selected");
    assert_eq!(v["access"], "none");
    // Decoded exactly, never looked up: this folder doesn't exist.
    assert_eq!(v["folder"]["path"], "/nowhere/opd pick é");
    assert_eq!(v["folder"]["name"], "opd pick é");
    assert!(!Path::new("/nowhere").exists());
    let seen = s.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one correlated request");
    let (method, params) = &seen[0];
    assert_eq!(method, "openai/elicitation/create");
    let params = params.clone().unwrap();
    assert_eq!(params["mode"], "form");
    assert_eq!(params["requestedSchema"]["required"], json!(["folder"]));
    assert_eq!(
        params["requestedSchema"]["properties"]["folder"],
        json!({"type":"string","format":"uri","title":"Project folder",
               "x-openai-input":{"type":"resource","options":[],"userOptions":{"kind":"directory"}}})
    );
    assert_eq!(state(&s).await, before, "nothing pinned, bound or retired");
    // Arguments are refused before anything is asked.
    let (ok, v) = call(&s, PICK_TOOL, json!({"folder": NOWHERE}), app_meta()).await;
    assert!(!ok && v.to_string().contains("takes no arguments"), "{v}");
    assert_eq!(s.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pick_answers_other_than_a_local_directory_select_nothing() {
    let accept = |c: Value| json!({"action": "accept", "content": c});
    let cases = [
        (json!({"action": "decline"}), Some("declined")),
        (
            json!({"action": "decline", "content": null}),
            Some("declined"),
        ),
        (json!({"action": "cancel"}), Some("cancelled")),
        (
            accept(json!({"folder": "host-resource://folder-1"})),
            Some("opaque"),
        ),
        (accept(json!({"folder": "file://remote.example/x"})), None),
        (accept(json!({"folder": "file:relative/x"})), None),
        (accept(json!({"folder": "file:///a/../b"})), None),
        (accept(json!({"folder": "file:///a%2Fb"})), None),
        (accept(json!({"folder": "/plain/path"})), None),
        (accept(json!({"folder": 7})), None),
        (accept(json!({"folder": NOWHERE, "extra": 1})), None),
        (accept(json!({})), None),
        (json!({"action": "accept"}), None),
        (
            json!({"action": "decline", "content": {"folder": NOWHERE}}),
            None,
        ),
        (json!({"action": "maybe"}), None),
        (json!("accept"), None),
        (json!({}), None),
    ];
    for (reply, status) in cases {
        let s = connect(Host::new(Answer::Reply(reply.clone()))).await;
        let before = state(&s).await;
        let (ok, v) = call(&s, PICK_TOOL, json!({}), app_meta()).await;
        match status {
            Some(status) => assert!(ok && v["status"] == status, "{reply} -> {v}"),
            None => assert!(
                !ok && v.to_string().contains("nothing was selected"),
                "{reply} -> {v}"
            ),
        }
        assert!(v.get("folder").is_none(), "{reply} -> {v}");
        assert_eq!(state(&s).await, before, "{reply}");
    }
    // rmcp decodes the answer into its typed elicitation result before the adapter sees it, so
    // an unknown top-level field is dropped there; only `action` and `content` are read.
    let s = connect(Host::new(Answer::Reply(
        json!({"action": "accept", "content": {"folder": NOWHERE}, "path": "/elsewhere"}),
    )))
    .await;
    let (ok, v) = call(&s, PICK_TOOL, json!({}), app_meta()).await;
    assert!(ok && v["folder"]["path"] == "/nowhere/opd pick é", "{v}");
    // A host error answer.
    let s = connect(Host::new(Answer::Error(-32601))).await;
    let (ok, v) = call(&s, PICK_TOOL, json!({}), app_meta()).await;
    assert!(
        ok && v == json!({"status": "unavailable", "code": -32601}),
        "{v}"
    );
}

#[tokio::test]
async fn pick_is_not_asked_of_a_host_without_the_picker() {
    for extensions in [
        None,
        Some(json!({"openai/form": {}})),
        Some(json!({"openai/elicitation": {}})),
    ] {
        let mut host = Host::new(Answer::Reply(
            json!({"action": "accept", "content": {"folder": NOWHERE}}),
        ));
        host.extensions = extensions.clone();
        let s = connect(host).await;
        let (ok, v) = call(&s, PICK_TOOL, json!({}), app_meta()).await;
        assert!(ok && v["status"] == "unsupported", "{extensions:?} -> {v}");
        assert!(s.seen.lock().unwrap().is_empty(), "never asked");
    }
}

#[tokio::test]
async fn pick_timeout_then_late_answer_changes_nothing() {
    let s = connect(Host::new(Answer::Late(
        json!({"action": "accept", "content": {"folder": NOWHERE}}),
    )))
    .await;
    let before = state(&s).await;
    let (ok, v) = call(&s, PICK_TOOL, json!({}), app_meta()).await;
    assert!(ok && v == json!({"status": "timeout"}), "{v}");
    // Let the late answer arrive; the connection keeps working and nothing changed.
    tokio::time::sleep(PICK_TIMEOUT * 4).await;
    assert_eq!(state(&s).await, before);
    let (ok, v) = call(&s, "openpaperdesign_status", json!({}), app_meta()).await;
    assert!(ok && v["workspace_state"] == "unresolved", "{v}");
}

#[tokio::test]
async fn pick_and_unadmitted_view_calls_leave_a_bound_session_alone() {
    let ws = scratch("bound");
    let meta = context::writable(&ws);
    let s = connect(Host::new(Answer::Reply(
        json!({"action": "accept", "content": {
            "folder": context::uri(&ws)
        }}),
    )))
    .await;
    let (ok, v) = call(&s, "create_project", json!({"name": "Model"}), meta.clone()).await;
    assert!(ok, "{v}");
    let (ok, v) = call(&s, "open_editor", json!({}), meta.clone()).await;
    assert!(ok, "{v}");
    let before = state(&s).await;
    assert_eq!(before["bound"][2], true, "editor opened");
    // The picker answer names exactly the bound workspace: still input only.
    let (ok, v) = call(&s, PICK_TOOL, json!({}), app_meta()).await;
    assert!(ok && v["status"] == "selected", "{v}");
    // View calls without context, with the chosen folder, are refused and retire nothing.
    let root = ws.to_str().unwrap();
    for (name, args) in [
        (UI_CANVASES, json!({"root": root})),
        (UI_CREATE, json!({"root": root, "name": "Other"})),
        (UI_OPEN, json!({"root": root, "project": "design/Model"})),
        (UI_EDITOR, json!({"root": root})),
    ] {
        let (ok, v) = call(&s, name, args, app_meta()).await;
        assert!(
            !ok && v.to_string().contains("no Codex workspace context"),
            "{name}: {v}"
        );
    }
    assert!(!ws.join("design/Other").exists());
    assert_eq!(
        state(&s).await,
        before,
        "same pin, binding, server and editor"
    );
    let (ok, v) = call(&s, "openpaperdesign_status", json!({}), meta).await;
    assert!(ok && v["workspace_state"] == "pinned", "{v}");
    std::fs::remove_dir_all(&ws).unwrap();
}

/// Every path under `dir`, with file bytes: compared before and after.
fn tree(dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut all = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p.clone());
            }
            all.push((p.clone(), m.is_file().then(|| std::fs::read(&p).unwrap())));
        }
    }
    all.sort();
    all
}

/// A direct action refused after admission pins nothing, even with valid context A: the
/// session stays unresolved and a later legitimate context B still works.
#[tokio::test]
async fn direct_refusals_with_valid_context_pin_nothing() {
    let a = scratch("pin-a");
    let b = scratch("pin-b");
    std::fs::create_dir(a.join("design")).unwrap();
    let s = connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await;
    let ma = context::writable(&a);
    let before = state(&s).await;
    assert_eq!(before["workspace"], Value::Null);
    let files = tree(&a);
    let b_root = b.to_str().unwrap();
    for (name, args) in [
        (UI_CANVASES, json!({"root": b_root})),
        (UI_CREATE, json!({"root": b_root, "name": "X"})),
        (UI_CREATE, json!({"name": "   "})),
        (UI_OPEN, json!({"project": "Landing"})),
        (UI_OPEN, json!({"project": "design/Missing"})),
        (UI_EDITOR, json!({})),
    ] {
        let (ok, v) = call(&s, name, args.clone(), ma.clone()).await;
        assert!(!ok, "{name} {args}: {v}");
        assert_eq!(
            state(&s).await,
            before,
            "{name} {args}: nothing pinned or bound"
        );
    }
    assert_eq!(tree(&a), files, "no file changed in A");
    assert!(!b.join("design").exists(), "nothing made in B");
    let (ok, v) = call(
        &s,
        "openpaperdesign_status",
        json!({}),
        context::writable(&b),
    )
    .await;
    assert!(ok, "{v}");
    assert_eq!(v["workspace_state"], "pinned", "{v}");
    assert_eq!(v["workspace"], b_root, "B is usable: A was never pinned");
    assert_eq!(v["workspace_usable"], true, "{v}");
    for d in [a, b] {
        std::fs::remove_dir_all(d).unwrap();
    }
}

/// An accepted direct action pins with its result (a create or open together with its
/// binding), and bad input later leaves the pinned, bound session as it was.
#[tokio::test]
async fn direct_success_pins_and_later_bad_input_changes_nothing() {
    let a = scratch("pos-a");
    let other = scratch("pos-other");
    let ma = context::writable(&a);
    // Create first, on a fresh session: pinned and bound in one step.
    let s = connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await;
    let (ok, v) = call(&s, UI_CREATE, json!({"name": "Landing"}), ma.clone()).await;
    assert!(ok && v["action"] == "created", "{v}");
    let pinned = state(&s).await;
    assert_eq!(pinned["workspace"][0], json!(a));
    assert_eq!(pinned["bound"][0], json!(a.join("design/Landing")));
    assert_eq!(pinned["bound"][2], true, "editor link given");
    let files = tree(&a);
    for (name, args, meta) in [
        (
            UI_OPEN,
            json!({"project": "design/Missing", "force": true}),
            ma.clone(),
        ),
        (
            UI_CREATE,
            json!({"name": "Landing", "force": true}),
            ma.clone(),
        ),
        (
            UI_CANVASES,
            json!({"root": other.to_str().unwrap()}),
            ma.clone(),
        ),
        (UI_EDITOR, json!({}), app_meta()),
    ] {
        let (ok, v) = call(&s, name, args.clone(), meta).await;
        assert!(!ok, "{name} {args}: {v}");
        assert_eq!(state(&s).await, pinned, "{name} {args}");
    }
    assert_eq!(tree(&a), files);
    // Changed since fb22: a valid host context for another workspace is not bad input but an
    // observed identity change. On this view-installed binding it retires the session, as a
    // model call would (a model-installed binding is covered in launcher_tests).
    let (ok, v) = call(&s, UI_CANVASES, json!({}), context::writable(&other)).await;
    assert!(!ok && v.to_string().contains("canvas was closed"), "{v}");
    let retired = state(&s).await;
    assert!(
        retired["invalidated"].is_string() && retired["bound"].is_null(),
        "{retired}"
    );
    assert_eq!(tree(&a), files);
    // A read-only success pins too.
    let r = connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await;
    let (ok, v) = call(&r, UI_CANVASES, json!({}), ma.clone()).await;
    assert!(ok && v["canvases"][0]["project"] == "design/Landing", "{v}");
    assert_eq!(state(&r).await["workspace"][0], json!(a));
    assert_eq!(state(&r).await["bound"], Value::Null);
    for d in [a, other] {
        std::fs::remove_dir_all(d).unwrap();
    }
}

// ---- failed creation leaves nothing behind (and never removes what it didn't make) ----

use crate::test_support::Point;

fn seeded(tag: &str, with_design: bool) -> PathBuf {
    let ws = scratch(tag);
    std::fs::write(ws.join("DESIGN.md"), "# Direction\n").unwrap();
    if with_design {
        std::fs::create_dir(ws.join("design")).unwrap();
        std::fs::write(ws.join("design/tokens.json"), "{\"ink\":\"#222\"}\n").unwrap();
    }
    ws
}

/// The listener is acquired before anything is written: its failure leaves no folder, no
/// file and no pin, and the same name then works.
#[tokio::test]
async fn direct_create_listener_failure_writes_nothing() {
    for with_design in [false, true] {
        let ws = seeded("listen", with_design);
        let s = connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await;
        let (files, before) = (tree(&ws), state(&s).await);
        let mut fault = Point::arm(&ws, "listen_fails", false);
        let (ok, v) = call(
            &s,
            UI_CREATE,
            json!({"name": "Failed"}),
            context::writable(&ws),
        )
        .await;
        fault.reached().await;
        assert!(!ok && v.to_string().contains("cannot listen"), "{v}");
        assert_eq!(tree(&ws), files, "with design/: {with_design}");
        assert_eq!(state(&s).await, before, "nothing pinned or bound");
        let (ok, v) = call(
            &s,
            UI_CREATE,
            json!({"name": "Failed"}),
            context::writable(&ws),
        )
        .await;
        assert!(
            ok && v["project"] == "design/Failed",
            "the name is free: {v}"
        );
        std::fs::remove_dir_all(&ws).unwrap();
    }
}

/// A failure after the files were written removes exactly what this creation made: the new
/// folder, and design/ only if this call made it. Existing files stay byte for byte.
#[tokio::test]
async fn direct_create_failure_after_files_removes_only_what_it_made() {
    for with_design in [false, true] {
        let ws = seeded("after-files", with_design);
        let s = connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await;
        let (files, before) = (tree(&ws), state(&s).await);
        let mut fault = Point::arm(&ws, "create_fails", false);
        let (ok, v) = call(
            &s,
            UI_CREATE,
            json!({"name": "Failed"}),
            context::writable(&ws),
        )
        .await;
        fault.reached().await;
        assert!(!ok && v.to_string().contains("Nothing was created"), "{v}");
        assert_eq!(tree(&ws), files, "with design/: {with_design}");
        assert_eq!(
            ws.join("design").exists(),
            with_design,
            "design/ only if it existed"
        );
        assert_eq!(state(&s).await, before);
        // The model path uses the same creation and the same recovery.
        let mut fault = Point::arm(&ws, "create_fails", false);
        let (ok, v) = call(
            &s,
            "create_project",
            json!({"name": "Model"}),
            context::writable(&ws),
        )
        .await;
        fault.reached().await;
        assert!(!ok && v.to_string().contains("Nothing was created"), "{v}");
        assert_eq!(tree(&ws), files);
        std::fs::remove_dir_all(&ws).unwrap();
    }
}

/// A failed creation while another canvas is bound and open leaves that binding (server,
/// port, editor state) exactly as it was.
#[tokio::test]
async fn direct_failed_create_keeps_the_old_binding() {
    let ws = seeded("keep", true);
    let s = connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await;
    let w = context::writable(&ws);
    let (ok, v) = call(&s, UI_CREATE, json!({"name": "First"}), w.clone()).await;
    assert!(ok, "{v}");
    let (bound, files) = (state(&s).await, tree(&ws));
    for point in ["listen_fails", "create_fails"] {
        let mut fault = Point::arm(&ws, point, false);
        let (ok, v) = call(
            &s,
            UI_CREATE,
            json!({"name": "Second", "force": true}),
            w.clone(),
        )
        .await;
        fault.reached().await;
        assert!(!ok, "{point}: {v}");
        assert_eq!(
            state(&s).await,
            bound,
            "{point}: same binding, port and editor"
        );
        assert_eq!(tree(&ws), files, "{point}");
    }
    let (ok, v) = call(&s, UI_EDITOR, json!({}), w).await;
    assert!(ok && v["project"] == "design/First", "{v}");
    std::fs::remove_dir_all(&ws).unwrap();
}

/// Content that appears in the new folder before a failure is never removed: the creation
/// removes only its own files and names what had to stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_failed_create_never_removes_unknown_content() {
    let ws = seeded("unknown", true);
    let s = Arc::new(connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await);
    let mut pause = Point::arm(&ws, "create_after_files", true);
    let (s2, w) = (s.clone(), context::writable(&ws));
    let creating =
        tokio::spawn(async move { call(&s2, UI_CREATE, json!({"name": "Shared"}), w).await });
    pause.reached().await;
    let stray = ws.join("design/Shared/pages/notes.txt");
    std::fs::write(&stray, "someone else's").unwrap();
    let mut fault = Point::arm(&ws, "create_fails", false);
    pause.resume();
    let (ok, v) = tokio::time::timeout(std::time::Duration::from_secs(15), creating)
        .await
        .unwrap()
        .unwrap();
    fault.reached().await;
    assert!(!ok && v.to_string().contains("these paths remain"), "{v}");
    assert_eq!(
        std::fs::read(&stray).unwrap(),
        b"someone else's",
        "never removed"
    );
    assert!(
        !ws.join("design/Shared/project.json").exists(),
        "its own files are gone"
    );
    assert!(ws.join("design/tokens.json").exists() && ws.join("DESIGN.md").exists());
    assert_eq!(state(&s).await["bound"], Value::Null);
    std::fs::remove_dir_all(&ws).unwrap();
}

/// Run a create of `name` that pauses after its files are written, apply `change` to the new
/// project folder, then fail it before the binding is installed.
async fn interrupted_create(
    s: &Arc<Session>,
    ws: &Path,
    name: &str,
    change: impl FnOnce(&Path),
) -> (bool, Value) {
    let mut pause = Point::arm(ws, "create_after_files", true);
    let (s2, w) = (s.clone(), context::writable(ws));
    let args = json!({"name": name, "force": true});
    let creating = tokio::spawn(async move { call(&s2, UI_CREATE, args, w).await });
    pause.reached().await;
    change(&ws.join("design").join(name));
    let mut fault = Point::arm(ws, "create_fails", false);
    pause.resume();
    let answer = tokio::time::timeout(std::time::Duration::from_secs(15), creating)
        .await
        .unwrap()
        .unwrap();
    fault.reached().await;
    answer
}

fn ino(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).unwrap().ino()
}

/// R3-F1: recovery removes only what is still exactly this creation's. A manifest replaced by
/// another regular file, a page changed in place and a replaced folder all stay and are named;
/// the open canvas's binding is unchanged and usable; the ordinary owned case still removes all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_create_keeps_what_is_no_longer_exactly_its_own() {
    let ws = seeded("owned", true);
    let s = Arc::new(connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await);
    let w = context::writable(&ws);
    let (ok, v) = call(&s, UI_CREATE, json!({"name": "First"}), w.clone()).await;
    assert!(ok, "{v}");
    let bound = state(&s).await;
    let kept = |p: &str| std::fs::read(ws.join(p)).unwrap();
    let (direction, tokens) = (kept("DESIGN.md"), kept("design/tokens.json"));
    let page_of = |dir: &Path| {
        std::fs::read_dir(dir.join("pages"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|x| x == "json"))
            .unwrap()
    };

    // 1. The manifest is replaced (renamed over) by a different regular file.
    let (ok, v) = interrupted_create(&s, &ws, "Replaced", |dir| {
        let foreign = dir.join("foreign.tmp");
        std::fs::write(&foreign, "someone else's manifest").unwrap();
        assert_ne!(ino(&foreign), ino(&dir.join("project.json")));
        std::fs::rename(&foreign, dir.join("project.json")).unwrap();
    })
    .await;
    let dir = ws.join("design/Replaced");
    let text = v.to_string();
    assert!(!ok && text.contains("these paths remain"), "{v}");
    assert!(!text.contains("Nothing was created"), "{v}");
    assert!(text.contains("design/Replaced/project.json"), "{v}");
    assert!(
        text.matches("design/Replaced").count() >= 2,
        "the manifest and its non-empty folder are both named: {v}"
    );
    assert_eq!(
        std::fs::read(dir.join("project.json")).unwrap(),
        b"someone else's manifest"
    );
    assert!(
        !dir.join("pages").exists(),
        "its own page and empty pages/ were removed"
    );
    assert_eq!(state(&s).await, bound, "same binding, port and editor");

    // 2. The page is changed in place (same file, different bytes).
    let (ok, v) = interrupted_create(&s, &ws, "Edited", |dir| {
        let page = page_of(dir);
        let before = ino(&page);
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&page)
            .unwrap()
            .write_all(b"edited by someone else")
            .unwrap();
        assert_eq!(ino(&page), before);
    })
    .await;
    let dir = ws.join("design/Edited");
    assert!(!ok && v.to_string().contains("these paths remain"), "{v}");
    assert_eq!(
        std::fs::read(page_of(&dir)).unwrap(),
        b"edited by someone else"
    );
    assert!(
        !dir.join("project.json").exists(),
        "its own unchanged manifest was removed"
    );
    assert_eq!(state(&s).await, bound);

    // 3. The new folder is moved away and another empty folder put in its place.
    let (ok, v) = interrupted_create(&s, &ws, "Swapped", |dir| {
        std::fs::rename(dir, dir.with_extension("moved")).unwrap();
        std::fs::create_dir(dir).unwrap();
    })
    .await;
    assert!(!ok && v.to_string().contains("design/Swapped"), "{v}");
    assert!(
        ws.join("design/Swapped").is_dir(),
        "the replacement folder stays"
    );
    assert!(
        ws.join("design/Swapped.moved/project.json").is_file(),
        "the moved folder is untouched"
    );
    assert_eq!(state(&s).await, bound);

    // 4. Control: nothing changed, so everything this creation made is removed.
    let (ok, v) = interrupted_create(&s, &ws, "Owned", |_| {}).await;
    assert!(!ok && v.to_string().contains("Nothing was created"), "{v}");
    assert!(!ws.join("design/Owned").exists());

    assert_eq!(
        (kept("DESIGN.md"), kept("design/tokens.json")),
        (direction, tokens)
    );
    let (ok, v) = call(&s, UI_EDITOR, json!({}), w).await;
    assert!(
        ok && v["project"] == "design/First",
        "the old canvas stays usable: {v}"
    );
    std::fs::remove_dir_all(&ws).unwrap();
}

/// Makes `dir` unsearchable for this test and restores its mode on drop, even when an
/// assertion fails. `armed()` is false when the environment can't induce the denial (for
/// example when run as root); the caller then reports the case NOT RUN instead of passing it.
struct Unsearchable(PathBuf);
impl Unsearchable {
    fn new(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        Self(dir.to_path_buf())
    }
    fn armed(&self, inside: &Path) -> bool {
        matches!(std::fs::symlink_metadata(inside), Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
    }
}
impl Drop for Unsearchable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// R4-F1: only an observed NotFound is absence. A destination that can't be observed
/// (permission denied) is kept and named, never reported as cleanly removed; a genuinely
/// missing one still counts as gone.
#[test]
fn made_observation_error_is_uncertainty_not_absence() {
    let made_under = |root: &Path| {
        let project = root.join("canvas");
        let pages = project.join("pages");
        let mut made = Made::default();
        std::fs::create_dir_all(root).unwrap();
        std::fs::create_dir(&project).unwrap();
        made.dir(project.clone());
        std::fs::create_dir(&pages).unwrap();
        made.dir(pages.clone());
        std::fs::write(pages.join("page.json"), b"owned page").unwrap();
        made.file(pages.join("page.json"));
        std::fs::write(project.join("project.json"), b"owned manifest").unwrap();
        made.file(project.join("project.json"));
        (project, made)
    };
    let base = scratch("observe");
    // Control: genuinely gone before cleanup is absence.
    let (project, made) = made_under(&base.join("absent"));
    std::fs::remove_dir_all(&project).unwrap();
    assert_eq!(
        made.fail("later failure".into()),
        "later failure. Nothing was created."
    );
    // Observation denied: every tracked destination stays and is named.
    let parent = base.join("denied");
    let (project, made) = made_under(&parent);
    let answer = {
        let guard = Unsearchable::new(&parent);
        if !guard.armed(&project) {
            eprintln!("NOT RUN: this environment can't deny metadata access (root?)");
            std::fs::remove_dir_all(&base).unwrap();
            return;
        }
        made.fail("later failure".into())
    };
    assert!(!answer.contains("Nothing was created"), "{answer}");
    for p in [
        "canvas/project.json",
        "canvas/pages/page.json",
        "canvas/pages",
        "canvas",
    ] {
        assert!(
            answer.contains(parent.join(p).to_str().unwrap()),
            "{p}: {answer}"
        );
    }
    assert_eq!(
        std::fs::read(project.join("project.json")).unwrap(),
        b"owned manifest"
    );
    assert_eq!(
        std::fs::read(project.join("pages/page.json")).unwrap(),
        b"owned page"
    );
    std::fs::remove_dir_all(&base).unwrap();
}

/// R4-F1 through a full create: the existing design/ becomes unsearchable after the files are
/// written, then the create fails. The new canvas's paths are named (not "Nothing was
/// created"), its files and the existing direction/tokens keep their bytes, and the open
/// canvas's binding is unchanged; one final reopen shows it still usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_create_names_destinations_it_cannot_observe() {
    let ws = seeded("unobservable", true);
    let s = Arc::new(connect(Host::new(Answer::Reply(json!({"action": "cancel"})))).await);
    let w = context::writable(&ws);
    let (ok, v) = call(&s, UI_CREATE, json!({"name": "First"}), w.clone()).await;
    assert!(ok, "{v}");
    let bound = state(&s).await;
    let (direction, tokens) = (
        std::fs::read(ws.join("DESIGN.md")).unwrap(),
        std::fs::read(ws.join("design/tokens.json")).unwrap(),
    );
    let design = ws.join("design");
    let mut armed = true;
    let (ok, v) = {
        let mut guard = None;
        let answer = interrupted_create(&s, &ws, "Unseen", |dir| {
            let g = Unsearchable::new(&design);
            armed = g.armed(&dir.join("project.json"));
            guard = Some(g);
        })
        .await;
        drop(guard);
        answer
    };
    if !armed {
        eprintln!("NOT RUN: this environment can't deny metadata access (root?)");
        std::fs::remove_dir_all(&ws).unwrap();
        return;
    }
    let text = v.to_string();
    assert!(!ok && !text.contains("Nothing was created"), "{v}");
    assert!(
        text.contains("design/Unseen/project.json") && text.contains("design/Unseen/pages"),
        "{v}"
    );
    assert!(
        ws.join("design/Unseen/project.json").is_file(),
        "kept, as named"
    );
    assert_eq!(std::fs::read(ws.join("DESIGN.md")).unwrap(), direction);
    assert_eq!(
        std::fs::read(ws.join("design/tokens.json")).unwrap(),
        tokens
    );
    assert_eq!(state(&s).await, bound, "no pin or binding change");
    let (ok, v) = call(&s, UI_EDITOR, json!({}), w).await;
    assert!(ok && v["project"] == "design/First", "{v}");
    std::fs::remove_dir_all(&ws).unwrap();
}
