//! The app library (`--app-library`) through the real launcher, rmcp stream protocol, axum
//! routes and HTTP API the editor uses, with no workspace or Codex metadata. Two launchers in
//! one test stand for two host processes sharing the library: the lease is a real OS file lock
//! on separate open files, so it excludes them as it excludes separate processes (that case
//! runs as real child processes in `tests/app_library.rs`). Only the listener is in memory.
//! This is not a browser or a native Codex host: the renderer and navigation are not exercised.
use super::*;
use crate::library::{Lease, NewDocument};

struct Lib {
    root: PathBuf,
    l: Launcher,
    c: Arc<Client>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn library_root() -> PathBuf {
    use std::hash::{BuildHasher, RandomState};
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "opd-app-library-{:016x}",
        RandomState::new().hash_one(std::time::SystemTime::now())
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A library launcher with no request metadata at all.
async fn launch(root: &Path) -> (Launcher, Arc<Client>, tokio::task::JoinHandle<()>) {
    let l = Launcher::with_library(Library::open(root.to_str().unwrap()).unwrap());
    let (c, task) = connect_launcher(l.clone(), Default::default()).await;
    (l, c, task)
}

async fn library() -> Lib {
    let root = library_root();
    let (l, c, task) = launch(&root).await;
    Lib { root, l, c, task }
}

fn url(v: &Value) -> (u16, String) {
    let (port, token) = v["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:")
        .unwrap()
        .split_once("/#token=")
        .unwrap();
    (port.parse().unwrap(), token.to_string())
}

/// The binding identity an editor result names (views send it back with their actions).
fn binding(v: &Value) -> String {
    v["binding"].as_str().unwrap().to_string()
}

async fn bound_server(l: &Launcher) -> Arc<Server> {
    l.state.lock().await.bound.as_ref().unwrap().server.clone()
}

async fn open_doc(l: &Launcher) -> Option<String> {
    l.state
        .lock()
        .await
        .bound
        .as_ref()
        .and_then(|b| b.doc.clone())
}

/// One raw HTTP request; returns (status, head, body text).
async fn raw(
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    extra: &str,
    body: &str,
) -> (u16, String, String) {
    let mut client = connect(port).await;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    bounded(client.read_to_end(&mut bytes)).await.unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.to_string(), body.to_string())
}

fn etag_of(head: &str) -> String {
    head.lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("etag"))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap()
}

/// What the browser editor does for one edit: load the active page, change it, save it with
/// its revision. Returns the page id and its node count after the save.
async fn browser_edit(port: u16, token: &str) -> (String, usize) {
    let (st, _, project) = raw(port, token, "GET", "/api/project", "", "").await;
    assert_eq!(st, 200, "{project}");
    let page = serde_json::from_str::<Value>(&project).unwrap()["active_page"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, head, body) = raw(port, token, "GET", &format!("/api/pages/{page}"), "", "").await;
    assert_eq!(st, 200, "{body}");
    let mut data = paper_rs::project::format::page_from_json(&body).unwrap();
    let id = data.doc.alloc_id();
    data.doc.nodes.insert(
        id,
        paper_rs::model::Node::new(
            id,
            paper_rs::model::NodeKind::Rect,
            paper_rs::geom::Rect::new(5.0, 5.0, 40.0, 30.0),
        ),
    );
    data.doc.root.push(id);
    let (st, _, answer) = raw(
        port,
        token,
        "PUT",
        &format!("/api/pages/{page}"),
        &format!("If-Match: {}\r\n", etag_of(&head)),
        &paper_rs::project::format::page_to_json(&data),
    )
    .await;
    assert_eq!(st, 200, "{answer}");
    (page, data.doc.nodes.len())
}

async fn page_nodes(port: u16, token: &str, page: &str) -> usize {
    let (st, _, body) = raw(port, token, "GET", &format!("/api/pages/{page}"), "", "").await;
    assert_eq!(st, 200, "{body}");
    paper_rs::project::format::page_from_json(&body)
        .unwrap()
        .doc
        .nodes
        .len()
}

/// An HTTP MCP session on a canvas's loopback server that has read the active page.
async fn mcp_session(port: u16, token: &str, page: &str) -> String {
    let (status, head, v) = http(port, token, "POST", "/mcp", "", &json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"library-test", "version":"1"}}}).to_string()).await;
    assert_eq!(status, 200, "{v}");
    let sid = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap();
    let extra = format!("Mcp-Session-Id: {sid}\r\n");
    let note = json!({"jsonrpc":"2.0", "method":"notifications/initialized"}).to_string();
    assert_eq!(
        http(port, token, "POST", "/mcp", &extra, &note).await.0,
        202
    );
    http_call(
        port,
        token,
        &sid,
        "get_tree_summary",
        json!({"nodeId":"root", "pageId": page}),
    )
    .await
    .unwrap();
    sid
}

fn shape() -> Value {
    json!({"type": "rectangle", "x": 1, "y": 1, "width": 9, "height": 9})
}

