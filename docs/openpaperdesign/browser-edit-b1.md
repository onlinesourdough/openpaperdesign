# B1: editing a paper-rs project in a browser — 2026-10-12

Updated 2026-10-12 after review rounds 1–3 and the operator's Codex browser runs (see "Fix
round 1", "Fix round 2" and "Fix round 3" below).

2026-10-13 documentation update: linked the later installed qualification under remaining limits.

Scope: [issue #1](https://github.com/onlinesourdough/openpaperdesign/issues/1), slice B1. A
loopback server opens one existing paper-rs project folder. A browser page runs the upstream
editor and canvas renderer as wasm and saves through the server with revision checks.
Agent/MCP writes are B2.

## Run it

```sh
./scripts/build-web.sh --offline          # cargo build (wasm32, release) + wasm-bindgen 0.2.129
cargo run --locked -p openpaperdesign-serve -- --project "<project folder>" [--port 29981]
# open the printed http://127.0.0.1:<port>/#token=… URL in a browser on the same machine
```

The page shows the project's active page. It supports select, drag-move, the Rectangle and
Ellipse tools, Delete, and undo/redo from the toolbar. Each change is saved about 300 ms after
the gesture ends.

Keys go through upstream's own routing (`Editor::key_down` and its shortcut table). The page
consumes a key (prevents the browser default) only when the editor acts on it. Shortcuts that
only open app chrome (save/open/export dialogs, palette, rename, help, page list) aren't in
this slice, so they aren't sent and stay the browser's: ⌘/Ctrl+R still reloads. Covered by
host tests (`state.rs`): tool keys (R, Escape), Delete, Ctrl/⌘+Z, Ctrl+Shift+Z, Ctrl/⌘+A
(consumed), Space, and the chrome shortcuts Ctrl+R/S/O/K (not consumed). Other upstream
shortcuts are routed the same way but untested here.

**First save migrates the page file.** Pages are saved in upstream's current format (page
v3). An older page file (for example a v1 `.paper-rs` document in `pages/`) is rewritten as
v3 on the browser's first save, exactly as native paper-rs does on its own first save.
Review confirmed that node content, asset references, other pages and the manifest are kept,
apart from `manifest.modified`.

## Pieces

- `crates/openpaperdesign-web` (wasm32 cdylib). Compiles upstream `editor`, `project`, `io`,
  `render::{scene, gpu, …}` and the rest of the non-UI modules from `src/` through `#[path]`,
  plus `web.rs` (wasm-bindgen API) and `keys.rs` (DOM key and modifier mapping).
  `static/{index.html, app.js}` holds the page. It has no egui panels.
  - **Rendering:** upstream `scene::canvas` → `gpu::CanvasRenderer` on a wgpu surface for the
    `<canvas>`. WebGPU is tried first, and WebGL2 is compiled in as a fallback (glow, offline).
    `?backend=webgl` forces WebGL2. The page shows which backend and adapter actually ran.
  - **Clipboard:** `openpaperdesign-clipboard-stub` is imported as `arboard`, so upstream
    `io/clipboard.rs` compiles unchanged. There is no system clipboard, so copy and paste work
    within the page only.
  - **Clocks:** times come from `performance.now()` / `event.timeStamp`. No upstream code path
    used here calls `Instant`/`SystemTime` (`project::now_ms` is only reached when creating a
    project, which the web side never does).
  - **Randomness:** `fastrand` is seeded from `crypto.getRandomValues` at startup. On
    wasm32-unknown-unknown it otherwise starts from a fixed seed, so page and project ids made
    by `project::new_id` could repeat. Node ids are per-document counters and never random.
  - **Input platform:** upstream picks the command modifier at compile time (Cmd on macOS,
    Ctrl otherwise), and a wasm build counts as "otherwise". When `navigator` reports a Mac,
    the page reports Meta (⌘) to the editor as `ctrl` and Control as `logo`. Shortcuts then
    use ⌘ on a Mac and Ctrl elsewhere. Upstream is unchanged.
- `crates/openpaperdesign-serve` (native bin, axum/tokio as in the root lockfile). It depends
  on the `paper-rs` library itself, so saves use upstream `Project::save_page` (format and
  atomic write).

## Server security and revision model

- **Binding and project:** binds `127.0.0.1` only. `--project` must already be a paper-rs
  project (`project.json`, checked with upstream `Project::open`); nothing is created.
- **Token:** every launch makes a random token (32 bytes from the OS RNG via `getrandom`). The
  printed URL carries it in the fragment, which never reaches the server, logs or `Referer`.
  The page moves it into `sessionStorage` for this tab, removes it from the address bar and
  sends `Authorization: Bearer …` on `/api/*`. A restart makes a new token, and the old one
  then gets 401.
