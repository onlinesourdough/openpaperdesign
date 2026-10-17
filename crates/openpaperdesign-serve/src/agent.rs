//! Agent edits over MCP, on the bound project only.
//!
//! The MCP endpoint (`/mcp`) reuses upstream's tool catalogue and `mcp::exec::call`. One host
//! thread owns an upstream `Editor` holding the bound project's pages (the `Editor` isn't
//! `Send`, so like upstream's `mcp::serve_host` it lives on its own thread). Each call takes
//! the same write lock as browser saves and:
//!
//! 1. re-verifies the project root and entries (same checks as every browser request),
//! 2. reloads the project if any page file or the manifest changed on disk since the host last
//!    read or wrote it (e.g. a browser save),
//! 3. applies the policy below, runs the tool,
//! 4. refuses the change (and discards it) if it touches a page this MCP session hasn't read,
//!    or whose content or metadata (name, position) changed since the session last read it,
//!    e.g. a browser save landed after the agent looked, and
//! 5. otherwise saves everything the call changed (new images, pages, the manifest) as one
//!    commit (`crate::commit`): all of it, or after an error none of it. Each written page
//!    gets a new revision, so the browser picks the change up.
//!
//! A call that returns an error changes nothing, in memory or on disk (the host reloads from
//! disk, which it matched before the call). Calls are queued; a call that couldn't start
//! within `CALL_TIMEOUT` is abandoned and never runs, and a call that started is always
//! waited for (see `Job`).
//!
//! What a session has read is kept per page: a page counts as read at a revision only when a
//! call that succeeded returned that page's content or state (`read_keys`), or when the
//! session's own successful call wrote that revision. The page list (names, order) is one
//! more item, read only through `get_basic_info`. Failed calls, calls that can't be attributed
//! to specific pages (`open_file` included), and calls whose result wasn't delivered,
//! acknowledge nothing. A change must find every page it writes (and the page list, if it
//! changes it) read at its current revision. A session that hasn't read (a new one, or one
//! whose record was dropped because more than `MAX_SESSIONS` sessions were seen) can't
//! change existing pages until it reads, nor add a page until it has read the page list.
//!
//! Policy: `export` and `export_combined_pdf` are refused (they write files where the agent
//! says), and so is `create_file`. `list_resources`, `open_file` and any `fileId` argument only
//! accept the bound project. Image references (`<img src>`, CSS `url(...)`) must be `data:`
//! URIs, http(s) URLs (never fetched upstream) or ids of assets already loaded (the bound
//! project's own `assets/`, or images an agent sent as `data:` URIs). Inline `<svg>` must be
//! well-formed XML with no DTD, no `@import` and no `<image>`/`<feImage>` elements (usvg reads
//! their href as a file name when it isn't a valid data URI; use HTML `<img>` with a `data:`
//! URI instead), and every `href` or `url(...)` must be `#id` of an element in that SVG.
//! Anything else is refused before upstream code runs, so no local path is ever read.

use crate::{
    ASSETS, Kind, check_project_dir, check_root, etag, page_prefix, plain, read_manifest_checked,
    valid_page_id,
};
use paper_rs::editor::Editor;
use paper_rs::mcp::html::{self, Html};
use paper_rs::mcp::server::{to_result, tool_list};
use paper_rs::mcp::{ToolResult, exec, tools};
use paper_rs::project::format::page_from_json;
use paper_rs::project::{PAGES_DIR, PageData, PageRef, Project, Store, now_ms};
use paper_rs::render::gpu::Headless;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Tools that are not offered at all.
pub const REFUSED_TOOLS: &[&str] = &["export", "export_combined_pdf", "create_file"];

const CALL_TIMEOUT: Duration = Duration::from_secs(120);

// ------------------------------------------------------------------------------ policy

/// The identity of the bound project, for `fileId` checks.
#[derive(Debug, Clone)]
pub struct Bound {
    pub dir: PathBuf,
    pub id: String,
    pub name: String,
}

impl Bound {
    fn matches(&self, key: &str) -> bool {
        let key = key.trim().trim_start_matches("/file/");
        key == self.id
            || key == self.name
            || Path::new(key).canonicalize().is_ok_and(|p| p == self.dir)
    }
}

/// Whether an image reference can be used without reading a local file.
fn allowed_image(src: &str) -> bool {
    let s = src.trim();
    if s.starts_with("data:") || s.starts_with("http://") || s.starts_with("https://") {
        return true;
    }
    // Same lookup as upstream `image_asset`: an id already registered.
    let id = s
        .strip_prefix("paper-asset://")
        .or_else(|| s.strip_prefix("file://"))
        .unwrap_or(s);
    !id.is_empty() && paper_rs::assets::get(id).is_some()
}

/// Every `url(...)` target in a string (case-insensitive, quotes trimmed).
fn css_urls(s: &str) -> Vec<String> {
    let lower = s.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = lower[at..].find("url(") {
        let start = at + i + 4;
        let end = s[start..].find(')').map_or(s.len(), |e| start + e);
        out.push(
            s[start..end]
                .trim()
                .trim_matches(['"', '\''])
                .trim()
                .to_string(),
        );
        at = end.min(s.len());
        if at <= start {
            break;
        }
    }
    out
}

fn refuse_image(src: &str) -> String {
    format!(
        "image reference `{src}` refused: openpaperdesign-serve doesn't read local files for \
         agents. Use a data: URI (or an image already in this project's assets)."
    )
}

/// Inline SVG must not reach outside itself.
fn check_svg(raw: &str) -> Result<(), String> {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("<!doctype") || lower.contains("<!entity") {
        return Err("inline SVG with a DTD is refused".into());
    }
    if lower.contains("@import") {
        return Err("inline SVG with @import is refused".into());
    }
    let doc = roxmltree::Document::parse(raw)
        .map_err(|e| format!("inline SVG must be well-formed XML ({e})"))?;
    let ids: std::collections::HashSet<&str> = doc
        .descendants()
        .filter_map(|n| n.attribute("id"))
        .collect();
    // `#id` must name an element of this same SVG.
    let internal = |v: &str| {
        v.trim()
            .strip_prefix('#')
            .is_some_and(|id| !id.is_empty() && ids.contains(id))
    };
    for n in doc.descendants() {
        let tag = n.tag_name().name();
        if tag.eq_ignore_ascii_case("image") || tag.eq_ignore_ascii_case("feimage") {
            return Err(format!(
                "inline SVG <{tag}> is refused: put images in HTML <img src=\"data:...\"> instead"
            ));
        }
        for a in n.attributes() {
            if a.name() == "href" && !internal(a.value()) {
                return Err(format!(
                    "inline SVG reference `{}` refused: only #id of an element in the same SVG \
                     is allowed",
                    a.value().trim()
                ));
            }
            for u in css_urls(a.value()) {
                if !internal(&u) {
                    return Err(format!("inline SVG url({u}) refused"));
                }
            }
        }
        if let Some(t) = n.text() {
            for u in css_urls(t) {
                if !internal(&u) {
                    return Err(format!("inline SVG url({u}) refused"));
                }
            }
        }
    }
    Ok(())
}

fn check_html(src: &str) -> Result<(), String> {
    fn walk(nodes: &[Html]) -> Result<(), String> {
        for n in nodes {
            match n {
                Html::Text(t) => {
                    for u in css_urls(t) {
                        if !allowed_image(&u) {
                            return Err(refuse_image(&u));
                        }
                    }
                }
                Html::Element(e) => {
                    for (k, v) in &e.attrs {
                        if (k.eq_ignore_ascii_case("src") || k.eq_ignore_ascii_case("srcset"))
                            && !allowed_image(v)
                        {
                            return Err(refuse_image(v));
                        }
                        for u in css_urls(v) {
                            if !allowed_image(&u) {
                                return Err(refuse_image(&u));
                            }
                        }
                    }
                    if e.tag == "svg" {
                        check_svg(e.raw.as_deref().unwrap_or(""))?;
                    }
                    walk(&e.children)?;
                }
            }
        }
        Ok(())
    }
    walk(&html::parse(src))
}