/// A late request on an already accepted connection (a retained router), as other tests do.
async fn late_get(server: &Arc<Server>, port: u16, token: &str) -> StatusCode {
    router(server.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/project")
                .header("Host", format!("127.0.0.1:{port}"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The positive workflow with no metadata on any call: list (creates nothing), create, the
/// editor's own load and save, the editor's readiness report, close (needs force once the
/// editor link was given; saves nothing), then a fresh launcher on the same root lists and
/// reopens the same canvas with the edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_create_edit_save_close_and_reopen_without_metadata() {
    let lib = library().await;
    let listed = value(call(&lib.c, app::LIST, json!({})).await);
    assert_eq!(listed["canvases"], json!([]));
    assert_eq!(listed["complete"], true);
    assert!(
        !lib.root.join("library").exists(),
        "listing creates nothing"
    );
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Plan"})).await);
    assert_eq!(created["action"], "created");
    assert_eq!(created["storage"], "app-library");
    let id = created["canvas"]["id"].as_str().unwrap().to_string();
    assert!(crate::library::valid_id(&id));
    assert_eq!(created["canvas"]["name"], "Plan");
    let dir = lib.root.join("library/v1/canvases").join(&id);
    assert_eq!(created["location"], json!(dir));
    assert!(dir.join("project.json").is_file());
    assert!(!lib.root.join("DESIGN.md").exists() && !lib.root.join("design.md").exists());
    assert!(
        !dir.join("DESIGN.md").exists(),
        "no design system is applied to a blank canvas"
    );
    assert_eq!(created["editor_loaded"], false, "a link is not a load");
    let (port, token) = url(&created);
    let (page, nodes) = browser_edit(port, &token).await;
    assert_eq!(nodes, 1);
    assert_eq!(raw(port, &token, "POST", "/api/ready", "", "").await.0, 204);
    let b = binding(&created);
    let state = value(call(&lib.c, app::EDITOR, json!({"binding": b})).await);
    assert_eq!(state["editor_loaded"], true);
    assert_eq!(state["browser_saves"], 1);
    assert_eq!(url(&state), (port, token.clone()), "the same binding");
    assert_eq!(binding(&state), b);
    let old = bound_server(&lib.l).await;
    let refusal = call(&lib.c, app::CLOSE, json!({"binding": b}))
        .await
        .unwrap_err();
    assert!(refusal.contains("force: true"), "{refusal}");
    let closed = value(call(&lib.c, app::CLOSE, json!({"binding": b, "force": true})).await);
    assert_eq!(closed["action"], "closed");
    assert!(
        closed["note"]
            .as_str()
            .unwrap()
            .contains("not saved by closing")
    );
    assert_eq!(open_doc(&lib.l).await, None);
    assert_eq!(
        late_get(&old, port, &token).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        lib.root
            .join(format!("library/v1/locks/{id}.lock"))
            .is_file(),
        "lease file stays"
    );
    // A fresh launcher (a new process) on the same root.
    let (l2, c2, task2) = launch(&lib.root).await;
    let listed = value(call(&c2, app::LIST, json!({})).await);
    assert_eq!(listed["canvases"][0]["id"], json!(id));
    assert_eq!(listed["canvases"][0]["name"], "Plan");
    assert_eq!(listed["canvases"][0]["open"], false);
    let reopened = value(call(&c2, app::OPEN, json!({"id": id})).await);
    assert_eq!(reopened["action"], "opened");
    let (port2, token2) = url(&reopened);
    assert_eq!(
        page_nodes(port2, &token2, &page).await,
        1,
        "the saved edit is there"
    );
    bound_server(&l2).await.retire().await;
    task2.abort();
}

/// Library mode refuses every project route without admission, whatever the arguments or
/// metadata say, and a workspace process refuses library actions without retiring anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_mode_has_no_project_routes_and_takes_no_paths() {
    let lib = library().await;
    let elsewhere = lib.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    call(&lib.c, app::CREATE, json!({"name": "Only"}))
        .await
        .unwrap();
    for meta in [Default::default(), context::writable(&elsewhere)] {
        for (tool, args) in [
            ("bind_project", json!({"path": elsewhere})),
            ("create_project", json!({"name": "Repo"})),
            ("open_editor", json!({})),
            ("openpaperdesign_canvases", json!({})),
            (
                "openpaperdesign_ui_open",
                json!({"root": elsewhere, "project": "design/X"}),
            ),
            ("openpaperdesign_choose_folder", json!({})),
        ] {
            let e = call_meta(&lib.c, tool, args, meta.clone())
                .await
                .unwrap_err();
            assert!(e.contains("runs the app library"), "{tool}: {e}");
        }
        // Upstream design tools work only on the open library canvas the call names, never
        // on a path or a workspace, whatever the metadata says.
        let e = call_meta(
            &lib.c,
            "get_tree_summary",
            json!({"nodeId": "root"}),
            meta.clone(),
        )
        .await
        .unwrap_err();
        assert!(e.contains("Name the canvas you mean"), "{e}");
        let e = call_meta(
            &lib.c,
            "open_file",
            json!({"fileId": elsewhere, "binding": "0123456789abcdef.1"}),
            meta.clone(),
        )
        .await
        .unwrap_err();
        assert!(e.contains("isn't the canvas open"), "{e}");
        for refused in ["export", "create_file", "export_combined_pdf"] {
            let e = call_meta(&lib.c, refused, json!({}), meta.clone())
                .await
                .unwrap_err();
            assert!(e.contains("runs the app library"), "{refused}: {e}");
        }
        for (tool, args, want) in [
            (app::OPEN, json!({"id": "../../elsewhere"}), "canvas id"),
            (
                app::OPEN,
                json!({"id": "0123456789abcdef", "path": "/etc"}),
                "unexpected argument",
            ),
            (
                app::CREATE,
                json!({"name": "x", "root": elsewhere}),
                "unexpected argument",
            ),
            (
                app::CREATE,
                json!({"name": "a\u{7}b"}),
                "control characters",
            ),
            (app::CREATE, json!({"name": "x".repeat(61)}), "1 to 60"),
            (
                app::OPEN,
                json!({"id": "0123456789abcdef"}),
                "no such canvas",
            ),
        ] {
            let e = call_meta(&lib.c, tool, args, meta.clone())
                .await
                .unwrap_err();
            assert!(e.contains(want), "{tool}: {e}");
        }
    }
    let listed = value(call(&lib.c, app::LIST, json!({})).await);
    assert_eq!(listed["canvases"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing reached it"
    );
    assert!(
        !lib.root
            .join("library/v1/locks/0123456789abcdef.lock")
            .exists(),
        "no lease file for a canvas that doesn't exist"
    );
    let status = value(call(&lib.c, "openpaperdesign_status", json!({})).await);
    assert_eq!(status["mode"], "app-library");
    assert_eq!(status["workspace_usable"], false);
    bound_server(&lib.l).await.retire().await;
    // A packaged workspace process without metadata: refused, nothing retired or pinned.
    let l = Launcher::new(None);
    let (c, task) = connect_launcher(l.clone(), Default::default()).await;
    let e = call(&c, app::LIST, json!({})).await.unwrap_err();
    assert!(e.contains("wasn't started for the app library"), "{e}");
    let st = l.state.lock().await;
    assert!(st.invalidated.is_none() && st.workspace.is_none() && st.bound.is_none());
    drop(st);
    task.abort();
}

/// The lease across two "processes": the same canvas is busy before anything opens it, a
/// different one is fine, the same canvas again is a checked no-op, a busy target keeps the
/// current canvas and URL, and close, switch and an ended process release it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_leases_exclude_the_same_canvas_only() {
    let a = library().await;
    let (lb, b, task_b) = launch(&a.root).await;
    let one = value(call(&a.c, app::CREATE, json!({"name": "One"})).await);
    let id1 = one["canvas"]["id"].as_str().unwrap().to_string();
    let (port1, token1) = url(&one);
    let e = call(&b, app::OPEN, json!({"id": id1})).await.unwrap_err();
    assert!(e.contains("open in another OpenPaperDesign window"), "{e}");
    assert_eq!(open_doc(&lb).await, None);
    let two = value(call(&b, app::CREATE, json!({"name": "Two"})).await);
    let id2 = two["canvas"]["id"].as_str().unwrap().to_string();
    assert_ne!(id1, id2);
    let again = value(call(&a.c, app::OPEN, json!({"id": id1})).await);
    assert_eq!(again["action"], "already-open");
    assert_eq!(url(&again), (port1, token1.clone()));
    let server1 = bound_server(&a.l).await;
    let b1 = binding(&one);
    let e = call(
        &a.c,
        app::OPEN,
        json!({"id": id2, "force": true, "replace": b1}),
    )
    .await
    .unwrap_err();
    assert!(e.contains("open in another OpenPaperDesign window"), "{e}");
    assert!(
        Arc::ptr_eq(&bound_server(&a.l).await, &server1),
        "busy target keeps the binding"
    );
    assert_eq!(
        raw(port1, &token1, "GET", "/api/project", "", "").await.0,
        200
    );
    // B closes Two; A switches to it, which releases One for B.
    call(
        &b,
        app::CLOSE,
        json!({"binding": binding(&two), "force": true}),
    )
    .await
    .unwrap();
    let switched = value(
        call(
            &a.c,
            app::OPEN,
            json!({"id": id2, "force": true, "replace": b1}),
        )
        .await,
    );
    assert_eq!(switched["action"], "opened");
    let reopened = value(call(&b, app::OPEN, json!({"id": id1})).await);
    assert_eq!(reopened["action"], "opened");
    // An ended "process" (its binding dropped without a close) releases its lease too.
    task_b.abort();
    let _ = task_b.await;
    drop(b);
    drop(lb);
    let (l3, c3, task3) = launch(&a.root).await;
    let mut opened = None;
    for _ in 0..50 {
        match call(&c3, app::OPEN, json!({"id": id1})).await {
            Ok(v) => {
                opened = Some(v);
                break;
            }
            Err(e) if e.contains("another OpenPaperDesign window") => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(opened.is_some(), "the ended launcher's lease was released");
    bound_server(&l3).await.retire().await;
    bound_server(&a.l).await.retire().await;
    task3.abort();
}

/// A switch keeps the old canvas's lease until its started writer finished: another process
/// can't open it meanwhile. A caller that goes away doesn't stop the handoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_switch_holds_the_lease_until_the_started_writer_drained() {
    let a = library().await;
    let (lb, b, task_b) = launch(&a.root).await;
    let two = value(call(&b, app::CREATE, json!({"name": "Two"})).await);
    let id2 = two["canvas"]["id"].as_str().unwrap().to_string();
    call(
        &b,
        app::CLOSE,
        json!({"binding": binding(&two), "force": true}),
    )
    .await
    .unwrap();
    let one = value(call(&a.c, app::CREATE, json!({"name": "One"})).await);
    let id1 = one["canvas"]["id"].as_str().unwrap().to_string();
    let dir1: PathBuf = serde_json::from_value(one["location"].clone()).unwrap();
    let (port, token) = url(&one);
    let page = crate::read_manifest_checked(&dir1).unwrap().pages[0]
        .id
        .clone();
    let sid = mcp_session(port, &token, &page).await;
    let old = bound_server(&a.l).await;
    let mut snapshot = Point::arm(&dir1, "snapshot", true);
    let t = token.clone();
    let running =
        tokio::spawn(async move { http_call(port, &t, &sid, "create_shape", shape()).await });
    snapshot.reached().await;
    let l = a.l.clone();
    // The switch target, for the cancelled caller; `id2` is checked again below.
    let target = id2.clone();
    let replace = binding(&one);
    let switch = tokio::spawn(async move {
        l.app(
            l.library.as_ref().unwrap(),
            app::OPEN,
            json!({"id": target, "force": true, "replace": replace}),
        )
        .await
    });
    bounded(old.lifecycle.retiring.notified()).await;
    switch.abort();
    assert!(switch.await.unwrap_err().is_cancelled());
    let e = call(&b, app::OPEN, json!({"id": id1})).await.unwrap_err();
    assert!(
        e.contains("another OpenPaperDesign window"),
        "lease held during drain: {e}"
    );
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    // The owned handoff finished without its caller.
    for _ in 0..100 {
        if open_doc(&a.l).await.as_deref() == Some(id2.as_str()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(open_doc(&a.l).await.as_deref(), Some(id2.as_str()));
    let reopened = value(call(&b, app::OPEN, json!({"id": id1})).await);
    let (p, t) = url(&reopened);
    assert_eq!(
        page_nodes(p, &t, &page).await,
        1,
        "the started write was kept"
    );
    bound_server(&lb).await.retire().await;
    bound_server(&a.l).await.retire().await;
    task_b.abort();
}

/// Failures before the handoff (listener, directory sync, commit, preparation) keep the open
/// canvas and its URL and remove exactly what the failed call made. The thread-local commit
/// fault needs the owned task on this same thread.
#[tokio::test(flavor = "current_thread")]
async fn library_failed_creation_keeps_the_open_canvas_and_removes_its_own_files() {
    let lib = library().await;
    let one = value(call(&lib.c, app::CREATE, json!({"name": "One"})).await);
    let (port, token) = url(&one);
    let server = bound_server(&lib.l).await;
    let canvases = lib.root.join("library/v1/canvases");
    let entries = || std::fs::read_dir(&canvases).unwrap().count();
    let check = |e: String, want: &str| {
        assert!(
            e.contains(want) && e.contains("Nothing was created"),
            "{want}: {e}"
        );
    };
    // Each switch confirms replacing exactly the open canvas.
    let create = |name: &str| json!({"name": name, "force": true, "replace": binding(&one)});
    let _listen = Point::arm(lib.root.as_path(), "listen_fails", false);
    check(
        call(&lib.c, app::CREATE, create("A")).await.unwrap_err(),
        "listen",
    );
    let _sync = Point::arm(&canvases, "sync_dir", false);
    check(
        call(&lib.c, app::CREATE, create("B")).await.unwrap_err(),
        "injected sync failure",
    );
    // Stage page, stage manifest, rename page, rename manifest (fails), then the rollback.
    crate::commit::fault::fail_at(&[3]);
    let e = call(&lib.c, app::CREATE, create("C")).await.unwrap_err();
    crate::commit::fault::fail_at(&[]);
    check(e, "nothing was written");
    let _prepare = Point::arm(lib.root.as_path(), "app_prepare_fails", false);
    check(
        call(&lib.c, app::CREATE, create("D")).await.unwrap_err(),
        "injected test failure",
    );
    assert_eq!(entries(), 1, "only the first canvas exists");
    assert!(
        Arc::ptr_eq(&bound_server(&lib.l).await, &server),
        "the open canvas is kept"
    );
    assert_eq!(
        raw(port, &token, "GET", "/api/project", "", "").await.0,
        200
    );
    // A removal that can't be made durable is named, never reported as clean.
    let lib2 = Library::open(lib.root.to_str().unwrap()).unwrap();
    let lease = Lease::fresh(&lib2).unwrap();
    let (dir, created) = lib2
        .create_document(&lease, &NewDocument::blank("E").fresh())
        .unwrap();
    let _sync = Point::arm(&canvases, "sync_dir", false);
    let left = created.undo();
    assert!(!dir.exists());
    assert!(
        left.iter().any(|l| l.contains("may not be durable")),
        "{left:?}"
    );
    server.retire().await;
}

/// The lease file or the library replaced under an open canvas fences it for browser and MCP
/// access, also after the original comes back; the next action closes it. A lease file that is
/// a link is refused before anything opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_replaced_lease_or_root_fences_old_handles() {
    let lib = library().await;
    let one = value(call(&lib.c, app::CREATE, json!({"name": "One"})).await);
    let id = one["canvas"]["id"].as_str().unwrap().to_string();
    let (port, token) = url(&one);
    let lock = lib.root.join(format!("library/v1/locks/{id}.lock"));
    let aside = lock.with_extension("aside");
    std::fs::rename(&lock, &aside).unwrap();
    std::fs::write(&lock, b"").unwrap();
    assert_eq!(
        raw(port, &token, "GET", "/api/project", "", "").await.0,
        403
    );
    std::fs::remove_file(&lock).unwrap();
    std::fs::rename(&aside, &lock).unwrap();
    assert_eq!(
        raw(port, &token, "GET", "/api/project", "", "").await.0,
        403,
        "stays fenced"
    );
    let old = bound_server(&lib.l).await;
    let e = call(&lib.c, app::EDITOR, json!({})).await.unwrap_err();
    assert!(e.contains("so it was closed"), "{e}");
    assert_eq!(open_doc(&lib.l).await, None);
    assert_eq!(
        late_get(&old, port, &token).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // A second link to the lease file.
    let two = value(call(&lib.c, app::OPEN, json!({"id": id})).await);
    let (port, token) = url(&two);
    std::fs::hard_link(&lock, lib.root.join("second-link")).unwrap();
    assert_eq!(
        raw(port, &token, "GET", "/api/project", "", "").await.0,
        403
    );
    std::fs::remove_file(lib.root.join("second-link")).unwrap();
    call(
        &lib.c,
        app::CLOSE,
        json!({"binding": binding(&two), "force": true}),
    )
    .await
    .unwrap();
    // A lease path that is a link is never followed or opened through.
    let target = lib.root.join("target-file");
    std::fs::remove_file(&lock).unwrap();
    std::os::unix::fs::symlink(&target, &lock).unwrap();
    let e = call(&lib.c, app::OPEN, json!({"id": id}))
        .await
        .unwrap_err();
    assert!(
        e.contains("symbolic link") && e.contains("Nothing was opened"),
        "{e}"
    );
    assert!(!target.exists());
    // The library folder itself replaced.
    let moved = lib.root.with_extension("moved");
    let fresh = value(call(&lib.c, app::CREATE, json!({"name": "Three"})).await);
    let (port, token) = url(&fresh);
    std::fs::rename(&lib.root, &moved).unwrap();
    std::fs::create_dir(&lib.root).unwrap();
    assert_eq!(
        raw(port, &token, "GET", "/api/project", "", "").await.0,
        403
    );
    let e = call(&lib.c, app::LIST, json!({})).await.unwrap_err();
    assert!(e.contains("moved or replaced"), "{e}");
    let _ = std::fs::remove_dir_all(&moved);
}

/// Export waits for a save under way and contains it; import validates the whole file and
/// publishes a new canvas without touching the source; a bad file creates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_export_waits_for_a_started_save_and_import_makes_a_new_canvas() {
    let lib = library().await;
    let e = call(&lib.c, app::EXPORT, json!({})).await.unwrap_err();
    assert!(e.contains("Open the canvas"), "{e}");
    let one = value(call(&lib.c, app::CREATE, json!({"name": "Export me"})).await);
    let id = one["canvas"]["id"].as_str().unwrap().to_string();
    let dir: PathBuf = serde_json::from_value(one["location"].clone()).unwrap();
    let (port, token) = url(&one);
    let page = crate::read_manifest_checked(&dir).unwrap().pages[0]
        .id
        .clone();
    let sid = mcp_session(port, &token, &page).await;
    let mut snapshot = Point::arm(&dir, "snapshot", true);
    let t = token.clone();
    let running =
        tokio::spawn(async move { http_call(port, &t, &sid, "create_shape", shape()).await });
    snapshot.reached().await;
    let c = lib.c.clone();
    let b1 = binding(&one);
    let args = json!({"binding": b1});
    let export = tokio::spawn(async move { call(&c, app::EXPORT, args).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!export.is_finished(), "export waits for the save under way");
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    let exported = value(bounded(export).await.unwrap());
    assert_eq!(exported["content"], "canvas-only");
    assert_eq!(
        binding(&exported),
        b1,
        "the export names the binding it read"
    );
    assert!(
        exported["file_name"]
            .as_str()
            .unwrap()
            .ends_with(".opdcanvas.json")
    );
    let file = exported["payload"].as_str().unwrap().to_string();
    let parsed: Value = serde_json::from_str(&file).unwrap();
    assert_eq!(
        parsed["pages"][0]["nodes"].as_array().unwrap().len(),
        1,
        "the save is in it"
    );
    let source = crate::read_manifest_checked(&dir).unwrap();
    let source_page = std::fs::read(dir.join("pages").join(format!("{page}.json"))).unwrap();
    // A bad file creates nothing and keeps the open canvas.
    let bad = file.replacen("\"format\":", "\"format\":\"x\",\"format\":", 1);
    let e = call(
        &lib.c,
        app::IMPORT,
        json!({"payload": bad, "force": true, "replace": b1}),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("appears twice") && e.contains("Nothing was imported"),
        "{e}"
    );
    assert_eq!(open_doc(&lib.l).await.as_deref(), Some(id.as_str()));
    assert_eq!(
        std::fs::read_dir(lib.root.join("library/v1/canvases"))
            .unwrap()
            .count(),
        1
    );
    let imported = value(
        call(
            &lib.c,
            app::IMPORT,
            json!({"payload": file, "force": true, "replace": b1}),
        )
        .await,
    );
    assert_eq!(imported["action"], "imported");
    let new_id = imported["canvas"]["id"].as_str().unwrap();
    assert_ne!(new_id, id);
    assert_eq!(imported["canvas"]["name"], "Export me");
    let (p, t) = url(&imported);
    assert_eq!(page_nodes(p, &t, &page).await, 1);
    let new_dir: PathBuf = serde_json::from_value(imported["location"].clone()).unwrap();
    let copy = crate::read_manifest_checked(&new_dir).unwrap();
    assert_ne!(copy.id, source.id, "a fresh identity");
    assert_eq!(copy.pages, source.pages);
    assert_eq!(
        crate::read_manifest_checked(&dir).unwrap(),
        source,
        "source untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("pages").join(format!("{page}.json"))).unwrap(),
        source_page
    );
    bound_server(&lib.l).await.retire().await;
}

/// A file with a node id the upstream parser would overflow on, a self-consistent group no top
/// level reaches, a page version upstream's migration would mis-handle, or a nested field the
/// parser would drop is an ordinary refusal: nothing is published, and the open canvas, its
/// URL and the session keep working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_refused_imports_keep_the_open_canvas_and_the_session() {
    let lib = library().await;
    let one = value(call(&lib.c, app::CREATE, json!({"name": "Keep"})).await);
    let (port, token) = url(&one);
    let b1 = binding(&one);
    let server = bound_server(&lib.l).await;
    // A saved rectangle, so the file has one real node with its canonical style.
    browser_edit(port, &token).await;
    let exported = value(call(&lib.c, app::EXPORT, json!({"binding": b1})).await);
    let file: Value = serde_json::from_str(exported["payload"].as_str().unwrap()).unwrap();
    let mut version0 = file.clone();
    version0["pages"][0]["version"] = json!(0);
    let mut lossy = file.clone();
    lossy["pages"][0]["nodes"][0]["style"]["fill"] = json!("#ff0000");
    let rect = |id: u64| json!({"id": id, "name": "R", "type": "rect", "rect": {"x": 0, "y": 0, "w": 5, "h": 5}});
    let mut huge = file.clone();
    huge["pages"][0]["nodes"] = json!([rect(u64::MAX)]);
    huge["pages"][0]["root"] = json!([u64::MAX]);
    let mut cycle = file.clone();
    let (mut a, mut b) = (rect(900), rect(901));
    a["parent"] = json!(901);
    a["children"] = json!([901]);
    b["parent"] = json!(900);
    b["children"] = json!([900]);
    cycle["pages"][0]["nodes"] = json!([a, b]);
    cycle["pages"][0]["root"] = json!([]);
    cycle["pages"][0]["next_id"] = json!(902);
    for (bad, want) in [
        (huge, "above the largest allowed id"),
        (cycle, "aren't reachable"),
        (version0, "must have version 3"),
        (lossy, "style.fill"),
    ] {
        let args = json!({"payload": bad.to_string(), "force": true, "replace": b1});
        let e = call(&lib.c, app::IMPORT, args).await.unwrap_err();
        assert!(
            e.contains(want) && e.contains("Nothing was imported"),
            "{want}: {e}"
        );
        assert!(Arc::ptr_eq(&bound_server(&lib.l).await, &server));
        assert_eq!(
            raw(port, &token, "GET", "/api/project", "", "").await.0,
            200
        );
        let listed = value(call(&lib.c, app::LIST, json!({})).await);
        assert_eq!(listed["canvases"].as_array().unwrap().len(), 1);
    }
    let again = value(call(&lib.c, app::EDITOR, json!({"binding": b1})).await);
    assert_eq!(
        url(&again),
        (port, token.clone()),
        "the session still works"
    );
    server.retire().await;
}

/// Two views of one process share its one binding. After the other view switches, the first
/// view's close, export, readiness poll and switch confirmation all name a binding that is no
/// longer open: each is refused, and the current canvas, its URL, files and lease stay. A
/// switch refusal names the current binding, so a confirmation is bound to exactly that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_stale_views_never_act_on_another_canvas() {
    let lib = library().await;
    let (view2, task2) = connect_launcher(lib.l.clone(), Default::default()).await;
    let one = value(call(&lib.c, app::CREATE, json!({"name": "One"})).await);
    let b1 = binding(&one);
    let id1 = one["canvas"]["id"].as_str().unwrap().to_string();
    // View 2 is asked to confirm replacing exactly One's binding, and does.
    let e = call(&view2, app::CREATE, json!({"name": "Two"}))
        .await
        .unwrap_err();
    assert!(
        e.contains("force: true") && e.contains(&format!("(open canvas: {b1})")),
        "{e}"
    );
    let two = value(
        call(
            &view2,
            app::CREATE,
            json!({"name": "Two", "force": true, "replace": b1}),
        )
        .await,
    );
    let b2 = binding(&two);
    assert_ne!(b1, b2);
    let (port2, token2) = url(&two);
    let server2 = bound_server(&lib.l).await;
    let dir2: PathBuf = serde_json::from_value(two["location"].clone()).unwrap();
    let files = || std::fs::read(dir2.join("project.json")).unwrap();
    let before = files();
    // View 1 still shows One.
    for (tool, args) in [
        (app::CLOSE, json!({"binding": b1, "force": true})),
        (app::EXPORT, json!({"binding": b1})),
        (app::EDITOR, json!({"binding": b1})),
        (app::OPEN, json!({"id": id1, "force": true, "replace": b1})),
        (
            app::CREATE,
            json!({"name": "Three", "force": true, "replace": b1}),
        ),
    ] {
        let e = call(&lib.c, tool, args).await.unwrap_err();
        assert!(e.contains("another view changed it"), "{tool}: {e}");
        assert!(Arc::ptr_eq(&bound_server(&lib.l).await, &server2), "{tool}");
        assert_eq!(
            raw(port2, &token2, "GET", "/api/project", "", "").await.0,
            200,
            "{tool}"
        );
    }
    assert_eq!(files(), before, "Two's files are untouched");
    let listed = value(call(&lib.c, app::LIST, json!({})).await);
    assert_eq!(
        listed["canvases"].as_array().unwrap().len(),
        2,
        "nothing created"
    );
    // A switch without the confirmation, or confirming nothing open, is refused too.
    let e = call(&lib.c, app::OPEN, json!({"id": id1, "force": true}))
        .await
        .unwrap_err();
    assert!(e.contains(&format!("(open canvas: {b2})")), "{e}");
    let e = call(&lib.c, app::CLOSE, json!({"binding": "0123456789abcdef"}))
        .await
        .unwrap_err();
    assert!(e.contains("isn't a binding"), "{e}");
    // Without a binding, "Open current canvas" shows what is open now: Two, with its binding.
    let current = value(call(&lib.c, app::EDITOR, json!({})).await);
    assert_eq!(binding(&current), b2);
    // View 2 closes Two; its old confirmation can't then open anything.
    call(&view2, app::CLOSE, json!({"binding": b2, "force": true}))
        .await
        .unwrap();
    let e = call(
        &view2,
        app::OPEN,
        json!({"id": id1, "force": true, "replace": b2}),
    )
    .await
    .unwrap_err();
    assert!(e.contains("another view changed it"), "{e}");
    assert_eq!(open_doc(&lib.l).await, None);
    task2.abort();
}

/// The normal end of the service (its stdio client goes away) while a started MCP write is
/// paused on the canvas's own server. `Launcher::shutdown`, run as `main` runs it after the
/// service ends, keeps the binding and its lease until that write drained: another launcher on
/// the same library stays busy meanwhile, a browser save queued behind the write and late
/// requests to the old server or launcher are refused, and only then is the canvas free, with
/// the started write saved. In-process: an in-memory stdio pair and the real rmcp protocol, not
/// a spawned executable (the executable's own idle exit is in `tests/app_library.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_service_end_drains_the_started_writer_before_releasing_the_lease() {
    let root = library_root();
    let l = Launcher::with_library(Library::open(root.to_str().unwrap()).unwrap());
    let (client, server) = tokio::io::duplex(1 << 20);
    let (svc, owner) = (l.clone(), l.clone());
    // The production end path: serve until the connection ends, then shut down.
    let service = tokio::spawn(async move {
        let running = svc.serve(server).await.unwrap();
        let _ = running.waiting().await;
        owner.shutdown().await;
    });
    let c = Client {
        peer: ().serve(client).await.unwrap(),
        meta: Default::default(),
    };
    let one = value(call(&c, app::CREATE, json!({"name": "One"})).await);
    let id = one["canvas"]["id"].as_str().unwrap().to_string();
    let dir: PathBuf = serde_json::from_value(one["location"].clone()).unwrap();
    let (port, token) = url(&one);
    let page = crate::read_manifest_checked(&dir).unwrap().pages[0]
        .id
        .clone();
    let page_file = dir.join("pages").join(format!("{page}.json"));
    let before = std::fs::read(&page_file).unwrap();
    let sid = mcp_session(port, &token, &page).await;
    let old = bound_server(&l).await;
    // A started MCP write, paused while it holds the canvas's writer lock.
    let mut snapshot = Point::arm(&dir, "snapshot", true);
    let t = token.clone();
    let running =
        tokio::spawn(async move { http_call(port, &t, &sid, "create_shape", shape()).await });
    snapshot.reached().await;
    // A browser save queued behind it.
    let mut queued = Point::arm(&dir, "browser_queued", false);
    let t = token.clone();
    let body = String::from_utf8(before.clone()).unwrap();
    let extra = format!("If-Match: {}\r\n", crate::etag(&before));
    let path = format!("/api/pages/{page}");
    let browser = tokio::spawn(async move { raw(port, &t, "PUT", &path, &extra, &body).await });
    queued.reached().await;
    // The stdio client goes away: the service ends and shutdown starts retiring the canvas.
    c.peer.cancel().await.unwrap();
    bounded(old.lifecycle.retiring.notified()).await;
    let (lb, b, task_b) = launch(&root).await;
    let e = call(&b, app::OPEN, json!({"id": id})).await.unwrap_err();
    assert!(
        e.contains("another OpenPaperDesign window"),
        "the lease is kept while the started write drains: {e}"
    );
    assert!(
        !service.is_finished(),
        "shutdown waits for the started write"
    );
    assert_eq!(std::fs::read(&page_file).unwrap(), before, "not saved yet");
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    assert_eq!(
        bounded(browser).await.unwrap().0,
        503,
        "the queued browser save is refused"
    );
    bounded(service).await.unwrap();
    // Late work on the old server and on the old launcher is refused; nothing new is made.
    assert_eq!(
        late_get(&old, port, &token).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let e = l
        .app(
            l.library.as_ref().unwrap(),
            app::CREATE,
            json!({"name": "Late"}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("shutting down"), "{e}");
    assert_eq!(
        std::fs::read_dir(root.join("library/v1/canvases"))
            .unwrap()
            .count(),
        1
    );
    assert!(l.state.lock().await.bound.is_none());
    // Only now is the canvas free, with the started write saved.
    let reopened = value(call(&b, app::OPEN, json!({"id": id})).await);
    let (p, t) = url(&reopened);
    assert_eq!(
        page_nodes(p, &t, &page).await,
        1,
        "the started write was saved"
    );
    bound_server(&lb).await.retire().await;
    task_b.abort();
    let _ = std::fs::remove_dir_all(&root);
}

/// Shutdown's owned drain exists before its first wait. Its caller is cancelled while a request
/// in flight still holds the launcher's state (before shutdown could take it); once the state is
/// free, the drain still retires the canvas, keeps the lease until the started write drained,
/// refuses late work and only then frees the canvas. Without the owned task made up front, the
/// cancelled caller would leave nothing to retire it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_shutdown_survives_a_waiter_cancelled_before_it_got_the_state() {
    let lib = library().await;
    let one = value(call(&lib.c, app::CREATE, json!({"name": "One"})).await);
    let id = one["canvas"]["id"].as_str().unwrap().to_string();
    let dir: PathBuf = serde_json::from_value(one["location"].clone()).unwrap();
    let (port, token) = url(&one);
    let page = crate::read_manifest_checked(&dir).unwrap().pages[0]
        .id
        .clone();
    let sid = mcp_session(port, &token, &page).await;
    let old = bound_server(&lib.l).await;
    let mut snapshot = Point::arm(&dir, "snapshot", true);
    let t = token.clone();
    let running =
        tokio::spawn(async move { http_call(port, &t, &sid, "create_shape", shape()).await });
    snapshot.reached().await;
    // A request in flight holds the launcher's state; the shutdown caller is polled (it can't
    // get the state) and then cancelled.
    let held = lib.l.state.clone().lock_owned().await;
    let waiter = tokio::time::timeout(Duration::from_millis(50), lib.l.shutdown()).await;
    assert!(
        waiter.is_err(),
        "the caller was still waiting for the state"
    );
    drop(held);
    // The owned drain runs anyway: it retires the canvas, and keeps the lease meanwhile.
    bounded(old.lifecycle.retiring.notified()).await;
    let (lb, b, task_b) = launch(&lib.root).await;
    let e = call(&b, app::OPEN, json!({"id": id})).await.unwrap_err();
    assert!(e.contains("another OpenPaperDesign window"), "{e}");
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    // Only after the started write drained is the canvas free.
    let mut reopened = None;
    for _ in 0..100 {
        match call(&b, app::OPEN, json!({"id": id})).await {
            Ok(out) => {
                reopened = Some(value(Ok(out)));
                break;
            }
            Err(e) if e.contains("another OpenPaperDesign window") => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    let reopened = reopened.expect("the canvas is released after the drain");
    assert_eq!(
        late_get(&old, port, &token).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let e = lib
        .l
        .app(
            lib.l.library.as_ref().unwrap(),
            app::CREATE,
            json!({"name": "Late"}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("shutting down"), "{e}");
    let (p, t) = url(&reopened);
    assert_eq!(
        page_nodes(p, &t, &page).await,
        1,
        "the started write was saved"
    );
    bound_server(&lb).await.retire().await;
    task_b.abort();
}

// ---- the home view's own editor: its app-only page, revision, save, Ready, release and
// renderer-part tools, through the real launcher and rmcp stream, on canonical library files.
// The renderer bytes here are the inert unit fixture (`FIXTURE_WASM`); nothing renders.

/// A panel tool's refusal (`panel_refusal`): its status, message and current revision.
fn refusal(e: &str) -> Value {
    serde_json::from_str(e).unwrap_or_else(|_| panic!("not a panel refusal: {e}"))
}

/// What the editor would save after one more rectangle on `text`.
fn edited(text: &str) -> String {
    let mut data = paper_rs::project::format::page_from_json(text).unwrap();
    let id = data.doc.alloc_id();
    data.doc.nodes.insert(
        id,
        paper_rs::model::Node::new(
            id,
            paper_rs::model::NodeKind::Rect,
            paper_rs::geom::Rect::new(5.0, 5.0, 40.0, 30.0),
        ),
    );
    data.doc.root.push(id);
    paper_rs::project::format::page_to_json(&data)
}

fn node_count(text: &str) -> usize {
    paper_rs::project::format::page_from_json(text)
        .unwrap()
        .doc
        .nodes
        .len()
}

fn location(v: &Value) -> PathBuf {
    serde_json::from_value(v["location"].clone()).unwrap()
}

/// The page file and the manifest, byte for byte.
fn files(dir: &Path, page: &str) -> (Vec<u8>, Vec<u8>) {
    (
        std::fs::read(dir.join("pages").join(format!("{page}.json"))).unwrap(),
        std::fs::read(dir.join("project.json")).unwrap(),
    )
}

/// The first page read of a view's editor: (page id, mount, revision, text).
async fn first_read(c: &Client, b: &str) -> (String, u64, String, String) {
    let v = value(call(c, app::PAGE, json!({"binding": b})).await);
    assert_eq!(v["binding"], json!(b));
    (
        v["page"]["id"].as_str().unwrap().to_string(),
        v["mount"].as_u64().unwrap(),
        v["revision"].as_str().unwrap().to_string(),
        v["text"].as_str().unwrap().to_string(),
    )
}

/// The owner's workflow with the view's own editor and no link: create, first read, save,
/// Ready, release, close (no confirmation needed), then a fresh launcher (a new process) reopens
/// the canvas with the edit. A link is never given, and nothing needs `ui/open-link`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_create_edit_save_close_and_reopen_through_the_app_tools() {
    let lib = library().await;
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Plan", "panel": true})).await);
    assert_eq!(created["panel"], true);
    assert!(created.get("url").is_none(), "no browser link: {created}");
    let id = created["canvas"]["id"].as_str().unwrap().to_string();
    let b = binding(&created);
    let dir = location(&created);
    assert!(
        !lib.l
            .state
            .lock()
            .await
            .bound
            .as_ref()
            .unwrap()
            .editor_opened,
        "no link was given"
    );
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    let before = files(&dir, &page);
    assert_eq!(
        text.as_bytes(),
        before.0.as_slice(),
        "the canonical page file"
    );
    assert_eq!(rev, crate::etag(&before.0));
    // The editor's own Ready report (its renderer's; this records it for the binding).
    value(call(&lib.c, app::READY, json!({"binding": b, "mount": mount})).await);
    let saved = value(
        call(
            &lib.c,
            app::SAVE,
            json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": edited(&text)}),
        )
        .await,
    );
    assert_eq!(saved["saved"], true);
    let after = files(&dir, &page);
    assert_eq!(saved["revision"], json!(crate::etag(&after.0)));
    assert_eq!(node_count(std::str::from_utf8(&after.0).unwrap()), 1);
    let polled = value(
        call(
            &lib.c,
            app::REVISION,
            json!({"binding": b, "page_id": page, "mount": mount}),
        )
        .await,
    );
    assert_eq!(polled["revision"], saved["revision"]);
    let state = value(call(&lib.c, app::EDITOR, json!({"binding": b, "panel": true})).await);
    assert_eq!(state["editor_loaded"], true);
    assert_eq!(state["browser_saves"], 1);
    assert!(state.get("url").is_none());
    // While an editor is mounted, closing asks first (it may hold edits).
    let e = call(&lib.c, app::CLOSE, json!({"binding": b}))
        .await
        .unwrap_err();
    assert!(e.contains("force: true"), "{e}");
    let released = value(call(&lib.c, app::RELEASE, json!({"binding": b, "mount": mount})).await);
    assert_eq!(released["released"], true);
    // A released editor is refused, with nothing written.
    let e = call(
        &lib.c,
        app::SAVE,
        json!({"binding": b, "page_id": page, "mount": mount, "if_match": saved["revision"], "text": edited(&text)}),
    )
    .await
    .unwrap_err();
    assert_eq!(refusal(&e)["status"], 410, "{e}");
    assert_eq!(files(&dir, &page), after);
    let closed = value(call(&lib.c, app::CLOSE, json!({"binding": b})).await);
    assert_eq!(closed["action"], "closed");
    let e = call(&lib.c, app::PAGE, json!({"binding": b}))
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&e)["status"],
        410,
        "a closed binding reads nothing: {e}"
    );
    // A fresh launcher on the same root.
    let (l2, c2, task2) = launch(&lib.root).await;
    let reopened = value(call(&c2, app::OPEN, json!({"id": id, "panel": true})).await);
    let (page2, _, _, text2) = first_read(&c2, &binding(&reopened)).await;
    assert_eq!(page2, page);
    assert_eq!(node_count(&text2), 1, "the saved edit is there");
    bound_server(&l2).await.retire().await;
    task2.abort();
}

/// One save transaction for both editors: the browser's HTTP save and the panel's save tool
/// refuse each other's stale revisions (409, with the current revision, nothing written), and
/// every commit step that fails is rolled back to byte-identical files; a failed rollback
/// fences both. Single-threaded, so the injected commit faults reach the tool's save.
#[tokio::test(flavor = "current_thread")]
async fn panel_and_browser_saves_share_one_transaction_with_conflicts_and_rollback() {
    let lib = library().await;
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Shared"})).await);
    let (port, token) = url(&created);
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    // The browser saves first: the panel's save on the older revision is a conflict.
    browser_edit(port, &token).await;
    let now = files(&dir, &page);
    let e = call(
        &lib.c,
        app::SAVE,
        json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": edited(&text)}),
    )
    .await
    .unwrap_err();
    let r = refusal(&e);
    assert_eq!(r["status"], 409, "{e}");
    assert_eq!(
        r["revision"],
        json!(crate::etag(&now.0)),
        "the current revision"
    );
    assert!(r["error"].as_str().unwrap().contains("nothing was written"));
    assert_eq!(files(&dir, &page), now);
    // The panel saves on the current revision; then the browser's older one is refused.
    let current = crate::etag(&now.0);
    let mine = edited(std::str::from_utf8(&now.0).unwrap());
    let saved = value(
        call(
            &lib.c,
            app::SAVE,
            json!({"binding": b, "page_id": page, "mount": mount, "if_match": current, "text": mine}),
        )
        .await,
    );
    let (st, head, _) = raw(
        port,
        &token,
        "GET",
        &format!("/api/pages/{page}/revision"),
        "",
        "",
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(
        json!(etag_of(&head)),
        saved["revision"],
        "the same revision"
    );
    let (st, _, _) = raw(
        port,
        &token,
        "PUT",
        &format!("/api/pages/{page}"),
        &format!("If-Match: {current}\r\n"),
        &mine,
    )
    .await;
    assert_eq!(st, 409, "the browser's stale save is refused too");
    // Every failing commit step leaves both files byte-identical.
    for at in 0..6 {
        let before = files(&dir, &page);
        crate::commit::fault::fail_at(&[at]);
        let e = call(
            &lib.c,
            app::SAVE,
            json!({"binding": b, "page_id": page, "mount": mount,
                "if_match": crate::etag(&before.0), "text": edited(std::str::from_utf8(&before.0).unwrap())}),
        )
        .await
        .unwrap_err();
        crate::commit::fault::fail_at(&[]);
        let r = refusal(&e);
        assert_eq!(r["status"], 500, "step {at}: {e}");
        assert!(
            r["error"].as_str().unwrap().contains("nothing was written"),
            "step {at}: {e}"
        );
        assert_eq!(files(&dir, &page), before, "step {at}");
    }
    // A failed rollback fences every later write, the panel's and the browser's.
    let before = files(&dir, &page);
    crate::commit::fault::fail_at(&[3, 4]);
    let e = call(
        &lib.c,
        app::SAVE,
        json!({"binding": b, "page_id": page, "mount": mount,
            "if_match": crate::etag(&before.0), "text": edited(std::str::from_utf8(&before.0).unwrap())}),
    )
    .await
    .unwrap_err();
    crate::commit::fault::fail_at(&[]);
    assert!(
        refusal(&e)["error"]
            .as_str()
            .unwrap()
            .contains("may now be inconsistent"),
        "{e}"
    );
    let fenced = files(&dir, &page);
    let e = call(
        &lib.c,
        app::SAVE,
        json!({"binding": b, "page_id": page, "mount": mount,
            "if_match": crate::etag(&fenced.0), "text": edited(std::str::from_utf8(&fenced.0).unwrap())}),
    )
    .await
    .unwrap_err();
    let r = refusal(&e);
    assert_eq!(r["status"], 503, "{e}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains(&format!("{page}.json")),
        "{e}"
    );
    let (st, _, _) = raw(
        port,
        &token,
        "PUT",
        &format!("/api/pages/{page}"),
        &format!("If-Match: {}\r\n", crate::etag(&fenced.0)),
        &edited(std::str::from_utf8(&fenced.0).unwrap()),
    )
    .await;
    assert_eq!(st, 503);
    assert_eq!(files(&dir, &page), fenced);
    bound_server(&lib.l).await.retire().await;
}

/// The first read takes the manifest's active page (not the first one), and later calls stay
/// on that exact page even when the active page changes; a save changes only that page and the
/// manifest's `modified`, keeping the other page, the page list, names, order and active page.
/// A page removed from the manifest is refused, with nothing written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_pins_its_page_and_preserves_the_rest_of_a_multi_page_canvas() {
    let lib = library().await;
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Pages", "panel": true})).await);
    let b = binding(&created);
    let dir = location(&created);
    let mut m = crate::read_manifest_checked(&dir).unwrap();
    let first = m.pages[0].id.clone();
    let second = "second-page".to_string();
    m.pages.push(paper_rs::project::PageRef {
        id: second.clone(),
        name: "Second".into(),
    });
    m.active_page = Some(second.clone());
    std::fs::write(dir.join("project.json"), m.to_json()).unwrap();
    let blank = paper_rs::project::PageData {
        id: second.clone(),
        name: "Second".into(),
        doc: paper_rs::model::Document {
            name: "Second".into(),
            ..Default::default()
        },
        camera: None,
    };
    std::fs::write(
        dir.join("pages").join(format!("{second}.json")),
        paper_rs::project::format::page_to_json(&blank),
    )
    .unwrap();
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    assert_eq!(page, second, "the active page, not the first");
    // The active page changes elsewhere; this editor's reads stay on its page.
    let mut m2 = crate::read_manifest_checked(&dir).unwrap();
    m2.active_page = Some(first.clone());
    std::fs::write(dir.join("project.json"), m2.to_json()).unwrap();
    let again = value(
        call(
            &lib.c,
            app::PAGE,
            json!({"binding": b, "page_id": page, "mount": mount}),
        )
        .await,
    );
    assert_eq!(again["page"]["id"], json!(second));
    assert_eq!(again["mount"], json!(mount));
    let other_before = std::fs::read(dir.join("pages").join(format!("{first}.json"))).unwrap();
    let manifest_before = crate::read_manifest_checked(&dir).unwrap();
    value(
        call(
            &lib.c,
            app::SAVE,
            json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": edited(&text)}),
        )
        .await,
    );
    assert_eq!(
        std::fs::read(dir.join("pages").join(format!("{first}.json"))).unwrap(),
        other_before,
        "the other page is untouched"
    );
    let manifest_after = crate::read_manifest_checked(&dir).unwrap();
    assert_eq!(
        serde_json::to_value(
            manifest_after
                .pages
                .iter()
                .map(|p| (&p.id, &p.name))
                .collect::<Vec<_>>()
        )
        .unwrap(),
        serde_json::to_value(
            manifest_before
                .pages
                .iter()
                .map(|p| (&p.id, &p.name))
                .collect::<Vec<_>>()
        )
        .unwrap(),
        "page list, names and order"
    );
    assert_eq!(
        manifest_after.active_page,
        Some(first.clone()),
        "the active page as it is on disk"
    );
    assert_eq!(manifest_after.id, manifest_before.id);
    assert_eq!(manifest_after.name, manifest_before.name);
    // A malformed or unknown page id is refused before anything is read.
    let e = call(
        &lib.c,
        app::PAGE,
        json!({"binding": b, "page_id": "../project", "mount": mount}),
    )
    .await
    .unwrap_err();
    assert_eq!(refusal(&e)["status"], 400, "{e}");
    // The page is removed from the manifest: every call for it is refused, nothing written.
    let mut m3 = crate::read_manifest_checked(&dir).unwrap();
    m3.pages.retain(|p| p.id != second);
    m3.active_page = Some(first.clone());
    std::fs::write(dir.join("project.json"), m3.to_json()).unwrap();
    let before = files(&dir, &second);
    for (tool, args) in [
        (
            app::PAGE,
            json!({"binding": b, "page_id": second, "mount": mount}),
        ),
        (
            app::REVISION,
            json!({"binding": b, "page_id": second, "mount": mount}),
        ),
        (
            app::SAVE,
            json!({"binding": b, "page_id": second, "mount": mount, "if_match": crate::etag(&before.0), "text": edited(&text)}),
        ),
    ] {
        let e = call(&lib.c, tool, args).await.unwrap_err();
        assert_eq!(refusal(&e)["status"], 404, "{tool}: {e}");
    }
    assert_eq!(files(&dir, &second), before);
    bound_server(&lib.l).await.retire().await;
}

/// The renderer parts: fixed packaged bytes only, numbered from 0, strictly whole numbers, each
/// naming the same identity, size and count; anything else is refused and nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_renderer_parts_are_bounded_strict_and_complete() {
    let lib = library().await;
    let identity = crate::etag(&super::FIXTURE_WASM);
    let mut joined = String::new();
    for part in 0..3u64 {
        let v = value(call(&lib.c, app::ASSET, json!({"part": part})).await);
        let start = part as usize * app::PART_BYTES;
        let end = (start + app::PART_BYTES).min(super::FIXTURE_WASM.len());
        assert_eq!(v["asset"], "openpaperdesign_web_bg.wasm");
        assert_eq!(v["identity"], json!(identity));
        assert_eq!(v["bytes"], json!(super::FIXTURE_WASM.len()));
        assert_eq!(v["parts"], 3);
        assert_eq!(v["part"], json!(part));
        assert_eq!(v["offset"], json!(start));
        assert_eq!(v["part_bytes"], json!(end - start));
        assert_eq!(v["data"], json!(base64(&super::FIXTURE_WASM[start..end])));
        joined.push_str(v["data"].as_str().unwrap());
    }
    assert!(!joined.is_empty());
    for args in [
        json!({"part": 3}),
        json!({"part": 64}),
        json!({"part": -1}),
        json!({"part": 1.5}),
        json!({"part": 1.0}),
        json!({"part": "1"}),
        json!({"part": null}),
        json!({}),
        json!({"part": 0, "path": "/etc/passwd"}),
        json!({"part": 0, "binding": "0123456789abcdef.1"}),
    ] {
        let e = call(&lib.c, app::ASSET, args.clone()).await.unwrap_err();
        assert!(
            e.contains("unexpected argument")
                || e.contains("no renderer part")
                || e.contains("is required"),
            "{args}: {e}"
        );
    }
    // The pure part function: its bounds.
    assert!(app::asset_part(&[], "x", 0).is_err());
    assert!(app::asset_part(&vec![0; app::MAX_ASSET_BYTES + 1], "x", 0).is_err());
    let exact = vec![1u8; app::PART_BYTES * 2];
    assert_eq!(
        app::asset_part(&exact, "x", 1).unwrap()["part_bytes"],
        json!(app::PART_BYTES)
    );
    assert!(
        app::asset_part(&exact, "x", 2).is_err(),
        "no empty last part"
    );
    // Nothing was bound, locked or created for any of it.
    assert!(lib.l.state.lock().await.bound.is_none());
    assert!(!lib.root.join("library").exists());
}

/// Page messages are bounded for the panel separately from the 64 MiB page limit, counting
/// JSON escaping: a page too large to send isn't opened here, and a save too large isn't sent
/// on; neither changes a file. Bad, missing or extra arguments are refused before anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_messages_are_bounded_and_arguments_strict() {
    let lib = library().await;
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Big", "panel": true})).await);
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    // Mostly quotes: under the limit as bytes, over it once escaped.
    let quoted = "\"".repeat(app::MAX_PANEL_MESSAGE / 2 + 16);
    assert!(quoted.len() < app::MAX_PANEL_MESSAGE);
    let before = files(&dir, &page);
    let e = call(
        &lib.c,
        app::SAVE,
        json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": quoted}),
    )
    .await
    .unwrap_err();
    assert_eq!(refusal(&e)["status"], 413, "{e}");
    assert_eq!(files(&dir, &page), before, "nothing written");
    // A page file too large for the panel: refused, unread by the editor, untouched.
    let big = format!("{}{}", text, " ".repeat(app::MAX_PANEL_MESSAGE));
    std::fs::write(dir.join("pages").join(format!("{page}.json")), &big).unwrap();
    let e = call(
        &lib.c,
        app::PAGE,
        json!({"binding": b, "page_id": page, "mount": mount}),
    )
    .await
    .unwrap_err();
    assert_eq!(refusal(&e)["status"], 413, "{e}");
    assert!(
        refusal(&e)["error"]
            .as_str()
            .unwrap()
            .contains("browser editor")
    );
    assert_eq!(
        std::fs::read(dir.join("pages").join(format!("{page}.json"))).unwrap(),
        big.as_bytes()
    );
    for (tool, args) in [
        (app::PAGE, json!({})),
        (app::PAGE, json!({"binding": b, "page_id": page})),
        (app::PAGE, json!({"binding": b, "mount": mount})),
        (app::PAGE, json!({"binding": b, "extra": 1})),
        (app::PAGE, json!({"binding": "not-a-binding"})),
        (
            app::REVISION,
            json!({"binding": b, "page_id": page, "mount": "1"}),
        ),
        (
            app::REVISION,
            json!({"binding": b, "page_id": page, "mount": 1.0}),
        ),
        (app::REVISION, json!({"binding": b, "page_id": page})),
        (
            app::SAVE,
            json!({"binding": b, "page_id": page, "mount": mount, "text": "{}"}),
        ),
        (app::READY, json!({"binding": b, "mount": -1})),
        (
            app::RELEASE,
            json!({"binding": b, "mount": mount, "force": true}),
        ),
    ] {
        let e = call(&lib.c, tool, args.clone()).await.unwrap_err();
        assert_eq!(refusal(&e)["status"], 400, "{tool} {args}: {e}");
    }
    bound_server(&lib.l).await.retire().await;
}

/// Stale views: another binding's, a released mount, a canvas whose lease broke, a process that
/// shut down: each is refused before anything is read or written. A workspace process refuses
/// the new tool names like the other library actions, without admission or state change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_refuses_stale_bindings_broken_leases_shutdown_and_workspace_processes() {
    let lib = library().await;
    let one = value(call(&lib.c, app::CREATE, json!({"name": "One", "panel": true})).await);
    let b1 = binding(&one);
    let (_, m1, _, _) = first_read(&lib.c, &b1).await;
    value(call(&lib.c, app::RELEASE, json!({"binding": b1, "mount": m1})).await);
    // Switching needs no confirmation once no editor is mounted and no link was given.
    let two = value(call(&lib.c, app::CREATE, json!({"name": "Two", "panel": true})).await);
    let b2 = binding(&two);
    let e = call(&lib.c, app::PAGE, json!({"binding": b1}))
        .await
        .unwrap_err();
    assert_eq!(refusal(&e)["status"], 410, "the old binding: {e}");
    let (page, m2, _, _) = first_read(&lib.c, &b2).await;
    // With an editor mounted, a switch asks first.
    let e = call(&lib.c, app::CREATE, json!({"name": "Three", "panel": true}))
        .await
        .unwrap_err();
    assert!(e.contains("force: true") && e.contains(&b2), "{e}");
    // The lease file is replaced: the canvas is fenced and closed, nothing read.
    let dir = location(&two);
    let id2 = two["canvas"]["id"].as_str().unwrap();
    let lock = lib.root.join(format!("library/v1/locks/{id2}.lock"));
    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&lock, b"").unwrap();
    let e = call(
        &lib.c,
        app::REVISION,
        json!({"binding": b2, "page_id": page, "mount": m2}),
    )
    .await
    .unwrap_err();
    assert_eq!(refusal(&e)["status"], 410, "{e}");
    assert!(
        lib.l.state.lock().await.bound.is_none(),
        "the canvas was closed"
    );
    assert!(dir.join("project.json").is_file(), "its files stay");
    // After shutdown nothing is answered from a binding.
    lib.l.shutdown().await;
    let e = call(&lib.c, app::PAGE, json!({"binding": b2}))
        .await
        .unwrap_err();
    assert_eq!(refusal(&e)["status"], 410, "{e}");
    // A workspace process.
    let l = Launcher::new(None);
    let (c, task) = connect_launcher(l.clone(), Default::default()).await;
    for tool in app::PANEL_TOOLS {
        let e = call(&c, tool, json!({"binding": b2, "part": 0}))
            .await
            .unwrap_err();
        assert!(
            e.contains("wasn't started for the app library"),
            "{tool}: {e}"
        );
    }
    let st = l.state.lock().await;
    assert!(st.invalidated.is_none() && st.workspace.is_none() && st.bound.is_none());
    drop(st);
    task.abort();
}

