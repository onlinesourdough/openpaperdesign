//! The home view's direct actions over actual child-process stdio. Workspace context is
//! synthetic (shaped from the operator's Codex receipts) and the "app" metadata is what the
//! host was observed to send from an app view. Not evidence of installed Codex behaviour.
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ContentBlock, CustomRequest,
    CustomResult, ErrorData, Implementation, ProtocolVersion, RequestMetaObject,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[path = "support/codex_context.rs"]
mod context;

/// A synthetic host: advertises the OpenAI form extension and answers the folder picker.
#[derive(Clone)]
struct Host {
    pick: Value,
}
impl ClientHandler for Host {
    fn get_info(&self) -> ClientConfig {
        let mut caps = ClientCapabilities::default();
        caps.extensions = Some(
            serde_json::from_value(json!({"openai/elicitation": {"form": {}}, "openai/form": {}}))
                .unwrap(),
        );
        ClientConfig::new(caps, Implementation::new("synthetic-host", "0"))
            .with_protocol_version(ProtocolVersion::V_2025_06_18)
    }
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleClient>,
    ) -> Result<CustomResult, ErrorData> {
        assert_eq!(request.method, "openai/elicitation/create");
        Ok(CustomResult(self.pick.clone()))
    }
}
type Client = RunningService<RoleClient, Host>;

