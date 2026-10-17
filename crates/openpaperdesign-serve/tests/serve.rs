//! API behavior of openpaperdesign-serve, driven in-process (no sockets) through the router.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use openpaperdesign_serve::{ABSENT, Config, Server, etag, router};
use paper_rs::editor::shortcuts::Action;
use paper_rs::editor::{Button, Editor, InputEvent, Tool};
use paper_rs::geom::v2;
use paper_rs::project::format::{page_from_json, page_to_json};
use paper_rs::project::{PAGES_DIR, Project, Store};
use paper_rs::session::Session;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const PORT: u16 = 41234;
const HOST: &str = "127.0.0.1:41234";
const TOKEN: &str = "test-token-0123456789abcdef";

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp() -> Tmp {
    let d = std::env::temp_dir().join(format!("opd-serve-{:016x}", fastrand_u64()));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn fastrand_u64() -> u64 {
    // Unique enough for temp dirs without another dependency.
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}

/// A project with one page holding one rectangle drawn through the upstream editor.
fn project(root: &Path) -> (Project, String) {
    let store = Store::open(root).unwrap();
    let (p, page) = store.create("Web test").unwrap();
    let mut ed = Editor::default();
    ed.open_pages(vec![page], None);
    ed.run(Action::Tool(Tool::Rect));
    for ev in [
        InputEvent::PointerDown {
            pos: v2(100.0, 100.0),
            button: Button::Primary,
            pressure: None,
            time_ms: 0.0,
        },
        InputEvent::PointerMove {
            pos: v2(220.0, 180.0),
            pressure: None,
            time_ms: 16.0,
        },
        InputEvent::PointerUp {
            pos: v2(220.0, 180.0),
            button: Button::Primary,
            time_ms: 32.0,
        },
    ] {
        ed.handle(ev);
    }
    assert_eq!(ed.doc.nodes.len(), 1, "editor drew a rectangle");
    p.save_page(&ed.page_data(0)).unwrap();
    let id = p.manifest.pages[0].id.clone();
    (p, id)
}

fn server(dir: &Path) -> Arc<Server> {
    Server::open(Config {
        project: dir.to_path_buf(),
        // Beside the project, never inside it (that is refused).
        web_dir: dir.parent().unwrap().join("no-bundle"),
        port: PORT,
        token: TOKEN.into(),
    })
    .unwrap()
}

fn req(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
}

async fn send(s: &Arc<Server>, r: Request<Body>) -> (StatusCode, Option<String>, String) {
    let res = router(s.clone()).oneshot(r).await.unwrap();
    let status = res.status();
    let tag = res
        .headers()
        .get(header::ETAG)
        .map(|v| v.to_str().unwrap().to_string());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, tag, String::from_utf8_lossy(&body).into_owned())
}

