//! Narrow admission for Codex 0.162.1's experimental per-tool-call context.
//! This validates a supported permission profile; it does not sandbox the process.

use super::{identity, real_dir, workspace_problem};
use rmcp::model::RequestMetaObject;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(super) const CAPABILITY: &str = "codex/sandbox-state-meta";
const PROTECTED: [&str; 4] = [".git", ".agents", ".codex", ".aws"];
const PERMISSION_ERROR: &str = "Codex permissions are read-only or unsupported. This adapter accepts only the observed exact disabled profile or managed/restricted workspace-write profile: one write grant for the selected workspace, root read, temporary-directory grants, the four standard read-only metadata paths, and network restricted or enabled. No project or writable browser is admitted; permissions are not expanded.";

#[derive(Clone)]
pub(super) struct Workspace {
    pub path: PathBuf,
    pub id: (u64, u64),
    pub source: &'static str,
    pub permission: &'static str,
}

impl Workspace {
    pub fn new(path: &Path, source: &'static str) -> Result<Self, String> {
        let path = path.canonicalize().map_err(|e| format!("workspace: {e}"))?;
        let id = identity(&real_dir(&path)?);
        Ok(Self {
            path,
            id,
            source,
            permission: "standalone",
        })
    }

    pub fn from_meta(meta: &RequestMetaObject) -> Result<Self, String> {
        let state = meta.get(CAPABILITY).and_then(Value::as_object).ok_or(
            "Missing Codex workspace context. Use a real model tool call in the supported Codex host; direct app-server tool calls do not supply this metadata. Tool arguments and the plugin working directory cannot authorize a workspace.",
        )?;
        let uri = state
            .get("sandboxCwd")
            .and_then(Value::as_str)
            .ok_or("Codex workspace context has no valid sandboxCwd file URI")?;
        let path = file_uri(uri)?;
        let mut ws = Self::new(&path, CAPABILITY)?;
        if let Some(problem) = workspace_problem(&ws.path) {
            return Err(problem);
        }
        ws.project_path(&ws.path)?;
        ws.permission = supported_permissions(state.get("permissionProfile"), &ws)?;
        Ok(ws)
    }