struct Fixture {
    root: PathBuf,
    ws: PathBuf,
    other: PathBuf,
    plugin: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        use std::hash::{BuildHasher, RandomState};
        let root = std::env::temp_dir().join(format!(
            "opd-direct-ui-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let ws = root.join("workspace é");
        let other = root.join("other");
        let plugin = root.join("plugin");
        let home = root.join("home");
        for dir in [&ws, &other, &home, &plugin.join("skills/openpaperdesign")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(plugin.join("mcp.json"), "{}").unwrap();
        std::fs::write(plugin.join("skills/openpaperdesign/SKILL.md"), "fixture").unwrap();
        Self {
            root,
            ws,
            other,
            plugin,
            home,
        }
    }
    async fn launch(&self, args: &[&str], pick: Value) -> Client {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_openpaperdesign-mcp"));
        cmd.current_dir(&self.plugin)
            .env("HOME", &self.home)
            .args(args);
        Host { pick }
            .serve(rmcp::transport::TokioChildProcess::new(cmd).unwrap())
            .await
            .unwrap()
    }
    async fn host(&self) -> Client {
        self.launch(&["--codex-context"], json!({"action": "cancel"}))
            .await
    }
    fn root_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call(c: &Client, name: &str, args: Value, meta: RequestMetaObject) -> (bool, Value) {
    let mut p = CallToolRequestParams::new(name.to_string());
    p.arguments = args.as_object().cloned();
    p.meta = Some(meta);
    let r = tokio::time::timeout(std::time::Duration::from_secs(20), c.call_tool(p))
        .await
        .unwrap()
        .unwrap();
    let s = r
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let v = serde_json::from_str(&s).unwrap_or(Value::String(s));
    (r.is_error != Some(true), v)
}
async fn status(c: &Client, meta: RequestMetaObject) -> Value {
    let (ok, v) = call(c, "openpaperdesign_status", json!({}), meta).await;
    assert!(ok, "{v}");
    v
}
/// What the host was observed to send with an app view's call: no workspace context.
fn app() -> RequestMetaObject {
    json!({"thread_id": "t-1", "threadId": "t-1"})
        .as_object()
        .unwrap()
        .clone()
        .into()
}
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
fn editor_url(v: &Value) -> &str {
    let url = v["url"].as_str().unwrap();
    let (port, token) = url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|r| r.split_once("/#token="))
        .unwrap();
    assert!(port.parse::<u16>().is_ok() && token.len() == 64, "{url}");
    url
}
/// Whether this build has the browser editor. A test that needs it says so loudly when it
/// doesn't, and fails instead when `OPD_REQUIRE_EDITOR` is set (the required local run).
fn built_in(s: &Value) -> bool {
    let built = s["browser_editor"] == "built in";
    if !built {
        assert!(
            std::env::var_os("OPD_REQUIRE_EDITOR").is_none(),
            "OPD_REQUIRE_EDITOR is set but this build has no browser editor"
        );
        eprintln!("SKIPPED positive branch: this build has no browser editor");
    }
    built
}

const ACTIONS: [&str; 4] = [
    "openpaperdesign_ui_canvases",
    "openpaperdesign_ui_create",
    "openpaperdesign_ui_open",
    "openpaperdesign_ui_editor",
];
fn args_for(name: &str, root: &str) -> Value {
    match name {
        "openpaperdesign_ui_create" => json!({"root": root, "name": "Landing"}),
        "openpaperdesign_ui_open" => json!({"root": root, "project": "design/Landing"}),
        _ => json!({"root": root}),
    }
}

/// Without this call's own supported context, every direct action is refused, writes
/// nothing and pins nothing; argument or view-supplied context never counts.
#[tokio::test]
async fn direct_actions_without_supported_context_are_refused_and_pin_nothing() {
    let f = Fixture::new();
    let c = f.host().await;
    let w = context::writable(&f.ws);
    let mut custom = w.clone();
    custom.get_mut(context::KEY).unwrap()["permissionProfile"] =
        json!({"type": "external", "network": "restricted"});
    let mut malformed = w.clone();
    malformed.get_mut(context::KEY).unwrap()["sandboxCwd"] = json!(12);
    let before = tree(&f.root);
    for meta in [
        app(),
        RequestMetaObject::default(),
        context::readonly(&f.ws),
        custom,
        malformed,
        context::writable(&f.home), // the home folder is never a workspace
    ] {
        for name in ACTIONS {
            let (ok, v) = call(&c, name, args_for(name, f.root_str()), meta.clone()).await;
            assert!(!ok, "{name}: {v}");
            assert!(
                v.as_str().unwrap().contains("Nothing was read or changed"),
                "{name}: {v}"
            );
        }
        assert_eq!(status(&c, app()).await["workspace_state"], "unresolved");
    }
    // Context smuggled in arguments is an unexpected argument.
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_canvases",
        json!({"root": f.root_str(), "_meta": {"codex/sandbox-state-meta": {"sandboxCwd": context::uri(&f.ws)}}}),
        app(),
    )
    .await;
    assert!(
        !ok && v.as_str().unwrap().contains("unexpected argument"),
        "{v}"
    );
    assert_eq!(tree(&f.root), before, "no file changed");
    c.cancel().await.unwrap();
}

/// New, open, reopen and restart through the direct actions: one binding (shared with the
/// agent's calls in the same admitted workspace), canvases only under design/, and the
/// existing design files untouched.
#[tokio::test]
async fn direct_new_open_reopen_use_the_one_binding_and_keep_design_files() {
    let f = Fixture::new();
    // An existing design setup: direction, tokens and an asset.
    std::fs::write(f.ws.join("DESIGN.md"), "# Direction\nCalm, warm.\n").unwrap();
    std::fs::create_dir_all(f.ws.join("design")).unwrap();
    std::fs::write(f.ws.join("design/tokens.json"), "{\"ink\":\"#222\"}\n").unwrap();
    std::fs::create_dir_all(f.ws.join("assets")).unwrap();
    std::fs::write(f.ws.join("assets/logo.svg"), "<svg/>").unwrap();
    let kept = |p: &str| std::fs::read(f.ws.join(p)).unwrap();
    let (direction, tokens, logo) = (
        kept("DESIGN.md"),
        kept("design/tokens.json"),
        kept("assets/logo.svg"),
    );

    let c = f.host().await;
    let w = context::writable(&f.ws);
    let root = f.root_str();
    if !built_in(&status(&c, w.clone()).await) {
        let (ok, v) = call(
            &c,
            "openpaperdesign_ui_create",
            json!({"name": "Landing"}),
            w,
        )
        .await;
        assert!(
            !ok && v.as_str().unwrap().contains("without the browser editor"),
            "{v}"
        );
        assert!(!f.ws.join("design/Landing").exists());
        return;
    }

    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_canvases",
        json!({"root": root}),
        w.clone(),
    )
    .await;
    assert!(
        ok && v["canvases"] == json!([]) && v["design_present"] == true,
        "{v}"
    );

