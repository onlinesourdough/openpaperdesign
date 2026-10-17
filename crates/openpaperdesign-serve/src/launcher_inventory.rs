//! Read-only native Codex project inventory for the sidebar home (issue #6).
//!
//! One short-lived `codex app-server` child per request, over its stdio JSONL protocol
//! (codex-cli 0.162.1, experimental API; the `jsonrpc` field is omitted). The client sends
//! only `initialize` (with `experimentalApi`), the `initialized` notification, and
//! `project/list` or `project/read`. It never starts threads or turns, changes settings,
//! authentication, permissions or projects, and never reads Codex's files itself.
//!
//! The result is discovery data in Codex's own namespace (`codex-app-server`, this computer):
//! its project ids are not the desktop or cloud ids, and nothing here joins them by name. A
//! listed root is never authority: workspace admission still comes only from per-call host
//! context. Anything outside the expected shape makes the whole answer unavailable.

use serde_json::{Map, Value, json};
use std::ffi::OsStr;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};

pub(super) const SOURCE: &str = "codex-app-server";
const HOST: &str = "this computer";
const PAGE: u32 = 100;
const MAX_PAGES: usize = 10;
const MAX_LINE: usize = 1 << 20;
const MAX_SKIPPED: usize = 256;
const MAX_ROOTS: usize = 64;
const MAX_ID: usize = 256;
const MAX_NAME: usize = 512;
const MAX_PATH: usize = 4096;
const MAX_CURSOR: usize = 1024;

/// Why the inventory is not available. Reasons never carry paths, tokens or host messages.
#[derive(Debug, PartialEq)]
pub(super) enum Unavailable {
    /// No `codex` executable on the normal command path.
    NotFound,
    /// It exists but could not be started.
    StartFailed,
    Timeout,
    /// The app-server closed its output before answering.
    Exited,
    /// This Codex does not offer the method (JSON-RPC -32601).
    Unsupported,
    /// The app-server answered with an error code.
    Api(i64),
    /// The answer was not in the expected shape.
    Protocol(&'static str),
    /// The MCP request was cancelled; the app-server was stopped.
    Cancelled,
}

impl Unavailable {
    pub fn to_json(&self) -> Value {
        let (reason, detail) = match self {
            Self::NotFound => (
                "codex_not_found",
                "The Codex command-line tool (codex) was not found on this computer's command path.".to_string(),
            ),
            Self::StartFailed => ("start_failed", "The Codex command-line tool could not be started.".into()),
            Self::Timeout => ("timeout", "Codex didn't answer in time.".into()),
            Self::Exited => ("exited", "Codex stopped before answering.".into()),
            Self::Unsupported => (
                "unsupported",
                "This Codex version doesn't offer its project list to other programs.".into(),
            ),
            Self::Api(code) => ("api_error", format!("Codex refused the request (code {code}).")),
            Self::Protocol(what) => ("protocol", format!("Codex's answer had an unexpected shape ({what}).")),
            Self::Cancelled => ("cancelled", "The request was cancelled.".into()),
        };
        json!({"available": false, "source": SOURCE, "reason": reason, "detail": detail})
    }
}

pub(super) enum Query<'a> {
    List,
    Read(&'a str),
}

/// Read the inventory with `program app-server`. The whole exchange is bounded by `deadline`
/// and ends early when `cancelled` completes (the MCP request's cancellation). An already
/// cancelled request starts nothing. On cancellation the child is killed and waited for
/// before this returns; otherwise it is closed, then killed if it lingers. It is also killed
/// if this future is dropped.
pub(super) async fn read(
    program: &OsStr,
    query: Query<'_>,
    deadline: Duration,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<Value, Unavailable> {
    let mut cancelled = std::pin::pin!(cancelled);
    tokio::select! {
        biased;
        () = &mut cancelled => return Err(Unavailable::Cancelled),
        () = std::future::ready(()) => {}
    }
    let mut child = Command::new(program)
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Unavailable::NotFound,
            _ => Unavailable::StartFailed,
        })?;
    let mut conn = Conn {
        stdin: child.stdin.take(),
        out: BufReader::new(child.stdout.take().expect("piped stdout")),
        next: 0,
    };
    let result = tokio::select! {
        biased;
        () = &mut cancelled => Err(Unavailable::Cancelled),
        r = tokio::time::timeout(deadline, exchange(&mut conn, query)) => {
            r.unwrap_or(Err(Unavailable::Timeout))
        }
    };
    drop(conn);
    if result == Err(Unavailable::Cancelled) {
        // Stop it now and wait for it, so the caller releases the inventory only after.
        let _ = child.kill().await;
    } else if tokio::time::timeout(Duration::from_millis(500), child.wait())
        .await
        .is_err()
    {
        // End of input asks the app-server to exit; one that lingers is killed.
        let _ = child.kill().await;
    }
    result
}