- **Request checks:** the server refuses a `Host` other than `127.0.0.1:<port>` /
  `localhost:<port>` (DNS rebinding), an `Origin` other than those, and
  `Sec-Fetch-Site` values other than `same-origin`/`none`, with one exception: a `same-site` or
  `cross-site` top-level document navigation to the page itself (`GET /` with
  `Sec-Fetch-Mode: navigate` and `Sec-Fetch-Dest: document`, as when a host link or redirect
  opens the editor URL) gets the static bootstrap page, which holds no project data. Frames,
  fetches, subresources, other paths and methods, the API and MCP stay refused from another
  site, and the API still needs the token. Every response carries `no-store`,
  `nosniff`, `no-referrer` and a same-origin CSP with `frame-ancestors 'none'`. That includes
  refusals, unknown paths and body-limit errors, because the headers are added by the
  outermost layer. Only the six bundle files are served (`index.html`, `app.js`,
  `page.js`, `session.js`, the wasm-bindgen JS and the wasm). They are checked (plain, single-link files
  in a folder that is not inside the project) and read into memory once at startup, and
  every request is served from memory, so replacing the bundle folder or a file in it later
  can't change what is sent. A `--web-dir` inside or equal to the project folder is refused
  at startup. An existing `--web-dir` is resolved by the operating system (symlinks
  included). A missing one serves nothing ("build it first"), and is refused if it contains
  `..`: there is no lexical guessing.
- **No links out of the project (fail closed):** `project.json`, `pages/`, each page file
  and each bundle file must be a real file or directory, not a symbolic link. Files must
  also have a single hard link. The project root must stay the directory opened at startup:
  on every access it must still be a real directory (not a link), canonicalize to the same
  path (which also catches a swapped ancestor folder), and have the same (device, inode).
  This is checked before upstream `Project::open` (whose stale-temp cleanup deletes `.*.tmp`
  files in `pages/`) and again on every read and write. Otherwise the server refuses to
  start or answers 403. Saves are atomic renames over the page file, so they replace a file
  inside `pages/` and never write through a link.
  **Residual race (TOCTOU):** a directory or link swapped in between a check and the file
  operation that follows can still redirect that read **or write** outside the project.
  The checks close persistent replacements, not that window. Someone able to swap folders
  there can already edit the project folder directly.
- **Platforms:** the root identity and hard-link checks use Unix metadata, so the server
  starts only on Unix (Linux, macOS) and refuses to start elsewhere. Linux is tested here,
  and macOS-arm64 by the operator (below). Other platforms are unsupported for these
  guarantees.
- **Pages:** pages are addressed only by ids listed in the manifest, which must also be plain
  `[A-Za-z0-9_-]`, max 64 characters. The server builds every path. The body's page id must
  match the URL. The body must parse and validate as an upstream page. The manifest's page
  name wins, as in upstream `load_page`.
- **Revisions:** a page's revision (ETag) is a 64-bit SipHash (std `DefaultHasher`, fixed
  keys) of its file bytes as they are on disk now, or `"absent"` for a listed page with no
  file. It detects changes; it isn't a cryptographic digest. `PUT` needs `If-Match`; without
  it the server answers 428, and with a stale value it answers 409 plus the current ETag,
  writing nothing. A process-wide lock serializes check and write. A successful save also
  bumps `manifest.modified`, like the native session.
- **Browser on 409:** the page stops saving and keeps the local edits in the tab without
  writing them. It shows a banner asking to reload from disk, and never overwrites.
- **B2 hook:** an MCP/agent writer added to this same server takes the same lock and the same
  `If-Match` revision, making the server the single authority for writers that go through
  it.

### Boundary: other writers are not protected

The server guarantees revision-checked, serialized writes only for writers that go through
it. Because the revision is computed from the file on disk, a change by any other writer (a
running native paper-rs, an editor, a sync tool) makes the browser's next save fail with 409
instead of overwriting it. The reverse is not protected: a running native app or other writer
can overwrite the browser's saves. A write landing between the server's revision check and
its atomic rename is also not detected. Reopening the project in native paper-rs after the
server stops is supported, and was checked below. Editing it in both at the same time is not
supported.

## Results after fix round 1 (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy` on the new crates (host, all targets) | 0 warnings |
| `cargo clippy` on core and web (wasm32) | only upstream `src/io/atomic.rs:30` (`drop_non_drop`) |
| `cargo test -p openpaperdesign-serve` | 13 passed (8 earlier + 4 link tests + headers test) |
| `cargo test -p openpaperdesign-web` (host) | 103 passed (97 earlier + 6 `state` tests) |
| `cargo check` core and web for wasm32 | pass |
| Native `cargo build --all-targets`; `cargo test -- --test-threads=1` | pass; 162 passed, 0 failed, 0 ignored (= baseline) |
| Root `cargo clippy --all-targets` | 1 upstream warning (= baseline) |
| `cargo tree -p paper-rs -e features` | byte-identical before and after the fixes |
| Bundle (`scripts/build-web.sh --offline`) | `openpaperdesign_web_bg.wasm` 11,634,501 B (4,223,115 B gzip -9), `openpaperdesign_web.js` 123,949 B, `app.js` 6,508 B, `index.html` 2,139 B |
| Browser run | still not possible in this sandbox (below); `browser-check.mjs` passes `node --check` only |

## Results at 13b2430 (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0)

