//! The app library (`openpaperdesign-mcp --app-library <dir>`) as real child processes sharing
//! one library folder, driven with the rmcp client and real loopback HTTP (the editor's API).
//! No request carries Codex metadata. This is a synthetic host: no browser renderer, no Codex
//! navigation, no native plugin data root, and not a Mac receipt.

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp() -> Tmp {
    use std::hash::{BuildHasher, RandomState};
    let d = std::env::temp_dir().join(format!(
        "opd-app-library-it-{:016x}",
        RandomState::new().hash_one(std::time::SystemTime::now())
    ));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d.canonicalize().unwrap())
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

fn command(root: &Path, home: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_openpaperdesign-mcp"));
    // A working directory that isn't the library: it must not matter.
    cmd.current_dir(home)
        .env("HOME", home)
        .env_remove("PLUGIN_ROOT")
        .arg("--app-library")
        .arg(root);
    cmd
}

async fn library(root: &Path, home: &Path) -> Client {
    ().serve(rmcp::transport::TokioChildProcess::new(command(root, home)).unwrap())
        .await
        .unwrap()
}

async fn call(c: &Client, name: &str, args: Value) -> Result<Value, String> {
    let mut p = CallToolRequestParams::new(name.to_string());
    p.arguments = args.as_object().cloned();
    let r = c.call_tool(p).await.unwrap();
    let text = r
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if r.is_error == Some(true) {
        Err(text)
    } else {
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
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

/// A minimal HTTP/1.1 request on loopback with the token; returns (status, ETag, body).
async fn http(
    port: u16,
    method: &str,
    path: &str,
    token: &str,
    if_match: Option<&str>,
    body: &str,
) -> (u16, Option<String>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let im = if_match.map_or(String::new(), |m| format!("If-Match: {m}\r\n"));
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n{im}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let etag = head.lines().find_map(|l| {
        l.to_ascii_lowercase()
            .starts_with("etag:")
            .then(|| l[5..].trim().to_string())
    });
    (status, etag, body.to_string())
}

/// The browser editor's load-change-save of the active page; returns the page id.
async fn edit(port: u16, token: &str) -> String {
    let (_, _, project) = http(port, "GET", "/api/project", token, None, "").await;
    let page = serde_json::from_str::<Value>(&project).unwrap()["active_page"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/api/pages/{page}");
    let (st, tag, body) = http(port, "GET", &path, token, None, "").await;
    assert_eq!(st, 200, "{body}");
    let mut data = paper_rs::project::format::page_from_json(&body).unwrap();
    let id = data.doc.alloc_id();
    data.doc.nodes.insert(
        id,
        paper_rs::model::Node::new(
            id,
            paper_rs::model::NodeKind::Rect,
            paper_rs::geom::Rect::new(1.0, 1.0, 20.0, 20.0),
        ),
    );
    data.doc.root.push(id);
    let body = paper_rs::project::format::page_to_json(&data);
    let (st, _, answer) = http(port, "PUT", &path, token, tag.as_deref(), &body).await;
    assert_eq!(st, 200, "{answer}");
    page
}

async fn nodes(port: u16, token: &str, page: &str) -> usize {
    let (st, _, body) = http(port, "GET", &format!("/api/pages/{page}"), token, None, "").await;
    assert_eq!(st, 200, "{body}");
    paper_rs::project::format::page_from_json(&body)
        .unwrap()
        .doc
        .nodes
        .len()
}

/// Open `id`, waiting (bounded) while another process still holds it.
async fn open_when_free(c: &Client, id: &str) -> Value {
    for _ in 0..100 {
        match call(c, "openpaperdesign_app_open", json!({"id": id})).await {
            Ok(v) => return v,
            Err(e) if e.contains("another OpenPaperDesign window") => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    panic!("the canvas stayed busy");
}

/// Two processes on one library: create and edit in one, busy in the other until closed,
/// a different canvas meanwhile, then the saved edit in a fresh process; an ended process
/// releases its canvas. No request carries metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_share_the_library_one_canvas_at_a_time() {
    let t = tmp();
    let (root, home) = (t.0.join("data"), t.0.join("home"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let a = library(&root, &home).await;
    let b = library(&root, &home).await;
    let created = call(&a, "openpaperdesign_app_create", json!({"name": "Shared"}))
        .await
        .unwrap();
    let id = created["canvas"]["id"].as_str().unwrap().to_string();
    let (port, token) = url(&created);
    let page = edit(port, &token).await;
    let e = call(&b, "openpaperdesign_app_open", json!({"id": id}))
        .await
        .unwrap_err();
    assert!(e.contains("another OpenPaperDesign window"), "{e}");
    let other = call(&b, "openpaperdesign_app_create", json!({"name": "Other"}))
        .await
        .unwrap();
    let other_id = other["canvas"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        nodes(port, &token, &page).await,
        1,
        "A's canvas is unaffected"
    );
    // Each view names the binding it was shown (close) or confirmed replacing (switch).
    let shown = |v: &Value| v["binding"].as_str().unwrap().to_string();
    call(
        &a,
        "openpaperdesign_app_close",
        json!({"binding": shown(&created), "force": true}),
    )
    .await
    .unwrap();
    let in_b = call(
        &b,
        "openpaperdesign_app_open",
        json!({"id": id, "force": true, "replace": shown(&other)}),
    )
    .await
    .unwrap();
    let (pb, tb) = url(&in_b);
    assert_eq!(
        nodes(pb, &tb, &page).await,
        1,
        "the saved edit, in another process"
    );
    // B ends without closing: its canvas becomes free for a fresh process.
    b.cancel().await.unwrap();
    let c = library(&root, &home).await;
    let reopened = open_when_free(&c, &id).await;
    let (pc, tc) = url(&reopened);
    assert_eq!(nodes(pc, &tc, &page).await, 1);
    let listed = call(&c, "openpaperdesign_app_canvases", json!({}))
        .await
        .unwrap();
    let ids: Vec<&str> = listed["canvases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id.as_str()) && ids.contains(&other_id.as_str()));
    // Project routes don't exist here, metadata or not.
    let e = call(&c, "bind_project", json!({"path": root}))
        .await
        .unwrap_err();
    assert!(e.contains("runs the app library"), "{e}");
    a.cancel().await.unwrap();
    c.cancel().await.unwrap();
}

/// An agent's path through the real executable (its own build, with whatever bundle it
/// embeds, not the unit tests' stand-in): the catalogue lists the library controls and
/// upstream's design tools with `binding` required; the agent creates a canvas, reads its page,
/// draws a rectangle with upstream's `create_shape`, and the saved page file (upstream's
/// format) has it; a fresh process reopens the canvas and reads the drawing back. No request
/// carries metadata. This is a synthetic host, not a Codex catalogue or a Mac receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_edits_a_library_canvas_through_the_real_executable() {
    let t = tmp();
    let (root, home) = (t.0.join("data"), t.0.join("home"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let a = library(&root, &home).await;
    let tools = a.list_all_tools().await.unwrap();
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    for n in [
        "openpaperdesign_library_canvases",
        "openpaperdesign_library_current",
        "openpaperdesign_library_create",
        "openpaperdesign_library_open",
        "openpaperdesign_library_close",
        "openpaperdesign_library_editor",
        "get_basic_info",
        "get_tree_summary",
        "create_shape",
        "update_styles",
        "write_html",
    ] {
        assert!(names.iter().any(|x| x == n), "{n} is listed");
    }
    for refused in ["export", "create_file", "bind_project"] {
        assert!(
            !names.iter().any(|x| x == refused),
            "{refused} isn't listed"
        );
    }
    let shape_tool = tools.iter().find(|t| t.name == "create_shape").unwrap();
    let schema = Value::Object((*shape_tool.input_schema).clone());
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("binding"))
    );
    let created = call(
        &a,
        "openpaperdesign_library_create",
        json!({"name": "Agent"}),
    )
    .await
    .unwrap();
    assert!(created.get("url").is_none());
    let b = created["binding"].as_str().unwrap().to_string();
    let id = created["canvas"]["id"].as_str().unwrap().to_string();
    let page = created["pages"][0]["id"].as_str().unwrap().to_string();
    let read = json!({"binding": b, "nodeId": "root", "pageId": page});
    call(&a, "get_tree_summary", read).await.unwrap();
    let shape = json!({"binding": b, "pageId": page, "type": "rectangle", "name": "Real box",
        "x": 30, "y": 40, "width": 50, "height": 25});
    call(&a, "create_shape", shape).await.unwrap();
    let file = root
        .join("library/v1/canvases")
        .join(&id)
        .join("pages")
        .join(format!("{page}.json"));
    let text = std::fs::read_to_string(&file).unwrap();
    let data = paper_rs::project::format::page_from_json(&text).unwrap();
    let node = data
        .doc
        .nodes
        .values()
        .find(|n| n.name == "Real box")
        .unwrap();
    assert_eq!((node.rect.x, node.rect.y), (30.0, 40.0));
    let status = call(&a, "openpaperdesign_status", json!({})).await.unwrap();
    assert_eq!(status["binding"], json!(b));
    call(&a, "openpaperdesign_library_close", json!({"binding": b}))
        .await
        .unwrap();
    a.cancel().await.unwrap();
    let c = library(&root, &home).await;
    let mut reopened = None;
    for _ in 0..100 {
        match call(&c, "openpaperdesign_library_open", json!({"id": id})).await {
            Ok(v) => {
                reopened = Some(v);
                break;
            }
            Err(e) if e.contains("another OpenPaperDesign window") => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    let reopened = reopened.expect("the canvas is free again");
    let b2 = reopened["binding"].as_str().unwrap();
    let read = json!({"binding": b2, "nodeId": "root", "pageId": page});
    let tree = call(&c, "get_tree_summary", read).await.unwrap();
    assert!(tree.to_string().contains("Real box"), "{tree}");
    c.cancel().await.unwrap();
}

/// The home view through the real executable in app-library mode. The home entry and the one
/// listed resource name the current versioned URI (`home-3`, the browser-first view). Reading
/// it returns this source tree's unchanged view frame with its slots filled (checked here for
/// shape only), with the view's MIME type, and changes nothing. Earlier, other,
/// query and file URIs are refused, so a fresh read can't get an older view. This says nothing
/// about a view a host already has open (it isn't evicted or refreshed). A synthetic host, not a
/// receipt of what a Codex host cached or shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_library_home_view_is_served_only_under_its_current_versioned_uri() {
    use rmcp::model::ReadResourceRequestParams;
    const HOME: &str = "ui://openpaperdesign/home-3.html";
    const PANEL_SLOT: &str = "const opdCreatePanel = null; // __OPD_PANEL_MODULE__";
    let t = tmp();
    let (root, home) = (t.0.join("data"), t.0.join("home"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let a = library(&root, &home).await;
    let tools: Vec<Value> = a
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| serde_json::to_value(t).unwrap())
        .collect();
    let entry = tools
        .iter()
        .find(|t| t["name"] == "openpaperdesign_home")
        .unwrap();
    assert_eq!(entry["_meta"]["ui"]["resourceUri"], HOME);
    for t in &tools {
        if let Some(uri) = t["_meta"]["ui"].get("resourceUri") {
            assert_eq!(uri, HOME, "{}", t["name"]);
        }
    }
    let resources: Vec<Value> = a
        .list_all_resources()
        .await
        .unwrap()
        .into_iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
    assert_eq!(
        resources,
        vec![
            json!({"uri":HOME,"name":"openpaperdesign-home","title":"OpenPaperDesign","mimeType":"text/html;profile=mcp-app"})
        ]
    );
    // An open canvas, so there is state for the reads to leave alone.
    let created = call(&a, "openpaperdesign_app_create", json!({"name": "Kept"}))
        .await
        .unwrap();
    let status = call(&a, "openpaperdesign_status", json!({})).await.unwrap();
    assert_eq!(status["binding"], created["binding"]);
    let listed = call(&a, "openpaperdesign_app_canvases", json!({}))
        .await
        .unwrap();
    let r = a
        .read_resource(ReadResourceRequestParams::new(HOME))
        .await
        .unwrap();
    let r = serde_json::to_value(r).unwrap();
    assert_eq!(r["contents"][0]["uri"], HOME);
    assert_eq!(r["contents"][0]["mimeType"], "text/html;profile=mcp-app");
    let html = r["contents"][0]["text"].as_str().unwrap();
    // Everything outside the slots is this source tree's view, byte for byte, with the mode slot
    // set to `library`. The two other slots are checked for shape only: the font slot holds
    // base64 text, and the editor slot is left as it is or names `opdCreatePanel`. Neither
    // proves the exact font or module bytes; the workspace test decodes the font, and the full
    // byte identity is the separate production gate on the exact non-test executable.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/home.html");
    let source = std::fs::read_to_string(path)
        .unwrap()
        .replacen("__OPD_MODE__", "library", 1);
    let (head, rest) = source.split_once("__OPD_WORDMARK_FONT__").unwrap();
    let (middle, tail) = rest.split_once(PANEL_SLOT).unwrap();
    let filled = html
        .strip_prefix(head)
        .and_then(|h| h.strip_suffix(tail))
        .expect("the served view is src/home.html with only its slots filled");
    let (font, panel) = filled.split_once(middle).expect("the slots in order");
    let base64 = |b: u8| b.is_ascii_alphanumeric() || b"+/=".contains(&b);
    assert!(
        !font.is_empty() && font.bytes().all(base64),
        "the font slot holds base64"
    );
    assert!(
        panel == PANEL_SLOT || panel.contains("function opdCreatePanel("),
        "the editor slot holds this build's module or nothing"
    );
    for other in [
        "ui://openpaperdesign/home-3.html?x",
        // Earlier view URIs: never served with this server's behaviour.
        "ui://openpaperdesign/home-2.html",
        "ui://openpaperdesign/home.html",
        "ui://openpaperdesign/other.html",
        "file:///etc/passwd",
    ] {
        let e = a.read_resource(ReadResourceRequestParams::new(other)).await;
        assert!(e.is_err(), "{other}");
    }
    assert_eq!(
        call(&a, "openpaperdesign_status", json!({})).await.unwrap(),
        status,
        "reading the view changed nothing"
    );
    assert_eq!(
        call(&a, "openpaperdesign_app_canvases", json!({}))
            .await
            .unwrap(),
        listed
    );
    a.cancel().await.unwrap();
}

/// Launch refusals: conflicting flags, an unexpanded placeholder, a relative or dotted path,
/// a missing folder, a link, the package root and the package cache. Each exits non-zero
/// before serving anything and creates nothing.
#[tokio::test]
async fn unusable_library_folders_are_refused_at_launch() {
    let t = tmp();
    let home = t.0.join("home");
    let real = t.0.join("data");
    let package = t.0.join("package");
    let cache = t.0.join("codex/plugins/cache/x");
    let fallback = package.join(".plugin-data");
    for d in [&home, &real, &fallback, &cache] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::os::unix::fs::symlink(&real, t.0.join("link")).unwrap();
    let bin = env!("CARGO_BIN_EXE_openpaperdesign-mcp");
    let run = |args: &[&str], plugin_root: Option<&Path>| {
        let mut cmd = std::process::Command::new(bin);
        cmd.current_dir(&home)
            .env("HOME", &home)
            .env_remove("PLUGIN_ROOT");
        if let Some(p) = plugin_root {
            cmd.env("PLUGIN_ROOT", p);
        }
        let out = cmd.args(args).arg("doctor").output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let real_s = real.to_str().unwrap();
    for (args, plugin_root, want) in [
        (
            vec!["--app-library", real_s, "--codex-context"],
            None,
            "cannot be repeated or combined",
        ),
        (
            vec!["--app-library", real_s, "--app-library", real_s],
            None,
            "cannot be repeated",
        ),
        (vec!["--app-library", "${PLUGIN_DATA}"], None, "placeholder"),
        (vec!["--app-library", "data"], None, "plain absolute path"),
        (
            vec!["--app-library", "/tmp/../tmp"],
            None,
            "plain absolute path",
        ),
        (vec!["--app-library", "/"], None, "filesystem root"),
        (
            vec!["--app-library", "--workspace", real_s],
            None,
            "cannot be repeated or combined",
        ),
    ] {
        let (code, err) = run(&args, plugin_root);
        assert_eq!(code, Some(2), "{args:?}: {err}");
        assert!(err.contains(want), "{args:?}: {err}");
    }
    let missing = t.0.join("missing");
    let link = t.0.join("link");
    for (dir, plugin_root, want) in [
        (&missing, None, "No such file"),
        (&link, None, "not a link"),
        (&home, None, "home folder"),
        (&fallback, Some(package.as_path()), "PLUGIN_ROOT"),
        (&fallback, None, ".plugin-data"),
        (&cache, None, "plugins/cache"),
    ] {
        let (code, err) = run(&["--app-library", dir.to_str().unwrap()], plugin_root);
        assert_eq!(code, Some(2), "{dir:?}: {err}");
        assert!(err.contains(want), "{dir:?}: {err}");
    }
    assert!(!missing.exists());
    assert!(
        std::fs::read_dir(&real).unwrap().next().is_none(),
        "nothing created"
    );
    // The usable folder passes doctor's library check (the editor may not be built here).
    let mut cmd = std::process::Command::new(bin);
    let out = cmd
        .current_dir(&home)
        .env("HOME", &home)
        .env_remove("PLUGIN_ROOT")
        .args(["--app-library", real_s, "doctor"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["mode"], "app-library");
    assert_eq!(report["library_usable"], true);
    assert!(
        std::fs::read_dir(&real).unwrap().next().is_none(),
        "doctor creates nothing"
    );
}