/// The writer fence: a save that can't start in time, or whose caller stopped waiting before it
/// started (the tool's request cancelled), never runs; a started save finishes its commit even
/// while Close retires the binding, and Close waits for it; afterwards the retained server
/// refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_saves_queue_boundedly_and_a_started_save_drains_through_close() {
    let lib = library().await;
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Fence", "panel": true})).await);
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    let server = bound_server(&lib.l).await;
    let before = files(&dir, &page);
    let body = edited(&text);
    // Expired while queued.
    let held = server.lifecycle.write.clone().lock_owned().await;
    let e = server
        .save_page_within(Duration::from_millis(50), None, &page, &rev, &body)
        .await
        .expect_err("refused");
    assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.msg.contains("could not start in time"), "{}", e.msg);
    // The tool's request cancelled while queued.
    let args =
        json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": body});
    let e = lib
        .l
        .panel_call(app::SAVE, &args, async {})
        .await
        .unwrap_err();
    assert!(
        refusal(&e)["error"]
            .as_str()
            .unwrap()
            .contains("cancelled before it started"),
        "{e}"
    );
    drop(held);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(files(&dir, &page), before, "neither queued save ran");
    // A started save, paused inside its transaction, while Close retires the binding.
    let mut started = Point::arm(&dir, "save_started", true);
    let c = lib.c.clone();
    let saving = tokio::spawn(async move { call(&c, app::SAVE, args).await });
    started.reached().await;
    // Leaving the editor: its mount is revoked at once; the release itself is acknowledged
    // only after the started save (so it is spawned, not awaited, here).
    let c = lib.c.clone();
    let b1 = b.clone();
    let releasing = tokio::spawn(async move {
        call(&c, app::RELEASE, json!({"binding": b1, "mount": mount})).await
    });
    mount_revoked(&lib.l, mount).await;
    let c = lib.c.clone();
    let b2 = b.clone();
    let closing = tokio::spawn(async move { call(&c, app::CLOSE, json!({"binding": b2})).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !releasing.is_finished(),
        "the release waits for the started save"
    );
    assert!(!closing.is_finished(), "Close waits for the started save");
    started.resume();
    let saved = value(bounded(saving).await.unwrap());
    assert_eq!(
        saved["saved"], true,
        "the started save finished and says so"
    );
    assert_eq!(value(bounded(releasing).await.unwrap())["released"], true);
    value(bounded(closing).await.unwrap());
    let after = files(&dir, &page);
    assert_eq!(saved["revision"], json!(crate::etag(&after.0)));
    assert_eq!(node_count(std::str::from_utf8(&after.0).unwrap()), 1);
    // The retained server is retired: a late save is refused, unwritten.
    let e = server
        .save_page(
            &page,
            &crate::etag(&after.0),
            &edited(std::str::from_utf8(&after.0).unwrap()),
        )
        .await
        .expect_err("refused");
    assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.page(Some(&page)).is_err(), "and reads nothing");
    assert_eq!(files(&dir, &page), after);
}