fn check_strings(v: &Value) -> Result<(), String> {
    match v {
        Value::String(s) => {
            for u in css_urls(s) {
                if !allowed_image(&u) {
                    return Err(refuse_image(&u));
                }
            }
            Ok(())
        }
        Value::Array(a) => a.iter().try_for_each(check_strings),
        Value::Object(o) => o.values().try_for_each(check_strings),
        _ => Ok(()),
    }
}

/// Check one call against the policy. Returns the arguments to pass on (with `fileId`
/// removed once it has been checked).
pub fn check_call(name: &str, args: &Value, bound: &Bound) -> Result<Value, String> {
    if REFUSED_TOOLS.contains(&name) {
        return Err(format!(
            "`{name}` is disabled in openpaperdesign-serve: it would create or write files \
             outside the project this server is bound to"
        ));
    }
    let mut args = match args {
        Value::Object(o) => o.clone(),
        Value::Null => Map::new(),
        _ => return Err("arguments must be an object".into()),
    };
    if let Some(key) = args.get("fileId").and_then(Value::as_str)
        && !key.trim().is_empty()
        && !bound.matches(key)
    {
        return Err(format!(
            "`{key}` is not the project this server is bound to ({}); only that project can \
             be used",
            bound.name
        ));
    }
    args.remove("fileId");
    // `html` is parsed (inline SVG has its own rules); every other string is scanned as CSS.
    for (k, v) in &args {
        match (k.as_str(), v) {
            ("html", Value::String(h)) => check_html(h)?,
            _ => check_strings(v)?,
        }
    }
    Ok(Value::Object(args))
}

// ------------------------------------------------------------------------------ host

/// The upstream `exec::Host` for the bound project.
pub struct BoundHost {
    pub ed: Editor,
    project: Project,
    store: Store,
    /// Revision of each page file (and of the manifest) as the editor last read or wrote it.
    known: HashMap<String, String>,
    /// Page revisions each MCP session last saw (see the module docs, step 4).
    seen: std::collections::BTreeMap<u64, HashMap<String, String>>,
    gpu: Option<Headless>,
    gpu_tried: bool,
    /// Set when a failed save couldn't be undone: every later write is refused (shared with
    /// the browser API, see `crate::Broken`).
    broken: crate::Broken,
    /// Every page id this host has held (released by `crate::Server::retire`).
    held: crate::HeldPages,
    /// The app library's identity check, for a library canvas (see `crate::Guard`).
    guard: Option<crate::Guard>,
}

const MANIFEST_KEY: &str = "\0manifest";

/// Key of the page list (ids, names and order: the manifest's page metadata) in the revision
/// maps, observed and checked as one item.
const PAGES_KEY: &str = "\0pages";

/// The page list's revision: the hash of a canonical JSON array of `[id, name]` pairs, so
/// no two different lists share an encoding (whatever characters names contain).
fn pages_rev<'a>(pages: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let list: Vec<[&str; 2]> = pages.map(|(id, name)| [id, name]).collect();
    etag(&serde_json::to_vec(&list).expect("strings serialize"))
}

/// A listed page that has no file yet: still a page whose content can be stale.
const NO_FILE: &str = "\0none";

/// Argument key `dispatch` sets to the caller's session number (overwriting any client value);
/// stripped before the call is checked or run.
pub const SESSION_ARG: &str = "_openpaperdesignSession";
/// A new MCP session number (see [`SESSION_ARG`]).
pub fn new_session() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Sessions whose reads are remembered. Beyond this the oldest is forgotten, and must read
/// again before it changes a page (never a silent pass).
const MAX_SESSIONS: usize = 256;

/// The project's pages, read safely: ids come from the checked manifest, and `pages/` and each
/// page file are checked (no links, a single hard link) before any byte is read. Each page
/// takes its id and name from its manifest entry. Also returns the
/// revision of each page file (from the same bytes) and of the manifest.
fn read_pages(project: &Project) -> Result<(Vec<PageData>, HashMap<String, String>), String> {
    let mut revs = HashMap::new();
    let m = std::fs::read(project.dir.join(paper_rs::project::MANIFEST))
        .map_err(|e| format!("project.json: {e}"))?;
    revs.insert(MANIFEST_KEY.to_string(), etag(&m));
    let have_dir = plain(&project.dir.join(PAGES_DIR), Kind::Dir)?;
    let mut pages = Vec::new();
    // Distinct pages must be distinct files (backs up the case-folded id check).
    let mut files = std::collections::HashSet::new();
    for r in &project.manifest.pages {
        if !valid_page_id(&r.id) {
            return Err(format!("unsafe page id {:?}", r.id));
        }
        let path = project.page_path(&r.id);
        let mut page = if have_dir && plain(&path, Kind::File)? {
            let m = std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", r.id))?;
            if !files.insert(crate::dev_ino(&m)) {
                return Err(format!("page {:?} is the same file as another page", r.id));
            }
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", r.id))?;
            revs.insert(r.id.clone(), etag(&bytes));
            let text = String::from_utf8(bytes).map_err(|e| format!("{}: {e}", r.id))?;
            page_from_json(&text).map_err(|e| format!("{}: {e}", r.id))?
        } else {
            // A listed page without a file starts empty, as in upstream `load_pages`.
            revs.insert(r.id.clone(), NO_FILE.to_string());
            PageData {
                id: String::new(),
                name: r.name.clone(),
                doc: paper_rs::model::Document {
                    name: r.name.clone(),
                    ..Default::default()
                },
                camera: None,
            }
        };
        // The manifest entry owns the page's identity and name (renames only write the
        // manifest), whatever the file says; as upstream `Project::load_page`.
        page.id = r.id.clone();
        page.name = r.name.clone();
        page.doc.name = r.name.clone();
        pages.push(page);
    }
    revs.insert(
        PAGES_KEY.to_string(),
        pages_rev(
            project
                .manifest
                .pages
                .iter()
                .map(|p| (p.id.as_str(), p.name.as_str())),
        ),
    );
    Ok((pages, revs))
}

fn disk_revisions(project: &Project) -> Result<HashMap<String, String>, String> {
    read_pages(project).map(|(_, revs)| revs)
}

/// Register the project's own images: plain single-link files in `assets/` only.
fn load_assets(dir: &Path) -> Result<(), String> {
    let assets = dir.join(ASSETS);
    if !plain(&assets, Kind::Dir)? {
        return Ok(());
    }
    let rd = std::fs::read_dir(&assets).map_err(|e| format!("assets: {e}"))?;
    for e in rd.flatten() {
        if plain(&e.path(), Kind::File).unwrap_or(false)
            && let Ok(bytes) = std::fs::read(e.path())
        {
            let _ = paper_rs::assets::register(&bytes);
        }
    }
    Ok(())
}

impl BoundHost {
    pub fn open(dir: &Path) -> Result<BoundHost, String> {
        let h = BoundHost::prepare(dir, Default::default(), Default::default())?;
        let ids = h.held.lock().unwrap();
        exec::with_no_stretch(|s| s.retain(|(p, _)| !ids.contains(p)));
        drop(ids);
        Ok(h)
    }

    /// Load and validate without touching the active binding's layout registry.
    fn prepare(
        dir: &Path,
        broken: crate::Broken,
        held: crate::HeldPages,
    ) -> Result<BoundHost, String> {
        BoundHost::prepare_guarded(dir, broken, held, None)
    }

    fn prepare_guarded(
        dir: &Path,
        broken: crate::Broken,
        held: crate::HeldPages,
        guard: Option<crate::Guard>,
    ) -> Result<BoundHost, String> {
        if let Some(g) = &guard {
            g()?;
        }
        let mut h = BoundHost {
            ed: Editor::default(),
            project: Project {
                dir: dir.to_path_buf(),
                manifest: read_manifest_checked(dir)?,
            },
            // Never listed or searched: project tools are answered by the adapter.
            store: Store {
                root: dir.to_path_buf(),
            },
            known: HashMap::new(),
            seen: Default::default(),
            gpu: None,
            gpu_tried: false,
            broken,
            held,
            guard,
        };
        h.load()?;
        Ok(h)
    }

    fn note_held_pages(&self) {
        let mut held = self.held.lock().unwrap();
        held.extend(self.ed.pages.iter().map(|p| p.id.clone()));
    }