| Check | Result |
|---|---|
| Native `cargo build --all-targets` | pass |
| `cargo test -- --test-threads=1` | 162 passed, 0 failed, 0 ignored (= baseline) |
| `cargo test` (parallel) | `tests/mcp.rs` SIGSEGV, others pass (= baseline) |
| `PAPER_RS_REQUIRE_GPU=1 cargo test --test headless` | 9 passed |
| `cargo clippy --all-targets` (root) | 1 upstream warning, `tests/perf.rs:105` (= baseline) |
| `cargo clippy` on the new crates, host and wasm32 | clean, apart from upstream `src/io/atomic.rs:30` (`drop_non_drop`, wasm32 only) |
| `cargo fmt --all --check` | clean |
| `cargo test -p openpaperdesign-serve` | 8 passed: stale `If-Match` → 409, file unchanged; outside change → 409; ids/traversal/body-id rejected, no stray files; Host/Origin/fetch-site/token; non-projects refused; restart serves the same bytes and revision; native `Session::open` sees the saved move; absent page file |
| `cargo test -p openpaperdesign-web` (host) | 97 passed (upstream unit tests compiled via `#[path]` + 3 key-mapping tests) |
| `cargo check -p openpaperdesign-web --target wasm32-unknown-unknown` | pass, 0 warnings |
| Bundle (`--release`, wasm-bindgen 0.2.129) | `openpaperdesign_web_bg.wasm` 11,648,700 B (4,228,063 B gzip -9), `openpaperdesign_web.js` 124,585 B, `app.js` 6,508 B, `index.html` 2,139 B |
| Real loopback HTTP, release bundle, demo project | bundle files 200 with the right content types; API without token 401, foreign Host 403, cross Origin 403, traversal 400; PUT with current revision 200 (legacy v1 page saved as page v3); PUT with stale revision 409, file unchanged; restart: new token, old token 401, same ETag and bytes |
| Native `paper-rs --export-svg` / `--screenshot` on that project | both open it. The screenshot (llvmpipe) shows all 23 layers with the moved text |

The demo project was built from upstream's `docs/screenshots/m6_launch_plan.paper-rs` (a
legacy single-file page) in a temporary folder.

### Browser runtime: not run here

Headless Chromium 152 could not start in the implementation sandbox:
`FATAL:chrome/browser/process_singleton_posix.cc:297 Check failed: . socket() failed:
Operation not permitted (1)`. The sandbox wasn't widened and Chromium's own sandbox was not
disabled. So in-browser loading, the drag/create/delete/undo/redo sequence, the backend
actually used and screenshots are **not yet proven**. Run them on the host, where the server
and browser share loopback:

```sh
./scripts/build-web.sh --offline
cargo run --locked -p openpaperdesign-serve -- --project "<project folder>"     # prints URL 1
node scripts/browser-check.mjs '<URL 1>' target/evidence/b1-browser edit
# stop the server (Ctrl+C), start it again (new token, prints URL 2), then:
node scripts/browser-check.mjs '<URL 2>' target/evidence/b1-browser verify /usr/bin/chromium \
    target/evidence/b1-browser/result-edit.json
paper-rs --screenshot target/evidence/b1-browser/native.png "<project folder>"   # native reopen
```

The script drives the real page through CDP and asserts every step. It drags a node (it must
move by the drag distance, within snapping slack), draws a rectangle with the toolbar tool,
deletes it with the Delete key, undoes with Ctrl+Z, then redoes and undoes from the toolbar.
Node ids and counts are checked after each step. It waits for each save, takes screenshots,
reloads, and requires the same revision and node positions after the reload. `verify` with
the edit run's result file requires the same after a server restart. Any console error, page
exception or missing GPU adapter fails the run, and so does any failed check: the exit code
is then 1. `result-<mode>.json` records the backend and adapter. The fifth argument and the
assertions are new in fix round 1; the first four positional arguments are unchanged.

## Fix round 1 (2026-10-12, after independent review of 13b2430)

| Finding | Resolution | Tests |
|---|---|---|
| Major: symlinks could lead reads, writes and upstream's temp cleanup outside the project, and a symlinked bundle file was served | Links refused, fail closed, before `Project::open` and on every access (above) | `symlinked_pages_dir_is_refused_before_cleanup_and_after_open` (outside `.keep.tmp` sentinel survives; also installed after start), `symlinked_or_hard_linked_page_file_is_refused`, `symlinked_manifest_is_refused` (before and after start), `symlinked_bundle_file_is_not_served`. All 5 new serve tests fail on the 13b2430 server code and pass now |
| Major: selection, hover, marquee and modifier changes didn't redraw (`Editor::revision` unchanged) | Editor logic moved to host-testable `state.rs`; every input or state-changing call marks a redraw, and idle frames are skipped only when nothing happened and no laser is animating | `selection_click_redraws_although_revision_is_unchanged` (asserts upstream's revision stays the same while the overlay changes), `hover_and_modifier_changes_redraw` |
| Minor: key consumption was inferred from side effects (Ctrl+A not consumed) | Decided from upstream's key routing and shortcut table before handling; chrome-only shortcuts left to the browser | `select_all_is_consumed`, `editing_keys_are_consumed_and_act`, `chrome_shortcuts_and_unbound_keys_stay_with_the_browser` |
| Minor: security headers missing on refusals | Outermost `map_response` layer plus an explicit fallback | `security_headers_are_on_every_response` (200, 401, 403, 404, 409, 413) |
| Minor: browser script recorded but didn't assert | Asserts every step, across reload and restart, and fails on console/GPU errors with exit 1 | (operator host run) |

Results after the fixes are under "Results" below.

## Fix round 2 (2026-10-12, after the operator's Codex browser run)

### Operator-observed on 13b2430 (not re-run by the implementation worker)