    // New: create, bind and the editor link, in one action.
    let (ok, created) = call(
        &c,
        "openpaperdesign_ui_create",
        json!({"root": root, "name": "Landing"}),
        w.clone(),
    )
    .await;
    assert!(ok, "{created}");
    assert_eq!(created["action"], "created");
    assert_eq!(created["project"], "design/Landing");
    assert_eq!(created["workspace"], root);
    assert_eq!(created["canvas"]["name"], "Landing");
    assert_eq!(created["canvas"]["pages"], 1);
    let url = editor_url(&created).to_string();
    let dir = f.ws.join("design/Landing");
    assert!(dir.join("project.json").is_file());
    assert_eq!(std::fs::read_dir(dir.join("pages")).unwrap().count(), 1);
    assert_eq!(
        (
            kept("DESIGN.md"),
            kept("design/tokens.json"),
            kept("assets/logo.svg")
        ),
        (direction.clone(), tokens.clone(), logo.clone())
    );
    assert!(!f.ws.join("design.md").exists(), "no second direction file");
    let s = status(&c, w.clone()).await;
    assert_eq!(s["bound_project"], dir.to_str().unwrap());
    assert_eq!(s["editor_opened"], true);

    let (ok, v) = call(&c, "openpaperdesign_ui_canvases", json!({}), w.clone()).await;
    assert!(ok, "{v}");
    assert_eq!(v["canvases"][0]["project"], "design/Landing");
    assert_eq!(v["canvases"][0]["bound"], true);

    // The agent edits the same binding in the same admitted workspace.
    let (ok, v) = call(&c, "get_tree_summary", json!({"nodeId": "root"}), w.clone()).await;
    assert!(ok, "{v}");
    let (ok, v) = call(
        &c,
        "create_artboard",
        json!({"name": "Scoped", "styles": {"width": "300px", "height": "200px"}}),
        w.clone(),
    )
    .await;
    assert!(ok, "{v}");

    // Reopen: the same server and link.
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_editor",
        json!({"root": root}),
        w.clone(),
    )
    .await;
    assert!(
        ok && v["action"] == "reopened" && v["url"] == url.as_str(),
        "{v}"
    );

    // Switching away from an opened editor needs an explicit force; nothing is made first.
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_create",
        json!({"name": "Second"}),
        w.clone(),
    )
    .await;
    assert!(!ok && v.as_str().unwrap().contains("force: true"), "{v}");
    assert!(!f.ws.join("design/Second").exists());
    let (ok, second) = call(
        &c,
        "openpaperdesign_ui_create",
        json!({"name": "Second", "force": true}),
        w.clone(),
    )
    .await;
    assert!(ok && second["project"] == "design/Second", "{second}");
    assert_ne!(editor_url(&second), url, "a new server for the new canvas");
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_open",
        json!({"project": "design/Landing"}),
        w.clone(),
    )
    .await;
    assert!(!ok && v.as_str().unwrap().contains("force: true"), "{v}");
    let (ok, opened) = call(
        &c,
        "openpaperdesign_ui_open",
        json!({"project": "design/Landing", "force": true}),
        w.clone(),
    )
    .await;
    assert!(ok && opened["action"] == "opened", "{opened}");
    assert_eq!(opened["canvas"]["id"], created["canvas"]["id"]);

    // Only a listed canvas directly under design/, in this workspace, can be opened.
    let bound = status(&c, w.clone()).await["bound_project"].clone();
    for project in [
        "design/../Landing",
        "design/Landing/pages",
        "Landing",
        "design/",
        ".git/x",
        "design/Missing",
        "design/tokens.json",
    ] {
        let (ok, v) = call(
            &c,
            "openpaperdesign_ui_open",
            json!({"project": project, "force": true}),
            w.clone(),
        )
        .await;
        assert!(!ok, "{project}: {v}");
    }
    let other = f.other.to_str().unwrap();
    for name in ACTIONS {
        let (ok, v) = call(&c, name, args_for(name, other), w.clone()).await;
        assert!(
            !ok && v.as_str().unwrap().contains("isn't the workspace"),
            "{name}: {v}"
        );
    }
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_create",
        json!({"name": "Landing", "force": true}),
        w.clone(),
    )
    .await;
    assert!(!ok && v.as_str().unwrap().contains("already exists"), "{v}");
    assert_eq!(status(&c, w.clone()).await["bound_project"], bound);
    c.cancel().await.unwrap();

    // A fresh process (restart): reopen the saved canvas directly and find the agent's edit.
    let c = f.host().await;
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_open",
        json!({"root": root, "project": "design/Landing"}),
        w.clone(),
    )
    .await;
    assert!(
        ok && v["action"] == "opened" && v["canvas"]["id"] == created["canvas"]["id"],
        "{v}"
    );
    let (ok, v) = call(&c, "get_tree_summary", json!({"nodeId": "root"}), w.clone()).await;
    assert!(ok && v.to_string().contains("Scoped"), "{v}");
    assert_eq!(
        (
            kept("DESIGN.md"),
            kept("design/tokens.json"),
            kept("assets/logo.svg")
        ),
        (direction, tokens, logo)
    );
    eprintln!("EXECUTED positive branch: direct new/open/reopen/restart");
    c.cancel().await.unwrap();
}