    pub fn bound(&self) -> Bound {
        Bound {
            dir: self.project.dir.clone(),
            id: self.project.manifest.id.clone(),
            name: self.project.manifest.name.clone(),
        }
    }

    /// (Re)load every page from disk. Undo history starts over.
    fn load(&mut self) -> Result<(), String> {
        self.project.manifest = read_manifest_checked(&self.project.dir)?;
        load_assets(&self.project.dir)?;
        let (pages, revs) = read_pages(&self.project)?;
        let active = self.project.manifest.active_page.clone();
        self.ed.open_pages(pages, active.as_deref());
        for i in 0..self.ed.pages.len() {
            self.ed.mark_page_saved(i);
        }
        self.known = revs;
        self.note_held_pages();
        Ok(())
    }

    /// Reload if anything changed on disk since the editor last read or wrote it.
    pub fn sync(&mut self) -> Result<(), String> {
        let manifest = read_manifest_checked(&self.project.dir)?;
        let now = disk_revisions(&Project {
            dir: self.project.dir.clone(),
            manifest,
        })?;
        if now != self.known {
            self.load()?;
        }
        Ok(())
    }

    /// Save every changed page (and the manifest when needed) like the native session does.
    /// Each written page gets a new revision (its on-disk hash).
    pub(crate) fn save_changes(&mut self) -> Result<(), crate::commit::CommitError> {
        let ed = &mut self.ed;
        ed.commit_pending();
        let dirty = ed.dirty_pages();
        if dirty.is_empty() && !ed.meta_dirty {
            return Ok(());
        }
        check_project_dir(&self.project.dir)?;
        plain(&self.project.dir.join(ASSETS), Kind::Dir)?;
        plain(&self.project.dir.join(PAGES_DIR), Kind::Dir)?;
        // Write only to safe names (listed pages come from the checked manifest, new ones get
        // upstream's random hex ids) and never through a link.
        if let Some(bad) = ed.pages.iter().find(|p| !valid_page_id(&p.id)) {
            return Err(crate::commit::CommitError::Failed(format!(
                "refusing to save a page with unsafe id {:?}",
                bad.id
            )));
        }
        let mut folded = std::collections::HashSet::new();
        if let Some(dup) = ed
            .pages
            .iter()
            .find(|p| !folded.insert(p.id.to_ascii_lowercase()))
        {
            return Err(crate::commit::CommitError::Failed(format!(
                "refusing to save: page id {:?} differs from another only in case",
                dup.id
            )));
        }
        let mut prefixes = std::collections::HashSet::new();
        if let Some(dup) = ed
            .pages
            .iter()
            .find(|p| !prefixes.insert(page_prefix(&p.id)))
        {
            return Err(crate::commit::CommitError::Failed(format!(
                "refusing to save: page id {:?} shares its first six characters with another",
                dup.id
            )));
        }
        crate::refuse_if_broken(&self.broken)?;
        // One commit for the whole call (see `crate::commit`): new images, then every changed
        // page, then the manifest; on any error every file is as it was.
        let assets_dir = self.project.dir.join(ASSETS);
        let mut writes = Vec::new();
        let mut assets = std::collections::BTreeSet::new();
        for &i in &dirty {
            let data = ed.page_data(i);
            let path = self.project.page_path(&data.id);
            plain(&path, Kind::File)?;
            for id in paper_rs::assets::used_by(&data.doc) {
                if let Some(a) = paper_rs::assets::get(&id) {
                    let path = assets_dir.join(format!("{id}.{}", a.ext));
                    if assets.insert(path.clone()) && !plain(&path, Kind::File)? {
                        writes.push(crate::commit::Write {
                            path,
                            data: a.bytes.clone(),
                        });
                    }
                }
            }
            writes.push(crate::commit::Write {
                path,
                data: paper_rs::project::format::page_to_json(&data).into_bytes(),
            });
        }
        let mut manifest = self.project.manifest.clone();
        manifest.pages = ed
            .pages
            .iter()
            .map(|s| PageRef {
                id: s.id.clone(),
                name: s.name.clone(),
            })
            .collect();
        manifest.modified = now_ms();
        writes.push(crate::commit::Write {
            path: self.project.dir.join(paper_rs::project::MANIFEST),
            data: manifest.to_json().into_bytes(),
        });
        // Images go into `assets/`, created (not through a link) if needed.
        if !plain(&assets_dir, Kind::Dir)? && writes.iter().any(|w| w.path.starts_with(&assets_dir))
        {
            std::fs::create_dir(&assets_dir).map_err(|e| format!("assets: {e}"))?;
        }
        if !plain(&self.project.dir.join(PAGES_DIR), Kind::Dir)? {
            std::fs::create_dir(self.project.dir.join(PAGES_DIR))
                .map_err(|e| format!("pages: {e}"))?;
        }
        if let Err(e) = crate::commit::commit(&writes) {
            if let crate::commit::CommitError::Broken { files, .. } = &e {
                crate::set_broken(&self.broken, &files.join(", "));
            }
            return Err(e);
        }
        for &i in &dirty {
            ed.mark_page_saved(i);
        }
        self.project.manifest = manifest;
        ed.meta_dirty = false;
        // Written. If the revisions can't be read back now, the next call's sync reloads.
        self.known = disk_revisions(&self.project).unwrap_or_default();
        self.note_held_pages();
        Ok(())
    }
}

impl exec::Host for BoundHost {
    fn editor(&mut self) -> &mut Editor {
        &mut self.ed
    }
    fn store(&self) -> &Store {
        &self.store
    }
    fn project(&self) -> Option<&Project> {
        Some(&self.project)
    }
    fn canvas_open(&self) -> bool {
        true
    }
    fn open_project(&mut self, dir: &Path) -> Result<(), String> {
        if dir.canonicalize().ok().as_deref() == Some(self.project.dir.as_path()) {
            Ok(())
        } else {
            Err("only the project this server is bound to can be opened".into())
        }
    }
    fn save(&mut self) -> Result<(), String> {
        // Changes are saved after every call anyway (see `run`).
        Ok(())
    }
    fn gpu(&mut self) -> Option<Headless> {
        if !self.gpu_tried {
            self.gpu_tried = true;
            self.gpu = Headless::new().ok();
        }
        self.gpu.as_ref().map(|g| Headless {
            device: g.device.clone(),
            queue: g.queue.clone(),
            adapter_info: g.adapter_info.clone(),
        })
    }
}

fn list_resources(host: &BoundHost) -> ToolResult {
    let p = &host.project;
    Ok(vec![paper_rs::mcp::Out::Text(
        json!({
            "items": [{
                "id": p.manifest.id,
                "type": "file",
                "name": p.manifest.name,
                "pages": p.manifest.pages.len(),
                "updatedAt": p.manifest.modified,
                "open": true,
                "active": true,
            }],
            "note": "openpaperdesign-serve is bound to this one project",
        })
        .to_string(),
    )])
}

/// Why this call may not change what it changed, if so. Each existing page whose content it
/// changed or which it removed must have been observed by `session` at its current revision,
/// and so must the page list if the call changed it (a rename, a new or removed page, a new
/// order).
fn stale_change(
    host: &mut BoundHost,
    session: u64,
    before: &HashMap<String, String>,
) -> Option<String> {
    host.ed.commit_pending();
    let empty = HashMap::new();
    let seen = host.seen.get(&session).unwrap_or(&empty);
    let ed = &host.ed;
    let now: std::collections::HashSet<&str> = ed.pages.iter().map(|p| p.id.as_str()).collect();
    let mut keys: Vec<String> = ed
        .dirty_pages()
        .into_iter()
        .map(|i| ed.pages[i].id.clone())
        .chain(
            before
                .keys()
                .filter(|k| !k.starts_with('\0') && !now.contains(k.as_str()))
                .cloned(),
        )
        .collect();
    let list = pages_rev(ed.pages.iter().map(|p| (p.id.as_str(), p.name.as_str())));
    if before.get(PAGES_KEY) != Some(&list) {
        keys.push(PAGES_KEY.to_string());
    }
    for k in keys {
        // Pages new in this call have nothing before it.
        let Some(rev) = before.get(&k) else { continue };
        let what = if k == PAGES_KEY {
            "the page list (names, order)".to_string()
        } else {
            host.project
                .manifest
                .pages
                .iter()
                .find(|p| p.id == k)
                .map_or(format!("page {k}"), |p| format!("page {} ({k})", p.name))
        };
        match seen.get(&k) {
            Some(s) if s == rev => {}
            Some(_) => {
                return Some(format!(
                    "conflict: {what} was changed by someone else (e.g. in the browser) since \
                     you last read it"
                ));
            }
            None => {
                return Some(format!(
                    "conflict: {what} hasn't been read in this MCP session yet (or the \
                     session's reads were forgotten)"
                ));
            }
        }
    }
    None
}