/// Wait (bounded) until `mount` is no longer one of the open binding's mounts.
async fn mount_revoked(l: &Launcher, mount: u64) {
    for _ in 0..1000 {
        let gone = !l
            .state
            .lock()
            .await
            .bound
            .as_ref()
            .is_some_and(|b| b.mounts.contains_key(&mount));
        if gone {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("mount {mount} was not revoked");
}

/// R8 F1, adapted from the independent reviewer's diagnostic (Codex review work): a release
/// revokes its mount at once. A save of that mount admitted earlier, still waiting for the
/// writer fence, then never runs (no page or manifest change), and the release is acknowledged
/// only once the fence is free. The binding itself stays open: Back isn't Close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_release_revokes_a_queued_save_before_it_can_start() {
    let lib = library().await;
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Queue", "panel": true})).await);
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    let server = bound_server(&lib.l).await;
    let before = files(&dir, &page);
    let held = server.lifecycle.write.clone().lock_owned().await;
    let mut queued = Point::arm(&dir, "browser_queued", false);
    let c = lib.c.clone();
    let body = edited(&text);
    let args =
        json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": body});
    let saving = tokio::spawn(async move { call(&c, app::SAVE, args).await });
    queued.reached().await;
    let c = lib.c.clone();
    let b1 = b.clone();
    let releasing = tokio::spawn(async move {
        call(&c, app::RELEASE, json!({"binding": b1, "mount": mount})).await
    });
    mount_revoked(&lib.l, mount).await;
    assert!(
        !releasing.is_finished(),
        "acknowledged only once the writer fence is free"
    );
    drop(held);
    let result = bounded(saving).await.unwrap();
    assert_eq!(value(bounded(releasing).await.unwrap())["released"], true);
    let e = result.expect_err("a released queued save must never execute");
    assert_eq!(refusal(&e)["status"], 410, "{e}");
    assert_eq!(
        files(&dir, &page),
        before,
        "neither page nor manifest changed"
    );
    assert!(
        lib.l.state.lock().await.bound.is_some(),
        "the canvas stays open"
    );
    server.retire().await;
}

/// R8 F2, adapted from the independent reviewer's diagnostic (Codex review work): a save whose
/// request is cancelled while it waits for admission (the launcher's state) is refused and
/// never starts, even though the writer fence is free once the state is; an already
/// cancelled save never takes a free fence either. Not cancelled, the same save is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_cancelled_save_never_starts_from_admission_or_a_free_fence() {
    let lib = library().await;
    let created = value(
        call(
            &lib.c,
            app::CREATE,
            json!({"name": "Cancel", "panel": true}),
        )
        .await,
    );
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    let before = files(&dir, &page);
    let body = edited(&text);
    let args =
        json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": body});
    // Cancelled while it waits for the state.
    let held = lib.l.state.clone().lock_owned().await;
    let l = lib.l.clone();
    let a = args.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let saving = tokio::spawn(async move {
        l.panel_call(app::SAVE, &a, async move {
            let _ = rx.await;
        })
        .await
    });
    tokio::task::yield_now().await;
    tx.send(()).unwrap();
    let e = bounded(saving).await.unwrap().unwrap_err();
    drop(held);
    assert!(e.contains("cancelled before it started"), "{e}");
    assert_eq!(files(&dir, &page), before);
    // Already cancelled, with the state and the writer fence both free.
    let e = lib
        .l
        .panel_call(app::SAVE, &args, async {})
        .await
        .unwrap_err();
    assert!(e.contains("cancelled before it started"), "{e}");
    assert_eq!(
        files(&dir, &page),
        before,
        "no fence taken, nothing written"
    );
    // The control: the same save, not cancelled, is written.
    let saved = value(
        lib.l
            .panel_call(app::SAVE, &args, std::future::pending::<()>())
            .await,
    );
    assert_eq!(saved["saved"], true);
    assert_ne!(files(&dir, &page), before);
    bound_server(&lib.l).await.retire().await;
}

