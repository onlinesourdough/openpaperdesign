//! Actual launcher, MCP stream protocol, authenticated HTTP, axum connection tasks and
//! persistence. Only the listener is substituted for portable tests. Set
//! OPD_LIFECYCLE_TCP=1 to exercise real loopback too (no change to production builds).
use super::*;
use crate::test_support::Point;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::model::ContentBlock;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tower::ServiceExt as _;

// A complete name list lets open_editor exercise its real lifecycle without requiring a
// web build for cargo test. These inert bytes are not browser-rendering evidence: the in-panel
// module parts are one-line stand-ins (enough for the launcher to assemble and serve a module,
// nothing that renders), and the "wasm" is 600,000 filler bytes that only exercise the bounded
// part transfer (three parts).
pub(super) static BUNDLE_FILES: crate::EmbeddedBundle = &[
    ("index.html", b"test fixture"),
    ("app.js", b""),
    ("page.js", b"export function frameStep() {}"),
    ("session.js", b"export class Session {}"),
    ("editor.js", b"export function opdMountEditor() {}"),
    (
        "panel.js",
        b"export function opdCreatePanel() { return null; }",
    ),
    ("openpaperdesign_web.js", b"export function start() {}"),
    ("openpaperdesign_web_bg.wasm", &FIXTURE_WASM),
];
pub(super) static FIXTURE_WASM: [u8; 600_000] = [0x61; 600_000];

type Listeners = HashMap<u16, UnboundedSender<DuplexStream>>;
fn listeners() -> &'static Mutex<Listeners> {
    static ALL: OnceLock<Mutex<Listeners>> = OnceLock::new();
    ALL.get_or_init(Default::default)
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub enum Listener {
    Memory {
        port: u16,
        rx: UnboundedReceiver<DuplexStream>,
    },
    Tcp(tokio::net::TcpListener),
}
impl Listener {
    pub async fn bind(addr: (&str, u16)) -> std::io::Result<Self> {
        if std::env::var_os("OPD_LIFECYCLE_TCP").is_some() {
            return Ok(Self::Tcp(tokio::net::TcpListener::bind(addr).await?));
        }
        static PORT: AtomicU16 = AtomicU16::new(30100);
        let port = PORT.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = unbounded_channel();
        listeners().lock().unwrap().insert(port, tx);
        Ok(Self::Memory { port, rx })
    }
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Memory { port, .. } => Ok(SocketAddr::from(([127, 0, 0, 1], *port))),
            Self::Tcp(l) => l.local_addr(),
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        if let Self::Memory { port, .. } = self {
            listeners().lock().unwrap().remove(port);
        }
    }
}
impl axum::serve::Listener for Listener {
    type Io = Box<dyn Stream>;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self {
            Self::Memory { rx, .. } => (
                Box::new(rx.recv().await.unwrap()),
                SocketAddr::from(([127, 0, 0, 1], 40000)),
            ),
            Self::Tcp(l) => {
                let (s, addr) = tokio::net::TcpListener::accept(l).await.unwrap();
                (Box::new(s), addr)
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.local_addr()
    }
}

async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("test timed out")
}

#[path = "../tests/support/codex_context.rs"]
pub(super) mod context;
struct Client {
    peer: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    meta: RequestMetaObject,
}
async fn connect_launcher(
    l: Launcher,
    meta: RequestMetaObject,
) -> (Arc<Client>, tokio::task::JoinHandle<()>) {
    let (client, server) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        l.serve(server).await.unwrap().waiting().await.unwrap();
    });
    let c = Arc::new(Client {
        peer: ().serve(client).await.unwrap(),
        meta,
    });
    (c, task)
}
async fn call(c: &Client, name: &str, args: Value) -> ToolResult {
    call_meta(c, name, args, c.meta.clone()).await
}
async fn call_meta(c: &Client, name: &str, args: Value, meta: RequestMetaObject) -> ToolResult {
    let mut p = CallToolRequestParams::new(name.to_string());
    p.arguments = args.as_object().cloned();
    p.meta = Some(meta);
    let r = bounded(c.peer.call_tool(p)).await.unwrap();
    let texts: Vec<String> = r
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    if r.is_error == Some(true) {
        Err(texts.join("\n"))
    } else {
        Ok(texts.into_iter().map(Out::Text).collect())
    }
}
fn value(r: ToolResult) -> Value {
    r.unwrap()
        .iter()
        .find_map(|x| match x {
            Out::Text(t) => serde_json::from_str(t).ok(),
            _ => None,
        })
        .expect("JSON tool result")
}
fn insertion(frame: &str) -> Value {
    json!({"targetNodeId": frame, "mode": "insert-children", "html": "<div style='height:20px'></div>"})
}
async fn width(c: &Client, frame: &str) -> f64 {
    let v = value(call(c, "write_html", insertion(frame)).await);
    value(
        call(
            c,
            "get_node_info",
            json!({"nodeId": v["createdNodeIds"][0]}),
        )
        .await,
    )["width"]
        .as_f64()
        .unwrap()
}
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.path().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