The operator ran the 13b2430 server on the Z13 host, opened it in the Codex in-app browser on
a Mac (Apple M4), and reached it through an explicit SSH loopback tunnel to `127.0.0.1:29981`.
This was the bridged Linux server, not a Mac-local binary or an installed plugin.

- **WebGL2 (`?backend=webgl`) works.** The adapter was `Gl / ANGLE Metal, Apple M4`. With real
  UI input the operator dragged node 23, drew rectangle 24, deleted it with Delete, restored
  it with native ⌘Z, used toolbar Redo/Undo, and reloaded. The disk readback went from 23 to
  24 nodes. Only node 23 moved (x 420 → 475, y 470 → 526.78), and rectangle 24 sits at x 105,
  y 658, w 175, h 80.
- **Default WebGPU drew a black canvas but said "Ready · BrowserWebGpu / (Other)".** The
  console showed `Error while parsing WGSL :55:17: textureSample must only be called from
  uniform control flow`, then an invalid shader module, invalid render pipelines and invalid
  command buffers. Upstream's fragment shader sampled the image texture inside
  `else if (k >= 3u)`, after a possible `discard`, and `k` comes from a fragment input. The
  browser's WGSL front end enforces the uniformity rule; native wgpu/naga accepted it.
- **Layout jump.** At about 697 px panel width the long "Ready · renderer" status wrapped the
  header to 103 px. "Saving…"/"Saved" shrank it back to 39 px, so the canvas jumped while
  editing.

### Fixes (working tree on top of e770d46)

| Problem | Change | Check |
|---|---|---|
| WGSL uniformity | **The one upstream edit:** `src/render/gpu.rs` `fs` samples `img_tex` once at the top, before any branch or `discard`, and the image branch uses that value. Group 2 is bound for every draw (`gpu.rs:716`, `blank_img` when there is no image), so the unconditional sample adds no binding requirement. Same texture, coordinates and sampler; the image filtering uses the same implicit derivatives, now taken in uniform control flow | `tests/shader.rs`: `implicit_derivative_samples_come_before_any_branch` is a focused structural source check, not a WGSL uniformity analysis. On the old shader it fails ("textureSample at byte 497 of fs is inside or after a branch (first at 124)"); on the new one it passes. naga 30 accepts the old shader even with every validation flag and `diagnostic(error, derivative_uniformity)`, so naga can't be the regression test. `canvas_shader_is_valid_wgsl` checks the new shader with naga |
| "Ready" over invalid GPU objects | `start()` wraps shader/pipeline creation, surface setup and the first frame in a wgpu validation error scope and fails with the message. A device-level handler records later errors, which stop drawing and switch the page to a red error state. On a WebGPU failure the page swaps in a fresh `<canvas>` (one that handed out a WebGPU context can't give WebGL) and retries with WebGL2. The footer then says `WebGPU failed: <reason> — using WebGL (...)`, and the failure is logged with `console.error` | browser-check fails on any console error, on a fallback, and when no backend is reported. It records the requested and actual backend. Run it once as is and once with `?backend=webgl` |
| Layout jump | Header and footer have fixed heights, and long text is cut with an ellipsis (full text in the tooltip). Renderer and adapter text moved to the footer; the status line only shows Ready/Saving…/Saved/conflict/errors. The canvas fills a separate stage, and the conflict banner overlays it instead of pushing it down | browser-check asserts the canvas rect is identical at load and after every save |

### Results of run 8 (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0)

| Check | Result |
|---|---|
| `cargo fmt --all --check`; `git diff --check` | clean |
| `cargo clippy` new crates (host, all targets) | 0 warnings |
| `cargo clippy` core and web (wasm32) | only upstream `src/io/atomic.rs:30` (`drop_non_drop`) |
| `cargo test -p openpaperdesign-serve -p openpaperdesign-web` | 13, 103 and 2 (`tests/shader.rs`) passed |
| `cargo check` core and web for wasm32 | pass |
| Native `cargo build --all-targets` | pass |
| `PAPER_RS_REQUIRE_GPU=1 cargo test --test headless` | 9 passed (llvmpipe) |
| `cargo test -- --test-threads=1` | 162 passed, 0 failed, 0 ignored (= baseline) |
| Regenerated native screenshots, before vs after the shader change (same sandbox, llvmpipe) | 9 of 11 byte-identical. `milestone1_home.png` and `milestone1_trash.png` also differ between two runs of the **old** shader: the home screen shows times |
| Root `cargo clippy --all-targets` | 1 upstream warning (= baseline) |
| `cargo tree -p paper-rs -e features` | byte-identical to the pre-round-1 capture |
| Bundle | `openpaperdesign_web_bg.wasm` 11,647,042 B (4,230,013 B gzip -9), `openpaperdesign_web.js` 124,728 B, `app.js` 7,716 B, `index.html` 3,002 B |

**Environment note:** the sandbox's shared tmpfs (`/tmp/claude-1001`, 81% full) returned
`Disk quota exceeded` during this round. Upstream's file-writing tests (`io::atomic`,
`io::file`, `library`, `project`, `session`, headless `m6_save_load_and_export`) failed on
those I/O errors, which had nothing to do with the GPU code (two home-screen screenshots were
also compared with the old shader, see above). The native results were taken with
`TMPDIR=<worktree>/target/tmp` (gitignored, main disk) as a workaround for the shared tmpfs
quota. No test or source was changed for this.