/// A page file whose node name is `quotes` quote characters (each escaped once in the file).
fn quote_heavy_page(text: &str, quotes: usize) -> String {
    let mut data = paper_rs::project::format::page_from_json(&edited(text)).unwrap();
    let node = data.doc.root[0];
    data.doc.nodes.get_mut(&node).unwrap().name = "\"".repeat(quotes);
    let body = paper_rs::project::format::page_to_json(&data);
    assert!(paper_rs::project::format::page_from_json(&body).is_ok());
    body
}

/// R8 F3, adapted from the independent reviewer's diagnostic (Codex review work): a page read
/// is bounded as the whole tool response it would be (the pretty JSON text inside the tool
/// result inside the JSON-RPC response, escaped at each level, plus the host allowance), not
/// only as its inner JSON. A quote-heavy page whose inner message fits is refused, before a
/// mount is registered; a smaller one is answered within the limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_page_reads_are_bounded_as_the_whole_tool_response() {
    let lib = library().await;
    let created = value(call(&lib.c, app::CREATE, json!({"name": "Big", "panel": true})).await);
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, _, text) = first_read(&lib.c, &b).await;
    let path = dir.join("pages").join(format!("{page}.json"));
    let big = quote_heavy_page(&text, 700_000);
    let inner = json!({"text": big}).to_string().len();
    assert!(
        inner < app::MAX_PANEL_MESSAGE,
        "the inner message alone fits"
    );
    std::fs::write(&path, &big).unwrap();
    let before = files(&dir, &page);
    let mounts = || async {
        lib.l
            .state
            .lock()
            .await
            .bound
            .as_ref()
            .map_or(0, |b| b.mounts.len())
    };
    let e = call(
        &lib.c,
        app::PAGE,
        json!({"binding": b, "page_id": page, "mount": mount}),
    )
    .await
    .unwrap_err();
    assert_eq!(refusal(&e)["status"], 413, "{e}");
    let registered = mounts().await;
    let e = call(&lib.c, app::PAGE, json!({"binding": b}))
        .await
        .unwrap_err();
    assert_eq!(refusal(&e)["status"], 413, "a first read too: {e}");
    assert_eq!(mounts().await, registered, "no mount registered for it");
    assert_eq!(files(&dir, &page), before, "files unchanged");
    // A smaller quote-heavy page: answered, and its whole response is within the limit.
    std::fs::write(&path, quote_heavy_page(&text, 200_000)).unwrap();
    let mut params = CallToolRequestParams::new(app::PAGE);
    params.arguments = json!({"binding": b, "page_id": page, "mount": mount})
        .as_object()
        .cloned();
    let response = bounded(lib.c.peer.call_tool(params)).await.unwrap();
    assert_ne!(response.is_error, Some(true));
    let envelope = json!({"jsonrpc": "2.0", "id": 1, "result": response});
    let wire = serde_json::to_vec(&envelope).unwrap();
    assert!(wire.len() <= app::MAX_PANEL_MESSAGE, "{}", wire.len());
    bound_server(&lib.l).await.retire().await;
}