    /// No project route may enter a protected subtree, including through an alias.
    /// The raw path check also rejects traversal through one before canonicalization.
    pub fn project_path(&self, path: &Path) -> Result<(), String> {
        if self.source == CAPABILITY
            && path.components().any(|c| {
                PROTECTED
                    .iter()
                    .any(|p| c.as_os_str().eq_ignore_ascii_case(p))
            })
        {
            return Err(
                "project paths through .git, .agents, .codex or .aws are read-only; refusing"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Deliberately accept only the live-observed managed workspace-write shape, in any
/// entry order. Exact structural comparison rejects unknown fields, duplicates, extra
/// grants, deny/glob rules and missing exclusions instead of approximating their policy.
/// Codex source: 092d3acd6bec3e3a14bdc7e7a2810ab628ab759d,
/// protocol/src/{models,permissions}.rs (Write::can_write and metadata carveouts).
fn supported_permissions(profile: Option<&Value>, ws: &Workspace) -> Result<&'static str, String> {
    let profile = profile.ok_or(PERMISSION_ERROR)?;
    // The other live-observed variant. This disables the HOST sandbox, never our
    // workspace containment, identity, lifecycle or plugin protected-path rules.
    if profile == &json!({"type":"disabled"}) {
        check_protected_paths(ws)?;
        return Ok("host-sandbox-disabled");
    }
    // Both strings are live-observed host values. Neither changes this adapter's
    // loopback-only listener or authorizes any outbound network behavior.
    let network = match profile.get("network").and_then(Value::as_str) {
        Some(network @ ("restricted" | "enabled")) => network,
        _ => return Err(PERMISSION_ERROR.into()),
    };
    let entries = profile
        .pointer("/file_system/entries")
        .and_then(Value::as_array)
        .ok_or(PERMISSION_ERROR)?;
    let write = entries
        .iter()
        .find(|e| e["access"] == "write" && e["path"]["type"] == "path")
        .and_then(|e| e["path"]["path"].as_str())
        .ok_or(PERMISSION_ERROR)?;
    let write_path = strict_absolute_path(write)?;
    if write_path.canonicalize().ok().as_ref() != Some(&ws.path) {
        return Err(PERMISSION_ERROR.into());
    }
    let mut expected = vec![
        json!({"path":{"type":"special","value":{"kind":"root"}},"access":"read"}),
        json!({"path":{"type":"path","path":write},"access":"write"}),
        json!({"path":{"type":"special","value":{"kind":"slash_tmp"}},"access":"write"}),
        json!({"path":{"type":"special","value":{"kind":"tmpdir"}},"access":"write"}),
    ];
    for name in PROTECTED {
        expected.push(json!({"path":{"type":"path","path":write_path.join(name)},"access":"read","missing_path_behavior":"skip"}));
    }
    check_protected_paths(ws)?;

    let mut actual = entries.clone();
    actual.sort_by_key(Value::to_string);
    expected.sort_by_key(Value::to_string);
    let envelope = json!({"type":"managed","file_system":{"type":"restricted","entries":entries},"network":network});
    if profile != &envelope || actual != expected {
        return Err(PERMISSION_ERROR.into());
    }
    Ok("managed-workspace-write")
}

fn check_protected_paths(ws: &Workspace) -> Result<(), String> {
    for name in PROTECTED {
        // A worktree's regular .git file is opaque: never open it, parse its
        // gitdir pointer or use that pointer as authority. Project paths cannot
        // enter any protected name. Symlinked carveouts remain unsupported.
        match std::fs::symlink_metadata(ws.path.join(name)) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(m) if name == ".git" && m.is_file() && !m.file_type().is_symlink() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("unsupported protected metadata path: .git, .agents, .codex and .aws must be absent or real directories; only .git may also be an opaque regular file (no links)".into()),
        }
    }
    Ok(())
}

fn strict_absolute_path(s: &str) -> Result<PathBuf, String> {
    // Do not let Path::components silently normalize dot/repeated-separator tricks.
    if !s.starts_with('/')
        || s.starts_with("//")
        || s.contains('\\')
        || s.chars().any(char::is_control)
        || s.split('/')
            .skip(1)
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err("workspace must be a normalized absolute local path".into());
    }
    Ok(PathBuf::from(s))
}

/// Only local UTF-8 file URIs: empty authority or literal localhost. Decode once,
/// strictly; never accept remote hosts, query/fragment, encoded separators or NUL.
pub(super) fn file_uri(uri: &str) -> Result<PathBuf, String> {
    let path = uri
        .strip_prefix("file://")
        .and_then(|p| {
            if p.starts_with('/') {
                Some(p)
            } else {
                p.strip_prefix("localhost")
            }
        })
        .ok_or("sandboxCwd must be a local absolute file URI")?;
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        let b = if b == b'%' {
            let hex = |b: u8| (b as char).to_digit(16).map(|n| n as u8);
            let a = bytes
                .next()
                .and_then(hex)
                .ok_or("invalid file URI percent encoding")?;
            let b = bytes
                .next()
                .and_then(hex)
                .ok_or("invalid file URI percent encoding")?;
            let b = a * 16 + b;
            if b == b'/' || b == b'\\' {
                return Err("encoded file URI separators are refused".into());
            }
            b
        } else {
            if !b.is_ascii() || b <= b' ' || matches!(b, b'?' | b'#' | b'\\') {
                return Err("invalid file URI character, query or fragment".into());
            }
            b
        };
        decoded.push(b);
    }
    let path = String::from_utf8(decoded).map_err(|_| "file URI path must be UTF-8")?;
    // Allow the root to reach the existing explicit root guard.
    if path == "/" {
        return Ok(PathBuf::from(path));
    }
    strict_absolute_path(&path)
}