async fn get_page(s: &Arc<Server>, id: &str) -> (StatusCode, Option<String>, String) {
    send(
        s,
        req("GET", &format!("/api/pages/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn put_page(
    s: &Arc<Server>,
    id: &str,
    if_match: Option<&str>,
    body: String,
) -> (StatusCode, Option<String>, String) {
    let mut r = req("PUT", &format!("/api/pages/{id}"));
    if let Some(m) = if_match {
        r = r.header(header::IF_MATCH, m);
    }
    send(s, r.body(Body::from(body)).unwrap()).await
}

/// The page file with its only node moved by (dx, dy), like a browser drag would save.
fn moved(page_json: &str, dx: f64, dy: f64) -> String {
    let mut page = page_from_json(page_json).unwrap();
    let n = page.doc.nodes.values_mut().next().unwrap();
    n.rect.x += dx;
    n.rect.y += dy;
    page_to_json(&page)
}

fn file(p: &Project, id: &str) -> Vec<u8> {
    std::fs::read(p.page_path(id)).unwrap()
}

#[tokio::test]
async fn save_with_current_revision_then_stale_revision_is_refused() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let s = server(&p.dir);

    let (st, tag, body) = get_page(&s, &id).await;
    assert_eq!(st, StatusCode::OK);
    let tag = tag.unwrap();
    assert_eq!(
        tag,
        etag(&file(&p, &id)),
        "revision is the on-disk file hash"
    );

    let (st, _, _) = put_page(&s, &id, None, moved(&body, 10.0, 0.0)).await;
    assert_eq!(st, StatusCode::PRECONDITION_REQUIRED);

    let (st, new_tag, _) = put_page(&s, &id, Some(&tag), moved(&body, 40.0, 25.0)).await;
    assert_eq!(st, StatusCode::OK);
    let new_tag = new_tag.unwrap();
    assert_ne!(new_tag, tag);
    let saved = file(&p, &id);
    assert_eq!(new_tag, etag(&saved));

    // A second writer still holding the old revision must not overwrite.
    let (st, current, _) = put_page(&s, &id, Some(&tag), moved(&body, -500.0, 0.0)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(current.as_deref(), Some(new_tag.as_str()));
    assert_eq!(file(&p, &id), saved, "file unchanged after 409");
}

#[tokio::test]
async fn change_made_outside_the_server_makes_the_next_save_conflict() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let s = server(&p.dir);
    let (_, tag, body) = get_page(&s, &id).await;

    // E.g. a running native paper-rs autosaving the same page.
    let outside = moved(&body, 0.0, 300.0);
    std::fs::write(p.page_path(&id), &outside).unwrap();

    let (st, current, _) = put_page(&s, &id, tag.as_deref(), moved(&body, 5.0, 5.0)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(current.unwrap(), etag(outside.as_bytes()));
    assert_eq!(file(&p, &id), outside.as_bytes(), "outside change kept");
}

#[tokio::test]
async fn bad_page_ids_and_paths_are_rejected() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let s = server(&p.dir);
    let before = file(&p, &id);
    let (_, tag, body) = get_page(&s, &id).await;
    let tag = tag.unwrap();

    for (bad, want) in [
        ("0123456789abcdef", StatusCode::NOT_FOUND), // well-formed, not in the manifest
        ("..", StatusCode::BAD_REQUEST),
        ("%2e%2e%2fproject", StatusCode::BAD_REQUEST),
        ("..%2F..%2Fetc%2Fpasswd", StatusCode::BAD_REQUEST),
        ("%2Ftmp%2Fx", StatusCode::BAD_REQUEST),
        ("a.json", StatusCode::BAD_REQUEST),
    ] {
        let (st, _, _) = get_page(&s, bad).await;
        assert_eq!(st, want, "GET {bad}");
        let (st, _, _) = put_page(&s, bad, Some(&tag), body.clone()).await;
        assert_eq!(st, want, "PUT {bad}");
    }
    let (st, _, _) = get_page(&s, &"a".repeat(65)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Extra path segments don't match a route at all.
    let (st, _, _) = get_page(&s, &format!("{id}/../../project.json")).await;
    assert_ne!(st, StatusCode::OK);

    // The body can't redirect the write to another page id either.
    let mut other = page_from_json(&body).unwrap();
    other.id = "ffffffffffffffff".into();
    let (st, _, _) = put_page(&s, &id, Some(&tag), page_to_json(&other)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Nor can it be something that isn't a valid page.
    let (st, _, _) = put_page(&s, &id, Some(&tag), "{\"format\":\"nope\"}".into()).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(file(&p, &id), before);
    let names: Vec<_> = std::fs::read_dir(p.dir.join(PAGES_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "no other files written: {names:?}");
}

#[tokio::test]
async fn host_origin_and_token_are_enforced() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let s = server(&p.dir);
    let uri = format!("/api/pages/{id}");
    let base = || Request::builder().method("GET").uri(&uri);

    let cases = [
        (
            "foreign host (DNS rebinding)",
            base()
                .header(header::HOST, "evil.example:41234")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
            StatusCode::FORBIDDEN,
        ),
        (
            "right host, other port",
            base()
                .header(header::HOST, "127.0.0.1:1")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
            StatusCode::FORBIDDEN,
        ),
        (
            "cross origin",
            req("GET", &uri).header(header::ORIGIN, "https://evil.example"),
            StatusCode::FORBIDDEN,
        ),
        (
            "null origin",
            req("GET", &uri).header(header::ORIGIN, "null"),
            StatusCode::FORBIDDEN,
        ),
        (
            "cross-site fetch metadata",
            req("GET", &uri).header("sec-fetch-site", "cross-site"),
            StatusCode::FORBIDDEN,
        ),
        (
            "no token",
            base().header(header::HOST, HOST),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "wrong token",
            base()
                .header(header::HOST, HOST)
                .header(header::AUTHORIZATION, "Bearer test-token-0123456789abcdeX"),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "same origin",
            req("GET", &uri)
                .header(header::ORIGIN, "http://127.0.0.1:41234")
                .header("sec-fetch-site", "same-origin"),
            StatusCode::OK,
        ),
        (
            "localhost alias",
            base()
                .header(header::HOST, "localhost:41234")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
            StatusCode::OK,
        ),
    ];
    for (what, r, want) in cases {
        let (st, _, _) = send(&s, r.body(Body::empty()).unwrap()).await;
        assert_eq!(st, want, "{what}");
    }

    // A refused write leaves the file alone.
    let before = file(&p, &id);
    let r = Request::builder()
        .method("PUT")
        .uri(&uri)
        .header(header::HOST, HOST)
        .header(header::IF_MATCH, etag(&before))
        .body(Body::from(moved(
            &String::from_utf8_lossy(&before),
            1.0,
            1.0,
        )))
        .unwrap();
    assert_eq!(send(&s, r).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(file(&p, &id), before);

    // The bundle itself needs no token (it holds no project data), but the Host check holds.
    let r = Request::builder()
        .uri("/")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&s, r).await.0,
        StatusCode::NOT_FOUND,
        "no bundle built here"
    );
    let r = Request::builder()
        .uri("/")
        .header(header::HOST, "evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&s, r).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn only_existing_projects_are_served() {
    let t = tmp();
    let empty = t.0.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let open = |dir: &Path| {
        Server::open(Config {
            project: dir.to_path_buf(),
            web_dir: dir.to_path_buf(),
            port: PORT,
            token: TOKEN.into(),
        })
    };
    assert!(open(&empty).is_err(), "folder without project.json");
    assert!(!empty.join("project.json").exists(), "nothing created");
    let bad = t.0.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("project.json"), "{\"format\":\"something-else\"}").unwrap();
    assert!(open(&bad).is_err(), "foreign manifest");
    assert!(open(&t.0.join("missing")).is_err(), "missing folder");
}

#[tokio::test]
async fn saved_page_survives_restart_and_opens_in_native_paper_rs() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let original = page_from_json(&String::from_utf8(file(&p, &id)).unwrap()).unwrap();
    let node = *original.doc.nodes.keys().next().unwrap();
    let start = original.doc.nodes[&node].rect;

    let s1 = server(&p.dir);
    let (_, tag, body) = get_page(&s1, &id).await;
    let (st, saved_tag, _) = put_page(&s1, &id, tag.as_deref(), moved(&body, 40.0, 25.0)).await;
    assert_eq!(st, StatusCode::OK);
    drop(s1);

    // "Restart": a fresh server over the same folder serves the same bytes and revision.
    let s2 = server(&p.dir);
    let (st, tag2, body2) = get_page(&s2, &id).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(tag2, saved_tag);
    assert_eq!(body2.as_bytes(), file(&p, &id));

    // The native app's own open path sees the moved node.
    let mut ed = Editor::default();
    let mut session = Session::new(Store::open(&t.0).unwrap());
    session.open(&mut ed, &p.dir).unwrap();
    let r = ed.doc.nodes[&node].rect;
    assert_eq!((r.x, r.y), (start.x + 40.0, start.y + 25.0));
    assert_eq!((r.w, r.h), (start.w, start.h));
    assert_eq!(ed.page_name(), p.manifest.pages[0].name);
}

#[tokio::test]
async fn listed_page_without_a_file_loads_empty_and_can_be_created() {
    let t = tmp();
    let (p, id) = project(&t.0);
    std::fs::remove_file(p.page_path(&id)).unwrap();
    let s = server(&p.dir);
    let (st, tag, body) = get_page(&s, &id).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(tag.as_deref(), Some(ABSENT));
    assert_eq!(page_from_json(&body).unwrap().doc.nodes.len(), 0);
    let (st, _, _) = put_page(&s, &id, Some(ABSENT), body).await;
    assert_eq!(st, StatusCode::OK);
    assert!(p.page_path(&id).exists());
}

#[tokio::test]
async fn project_lists_pages() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let s = server(&p.dir);
    let (st, _, body) = send(&s, req("GET", "/api/project").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["name"], "Web test");
    assert_eq!(v["pages"][0]["id"], id.as_str());
    assert!(
        !body.contains(&p.dir.display().to_string()),
        "no host paths"
    );
}

// ------------------------------------------------------------------ links out of the project

/// A sibling folder outside the project with a copy of the page and a stale-temp-looking
/// sentinel that upstream's cleanup would delete if it ever ran there.
#[cfg(unix)]
fn outside(t: &Tmp, p: &Project, id: &str) -> PathBuf {
    let out = t.0.join("outside");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::copy(p.page_path(id), out.join(format!("{id}.json"))).unwrap();
    std::fs::write(out.join(".keep.tmp"), "sentinel").unwrap();
    std::fs::write(out.join("secret.json"), "{\"secret\": \"outside bytes\"}").unwrap();
    out
}

#[cfg(unix)]
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_pages_dir_is_refused_before_cleanup_and_after_open() {
    use std::os::unix::fs::symlink;
    let t = tmp();
    let (p, id) = project(&t.0);
    let out = outside(&t, &p, &id);
    let before = snapshot(&out);
    let pages = p.dir.join(PAGES_DIR);

    // Installed before the server starts: refused, and upstream cleanup never runs there.
    std::fs::rename(&pages, t.0.join("pages-real")).unwrap();
    symlink(&out, &pages).unwrap();
    let err = Server::open(Config {
        project: p.dir.clone(),
        web_dir: t.0.join("no-bundle"),
        port: PORT,
        token: TOKEN.into(),
    })
    .err()
    .expect("refused");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert!(
        out.join(".keep.tmp").exists(),
        "outside sentinel not deleted"
    );
    assert_eq!(snapshot(&out), before);

    // Installed after the server started: every read and write is refused.
    std::fs::remove_file(&pages).unwrap();
    std::fs::rename(t.0.join("pages-real"), &pages).unwrap();
    let s = server(&p.dir);
    let (_, tag, body) = get_page(&s, &id).await;
    std::fs::rename(&pages, t.0.join("pages-real")).unwrap();
    symlink(&out, &pages).unwrap();
    assert_eq!(get_page(&s, &id).await.0, StatusCode::FORBIDDEN);
    let (st, _, _) = put_page(&s, &id, tag.as_deref(), moved(&body, 9.0, 9.0)).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _, _) = send(&s, req("GET", "/api/project").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(snapshot(&out), before, "outside folder untouched");
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_or_hard_linked_page_file_is_refused() {
    use std::os::unix::fs::symlink;
    let t = tmp();
    let (p, id) = project(&t.0);
    let out = outside(&t, &p, &id);
    let s = server(&p.dir);
    let (_, tag, body) = get_page(&s, &id).await;
    let page = p.page_path(&id);

    for link in ["symlink", "hard link"] {
        std::fs::remove_file(&page).unwrap();
        if link == "symlink" {
            symlink(out.join("secret.json"), &page).unwrap();
        } else {
            std::fs::hard_link(out.join("secret.json"), &page).unwrap();
        }
        let before = snapshot(&out);
        let (st, _, got) = get_page(&s, &id).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{link}");
        assert!(
            !got.contains("outside bytes"),
            "{link}: no outside bytes served"
        );
        let (st, _, _) = put_page(&s, &id, tag.as_deref(), moved(&body, 9.0, 9.0)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{link}");
        let current = etag(&std::fs::read(out.join("secret.json")).unwrap());
        let (st, _, _) = put_page(&s, &id, Some(&current), moved(&body, 9.0, 9.0)).await;
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "{link}, even with its current revision"
        );
        assert_eq!(snapshot(&out), before, "{link}: outside file untouched");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_manifest_is_refused() {
    use std::os::unix::fs::symlink;
    let t = tmp();
    let (p, id) = project(&t.0);
    let out = outside(&t, &p, &id);
    let manifest = p.dir.join("project.json");
    std::fs::copy(&manifest, out.join("project.json")).unwrap();
    let open = || {
        Server::open(Config {
            project: p.dir.clone(),
            web_dir: t.0.join("no-bundle"),
            port: PORT,
            token: TOKEN.into(),
        })
    };

    // After open.
    let s = open().unwrap();
    std::fs::rename(&manifest, t.0.join("project-real.json")).unwrap();
    symlink(out.join("project.json"), &manifest).unwrap();
    let (st, _, _) = send(&s, req("GET", "/api/project").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(get_page(&s, &id).await.0, StatusCode::FORBIDDEN);
    // Before open.
    assert!(open().is_err());
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap()
}

/// A bundle folder beside the project with a recognizable index.html.
fn bundle_dir(t: &Tmp, name: &str, index: &str) -> PathBuf {
    let web = t.0.join(name);
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("index.html"), index).unwrap();
    std::fs::write(web.join("app.js"), "// app").unwrap();
    web
}

fn open_with_web(
    p: &Project,
    web: PathBuf,
) -> Result<Arc<Server>, openpaperdesign_serve::OpenError> {
    Server::open(Config {
        project: p.dir.clone(),
        web_dir: web,
        port: PORT,
        token: TOKEN.into(),
    })
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_bundle_file_is_refused_at_startup() {
    use std::os::unix::fs::symlink;
    let t = tmp();
    let (p, id) = project(&t.0);
    let out = outside(&t, &p, &id);
    let web = bundle_dir(&t, "web", "<!doctype html>ok");
    std::fs::remove_file(web.join("app.js")).unwrap();
    symlink(out.join("secret.json"), web.join("app.js")).unwrap();
    let err = open_with_web(&p, web).err().expect("refused");
    assert!(err.to_string().contains("symbolic link"), "{err}");
}

/// The bundle is read once at startup: replacing its folder (or a file in it) afterwards
/// can't make the server send other bytes.
#[cfg(unix)]
#[tokio::test]
async fn bundle_is_served_from_memory_after_its_folder_is_replaced() {
    use std::os::unix::fs::symlink;
    let t = tmp();
    let (p, _) = project(&t.0);
    let web = bundle_dir(&t, "web", "<!doctype html>original");
    let s = open_with_web(&p, web.clone()).unwrap();
    assert_eq!(send(&s, get("/")).await.2, "<!doctype html>original");

    // Reviewer case (b): the whole folder replaced by a symlink to an outside folder.
    let outside_web = bundle_dir(&t, "outside-web", "OUTSIDE SENTINEL");
    std::fs::write(
        outside_web.join("openpaperdesign_web.js"),
        "OUTSIDE SENTINEL js",
    )
    .unwrap();
    std::fs::rename(&web, t.0.join("web-away")).unwrap();
    symlink(&outside_web, &web).unwrap();
    let (st, _, body) = send(&s, get("/")).await;
    assert_eq!(
        (st, body.as_str()),
        (StatusCode::OK, "<!doctype html>original"),
        "outside bytes must never be served"
    );
    let (st, _, body) = send(&s, get("/app.js")).await;
    assert_eq!((st, body.as_str()), (StatusCode::OK, "// app"));
    // A file that wasn't there at startup stays absent even though the outside folder has it.
    let (st, _, body) = send(&s, get("/openpaperdesign_web.js")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(!body.contains("SENTINEL"));

    // The real folder back, with a file rewritten in place.
    std::fs::remove_file(&web).unwrap();
    std::fs::rename(t.0.join("web-away"), &web).unwrap();
    std::fs::write(web.join("index.html"), "rewritten").unwrap();
    assert_eq!(send(&s, get("/")).await.2, "<!doctype html>original");
}

/// The host opens the editor URL as a top-level page from its own site (a link, or a redirect
/// to the loopback URL): the browser marks that `GET /` as a cross-site or same-site document
/// navigation, and it gets the static bootstrap page. Nothing else from another site does: the
/// same navigation into a frame, fetches and subresources, other paths and methods, the API and
/// MCP (with the token, too), partial or unknown fetch metadata, a foreign Origin or Host. The
/// page's own later requests are same-origin and work as before. No request changes a file.
#[tokio::test]
async fn top_level_navigation_from_another_site_gets_only_the_static_page() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let s = open_with_web(&p, bundle_dir(&t, "web", "<!doctype html>editor page")).unwrap();
    let files = || {
        (
            file(&p, &id),
            std::fs::read(p.dir.join("project.json")).unwrap(),
        )
    };
    let before = files();
    let fetch = |path: &str, site: &str, mode: &str, dest: &str| {
        Request::builder()
            .uri(path)
            .header(header::HOST, HOST)
            .header("sec-fetch-site", site)
            .header("sec-fetch-mode", mode)
            .header("sec-fetch-dest", dest)
    };
    for (what, r) in [
        (
            "cross-site link",
            fetch("/", "cross-site", "navigate", "document"),
        ),
        (
            "same-site link",
            fetch("/", "same-site", "navigate", "document"),
        ),
        (
            "cross-site, with the page's query",
            fetch("/?backend=webgl", "cross-site", "navigate", "document"),
        ),
        ("typed URL", fetch("/", "none", "navigate", "document")),
        (
            "the page's own script",
            fetch("/app.js", "same-origin", "no-cors", "script"),
        ),
    ] {
        let res = router(s.clone())
            .oneshot(r.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{what}");
        // Admitted, the page keeps every protection: no framing, no caching, no referrer,
        // and nothing that would let another site read it.
        let h = res.headers();
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{what}: {csp}");
        assert_eq!(h[header::CACHE_CONTROL], "no-store", "{what}");
        assert_eq!(h["referrer-policy"], "no-referrer", "{what}");
        assert!(
            !h.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            "{what}: no CORS"
        );
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert!(
            body == "<!doctype html>editor page" || body == "// app",
            "{what}"
        );
    }
    // The page's API calls still need the token, and only the right one.
    let api = format!("/api/pages/{id}");
    let bearer = format!("Bearer {TOKEN}");
    let same_origin = || {
        Request::builder()
            .uri(&api)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, "http://127.0.0.1:41234")
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-dest", "empty")
    };
    for (what, r, want) in [
        ("no token", same_origin(), StatusCode::UNAUTHORIZED),
        (
            "a wrong token",
            same_origin().header(header::AUTHORIZATION, "Bearer wrong"),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "the token",
            same_origin().header(header::AUTHORIZATION, &bearer),
            StatusCode::OK,
        ),
    ] {
        let (st, _, body) = send(&s, r.body(Body::empty()).unwrap()).await;
        assert_eq!(st, want, "{what}");
        assert_eq!(
            body.as_bytes() == file(&p, &id).as_slice(),
            want == StatusCode::OK,
            "{what}: the page only with the token"
        );
    }
    for (what, r) in [
        (
            "into a frame",
            fetch("/", "cross-site", "navigate", "iframe"),
        ),
        (
            "into a legacy frame",
            fetch("/", "cross-site", "navigate", "frame"),
        ),
        (
            "a CORS fetch of the page",
            fetch("/", "cross-site", "cors", "empty"),
        ),
        (
            "a no-cors fetch of the page",
            fetch("/", "same-site", "no-cors", "empty"),
        ),
        (
            "a script from another site",
            fetch("/app.js", "cross-site", "no-cors", "script"),
        ),
        (
            "a navigation to a bundle file",
            fetch("/app.js", "cross-site", "navigate", "document"),
        ),
        (
            "a navigation to the wasm",
            fetch(
                "/openpaperdesign_web_bg.wasm",
                "cross-site",
                "navigate",
                "document",
            ),
        ),
        (
            "a HEAD navigation",
            fetch("/", "cross-site", "navigate", "document").method("HEAD"),
        ),
        (
            "a navigation to the API, with the token",
            fetch(&api, "cross-site", "navigate", "document")
                .header(header::AUTHORIZATION, &bearer),
        ),
        (
            "a navigation to MCP, with the token",
            fetch("/mcp", "cross-site", "navigate", "document")
                .header(header::AUTHORIZATION, &bearer),
        ),
        (
            "a form POST navigation",
            fetch("/", "cross-site", "navigate", "document").method("POST"),
        ),
        (
            "no mode",
            Request::builder()
                .uri("/")
                .header(header::HOST, HOST)
                .header("sec-fetch-site", "cross-site")
                .header("sec-fetch-dest", "document"),
        ),
        (
            "no destination",
            Request::builder()
                .uri("/")
                .header(header::HOST, HOST)
                .header("sec-fetch-site", "cross-site")
                .header("sec-fetch-mode", "navigate"),
        ),
        (
            "an unknown site value",
            fetch("/", "elsewhere", "navigate", "document"),
        ),
        (
            "a mode that isn't the browser's",
            fetch("/", "cross-site", "Navigate", "document"),
        ),
        (
            "a foreign Origin",
            fetch("/", "cross-site", "navigate", "document")
                .header(header::ORIGIN, "https://evil.example"),
        ),
        (
            "a null Origin",
            fetch("/", "cross-site", "navigate", "document").header(header::ORIGIN, "null"),
        ),
        (
            "a foreign Host (DNS rebinding)",
            Request::builder()
                .uri("/")
                .header(header::HOST, "evil.example:41234")
                .header("sec-fetch-site", "cross-site")
                .header("sec-fetch-mode", "navigate")
                .header("sec-fetch-dest", "document"),
        ),
    ] {
        let (st, _, body) = send(&s, r.body(Body::empty()).unwrap()).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{what}");
        assert!(
            !body.contains("editor page") && !body.contains("// app"),
            "{what}"
        );
    }
    assert_eq!(files(), before, "no file changed");
    // A retired server serves no page, even to an admitted navigation.
    s.retire().await;
    let (st, _, body) = send(
        &s,
        fetch("/", "cross-site", "navigate", "document")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!body.contains("editor page"));
    assert_eq!(files(), before, "no file changed");
}

/// A bundle folder inside the project would follow the project root if that were replaced;
/// it is refused at startup, as is the project folder itself.
#[cfg(unix)]
#[tokio::test]
async fn web_dir_inside_the_project_is_refused() {
    let t = tmp();
    let (p, _) = project(&t.0);
    let inside = p.dir.join("web");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("index.html"), "x").unwrap();
    for (what, web, why) in [
        ("inside", inside, "inside the project"),
        ("the project folder", p.dir.clone(), "inside the project"),
        (
            "missing, via ..",
            t.0.join("elsewhere/..")
                .join(p.dir.file_name().unwrap())
                .join("web"),
            "may not contain `..`",
        ),
    ] {
        let err = open_with_web(&p, web)
            .err()
            .unwrap_or_else(|| panic!("{what}: refused"));
        assert!(err.to_string().contains(why), "{what}: {err}");
    }
    // A missing folder without `..` serves nothing ("build it first"); nothing is guessed.
    let s = open_with_web(&p, p.dir.join("not-built")).unwrap();
    assert_eq!(s.bundle_files(), 0);
    assert_eq!(send(&s, get("/")).await.0, StatusCode::NOT_FOUND);
}

/// Reviewer case: `alias` links to a folder inside the project, and the missing
/// `alias/../not-built` must not be resolved lexically to an unrelated sibling bundle.
#[cfg(unix)]
#[tokio::test]
async fn missing_web_dir_with_dotdot_after_a_symlink_is_not_guessed() {
    use std::os::unix::fs::symlink;
    let t = tmp();
    let (p, _) = project(&t.0);
    std::fs::create_dir_all(p.dir.join("sub")).unwrap();
    let alias = t.0.join("alias");
    symlink(p.dir.join("sub"), &alias).unwrap();
    // What a lexical `..` would land on: a sibling bundle nobody asked for.
    bundle_dir(&t, "not-built", "UNREQUESTED SIBLING BUNDLE");
    match open_with_web(&p, alias.join("..").join("not-built")) {
        Err(e) => assert!(e.to_string().contains("may not contain `..`"), "{e}"),
        Ok(s) => {
            let (_, _, body) = send(&s, get("/")).await;
            panic!("must be refused, but served: {body}");
        }
    }
}

// ------------------------------------------------------------------ headers

#[tokio::test]
async fn security_headers_are_on_every_response() {
    let t = tmp();
    let (p, id) = project(&t.0);
    let s = server(&p.dir);
    let (_, tag, body) = get_page(&s, &id).await;
    let stale = moved(&body, 1.0, 1.0);
    put_page(&s, &id, tag.as_deref(), stale.clone()).await;
    let uri = format!("/api/pages/{id}");
    let cases = [
        (
            "ok",
            req("GET", &uri).body(Body::empty()).unwrap(),
            StatusCode::OK,
        ),
        (
            "no token",
            Request::builder()
                .uri(&uri)
                .header(header::HOST, HOST)
                .body(Body::empty())
                .unwrap(),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "foreign host",
            Request::builder()
                .uri(&uri)
                .header(header::HOST, "evil.example")
                .body(Body::empty())
                .unwrap(),
            StatusCode::FORBIDDEN,
        ),
        (
            "unknown path",
            req("GET", "/nope").body(Body::empty()).unwrap(),
            StatusCode::NOT_FOUND,
        ),
        (
            "stale write",
            req("PUT", &uri)
                .header(header::IF_MATCH, tag.as_deref().unwrap())
                // Based on an older revision: refused (nothing merges).
                .body(Body::from(moved(&body, 9.0, 9.0)))
                .unwrap(),
            StatusCode::CONFLICT,
        ),
        (
            "body too large",
            req("PUT", &uri)
                .header(header::IF_MATCH, "\"x\"")
                .body(Body::from(vec![b' '; (64 << 20) + 1]))
                .unwrap(),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ];
    for (what, r, want) in cases {
        let res = router(s.clone()).oneshot(r).await.unwrap();
        assert_eq!(res.status(), want, "{what}");
        let h = res.headers();
        assert_eq!(h[header::CACHE_CONTROL], "no-store", "{what}");
        assert_eq!(h["x-content-type-options"], "nosniff", "{what}");
        assert_eq!(h["referrer-policy"], "no-referrer", "{what}");
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{what}: {csp}");
    }
}

// ------------------------------------------------------------------ project root replaced

/// A byte-identical copy of the project (same page ids and revisions) somewhere else.
#[cfg(unix)]
fn copy_project(from: &Path, to: &Path) {
    std::fs::create_dir_all(to.join(PAGES_DIR)).unwrap();
    std::fs::copy(from.join("project.json"), to.join("project.json")).unwrap();
    for e in std::fs::read_dir(from.join(PAGES_DIR)).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join(PAGES_DIR).join(e.file_name())).unwrap();
    }
}

/// After the server started and before any request, the project folder is renamed away and
/// replaced: by a symlink to an outside copy, by a different real directory, or by swapping
/// an ancestor folder. Every read and write must be refused, and the outside copy untouched.
#[cfg(unix)]
#[tokio::test]
async fn replaced_project_root_is_refused() {
    use std::os::unix::fs::symlink;
    let t = tmp();
    let parent = t.0.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    let (p, id) = project(&parent);
    let body = String::from_utf8(file(&p, &id)).unwrap();
    // The copy has the same bytes, so the revision a client holds would match it.
    let tag = etag(body.as_bytes());

    let refused = async |s: &Arc<Server>, outside: &Path, what: &str| {
        let before = snapshot(&outside.join(PAGES_DIR));
        let manifest = std::fs::read(outside.join("project.json")).unwrap();
        assert_eq!(
            get_page(s, &id).await.0,
            StatusCode::FORBIDDEN,
            "{what}: GET page"
        );
        let (st, _, _) = send(s, req("GET", "/api/project").body(Body::empty()).unwrap()).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{what}: GET project");
        let (st, _, _) = put_page(s, &id, Some(&tag), moved(&body, 777.0, 0.0)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{what}: PUT");
        assert_eq!(
            snapshot(&outside.join(PAGES_DIR)),
            before,
            "{what}: outside pages"
        );
        assert_eq!(
            std::fs::read(outside.join("project.json")).unwrap(),
            manifest,
            "{what}: outside manifest"
        );
    };

    // 1. Root renamed away and replaced by a symlink to an outside project.
    let outside = t.0.join("outside-project");
    copy_project(&p.dir, &outside);
    let s = server(&p.dir);
    let away = t.0.join("moved-away");
    std::fs::rename(&p.dir, &away).unwrap();
    symlink(&outside, &p.dir).unwrap();
    refused(&s, &outside, "symlinked root").await;
    // Persistent: still refused on later requests.
    refused(&s, &outside, "symlinked root, again").await;

    // 2. Root replaced by a different real directory with the same content.
    std::fs::remove_file(&p.dir).unwrap();
    copy_project(&away, &p.dir);
    refused(&s, &p.dir, "replaced root directory").await;
    std::fs::remove_dir_all(&p.dir).unwrap();
    std::fs::rename(&away, &p.dir).unwrap();

    // 3. An ancestor swapped for a symlink to a folder holding a copy at the same name.
    let s = server(&p.dir);
    let other_parent = t.0.join("other-parent");
    copy_project(&p.dir, &other_parent.join(p.dir.file_name().unwrap()));
    std::fs::rename(&parent, t.0.join("parent-away")).unwrap();
    symlink(&other_parent, &parent).unwrap();
    refused(
        &s,
        &other_parent.join(p.dir.file_name().unwrap()),
        "swapped ancestor",
    )
    .await;

    // Back in place, the original root serves again.
    std::fs::remove_file(&parent).unwrap();
    std::fs::rename(t.0.join("parent-away"), &parent).unwrap();
    assert_eq!(get_page(&s, &id).await.0, StatusCode::OK);
}