struct Fixture {
    ws: PathBuf,
    l: Launcher,
    c: Arc<Client>,
    task: tokio::task::JoinHandle<()>,
    x: paper_rs::project::Project,
    page: String,
    frame: String,
    port: u16,
    token: String,
    old: Arc<Server>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_context(false).await
    }
    async fn with_context(host: bool) -> Self {
        use std::hash::{BuildHasher, RandomState};
        // Match the launcher's canonical paths before deriving project and hook paths.
        // macOS temp_dir() can name an alias (e.g. /var rather than /private/var).
        let ws = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "opd-lifecycle-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (x, _) = paper_rs::project::Store::open(ws.join("design"))
            .unwrap()
            .create("X")
            .unwrap();
        let page = x.manifest.pages[0].id.clone();
        let l = Launcher::new(if host {
            None
        } else {
            Some(Workspace::new(&ws, "process cwd").unwrap())
        });
        let meta = if host {
            context::writable(&ws)
        } else {
            Default::default()
        };
        // Real rmcp stream protocol; only the transport/listener is in memory.
        let (c, task) = connect_launcher(l.clone(), meta).await;
        call(&c, "bind_project", json!({"path": "design/X"}))
            .await
            .unwrap();
        call(
            &c,
            "get_tree_summary",
            json!({"nodeId": "root", "pageId": page}),
        )
        .await
        .unwrap();
        let frame = value(call(&c, "create_artboard", json!({"name": "Frame", "styles": {"display": "flex", "flexDirection": "column", "alignItems": "stretch", "width": "300px", "height": "200px"}})).await)["id"].as_str().unwrap().to_string();
        copy_dir(&x.dir, &ws.join("design/Y"));
        call(&c, "update_styles", json!({"updates": [{"nodeIds": [frame], "styles": {"alignItems": "flex-start", "width": "450px"}}]})).await.unwrap();
        assert_eq!(width(&c, &frame).await, 0.0);
        let editor = value(call(&c, "open_editor", json!({})).await);
        let (port, token) = editor["url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://127.0.0.1:")
            .unwrap()
            .split_once("/#token=")
            .unwrap();
        let port = port.parse().unwrap();
        let token = token.to_string();
        let old = l.state.lock().await.bound.as_ref().unwrap().server.clone();
        assert!(
            call(&c, "bind_project", json!({"path": "design/Y"}))
                .await
                .unwrap_err()
                .contains("force: true")
        );
        Self {
            ws,
            l,
            c,
            task,
            x,
            page,
            frame,
            port,
            token,
            old,
        }
    }
    fn bytes(&self) -> (Vec<u8>, Vec<u8>) {
        (
            std::fs::read(self.x.page_path(&self.page)).unwrap(),
            std::fs::read(self.x.dir.join(paper_rs::project::MANIFEST)).unwrap(),
        )
    }
    async fn finish(&self) {
        self.l
            .state
            .lock()
            .await
            .bound
            .as_ref()
            .unwrap()
            .server
            .retire()
            .await;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.ws);
    }
}