/// Supported profiles keep the plugin's own scope: a worktree's .git file stays opaque, and a
/// linked protected path or design folder refuses before anything is written.
#[tokio::test]
async fn direct_actions_keep_protected_paths_links_and_profiles() {
    let f = Fixture::new();
    std::fs::write(f.ws.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
    let c = f.host().await;
    let d = context::disabled(&f.ws);
    if built_in(&status(&c, d.clone()).await) {
        let (ok, v) = call(
            &c,
            "openpaperdesign_ui_create",
            json!({"name": "Disabled"}),
            d.clone(),
        )
        .await;
        assert!(ok && v["project"] == "design/Disabled", "{v}");
        eprintln!("EXECUTED positive branch: disabled profile create");
    }
    assert_eq!(
        std::fs::read(f.ws.join(".git")).unwrap(),
        b"gitdir: /elsewhere/.git/worktrees/x\n"
    );
    c.cancel().await.unwrap();

    // A linked protected path: the context itself is refused.
    let g = Fixture::new();
    std::os::unix::fs::symlink(&g.other, g.ws.join(".agents")).unwrap();
    let c = g.host().await;
    let before = tree(&g.root);
    for name in ACTIONS {
        let (ok, v) = call(
            &c,
            name,
            args_for(name, g.root_str()),
            context::writable(&g.ws),
        )
        .await;
        assert!(!ok, "{name}: {v}");
    }
    assert_eq!(tree(&g.root), before);
    c.cancel().await.unwrap();

    // A design/ folder that links elsewhere: nothing is listed or made through it.
    let h = Fixture::new();
    std::os::unix::fs::symlink(&h.other, h.ws.join("design")).unwrap();
    let c = h.host().await;
    let w = context::writable(&h.ws);
    let before = tree(&h.root);
    for name in ["openpaperdesign_ui_canvases", "openpaperdesign_ui_create"] {
        let (ok, v) = call(&c, name, args_for(name, h.root_str()), w.clone()).await;
        assert!(!ok, "{name}: {v}");
    }
    assert_eq!(tree(&h.root), before, "nothing written through the link");
    assert_eq!(std::fs::read_dir(&h.other).unwrap().count(), 0);
    c.cancel().await.unwrap();
}

/// The picker end to end: its answer is input only. It works for a project action only when
/// that action's own call is admitted for exactly the chosen folder.
#[tokio::test]
async fn direct_picker_answer_is_input_only_end_to_end() {
    let f = Fixture::new();
    let pick = json!({"action": "accept", "content": {"folder": context::uri(&f.ws)}});
    let c = f.launch(&["--codex-context"], pick).await;
    let (ok, v) = call(&c, "openpaperdesign_choose_folder", json!({}), app()).await;
    assert!(
        ok && v["status"] == "selected" && v["access"] == "none",
        "{v}"
    );
    let chosen = v["folder"]["path"].as_str().unwrap().to_string();
    assert_eq!(chosen, f.root_str());
    assert_eq!(status(&c, app()).await["workspace_state"], "unresolved");
    // Not by itself.
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_canvases",
        json!({"root": chosen}),
        app(),
    )
    .await;
    assert!(
        !ok && v.as_str().unwrap().contains("no Codex workspace context"),
        "{v}"
    );
    // With this call's own context for exactly that folder.
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_canvases",
        json!({"root": chosen}),
        context::writable(&f.ws),
    )
    .await;
    assert!(ok && v["workspace"] == chosen.as_str(), "{v}");
    c.cancel().await.unwrap();

    // A different chosen folder never redirects an admitted call.
    let pick = json!({"action": "accept", "content": {"folder": context::uri(&f.other)}});
    let c = f.launch(&["--codex-context"], pick).await;
    let (ok, v) = call(&c, "openpaperdesign_choose_folder", json!({}), app()).await;
    assert!(ok, "{v}");
    let chosen = v["folder"]["path"].as_str().unwrap().to_string();
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_create",
        json!({"root": chosen, "name": "X"}),
        context::writable(&f.ws),
    )
    .await;
    assert!(
        !ok && v.as_str().unwrap().contains("isn't the workspace"),
        "{v}"
    );
    assert!(!f.ws.join("design").exists() && !f.other.join("design").exists());
    c.cancel().await.unwrap();
}