async fn exchange(c: &mut Conn, query: Query<'_>) -> Result<Value, Unavailable> {
    let init = c
        .request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "openpaperdesign_inventory",
                    "title": "OpenPaperDesign native project inventory",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {"experimentalApi": true},
            }),
        )
        .await?;
    // InitializeResponse: all four are required. None of them is passed on.
    for key in ["codexHome", "platformFamily", "platformOs", "userAgent"] {
        if !init.get(key).is_some_and(Value::is_string) {
            return Err(Unavailable::Protocol("initialize response"));
        }
    }
    c.send(&json!({"method": "initialized"})).await?;
    let mut projects = Vec::new();
    let mut complete = true;
    match query {
        Query::List => {
            let mut cursor: Option<String> = None;
            complete = false;
            for _ in 0..MAX_PAGES {
                let mut params = json!({"limit": PAGE});
                if let Some(cursor) = &cursor {
                    params["cursor"] = json!(cursor);
                }
                let page = c.request("project/list", params).await?;
                let data = page
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or(Unavailable::Protocol("project/list data"))?;
                for p in data {
                    projects.push(project(p)?);
                }
                match page.get("nextCursor") {
                    None | Some(Value::Null) => {
                        complete = true;
                        break;
                    }
                    Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_CURSOR => {
                        cursor = Some(s.clone())
                    }
                    _ => return Err(Unavailable::Protocol("project/list nextCursor")),
                }
            }
        }
        Query::Read(id) => {
            let r = c.request("project/read", json!({"projectId": id})).await?;
            let p = project(
                r.get("project")
                    .ok_or(Unavailable::Protocol("project/read project"))?,
            )?;
            if p["id"] != id {
                return Err(Unavailable::Protocol("project/read id"));
            }
            projects.push(p);
        }
    }
    Ok(json!({
        "available": true,
        "source": SOURCE,
        "host": HOST,
        "complete": complete,
        "projects": projects,
    }))
}

/// One `Project`, checked against the 0.162.1 schema and reduced to what the home shows.
/// `metadata` is checked but not passed on.
fn project(p: &Value) -> Result<Value, Unavailable> {
    let bad = Unavailable::Protocol("project");
    let o = p.as_object().ok_or(Unavailable::Protocol("project"))?;
    let text = |k: &str, max: usize, empty: bool| {
        o.get(k)
            .and_then(Value::as_str)
            .filter(|s| {
                (empty || !s.is_empty()) && s.len() <= max && !s.chars().any(char::is_control)
            })
            .ok_or(Unavailable::Protocol("project"))
    };
    let id = text("id", MAX_ID, false)?;
    let name = text("name", MAX_NAME, true)?;
    for k in ["createdAt", "updatedAt", "position"] {
        if !o.get(k).is_some_and(Value::is_i64) {
            return Err(bad);
        }
    }
    let recency = match o.get("recencyAt") {
        None | Some(Value::Null) => Value::Null,
        Some(v) if v.is_i64() => v.clone(),
        _ => return Err(bad),
    };
    let metadata_ok = o
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|m: &Map<String, Value>| m.values().all(Value::is_string));
    if !metadata_ok {
        return Err(bad);
    }
    let roots = o
        .get("roots")
        .and_then(Value::as_array)
        .ok_or(Unavailable::Protocol("project roots"))?;
    if roots.len() > MAX_ROOTS {
        return Err(Unavailable::Protocol("project roots"));
    }
    let roots = roots
        .iter()
        .map(|r| {
            r.get("path")
                .and_then(Value::as_str)
                .filter(|s| absolute(s))
                .map(|s| json!({"path": s}))
                .ok_or(Unavailable::Protocol("project root path"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "source": SOURCE,
        "host": HOST,
        "id": id,
        "name": name,
        "position": o["position"],
        "recencyAt": recency,
        "roots": roots,
    }))
}

