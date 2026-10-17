//! The portable canvas file: one bounded JSON document with a library canvas's saved content.
//!
//! ```text
//! {
//!   "format": "openpaperdesign-canvas", "version": 1, "content": "canvas-only",
//!   "note": "...",                         optional, ignored on import
//!   "manifest": { project.json },          name, pages (ids, names, order), active page
//!   "pages": [ { page file }, ... ],       exactly one per manifest page, in manifest order
//!   "assets": [ {"id", "ext", "data"} ]    exactly the images the pages use, base64
//! }
//! ```
//!
//! Version 1 is canvas-only: it carries no design direction (`DESIGN.md`/`design.md`), tokens
//! or other project files, and a file that offers such companions is refused rather than
//! partly imported. Import validates the whole file before anything is written. No record
//! names a path: page files are `pages/<checked page id>.json` and images
//! `assets/<content hash>.<ext>`. The contained manifest and pages must be exactly the current
//! upstream formats (`paper-rs-project` version 1, `paper-rs-page` version 3), checked on the
//! raw JSON before any upstream parsing or migration; older or legacy versions aren't
//! supported in canvas files. Each page must also say nothing upstream's parser would drop:
//! its JSON must match what upstream writes back from it (numbers by value, node order by id,
//! and only the writer's known omissions explicit at their defaults), so an unknown, legacy or
//! misplaced field in a node, style, paint or camera is refused rather than silently lost,
//! whatever its value. Every image is fully decoded (with the `image` crate, never through
//! upstream's process-wide registry) on import and export.
//! Rejected: oversized files, more pages, nodes or images than the
//! bounds, duplicate JSON keys anywhere, unknown fields, other formats or versions, unsafe or
//! colliding page ids (case, first six characters), an empty page list, an active page that
//! isn't listed, page records that don't match the manifest one for one (a missing page is
//! never imported as a blank one), duplicate node ids, node ids above 2^53 - 1 (or a `next_id`
//! above 2^53), checked before upstream parsing, a node graph that isn't one rooted forest
//! (every node reached once from the top level, parents matching, at most 256 levels; no
//! disconnected group or cycle), and images that
//! are missing, unused, duplicated, not standard base64, of another type than declared or not
//! matching their content hash. Images come only from the file itself (on import) or from the
//! canvas's own `assets/` folder (on export), never from the process-wide image registry.

use super::{MAX_ASSET_BYTES, MAX_ASSETS, MAX_MANIFEST_BYTES, MAX_PAGES, NewDocument, valid_id};
use crate::{Kind, page_prefix, plain, valid_page_id};
use paper_rs::project::format::{
    PAGE_FORMAT, PAGE_VERSION, PROJECT_FORMAT, PROJECT_VERSION, page_from_json, page_to_json,
};
use paper_rs::project::{ASSETS_DIR, MANIFEST, Manifest, PAGES_DIR, PageData, PageRef};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

pub(crate) const FORMAT: &str = "openpaperdesign-canvas";
pub(crate) const VERSION: u64 = 1;
const SUPPORTED: [u64; 1] = [1];
pub(crate) const CANVAS_ONLY: &str = "canvas-only";
/// The whole file, as text.
pub(crate) const MAX_PAYLOAD_BYTES: usize = 32 << 20;
pub(crate) const MAX_NODES: usize = 200_000;
pub(crate) const MAX_PAGE_NODES: usize = 100_000;
const MAX_NAME_CHARS: usize = 200;
const MAX_NOTE_BYTES: usize = 4000;
const MAX_DEPTH: usize = 64;
const IMAGE_EXTS: [&str; 4] = ["png", "jpg", "gif", "webp"];
pub(crate) const NOTE: &str = "This file holds the canvas as last saved: its pages and the images \
    they use. Edits still unsaved in a browser tab aren't in it, and it holds no design direction \
    (DESIGN.md), tokens or other project files.";

fn short(s: &str) -> String {
    let mut t: String = s.chars().take(60).collect();
    if t.len() < s.len() {
        t.push('…');
    }
    t
}