/// R8 F3 for requests: a save is bounded as the whole `tools/call` request it came in (the
/// JSON-RPC request with its arguments, plus the host allowance), not only its arguments: an
/// escaping-heavy save and a plain one whose arguments alone fit are refused, unwritten; one
/// within the limit is admitted (and then refused only as not a page).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_saves_are_bounded_as_the_whole_tool_request() {
    let lib = library().await;
    let created = value(
        call(
            &lib.c,
            app::CREATE,
            json!({"name": "Request", "panel": true}),
        )
        .await,
    );
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, rev, _) = first_read(&lib.c, &b).await;
    let before = files(&dir, &page);
    let args = |text: &str| json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev, "text": text});
    for (what, text) in [
        ("quotes", "\"".repeat(app::MAX_PANEL_MESSAGE / 2 - 4096)),
        ("plain", "x".repeat(app::MAX_PANEL_MESSAGE - 1024)),
    ] {
        let a = args(&text);
        assert!(
            serde_json::to_string(&a).unwrap().len() < app::MAX_PANEL_MESSAGE,
            "{what}: the arguments alone fit"
        );
        let wrapped = app::request_bytes(app::SAVE, &a);
        assert!(wrapped > app::MAX_PANEL_MESSAGE, "{what}: {wrapped}");
        let e = call(&lib.c, app::SAVE, a).await.unwrap_err();
        assert_eq!(refusal(&e)["status"], 413, "{what}: {e}");
        assert_eq!(files(&dir, &page), before, "{what}: nothing written");
    }
    let fits = "x".repeat(app::MAX_PANEL_MESSAGE - 64 * 1024);
    let e = call(&lib.c, app::SAVE, args(&fits)).await.unwrap_err();
    assert_eq!(refusal(&e)["status"], 422, "admitted, then not a page: {e}");
    assert_eq!(files(&dir, &page), before);
    bound_server(&lib.l).await.retire().await;
}