/// What a successful read call returned, as revision keys: the pages whose content or state
/// it returned (resolved like upstream: `pageId`, else the page the user is viewing; node
/// keys `<page prefix>:<n>` name their own page), and the page list for `get_basic_info`.
/// Calls that can't be attributed to specific pages, or that return matches or a selection
/// rather than a page's content (`find_nodes`, `get_selection`, guides, fonts, ...), return
/// nothing here, so they acknowledge nothing. See also `returned`.
fn read_keys(ed: &Editor, name: &str, args: &Value) -> Vec<String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim);
    let find = |key: &str| -> Option<usize> {
        ed.pages
            .iter()
            .position(|p| p.id == key)
            .or_else(|| {
                ed.pages
                    .iter()
                    .position(|p| p.id.starts_with(key) && key.len() >= 4)
            })
            .or_else(|| {
                ed.pages
                    .iter()
                    .position(|p| p.name.eq_ignore_ascii_case(key))
            })
    };
    let default = match s("pageId") {
        Some(p) if !p.is_empty() => find(p),
        _ => Some(ed.active_page),
    };
    let node = |key: &str| -> Option<usize> {
        let key = key.trim().trim_start_matches('#');
        match key.split_once(':') {
            Some((p, _)) => find(p),
            None => default,
        }
    };
    let root_or_node = |k: Option<&str>| match k.filter(|k| !k.is_empty() && *k != "root") {
        Some(k) => node(k),
        None => default,
    };
    let pages: Vec<Option<usize>> = match name {
        "get_basic_info" => vec![default],
        "get_node_info" | "get_jsx" | "get_fill_image" => vec![s("nodeId").and_then(node)],
        "get_children" | "get_tree_summary" | "get_screenshot" => vec![root_or_node(s("nodeId"))],
        "get_computed_styles" => match args.get("nodeIds").and_then(Value::as_array) {
            Some(ids) => ids
                .iter()
                .map(|v| match v {
                    Value::String(k) => node(k),
                    Value::Number(_) => default,
                    _ => None,
                })
                .collect(),
            None => vec![None],
        },
        _ => vec![None],
    };
    if pages.iter().any(Option::is_none) {
        return Vec::new();
    }
    let mut keys: Vec<String> = pages
        .into_iter()
        .flatten()
        .filter_map(|i| ed.pages.get(i).map(|p| p.id.clone()))
        .collect();
    if name == "get_basic_info" {
        keys.push(PAGES_KEY.to_string());
    }
    keys
}

/// Whether a successful read's output really contains page `key`, for tools whose output
/// can leave a requested page out: `get_computed_styles` answers with one map keyed by node
/// id, so a page counts only if one of its node ids is a key there.
fn returned(name: &str, out: &[paper_rs::mcp::Out], key: &str) -> bool {
    if name != "get_computed_styles" || key.starts_with('\0') {
        return true;
    }
    let prefix = page_prefix(key);
    out.iter().any(|o| match o {
        paper_rs::mcp::Out::Text(t) => serde_json::from_str::<Map<String, Value>>(t)
            .map(|m| {
                m.keys().any(|k| {
                    k.split_once(':')
                        .is_some_and(|(p, _)| p.to_ascii_lowercase() == prefix)
                })
            })
            .unwrap_or(false),
        _ => false,
    })
}

/// One tool call on the host thread, with the write lock held by the caller.
/// Tools that only read: they change nothing in the editor, so a failure needs no restore.
const READ_TOOLS: &[&str] = &[
    "list_resources",
    "get_basic_info",
    "get_selection",
    "get_node_info",
    "get_children",
    "get_tree_summary",
    "get_screenshot",
    "get_jsx",
    "get_computed_styles",
    "get_fill_image",
    "find_nodes",
    "get_font_family_info",
    "get_guide",
];

/// The editor state a call can change, copied before a call that may change it, and put back
/// when it fails: every page's document and undo history (the active page's in the editor,
/// the others parked in their slots), the page list, selection, view and active page, and
/// upstream's process-wide stretch record for these pages.
struct EditorSnapshot {
    doc: paper_rs::model::Document,
    history: paper_rs::history::History,
    camera: paper_rs::geom::Camera,
    selection: Vec<paper_rs::model::NodeId>,
    active_page: usize,
    pages: Vec<paper_rs::editor::pages::PageSlot>,
    meta_dirty: bool,
    doc_stamp: u64,
    revision: u64,
    /// Upstream's process-wide record of containers whose children don't stretch
    /// (`exec::with_no_stretch`), for this project's pages only: other projects in the same
    /// process keep theirs.
    no_stretch: Vec<(String, paper_rs::model::NodeId)>,
}

impl EditorSnapshot {
    fn take(ed: &Editor) -> EditorSnapshot {
        EditorSnapshot {
            doc: ed.doc.clone(),
            history: ed.history.clone(),
            camera: ed.camera,
            selection: ed.selection.clone(),
            active_page: ed.active_page,
            pages: ed.pages.clone(),
            meta_dirty: ed.meta_dirty,
            doc_stamp: ed.doc_stamp,
            revision: ed.revision,
            no_stretch: exec::with_no_stretch(|s| {
                s.iter()
                    .filter(|(p, _)| ed.pages.iter().any(|q| q.id == *p))
                    .cloned()
                    .collect()
            }),
        }
    }

    fn restore(self, ed: &mut Editor) {
        // The pages before the call and any it added: their entries become what they were.
        let ours: std::collections::HashSet<String> = ed
            .pages
            .iter()
            .chain(self.pages.iter())
            .map(|p| p.id.clone())
            .collect();
        exec::with_no_stretch(|s| {
            s.retain(|(p, _)| !ours.contains(p));
            s.extend(self.no_stretch.iter().cloned());
        });
        ed.doc = self.doc;
        ed.history = self.history;
        ed.camera = self.camera;
        ed.selection = self.selection;
        ed.active_page = self.active_page;
        ed.pages = self.pages;
        ed.meta_dirty = self.meta_dirty;
        ed.doc_stamp = self.doc_stamp;
        // Views are told to redraw.
        ed.revision = self.revision.max(ed.revision) + 1;
    }
}

/// What a call acknowledged for its session: applied only once its result was delivered
/// (see `spawn_host`).
pub struct Ack {
    session: u64,
    entries: Vec<(String, String)>,
}

impl BoundHost {
    fn acknowledge(&mut self, a: Ack) {
        if !self.seen.contains_key(&a.session) && self.seen.len() >= MAX_SESSIONS {
            // Forgetting fails closed: that session must read again before changing a page.
            self.seen.pop_first();
        }
        self.seen.entry(a.session).or_default().extend(a.entries);
    }
}

/// One tool call, acknowledged at once (for callers that deliver the result themselves).
pub fn run(host: &mut BoundHost, root_id: (u64, u64), name: &str, args: &Value) -> ToolResult {
    let (r, ack) = execute(host, root_id, name, args);
    if let Some(a) = ack {
        host.acknowledge(a);
    }
    r
}

/// One tool call on the host thread, with the write lock held by the caller. Returns the
/// result and what it would acknowledge for its session.
pub fn execute(
    host: &mut BoundHost,
    root_id: (u64, u64),
    name: &str,
    args: &Value,
) -> (ToolResult, Option<Ack>) {
    #[cfg(test)]
    tests::slow_down(&host.project.dir, name);
    match execute_inner(host, root_id, name, args) {
        Ok(v) => v,
        Err(e) => (Err(e), None),
    }
}