/// Every object's keys are distinct (serde_json silently keeps the last one), and nesting is
/// bounded. `text` must already have parsed as JSON.
fn check_keys(text: &str) -> Result<(), String> {
    let b = text.as_bytes();
    let mut stack: Vec<Option<HashSet<String>>> = Vec::new();
    let mut expect_key = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => {
                if stack.len() >= MAX_DEPTH {
                    return Err(format!("it nests deeper than {MAX_DEPTH} levels"));
                }
                let object = b[i] == b'{';
                stack.push(object.then(HashSet::new));
                expect_key = object;
            }
            b'}' | b']' => {
                stack.pop();
                expect_key = false;
            }
            b',' => expect_key = matches!(stack.last(), Some(Some(_))),
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if expect_key {
                    let key: String = serde_json::from_str(&text[start..=i.min(b.len() - 1)])
                        .map_err(|e| format!("a malformed key ({e})"))?;
                    if let Some(Some(keys)) = stack.last_mut()
                        && !keys.insert(key.clone())
                    {
                        return Err(format!(
                            "the key {:?} appears twice in one object",
                            short(&key)
                        ));
                    }
                    expect_key = false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

fn keys_exactly(
    o: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
    what: &str,
) -> Result<(), String> {
    if let Some(k) = o
        .keys()
        .find(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str()))
    {
        return Err(format!("{what} has an unsupported field {:?}", short(k)));
    }
    if let Some(r) = required.iter().find(|r| !o.contains_key(**r)) {
        return Err(format!("{what} has no {r:?}"));
    }
    Ok(())
}

fn check_name(s: &str, what: &str) -> Result<(), String> {
    if s.trim().is_empty() || s.chars().count() > MAX_NAME_CHARS || s.chars().any(char::is_control)
    {
        return Err(format!(
            "{what} must be 1 to {MAX_NAME_CHARS} characters without control characters"
        ));
    }
    Ok(())
}

/// The manifest rules every library canvas and every file must meet.
fn check_manifest(m: &Manifest) -> Result<(), String> {
    check_name(&m.name, "the canvas name")?;
    if m.pages.is_empty() {
        return Err("it has no pages".into());
    }
    if m.pages.len() > MAX_PAGES {
        return Err(format!("it has more than {MAX_PAGES} pages"));
    }
    let (mut ids, mut prefixes) = (HashSet::new(), HashSet::new());
    for p in &m.pages {
        if !valid_page_id(&p.id) {
            return Err(format!("page id {:?} isn't a safe id", short(&p.id)));
        }
        if !ids.insert(p.id.to_ascii_lowercase()) {
            return Err(format!(
                "page id {:?} differs from another only in case",
                p.id
            ));
        }
        if !prefixes.insert(page_prefix(&p.id)) {
            return Err(format!(
                "page id {:?} shares its first six characters (ignoring case) with another",
                p.id
            ));
        }
        check_name(&p.name, "a page name")?;
    }
    if let Some(a) = &m.active_page
        && !m.pages.iter().any(|p| &p.id == a)
    {
        return Err("its active page isn't one of its pages".into());
    }
    Ok(())
}

/// The largest node id a canvas may use: 2^53 - 1, the largest integer every JSON reader
/// represents exactly. A page's `next_id` may be at most one more. This keeps the upstream
/// parser's `max id + 1` and later id allocation far from overflow, so a bad id is an
/// ordinary refusal before any upstream code sees it.
pub(crate) const MAX_NODE_ID: u64 = (1 << 53) - 1;
/// How deeply nodes may nest (upstream walks the tree recursively).
pub(crate) const MAX_TREE_DEPTH: usize = 256;

/// A contained file's raw `format` and `version`: exactly the current upstream format and
/// version (manifest `paper-rs-project` 1, page `paper-rs-page` 3), as OpenPaperDesign writes
/// them. Checked before upstream parsing, whose migration helper would treat a missing or
/// malformed version as 1, narrow it to 32 bits and subtract one from it. Older page versions
/// and the legacy single-file format are not part of canvas files: no migration runs on import.
fn check_file_version(v: &Value, format: &str, version: u32, what: &str) -> Result<(), String> {
    if v.get("format").and_then(Value::as_str) != Some(format) {
        return Err(format!("{what} isn't a {format:?} file"));
    }
    if v.get("version").and_then(Value::as_u64) != Some(u64::from(version)) {
        return Err(format!(
            "{what} must have version {version}, the current {format} version (older, newer or \
             malformed versions aren't supported in canvas files)"
        ));
    }
    Ok(())
}

/// One known omission: a field name and whether a value is the default the writer leaves out.
type Omission = (&'static str, fn(&Value) -> bool);

/// What upstream's page writer deliberately leaves out when a field has its default value: the
/// page's `camera` when there is none, and a node's `parent`, `children`, `layout`, `instance`
/// and `component` at their defaults. Only these may be present in a file and missing from what
/// upstream writes back; any other key, whatever its value (`null` and `[]` included), must
/// survive.
const PAGE_OMISSIONS: &[Omission] = &[("camera", Value::is_null)];
const NODE_OMISSIONS: &[Omission] = &[
    ("parent", Value::is_null),
    ("children", |v| v.as_array().is_some_and(Vec::is_empty)),
    ("layout", Value::is_null),
    ("instance", Value::is_null),
    ("component", |v| v.as_bool() == Some(false)),
];

/// Where a page record holds something the parser didn't keep, if anywhere: every key of the
/// page, each node and everything inside them must be in `canonical` (what upstream writes back
/// from the parsed page) with the same value, numbers compared by value. Only the omissions
/// listed above may be missing. Upstream's structures ignore unknown fields, so an unknown,
/// legacy or misplaced field anywhere (a node, its style, a paint, the camera) would otherwise
/// vanish on import without notice. Input node order must already match (sorted by id).
fn dropped(input: &Value, canonical: &Value) -> Option<String> {
    let (Value::Object(page), Value::Object(out)) = (input, canonical) else {
        return strict(input, canonical);
    };
    keys_kept(page, out, PAGE_OMISSIONS, |k, v, w| {
        if k != "nodes" {
            return strict(v, w);
        }
        match (v, w) {
            (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
                a.iter().zip(b).enumerate().find_map(|(i, (n, m))| {
                    let at = match (n, m) {
                        (Value::Object(n), Value::Object(m)) => {
                            keys_kept(n, m, NODE_OMISSIONS, |_, x, y| strict(x, y))
                        }
                        _ => strict(n, m),
                    };
                    at.map(|at| format!("[{i}]{at}"))
                })
            }
            _ => Some(String::new()),
        }
    })
}

/// Every key of `a` is in `b` and `inner` finds nothing lost under it, unless it is one of the
/// record's known omissions at its default value.
fn keys_kept(
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    omissions: &[Omission],
    inner: impl Fn(&str, &Value, &Value) -> Option<String>,
) -> Option<String> {
    a.iter().find_map(|(k, v)| match b.get(k) {
        Some(w) => inner(k, v, w).map(|at| format!(".{}{at}", short(k))),
        None if omissions
            .iter()
            .any(|(name, is_default)| *name == k.as_str() && is_default(v)) =>
        {
            None
        }
        None => Some(format!(".{}", short(k))),
    })
}

/// `a` says exactly what `b` says: same keys, same values, numbers compared by value.
fn strict(a: &Value, b: &Value) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => keys_kept(x, y, &[], |_, v, w| strict(v, w)),
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (v, w))| strict(v, w).map(|at| format!("[{i}]{at}"))),
        (Value::Number(x), Value::Number(y))
            if x == y || matches!((x.as_f64(), y.as_f64()), (Some(p), Some(q)) if p == q) =>
        {
            None
        }
        _ if a == b => None,
        _ => Some(String::new()),
    }
}