/// An absolute local path as the schema promises (Unix, or a Windows drive or UNC path).
fn absolute(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() <= MAX_PATH
        && !s.chars().any(char::is_control)
        && (s.starts_with('/')
            || s.starts_with("\\\\")
            || (b.len() >= 3
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && (b[2] == b'\\' || b[2] == b'/')))
}

struct Conn {
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
    next: i64,
}

impl Conn {
    async fn send(&mut self, msg: &Value) -> Result<(), Unavailable> {
        let stdin = self.stdin.as_mut().ok_or(Unavailable::Exited)?;
        let mut line = serde_json::to_vec(msg).expect("serializable");
        line.push(b'\n');
        stdin
            .write_all(&line)
            .await
            .map_err(|_| Unavailable::Exited)?;
        stdin.flush().await.map_err(|_| Unavailable::Exited)
    }

    /// Send one request and wait for its response. Notifications are skipped (bounded); a
    /// request from the app-server or a response to another id is a protocol error.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Unavailable> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"id": id, "method": method, "params": params}))
            .await?;
        for _ in 0..=MAX_SKIPPED {
            let line = self.line().await?;
            let msg: Value =
                serde_json::from_slice(&line).map_err(|_| Unavailable::Protocol("not JSON"))?;
            let Some(o) = msg.as_object() else {
                return Err(Unavailable::Protocol("not an object"));
            };
            match (o.get("id"), o.get("method")) {
                (None, Some(_)) => continue,
                (Some(_), Some(_)) => {
                    return Err(Unavailable::Protocol("unexpected server request"));
                }
                (Some(got), None) if got == &json!(id) => {
                    return match (o.get("result"), o.get("error")) {
                        (Some(result), None) => Ok(result.clone()),
                        (None, Some(e)) => match e.get("code").and_then(Value::as_i64) {
                            Some(-32601) => Err(Unavailable::Unsupported),
                            Some(code) if e.get("message").is_some_and(Value::is_string) => {
                                Err(Unavailable::Api(code))
                            }
                            _ => Err(Unavailable::Protocol("error response")),
                        },
                        _ => Err(Unavailable::Protocol("response")),
                    };
                }
                _ => return Err(Unavailable::Protocol("response id")),
            }
        }
        Err(Unavailable::Protocol("too many notifications"))
    }

    /// One line, at most `MAX_LINE` bytes.
    async fn line(&mut self) -> Result<Vec<u8>, Unavailable> {
        let mut buf = Vec::new();
        let n = (&mut self.out)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|_| Unavailable::Exited)?;
        if n == 0 {
            return Err(Unavailable::Exited);
        }
        if buf.last() != Some(&b'\n') {
            return Err(if buf.len() > MAX_LINE {
                Unavailable::Protocol("line too long")
            } else {
                Unavailable::Exited
            });
        }
        Ok(buf)
    }
}

#[cfg(test)]
#[path = "launcher_inventory_tests.rs"]
mod tests;