async fn connect(port: u16) -> Box<dyn Stream> {
    if std::env::var_os("OPD_LIFECYCLE_TCP").is_some() {
        return Box::new(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap(),
        );
    }
    let (client, server) = tokio::io::duplex(1 << 20);
    listeners()
        .lock()
        .unwrap()
        .get(&port)
        .unwrap()
        .send(server)
        .unwrap();
    Box::new(client)
}
async fn http(
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    extra: &str,
    body: &str,
) -> (u16, String, Value) {
    let mut client = connect(port).await;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    bounded(client.read_to_end(&mut bytes)).await.unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    // rmcp responds with compact JSON in SSE. Ignore HTTP chunk-length lines.
    let value = body
        .lines()
        .find_map(|l| {
            serde_json::from_str::<Value>(l.strip_prefix("data: ").unwrap_or(l))
                .ok()
                .filter(Value::is_object)
        })
        .unwrap_or(Value::Null);
    (status, head.to_owned(), value)
}
async fn http_session(f: &Fixture, read: bool) -> String {
    let (status, head, v) = http(f.port, &f.token, "POST", "/mcp", "", &json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"lifecycle-test", "version":"1"}}}).to_string()).await;
    assert_eq!(status, 200);
    assert!(v["result"].is_object());
    let sid = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap();
    let extra = format!("Mcp-Session-Id: {sid}\r\n");
    assert_eq!(
        http(
            f.port,
            &f.token,
            "POST",
            "/mcp",
            &extra,
            &json!({"jsonrpc":"2.0", "method":"notifications/initialized"}).to_string()
        )
        .await
        .0,
        202
    );
    if read {
        http_call(
            f.port,
            &f.token,
            &sid,
            "get_tree_summary",
            json!({"nodeId":"root", "pageId": f.page}),
        )
        .await
        .unwrap();
    }
    sid
}
async fn http_call(port: u16, token: &str, sid: &str, name: &str, args: Value) -> ToolResult {
    static ID: AtomicU64 = AtomicU64::new(10);
    let id = ID.fetch_add(1, Ordering::Relaxed);
    let (status, _, v) = http(port, token, "POST", "/mcp", &format!("Mcp-Session-Id: {sid}\r\n"), &json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":name,"arguments":args}}).to_string()).await;
    assert_eq!(status, 200, "{v}");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    if v["result"]["isError"] == true {
        Err(text)
    } else {
        Ok(vec![Out::Text(text)])
    }
}

/// The two round-3 failures, each through HTTP and the concurrent stdio handler. A second
/// MCP call and a browser save are queued before retirement; neither may write afterward.
async fn overlap(use_http: bool, accepted: bool, downgrade: bool) {
    let f = Fixture::with_context(downgrade).await;
    let sid = http_session(&f, accepted).await;
    if !use_http && !accepted {
        // Make the stdio session stale via a real browser save; HTTP uses an unread session.
        let bytes = f.bytes().0;
        let mut page =
            paper_rs::project::format::page_from_json(std::str::from_utf8(&bytes).unwrap())
                .unwrap();
        page.doc.next_id += 1;
        assert_eq!(
            http(
                f.port,
                &f.token,
                "PUT",
                &format!("/api/pages/{}", f.page),
                &format!("If-Match: {}\r\n", crate::etag(&bytes)),
                &paper_rs::project::format::page_to_json(&page)
            )
            .await
            .0,
            200
        );
    }
    let before = f.bytes();
    let mut snapshot = Point::arm(&f.x.dir, "snapshot", true);
    let name = if accepted {
        "write_html"
    } else {
        "update_styles"
    };
    let args = if accepted {
        insertion(&f.frame)
    } else {
        json!({"updates":[{"nodeIds":[f.frame],"styles":{"width":"460px"}}]})
    };
    let c = f.c.clone();
    let token = f.token.clone();
    let port = f.port;
    let sid2 = sid.clone();
    let running = tokio::spawn(async move {
        if use_http {
            http_call(port, &token, &sid2, name, args).await
        } else {
            call(&c, name, args).await
        }
    });
    snapshot.reached().await;
    let mut queued_point = Point::arm(&f.x.dir, "queued", false);
    let c = f.c.clone();
    let token = f.token.clone();
    let args = insertion(&f.frame);
    let queued = tokio::spawn(async move {
        if use_http {
            http_call(port, &token, &sid, "write_html", args).await
        } else {
            call(&c, "write_html", args).await
        }
    });
    queued_point.reached().await;
    let mut browser_point = Point::arm(&f.x.dir, "browser_queued", false);
    let token = f.token.clone();
    let page_path = format!("/api/pages/{}", f.page);
    let body = std::str::from_utf8(&before.0).unwrap().to_owned();
    let extra = format!("If-Match: {}\r\n", crate::etag(&before.0));
    let browser =
        tokio::spawn(async move { http(port, &token, "PUT", &page_path, &extra, &body).await });
    browser_point.reached().await;
    let c = f.c.clone();
    let revoked = context::readonly(&f.ws);
    let switch = tokio::spawn(async move {
        if downgrade {
            call_meta(
                &c,
                "bind_project",
                json!({"path":"design/Y","force":true}),
                revoked,
            )
            .await
        } else {
            call(&c, "bind_project", json!({"path":"design/Y","force":true})).await
        }
    });
    bounded(f.old.lifecycle.retiring.notified()).await;
    assert!(
        !switch.is_finished(),
        "handoff must wait for the started call"
    );
    assert_eq!(f.bytes(), before, "paused call has not saved");
    assert!(
        paper_rs::mcp::exec::with_no_stretch(|s| s.iter().any(|(p, _)| p == &f.page)),
        "preparation must preserve X's live layout state"
    );
    snapshot.resume();
    let result = bounded(running).await.unwrap();
    if accepted {
        let id = value(result)["createdNodeIds"][0]
            .as_str()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let page = f.x.load_page(&f.page).unwrap();
        assert_eq!(
            page.doc.get(paper_rs::model::NodeId(id)).unwrap().rect.w,
            0.0,
            "started X insertion persists non-stretch geometry"
        );
        let before_page =
            paper_rs::project::format::page_from_json(std::str::from_utf8(&before.0).unwrap())
                .unwrap();
        assert_eq!(
            page.doc.next_id,
            before_page.doc.next_id + 1,
            "exactly one insertion persisted"
        );
    } else {
        assert!(result.unwrap_err().contains("conflict"));
        assert_eq!(
            f.bytes(),
            before,
            "refused call and queued work change no files"
        );
    }
    assert!(
        bounded(queued)
            .await
            .unwrap()
            .unwrap_err()
            .contains("retired")
    );
    assert_eq!(bounded(browser).await.unwrap().0, 503);
    let switch_result = bounded(switch).await.unwrap();
    let fresh;
    let fresh_task;
    let live = if downgrade {
        assert!(switch_result.unwrap_err().contains("retired"));
        assert!(
            call(&f.c, "write_html", insertion(&f.frame))
                .await
                .unwrap_err()
                .contains("retired")
        );
        assert_eq!(
            value(call(&f.c, "openpaperdesign_status", json!({})).await)["workspace_state"],
            "retired"
        );
        // Reconnection is a new one-workspace launcher after the old one retired.
        let l = Launcher::new(None);
        let (c, task) = connect_launcher(l.clone(), context::writable(&f.ws)).await;
        call(&c, "bind_project", json!({"path":"design/Y"}))
            .await
            .unwrap();
        fresh = Some((l, c));
        fresh_task = Some(task);
        &fresh.as_ref().unwrap().1
    } else {
        switch_result.unwrap();
        fresh = None;
        fresh_task = None;
        &f.c
    };
    let old_bytes = f.bytes();
    call(
        live,
        "get_tree_summary",
        json!({"nodeId":"root","pageId":f.page}),
    )
    .await
    .unwrap();
    assert_eq!(
        width(live, &f.frame).await,
        300.0,
        "Y never inherits X's rollback entries"
    );
    assert!(
        f.old
            .call_tool(f.l.session, "write_html", insertion(&f.frame))
            .await
            .unwrap_err()
            .contains("retired")
    );
    // A retained router models a late request on an already accepted connection.
    let response = router(f.old.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/pages/{}", f.page))
                .header("Host", format!("127.0.0.1:{}", f.port))
                .header("Authorization", format!("Bearer {}", f.token))
                .header("If-Match", crate::etag(&old_bytes.0))
                .body(Body::from(old_bytes.0.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(f.bytes(), old_bytes);
    assert_eq!(width(live, &f.frame).await, 300.0);
    eprintln!(
        "{} {}: X {}, Y 300; queued MCP/browser and late handles refused",
        if use_http { "HTTP" } else { "stdio protocol" },
        if accepted { "accepted" } else { "refused" },
        if accepted { "child 0" } else { "unchanged" }
    );
    if let Some((l, _)) = fresh {
        l.state
            .lock()
            .await
            .bound
            .as_ref()
            .unwrap()
            .server
            .retire()
            .await;
        fresh_task.unwrap().abort();
    } else {
        f.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_http_refused_rollback_is_drained() {
    overlap(true, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_http_started_insert_keeps_geometry() {
    overlap(true, true, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_stdio_refused_rollback_is_drained() {
    overlap(false, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_stdio_started_insert_keeps_geometry() {
    overlap(false, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_downgrade_drains_http_rollback_and_fences_browser() {
    overlap(true, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_downgrade_drains_stdio_insertion_and_fences_browser() {
    overlap(false, true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_invalidation_survives_cancellation_and_never_revives() {
    let f = Fixture::with_context(true).await;
    assert_eq!(
        value(
            call_meta(
                &f.c,
                "openpaperdesign_status",
                json!({}),
                context::disabled(&f.ws)
            )
            .await
        )["workspace_permission_mode"],
        "host-sandbox-disabled"
    );
    let mut snapshot = Point::arm(&f.x.dir, "snapshot", true);
    let c = f.c.clone();
    let args = insertion(&f.frame);
    let running = tokio::spawn(async move { call(&c, "write_html", args).await });
    snapshot.reached().await;
    let l = f.l.clone();
    let retire =
        tokio::spawn(async move { l.run("open_editor", json!({}), &Default::default()).await });
    bounded(f.old.lifecycle.retiring.notified()).await;
    retire.abort();
    assert!(retire.await.unwrap_err().is_cancelled());
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    let s = value(call(&f.c, "openpaperdesign_status", json!({})).await);
    assert_eq!(s["workspace_state"], "retired");
    assert!(s["bound_project"].is_null());
    let before = f.bytes();
    assert!(
        call(&f.c, "create_project", json!({"name":"Late"}))
            .await
            .is_err()
    );
    assert!(!f.ws.join("design/Late").exists());
    assert!(
        f.old
            .call_tool(f.l.session, "write_html", insertion(&f.frame))
            .await
            .is_err()
    );
    assert_eq!(f.bytes(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_concurrent_initial_roots_cannot_cross_bind() {
    let f = Fixture::new().await;
    f.finish().await;
    let other = f.ws.join("other-workspace");
    std::fs::create_dir(&other).unwrap();
    let l = Launcher::new(None);
    let (c, task) = connect_launcher(l.clone(), Default::default()).await;
    let a = call_meta(
        &c,
        "create_project",
        json!({"name":"First"}),
        context::writable(&f.ws),
    );
    let b = call_meta(
        &c,
        "create_project",
        json!({"name":"Second"}),
        context::writable(&other),
    );
    let (a, b) = tokio::join!(a, b);
    assert_ne!(a.is_ok(), b.is_ok());
    assert_ne!(
        f.ws.join("design/First").exists(),
        other.join("design/Second").exists()
    );
    let state = l.state.lock().await;
    assert!(state.invalidated.is_some());
    assert!(state.bound.is_none());
    drop(state);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_invalidation_serializes_with_an_in_progress_bind() {
    let f = Fixture::with_context(true).await;
    let mut snapshot = Point::arm(&f.x.dir, "snapshot", true);
    let c = f.c.clone();
    let args = insertion(&f.frame);
    let running = tokio::spawn(async move { call(&c, "write_html", args).await });
    snapshot.reached().await;
    let c = f.c.clone();
    let switch = tokio::spawn(async move {
        call(&c, "bind_project", json!({"path":"design/Y","force":true})).await
    });
    bounded(f.old.lifecycle.retiring.notified()).await;
    // The switch owns the launcher state until the old writer drains and Y is
    // activated. A changed workspace must retire Y too, never race its activation.
    let c = f.c.clone();
    let changed = context::disabled(&f.ws.join("design/Y"));
    let invalidate =
        tokio::spawn(async move { call_meta(&c, "open_editor", json!({}), changed).await });
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    bounded(switch).await.unwrap().unwrap();
    assert!(
        bounded(invalidate)
            .await
            .unwrap()
            .unwrap_err()
            .contains("retired")
    );
    let state = f.l.state.lock().await;
    assert!(state.bound.is_none());
    assert!(state.invalidated.is_some());
    drop(state);
    assert!(
        call(
            &f.c,
            "bind_project",
            json!({"path":"design/Y","force":true})
        )
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_failed_and_same_bind_preserve_identity_and_layout() {
    let f = Fixture::new().await;
    copy_dir(&f.x.dir, &f.ws.join("design/Broken"));
    std::fs::write(
        f.ws.join("design/Broken/pages")
            .join(format!("{}.json", f.page)),
        "bad json",
    )
    .unwrap();
    assert!(
        call(
            &f.c,
            "bind_project",
            json!({"path":"design/Broken","force":true})
        )
        .await
        .is_err()
    );
    assert!(
        value(call(&f.c, "bind_project", json!({"path":"design/X"})).await)["note"]
            .as_str()
            .unwrap()
            .contains("already bound")
    );
    {
        let bound = f.l.state.lock().await;
        let b = bound.bound.as_ref().unwrap();
        assert!(Arc::ptr_eq(&b.server, &f.old));
        assert_eq!((b.port, &b.token), (f.port, &f.token));
        assert!(!b.task.is_finished());
    }
    assert_eq!(width(&f.c, &f.frame).await, 0.0);
    assert_eq!(
        http(f.port, &f.token, "GET", "/api/project", "", "")
            .await
            .0,
        200
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_cancelled_bind_waiter_still_completes_handoff() {
    let f = Fixture::new().await;
    let mut snapshot = Point::arm(&f.x.dir, "snapshot", true);
    let c = f.c.clone();
    let args = insertion(&f.frame);
    let running = tokio::spawn(async move { call(&c, "write_html", args).await });
    snapshot.reached().await;
    let l = f.l.clone();
    let switch = tokio::spawn(async move {
        l.run(
            "bind_project",
            json!({"path":"design/Y","force":true}),
            &Default::default(),
        )
        .await
    });
    bounded(f.old.lifecycle.retiring.notified()).await;
    switch.abort();
    assert!(switch.await.unwrap_err().is_cancelled());
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    let b = bounded(f.l.state.lock()).await;
    assert_eq!(
        b.bound.as_ref().unwrap().server.dir(),
        f.ws.join("design/Y")
    );
    drop(b);
    call(
        &f.c,
        "get_tree_summary",
        json!({"nodeId":"root","pageId":f.page}),
    )
    .await
    .unwrap();
    assert_eq!(width(&f.c, &f.frame).await, 300.0);
    call(
        &f.c,
        "update_styles",
        json!({"updates":[{"nodeIds":[f.frame],"styles":{"alignItems":"flex-start"}}]}),
    )
    .await
    .unwrap();
    // A second retirement through the old clone cannot clear the new owner's keys.
    f.old.retire().await;
    assert_eq!(width(&f.c, &f.frame).await, 0.0);
    f.finish().await;
}

// ---- direct (home view) origin: authority lifetime ----

impl Fixture {
    /// X bound by a direct view action (`openpaperdesign_ui_open`) in a fresh packaged session:
    /// no model call ever admitted it. Page reads and the frame go through the editor's own
    /// HTTP MCP session, so no launcher admission (model or view) happens after the bind.
    async fn direct() -> Self {
        use std::hash::{BuildHasher, RandomState};
        let ws = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "opd-direct-life-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (x, _) = paper_rs::project::Store::open(ws.join("design"))
            .unwrap()
            .create("X")
            .unwrap();
        let page = x.manifest.pages[0].id.clone();
        let l = Launcher::new(None);
        let (c, task) = connect_launcher(l.clone(), context::writable(&ws)).await;
        let opened = value(
            call(
                &c,
                "openpaperdesign_ui_open",
                json!({"project": "design/X"}),
            )
            .await,
        );
        assert_eq!(opened["action"], "opened");
        let (port, token) = opened["url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://127.0.0.1:")
            .unwrap()
            .split_once("/#token=")
            .unwrap();
        let (port, token) = (port.parse().unwrap(), token.to_string());
        let old = {
            let st = l.state.lock().await;
            let b = st.bound.as_ref().unwrap();
            assert_eq!(b.owner, Owner::Direct);
            assert_eq!(st.pinned_by, Owner::Direct);
            b.server.clone()
        };
        let mut f = Self {
            ws,
            l,
            c,
            task,
            x,
            page,
            frame: String::new(),
            port,
            token,
            old,
        };
        let sid = http_session(&f, true).await;
        let frame = http_call(f.port, &f.token, &sid, "create_artboard", json!({"name": "Frame", "styles": {"display": "flex", "flexDirection": "column", "width": "300px", "height": "200px"}})).await;
        f.frame = value(frame)["id"].as_str().unwrap().to_string();
        f
    }
}

async fn status_of(l: &Launcher) -> (Option<String>, bool) {
    let st = l.state.lock().await;
    (st.invalidated.clone(), st.bound.is_some())
}

/// A downgrade a view call observes for a binding the view installed retires it: the started
/// HTTP write finishes, the queued MCP write and browser save are refused, and late HTTP and
/// stdio handles stay fenced. Nothing saves after retirement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_downgrade_drains_writers_and_fences_browser_and_stdio() {
    let f = Fixture::direct().await;
    let sid = http_session(&f, true).await;
    let before = f.bytes();
    let mut snapshot = Point::arm(&f.x.dir, "snapshot", true);
    let (port, token) = (f.port, f.token.clone());
    let (sid2, args) = (sid.clone(), insertion(&f.frame));
    let running =
        tokio::spawn(async move { http_call(port, &token, &sid2, "write_html", args).await });
    snapshot.reached().await;
    let mut queued_point = Point::arm(&f.x.dir, "queued", false);
    let (token, args) = (f.token.clone(), insertion(&f.frame));
    let queued =
        tokio::spawn(async move { http_call(port, &token, &sid, "write_html", args).await });
    queued_point.reached().await;
    let mut browser_point = Point::arm(&f.x.dir, "browser_queued", false);
    let token = f.token.clone();
    let page_path = format!("/api/pages/{}", f.page);
    let body = std::str::from_utf8(&before.0).unwrap().to_owned();
    let extra = format!("If-Match: {}\r\n", crate::etag(&before.0));
    let browser =
        tokio::spawn(async move { http(port, &token, "PUT", &page_path, &extra, &body).await });
    browser_point.reached().await;
    // The same workspace, its write grant gone: observed by the view's own call.
    let c = f.c.clone();
    let revoked = context::readonly(&f.ws);
    let downgrade = tokio::spawn(async move {
        call_meta(&c, "openpaperdesign_ui_canvases", json!({}), revoked).await
    });
    bounded(f.old.lifecycle.retiring.notified()).await;
    assert!(
        !downgrade.is_finished(),
        "retirement waits for the started write"
    );
    assert_eq!(f.bytes(), before, "paused write has not saved");
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    let after_started = f.bytes();
    assert_ne!(
        after_started, before,
        "the started write completed before retirement"
    );
    assert!(
        bounded(queued)
            .await
            .unwrap()
            .unwrap_err()
            .contains("retired")
    );
    assert_eq!(bounded(browser).await.unwrap().0, 503);
    let refusal = bounded(downgrade).await.unwrap().unwrap_err();
    assert!(refusal.contains("canvas was closed"), "{refusal}");
    let (invalidated, bound) = status_of(&f.l).await;
    assert!(
        invalidated.is_some() && !bound,
        "admission closed, binding dropped"
    );
    // Late handles: a retained router (an already accepted connection) and the stdio session.
    let response = router(f.old.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/pages/{}", f.page))
                .header("Host", format!("127.0.0.1:{}", f.port))
                .header("Authorization", format!("Bearer {}", f.token))
                .header("If-Match", crate::etag(&after_started.0))
                .body(Body::from(after_started.0.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        f.old
            .call_tool(f.l.session, "write_html", insertion(&f.frame))
            .await
            .unwrap_err()
            .contains("retired")
    );
    // Nothing revives it: a valid view call and a valid model call are both refused.
    assert!(
        call(&f.c, "openpaperdesign_ui_canvases", json!({}))
            .await
            .unwrap_err()
            .contains("retired")
    );
    assert!(
        call(&f.c, "create_project", json!({"name": "Late"}))
            .await
            .is_err()
    );
    assert!(!f.ws.join("design/Late").exists());
    assert_eq!(f.bytes(), after_started, "nothing saved after retirement");
}

/// The workspace replaced under a view-installed binding (same path, new identity) is an
/// observed identity loss: retired, and the browser endpoint fenced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_identity_loss_retires_its_binding() {
    let f = Fixture::direct().await;
    let moved = f.ws.with_extension("moved");
    std::fs::rename(&f.ws, &moved).unwrap();
    std::fs::create_dir(&f.ws).unwrap();
    let refusal = call(&f.c, "openpaperdesign_ui_editor", json!({}))
        .await
        .unwrap_err();
    assert!(refusal.contains("canvas was closed"), "{refusal}");
    assert!(!status_of(&f.l).await.1, "binding dropped");
    assert!(status_of(&f.l).await.0.is_some());
    let page = std::fs::read(
        moved
            .join("design/X/pages")
            .join(format!("{}.json", f.page)),
    )
    .unwrap();
    // The listener is gone; a retained router (an already accepted connection) is fenced.
    let response = router(f.old.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/pages/{}", f.page))
                .header("Host", format!("127.0.0.1:{}", f.port))
                .header("Authorization", format!("Bearer {}", f.token))
                .header("If-Match", crate::etag(&page))
                .body(Body::from(page.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = std::fs::remove_dir_all(&moved);
}

/// A view-call retirement owned by its spawned task survives cancelling the caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_retirement_survives_cancellation_and_never_revives() {
    let f = Fixture::direct().await;
    let sid = http_session(&f, true).await;
    let mut snapshot = Point::arm(&f.x.dir, "snapshot", true);
    let (port, token, args) = (f.port, f.token.clone(), insertion(&f.frame));
    let running =
        tokio::spawn(async move { http_call(port, &token, &sid, "write_html", args).await });
    snapshot.reached().await;
    let l = f.l.clone();
    let revoked = context::readonly(&f.ws);
    let retire = tokio::spawn(async move {
        l.direct("openpaperdesign_ui_canvases", json!({}), &revoked)
            .await
    });
    bounded(f.old.lifecycle.retiring.notified()).await;
    retire.abort();
    assert!(retire.await.unwrap_err().is_cancelled());
    snapshot.resume();
    bounded(running).await.unwrap().unwrap();
    let (invalidated, bound) = status_of(&f.l).await;
    assert!(invalidated.is_some() && !bound);
    let before = f.bytes();
    assert!(
        f.old
            .call_tool(f.l.session, "write_html", insertion(&f.frame))
            .await
            .is_err()
    );
    assert!(
        call(&f.c, "openpaperdesign_ui_create", json!({"name": "Late"}))
            .await
            .is_err()
    );
    assert!(!f.ws.join("design/Late").exists());
    assert_eq!(f.bytes(), before);
}

/// Controls: a view call without the host's context observes nothing, and no view call
/// retires a binding a model call installed, even with a downgraded or malformed context.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_calls_never_retire_without_context_or_a_model_binding() {
    // View-installed binding, view call without context: refused, still bound and writable.
    let f = Fixture::direct().await;
    let app: RequestMetaObject = json!({"thread_id": "t", "threadId": "t"})
        .as_object()
        .unwrap()
        .clone()
        .into();
    let refusal = call_meta(&f.c, "openpaperdesign_ui_canvases", json!({}), app.clone())
        .await
        .unwrap_err();
    assert!(refusal.contains("no Codex workspace context"), "{refusal}");
    assert_eq!(status_of(&f.l).await, (None, true));
    assert_eq!(
        http(f.port, &f.token, "GET", "/api/project", "", "")
            .await
            .0,
        200
    );
    f.finish().await;

    // Model-installed binding: downgraded, malformed and other-workspace view calls refuse only.
    let m = Fixture::with_context(true).await;
    let mut malformed = context::writable(&m.ws);
    malformed.get_mut(context::KEY).unwrap()["sandboxCwd"] = json!(12);
    let other = m.ws.join("design");
    for meta in [
        context::readonly(&m.ws),
        malformed,
        context::writable(&other),
        app,
    ] {
        assert!(
            call_meta(&m.c, "openpaperdesign_ui_canvases", json!({}), meta)
                .await
                .is_err()
        );
        assert_eq!(status_of(&m.l).await, (None, true));
    }
    {
        let st = m.l.state.lock().await;
        assert_eq!(st.bound.as_ref().unwrap().owner, Owner::Model);
        assert!(Arc::ptr_eq(&st.bound.as_ref().unwrap().server, &m.old));
    }
    assert_eq!(
        http(m.port, &m.token, "GET", "/api/project", "", "")
            .await
            .0,
        200
    );
    // The model's own downgrade still retires it (unchanged).
    assert!(
        call_meta(
            &m.c,
            "openpaperdesign_status",
            json!({}),
            context::readonly(&m.ws)
        )
        .await
        .is_ok()
    );
    assert!(status_of(&m.l).await.0.is_some());
}

// ---- the app library (`--app-library`) ----

#[path = "launcher_app_tests.rs"]
mod app_library;