/// Stand-in parts for `panel_module` (inert text, not the real glue or editor).
static MODULE_PARTS: [(&str, &[u8]); 6] = [
    (
        "openpaperdesign_web.js",
        b"export function start() {}\nexport { start as default };",
    ),
    ("session.js", b"export class Session {}"),
    ("page.js", b"export function frameStep() {}"),
    ("editor.js", b"export function opdMountEditor() {}"),
    (
        "panel.js",
        b"export function opdCreatePanel(o) { return o; }",
    ),
    ("openpaperdesign_web_bg.wasm", b"\0asm"),
];

fn module_parts_with(name: &str, text: &str) -> crate::EmbeddedBundle {
    let bytes: &'static [u8] = Box::leak(text.as_bytes().to_vec().into_boxed_slice());
    let files: Vec<(&'static str, &'static [u8])> = MODULE_PARTS
        .iter()
        .map(|&(f, b)| (f, if f == name { bytes } else { b }))
        .collect();
    Box::leak(files.into_boxed_slice())
}

/// The in-panel module is made only of the fixed packaged parts, in order, after the renderer
/// identity, and only when it is safe inside the view's script element; otherwise the view
/// keeps its null editor slot (and its primary actions are refused, see below).
#[test]
fn panel_module_is_fixed_ordered_and_safe_to_embed() {
    let m = panel_module(&MODULE_PARTS, "\"0011\"").unwrap();
    assert!(
        m.contains("const OPD_WASM_IDENTITY = \"\\\"0011\\\"\";"),
        "{m}"
    );
    let at = |s: &str| m.find(s).unwrap();
    assert!(at("OPD_WASM_IDENTITY") < at("export function start"));
    assert!(at("export function start") < at("export class Session"));
    assert!(at("export class Session") < at("export function frameStep"));
    assert!(at("export function frameStep") < at("export function opdMountEditor"));
    assert!(at("export function opdMountEditor") < at("function opdCreatePanel("));
    for (name, text, want) in [
        ("session.js", "const s = '</SCRIPT>';", "</script"),
        ("page.js", "// <!-- x", "<!--"),
        ("editor.js", "const t = '<script>';", "<script"),
        ("editor.js", "import { a } from './x.js';", "import"),
        ("panel.js", "  import('./x.js');", "import"),
        ("openpaperdesign_web.js", "", "isn't in this build"),
        ("panel.js", "export function other() {}", "opdCreatePanel"),
    ] {
        let e = panel_module(module_parts_with(name, text), "\"0011\"").unwrap_err();
        assert!(e.contains(want), "{name}: {e}");
    }
    // This test build's inert fixture parts assemble: the library view gets them in its slot,
    // after this build's renderer identity; a workspace view never does.
    assert!(panel_editor().is_ok());
    let library = home_html_for(true);
    assert!(!library.contains(PANEL_SLOT));
    let inert = "export function opdCreatePanel() { return null; }";
    assert!(library.contains(inert));
    let identity = format!(
        "const OPD_WASM_IDENTITY = {};",
        serde_json::to_string(&crate::etag(&super::FIXTURE_WASM)).unwrap()
    );
    assert!(library.contains(&identity));
    assert!(home_html_for(false).contains(PANEL_SLOT));
    assert_eq!(library.matches("<script").count(), 1, "one script element");
    assert!(library.contains("<script type=\"module\">"));
    assert_eq!(
        HOME_URI, "ui://openpaperdesign/home-3.html",
        "versioned for the browser-first view code"
    );
}

/// A request for the view's own editor (`panel: true`) from a build that can't serve it is
/// refused before anything happens: no canvas file, lease, binding, switch, mount or link.
/// The unavailable build is simulated by a one-shot test fault; plain (link) requests from
/// other callers keep working as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panel_requests_are_refused_before_any_effect_when_the_build_has_no_panel() {
    let lib = library().await;
    let unavailable = || Point::arm(lib.root.as_path(), "panel_unavailable", false);
    let refused = |e: String| {
        assert!(
            e.contains("in-panel editor isn't available")
                && e.contains("nothing was created, imported, opened or changed"),
            "{e}"
        );
    };
    let fault = unavailable();
    refused(
        call(&lib.c, app::CREATE, json!({"name": "None", "panel": true}))
            .await
            .unwrap_err(),
    );
    assert!(
        !lib.root.join("library").exists(),
        "no canvas, lease or folder"
    );
    drop(fault);
    // An existing canvas, made by a plain (link) request and closed again.
    let made = value(call(&lib.c, app::CREATE, json!({"name": "Made"})).await);
    let id = made["canvas"]["id"].as_str().unwrap().to_string();
    call(
        &lib.c,
        app::CLOSE,
        json!({"binding": binding(&made), "force": true}),
    )
    .await
    .unwrap();
    let canvases = || {
        std::fs::read_dir(lib.root.join("library/v1/canvases"))
            .unwrap()
            .count()
    };
    let count = canvases();
    let fault = unavailable();
    refused(
        call(&lib.c, app::OPEN, json!({"id": id, "panel": true}))
            .await
            .unwrap_err(),
    );
    drop(fault);
    assert!(lib.l.state.lock().await.bound.is_none(), "nothing opened");
    let fault = unavailable();
    refused(
        call(&lib.c, app::IMPORT, json!({"payload": "{}", "panel": true}))
            .await
            .unwrap_err(),
    );
    drop(fault);
    assert_eq!(canvases(), count, "nothing imported");
    let fault = unavailable();
    refused(
        call(&lib.c, app::EDITOR, json!({"panel": true}))
            .await
            .unwrap_err(),
    );
    drop(fault);
    // Opened for the panel (the build serves it again), then a switch request is refused
    // before it is weighed, and the open canvas, its mounts and link state stay as they were.
    let opened = value(call(&lib.c, app::OPEN, json!({"id": id, "panel": true})).await);
    let b = binding(&opened);
    let fault = unavailable();
    refused(
        call(
            &lib.c,
            app::CREATE,
            json!({"name": "Other", "panel": true, "force": true, "replace": b}),
        )
        .await
        .unwrap_err(),
    );
    drop(fault);
    {
        let st = lib.l.state.lock().await;
        let bound = st.bound.as_ref().unwrap();
        assert_eq!(
            bound.binding.as_deref(),
            Some(b.as_str()),
            "still the same canvas"
        );
        assert!(bound.mounts.is_empty() && !bound.editor_opened);
    }
    assert_eq!(canvases(), count);
    bound_server(&lib.l).await.retire().await;
}

// ---- the agent on the library canvas (`launcher_library_agent.rs`): model-visible library
// controls and upstream's own design tools, on the canvas the person's view shows. The engine
// here is upstream paper-rs (`exec::call` on the server's host); only the unit-test bundle is
// a stand-in, and nothing renders.

/// Every text block of a tool result, joined.
fn out_text(r: ToolResult) -> String {
    r.unwrap()
        .into_iter()
        .filter_map(|o| match o {
            Out::Text(t) => Some(t),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The saved node named `name` on `page` of the canvas in `dir` (upstream's page file format),
/// with its tool id (`<first six characters of the page id>:<n>`).
fn saved_node(dir: &Path, page: &str, name: &str) -> (String, paper_rs::model::Node) {
    let path = dir.join("pages").join(format!("{page}.json"));
    let text = std::fs::read_to_string(path).unwrap();
    let data = paper_rs::project::format::page_from_json(&text).unwrap();
    let node = data
        .doc
        .nodes
        .values()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("no saved node {name}"))
        .clone();
    (format!("{}:{}", &page[..6], node.id.0), node)
}

/// The catalogue a model sees from an app-library process: the library controls (no app-only
/// visibility), and every upstream design tool the adapter offers, under its own name,
/// description and schema, with `binding` required on top. Export, file creation and the
/// project routes aren't there; the view's own tools stay app-only. A workspace process's
/// catalogue is unchanged, and it refuses the library controls without any state change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_catalogue_shows_library_controls_and_binding_guarded_upstream_tools() {
    let lib = library().await;
    let tools: Vec<Value> = lib
        .c
        .peer
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| serde_json::to_value(t).unwrap())
        .collect();
    let tool = |n: &str| tools.iter().find(|t| t["name"] == n).cloned();
    for n in app::AGENT_TOOLS {
        let t = tool(n).unwrap_or_else(|| panic!("{n} is listed"));
        assert!(
            t["_meta"]["ui"]["visibility"].is_null(),
            "{n} is model-visible"
        );
    }
    for upstream in crate::agent::offered_tools() {
        let name = upstream.name.to_string();
        let original = serde_json::to_value(upstream).unwrap();
        let t = tool(&name).unwrap_or_else(|| panic!("{name} is listed"));
        let schema = &t["inputSchema"];
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("binding")),
            "{name}"
        );
        assert!(schema["properties"]["binding"].is_object(), "{name}");
        for (k, v) in original["inputSchema"]["properties"].as_object().unwrap() {
            assert_eq!(
                &schema["properties"][k], v,
                "{name}.{k}: upstream's own schema"
            );
        }
        let desc = original["description"].as_str().unwrap();
        assert!(
            t["description"].as_str().unwrap().starts_with(desc),
            "{name}"
        );
    }
    for gone in [
        "export",
        "export_combined_pdf",
        "create_file",
        "bind_project",
        "create_project",
        "open_editor",
    ] {
        assert!(tool(gone).is_none(), "{gone} isn't offered here");
    }
    for n in app::TOOLS {
        assert_eq!(
            tool(n).unwrap()["_meta"]["ui"]["visibility"],
            json!(["app"]),
            "{n}"
        );
    }
    // A workspace process.
    let l = Launcher::new(None);
    let (c, task) = connect_launcher(l.clone(), Default::default()).await;
    let ws: Vec<Value> = c
        .peer
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| serde_json::to_value(t).unwrap())
        .collect();
    assert!(
        ws.iter()
            .all(|t| !app::AGENT_TOOLS.contains(&t["name"].as_str().unwrap()))
    );
    let summary = ws.iter().find(|t| t["name"] == "get_tree_summary").unwrap();
    assert!(summary["inputSchema"]["properties"]["binding"].is_null());
    for n in app::AGENT_TOOLS {
        let e = call(&c, n, json!({})).await.unwrap_err();
        assert!(e.contains("wasn't started for the app library"), "{n}: {e}");
    }
    let st = l.state.lock().await;
    assert!(st.bound.is_none() && st.workspace.is_none() && st.invalidated.is_none());
    drop(st);
    task.abort();
}