### Operator-observed on the uncommitted run-8 tree (macOS-arm64 server, Codex in-app browser)

The operator froze the uncommitted run-8 source on top of e770d46 (source archive SHA-256
`d3857ecfd6c30915114dea62fc1c958f9390e420cbe521aaec31c3abfa50b754`). It was built as a
macOS-arm64 server and used in the actual Codex in-app browser with native UI input:

- **Default WebGPU renders**, with no warnings, errors or fallback. The selection outline
  updates without any document change. ⌘A selects canvas objects and no browser text.
- **Edits:** drag of node 23, rectangle 26 created, Delete (25 nodes), ⌘Z (26), toolbar Redo
  (25), Undo (26). A reload keeps the changes. The header/canvas top stays at 40 px.
- **Explicit WebGL (ANGLE) also renders.** A marquee selects 24/25, and ellipse 27 saves.
- **Restart:** the old token gets 401, the new one 200, with the same bytes. A new Codex tab
  renders all 27 nodes on WebGPU.
- **Content:** of the 25 nodes that existed before, only the moved node 23 differs. The final
  page SHA-256 is `345946161710edae7c45a7853bdd250eaa55afdb16502f97b2c8047278b8eec7`.
- **Evidence:** `visual-readback.json`, `restart-readback.json` and two images, kept
  privately outside the repository.

This proves the run-8 changes only. It doesn't prove acceptance of e770d46, an installed
plugin or MCP. *(Historical note, as of run 8: the round-2 changes A–C below had not been run in
a browser yet. They were later operator-observed on 260ac38, see "Fix round 3". The current
browser status of the final revision is under "Not proven / open".)*

### Round-2 review of e770d46 (REJECT; round-1 findings confirmed fixed)

| Finding | Resolution | Tests |
|---|---|---|
| Major: replacing the project root (renamed away, symlink to an outside project) bypassed containment: GET 200, PUT 200, outside rect x changed to 777 | Root identity recorded at startup and re-verified on every access, and right before a save: a real directory, same canonical path, same (device, inode). Fails closed with 403. The residual race is documented above | `replaced_project_root_is_refused`: symlinked root (twice), a different real directory, and a swapped ancestor, all refused for GET page, GET project and PUT, with outside files byte-identical. On e770d46's server code it fails: `symlinked root: GET page`, 200 instead of 403 |
| Minor: laser expiry skipped the clearing frame (points removed before `animating()` was checked, so the last mark stayed on screen) | `tick()` marks a redraw when expiry removed points; it stays pending until a frame is actually drawn | `laser_expiry_draws_the_clearing_frame`, `laser_expiry_after_a_long_gap_still_draws`. Both fail without the fix ("frame at 1012 ms while the trail fades", "the stale trail must be cleared") |
| Minor: hard-link check returned 1 on non-Unix while the docs promised refusal | The server refuses to start on non-Unix. Docs scope the guarantees to Linux/macOS | none: non-Unix not compiled here (no such target installed) |

### Final round-2 results (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0, `TMPDIR=<worktree>/target/tmp`)

| Check | Result |
|---|---|
| `cargo fmt --all --check`; `git diff --check` | clean |
| `cargo clippy` new crates (host, all targets) | 0 warnings |
| `cargo clippy` core and web (wasm32) | only upstream `src/io/atomic.rs:30` (`drop_non_drop`), once per crate |
| `cargo test -p openpaperdesign-serve` / `-p openpaperdesign-web` (lib) / `tests/shader.rs` | 14 / 105 / 2 passed |
| `cargo check` core and web for wasm32 | pass |
| Native `cargo build --all-targets` | pass |
| `PAPER_RS_REQUIRE_GPU=1 cargo test --test headless` | 9 passed, 0 failed, 0 ignored on llvmpipe; no skips, no quota errors |
| `cargo test -- --test-threads=1` | 162 passed, 0 failed, 0 ignored (= baseline); no quota errors |
| Root `cargo clippy --all-targets` | 1 upstream warning (= baseline) |
| `cargo tree -p paper-rs -e features` | byte-identical to the pre-round-1 capture |
| Bundle (`scripts/build-web.sh --offline` → `target/openpaperdesign-web/`) | `openpaperdesign_web_bg.wasm` 11,647,034 B (4,229,799 B gzip -9), `openpaperdesign_web.js` 124,728 B, `app.js` 7,716 B, `index.html` 3,002 B |

## Fix round 3 (2026-10-12, after independent review of 260ac38)

### Operator-observed on exact 260ac38 (not re-run by the implementation worker)

The server was built for the Mac from 260ac38, and all 14 serve tests passed there. In the
actual Codex in-app browser:

- **WebGPU:** BrowserWebGpu paints. A laser trail expires without new input. The toolbar
  stays 40 px.
- **Edits:** R plus a drag created rectangle 28, which persisted, and all 27 earlier nodes
  were unchanged.
- **WebGL:** an explicit WebGL fresh reload shows all 28 nodes.
- **Logs:** no errors or warnings.

This is operator-observed on 260ac38 only; the round-3 changes below were not part of it.

### Round-3 review of 260ac38 (REJECT; round-2 items confirmed fixed)

