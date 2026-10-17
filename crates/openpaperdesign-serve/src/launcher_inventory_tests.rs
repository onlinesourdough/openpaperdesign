//! The inventory client against task-owned fake `codex app-server` scripts (POSIX sh). They
//! log every line they receive, so the tests check the exact protocol traffic. Synthetic
//! fixtures only; the real Codex CLI is an operator check.
//!
//! Writing an executable while another thread forks lets the forked child inherit the write
//! handle until it execs, and executing the file meanwhile fails (ETXTBSY on Linux). So in
//! this test binary, writing a fixture and starting any process are mutually exclusive
//! (`spawns`): writes take the write lock, every `read` and `ps` the read lock. Start
//! failures are never retried, so an ordinary one (EACCES, ...) fails its test.
use super::*;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

fn spawns() -> &'static RwLock<()> {
    static SPAWNS: std::sync::OnceLock<RwLock<()>> = std::sync::OnceLock::new();
    SPAWNS.get_or_init(|| RwLock::new(()))
}

const INIT: &str = r#"{"id":1,"result":{"codexHome":"/fixture-home/.codex","platformFamily":"unix","platformOs":"linux","userAgent":"codex_cli_rs/0.162.1"}}"#;
const ACME: &str = r#"{"id":"prj_fixture_acme","name":"Acme site","createdAt":1,"updatedAt":2,"position":0,"recencyAt":null,"metadata":{"note":"private"},"roots":[{"path":"/w/acme"},{"path":"/w/acme-docs"}]}"#;
const BETA: &str = r#"{"id":"prj_fixture_beta","name":"Acme site","createdAt":3,"updatedAt":4,"position":1,"recencyAt":1700000000,"metadata":{},"roots":[{"path":"/w/beta"}]}"#;

struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new() -> Self {
        use std::hash::{BuildHasher, RandomState};
        let dir = std::env::temp_dir().join(format!(
            "opd-inventory-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        std::fs::create_dir(&dir).unwrap();
        Self { dir }
    }

    /// A fake app-server answering by method. `replies` are (case pattern, reply lines).
    async fn server(&self, init: &str, replies: &[(&str, &str)]) -> PathBuf {
        let mut cases = format!("    *'\"method\":\"initialize\"'*) printf '%s\\n' '{init}' ;;\n");
        cases.push_str("    *'\"method\":\"initialized\"'*) ;;\n");
        for (pattern, reply) in replies {
            cases.push_str(&format!("    {pattern}) printf '%s\\n' '{reply}' ;;\n"));
        }
        cases.push_str("    *) printf '%s\\n' '{\"id\":99,\"error\":{\"code\":-32600,\"message\":\"unexpected\"}}' ;;\n");
        self.script(&format!(
            "while IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> '{log}'\n  case \"$line\" in\n{cases}  esac\ndone\n",
            log = self.dir.join("log").display()
        )).await
    }

    async fn script(&self, body: &str) -> PathBuf {
        self.script_mode(body, 0o755).await
    }

    async fn script_mode(&self, body: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join("codex");
        let text = format!(
            "#!/bin/sh\n[ \"$1\" = app-server ] || exit 64\necho $$ > '{pid}'\n{body}",
            pid = self.dir.join("pid").display()
        );
        let _no_spawns = spawns().write().await;
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// Every line the fake received, parsed.
    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn pid(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("pid"))
            .ok()
            .map(|s| s.trim().to_string())
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The process is gone or a zombie waiting to be reaped (no longer running).
async fn running(pid: &str) -> bool {
    let _spawn = spawns().read().await;
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// `read` once, never cancelled, with no fixture being written meanwhile. No retry.
async fn run(program: &Path, query: Query<'_>, deadline: Duration) -> Result<Value, Unavailable> {
    run_until(program, query, deadline, std::future::pending()).await
}

/// The read lock covers only this request's fork-to-exec window: it is held until the fake
/// has started (it writes its pid first) or `read` has returned, never for the whole
/// exchange, so other tests' `ps` checks and fixture writes don't queue behind a live request.
async fn run_until(
    program: &Path,
    query: Query<'_>,
    deadline: Duration,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<Value, Unavailable> {
    let pid = program.with_file_name("pid");
    let mut spawn = Some(spawns().read().await);
    // A pid from an earlier run must not end this run's window early.
    let _ = std::fs::remove_file(&pid);
    let read = read(program.as_os_str(), query, deadline, cancelled);
    tokio::pin!(read);
    loop {
        tokio::select! {
            r = &mut read => return r,
            _ = tokio::time::sleep(Duration::from_millis(5)), if spawn.is_some() => {
                if pid.exists() {
                    spawn = None;
                }
            }
        }
    }
}

async fn list(program: &Path) -> Result<Value, Unavailable> {
    run(program, Query::List, Duration::from_secs(5)).await
}

const LIST: &str = "*'\"method\":\"project/list\"'*";

#[tokio::test]
async fn lists_every_page_with_every_root_and_exact_traffic() {
    let f = Fake::new();
    let page1 = format!(r#"{{"id":2,"result":{{"data":[{ACME}],"nextCursor":"c2"}}}}"#);
    let page2 = format!(r#"{{"id":3,"result":{{"data":[{BETA}],"nextCursor":null}}}}"#);
    let program = f
        .server(
            INIT,
            &[
                ("*'\"cursor\":\"c2\"'*", page2.as_str()),
                (LIST, page1.as_str()),
            ],
        )
        .await;
    let v = list(&program).await.unwrap();
    assert_eq!(
        v,
        json!({
            "available": true, "source": "codex-app-server", "host": "this computer", "complete": true,
            "projects": [
                {"source":"codex-app-server","host":"this computer","id":"prj_fixture_acme","name":"Acme site","position":0,"recencyAt":null,"roots":[{"path":"/w/acme"},{"path":"/w/acme-docs"}]},
                {"source":"codex-app-server","host":"this computer","id":"prj_fixture_beta","name":"Acme site","position":1,"recencyAt":1700000000,"roots":[{"path":"/w/beta"}]},
            ]
        }),
        "same names stay separate projects; every root is kept; metadata and codexHome are dropped"
    );
    assert_eq!(
        f.log(),
        vec![
            json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"openpaperdesign_inventory","title":"OpenPaperDesign native project inventory","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}}),
            json!({"method":"initialized"}),
            json!({"id":2,"method":"project/list","params":{"limit":100}}),
            json!({"id":3,"method":"project/list","params":{"limit":100,"cursor":"c2"}}),
        ],
        "initialize, initialized and read-only list calls only"
    );
    assert!(
        !running(&f.pid().unwrap()).await,
        "the app-server was closed"
    );
}

#[tokio::test]
async fn reads_one_project_by_id_and_checks_the_answer() {
    let f = Fake::new();
    let ok = format!(r#"{{"id":2,"result":{{"project":{ACME}}}}}"#);
    let program = f
        .server(INIT, &[("*'\"method\":\"project/read\"'*", ok.as_str())])
        .await;
    let v = run(
        &program,
        Query::Read("prj_fixture_acme"),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(v["projects"][0]["roots"].as_array().unwrap().len(), 2);
    assert_eq!(
        f.log()[2],
        json!({"id":2,"method":"project/read","params":{"projectId":"prj_fixture_acme"}})
    );
    // An answer for another project is refused.
    let r = run(&program, Query::Read("prj_other"), Duration::from_secs(5)).await;
    assert_eq!(r, Err(Unavailable::Protocol("project/read id")));
}

#[tokio::test]
async fn host_errors_map_to_unsupported_or_api_error() {
    for (reply, want) in [
        (
            r#"{"id":2,"error":{"code":-32601,"message":"method not found: project/list"}}"#,
            Unavailable::Unsupported,
        ),
        (
            r#"{"id":2,"error":{"code":-32000,"message":"at /private/path"}}"#,
            Unavailable::Api(-32000),
        ),
        (
            r#"{"id":2,"error":{"code":"x","message":"m"}}"#,
            Unavailable::Protocol("error response"),
        ),
    ] {
        let f = Fake::new();
        let program = f.server(INIT, &[(LIST, reply)]).await;
        let got = list(&program).await;
        assert_eq!(got, Err(want), "{reply}");
        let json = got.unwrap_err().to_json().to_string();
        assert!(
            !json.contains("/private/path") && json.contains("\"available\":false"),
            "{json}"
        );
    }
}

#[tokio::test]
async fn anything_outside_the_schema_makes_the_inventory_unavailable() {
    let page =
        |project: &str| format!(r#"{{"id":2,"result":{{"data":[{project}],"nextCursor":null}}}}"#);
    let cases: Vec<(String, String)> = vec![
        (
            INIT.into(),
            page(&ACME.replace(
                r#","roots":[{"path":"/w/acme"},{"path":"/w/acme-docs"}]"#,
                "",
            )),
        ),
        (
            INIT.into(),
            page(&ACME.replace("/w/acme-docs", "w/relative")),
        ),
        (
            INIT.into(),
            page(&ACME.replace(r#""note":"private""#, r#""note":1"#)),
        ),
        (INIT.into(), page(&ACME.replace("prj_fixture_acme", ""))),
        (
            INIT.into(),
            page(&ACME.replace(r#""position":0"#, r#""position":"0""#)),
        ),
        (
            INIT.into(),
            page(&ACME.replace(r#""recencyAt":null"#, r#""recencyAt":"soon""#)),
        ),
        (
            INIT.into(),
            r#"{"id":2,"result":{"data":{},"nextCursor":null}}"#.into(),
        ),
        (
            INIT.into(),
            r#"{"id":2,"result":{"data":[],"nextCursor":7}}"#.into(),
        ),
        (
            INIT.into(),
            r#"{"id":7,"result":{"data":[],"nextCursor":null}}"#.into(),
        ),
        (INIT.into(), "not json".into()),
        (
            INIT.into(),
            r#"{"id":"s1","method":"item/commandExecution/requestApproval","params":{}}"#.into(),
        ),
        (
            INIT.replace(r#""codexHome":"/fixture-home/.codex","#, ""),
            page(ACME),
        ),
    ];
    for (init, reply) in cases {
        let f = Fake::new();
        let program = f.server(&init, &[(LIST, reply.as_str())]).await;
        let got = list(&program).await;
        assert!(
            matches!(got, Err(Unavailable::Protocol(_))),
            "{reply}: {got:?}"
        );
        assert!(!running(&f.pid().unwrap()).await);
    }
}

#[tokio::test]
async fn notifications_are_skipped_and_an_empty_inventory_is_complete() {
    let f = Fake::new();
    let reply = "{\"method\":\"thread/started\",\"params\":{}}\n{\"id\":2,\"result\":{\"data\":[],\"nextCursor\":null}}";
    let program = f.server(INIT, &[(LIST, reply)]).await;
    let v = list(&program).await.unwrap();
    assert_eq!(v["projects"], json!([]));
    assert_eq!(v["complete"], true);
}

#[tokio::test]
async fn pages_are_bounded_and_reported_incomplete() {
    let f = Fake::new();
    // Always another page, each answered with its own request id: the client stops at its
    // page limit.
    let mut cases = String::new();
    for id in 2..=(MAX_PAGES as i64 + 1) {
        cases.push_str(&format!(
            "    *'\"id\":{id},'*) printf '%s\\n' '{{\"id\":{id},\"result\":{{\"data\":[],\"nextCursor\":\"again\"}}}}' ;;\n"
        ));
    }
    let program = f.script(&format!(
        "while IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> '{log}'\n  case \"$line\" in\n    *'\"method\":\"initialize\"'*) printf '%s\\n' '{INIT}' ;;\n{cases}  esac\ndone\n",
        log = f.dir.join("log").display()
    )).await;
    let v = list(&program).await.unwrap();
    assert_eq!(v["complete"], false);
    let lists = f
        .log()
        .iter()
        .filter(|m| m["method"] == "project/list")
        .count();
    assert_eq!(lists, MAX_PAGES);
}

#[tokio::test]
async fn a_silent_app_server_times_out_and_is_killed() {
    let f = Fake::new();
    let program = f.script("read -r line\nexec sleep 30\n").await;
    let started = std::time::Instant::now();
    let r = run(&program, Query::List, Duration::from_millis(300)).await;
    assert_eq!(r, Err(Unavailable::Timeout));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        !running(&f.pid().unwrap()).await,
        "killed after the deadline"
    );
}

#[tokio::test]
async fn dropping_the_request_future_kills_the_app_server() {
    let f = Fake::new();
    let program = f.script("read -r line\nexec sleep 30\n").await;
    let task =
        tokio::spawn(async move { run(&program, Query::List, Duration::from_secs(30)).await });
    let waited = std::time::Instant::now();
    let pid = loop {
        if let Some(pid) = f.pid().filter(|p| !p.is_empty()) {
            break pid;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "the fake never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    task.abort();
    let _ = task.await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while running(&pid).await && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!running(&pid).await, "kill_on_drop stopped the app-server");
}

#[tokio::test]
async fn oversized_lines_early_exit_and_missing_cli_are_unavailable() {
    let f = Fake::new();
    let program = f
        .script("read -r line\nhead -c 1100000 /dev/zero | tr '\\000' a\necho\n")
        .await;
    assert_eq!(
        list(&program).await,
        Err(Unavailable::Protocol("line too long"))
    );

    let f = Fake::new();
    let program = f.script("read -r line\nexit 0\n").await;
    assert_eq!(list(&program).await, Err(Unavailable::Exited));

    let missing = f.dir.join("no-such-codex");
    assert_eq!(list(&missing).await, Err(Unavailable::NotFound));
    let v = Unavailable::NotFound.to_json();
    assert_eq!(v["reason"], "codex_not_found");
    assert!(!v.to_string().contains(&f.dir.display().to_string()));
}

/// The MCP request's cancellation (R1-01): the child is killed and reaped before `read`
/// returns, so the caller frees the inventory only after it is gone.
#[tokio::test]
async fn a_cancellation_signal_stops_and_reaps_the_app_server_before_returning() {
    let f = Fake::new();
    let program = f.script("read -r line\nexec sleep 30\n").await;
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        run_until(&program, Query::List, Duration::from_secs(30), async {
            let _ = cancelled.await;
        })
        .await
    });
    let waited = std::time::Instant::now();
    let pid = loop {
        if let Some(pid) = f.pid().filter(|p| !p.is_empty()) {
            break pid;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "the fake never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(running(&pid).await);
    let cancelled_at = std::time::Instant::now();
    cancel.send(()).unwrap();
    assert_eq!(task.await.unwrap(), Err(Unavailable::Cancelled));
    assert!(cancelled_at.elapsed() < Duration::from_secs(5));
    assert!(
        !running(&pid).await,
        "gone when read returned, without polling"
    );
    assert_eq!(Unavailable::Cancelled.to_json()["reason"], "cancelled");
}

/// A request already cancelled starts no app-server at all.
#[tokio::test]
async fn an_already_cancelled_request_starts_nothing() {
    let f = Fake::new();
    let program = f.script("read -r line\nexec sleep 30\n").await;
    let r = run_until(
        &program,
        Query::List,
        Duration::from_secs(5),
        std::future::ready(()),
    )
    .await;
    assert_eq!(r, Err(Unavailable::Cancelled));
    assert_eq!(f.pid(), None, "never started");
}

/// An ordinary start failure (here EACCES: not executable) is reported once, never retried
/// (R1-03). Making the file executable afterwards changes nothing about that answer.
#[tokio::test]
async fn an_ordinary_start_failure_is_reported_not_retried() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fake::new();
    let program = f.script_mode("read -r line\nexit 0\n", 0o644).await;
    let path = program.clone();
    let fix = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _no_spawns = spawns().write().await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    assert_eq!(list(&program).await, Err(Unavailable::StartFailed));
    assert_eq!(f.pid(), None);
    fix.await.unwrap();
    assert_eq!(
        Unavailable::StartFailed.to_json()["reason"],
        "start_failed",
        "no OS detail is passed on"
    );
}