/// Node ids of one page file are distinct and in range, its `next_id` is in range, and the node
/// counts stay in bounds. Runs on the raw record, before upstream parsing.
fn count_nodes(raw: &Value, page: &str, total: &mut usize) -> Result<(), String> {
    if let Some(next) = raw.get("next_id")
        && next.as_u64().is_none_or(|n| n > MAX_NODE_ID + 1)
    {
        return Err(format!(
            "page {page}'s next_id isn't a whole number from 0 to {}",
            MAX_NODE_ID + 1
        ));
    }
    let nodes = raw
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("page {page} has no node list"))?;
    if nodes.len() > MAX_PAGE_NODES {
        return Err(format!("page {page} has more than {MAX_PAGE_NODES} nodes"));
    }
    *total += nodes.len();
    if *total > MAX_NODES {
        return Err(format!(
            "the pages have more than {MAX_NODES} nodes together"
        ));
    }
    let mut seen = HashSet::new();
    for n in nodes {
        let id = n
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("page {page} has a node without a numeric id"))?;
        if id > MAX_NODE_ID {
            return Err(format!(
                "page {page} has node id {id}, above the largest allowed id {MAX_NODE_ID}"
            ));
        }
        if !seen.insert(id) {
            return Err(format!("page {page} has node id {id} twice"));
        }
    }
    Ok(())
}

/// The page's nodes form one rooted forest: walking from the page's top-level list (without
/// recursion), every node is reached exactly once, each through a node that lists it as a
/// child and that it names as its parent, no deeper than [`MAX_TREE_DEPTH`]. Upstream
/// `Document::validate` only counts child references, so a self-consistent group that no top
/// level reaches (a disconnected cycle) would pass it. Nothing is repaired: a page that isn't
/// such a forest is refused.
fn check_tree(page: &PageData) -> Result<(), String> {
    let doc = &page.doc;
    let mut seen = HashSet::with_capacity(doc.nodes.len());
    let mut stack: Vec<_> = doc.root.iter().rev().map(|id| (*id, None, 1)).collect();
    while let Some((id, parent, depth)) = stack.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(format!(
                "page {}: nodes nest deeper than {MAX_TREE_DEPTH} levels",
                page.id
            ));
        }
        let n = doc
            .nodes
            .get(&id)
            .ok_or_else(|| format!("page {}: node {} is listed but missing", page.id, id.0))?;
        if n.parent != parent {
            return Err(format!(
                "page {}: node {} doesn't name the node that lists it as its parent",
                page.id, id.0
            ));
        }
        if !seen.insert(id) {
            return Err(format!(
                "page {}: node {} is listed more than once",
                page.id, id.0
            ));
        }
        stack.extend(n.children.iter().rev().map(|c| (*c, Some(id), depth + 1)));
    }
    if seen.len() != doc.nodes.len() {
        return Err(format!(
            "page {}: {} nodes aren't reachable from the page's top level (a disconnected group \
             or cycle)",
            page.id,
            doc.nodes.len() - seen.len()
        ));
    }
    Ok(())
}

/// One page record for manifest entry `want` (number `i`): its fields, node ids and graph. The
/// manifest owns the page's id and name, as on disk.
fn page_record(i: usize, v: &Value, want: &PageRef, total: &mut usize) -> Result<PageData, String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("page record {i} isn't an object"))?;
    keys_exactly(
        o,
        &[
            "format", "version", "id", "name", "next_id", "root", "nodes",
        ],
        &["camera"],
        &format!("page record {i}"),
    )?;
    if o["id"].as_str() != Some(want.id.as_str()) {
        return Err(format!(
            "page record {i} isn't for page {i} of the manifest ({}); records must match the \
             pages one for one, in order",
            want.id
        ));
    }
    check_file_version(v, PAGE_FORMAT, PAGE_VERSION, &format!("page record {i}"))?;
    count_nodes(v, &want.id, total)?;
    let mut page = page_from_json(&v.to_string()).map_err(|e| format!("page {}: {e}", want.id))?;
    check_tree(&page)?;
    // Nothing the file says may be lost on the way in (node order aside: it is by id).
    let mut input = v.clone();
    if let Some(nodes) = input.get_mut("nodes").and_then(Value::as_array_mut) {
        nodes.sort_by_key(|n| n.get("id").and_then(Value::as_u64));
    }
    let canonical: Value =
        serde_json::from_str(&page_to_json(&page)).map_err(|e| format!("page {}: {e}", want.id))?;
    if let Some(at) = dropped(&input, &canonical) {
        return Err(format!(
            "page {} holds `{}` that this version of OpenPaperDesign doesn't keep (an unknown, \
             legacy or non-canonical field or value); nothing is imported partially",
            want.id,
            if at.is_empty() {
                "the page"
            } else {
                at.trim_start_matches('.')
            }
        ));
    }
    page.id = want.id.clone();
    page.name = want.name.clone();
    page.doc.name = want.name.clone();
    Ok(page)
}

/// The image type its bytes say, if it is one the editor stores.
fn ext_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// The bytes really are a `ext` image: decoded completely with the same `image` decoder (and
/// its default memory limits) the editor uses, into a value that is dropped at once. Nothing is
/// registered in upstream's process-wide image registry.
fn decodes(bytes: &[u8], ext: &str) -> bool {
    let format = match ext {
        "png" => image::ImageFormat::Png,
        "jpg" => image::ImageFormat::Jpeg,
        "gif" => image::ImageFormat::Gif,
        "webp" => image::ImageFormat::WebP,
        _ => return false,
    };
    image::load_from_memory_with_format(bytes, format)
        .is_ok_and(|img| img.width() > 0 && img.height() > 0)
}