| Finding | Resolution | Tests |
|---|---|---|
| Major: static bundle serving checked only the final file. Replacing the project root while `--web-dir` was inside it, or replacing a separate bundle folder with a symlink to an outside folder, served outside bytes | Bundle checked and read into memory once at startup and served only from memory. A `--web-dir` inside or equal to the project folder is refused at startup (checked on the real path, or for a missing folder on its resolved path, including `..`). The first case can't arise any more: a bundle inside the project is refused | `bundle_is_served_from_memory_after_its_folder_is_replaced` (folder replaced by a symlink to an outside folder with sentinel files: the original bytes are served, a file absent at startup stays 404, and an in-place rewrite changes nothing), `web_dir_inside_the_project_is_refused` (inside, equal, missing-inside, via `..`), `symlinked_bundle_file_is_refused_at_startup`. All three fail on 260ac38's server code; the first shows exactly `(200, "OUTSIDE SENTINEL")` |
| Minor: Ready without renderability. Surface failures returned silently, recorded errors weren't checked before returning, no device-loss handling | New host-testable `health.rs`. Surface outcomes: Success draws; Suboptimal draws, then reconfigures; Timeout/Occluded skip and retry; Outdated reconfigures and retries; Lost/Validation are terminal with a visible message. Uncaptured errors and `set_device_lost_callback` record a terminal error and stop drawing. `start()` fails if a problem is recorded during startup. The page says Ready only when `ready()` is true (validated, a frame presented, no recorded problem). If WebGPU fails before Ready, the page falls back to WebGL2 once and says so; after Ready, a failure is shown as a red error | `health::tests::{surface_outcomes, ready_needs_a_presented_frame, any_recorded_error_ends_readiness_and_drawing, terminal_surface_outcomes_fail_readiness}`. `health.rs` is new, so there is nothing on 260ac38 to run them against; there, `app.js` set Ready right after `start()` returned. **Only the state machine is tested; the wgpu callback wiring, the surface-acquire mapping and the page's fallback are untested until a browser re-check, and a real device loss hasn't been exercised at all** |
| Minor: stale docs about the Mac ⌘ mapping and Codex behavior | Corrected (see "Not proven / open") | — |

**Reviewer observation (llvmpipe PNG variance).** The reviewer saw 3 of 14 headless PNGs
differ in 55–61 px by at most 1 channel level between the base and candidate shader. In this
sandbox (llvmpipe, GL) the 14 PNGs that `tests/headless.rs` writes were compared, by bytes
and by decoded pixels, across two runs with the base shader and two with the new one.
base/base, new/new and base/new were all 14/14 byte-identical. So the reviewer's difference
wasn't reproduced here, and its cause (llvmpipe nondeterminism or something else) is not
established by this check.

### Round-3 results (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0, `TMPDIR=<worktree>/target/tmp` as the shared-tmpfs quota workaround)

| Check | Result |
|---|---|
| `cargo fmt --all --check`; `git diff --check` | clean |
| `cargo clippy` new crates (host, all targets) | 0 warnings |
| `cargo clippy` core and web (wasm32) | only upstream `src/io/atomic.rs:30`, once per crate |
| `cargo test -p openpaperdesign-serve` / web lib / `tests/shader.rs` | 16 / 109 / 2 passed |
| `cargo check` core and web for wasm32 | pass |
| Native `cargo build --all-targets` | pass |
| `PAPER_RS_REQUIRE_GPU=1 cargo test --test headless` | 9 passed, 0 failed, 0 ignored; no skips, no quota errors |
| `cargo test -- --test-threads=1` | 162 passed, 0 failed, 0 ignored (= baseline); no quota errors |
| Root `cargo clippy --all-targets` | 1 upstream warning (= baseline) |
| `cargo tree -p paper-rs -e features` | byte-identical to the pre-round-1 capture |
| Bundle (`target/openpaperdesign-web/`) | `openpaperdesign_web_bg.wasm` 11,662,088 B (4,234,028 B gzip -9), `openpaperdesign_web.js` 126,514 B, `app.js` 8,648 B, `index.html` 3,002 B |

## Fix round 4 (2026-10-12, after independent review of 2b5c9ae)

| Finding | Resolution | Tests |
|---|---|---|
| P2: a fallback freed the editor and reopened the initially loaded JSON while edits and saves were allowed before Ready, so it could restore old content under a newer revision. Probes showed a null-editor exception, and a save's answer applied to the replacement editor | The editor lifecycle moved to `static/session.js`. Editing input, toolbar actions and saves only reach the editor once it is Ready and no fallback is running. A fallback waits for a save in flight, then reopens the page **as the server has it now** (content and revision). A save's answer only applies to the editor that took the snapshot | `tests/session.test.mjs` (Node `node:test`, mocked editors/canvas/DOM, no browser): `input, toolbar and saves are ignored until Ready`; `a startup failure falls back to WebGL on a fresh canvas with the current server page`; `a fallback waits for a save in flight and never applies its answer to the new editor`; `an answer arriving after the editor was swapped is dropped`; `forced WebGL failing is an error, not a fallback`. `session.js` is new, so there is nothing on 2b5c9ae to run them against: there the logic was inline in `app.js`, ungated |
| P2: the ResizeObserver stayed on the replaced canvas and toolbar handlers stayed active, so `ed.resize()`/`ed.set_tool()` were called on null | The old observer is disconnected, and a new one observes the new canvas, which also gets its own input listeners. Every handler goes through the session's guards (`edit` for input/toolbar, `withEditor` for resize/frames/blur) | `a GPU error before Ready falls back once; the old observer and editor are retired` (the old observer is disconnected; a late callback from it never reaches the freed editor and can only resize the current replacement editor, which is safe; the new observer resizes the new editor; nothing reaches an editor while switching) |
| P2: `resolve()` applied `..` before resolving symlinks, so with `alias` → `project/sub`, a missing `alias/../not-built` resolved to an unrelated sibling bundle, which was served | No lexical resolution any more (see "Server security"). This supersedes the missing-folder rule from round 3 | `missing_web_dir_with_dotdot_after_a_symlink_is_not_guessed` fails on 2b5c9ae's server code with `served: UNREQUESTED SIBLING BUNDLE`, and passes now. `web_dir_inside_the_project_is_refused` was updated (a missing folder without `..` now serves nothing instead of being refused) |
| P3: "Codex built-in browser" listed as unproven without qualification | Scoped to the final revision ("Not proven / open") | — |