/// The agent's whole path on an app-owned canvas, with upstream's own commands: create it
/// (no link, no token), see it as the open canvas, read the page, draw a rectangle, move and
/// recolour it, read the results back, close, and reopen it in a fresh process with the
/// drawing there. A change before the page is read, a missing or stale binding and a forged
/// session field are refused with nothing written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_creates_reads_draws_moves_recolours_and_a_fresh_process_reopens_it() {
    let lib = library().await;
    let created = value(call(&lib.c, app::AGENT_CREATE, json!({"name": "Agent board"})).await);
    assert!(created.get("url").is_none(), "no link or token: {created}");
    let b = binding(&created);
    let id = created["canvas"]["id"].as_str().unwrap().to_string();
    let page = created["pages"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(created["active_page"], json!(page));
    let dir = lib.root.join("library/v1/canvases").join(&id);
    let current = value(call(&lib.c, app::AGENT_CURRENT, json!({})).await);
    assert_eq!(binding(&current), b);
    assert_eq!(current["canvas"]["id"], json!(id));
    let shape = json!({"binding": b, "pageId": page, "type": "rectangle", "name": "Agent box",
        "x": 10, "y": 20, "width": 80, "height": 40, "styles": {"backgroundColor": "#ff0000"}});
    // Neither a library result nor the current view counts as reading the page.
    let before = files(&dir, &page);
    let e = call(&lib.c, "create_shape", shape.clone())
        .await
        .unwrap_err();
    assert!(e.contains("conflict"), "{e}");
    assert_eq!(files(&dir, &page), before, "nothing written");
    let mut unbound = shape.clone();
    unbound.as_object_mut().unwrap().remove("binding");
    let e = call(&lib.c, "create_shape", unbound).await.unwrap_err();
    assert!(e.contains("Name the canvas you mean"), "{e}");
    let stale = json!({"binding": "0123456789abcdef.99999", "nodeId": "root"});
    let e = call(&lib.c, "get_tree_summary", stale).await.unwrap_err();
    assert!(e.contains("isn't the canvas open"), "{e}");
    let forged = json!({"binding": b, "nodeId": "root", "_openpaperdesignSession": 1});
    let e = call(&lib.c, "get_tree_summary", forged).await.unwrap_err();
    assert!(e.contains("unexpected argument"), "{e}");
    assert_eq!(files(&dir, &page), before);
    // Read the page, then draw.
    let read = json!({"binding": b, "nodeId": "root", "pageId": page});
    out_text(call(&lib.c, "get_tree_summary", read).await);
    call(&lib.c, "create_shape", shape).await.unwrap();
    let (nid, node) = saved_node(&dir, &page, "Agent box");
    assert_eq!(
        (node.rect.x, node.rect.y, node.rect.w, node.rect.h),
        (10.0, 20.0, 80.0, 40.0)
    );
    // Move it and recolour it (its own committed change counts as read).
    let moved = json!({"binding": b, "updates": [{"nodeIds": [nid],
        "styles": {"left": "120px", "top": "60px"}}]});
    call(&lib.c, "update_styles", moved).await.unwrap();
    let (_, node) = saved_node(&dir, &page, "Agent box");
    assert_eq!((node.rect.x, node.rect.y), (120.0, 60.0));
    let before_fill = files(&dir, &page);
    let fill = json!({"binding": b, "updates": [{"nodeIds": [nid],
        "styles": {"backgroundColor": "#00ff00"}}]});
    call(&lib.c, "update_styles", fill).await.unwrap();
    assert_ne!(files(&dir, &page).0, before_fill.0, "the fill was saved");
    // Read the results back through upstream's readers.
    let ask = json!({"binding": b, "nodeId": nid});
    let info = out_text(call(&lib.c, "get_node_info", ask).await);
    let info: Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["worldX"].as_f64(), Some(120.0), "{info}");
    assert_eq!(info["worldY"].as_f64(), Some(60.0), "{info}");
    let ask = json!({"binding": b, "nodeIds": [nid]});
    let styles = out_text(call(&lib.c, "get_computed_styles", ask).await);
    let styles = styles.to_ascii_lowercase();
    assert!(
        styles.contains("#00ff00") || styles.contains("rgb(0, 255, 0)"),
        "{styles}"
    );
    // Close (no editor holds anything), then a fresh process reopens it.
    let closed = value(call(&lib.c, app::AGENT_CLOSE, json!({"binding": b})).await);
    assert_eq!(closed["action"], "closed");
    let late = json!({"binding": b, "nodeId": "root", "pageId": page});
    let e = call(&lib.c, "get_tree_summary", late).await.unwrap_err();
    assert!(e.contains("No canvas is open"), "{e}");
    let (l2, c2, task2) = launch(&lib.root).await;
    let reopened = value(call(&c2, app::AGENT_OPEN, json!({"id": id})).await);
    let b2 = binding(&reopened);
    assert_ne!(b2, b, "a new binding");
    let read = json!({"binding": b2, "nodeId": "root", "pageId": page});
    let tree = out_text(call(&c2, "get_tree_summary", read).await);
    assert!(tree.contains("Agent box"), "{tree}");
    bound_server(&l2).await.retire().await;
    task2.abort();
}

/// The agent and the person on one canvas: the person's panel (and then a browser tab from the
/// agent's link) and the agent's tools share the binding, its server and its revisions. A clean
/// panel sees the agent's commit; a panel save based on the old revision is a conflict and
/// overwrites nothing; after the person saves, the agent's stale change is refused. The agent
/// can't switch or close the canvas while the person's editor is mounted or a link was given.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_and_person_share_the_canvas_with_strict_conflicts_and_no_forced_switch() {
    let lib = library().await;
    let created = value(
        call(
            &lib.c,
            app::CREATE,
            json!({"name": "Shared", "panel": true}),
        )
        .await,
    );
    let b = binding(&created);
    let dir = location(&created);
    let (page, mount, rev, text) = first_read(&lib.c, &b).await;
    let current = value(call(&lib.c, app::AGENT_CURRENT, json!({})).await);
    assert_eq!(binding(&current), b, "the canvas the person's panel shows");
    assert_eq!(current["editors"]["panel_editors"], 1);
    let read = json!({"binding": b, "nodeId": "root", "pageId": page});
    out_text(call(&lib.c, "get_tree_summary", read.clone()).await);
    let shape = |name: &str| json!({"binding": b, "pageId": page, "type": "rectangle", "name": name, "x": 1, "y": 1, "width": 9, "height": 9});
    call(&lib.c, "create_shape", shape("From the agent"))
        .await
        .unwrap();
    let agent_bytes = files(&dir, &page);
    // The clean panel's poll and re-read see the commit.
    let ids = json!({"binding": b, "page_id": page, "mount": mount});
    let polled = value(call(&lib.c, app::REVISION, ids.clone()).await);
    assert_ne!(polled["revision"], json!(rev));
    assert_eq!(polled["revision"], json!(crate::etag(&agent_bytes.0)));
    let reread = value(call(&lib.c, app::PAGE, ids).await);
    let reread_text = reread["text"].as_str().unwrap().to_string();
    assert!(reread_text.contains("From the agent"));
    // A dirty panel's save on the old revision: a conflict, nothing overwritten.
    let stale = json!({"binding": b, "page_id": page, "mount": mount, "if_match": rev,
        "text": edited(&text)});
    let e = call(&lib.c, app::SAVE, stale).await.unwrap_err();
    assert_eq!(refusal(&e)["status"], 409, "{e}");
    assert_eq!(files(&dir, &page), agent_bytes, "the agent's bytes stay");
    // The person saves on the current revision; the agent's next change is then stale.
    let human = json!({"binding": b, "page_id": page, "mount": mount,
        "if_match": polled["revision"], "text": edited(&reread_text)});
    value(call(&lib.c, app::SAVE, human).await);
    let human_bytes = files(&dir, &page);
    let e = call(&lib.c, "create_shape", shape("Stale"))
        .await
        .unwrap_err();
    assert!(e.contains("conflict"), "{e}");
    assert_eq!(files(&dir, &page), human_bytes, "the person's bytes stay");
    // No switch or close from the agent while the person's editor is mounted.
    for (tool, args) in [
        (app::AGENT_CREATE, json!({"name": "Other"})),
        (app::AGENT_CLOSE, json!({"binding": b})),
    ] {
        let e = call(&lib.c, tool, args).await.unwrap_err();
        assert!(e.contains("can't be switched or closed"), "{tool}: {e}");
        let st = lib.l.state.lock().await;
        assert_eq!(
            st.bound.as_ref().unwrap().binding.as_deref(),
            Some(b.as_str())
        );
    }
    let e = call(
        &lib.c,
        app::AGENT_OPEN,
        json!({"id": "0123456789abcdef", "force": true}),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("unexpected argument"),
        "no confirmation from the agent: {e}"
    );
    // The browser editor link, for Codex's built-in browser: the same binding's loopback
    // editor, whose save and the agent's tools share one revision check.
    let link = value(call(&lib.c, app::AGENT_EDITOR, json!({"binding": b})).await);
    assert_eq!(binding(&link), b);
    let (port, token) = url(&link);
    assert!(
        lib.l
            .state
            .lock()
            .await
            .bound
            .as_ref()
            .unwrap()
            .editor_opened
    );
    browser_edit(port, &token).await;
    let e = call(&lib.c, "create_shape", shape("Before reading"))
        .await
        .unwrap_err();
    assert!(
        e.contains("conflict"),
        "the browser's save made the agent's read stale: {e}"
    );
    out_text(call(&lib.c, "get_tree_summary", read).await);
    let before = page_nodes(port, &token, &page).await;
    call(&lib.c, "create_shape", shape("After reading"))
        .await
        .unwrap();
    assert_eq!(
        page_nodes(port, &token, &page).await,
        before + 1,
        "the browser reads it"
    );
    // After the service ends, nothing more is done through the binding.
    lib.l.shutdown().await;
    let e = call(&lib.c, "create_shape", shape("Late"))
        .await
        .unwrap_err();
    assert!(e.contains("shutting down"), "{e}");
}

/// Ownership across processes stays the lease's: an agent can't open a canvas another process
/// holds (a truthful busy refusal, no transfer), and the other process's canvas is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_open_of_a_canvas_another_process_holds_is_busy() {
    let a = library().await;
    let (lb, b, task_b) = launch(&a.root).await;
    let created = value(call(&a.c, app::AGENT_CREATE, json!({"name": "Held"})).await);
    let id = created["canvas"]["id"].as_str().unwrap().to_string();
    let e = call(&b, app::AGENT_OPEN, json!({"id": id}))
        .await
        .unwrap_err();
    assert!(e.contains("another OpenPaperDesign window"), "{e}");
    assert!(lb.state.lock().await.bound.is_none());
    let listed = value(call(&b, app::AGENT_LIST, json!({})).await);
    assert_eq!(listed["canvases"][0]["id"], json!(id));
    // In the process that holds it, opening it again is a no-op on the same binding.
    let again = value(call(&a.c, app::AGENT_OPEN, json!({"id": id})).await);
    assert_eq!(binding(&again), binding(&created));
    bound_server(&a.l).await.retire().await;
    task_b.abort();
}