#[allow(clippy::type_complexity)]
fn execute_inner(
    host: &mut BoundHost,
    root_id: (u64, u64),
    name: &str,
    args: &Value,
) -> Result<(ToolResult, Option<Ack>), String> {
    let dir = host.project.dir.clone();
    check_root(&dir, root_id)?;
    if let Some(g) = &host.guard {
        g()?;
    }
    check_project_dir(&dir)?;
    host.sync()?;
    let mut args = args.clone();
    let session = args
        .as_object_mut()
        .and_then(|a| a.remove(SESSION_ARG))
        .and_then(|v| v.as_u64());
    let before = host.known.clone();
    let args = check_call(name, &args, &host.bound())?;
    // Worked out before the call, on the pages as they are; used only if it succeeds.
    let reads = read_keys(&host.ed, name, &args);
    // What a failed call restores: the editor as it was (documents and undo histories of
    // every page, selection, view, page list). Reads don't change it, so they need none.
    let snap = (!READ_TOOLS.contains(&name)).then(|| EditorSnapshot::take(&host.ed));
    #[cfg(test)]
    crate::test_support::hit(&dir, "snapshot");
    let undo = |host: &mut BoundHost, snap: Option<EditorSnapshot>| -> Result<(), String> {
        match snap {
            Some(s) => s.restore(&mut host.ed),
            // A read that changed something after all: back to what is on disk.
            None if host.ed.project_dirty() => host.load()?,
            None => {}
        }
        Ok(())
    };
    // Nothing reaches the disk during the call (`Host::save` is a no-op, export and file
    // creation are refused): changes are saved below, only after a successful result.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match name {
        "list_resources" => list_resources(host),
        "open_file" => {
            // Upstream resolves an absolute project path itself: hand it only ours.
            let mut a = args.as_object().cloned().unwrap_or_default();
            a.insert("fileId".into(), json!(dir.to_string_lossy()));
            exec::call(host, name, &Value::Object(a))
        }
        _ => exec::call(host, name, &args),
    }))
    .map_err(|_| ());
    let r = match r {
        Ok(r) => r,
        Err(()) => {
            // A panic may have left the editor mid-operation: reload what is on disk (which it
            // matched before the call). The undo history restarts; the stretch record is put
            // back from the copy.
            if let Some(s) = snap {
                s.restore(&mut host.ed);
            }
            host.load()?;
            return Ok((Err(format!("{name} failed unexpectedly")), None));
        }
    };
    if r.is_err() {
        // A call that fails changes nothing: put the editor back as it was, save nothing and
        // acknowledge nothing.
        undo(host, snap)?;
        return Ok((r, None));
    }
    if let Some(s) = session
        && let Some(why) = stale_change(host, s, &before)
    {
        // Drop the change: it wasn't decided on the page as it is now.
        undo(host, snap)?;
        return Ok((
            Err(format!(
                "{why}, so this change was not applied and nothing was written. Read every page \
             you change (get_tree_summary with nodeId \"root\" and its pageId, or get_node_info \
             on its nodes; get_basic_info for the page list) and redo the change on what is \
             there now."
            )),
            None,
        ));
    }
    let saved = host.save_changes();
    if saved.is_err() {
        // Not saved: drop the call's changes, as for a failed call.
        undo(host, snap)?;
    }
    // Only a call that succeeded acknowledges anything: a failed or refused call, or a
    // failed save, leaves what the session has observed as it was.
    let ack = match (session, &saved, &r) {
        (Some(s), Ok(()), Ok(out)) => {
            let entries = if before == host.known {
                // Nothing written: exactly the pages (and page list) it returned.
                reads
                    .iter()
                    .filter(|k| returned(name, out, k))
                    .filter_map(|k| host.known.get(k).map(|v| (k.clone(), v.clone())))
                    .collect()
            } else {
                // What this call wrote is observed as written; nothing else.
                host.known
                    .iter()
                    .filter(|(k, v)| *k != MANIFEST_KEY && before.get(*k) != Some(*v))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            Some(Ack {
                session: s,
                entries,
            })
        }
        _ => None,
    };
    Ok((
        match saved {
            Ok(()) => r,
            Err(e) => Err(format!("the change was {}", e.describe())),
        },
        ack,
    ))
}

/// A tool call queued for the host thread. Its state moves Queued → Running → (done), or
/// Queued → Abandoned when the caller stopped waiting before it started: an abandoned call
/// is never run. A call that has started is always waited for, however long it takes.
pub struct Job {
    pub name: String,
    pub args: Value,
    pub reply: tokio::sync::oneshot::Sender<ToolResult>,
    state: Arc<AtomicU8>,
}

/// All senders, including retained HTTP sessions, share the binding's admission fence.
#[derive(Clone)]
pub struct Calls {
    tx: Sender<Option<Job>>,
    lifecycle: Arc<crate::lifecycle::Lifecycle>,
    #[cfg(test)]
    dir: PathBuf,
}

impl Calls {
    pub(crate) fn stop(&self) {
        // Wake an idle worker too. Jobs already in the queue are refused under the fence;
        // a sender racing with this message either gets the refusal or a closed channel.
        let _ = self.tx.send(None);
    }
}

const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const ABANDONED: u8 = 2;