**Build command and environment for the bundle**, recorded so reproducibility can be
assessed (reproducibility is not claimed):

```sh
export RUSTUP_HOME=$PWD/.toolchain/rustup CARGO_HOME=$PWD/.toolchain/cargo PATH=$PWD/.toolchain/cargo/bin:$PATH
export TMPDIR=$PWD/target/tmp
./scripts/build-web.sh --offline
# = cargo build --locked --release -p openpaperdesign-web --target wasm32-unknown-unknown --offline
#   wasm-bindgen --target web --no-typescript --out-dir target/openpaperdesign-web \
#       target/wasm32-unknown-unknown/release/openpaperdesign_web.wasm
```

Rust 1.95.0 (59807616e), wasm-bindgen CLI 0.2.129 (operator-staged in `.toolchain/cargo/bin`),
Linux x86_64 host, worktree path
`/home/factory/.t3/worktrees/openpaperdesign/foundation-wasm-core-feasibility`, default
`target/` dir, and no `RUSTFLAGS` or `SOURCE_DATE_EPOCH` set. Absolute paths (worktree,
`CARGO_HOME`) can be embedded in the wasm, for example in panic location strings, so a
build from a different path may well hash differently.

### Round-4 results (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0, `TMPDIR=<worktree>/target/tmp`)

| Check | Result |
|---|---|
| `cargo fmt --all --check`; `git diff --check` | clean |
| `cargo clippy` new crates (host, all targets) | 0 warnings |
| `cargo clippy` core and web (wasm32) | only upstream `src/io/atomic.rs:30`, once per crate |
| `cargo test -p openpaperdesign-serve` / web lib / `tests/shader.rs` | 17 / 109 / 2 passed |
| `node --test crates/openpaperdesign-web/tests/` (Node 26.8.1, mocked DOM) | 6 passed (this directory form fails on Node 22, see round 5) |
| `cargo check` core and web for wasm32 | pass |
| Native `cargo build --all-targets` | pass |
| `PAPER_RS_REQUIRE_GPU=1 cargo test --test headless` | 9 passed, 0 failed, 0 ignored; no skips, no quota errors |
| `cargo test -- --test-threads=1` | 162 passed, 0 failed, 0 ignored (= baseline); no quota errors |
| Root `cargo clippy --all-targets` | 1 upstream warning (= baseline) |
| `cargo tree -p paper-rs -e features` | byte-identical to the pre-round-1 capture |
| Bundle | `openpaperdesign_web_bg.wasm` 11,662,088 B and `openpaperdesign_web.js` 126,514 B: SHA-256 identical to the round-3 build (no Rust change since, same environment). `app.js` 8,246 B, `session.js` 5,334 B, `index.html` 3,002 B |

## Fix round 5 (2026-10-12, after independent review of e1ee940)