fn used_images(pages: &[PageData]) -> BTreeSet<String> {
    pages
        .iter()
        .flat_map(|p| paper_rs::assets::used_by(&p.doc))
        .collect()
}

fn check_assets(v: &Value, pages: &[PageData]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let list = v.as_array().ok_or("assets must be a list")?;
    if list.len() > MAX_ASSETS {
        return Err(format!("it has more than {MAX_ASSETS} images"));
    }
    let max_text = (MAX_ASSET_BYTES as usize).div_ceil(3) * 4;
    let (mut out, mut ids) = (Vec::new(), BTreeSet::new());
    for a in list {
        let a = a.as_object().ok_or("an image record isn't an object")?;
        keys_exactly(a, &["id", "ext", "data"], &[], "an image record")?;
        let id = a["id"]
            .as_str()
            .filter(|s| valid_id(s))
            .ok_or("an image record's id isn't a 16-digit content hash")?;
        let ext = a["ext"]
            .as_str()
            .filter(|e| IMAGE_EXTS.contains(e))
            .ok_or_else(|| format!("image {id} has an unsupported type"))?;
        let data = a["data"]
            .as_str()
            .ok_or_else(|| format!("image {id} has no data"))?;
        if data.len() > max_text {
            return Err(format!("image {id} is larger than {MAX_ASSET_BYTES} bytes"));
        }
        let bytes = paper_rs::assets::base64_decode(data)
            .filter(|b| paper_rs::assets::base64(b) == data)
            .ok_or_else(|| format!("image {id} isn't standard padded base64"))?;
        if paper_rs::assets::hash(&bytes) != id {
            return Err(format!("image {id}'s bytes don't match its id"));
        }
        if ext_of(&bytes) != Some(ext) {
            return Err(format!("image {id} isn't a {ext} image"));
        }
        if !ids.insert(id.to_string()) {
            return Err(format!("image {id} appears twice"));
        }
        out.push((format!("{id}.{ext}"), bytes));
    }
    let used = used_images(pages);
    if let Some(m) = used.difference(&ids).next() {
        return Err(format!(
            "the pages use image {}, which isn't in the file (images are never taken from \
             elsewhere)",
            short(m)
        ));
    }
    if let Some(x) = ids.difference(&used).next() {
        return Err(format!("image {x} isn't used by any page"));
    }
    // Last, as it costs the most: every image really decodes (nothing is registered).
    for (name, bytes) in &out {
        let (id, ext) = name.split_once('.').unwrap_or((name, ""));
        if !decodes(bytes, ext) {
            return Err(format!("image {id} doesn't decode as a {ext} image"));
        }
    }
    Ok(out)
}

/// Check a whole file. The result keeps the file's manifest (give it a fresh identity before
/// publishing it) and is written as canonical page and manifest files.
pub(crate) fn validate(text: &str) -> Result<NewDocument, String> {
    validate_inner(text).map_err(|e| {
        format!("This isn't an importable OpenPaperDesign canvas file: {e}. Nothing was imported.")
    })
}

fn validate_inner(text: &str) -> Result<NewDocument, String> {
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(format!("it is larger than {MAX_PAYLOAD_BYTES} bytes"));
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("it isn't valid JSON ({e})"))?;
    check_keys(text)?;
    let o = v.as_object().ok_or("it isn't a JSON object")?;
    if o.contains_key("companions") {
        return Err(
            "design-direction and token companions aren't supported by this version (canvas-only \
             files only)"
                .into(),
        );
    }
    keys_exactly(
        o,
        &[
            "format", "version", "content", "manifest", "pages", "assets",
        ],
        &["note"],
        "the file",
    )?;
    if o["format"].as_str() != Some(FORMAT) {
        return Err(format!("its format isn't {FORMAT:?}"));
    }
    let version = o["version"].as_u64().ok_or("its version isn't a number")?;
    if !SUPPORTED.contains(&version) {
        return Err(format!(
            "file version {version} isn't supported (supported: 1)"
        ));
    }
    if o["content"].as_str() != Some(CANVAS_ONLY) {
        return Err(format!("its content isn't {CANVAS_ONLY:?}"));
    }
    if let Some(n) = o.get("note")
        && n.as_str().is_none_or(|s| s.len() > MAX_NOTE_BYTES)
    {
        return Err("its note isn't a short text".into());
    }
    let mo = o["manifest"]
        .as_object()
        .ok_or("its manifest isn't an object")?;
    keys_exactly(
        mo,
        &[
            "format", "version", "id", "name", "created", "modified", "pages",
        ],
        &["active_page"],
        "the manifest",
    )?;
    for p in mo["pages"]
        .as_array()
        .ok_or("the manifest's pages aren't a list")?
    {
        let p = p
            .as_object()
            .ok_or("a manifest page entry isn't an object")?;
        keys_exactly(p, &["id", "name"], &[], "a manifest page entry")?;
    }
    check_file_version(
        &o["manifest"],
        PROJECT_FORMAT,
        PROJECT_VERSION,
        "the manifest",
    )?;
    let manifest =
        Manifest::from_json(&o["manifest"].to_string()).map_err(|e| format!("manifest: {e}"))?;
    check_manifest(&manifest)?;
    let records = o["pages"].as_array().ok_or("its pages aren't a list")?;
    if records.len() != manifest.pages.len() {
        return Err(format!(
            "it has {} page records for {} pages; every page needs exactly one (a missing page is \
             not imported as a blank one)",
            records.len(),
            manifest.pages.len()
        ));
    }
    let mut total = 0;
    let mut pages = Vec::with_capacity(records.len());
    for (i, (r, want)) in records.iter().zip(&manifest.pages).enumerate() {
        pages.push(page_record(i, r, want, &mut total)?);
    }
    let assets = check_assets(&o["assets"], &pages)?;
    Ok(NewDocument {
        manifest,
        pages,
        assets,
    })
}