/// Standalone `--workspace` keeps its deliberate configured scope (backend proof only; this
/// is not the packaged sidebar).
#[tokio::test]
async fn direct_actions_in_standalone_mode_use_the_configured_workspace() {
    let f = Fixture::new();
    let c = f
        .launch(&["--workspace", f.root_str()], json!({"action": "cancel"}))
        .await;
    if !built_in(&status(&c, app()).await) {
        return;
    }
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_create",
        json!({"name": "Solo"}),
        app(),
    )
    .await;
    assert!(ok && v["project"] == "design/Solo", "{v}");
    eprintln!("EXECUTED positive branch: standalone create");
    let (ok, v) = call(
        &c,
        "openpaperdesign_ui_create",
        json!({"root": f.other.to_str().unwrap(), "name": "X"}),
        app(),
    )
    .await;
    assert!(!ok, "{v}");
    c.cancel().await.unwrap();
}

/// A refusal after admission, with valid context A, leaves a fresh packaged session
/// unresolved: a later legitimate model context B still works.
#[tokio::test]
async fn direct_refusal_with_valid_context_leaves_a_fresh_session_unpinned() {
    let f = Fixture::new();
    let c = f.host().await;
    let w = context::writable(&f.ws);
    let before = tree(&f.root);
    for (name, args) in [
        (
            "openpaperdesign_ui_canvases",
            json!({"root": f.other.to_str().unwrap()}),
        ),
        ("openpaperdesign_ui_create", json!({"name": " "})),
        (
            "openpaperdesign_ui_open",
            json!({"project": "design/Missing"}),
        ),
        ("openpaperdesign_ui_editor", json!({})),
    ] {
        let (ok, v) = call(&c, name, args, w.clone()).await;
        assert!(!ok, "{name}: {v}");
        assert_eq!(
            status(&c, app()).await["workspace_state"],
            "unresolved",
            "{name}"
        );
    }
    assert_eq!(tree(&f.root), before);
    let s = status(&c, context::writable(&f.other)).await;
    assert_eq!(s["workspace_state"], "pinned");
    assert_eq!(s["workspace"], f.other.to_str().unwrap());
    assert_eq!(s["workspace_usable"], true);
    c.cancel().await.unwrap();
}