/// Queue a call and wait for its result. `start_within` bounds only the wait for the call
/// to start; once it runs, its real result is returned.
pub(crate) async fn send_and_wait(
    calls: &Calls,
    name: String,
    args: Value,
    start_within: Duration,
) -> ToolResult {
    calls.lifecycle.check_active()?;
    let (reply, mut rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(AtomicU8::new(QUEUED));
    let job = Job {
        name,
        args,
        reply,
        state: state.clone(),
    };
    if calls.tx.send(Some(job)).is_err() {
        return Err("the project host has stopped".into());
    }
    #[cfg(test)]
    crate::test_support::hit(&calls.dir, "queued");
    match tokio::time::timeout(start_within, &mut rx).await {
        Ok(r) => r.unwrap_or_else(|_| Err("the project host dropped the call".into())),
        Err(_) => {
            if state
                .compare_exchange(QUEUED, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                Err(
                    "the call could not start in time (the project was busy); it was not run \
                     and changed nothing"
                        .into(),
                )
            } else {
                rx.await
                    .unwrap_or_else(|_| Err("the project host dropped the call".into()))
            }
        }
    }
}

/// Prepare the host thread without resetting layout state. Once activated, calls run one
/// at a time under the binding's writer fence; a retired binding never executes another job.
#[allow(dead_code)]
pub(crate) fn spawn_host(
    dir: PathBuf,
    root_id: (u64, u64),
    lifecycle: Arc<crate::lifecycle::Lifecycle>,
    broken: crate::Broken,
    held: crate::HeldPages,
) -> Result<Calls, String> {
    spawn_host_guarded(dir, root_id, lifecycle, broken, held, None)
}

/// [`spawn_host`] with the app library's guard (see `crate::Guard`), checked before the
/// project is loaded and before every call.
pub(crate) fn spawn_host_guarded(
    dir: PathBuf,
    root_id: (u64, u64),
    lifecycle: Arc<crate::lifecycle::Lifecycle>,
    broken: crate::Broken,
    held: crate::HeldPages,
    guard: Option<crate::Guard>,
) -> Result<Calls, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<Job>>();
    let calls = Calls {
        tx,
        lifecycle: lifecycle.clone(),
        #[cfg(test)]
        dir: dir.clone(),
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("opd-mcp-host".into())
        .spawn(move || {
            let host = BoundHost::prepare_guarded(&dir, broken, held, guard);
            let mut host = match host {
                Ok(h) => {
                    let _ = ready_tx.send(Ok(()));
                    h
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            while let Ok(Some(job)) = rx.recv() {
                let _g = lifecycle.write.blocking_lock();
                if let Err(e) = lifecycle.check_active() {
                    let _ = job.reply.send(Err(e));
                    continue;
                }
                // Waiting for the lock still counts as queued: a call abandoned meanwhile
                // is skipped entirely.
                if job
                    .state
                    .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    continue;
                }
                let (r, ack) = execute(&mut host, root_id, &job.name, &job.args);
                // Reads are acknowledged only if the result reached the caller.
                if job.reply.send(r).is_ok()
                    && let Some(a) = ack
                {
                    host.acknowledge(a);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    ready_rx
        .recv()
        .map_err(|_| "the MCP host thread stopped".to_string())??;
    Ok(calls)
}

// ------------------------------------------------------------------------------ MCP

/// The tools offered: upstream's catalogue without the refused ones.
pub fn offered_tools() -> Vec<Tool> {
    tool_list()
        .into_iter()
        .filter(|t| !REFUSED_TOOLS.contains(&t.name.as_ref()))
        .collect()
}

/// Run one tool call on the project's host thread (policy, save and revision included).
/// The timeout bounds only the wait for the call to start (see `Job`).
pub async fn dispatch(calls: &Calls, session: u64, name: String, args: Value) -> ToolResult {
    let mut args = match args {
        Value::Object(a) => a,
        _ => Map::new(),
    };
    args.insert(SESSION_ARG.into(), json!(session));
    send_and_wait(calls, name, Value::Object(args), CALL_TIMEOUT).await
}

#[derive(Clone)]
pub struct AgentServer {
    calls: Arc<Mutex<Calls>>,
    /// One per MCP session (the service makes a handler per session).
    session: u64,
}

impl AgentServer {
    pub fn new(calls: Calls) -> AgentServer {
        AgentServer {
            calls: Arc::new(Mutex::new(calls)),
            session: new_session(),
        }
    }
}

impl ServerHandler for AgentServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "openpaperdesign",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "{}\n\nopenpaperdesign-serve: these tools edit the one project this server is \
                 bound to; a person may be editing it in a browser at the same time and sees \
                 your changes live. Projects can't be created or switched, export is disabled, \
                 and images must be data: URIs.",
                tools::INSTRUCTIONS
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(offered_tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let calls = self.calls.lock().unwrap().clone();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let r = dispatch(&calls, self.session, request.name.to_string(), args).await;
        Ok(to_result(r).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paper_rs::model::NodeId;

    // ------------------------------------------------- test-only seams (never in builds)

    /// Projects whose calls of a given tool are slowed down (a long-running call).
    static SLOW: Mutex<Vec<(PathBuf, String, Duration)>> = Mutex::new(Vec::new());

    pub(super) fn slow_down(dir: &Path, name: &str) {
        let d = SLOW
            .lock()
            .unwrap()
            .iter()
            .find(|(p, n, _)| p == dir && n == name)
            .map(|(_, _, d)| *d);
        if let Some(d) = d {
            std::thread::sleep(d);
        }
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A project with two pages (each with one rectangle), opened by a host.
    fn two_page_host() -> (Tmp, BoundHost, (u64, u64), String, String) {
        use std::hash::{BuildHasher, RandomState};
        let root = std::env::temp_dir().join(format!(
            "opd-unit-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (p, _) = Store::open(&root).unwrap().create("Unit").unwrap();
        let dir = p.dir.canonicalize().unwrap();
        let root_id = crate::dev_ino(&std::fs::symlink_metadata(&dir).unwrap());
        let mut h = BoundHost::open(&dir).unwrap();
        let a = h.project.manifest.pages[0].id.clone();
        run(&mut h, root_id, "create_page", &json!({"name": "B"})).unwrap();
        let b = h.project.manifest.pages[1].id.clone();
        for page in [&a, &b] {
            run(
                &mut h,
                root_id,
                "create_shape",
                &json!({"type": "rectangle", "x": 0, "y": 0, "width": 9, "height": 9, "pageId": page}),
            )
            .unwrap();
        }
        (Tmp(root), h, root_id, a, b)
    }

    /// Every file under `dir` with its bytes.
    fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files(&p));
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
        out.sort();
        out
    }

    fn rect(page: &str) -> String {
        format!("{}:1", &page[..6])
    }

    /// Review round 4, F1: a save that fails at any step (staging any file, renaming any
    /// file) leaves every project file byte-identical, for a one-page commit (page and
    /// manifest), a two-page commit, and a manifest-only commit; and the editor drops the
    /// change.
    #[test]
    fn a_failed_save_leaves_every_file_unchanged() {
        let (t, mut h, root_id, a, b) = two_page_host();
        let one_page = json!({"updates": [{"nodeIds": [rect(&a)], "styles": {"width": "20px"}}]});
        let two_pages = json!({"updates": [
            {"nodeIds": [rect(&a)], "styles": {"width": "30px"}},
            {"nodeIds": [rect(&b)], "styles": {"width": "30px"}},
        ]});
        let rename = json!({"updates": [{"pageId": a, "name": "Renamed"}]});
        // Steps: one staging per file, one rename per file, then one sync per directory
        // (pages/ and the project folder; the manifest alone: the project folder).
        for (tool, args, steps) in [
            ("update_styles", &one_page, 6),
            ("update_styles", &two_pages, 8),
            ("rename_pages", &rename, 3),
        ] {
            for at in 0..steps {
                let before = files(&t.0);
                crate::commit::fault::fail_at(&[at]);
                let r = run(&mut h, root_id, tool, args);
                crate::commit::fault::fail_at(&[]);
                assert!(r.is_err(), "{tool} step {at}: {r:?}");
                assert_eq!(files(&t.0), before, "{tool} step {at}: files unchanged");
                // In memory too: the page list and the rectangle are as before.
                assert_eq!(h.ed.pages[0].name, "Page 1", "{tool} step {at}");
                let info = run(
                    &mut h,
                    root_id,
                    "get_node_info",
                    &json!({"nodeId": rect(&a)}),
                )
                .unwrap();
                let Some(paper_rs::mcp::Out::Text(info)) = info.first() else {
                    panic!()
                };
                assert!(info.contains("\"width\": 9.0"), "{tool} step {at}: {info}");
            }
            // Without a failure the same call goes through.
            run(&mut h, root_id, tool, args).unwrap();
            run(
                &mut h,
                root_id,
                "rename_pages",
                &json!({"updates": [{"pageId": a, "name": "Page 1"}]}),
            )
            .unwrap();
            run(
                &mut h,
                root_id,
                "update_styles",
                &json!({"updates": [
                    {"nodeIds": [rect(&a)], "styles": {"width": "9px"}},
                    {"nodeIds": [rect(&b)], "styles": {"width": "9px"}},
                ]}),
            )
            .unwrap();
        }
        // No temporary files are left behind.
        assert!(
            files(&t.0)
                .iter()
                .all(|(p, _)| !p.to_string_lossy().ends_with(".tmp")),
            "{:?}",
            files(&t.0)
        );
    }

    /// Review round 4, F1: if undoing a failed save fails too, every later write is refused
    /// (fail closed), naming the files; reads still work.
    #[test]
    fn a_failed_rollback_refuses_every_later_write() {
        let (t, mut h, root_id, a, b) = two_page_host();
        let two_pages = json!({"updates": [
            {"nodeIds": [rect(&a)], "styles": {"width": "30px"}},
            {"nodeIds": [rect(&b)], "styles": {"width": "30px"}},
        ]});
        // Steps 0-2 stage A, B and the manifest; 3 renames A; 4 (B's rename) fails; 5 is
        // the staging of A's restore, which fails too.
        crate::commit::fault::fail_at(&[4, 5]);
        let r = run(&mut h, root_id, "update_styles", &two_pages);
        crate::commit::fault::fail_at(&[]);
        // Review round 5, F3: not "nothing was written", but which files may be inconsistent
        // and that writes are disabled.
        let e = r.unwrap_err();
        assert!(
            e.contains("may now be inconsistent")
                && e.contains(&format!("{a}.json"))
                && e.contains("disabled")
                && !e.contains("nothing was written"),
            "{e}"
        );
        assert!(h.broken.lock().unwrap().is_some());
        let before = files(&t.0);
        let r = run(
            &mut h,
            root_id,
            "update_styles",
            &json!({"updates": [{"nodeIds": [rect(&b)], "styles": {"width": "40px"}}]}),
        );
        assert!(r.unwrap_err().contains("writes are disabled"));
        assert_eq!(files(&t.0), before, "nothing written");
        assert!(run(&mut h, root_id, "get_basic_info", &json!({})).is_ok());
    }

    /// Review round 5, F1: a failing directory sync is an error, not a success: after the
    /// renames, the sync of pages/ fails, the files are put back; and when the directory
    /// sync of that rollback fails too, the server fails closed.
    #[test]
    fn directory_sync_failures_roll_back_or_fail_closed() {
        let (t, mut h, root_id, a, _) = two_page_host();
        let change = json!({"updates": [{"nodeIds": [rect(&a)], "styles": {"width": "30px"}}]});
        // 0-1 stage page and manifest, 2-3 rename them, 4 syncs pages/: fails.
        let before = files(&t.0);
        crate::commit::fault::fail_at(&[4]);
        let r = run(&mut h, root_id, "update_styles", &change);
        crate::commit::fault::fail_at(&[]);
        let e = r.unwrap_err();
        assert!(
            e.contains("nothing was written") && e.contains("directory sync"),
            "{e}"
        );
        assert_eq!(files(&t.0), before, "the renamed files were put back");
        assert!(h.broken.lock().unwrap().is_none());
        // Again, and the rollback's directory sync (after restoring both files: 5-8) fails.
        crate::commit::fault::fail_at(&[4, 9]);
        let r = run(&mut h, root_id, "update_styles", &change);
        crate::commit::fault::fail_at(&[]);
        let e = r.unwrap_err();
        assert!(
            e.contains("may now be inconsistent") && e.contains("disabled"),
            "{e}"
        );
        assert!(h.broken.lock().unwrap().is_some());
        assert!(
            run(&mut h, root_id, "update_styles", &change)
                .unwrap_err()
                .contains("writes are disabled")
        );
    }

    fn stretch_entries(page: &str) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = exec::with_no_stretch(|s| {
            s.iter()
                .filter(|(p, _)| p == page)
                .map(|(_, n)| *n)
                .collect()
        });
        v.sort();
        v
    }

    fn frame(h: &mut BoundHost, root_id: (u64, u64), align: &str) -> String {
        let out = run(
            h,
            root_id,
            "create_artboard",
            &json!({"name": "F", "styles": {"display": "flex", "flexDirection": "column", "alignItems": align, "width": "300px", "height": "200px"}}),
        )
        .unwrap();
        let Some(paper_rs::mcp::Out::Text(t)) = out.first() else {
            panic!()
        };
        serde_json::from_str::<Value>(t).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn child_width(h: &mut BoundHost, root_id: (u64, u64), frame: &str) -> f64 {
        let out = run(
            h,
            root_id,
            "write_html",
            &json!({"targetNodeId": frame, "mode": "insert-children", "html": "<div style='height:20px'></div>"}),
        )
        .unwrap();
        let Some(paper_rs::mcp::Out::Text(t)) = out.first() else {
            panic!()
        };
        let v: Value = serde_json::from_str(t).unwrap();
        let id = v["createdNodeIds"][0]
            .as_str()
            .unwrap_or_else(|| panic!("{t}"))
            .to_string();
        let info = run(h, root_id, "get_node_info", &json!({"nodeId": id})).unwrap();
        let Some(paper_rs::mcp::Out::Text(t)) = info.first() else {
            panic!()
        };
        serde_json::from_str::<Value>(t).unwrap()["width"]
            .as_f64()
            .unwrap()
    }

    /// Final review, P2 1, failed-save variant: a change whose save fails leaves upstream's
    /// stretch record as it was, so the next child still stretches.
    #[test]
    fn a_failed_save_leaves_layout_state_unchanged() {
        let (_t, mut h, root_id, _, _) = two_page_host();
        let f = frame(&mut h, root_id, "stretch");
        crate::commit::fault::fail_at(&[0]);
        let r = run(
            &mut h,
            root_id,
            "update_styles",
            &json!({"updates": [{"nodeIds": [f], "styles": {"alignItems": "flex-start", "width": "450px"}}]}),
        );
        crate::commit::fault::fail_at(&[]);
        assert!(r.is_err());
        assert_eq!(child_width(&mut h, root_id, &f), 300.0);
    }

    /// Final review, P2 1: restoring one host's stretch record touches only its own pages'
    /// entries; another host's (in the same process), including one added meanwhile, stay.
    #[test]
    fn restoring_layout_state_leaves_other_hosts_alone() {
        let (_ta, mut a, ra, a_page, _) = two_page_host();
        let (_tb, mut b, rb, b_page, _) = two_page_host();
        let fb = frame(&mut b, rb, "flex-start");
        let b_before = stretch_entries(&b_page);
        assert_eq!(b_before.len(), 1);
        let fa = frame(&mut a, ra, "stretch");
        assert!(stretch_entries(&a_page).is_empty());
        // A's copy, then B adds another entry, then A's failed call adds one, then A restores.
        let snap = EditorSnapshot::take(&a.ed);
        let fb2 = frame(&mut b, rb, "flex-end");
        let fa_n: u64 = fa.split(':').nth(1).unwrap().parse().unwrap();
        exec::with_no_stretch(|s| s.insert((a_page.clone(), NodeId(fa_n))));
        snap.restore(&mut a.ed);
        assert!(stretch_entries(&a_page).is_empty(), "A's entry rolled back");
        let b_after = stretch_entries(&b_page);
        assert_eq!(
            b_after.len(),
            2,
            "B's entries kept, the new one too: {b_after:?}"
        );
        for f in [&fb, &fb2] {
            let n: u64 = f.split(':').nth(1).unwrap().parse().unwrap();
            assert!(b_after.contains(&NodeId(n)));
        }
    }

    /// Operator Mac finding on cffeec6: a project opened in a process where an earlier
    /// binding left stretch entries for the same page ids starts fresh; other page ids keep
    /// theirs; and a project that fails to open clears nothing.
    #[test]
    fn a_new_binding_starts_with_fresh_layout_state() {
        let (t, h, _, a, _) = two_page_host();
        let dir = h.project.dir.clone();
        drop(h);
        let other = format!("other-{a}");
        exec::with_no_stretch(|s| {
            s.insert((a.clone(), NodeId(1)));
            s.insert((other.clone(), NodeId(1)));
        });
        // A failed open (the manifest is unreadable) leaves everything as it was.
        let manifest = dir.join(paper_rs::project::MANIFEST);
        let good = std::fs::read(&manifest).unwrap();
        std::fs::write(&manifest, "not json").unwrap();
        assert!(BoundHost::open(&dir).is_err());
        assert_eq!(stretch_entries(&a), vec![NodeId(1)]);
        std::fs::write(&manifest, good).unwrap();
        let _h = BoundHost::open(&dir).unwrap();
        assert!(
            stretch_entries(&a).is_empty(),
            "fresh for the opened project's pages"
        );
        assert_eq!(
            stretch_entries(&other),
            vec![NodeId(1)],
            "other pages untouched"
        );
        exec::with_no_stretch(|s| s.retain(|(p, _)| p != &other));
        drop(t);
    }

    fn host_thread() -> (Tmp, Calls, Arc<tokio::sync::Mutex<()>>, PathBuf, String) {
        let (t, h, root_id, a, _) = two_page_host();
        let dir = h.project.dir.clone();
        drop(h);
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::default());
        let held = crate::HeldPages::default();
        let lock = lifecycle.write.clone();
        let calls = spawn_host(
            dir.clone(),
            root_id,
            lifecycle.clone(),
            Default::default(),
            held.clone(),
        )
        .unwrap();
        lifecycle.activate(&held);
        (t, calls, lock, dir, a)
    }

    fn with_session(args: Value, s: u64) -> Value {
        let mut a = args.as_object().cloned().unwrap();
        a.insert(SESSION_ARG.into(), json!(s));
        Value::Object(a)
    }

    /// Review round 4, F4: a read that timed out before it started is never run, so it
    /// acknowledges nothing and a following blind write is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_call_abandoned_before_it_started_is_never_run() {
        let (_t, calls, lock, _dir, a) = host_thread();
        let s = new_session_for_test();
        let read = with_session(json!({"nodeId": "root", "pageId": a}), s);
        // The project is busy (another writer holds the lock) past the wait.
        let busy = lock.lock().await;
        let r = send_and_wait(
            &calls,
            "get_tree_summary".into(),
            read,
            Duration::from_millis(100),
        )
        .await;
        assert!(r.unwrap_err().contains("could not start in time"));
        drop(busy);
        let write = with_session(
            json!({"updates": [{"nodeIds": [rect(&a)], "styles": {"width": "50px"}}]}),
            s,
        );
        let r = send_and_wait(
            &calls,
            "update_styles".into(),
            write,
            Duration::from_secs(30),
        )
        .await;
        assert!(r.unwrap_err().contains("hasn't been read"));
    }

    /// Review round 4, F4: once a call has started, its real result is returned, however
    /// long it takes (no timeout after the start).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_started_call_returns_its_real_result() {
        let (_t, calls, _lock, dir, a) = host_thread();
        SLOW.lock()
            .unwrap()
            .push((dir, "get_tree_summary".into(), Duration::from_millis(400)));
        let s = new_session_for_test();
        let read = with_session(json!({"nodeId": "root", "pageId": a}), s);
        let r = send_and_wait(
            &calls,
            "get_tree_summary".into(),
            read,
            Duration::from_millis(100),
        )
        .await;
        assert!(r.is_ok(), "{r:?}");
        // It was delivered, so it counts as a read.
        let write = with_session(
            json!({"updates": [{"nodeIds": [rect(&a)], "styles": {"width": "50px"}}]}),
            s,
        );
        let r = send_and_wait(
            &calls,
            "update_styles".into(),
            write,
            Duration::from_secs(30),
        )
        .await;
        assert!(r.is_ok(), "{r:?}");
    }

    /// Review round 4, F4: a result that can't be delivered (the caller is gone) is not
    /// acknowledged.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_undelivered_read_acknowledges_nothing() {
        let (_t, calls, _lock, _dir, a) = host_thread();
        let s = new_session_for_test();
        let (reply, rx) = tokio::sync::oneshot::channel();
        drop(rx);
        calls
            .tx
            .send(Some(Job {
                name: "get_tree_summary".into(),
                args: with_session(json!({"nodeId": "root", "pageId": a}), s),
                reply,
                state: Arc::new(AtomicU8::new(QUEUED)),
            }))
            .unwrap();
        let write = with_session(
            json!({"updates": [{"nodeIds": [rect(&a)], "styles": {"width": "50px"}}]}),
            s,
        );
        let r = send_and_wait(
            &calls,
            "update_styles".into(),
            write,
            Duration::from_secs(30),
        )
        .await;
        assert!(r.unwrap_err().contains("hasn't been read"));
    }

    fn new_session_for_test() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 40);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn bound() -> Bound {
        Bound {
            dir: PathBuf::from("/nonexistent/bound"),
            id: "abc".into(),
            name: "Bound".into(),
        }
    }

    #[test]
    fn refused_tools_and_other_projects() {
        for t in REFUSED_TOOLS {
            assert!(check_call(t, &json!({}), &bound()).is_err(), "{t}");
            assert!(!offered_tools().iter().any(|x| x.name == *t), "{t} offered");
        }
        assert!(check_call("get_basic_info", &json!({"fileId": "abc"}), &bound()).is_ok());
        assert!(check_call("get_basic_info", &json!({"fileId": "Bound"}), &bound()).is_ok());
        for other in ["other", "/tmp", "../x", "/file/zzz"] {
            assert!(
                check_call("get_basic_info", &json!({"fileId": other}), &bound()).is_err(),
                "{other}"
            );
        }
        let a = check_call("get_basic_info", &json!({"fileId": "abc"}), &bound()).unwrap();
        assert!(a.get("fileId").is_none(), "checked fileId is removed");
    }

    #[test]
    fn local_image_paths_are_refused() {
        let b = bound();
        let html = |h: &str| check_call("write_html", &json!({"html": h}), &b);
        let style = |s: &str| {
            check_call(
                "update_styles",
                &json!({"updates": [{"nodeIds": ["1"], "styles": {"background-image": s}}]}),
                &b,
            )
        };
        for bad in [
            "/etc/passwd",
            "../secret.png",
            "file:///etc/passwd",
            "paper-asset:///home/x/a.png",
            "a.png",
            "#frag",
            "DATA:image/png;base64,AA==",
        ] {
            assert!(html(&format!("<img src=\"{bad}\">")).is_err(), "img {bad}");
            assert!(
                html(&format!("<div style=\"background:url('{bad}')\"></div>")).is_err(),
                "css {bad}"
            );
            assert!(
                style(&format!("url({bad})")).is_err(),
                "update_styles {bad}"
            );
        }
        // Entity-encoded attributes are checked after decoding.
        assert!(html("<div style=\"background:u&#114;l(/etc/passwd)\"></div>").is_err());
        assert!(html("<IMG SRC=/etc/passwd>").is_err());
        // Allowed: data: URIs and remote URLs (upstream never fetches those).
        assert!(html("<img src=\"data:image/png;base64,iVBORw0KGgo=\">").is_ok());
        assert!(style("url(data:image/png;base64,iVBORw0KGgo=)").is_ok());
        assert!(html("<div style=\"color:red\">no images</div>").is_ok());
    }

    #[test]
    fn inline_svg_must_stay_inside_itself() {
        let b = bound();
        let svg = |s: &str| check_call("write_html", &json!({"html": s}), &b);
        assert!(svg("<svg viewBox=\"0 0 10 10\"><rect width=\"10\" height=\"10\" fill=\"url(#g)\"/><defs><linearGradient id=\"g\"/></defs></svg>").is_ok());
        for bad in [
            "<svg><image href=\"/etc/passwd\"/></svg>",
            "<svg xmlns:xlink=\"http://www.w3.org/1999/xlink\"><image xlink:href=\"file:///etc/x.png\"/></svg>",
            "<svg><image href=\"../x.png\"/></svg>",
            "<svg><rect style=\"fill:url(/etc/x)\"/></svg>",
            "<svg><style>@import url(x.css);</style></svg>",
            "<!DOCTYPE svg [<!ENTITY x SYSTEM \"file:///etc/passwd\">]><svg>&x;</svg>",
            "<svg><rect></svg>",
        ] {
            assert!(svg(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn svg_image_elements_are_refused_and_references_stay_inside() {
        let b = bound();
        let svg = |s: &str| check_call("write_html", &json!({"html": s}), &b);
        // Review round 1 finding 2 (`#private.png`) and round 2 finding 1 (usvg reads a
        // malformed data: href as a file name): no image element is accepted at all.
        for bad in [
            "<svg xmlns='http://www.w3.org/2000/svg' width='30' height='20'><image href='#private.png' width='30' height='20'/></svg>",
            "<svg xmlns='http://www.w3.org/2000/svg' width='30' height='20'><image href='data:private.png' width='30' height='20'/></svg>",
            "<svg><image href='data:'/></svg>",
            "<svg><image href=' data:image/png;base64,!!!'/></svg>",
            "<svg><image href=\"data:image/png;base64,iVBORw0KGgo=\"/></svg>",
            "<svg xmlns:xlink=\"http://www.w3.org/1999/xlink\"><image xlink:href=\"data:private.png\"/></svg>",
            "<svg><filter id=\"f\"><feImage href=\"data:private.png\"/></filter></svg>",
            "<svg><filter id=\"f\"><FEIMAGE href=\"#f\"/></filter></svg>",
            "<svg><use href=\"data:private.png\"/></svg>",
            "<svg><use href=\"#missing.png\"/></svg>",
            "<svg><rect fill=\"url(#missing.png)\"/></svg>",
            "<svg><rect fill=\"url(data:private.png)\"/></svg>",
            "<svg><use href=\"#\"/></svg>",
        ] {
            assert!(svg(bad).is_err(), "{bad}");
        }
        assert!(svg("<svg><defs><rect id=\"r\" width=\"5\" height=\"5\"/></defs><use href=\"#r\"/></svg>").is_ok());
        assert!(svg("<svg viewBox=\"0 0 10 10\"><defs><linearGradient id=\"g\"/></defs><rect width=\"10\" height=\"10\" fill=\"url(#g)\"/></svg>").is_ok());
    }
}