/// An exported canvas: the file's text and the canvas name.
pub(crate) struct Exported {
    pub text: String,
    pub name: String,
}

/// The saved canvas in `dir` as a file. The caller holds the canvas's lease and its writer
/// fence, so no save is half-written while this reads. Bounded before anything is returned;
/// the result passes [`validate`].
pub(crate) fn export(dir: &Path) -> Result<Exported, String> {
    let mlen = std::fs::symlink_metadata(dir.join(MANIFEST))
        .map_err(|e| format!("{MANIFEST}: {e}"))?
        .len();
    if mlen > MAX_MANIFEST_BYTES {
        return Err(format!(
            "its {MANIFEST} is larger than {MAX_MANIFEST_BYTES} bytes"
        ));
    }
    let manifest = crate::read_manifest_checked(dir)?;
    check_manifest(&manifest)?;
    let too_large = || format!("the canvas is larger than {MAX_PAYLOAD_BYTES} bytes as a file");
    let mut total = mlen as usize;
    let (mut pages, mut empty, mut nodes) = (Vec::new(), Vec::new(), 0);
    for r in &manifest.pages {
        let path = dir.join(PAGES_DIR).join(format!("{}.json", r.id));
        let mut page = if plain(&dir.join(PAGES_DIR), Kind::Dir)? && plain(&path, Kind::File)? {
            let bytes = std::fs::read(&path).map_err(|e| format!("page {}: {e}", r.id))?;
            total += bytes.len();
            if total > MAX_PAYLOAD_BYTES {
                return Err(too_large());
            }
            let text = String::from_utf8(bytes).map_err(|e| format!("page {}: {e}", r.id))?;
            let raw: Value =
                serde_json::from_str(&text).map_err(|e| format!("page {}: {e}", r.id))?;
            check_keys(&text).map_err(|e| format!("page {}: {e}", r.id))?;
            check_file_version(&raw, PAGE_FORMAT, PAGE_VERSION, &format!("page {}", r.id))?;
            count_nodes(&raw, &r.id, &mut nodes)?;
            page_from_json(&text).map_err(|e| format!("page {}: {e}", r.id))?
        } else {
            // As the editor shows it: a listed page without a file is empty.
            empty.push(r.id.clone());
            PageData {
                id: String::new(),
                name: String::new(),
                doc: paper_rs::model::Document::default(),
                camera: None,
            }
        };
        page.id = r.id.clone();
        page.name = r.name.clone();
        page.doc.name = r.name.clone();
        pages.push(page);
    }
    let mut assets = Vec::new();
    for id in used_images(&pages) {
        if !valid_id(&id) {
            return Err(format!(
                "a page uses image {:?}, which isn't a content id",
                short(&id)
            ));
        }
        let assets_dir = plain(&dir.join(ASSETS_DIR), Kind::Dir)?;
        let found = IMAGE_EXTS
            .iter()
            .map(|e| (*e, dir.join(ASSETS_DIR).join(format!("{id}.{e}"))))
            .find(|(_, p)| assets_dir && plain(p, Kind::File).unwrap_or(false));
        let Some((ext, path)) = found else {
            return Err(format!(
                "image {id}, which the canvas uses, is missing from its assets folder"
            ));
        };
        let bytes = std::fs::read(&path).map_err(|e| format!("image {id}: {e}"))?;
        if bytes.len() as u64 > MAX_ASSET_BYTES {
            return Err(format!("image {id} is larger than {MAX_ASSET_BYTES} bytes"));
        }
        if paper_rs::assets::hash(&bytes) != id || ext_of(&bytes) != Some(ext) {
            return Err(format!(
                "the canvas's file for image {id} doesn't match its id or type"
            ));
        }
        if !decodes(&bytes, ext) {
            return Err(format!(
                "the canvas's file for image {id} doesn't decode as a {ext} image"
            ));
        }
        total += bytes.len().div_ceil(3) * 4;
        if total > MAX_PAYLOAD_BYTES {
            return Err(too_large());
        }
        assets.push(json!({"id": id, "ext": ext, "data": paper_rs::assets::base64(&bytes)}));
    }
    let note = if empty.is_empty() {
        NOTE.to_string()
    } else {
        format!(
            "{NOTE} Pages without a saved file were exported as the empty pages the editor \
             shows: {}.",
            empty.join(", ")
        )
    };
    let parse = |s: String| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string());
    let manifest_value = parse(manifest.to_json())?;
    let mut records = Vec::with_capacity(pages.len());
    for p in &pages {
        records.push(parse(page_to_json(p))?);
    }
    let v = json!({
        "format": FORMAT,
        "version": VERSION,
        "content": CANVAS_ONLY,
        "note": note,
        "manifest": manifest_value,
        "pages": records,
        "assets": assets,
    });
    let text = serde_json::to_string(&v).map_err(|e| e.to_string())?;
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(too_large());
    }
    // What is exported can be imported again.
    validate_inner(&text)?;
    Ok(Exported {
        text,
        name: manifest.name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use paper_rs::model::{Node, NodeKind, Paint};

    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A real 2×1 PNG, encoded by the same `image` crate: one red and one blue pixel.
    fn png() -> Vec<u8> {
        let img = image::RgbaImage::from_fn(2, 1, |x, _| {
            image::Rgba(if x == 0 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            })
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// PNG-signed bytes that aren't an image: they pass the hash, signature and base64 checks.
    const NOT_AN_IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nnot an image after its signature";

    /// A saved two-page canvas: a moved camera, a renamed second page in a deliberate order,
    /// a rectangle on each page and an image fill on the first, with its image file.
    fn canvas() -> (Tmp, std::path::PathBuf, String) {
        use std::hash::{BuildHasher, RandomState};
        let root = std::env::temp_dir().join(format!(
            "opd-payload-{:016x}",
            RandomState::new().hash_one(std::time::SystemTime::now())
        ));
        let (mut p, mut first) = paper_rs::project::Store::open(&root)
            .unwrap()
            .create("Portable")
            .unwrap();
        let image = paper_rs::assets::hash(&png());
        std::fs::create_dir_all(p.dir.join(ASSETS_DIR)).unwrap();
        std::fs::write(p.dir.join(ASSETS_DIR).join(format!("{image}.png")), png()).unwrap();
        let mut camera = serde_json::to_value(paper_rs::geom::Camera::default()).unwrap();
        for v in camera.as_object_mut().unwrap().values_mut() {
            if let Some(x) = v.as_f64() {
                *v = json!(x + 3.5);
            }
        }
        first.camera = Some(serde_json::from_value(camera).unwrap());
        let id = first.doc.alloc_id();
        let mut n = Node::new(
            id,
            NodeKind::Rect,
            paper_rs::geom::Rect::new(1.0, 2.0, 30.0, 40.0),
        );
        n.style.fills = vec![Paint::Image {
            asset: image.clone(),
            mode: Default::default(),
            opacity: 1.0,
        }];
        first.doc.nodes.insert(id, n);
        first.doc.root.push(id);
        p.save_page(&first).unwrap();
        let mut second = PageData {
            id: "b1c2d3e4f5a6b7c8".into(),
            name: "Second".into(),
            doc: Default::default(),
            camera: None,
        };
        let id = second.doc.alloc_id();
        second.doc.nodes.insert(
            id,
            Node::new(
                id,
                NodeKind::Rect,
                paper_rs::geom::Rect::new(0.0, 0.0, 5.0, 5.0),
            ),
        );
        second.doc.root.push(id);
        p.save_page(&second).unwrap();
        // The second page first, and active.
        p.manifest.pages.insert(
            0,
            PageRef {
                id: second.id.clone(),
                name: "Second".into(),
            },
        );
        p.manifest.active_page = Some(second.id.clone());
        p.write_manifest().unwrap();
        (Tmp(root), p.dir.clone(), image)
    }

    fn exported() -> (Tmp, std::path::PathBuf, Value) {
        let (t, dir, _) = canvas();
        let e = export(&dir).unwrap();
        assert_eq!(e.name, "Portable");
        let v = serde_json::from_str(&e.text).unwrap();
        (t, dir, v)
    }

    fn refused(v: &Value, want: &str) {
        let e = validate(&v.to_string()).err().expect("refused");
        assert!(
            e.contains(want) && e.contains("Nothing was imported"),
            "{want}: {e}"
        );
    }

    #[test]
    fn a_saved_canvas_round_trips_with_order_camera_and_images() {
        let (_t, dir, _) = canvas();
        let project = paper_rs::project::Project::open(&dir).unwrap();
        let e = export(&dir).unwrap();
        let v: Value = serde_json::from_str(&e.text).unwrap();
        assert_eq!(v["content"], CANVAS_ONLY);
        assert!(v["note"].as_str().unwrap().contains("no design direction"));
        let doc = validate(&e.text).unwrap();
        assert_eq!(
            doc.manifest.pages, project.manifest.pages,
            "names and order"
        );
        assert_eq!(doc.manifest.active_page, project.manifest.active_page);
        assert_eq!(
            doc.pages,
            project.load_pages().unwrap(),
            "content and camera"
        );
        assert!(doc.pages.iter().any(|p| p.camera.is_some()));
        // The real image round-trips byte for byte, and decodes to the same two pixels.
        let asset = paper_rs::assets::hash(&png());
        assert_eq!(doc.assets, vec![(format!("{asset}.png"), png())]);
        let pixels = image::load_from_memory(&doc.assets[0].1)
            .unwrap()
            .to_rgba8();
        assert_eq!(
            (pixels.width(), pixels.height(), pixels.get_pixel(1, 0).0),
            (2, 1, [0, 0, 255, 255])
        );
        // The fresh identity changes only the manifest's own id and times.
        let fresh = doc.fresh();
        assert_ne!(fresh.manifest.id, project.manifest.id);
        assert_eq!(fresh.manifest.pages, project.manifest.pages);
    }

    #[test]
    fn images_that_dont_decode_are_refused_on_import_and_export() {
        // Bytes with a matching content hash, a PNG signature and standard base64: only the
        // decoder can tell they aren't an image.
        let bad = paper_rs::assets::hash(NOT_AN_IMAGE);
        let (_t, _dir, v) = exported();
        let mut x = v.clone();
        x["pages"][1]["nodes"][0]["style"]["fills"][0]["asset"] = json!(bad);
        x["assets"] =
            json!([{"id": bad, "ext": "png", "data": paper_rs::assets::base64(NOT_AN_IMAGE)}]);
        refused(&x, "doesn't decode as a png image");
        // Export doesn't bless such a file in the canvas's own assets either.
        let (_t2, dir, _) = canvas();
        let p = paper_rs::project::Project::open(&dir).unwrap();
        let id = p.manifest.pages[1].id.clone();
        let mut page = p.load_page(&id).unwrap();
        for n in page.doc.nodes.values_mut() {
            for f in &mut n.style.fills {
                if let Paint::Image { asset, .. } = f {
                    *asset = bad.clone();
                }
            }
        }
        p.save_page(&page).unwrap();
        std::fs::write(
            dir.join(ASSETS_DIR).join(format!("{bad}.png")),
            NOT_AN_IMAGE,
        )
        .unwrap();
        let e = export(&dir).err().unwrap();
        assert!(e.contains("doesn't decode as a png image"), "{e}");
        // Neither check registered anything in upstream's process-wide image registry.
        assert!(paper_rs::assets::get(&bad).is_none());
        assert!(paper_rs::assets::get(&paper_rs::assets::hash(&png())).is_none());
    }

    #[test]
    fn a_missing_image_or_an_unlisted_page_file_is_handled_explicitly() {
        let (_t, dir, image) = canvas();
        // A listed page without a file is exported as the empty page the editor shows, and
        // the note says so.
        let mut p = paper_rs::project::Project::open(&dir).unwrap();
        p.manifest.pages.push(PageRef {
            id: "c0ffee0123456789".into(),
            name: "Unsaved".into(),
        });
        p.write_manifest().unwrap();
        let e = export(&dir).unwrap();
        assert!(e.text.contains("c0ffee0123456789"), "explicit page record");
        assert!(e.text.contains("exported as the empty pages"));
        // A used image whose file is gone is never filled in from elsewhere.
        std::fs::remove_file(dir.join(ASSETS_DIR).join(format!("{image}.png"))).unwrap();
        let err = export(&dir).err().unwrap();
        assert!(err.contains("missing from its assets folder"), "{err}");
    }

    #[test]
    fn malformed_duplicate_and_inconsistent_files_are_refused() {
        let (_t, _dir, v) = exported();
        let text = v.to_string();
        // Duplicate keys anywhere, even where serde_json would keep the last one.
        let dup = text.replacen("\"format\":", "\"format\":\"x\",\"format\":", 1);
        let e = validate(&dup).err().unwrap();
        assert!(e.contains("appears twice"), "{e}");
        let mut x = v.clone();
        x["version"] = json!(2);
        refused(&x, "version 2 isn't supported");
        let mut x = v.clone();
        x["format"] = json!("paper-rs-project");
        refused(&x, "format");
        let mut x = v.clone();
        x["companions"] = json!([{"kind": "direction", "name": "DESIGN.md", "text": "# D"}]);
        refused(&x, "companions aren't supported");
        let mut x = v.clone();
        x["extra"] = json!(1);
        refused(&x, "unsupported field");
        // Pages: one record per manifest page, in order; never a blank stand-in.
        let mut x = v.clone();
        x["pages"].as_array_mut().unwrap().pop();
        refused(&x, "not imported as a blank one");
        let mut x = v.clone();
        x["pages"].as_array_mut().unwrap().reverse();
        refused(&x, "one for one, in order");
        let mut x = v.clone();
        x["manifest"]["pages"] = json!([]);
        x["pages"] = json!([]);
        refused(&x, "no pages");
        let mut x = v.clone();
        x["manifest"]["active_page"] = json!("ffffffffffffffff");
        refused(&x, "active page");
        // Ids: safe, unique ignoring case, distinct first six characters.
        let mut x = v.clone();
        x["manifest"]["pages"][0]["id"] = json!("../../escape");
        x["pages"][0]["id"] = json!("../../escape");
        refused(&x, "isn't a safe id");
        let first = v["manifest"]["pages"][1]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut x = v.clone();
        x["manifest"]["pages"][0]["id"] = json!(first.to_ascii_uppercase());
        x["pages"][0]["id"] = json!(first.to_ascii_uppercase());
        x["manifest"]["active_page"] = Value::Null;
        refused(&x, "only in case");
        let mut x = v.clone();
        let twin = format!("{}0000000000", &first[..6]);
        x["manifest"]["pages"][0]["id"] = json!(twin);
        x["pages"][0]["id"] = json!(twin);
        x["manifest"]["active_page"] = Value::Null;
        refused(&x, "first six characters");
        // Nodes: distinct ids and a valid graph.
        let mut x = v.clone();
        let node = x["pages"][0]["nodes"][0].clone();
        x["pages"][0]["nodes"].as_array_mut().unwrap().push(node);
        refused(&x, "twice");
        let mut x = v.clone();
        x["pages"][0]["root"] = json!([999]);
        refused(&x, "page");
        // Images: complete, used, matching and standard.
        let mut x = v.clone();
        x["assets"] = json!([]);
        refused(&x, "never taken from elsewhere");
        let mut x = v.clone();
        let extra = paper_rs::assets::base64(b"GIF89a extra");
        x["assets"].as_array_mut().unwrap().push(
            json!({"id": paper_rs::assets::hash(b"GIF89a extra"), "ext": "gif", "data": extra}),
        );
        refused(&x, "isn't used by any page");
        let mut x = v.clone();
        x["assets"][0]["ext"] = json!("gif");
        refused(&x, "isn't a gif image");
        let mut x = v.clone();
        let data = x["assets"][0]["data"].as_str().unwrap().replace('=', "");
        x["assets"][0]["data"] = json!(format!("{data}-"));
        refused(&x, "base64");
        let mut x = v.clone();
        x["assets"][0]["id"] = json!("0000000000000000");
        refused(&x, "don't match its id");
    }

    #[test]
    fn contained_file_versions_must_be_the_current_ones_before_any_parsing() {
        let (_t, _dir, v) = exported();
        // Page versions upstream's migration would mis-handle (zero underflows, 2^32 + 2
        // narrows to 2, a string or float would count as 1), older and newer ones alike.
        for bad in [
            json!(0),
            json!(2),
            json!(4),
            json!(4_294_967_298_u64),
            json!("3"),
            json!(3.0),
            json!(-3),
            Value::Null,
        ] {
            let mut x = v.clone();
            x["pages"][0]["version"] = bad.clone();
            refused(&x, "must have version 3");
            let mut x = v.clone();
            x["manifest"]["version"] = if bad == json!(2) { json!(1.0) } else { bad };
            refused(&x, "must have version 1");
        }
        // The legacy single-file format isn't a contained page, even with its own version.
        let mut x = v.clone();
        x["pages"][0]["format"] = json!("paper-rs");
        x["pages"][0]["version"] = json!(1);
        refused(&x, "isn't a \"paper-rs-page\" file");
        let mut x = v.clone();
        x["manifest"]["format"] = json!("paper-rs-page");
        refused(&x, "isn't a \"paper-rs-project\" file");
    }

    #[test]
    fn nested_content_the_parser_would_drop_is_refused_not_lost() {
        let (_t, _dir, v) = exported();
        // The exported file itself keeps everything (positive control).
        validate(&v.to_string()).unwrap();
        // The second page record's rectangle, the first page record's image fill and camera.
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["style"]["fill"] = json!("#ff0000");
        refused(&x, "nodes[0].style.fill");
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["shadow"] = json!({"blur": 4});
        refused(&x, "shadow");
        let mut x = v.clone();
        x["pages"][1]["nodes"][0]["style"]["fills"][0]["blur"] = json!(2);
        refused(&x, "fills[0].blur");
        let mut x = v.clone();
        x["pages"][1]["camera"]["tilt"] = json!(1);
        refused(&x, "camera.tilt");
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["visible"] = json!("yes");
        refused(&x, "page");
        // Unknown fields are refused whatever their value: `null` and `[]` included.
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["glow"] = Value::Null;
        refused(&x, "glow");
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["style"]["effects"] = json!([]);
        refused(&x, "nodes[0].style.effects");
        let mut x = v.clone();
        x["pages"][1]["camera"]["roll"] = Value::Null;
        refused(&x, "camera.roll");
        let mut x = v.clone();
        x["pages"][1]["nodes"][0]["style"]["fills"][0]["filters"] = json!([]);
        refused(&x, "fills[0].filters");
        // A known omission is accepted only at its default value.
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["component"] = json!(true);
        assert!(
            validate(&x.to_string()).is_ok(),
            "a real value is kept, not omitted"
        );
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["children"] = json!(null);
        refused(&x, "page");
        // What the writer itself omits may be explicit, and numbers compare by value.
        let mut x = v.clone();
        x["pages"][0]["camera"] = Value::Null;
        x["pages"][0]["nodes"][0]["children"] = json!([]);
        x["pages"][0]["nodes"][0]["parent"] = Value::Null;
        x["pages"][0]["nodes"][0]["layout"] = Value::Null;
        x["pages"][0]["nodes"][0]["instance"] = Value::Null;
        x["pages"][0]["nodes"][0]["component"] = json!(false);
        let w = x["pages"][0]["nodes"][0]["rect"]["w"].as_f64().unwrap();
        if w.fract() == 0.0 {
            x["pages"][0]["nodes"][0]["rect"]["w"] = json!(w as u64);
        }
        let doc = validate(&x.to_string()).unwrap();
        assert_eq!(doc.pages, validate(&v.to_string()).unwrap().pages);
    }

    #[test]
    fn out_of_range_ids_and_unrooted_graphs_are_refused_before_use() {
        let (_t, _dir, v) = exported();
        // Ids the upstream parser would add one to: refused before it runs, not a panic.
        for id in [u64::MAX, MAX_NODE_ID + 1] {
            let mut x = v.clone();
            x["pages"][0]["nodes"][0]["id"] = json!(id);
            x["pages"][0]["root"] = json!([id]);
            refused(&x, "above the largest allowed id");
        }
        let mut x = v.clone();
        x["pages"][0]["next_id"] = json!(u64::MAX);
        refused(&x, "next_id");
        // The largest allowed id and next_id are fine.
        let mut x = v.clone();
        x["pages"][0]["nodes"][0]["id"] = json!(MAX_NODE_ID);
        x["pages"][0]["root"] = json!([MAX_NODE_ID]);
        x["pages"][0]["next_id"] = json!(MAX_NODE_ID + 1);
        validate(&x.to_string()).unwrap();
        // A pair that names each other as parent and child, reached from no top level.
        let mut x = v.clone();
        let node = x["pages"][0]["nodes"][0].clone();
        let (mut a, mut b) = (node.clone(), node);
        a["id"] = json!(900);
        a["parent"] = json!(901);
        a["children"] = json!([901]);
        b["id"] = json!(901);
        b["parent"] = json!(900);
        b["children"] = json!([900]);
        x["pages"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .extend([a, b]);
        x["pages"][0]["next_id"] = json!(902);
        let page = page_from_json(&x["pages"][0].to_string()).unwrap();
        assert!(
            page.doc.validate().is_ok(),
            "upstream's own check counts child references only"
        );
        refused(&x, "aren't reachable");
        // Nesting: exactly the bound is fine, one more level is refused.
        let chain = |len: u64| {
            let mut x = v.clone();
            let node = x["pages"][0]["nodes"][0].clone();
            let nodes: Vec<Value> = (0..len)
                .map(|i| {
                    let mut n = node.clone();
                    n["id"] = json!(1000 + i);
                    if i > 0 {
                        n["parent"] = json!(1000 + i - 1);
                    }
                    if i + 1 < len {
                        n["children"] = json!([1000 + i + 1]);
                    }
                    n
                })
                .collect();
            x["pages"][0]["nodes"] = json!(nodes);
            x["pages"][0]["root"] = json!([1000]);
            x["pages"][0]["next_id"] = json!(1000 + len);
            x
        };
        validate(&chain(MAX_TREE_DEPTH as u64).to_string()).unwrap();
        refused(&chain(MAX_TREE_DEPTH as u64 + 1), "nest deeper");
    }

    #[test]
    fn oversized_and_deep_files_are_refused_before_use() {
        let big = " ".repeat(MAX_PAYLOAD_BYTES + 1);
        assert!(validate(&big).err().unwrap().contains("larger than"));
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(validate(&deep).err().unwrap().contains("nests deeper"));
        let many: Vec<Value> = (0..=MAX_PAGE_NODES as u64)
            .map(|i| json!({"id": i}))
            .collect();
        let mut total = 0;
        let e = count_nodes(&json!({ "nodes": many }), "p", &mut total).unwrap_err();
        assert!(e.contains("more than"), "{e}");
        let mut total = MAX_NODES;
        let e = count_nodes(&json!({"nodes": [{"id": 1}]}), "p", &mut total).unwrap_err();
        assert!(e.contains("together"), "{e}");
    }
}