| Finding | Resolution | Tests |
|---|---|---|
| P2: a fallback that failed (page fetch or WebGL init rejected) left the page at "switching to WebGL…". The error was recorded but later frames returned nothing, so it was never shown | `Session.frame()` reports `'error'` once after any recorded failure, including an asynchronous fallback failure, then `'stopped'`. The new `static/page.js` (`frameStep`) shows it in red in the status line and footer and stops the frame loop. `app.js` uses it, and startup failures also mark the footer red. `page.js` is shipped and allowlisted | `tests/session.test.mjs` through `frameStep` with mocked UI: `a fallback that fails (the page fetch fails) …`, `a fallback that fails (WebGL init fails) … ends in a visible red error and stops the frame loop`, `the page glue shows Ready once and keeps the loop running`. All three fail with e1ee940's `session.js` |
| P2: `node --test crates/openpaperdesign-web/tests/` (a directory) failed with MODULE_NOT_FOUND on Node 22 (reviewer: 22.23.3, the ubuntu-24.04 runner's documented Node; operator: 22.22.3 on the Mac) | CI runs the explicit file `node --test crates/openpaperdesign-web/tests/session.test.mjs` and prints `node --version`. Supported: Node ≥ 22, the runner's preinstalled Node (no setup-node action, no package.json) | The exact command passes here on Node 26.8.1. The Node 22 behaviour is the reviewer's and operator's observation and was not reproduced here |
| P3: the run-8 browser caveat read as current; "a late observer callback touches nothing" overstated | Marked historical; narrowed (see the round-4 table) | — |

### Round-5 results (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0, `TMPDIR=<worktree>/target/tmp`)

| Check | Result |
|---|---|
| `cargo fmt --all --check`; `git diff --check` | clean |
| `cargo clippy` new crates (host) / core and web (wasm32) | 0 warnings / only upstream `src/io/atomic.rs:30` |
| `cargo test -p openpaperdesign-serve` / web lib / `tests/shader.rs` | 17 / 109 / 2 passed |
| `node --test crates/openpaperdesign-web/tests/session.test.mjs` (the CI command; Node 26.8.1) | 9 passed |
| `cargo check` wasm32; native `cargo build --all-targets` | pass; pass |
| `PAPER_RS_REQUIRE_GPU=1 cargo test --test headless` | 9 passed, 0 failed, 0 ignored; no skips |
| `cargo test -- --test-threads=1` | 162 passed, 0 failed, 0 ignored (= baseline); no quota errors |
| Root clippy; `cargo tree -p paper-rs -e features` | 1 upstream warning; unchanged |
| Bundle | wasm and wasm-bindgen JS unchanged (same SHA-256 as rounds 3–4); `app.js` 8,361 B, `session.js` 5,619 B, `page.js` 760 B, `index.html` 3,002 B |

## Fix round 6 (2026-10-12, after independent review of 0f759a4)

### Operator-observed on exact 0f759a4 (that revision only; not re-run by the implementation worker)

On the Mac, the serve tests (17) and the Node tests (9, Node 22.22.3) passed. In the Codex
in-app browser on WebGPU an edit was saved; after a reload and a reopen on WebGL the page had
31 nodes, and the 30 earlier ones were unchanged.

| Finding | Resolution | Tests |
|---|---|---|
| P2: a terminal renderer error (e.g. device loss) while a save was in flight was overwritten by that save's result ("Saved", "Not saved: conflict", "retrying"), although no more saves would happen | Saving moved into `page.js` (`saveStep`), which `app.js` runs every 300 ms. The save's bookkeeping (ack and revision, conflict state, failure) still happens in `Session.save`, and the conflict banner is still shown. Once terminal, the status line and footer keep the red error and no further save starts | `tests/session.test.mjs` (mocked page; through `saveStep`, `frameStep` and `Session`): `a save in flight when the renderer fails (success \| 409 conflict \| HTTP failure \| network rejection) keeps the red terminal status`. Against 0f759a4: its `page.js` has no `saveStep`, so the file doesn't load. With 0f759a4's `app.js` save logic transplanted verbatim into `saveStep`, all four fail and the other 9 pass |
| P3: the bundle list said five files and left out `page.js` | Six, listed (above) | — |

**Checks: current versus inherited.** This round changed JavaScript and docs only.
`git diff --stat 0f759a4` lists `crates/openpaperdesign-web/static/{app.js,page.js}`,
`crates/openpaperdesign-web/tests/session.test.mjs` and this file. No Rust, manifest or
lockfile changed.

- **Current (2026-10-12, this sandbox):**
  - `node --check` on the changed JavaScript;
  - the CI command `node --test crates/openpaperdesign-web/tests/session.test.mjs`: 13
    passed (Node 26.8.1);
  - the regression demonstration above;
  - `git diff --check`;
  - the bundle rebuild. Only `app.js` (8,016 B, SHA-256
    `14aa2c6865e9087763ed661238f1973d9aace7f96d38e70d0c7dc0cba91f9d7a`) and `page.js`
    (1,707 B, `ed9762b360347812960b5141f59d11378ccf0e5921c41f796d0f504e366742ed`) changed.
    The wasm, the wasm-bindgen JS, `session.js` and `index.html` are byte-identical to 0f759a4.
- **Inherited from the round-5 results on 0f759a4 (not re-run, by the owner's instruction):**
  native build, serial suite (162 passed), headless GPU (9), clippy, fmt,
  `cargo tree -p paper-rs`, the serve/web/shader tests and the wasm32 check.

## Not proven / open

- Historical B1 browser runs are operator-observed, all in the Codex in-app browser on a Mac (Apple
  M4): the WebGPU failure and WebGL2 on 13b2430, WebGPU and WebGL on the run-8 tree, and
  WebGPU and WebGL on 260ac38 (see "Fix round 3"). The later
  [installed `0078487` qualification](plugin-b2b.md#installed-qualification-2026-10-13)
  records WebGPU Ready and browser/agent save, stale refusal, reload and process-reopen
  preservation. It does not establish live device-loss recovery or a physical crash test.
- Other browsers, MSAA and blend-mode output in a browser compared with native.
- Image fills: the browser registers no project assets, so image fills don't draw. Saving
  keeps their references.
- Text editing UI, inspector, layers, multiple pages (only the active page loads), export,
  thumbnails, async GPU readback, IME and touch/pen pressure.
- The Mac browser ⌘ mapping is unit-tested and was operator-observed in Codex on the run-8
  tree (⌘A, ⌘Z). Other Mac browsers are untested.
- ~4.2 MB gzip download: the fonts alone are 2.5 MB uncompressed. Size work is open.
- Renderer fallback transitions, startup input gating and `--web-dir` variants have no
  new browser qualification from the installed run; their earlier evidence remains above.
- MCP App panel and other-host embedding. Local plugin packaging and B2 model writes are
  covered only to the extent of the linked installed qualification.
