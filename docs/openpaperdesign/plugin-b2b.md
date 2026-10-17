# B2b: the standalone `openpaperdesign` Codex plugin — 2026-10-12

Updated 2026-10-13: installed qualification on exact `0078487`; prior results retain their original provenance.

Scope: [issue #1](https://github.com/onlinesourdough/openpaperdesign/issues/1), slice B2b, on top
of B1 (`browser-edit-b1.md`) and B2a (`agent-edit-b2a.md`). It is a small local plugin. The
agent deliberately binds one design project in the user's workspace, edits it over MCP, and
opens the editable canvas in Codex's in-app browser on a loopback URL. No AIOS dependency, no
cloud, no telemetry. Not affiliated with Paper (paper.design).

Out of scope: MCP Apps / file entrypoints (the in-app browser at loopback is the editing
surface), plugin publishing, other hosts. The later sidebar home is described in the next section.

## Agents on library canvases, and what is upstream's (issues #6/#10): source candidate, 2026-10-17

Not built, tested, installed or qualified by its writer. The owner's target (2026-10-17): keep
the Home-first plugin. The home owns the overview, the native project inventory and the canvas
library with New/Open. A created or chosen canvas goes to one localhost web editor, in Codex's
built-in browser or an ordinary browser on this computer. The person uses it directly
(New/Open/edit/Saved/Close/reopen with no agent message), and supported MCP agents edit the same
canvas. The MCP App panel and the native paper-rs app are optional.

**Home to web editor, as implemented.** The home's New, Open, Import and "Open current canvas"
call the library actions without `panel`. The server returns the canvas and its same-binding
loopback editor link, and the home checks it against the loopback pattern before asking Codex
once to open it (`ui/open-link`).
- **What it shows.** The home shows the canvas, Close and Export. Under Details it shows the
  port, never the link or its token. A link given makes Close and Switch ask the person first.
- **Readiness.** Asking for the link is never treated as a load. The home reports only what
  its request did (acknowledged, refused, failed or unanswered). Each attempt stands on its
  own: a late reply to an earlier attempt never replaces a later one's outcome. That holds for
  Open canvas and for the panel's browser button, from its link fetch to Codex's answer
  (R12-F1).
- **The editor itself shows Ready and Saved.** The home doesn't infer a load. The binding's
  `editor_loaded` is shared: an earlier tab or the optional panel can set it, so it says nothing
  about this opening. (R11-F1, 2026-10-17, replacing the earlier poll.)
- **Stale and wrong results.** A result opens nothing unless its canvas id is valid and its
  binding is valid and names that same canvas. When an action named a binding ("Edit in this
  view"), the result must carry exactly that one, in the presentation it asked for. A library
  view treats every editor result as a library one, so a reply that omits or changes its
  `storage` label is checked the same way (R12-F2). An answer for another canvas, a link that
  isn't loopback, or a late reply after Back opens nothing either.
- **The panel.** The in-panel editor (same WASM/Session/editor) is offered only as "Edit in this
  view" when the build has it.
- **View version.** This home is `ui://openpaperdesign/home-3.html` (2026-10-17). The earlier
  `home-2` view opened canvases in the panel and expected panel results. Hosts may cache a view
  by its resource URI; OpenAI's [Apps UI guidance](https://developers.openai.com/plugins/build/chatgpt-ui)
  (read 2026-10-17) asks for a new URI, everywhere it is referenced, for breaking HTML/JS/CSS
  changes. So a fresh tool listing or resource read gets `home-3` under a cache key no older
  view used, and a read of an earlier URI is refused. That guidance promises no eviction.
  - **What it doesn't do.** It doesn't evict or refresh a view a host already has open, and it
    doesn't stop such a view from making ordinary tool calls. The server doesn't learn which
    view version is calling (`panel: true` is still a supported request).
  - **Still unproven.** Which `home-3` metadata, resource and visible view a native host
    actually uses still has to be shown through its supported lifecycle.
  - **Not a diagnosis.** This is a compatibility rule, not an explanation of what a Mac host
    cached.
- **Who owns what.** Routing and lifecycle are the home's. Editing belongs to the web editor
  (browser/WASM hosting of upstream's engine). The document, revisions, conflicts and authority
  belong to the server.

A future upstream plugin could replace the thin home/routing/lifecycle adapter, but only once
its actual contracts are verified; compatibility is not assumed.

**Agent contract, as implemented.**
- **Library controls (model-visible), on the existing library operations:**
  - `openpaperdesign_library_canvases`: the list.
  - `_current`: the open canvas's id, name, binding, pages, active page and editor state.
  - `_create {name}` and `_open {id}`.
  - `_close {binding}`.
  - `_editor {binding}`: the loopback editor link, for Codex's built-in browser; it counts as
    a link given.

  None of them takes `force` or `replace`. While a panel is mounted or a link was given, the
  agent can't switch or close the canvas, and must ask the person. A canvas another process
  holds is refused as busy.
- **Design operations: upstream's own.** These are `agent::offered_tools`, the paper-rs
  catalogue minus `export`, `export_combined_pdf` and `create_file`, with their own names,
  descriptions and schemas. Each also requires `binding`. The adapter compares the binding
  under the state lock, removes it, and calls `Server::call_tool`, the path workspace mode
  uses. Reads count per page, session and revision, and only when delivered. Nothing
  `create`/`open`/`current` returns counts as a read, and a caller-supplied session field is
  refused. Successful changes are saved as one commit, with rollback and the `Broken` fence.
- **Same canvas as the person.** The panel and the browser editor poll revisions. A clean view
  picks up agent commits; a stale save is a 409 that overwrites nothing. A stale agent change
  is refused as a conflict.
- **What was missing before.** Library mode offered no model tools besides the home, status and
  inventory. The library actions were app-only, and every upstream design tool was refused.

**Reuse map (upstream paper-rs).** The pinned baseline is upstream `0b1b91da` (from the
2026-10-12 feasibility record). Verified there, not a claim about upstream's latest; no new
upstream source change in this delta.

| Upstream part | How it's used |
|---|---|
| Agent catalogue and execution (`mcp::tools`, `mcp::exec`, `mcp::css`/`html`) | Directly, in the serve crate's host (`agent.rs` `BoundHost`: an upstream `Editor` on its own thread). The library adds only the binding envelope. |
| Document, scene and project structures (`model`, `project`, `project::format`) | Directly: library canvases are standard paper-rs projects. Saves write upstream's page and manifest format. |
| Editor, tools, pointer/keyboard/undo (`editor`, `history`, `layout`, `text`, …) | In the browser through `#[path]` in `openpaperdesign-web` (`WebState` feeds DOM input to `Editor::{pointer_*, key_*}`), and on the server through the agent host. |
| Renderer (`render::{draw, gpu, mesh, rough, scene, tess}`) | Directly, compiled to wasm32 (wgpu WebGPU/WebGL2). |
| Core `#[path]` crate, web/WASM wrappers | Ours: `openpaperdesign-core`/`-web` compile upstream modules unchanged. Our code is `state`/`web` glue, `health` and `keys`. |
| Session lifecycle and editor mount (`static/session.js`, `editor.js`, `panel.js`) | Ours: hosting, fallback, save/conflict, disposal. |
| Library authority, binding, leases, fences, revisions, transport, home view | Ours: `library*.rs`, `launcher*.rs`, `lib.rs` `Server`, `home.html`. |

**Not reused, by inspection.**
- **The egui/winit app shell** (`app`, `ui`, `render::frame`). This covers panels, the
  properties and layers UI, the page list and the command palette. The browser editor has a
  small toolbar (select/hand/rect/ellipse, undo/redo/delete); upstream's keyboard shortcuts
  reach `Editor` except chrome-only ones (`state::chrome_only`). The gap is a browser-hosted
  chrome. The smallest adapter is DOM controls calling the same `Editor` actions, as the
  toolbar does; there is no parity claim.
- **Native `session`** (clocks, storage, autosave): library saves go through `Server`.
- **Upstream's HTTP MCP endpoint** (`mcp::server`). It rejects browser `Origin`s and is not an
  editor URL. The browser never talks to it, and no origin rule was relaxed.

**Native alternative, for comparison only.**
- Upstream `app.rs` runs MCP calls through the running native app's `Editor` on its event loop.
  Its `Session` owns loading, autosave and close, and `paper-rs-mcp` relays stdio to it.
- That route could reuse the egui/winit UI, `Editor`, renderer, `Session` and MCP unchanged,
  with little setup, as a separate window. It would still need its own Mac GUI, agent and
  persistence proof, and one explicit owner of its project session.
- It does not follow our library's advisory lease, transaction or revision boundary (native
  `Session`/`Project::open` don't). It must never open an active `PLUGIN_DATA` canvas
  concurrently, and isn't interchangeable with this path.
- The browser route stays the target. Native is a lower-code standalone alternative only if a
  separate-window experience is later chosen and qualified.

**Unproven.** None of this is proven yet:
- an actual Codex catalogue listing these tools;
- Codex opening the editor link from the home (an earlier Mac run saw no destination for an
  open-link request);
- a native agent edit appearing in the person's browser editor or panel;
- Mac persistence of edits made this way.

## In-panel editor (issue #6): source candidate, 2026-10-16

Not built, tested, installed or qualified by its writer. On the Mac, the host received the
library's app-origin calls, but `ui/open-link` showed no editor destination, so the basic flow
then edited in the home view itself, with the browser editor link as a secondary action.
Superseded on 2026-10-17 by the owner's Home-first decision (section above): the web editor is
primary and this panel is optional ("Edit in this view").

- **Same editor.** The view runs the existing Rust/WASM renderer, `Session` (save, conflict,
  discard, WebGPU-to-WebGL fallback) and one shared mount (`static/editor.js`, also used by
  `app.js`). No second renderer, document store, runtime or dependency.
- **One fixed module.** The home resource was versioned `ui://openpaperdesign/home-2.html` for
  this module (now `home-3`, the browser-first home above).
  Its one `<script type="module">` starts with an editor slot. An app-library server fills that
  slot with the packaged glue, `session.js`, `page.js`, `editor.js` and `panel.js`, in that
  order, only if no part has an import, `</script`, `<script` or `<!--`. Otherwise the slot
  stays `null`. The view then still lists canvases but refuses New, Import and Open itself,
  sending nothing. The server also refuses any `panel: true` request its build can't serve,
  before any lock, file, lease, switch or mount. The browser link is never a substitute.
  Nothing is fetched by URL or evaluated. No CSP, CORS, framing or outbound domain is added.
- **Renderer transfer.** `openpaperdesign_app_editor_asset {part}` sends the packaged wasm in
  sequential 262,144-byte parts, base64, 16 MiB at most. `part` must be a whole number. Each
  answer names the renderer's identity, size, part count, offset and length. The view checks
  them against its own module and stops on any mismatch or when the person leaves. It then
  calls the glue's `__wbg_init({module_or_path: bytes})`.
- **Page transport.** These tools run the same `Server` operations as the HTTP API, never a
  loopback request:
  - `_app_page {binding}` reads the manifest's active page (else its first) and returns a
    `mount` number;
  - `_app_page`, `_app_page_revision` and `_app_page_save` then name that `page_id` and `mount`;
  - `_app_ready` and `_app_page_release` complete the set.

  The server compares the binding and the mount; it checks `page_id` only as a page of the
  canvas (keeping to one page is the view's own rule). A save is one transaction: 409 with the
  current revision, rollback, the `Broken` fence, the committed-save counter, and a bounded
  wait for the writer fence. It changes only its page and the manifest's `modified`. A removed
  page is refused (404). A save that hasn't started never runs if its request is cancelled
  (before or while it waits for admission, or before it takes the writer fence) or if its mount
  is released first. A started one always finishes; Close and the release wait for it.
- **Message limits.** Page messages are limited to 4 MiB as sent, escaping included: a read as
  the whole tool result in its JSON-RPC response, a save as its whole `tools/call` request,
  each plus a fixed 16 KiB allowance for what a host adds. That allowance is a guess, not a
  known host limit. A larger page isn't opened in the panel (no mount is registered), and a
  larger save isn't sent; neither changes any file.
- **Confirmation.** `panel: true` on create, open, import or editor returns no link and gives
  none. Close and Switch need `force` while any editor is mounted or a link was given.
- **Leaving.** Back and Close stop input and polling, wait for the save in flight, save what is
  left and require `can_reload`. A conflict, a failed or unanswered save, or an edit still
  under way keeps the editor, with "Leave without saving" as the explicit discard. The editor
  is then disposed once: frame loop, timers, listeners, pointer capture and wasm handle.
  `ui/resource-teardown` drains first, for up to 8 seconds. An abrupt removal or a crash saves
  nothing. Full screen is requested only when the host lists it.

Unproven until a native run on the exact candidate:
- whether the host's CSP allows WebAssembly compilation;
- WebGL2 or WebGPU in the panel;
- the host's message limits for parts, pages and saves;
- pointer capture, keyboard, focus and resize;
- the whole direct flow: New/Open, visible edit, Saved, Close, fresh-process reopen.

The synthetic tests (`home.test.mjs`, `panel.test.mjs`, `session.test.mjs`, the launcher's Rust
tests) use stand-ins for the host, the browser and the GPU, so they prove none of it.

## App library (issue #6): source candidate, 2026-10-16

Not built, tested, installed or qualified by its writer. The owner admitted (2026-10-16) an
app-owned document store so that the sidebar's New, open, edit, save, close and reopen work
without any model turn or message. The packaged `mcp.json` now starts
`openpaperdesign-mcp --app-library ${PLUGIN_DATA}`. Codex 0.162.1's Agent Plugin MCP loader
expands that literal placeholder to the plugin's data folder, keyed by Codex home, marketplace
and plugin (not version), and creates it. Some loader helper paths pass no data root or fall
back to `<plugin root>/.plugin-data`; that fallback, the package itself and the package cache
are refused as not durable. Whether the installed Mac host uses the normal route is a later
installed check.

- **Modes.** `--app-library <dir>`, `--codex-context` and `--workspace <dir>` are mutually
  exclusive. The workspace modes and every guard and fence below are unchanged; they are now
  started separately (the package no longer starts `--codex-context`).
- **Root.** An existing real folder, given as a plain absolute path. Refused: an unexpanded
  `${…}`, relative or dotted paths, `/`, `$HOME`, links, anything inside `$PLUGIN_ROOT`, a
  `.plugin-data` folder or a `plugins/cache` path. It is pinned by (device, inode) and
  canonical path. No `Workspace` is made for it, and workspace protected-path rules are
  untouched. `doctor` reports `library_usable` and creates nothing.
- **Layout.** `<root>/library/v1/canvases/<id>/` holds one standard paper-rs project per canvas
  (the only live copy). `<root>/library/v1/locks/<id>.lock` is its permanent lease file. Ids are
  16 hex digits made by the server; names are data. Existing projects are never migrated or
  copied.
- **Tools.** In a library process only `openpaperdesign_home` (and its resource),
  `openpaperdesign_status` (`mode: app-library`), `openpaperdesign_projects` (discovery only) and
  the app-only `openpaperdesign_app_canvases`, `_app_create {name}`, `_app_open {id}`,
  `_app_editor`, `_app_close {force}`, `_app_import {payload}` and `_app_export` are answered.
  Arguments are checked by key and type, ids by format, names by length and characters.
  Project routes, the folder picker, the `_ui_*` workspace actions and the upstream design tools
  are refused without admission, retirement or any state change. A workspace process refuses
  the `_app_*` names in the same way. Library stdio page tools are deferred: canvases are edited
  in the browser editor (its loopback `/mcp` endpoint stays as before).
- **Listing.** Read-only and bounded: at most 1000 entries scanned and 200 returned, manifests
  at most 1 MiB, at least one page, and the active page among them. Others are reported as
  skipped. Nothing is created, opened or locked.
- **Lease.** Before anything opens or writes a canvas (including upstream `Project::open`,
  whose stale-temp cleanup deletes every matching temporary file), the process takes a
  nonblocking exclusive `File::try_lock` on the canvas's lease file. That file must be a
  regular, single-link file; after locking, its handle and path must be the same file. A
  canvas held elsewhere is reported busy, and the current canvas, URL and lease stay. Opening
  the canvas already open is a checked no-op. The lease moves into the binding; a switch or
  close retires the old server (admission closed, started writers and rollback drained)
  before its lease is dropped. Process exit releases it. Lock files are never removed,
  truncated, replaced or exported. The lock is advisory: it coordinates OpenPaperDesign
  processes only.
- **Identity checks on access.** A guard re-checks the library folders and the lease (handle,
  path, link count) before every browser API request, every MCP call on the canvas's loopback
  server, `open_editor`-style identity reads and export. The first failure fences that canvas
  for good; the next library action retires it. A replaced root, library folder or lease
  file, or a second link to it, gets 403 from then on, also after the original comes back.
- **Create and import.** The listener is acquired first. Then a fresh id and its lease, then the
  folders, each parent synced with errors reported (unlike upstream `atomic::write`), then the
  files through the checked commit (staged, renamed, directories synced, rolled back on error).
  The bounds are checked before the server is prepared, and preparation finishes before the
  open canvas is retired. The work runs in an owned task, so a caller that goes away can't
  strand it. On a failure only what the call made and is still exactly its own is removed, and
  the parents of removals are synced; anything left, or a removal that may not be durable, is
  named. Nothing is retried automatically. A lost reply after a completed create is recovered
  from the list, and the view says so instead of retrying.
- **Canvas file** (`library_payload.rs`). JSON with `format: "openpaperdesign-canvas"`,
  `version: 1` and `content: "canvas-only"`: the manifest, one page record per manifest page in
  order, and exactly the images the pages use (base64, content-hash ids). Bounds: 32 MiB of
  text, 200 pages, 100 000 nodes per page and 200 000 in all, 256 images of at most 16 MiB, 64
  nesting levels. The contained manifest and pages must be exactly `paper-rs-project` version
  1 and `paper-rs-page` version 3, checked on the raw JSON before any upstream parsing or
  migration (upstream would treat a malformed version as 1, narrow it and subtract from it);
  older and legacy versions aren't supported in canvas files. Each page's JSON must match what
  upstream writes back from it (numbers by value, node order by id), so an unknown, legacy or
  misplaced nested field is refused, not dropped, whatever its value. Only the writer's known
  omissions may be explicit at their defaults: page `camera: null`; node `parent: null`,
  `children: []`, `layout: null`, `instance: null`, `component: false`. Every image is fully
  decoded with the `image` crate (the root's exact dependency, declared for the adapter), never
  registered in upstream's process-wide registry, before anything is published or exported; the
  browser canvas still doesn't draw image fills. Refused: duplicate JSON keys
  anywhere, unknown fields, other formats or versions, companions, unsafe or colliding page ids (case, first six characters), no pages, an
  active page that isn't listed, missing or misordered page records (never a blank stand-in),
  duplicate node ids, node ids above 2^53 - 1 or a `next_id` above 2^53 (checked before the
  upstream parser, which adds one to the largest id), a node graph that isn't one rooted forest
  (walked iteratively from the top level: every node once, parents matching, at most 256
  levels; upstream's own check would accept a disconnected self-consistent cycle), and missing,
  unused, duplicate, non-standard or
  mismatched images. Images come only from the file, or on export from the canvas's own
  `assets/`, never from the process-wide image registry. Import publishes a new canvas with a
  fresh manifest id and opens it; nothing is overwritten or merged. Export needs the canvas
  open. It reads one saved snapshot under the lease and the writer fence (waiting for a save
  under way), bounds and re-validates it, and releases the fence before answering. It holds the
  canvas as last saved, not edits only in a browser tab. Version 1 has no `DESIGN.md`, token or
  other companions, and says so. Project-bound export (`design/<name>/`) is not offered.
- **View.** In a library process the home's mode slot is `library`: it lists the library's
  canvases (no "This chat's project" row, no folder picker), creates with a name only, opens a
  row directly, and asks Codex to open the exact loopback editor link. It shows the canvas id
  and location under Details, and offers Close and Export. Close asks first once the editor
  link was given, and never claims it saved browser memory. Import reads the chosen file's text
  only. Export asks the view to download the text, or says it can't; Codex never confirms a
  saved file. Stale-result, expected-identity and exact-URL guards are as before.
- **Shared views.** Several views (sidebar, chat tabs) share the process's one binding. Each
  binding has an identity, `binding` (`<canvas id>.<n>`, unique in the process), in every
  editor result. `_app_close` and `_app_export` require it, `_app_editor` takes it for the
  readiness poll, and a confirmed switch (`force`) on create, open or import sends `replace`,
  the binding its refusal named (`(open canvas: …)`). Each is compared under the state lock
  before any effect; a stale one is refused, and the current canvas, files, URL and lease stay.
  The view downloads an export only if the answer names the binding and canvas it showed. The
  identity is a stale-view check, not a credential or authority.
- **Readiness.** The browser editor page posts `/api/ready` (token-checked, no content) when
  its renderer is Ready; an in-panel editor reports the same through `_app_ready`. Either sets
  `editor_loaded` in `_app_editor` results. An open-link acknowledgement is never a load.
  Committed saves are counted the same way. (Since 2026-10-17 the home opens the link but
  doesn't poll this shared state: it can't tell which editor set it.)
- **Unchanged limits.** Commits are not crash-atomic, the broken-state fence is in memory, other
  writers are not excluded, and actual navigation from the native sidebar is unproven (the
  inherited Mac observation is an acknowledgement with no loopback hit).

## Sidebar home (issue #6): source candidate, 2026-10-13

Not installed or qualified. [Issue #6](https://github.com/onlinesourdough/openpaperdesign/issues/6)
adds `openpaperdesign_home`, a Codex sidebar (global) entry that is also a chat tab (thread
entry), and its static MCP App view `ui://openpaperdesign/home.html`
(`text/html;profile=mcp-app`; the current view is `ui://openpaperdesign/home-3.html`, the
browser-first home at the top of this record). It also adds a read-only native project inventory,
`openpaperdesign_projects`, and a read-only list of this workspace's canvases,
`openpaperdesign_canvases`. The server now lists 36 tools and one resource.

- **Answered before workspace admission.** The home tool, the view resource and the inventory
  read or change none of the session's context, binding, project, listener or files. The home
  refuses any argument; the inventory accepts only `projectId`. The exemption names exactly
  these three. Every other tool keeps per-call admission and retirement unchanged.
- **Canvases in this workspace** (`openpaperdesign_canvases`, `{}` only).
  - It is admitted like `openpaperdesign_status`: missing, changed or read-only context fails
    closed and retires the session.
  - It lists the projects directly under the validated workspace's `design/`, at most 200,
    with `complete` reporting a cut-off. Each passes the binding checks: a real folder on the
    same file system, and a real `project.json` with safe page ids.
  - Links are reported, never followed, and a linked `design/` is refused.
  - It creates nothing (not even `design/`), binds nothing, starts no server and counts as
    reading no page.
- **`open_editor`** now names its own validated workspace, the bound project's relative and
  absolute path, and its canvas (manifest id, name, page count). The URL and token are
  unchanged. It reads the bound manifest only after the bound root's identity (real directory, same
  canonical path and device/inode) and its entries pass again, as for every API read. A
  replaced or linked root, or a linked `project.json`, is refused. Nothing is then marked as
  opened, and no other project's identity is shown.
- **Native inventory.** This is Codex CLI 0.162.1's experimental app-server, started as
  `codex app-server` from PATH for one exchange:
  - It speaks JSONL over stdio and sends `initialize` with `experimentalApi`, then
    `initialized`, then `project/list` (pages of 100, at most 10) or `project/read`.
  - Every reply is checked against the 0.162.1 schemas. Anything else, a missing CLI, an
    error, a 15 s timeout or an oversized line gives `available: false` with a reason and no
    paths.
  - The child is closed afterwards and killed if it lingers. An MCP cancellation of the
    request kills it and waits for it to exit before the inventory takes the next request; the
    cancelled request gets no reply.
  - There are no threads, turns, settings, auth or project writes.
  - Projects keep Codex's `codex-app-server` namespace, their ids and every root. Ids are not
    joined with desktop or cloud ids by name.
- **The view** (issue #10 minimal home, 2026-10-14; supersedes the `9af7b25` layout).
  - One quiet list of project rows: "This chat's project" first, then the Codex projects. Each
    row shows the name and a generic icon (Codex's project list has no icon field), plus a short
    folder-name or `…id` hint when names repeat. No paths are shown in the list; the folders
    are an optional tooltip. A labelled details button per row reveals the folders, source,
    host and Codex project id from the keyboard.
  - Click or Enter on a row proceeds. A one-folder project opens directly. A project with
    several folders asks "Which folder of …?" first; it never picks a root. Escape or Back
    returns to the list.
  - One labelled **New** menu (top right; arrow keys, Home/End, Escape with focus return):
    New canvas (a name, and a destination select only when projects exist; a project's folders
    with equal names show their shortest distinguishing path, full path as tooltip), Open current
    canvas, Open a folder (the host's folder picker; without one, a hint to add the folder in
    Codex and refresh), and Refresh projects.
  - **Direct actions (owner correction, 2026-10-14).** Basic actions call the server directly;
    no chat message, paste prompt or model turn. Opening a project lists its canvases
    (`openpaperdesign_ui_canvases {root}`); a canvas opens with `openpaperdesign_ui_open {root,
    project}`; New creates with `openpaperdesign_ui_create {root?, name}`; Open current canvas
    uses `openpaperdesign_ui_editor`. Create and open bind the canvas (the session's one
    binding, shared with the agent) and return its editor link, which the view asks Codex to
    open. A switch away from an opened editor is refused until the person confirms (force).
  - **Admission.** These app-only tools need the call's own Codex workspace context, exactly
    as a model call (`Workspace::from_meta`), and `root` must equal that admitted workspace.
    Without it they refuse and change nothing: no pin, no retirement, no binding change. Even
    with valid context, a refused action (another folder, a bad name or canvas, nothing bound)
    pins nothing; only an accepted action pins an unpinned session, a create or open in the same
    bind handoff that installs its binding.
  - **Authority lifetime.** A pin or binding admitted by a direct action is owned by the view's
    authority. A later view call that carries the host's workspace context, where that context
    is downgraded, invalid or names another workspace, retires the session through the same
    fence as a model call: started writes finish, queued and late HTTP, stdio and browser
    writers are refused, and the editor endpoint closes. View calls without that context
    never retire anything, and no view call retires a binding a model call admitted. As for
    model calls, revocation is noticed only on calls, not while idle.
  - **Failed creation.** Creating a canvas (view or model) acquires its listener before
    writing anything. A later failure before the binding is installed removes, under the same
    lock, only what is still provably that creation's: files it finished writing (same
    identity and bytes) and folders it created (same identity at their canonical path, once
    empty). A file replaced or changed, a folder swapped or moved, or anything unknown or
    unobservable (only a missing path counts as gone) stays and is named; existing `design/` content, direction files, tokens and assets are never
    removed, and any existing binding is kept. Not crash atomic; a final check-then-remove
    race and other writers remain unprotected. Tool
    arguments, a listed root and a picked folder grant nothing. On the observed host (model
    view and global entry) app calls carry no workspace context, so these actions are refused
    there with a plain message; a user-opened conversation-panel tab is not yet observed.
  - **Folder picker** (`openpaperdesign_choose_folder`, answered before admission). The server
    sends `openai/elicitation/create` with a `directory` resource input when the client
    advertises `extensions["openai/elicitation"].form` (protocol before 2026-07-28). Only an
    accepted local `file://` URI becomes a selection, decoded without touching the filesystem;
    decline, cancel, timeout, an opaque or malformed answer and host errors select nothing.
    The selection is the next action's `root` only; it is not a workspace, grant or lifetime.
    rmcp decodes the answer into its typed elicitation result first, so unknown top-level
    fields are dropped before the adapter reads `action` and `content`.
  - The New form keeps copies of the destinations it opened with: a refresh while it is open
    can't change what a shown choice sends; the next New canvas uses the new list. Request
    buttons belong to the list that rendered them, so re-rendering the hidden home list (a late
    inventory reply) never strands the visible rows disabled. Details toggles stay usable.
  - Results come from the direct actions or from admitted model results the host delivers.
    Each result, cancellation, Back or a delivered home result replaces the view: earlier
    editor links, busy state and late replies (actions, open-link and inventory) are dropped.
    A refusal, error or unreadable answer is never shown as done. A bound canvas is labelled "current", not open or
    loaded. Results show concise identity (canvas name, folder name); the exact folder and
    project paths and the local port are under Details, which each new result clears and closes.
    The editor link and its token are never shown.
  - A delivered failed model step says only that the agent's last step didn't work and that
    earlier steps may already have happened; it never claims nothing changed.
  - It opens only the exact loopback editor URL through `ui/open-link`. Codex's
    acknowledgement is not proof that the canvas loaded.
  - Host appearance: `HostContext.theme` and a fixed list of standard `styles.variables`
    (colours, fonts, radius) apply on initialize and on merged
    `ui/notifications/host-context-changed` updates, with built-in light/dark fallbacks.
    Values with `url(`, blocks or at-rules are ignored, and `styles.css.fonts` is never
    applied. This is presentation only, never a workspace or permission.
  - The lower-left `onlinesourdough` wordmark uses Geist Pixel Square (SIL OFL 1.1), bundled
    unmodified and served inside the view as a data URI; there is no font URL or service.
  - It calls only the inventory, the picker and the four direct actions, and stores nothing.

Issue #6 stays open until these are proven on a real host:
1. That the installed packaged plugin reaches `codex app-server` and lists the real projects.
   This source is tested against task-owned fake app-servers. On 2026-10-13 an operator ran
   the exact `8a8554c` launcher against the real Codex CLI 0.162.1 over direct stdio on macOS:
   `project/list` and `project/read` passed, with both roots of a local project. That was not
   the packaged plugin inside Codex.
2. Model calls carrying the chosen workspace. The Mac global-entry receipts carried only
   thread ids, with no workspace context. None of the observations so far demonstrates
   handing a chosen root to a desktop chat:
   - `ui/message` carries only `target`/`send` intent.
   - This plugin's separate read-only inventory connection has not been shown to reach the
     desktop conversation.

   These observations don't exhaust what Codex may support. An actual model call with
   matching workspace context and a supported profile stays an open gate. A thread-entry tab
   in a project chat would use that chat's own context, but that is source metadata until it
   is observed on a real host.
3. Delivery of model results to the view.
4. Where `ui/open-link` opens the loopback page.
5. An actual canvas load.

## Installed qualification (2026-10-13)

**PASS for exact `0078487e9f2bea191b8156172daa5cba6d1f17f9` on macOS**, using installed
`openpaperdesign@openpaperdesign-local` 0.1.0, fresh Codex 0.162.1 CLI app-server **model
turns**, and the actual Codex in-app browser. Independent R6 accepted this source with no
material findings (13 focused Rust checks and seven package checks). Installation for
qualification began at 2026-10-12 22:46 UTC while R6 was still running; qualification
completed at 23:05 UTC (2026-10-13 in Copenhagen). Distribution is source-only / local build,
with no packaged installer or public marketplace listing. See
[repository Releases](https://github.com/onlinesourdough/openpaperdesign/releases) for delivery status.

| Check | Observed result |
| --- | --- |
| Binary and discovery | Installed binary matched the exact Mac release; doctor passed, all 23 package files matched, four skills and 33 tools were discovered. |
| Mac checks | 85 distinct serve tests passed (28 library, 24 agent, eight launcher, eight context, 17 API), plus a separate five-test real-TCP context run. The browser bundle was inherited unchanged from `cffeec6`, not freshly rebuilt. |
| Selected workspace and worktree | Real model status/create/read/edit passed under workspace-write. The ordinary Git worktree fixture kept all 32 protected/sibling file hashes unchanged and created its design only inside the selected worktree. |
| Refusal and compatibility | Read-only creation refused with no design output. Direct app-server calls lacking metadata failed closed. A model status call under the unchanged inherited exact `disabled` profile passed. |
| Browser and agent | A CUA pointer move showed Saved. A stale agent `update_styles` call was refused; the two authoritative manifest/page hashes stayed unchanged. After a fresh read, an agent color edit preserved the moved position; browser reload preserved both. MCP `get_screenshot` returned a PNG. |
| Binding and restart | Missing/same-project binds and unforced switch/create guards behaved as expected. Forced switch closed the old port; a later old-tab save visibly failed as server unreachable without changing the saved page. Full MCP/app-server stop, new-process bind/read and a fresh WebGPU browser reopen preserved all 39 nodes; node 28 remained at (720, 566), 230 × 64, fill `#9b5de5`. Probe processes stopped and old ports closed. |

Installed binary SHA-256:
`69cfd77434f17c23c08cbe9b2c22d43151de02309431b69da6d1d24de059734d`.
Project-private receipts are under `.toolchain/operator-evidence/installed-0078487/`
(`installed-model-plugin-proof/qualification-summary.json`, `binary-install.json`, model
readbacks and preservation/reopen proofs); exact Mac checks are under
`.toolchain/operator-evidence/mac-0078487/`. Raw receipts, private paths and token URLs are
not distributed with these docs.

The initial managed-profile attempt inherited `approval_policy: never`; Codex blocked the
MCP call before the plugin. Successful managed tests used ephemeral `on-request` with
individually inspected, once-only approvals, without persistent policy or grant changes.
If the host reports that approval is required but forbidden, use an owner-authorized
per-call approval route or stop and report the host-policy limitation. Do not disable
protection or broaden grants to make the plugin work.

An unreferenced `project 2.json` copy matching the pre-test manifest appeared; its cause is
unknown and it was left untouched. The two authoritative files and document preservation
were verified separately; this is not a claim that every workspace byte stayed unchanged.

![Launch plan reopened in the Codex browser, with the purple rectangle preserved](screenshots/codex-reopen-0078487.jpg)

Observed after full process stop and reopen: Ready, WebGPU, the saved launch-plan canvas
and purple rectangle. This is the operator's unchanged screenshot, not a generated mockup.

## What is in it

- **`openpaperdesign-mcp`** (entrypoint `crates/openpaperdesign-serve/src/bin/openpaperdesign-mcp.rs`,
  shared implementation `src/launcher.rs` in that crate)
  is the plugin's MCP server, over stdio.
  - **Workspace:** the package uses `--codex-context`, pinning Codex's selected workspace
    from per-call metadata. Standalone `--workspace` or deliberate cwd is a separate mode.
    See the versioned contract below; process cwd is not package workspace authority.
  - **Tools:** upstream's design tools, behind B2a's bound-project host and policy, plus:
    - `bind_project {path}`: an existing paper-rs project (has `project.json`) inside the
      workspace. Symlinks are resolved before the inside-check, so links leading out are
      refused. A workspace that is the home folder or `/` is refused.
    - `create_project {name}`: makes `<workspace>/design/<name>/`. It refuses an existing
      folder and a `design/` that leads outside.
    - `open_editor`: returns `http://127.0.0.1:<port>/#token=<token>` for the in-app browser.
      The URL is never written to disk.
    - `openpaperdesign_status`: version, workspace, bound project, whether the editor is
      built in, plus workspace source, permission mode, state and refusal reason.
  - **Before a bind:** valid workspace/permission context is required, then editing tools require a deliberate project bind.
  - **One project per process:** binding again switches projects.
    - **Failed rebind:** the new project is opened and fully checked before the current one
      is touched (containment, `project.json`, page ids, page links). A failed bind leaves
      the current binding, its server, and its editor URL and token working, provided workspace/permission context remains valid.
    - **Rebinding the same project** changes nothing.
    - **In-flight work on a successful switch:** preparation does not reset layout state.
      The old binding closes admission and waits for its started writer/rollback under the
      shared MCP/browser write lock. Queued and late old work is refused without executing.
      Only then are the old page ids released and the new binding activated. Old HTTP
      sessions, routes and cloned server handles remain retired. Cancelling the bind's
      response waiter does not interrupt an already-started handoff.
    - **Unsaved browser edits:** once `open_editor` has handed out the URL, switching stops
      the server that browser tab saves to. Edits made through the tools are saved by each
      call, but the tab may hold an unsaved edit, which could no longer be saved. So the
      switch (`bind_project` or `create_project`) is refused with that explanation until it is
      called again with `force: true`. The skill tells the agent to ask the person first.
      The server doesn't know whether the tab actually has unsaved edits; the earlier claim
      that "changes are already saved" was an assumption and was removed.
    - **Exit:** when the launcher exits (the session ends), an open tab loses its server
      the same way, without a prompt.
  - **Serving:** the bound project is served by the B2a server, in the same process, on
    127.0.0.1 with a fresh 32-byte token. Tool calls go to that server's single host, with the
    same write lock, policy, atomic saves and revisions as its HTTP `/mcp`, so the browser and
    the agent edit the same document. The stdio client is one MCP session of that host, so
    B2a's stale-write check applies: a change to a page the person saved since the agent last
    read it is refused as a `conflict`, and nothing is written.
  - **`openpaperdesign-mcp doctor`** prints the setup as JSON (exit 0 only when the editor is
    built in and the platform is supported). `--version` prints the version.
- **The web bundle is compiled into the command.** `build.rs` embeds
  `target/openpaperdesign-web` (or `$OPD_WEB_BUNDLE_DIR`) when all its files exist (six at
  the time; eight now, with the in-panel editor's `editor.js` and `panel.js`). Otherwise
  the list is empty and `open_editor` says "build the web bundle first", without guessing a
  path at run time. Editing through the tools still works without it.
- **`plugin/openpaperdesign/`:**
  - `plugin.json`, the only manifest (see "Packaging" below): `$schema` agent-plugins 1.0.0,
    MIT OR Apache-2.0, repository onlinesourdough/openpaperdesign, version 0.1.0, and
    `extensions.com.openai` with `interface` and `onboardingSkill`;
  - `mcp.json`: one server, `{"type": "stdio", "command": "openpaperdesign-mcp", "args": ["--codex-context"]}`,
    with no `cwd` (see "Packaging");
  - `NOTICE.md`;
  - skills `openpaperdesign` (bind, edit, `open_editor`, in-app browser, limits), `setup`
    (build, install, `doctor`), and the ported `design` and `review-design`.
- **`.agents/plugins/marketplace.json`**: the local marketplace `openpaperdesign-local`,
  pointing at `./plugin/openpaperdesign`.

### Current-host workspace contract (2026-10-13)

This is experimental support for **Codex 0.162.1**, not a portable MCP workspace standard.
The server advertises `capabilities.experimental["codex/sandbox-state-meta"] = {}`.
Codex supplies `sandboxCwd` (a file URI) and `permissionProfile` in per-call `_meta` on real
model tool calls. rmcp exposes it as `RequestContext.meta`, separate from tool arguments.
The pinned [Codex call path](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/core/src/mcp_tool_call.rs#L853)
and actual operator no-op model calls confirm delivery even when the MCP process cwd is
different. Direct app-server calls do not add this context. No client roots, workspace
interpolation, parent-process discovery or manual per-project overrides are used.

Accepted profiles are deliberately narrow, checked against the pinned
[profile types](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/protocol/src/models.rs#L299)
and [permission semantics](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/protocol/src/permissions.rs#L120):

- Exact `{"type":"disabled"}`: the host sandbox is disabled; plugin containment, identity,
  protected-path rules and lifecycle checks remain enforced. This is not a request to
  enable full access.
- `type: managed`, `network: restricted` or `enabled`, `file_system.type: restricted`, with
  exactly eight entries (order does not matter): special `root` read; one explicit absolute path
  write matching the selected workspace; special `slash_tmp` and `tmpdir` write; and
  `<write-path>/.git`, `.agents`, `.codex`, `.aws` read, each with
  `missing_path_behavior: skip`. Temporary grants never expand project scope.
  Both network strings are observed host values; neither changes the adapter's
  loopback-only listener or initiates outbound requests. Other/missing values refuse.

Read-only, absent, unknown, custom or ambiguous profiles refuse all editing/binding and
writable-browser access. Extra entries/fields, missing exclusions and alternative grant
shapes are unsupported. The four protected path names are refused in project paths,
including canonical aliases; protected entries must be absent or real directories,
except that root `.git` may also be an opaque regular file, as in a Git worktree. The
adapter never reads/parses that file or follows its gitdir pointer; a sibling repository
gains no authority. Symlinked protected entries remain unsupported. These are
also plugin-specific restrictions for the `disabled` variant, not claimed host exclusions.
The URI parser accepts normalized absolute UTF-8 local `file:///` or `file://localhost/`
paths, with strict percent decoding; remote hosts, dot segments, encoded separators,
query/fragment, NUL and invalid encodings refuse. Existing root/home/plugin-folder,
canonical containment, moved/replaced-directory and safe-creation checks remain.

The first valid writable context pins one canonical workspace and its device/inode.
Before that, invalid context stays unresolved and writes nothing. Every later tool call
must provide valid context for that same directory. Missing, changed, unsupported or
no-longer-writable context permanently invalidates the session: admission closes, the
started writer/rollback drains, queued/retained stdio/HTTP/browser calls are fenced, and
the listener stops. Cancellation cannot interrupt retirement. A later valid call cannot
revive it; use a fresh supported session. Valid same-project/failed project rebind behavior
is unchanged. Status reports source, permission mode, state and refusal reason.

This is conservative adapter admission, **not OS sandboxing**. Checks observe tool calls;
idle permission revocation is not detected instantly, and it does not protect a concurrent
native app/external writer. Standalone `--workspace <dir>` or deliberate cwd remains
separate; combining it with `--codex-context` refuses. A standalone doctor pass is a
binary/bundle check, not host context or installed-editing proof.

Historical evidence (2026-10-12/13): `0921280` installation/discovery passed with four
skills and 33 tools, but plugin-cache cwd made workspace status unusable and creation
correctly refused. Later no-op model diagnostics established the current metadata shapes;
they did not edit through this product. Synthetic adapter tests remain distinct from the
installed qualification above.

| Earlier implementation evidence | Result at that revision |
| --- | --- |
| Linux `e004da4` | 83 serve tests; the final protected-workspace guard was rechecked with 28 library + six context tests. The library also passed with the portable listener. Serve clippy/fmt/diff and seven package checks passed. Initial TCP EPERM was resolved by scoped loopback approval. |
| Intermediate Mac `e004da4` | Fresh release, 83 serve tests and five separate real-TCP context tests passed; inherited `cffeec6` bundle. Not installed or published. |
| Linux `0078487` worktree/network correction | 28 library + eight synthetic-context tests with real loopback, serve clippy/fmt/diff and seven package checks passed. Two new regressions first failed on `e004da4`; the corrected fixture preserved opaque pointer bytes and sibling files while editing selected `design/`. |

These runs did not repeat unchanged root/GPU/wasm/browser-bundle baselines. Their original
logs remain retained; exact Mac and installed results for `0078487` are recorded above.

### Packaging (corrected 2026-10-12)

The operator checked draft `1714a58` against the installed Codex plugin-creator 0.1.26
(`references/plugin-packages.md`). The corrections:
- **Fixed discovery:** skills come from the `skills/` folder and the MCP server from the root
  `mcp.json`. It names the server as `type: "stdio"`, command `openpaperdesign-mcp`,
  with no `cwd`. The original `args: []` relied on host process cwd, whose installed
  behavior was unverified at that review. The 092 installation later showed plugin-cache
  cwd and correctly refused creation. The current package uses `args: ["--codex-context"]`
  and the explicit contract above. `plugin.json` still has no top-level `skills`,
  `mcpServers`, `apps` or `interface`.
- **Short description:** `extensions.com.openai.interface.shortDescription` is at most 30
  characters. It is now "Agent + human design canvas" (27).
- **One manifest:** the `.codex-plugin/plugin.json` overlay was removed. Nothing showed that
  the local Codex needs it, and an `extensions.com.openai` object replaces the overlay
  entirely rather than merging with it. Two copies could only contradict each other.
  - **Intended:** per the packaging reference, Codex reads `plugin/openpaperdesign/plugin.json`
    (common fields at the top level, Codex presentation in `extensions.com.openai`) and
    discovers `skills/` and `mcp.json` itself.
  - **Later installed evidence:** on `0921280`, Codex discovered four skills and 33 tools
    without an overlay. That historical discovery check did not qualify editing/browser behavior.
- **Field limits:** the staged reference (`codex-plugin-packages-0.1.26.md`, checked by the
  B2b review) caps only `shortDescription` (30 characters). `defaultPrompt` isn't set. The
  icon fields (issue #8) follow plugin-creator 0.1.26's packaging rule: contained relative
  paths to square PNG/JPEG/WebP/SVG files of at least 48×48 and at most 5 MiB.
- **Icon (issue #8, source only):**
  - The static SVGs live in `assets/`: `logo.svg`/`logo-dark.svg` for the listing, and
    `icon.svg`/`icon-dark.svg` for the composer. Each has a 24-unit `viewBox` and a 48×48
    intrinsic size.
  - The mark is an open sheet with a folded corner and a selection handle.
  - The launcher embeds `icon.svg`/`icon-dark.svg` at build time. It sends them as SVG data
    URIs (light and dark themes) in the home entry's tool `icons` and as the `serverInfo.icons`
    fallback.
  - Whether Codex shows them in the listing, the composer or the sidebar is unproven until
    it is installed.

### Ported design methods (AIOS #54)

The source-only handoff (AIOS `a14f2857`, v0.28.1, archive SHA-256 `c07f5553…`, MIT 2026
onlinesourdough) was verified: all 28 files matched their manifest hashes.

- **Ported:** `design` (skill, references, `create-handoff.mjs`, `paths.mjs`, `serve.mjs`)
  and `review-design` (skill, `audit.md`, `audit.py`), adapted host-neutral. AIOS names and
  links to AIOS-only skills became neutral wording, and the editable companion is now the
  openpaperdesign editor.
- **Left out:** the OpenPencil adapter (`openpencil-workbench`, the OpenPencil route of
  `create-handoff.mjs` and `test_openpencil.py`). It drives an external editor this plugin
  doesn't ship or test, and openpaperdesign is its own editable companion here.
- **Provenance:** each ported `SKILL.md` ends with a note, keeps its `LICENSE`, and is listed
  in `plugin/openpaperdesign/NOTICE.md`.
- **Tests:** ported to `plugin/tests/design/` (Python 3, standard library). They point at the
  plugin and drop the OpenPencil cases; the proof.json case is kept, and a preview-copy case
  replaces the mixed OpenPencil one. CI now runs them, which AIOS did not.

The replacement's local installed qualification now passes on `0078487`, including
discovery of these portable methods. AIOS #54 remains separately owned. Legacy removal
requires a published replacement plus installed readback. This record provides verified
local installation, not an assertion that legacy removal is complete.

## Install on a Mac (operator steps)

These follow the operator's installation contract for the local Codex CLI (operator notes,
2026-10-12): `codex plugin marketplace add <local-repo> --json` and
`codex plugin add openpaperdesign@openpaperdesign-local --json`. Installation and discovery
passed on `0921280`; its selected-workspace editing attempt failed closed. The corrected
`0078487` installed model/browser qualification passed as recorded above. These are
source-only / local-build steps; they do not provide a packaged installer or public
marketplace listing.

```sh
# 1. Build the browser editor and install the command (Rust 1.95 + wasm32 target,
#    wasm-bindgen CLI 0.2.129), from a checkout of this repository at the reviewed commit:
cd <checkout>
./scripts/build-web.sh
# With a shared CARGO_TARGET_DIR, point the build at the bundle just verified:
OPD_WEB_BUNDLE_DIR="$PWD/target/openpaperdesign-web" \
  cargo install --locked --path crates/openpaperdesign-serve --bin openpaperdesign-mcp
openpaperdesign-mcp --version       # openpaperdesign-mcp 0.1.0
openpaperdesign-mcp doctor          # exit 0, "browser_editor": "built in", no missing files
command -v openpaperdesign-mcp      # ~/.cargo/bin/openpaperdesign-mcp

# 2. Add the local marketplace and the plugin (existing plugins stay untouched):
codex plugin marketplace add <checkout> --json
codex plugin add openpaperdesign@openpaperdesign-local --json
```

For an existing 0.1.0 installation, the normal CLI refresh is
`codex plugin remove openpaperdesign@openpaperdesign-local --json`, then the `plugin add`
command above, using the reviewed checkout. This removes only this plugin's local cache;
never edit cached files. Package version remains 0.1.0. The qualification install matched
all 23 package files; verify your own installation rather than assuming a current desktop
chat has reloaded its tool catalog.

3. Start a fresh Codex session after the operator's normal plugin reload. The qualified
   surface was a fresh CLI app-server model session, not a reload of an existing desktop
   chat catalog. Verify installed MCP args `["--codex-context"]`, package files and binary
   identity. Observe the host-policy/once-only approval limitation above.
4. Use **actual model tool calls** in the selected task workspace, first with ordinary
   workspace-write permissions. Direct app-server `mcpServer/tool/call` lacks automatic
   context and is not an installed editing proof. Read back and record:
   - the plugin `openpaperdesign@openpaperdesign-local`, installed and enabled ("Agent + human
     design canvas");
   - its four skills: `openpaperdesign`, `setup`, `design`, `review-design`;
   - the MCP server `openpaperdesign`, started, with `bind_project`, `create_project`,
     `open_editor` and `openpaperdesign_status` among its tools;
   - `openpaperdesign_status`: `version` 0.1.0, intended canonical `workspace`,
     `workspace_source: codex/sandbox-state-meta`, `workspace_state: pinned`,
     `workspace_permission_mode: managed-workspace-write`, `workspace_usable: true`,
     and `browser_editor: built in`;
   - that the `PATH` Codex used found `~/.cargo/bin`;
   - that no file containing the token appears in the workspace or the Codex config.
5. Fixture (a task-owned synthetic project only), in that workspace:
   - `create_project` with the name "Fixture", giving `design/Fixture/`. Or bind an existing
     synthetic project with `bind_project design/<name>`;
   - a successful read of the target page, `get_tree_summary` with `nodeId` `"root"` and its
     `pageId`. The summary `bind_project`/`create_project` returns doesn't count; without
     the read the next change is refused as a `conflict`;
   - a `create_shape` call on that page; `design/<name>/pages/*.json` changes;
   - `open_editor`; open the URL in the in-app browser. The canvas shows the shape;
   - move it in the browser and wait for "Saved", then let the agent edit again (it reads
     first). Reload: both changes are there;
   - conflict: the agent reads, you move the shape and it saves, then the agent edits that
     page. The agent gets a `conflict` and nothing is written;
   - rebind: `bind_project design/<missing>` fails and the open tab still saves;
     `bind_project` to another project is refused (needs `force: true`, explains the loss)
     until forced;
   - shutdown: quitting the session stops the editor server (the tab can't save any more).

   Include a selected Git worktree under ordinary workspace-write, with its root `.git`
   kept opaque and sibling metadata unchanged. Also check one actual model status call
   under the operator's unchanged existing
   `disabled` profile: `workspace_permission_mode: host-sandbox-disabled`, same scope
   guards. This does not authorize changing permissions to make a test pass.
   Don't paste the token URL into reports.

## Tests and results (2026-10-12, implementation worker sandbox, offline, Rust 1.95.0, Node 26.8.1, Python 3)

| Check | Result |
|---|---|
| `tests/launcher.rs` (the real binary over stdio, rmcp child-process client) | 5 passed (details below) |
| `node --test plugin/tests/plugin.test.mjs` | 7 passed (after the packaging correction): plugin.json fields, no top-level `skills`/`mcpServers`/`apps`/`interface`, shortDescription ≤ 30; every `./` path exists inside its root; no `.codex-plugin` overlay; `mcp.json` has exactly one entry, explicit `type: "stdio"`, the command on PATH, no `cwd` and no token; skills' frontmatter and size; the marketplace path resolves (after links) to the plugin folder inside the repository; notices and licenses. On `1714a58`'s files, 2 fail (top-level `skills`, the overlay) |
| `plugin/tests/design` (`python3 -m unittest`) | 12 passed |
| `cargo install --locked --offline --path crates/openpaperdesign-serve --bin openpaperdesign-mcp --root <tmp>` (the setup skill's command, offline here) | installed; release binary 39,032,168 B; `--version` 0.1.0; `doctor`: `"browser_editor": "built in"`, no missing files |
| Full check list | `cargo fmt --all --check`; clippy 0 warnings on the new crates (host) and only upstream `atomic.rs:30` on wasm32; serve 10 unit + 5 agent + 3 launcher + 17, web 111, shader 2; Node 18 (`session.test.mjs`) + 6 (`plugin.test.mjs`); Python 12; wasm32 check; native build; headless 9/0/0; serial 162/0/0 (= baseline, no quota errors); root clippy 1 upstream warning; `cargo tree -p paper-rs -e features` unchanged; `git diff --check` clean |

The launcher tests:

- `bind_edit_and_open_the_editor_inside_the_workspace_only`:
  - the tools are listed and the refused ones aren't;
  - before a bind, editing and `open_editor` are refused;
  - an absolute outside path, `../`, a symlink out, a non-project folder and a missing folder
    are all refused, with the outside project untouched;
  - binding `design/Landing` works and `create_shape` changes its page file;
  - `open_editor` returns a 127.0.0.1 URL with a 64-hex token. On that port `/api/project`
    gives 200 with the token and 401 with a wrong one, and `/` gives 200 (the editor is built
    in here). No token-bearing file appears in the workspace;
  - `create_project` creates `design/New board` and switches to it, and refuses a duplicate.
    After `open_editor`, the switch is first refused without `force` (and nothing is
    created), then succeeds with it.
- `home_folder_workspace_and_escaping_design_folder_are_refused`.
- `embedded_bundle_list_matches_the_server_allowlist`.
- `a_failed_rebind_keeps_the_current_project_and_its_editor` (B2b additions): it binds A,
  edits it and opens the editor. Then forced binds to a project with an unsafe page id, a
  project with a symlinked page file, a folder without `project.json`, a symlink out of the
  workspace and a missing path all fail. After each one, A is still bound, its editor URL
  answers 200 with its token, and its tools work. No file in the workspace, the outside
  folder or the outside project changed. Rebinding A says "already bound". A switch to a
  valid B is refused without `force` (the message says the edits would be lost, and A still
  answers). With `force`, B is bound and A's server stops listening.
- `the_plugin_folder_is_not_a_workspace`: started in a folder that looks like the installed
  plugin, `create_project` is refused, nothing is created, and `workspace_usable` is false.

Rebased on the B2a review round 1 fixes (`a96b32c`, 2026-10-12). The launcher now passes its
session to the host (`Server::call_tool(session, …)`) and the skill tells the agent to read
again after a `conflict`. Re-run after the rebase (proportionate: serve crate, plugin files):
`cargo fmt --all --check`; clippy for serve and web, 0 warnings; serve 4 unit + 9 agent + 3
launcher + 17; `node --test plugin/tests/plugin.test.mjs`; Python design tests; native build;
`git diff --check`. The rows above are from the pre-rebase run; serial and headless are
inherited from it (no root-package file changed).

B2b additions (rebind safety, the `force` switch, `mcp.json` stdio without `cwd`, install
steps), 2026-10-12. Run: `cargo fmt --all --check`; serve clippy, 0 warnings; serve 4 unit +
9 agent + 5 launcher + 17; plugin tests plus the Node CI command, 27 passed; `git diff
--check`. On `aea6397`'s launcher (with the new tests copied in) 3 fail: after a failed
rebind nothing is bound, a switch after `open_editor` needs no force, and a project is
created inside the plugin folder.

Rebased on the B2a review round 2 fixes (`9c3ba69`, 2026-10-12). An MCP session must read a
page before it changes it, and the skill now says to read first. (At this rebase the
launcher's bind-time `get_basic_info` still counted as a read; since the B2b review round 2
fixes it doesn't, see below.) Inline SVG may not
contain `<image>`. Re-run after the rebase: serve 4 unit + 13 agent + 5 launcher + 17;
plugin tests plus the Node CI command; `cargo fmt --all --check`; serve clippy; `git diff
--check`.

### B2b review round 1 (on `b4d41e3`)

| Finding | Fix | Test |
| --- | --- | --- |
| 1. A moved workspace whose old path became a symlink: `create_project` wrote into the link's target | At start the launcher records the workspace's canonical path and (device, inode). Before every bind and create it re-checks: still a real directory, same canonical path and identity. `design/`, `design/<name>/` and `pages/` are made with `create_dir` (no final-symlink following); an existing `design/` must be a real folder. Everything made is verified before any file is written. Upstream `Store::create` isn't used for this. | `a_replaced_workspace_is_refused_not_followed` (the reviewer's sequence, plus a plain directory put back at the same path). It fails on `b4d41e3`, where the project lands in the replacement target. |
| 2. `audit.py`: a companion named `DESIGN.md` overrode a stale reviewed hash (false PASS) | Canonical hashes are checked on their own. Companion entries naming canonical evidence (`BRIEF.md`, `DESIGN.md`, `REVIEW.md`, `proof.json`, `HANDOFF.md`, any case, `./` forms) fail. A defect in the ported AIOS method, recorded in `NOTICE.md` as fixed on port. | `test_companion_entries_never_stand_in_for_canonical_evidence` (REVIEW.md and proof.json forms). It fails with `b4d41e3`'s `audit.py`. |
| 3. Stale `cwd: "."` in this record | Corrected. Intended packaging is separated from unproven installed behaviour. | — |
| 4. Read before write (B2a `9c3ba69`) | The skill says to read the target page before the first change (`get_tree_summary` root, `get_node_info`), not to rely on the bind summary. The launcher tests read first. | `the_stdio_session_must_read_before_it_writes`: after a browser-style save through the editor port, the stdio session's write is refused and the page is unchanged; after a read it lands. It verifies the wiring, so it also passes on `b4d41e3`, which already tracked sessions. |

Remaining limit: verifying and then writing is a check-then-use sequence. A file-system actor
racing between the check and the write isn't excluded (as with B1's documented check/use
race). Fully directory-relative creation would need `openat`-style calls, which the
standard library doesn't offer and which would mean a new dependency.

Checks (2026-10-12): `cargo fmt --all --check`; serve clippy, 0 warnings; serve 4 unit + 13
agent + 7 launcher + 17; plugin tests plus the Node CI command, 28 passed; Python design
tests, 13 passed; `git diff --check`.

Rebased on the B2a review round 3 fixes (`b730f4c`, 2026-10-12). Reads now count per page
and only when they succeed, and the page list is read via `get_basic_info`. The skill says
so. Its tests read the page they write.

B2b review round 2 (on `66a825f`), 2026-10-12:
- **Duplicate evidence:** in the audit, `proof.json` is parsed with duplicate keys rejected
  at any depth (FAIL), and repeated Markdown records fail rather than keeping one. That
  covers reviewed companions, snapshot entries, derived exports, and any field label used
  twice. `create-handoff.mjs` likewise refuses a REVIEW.md that repeats a companion record
  or a field. A duplicate key in its `proof.json` input isn't detectable with `JSON.parse`,
  but every snapshot it makes is checked by the audit, which fails on it. Tests:
  `test_duplicate_evidence_is_never_collapsed` (the reviewer's five cases, plus repeated
  `Result`) and `test_repeated_review_records_refuse_the_handoff`; both fail with
  `535df68`'s scripts (identical to `66a825f`'s).
- **Target-page read:** the summary returned by `bind_project`/`create_project` now runs in
  a one-off session, so it is not the agent's read. Its `next` text, the MCP instructions,
  the skill and the operator fixture all require a successful read of the target page
  (`get_tree_summary` root with its `pageId`) before the first change. The launcher tests
  assert that a change straight after binding is refused. Re-run: serve 4 unit + 18 agent + 7 launcher + 17;
plugin tests plus the Node CI command; Python design tests; fmt, serve clippy and
`git diff --check`.

### Lifecycle correction after final review round 3 (2026-10-12)

Exact `b846b8a` has operator Mac evidence for a built-in editor bundle, doctor, 65 serve
checks and the sequential actual-stdio same-id switch (Y child 300px). The bundle was
inherited from unchanged `cffeec6` web inputs, not freshly rebuilt. Round 3 still rejected
`b846b8a`: overlapping old HTTP calls could restore X's layout state into Y or save X with
incorrect geometry after an early reset. This replacement introduces the retirement
boundary described above. Its regression tests use the actual launcher and MCP handler,
authenticated HTTP/axum connection tasks, worker and disk commits with deterministic
test-only scheduling. Portable tests substitute an in-memory listener and an inert web
bundle; `OPD_LIFECYCLE_TCP=1` selects real loopback for the same unit regressions. The
separate `tests/launcher.rs` continues to drive the actual executable over stdio.

Linux implementation checks for this correction: 23 library tests, 24 agent tests, eight
actual-executable stdio launcher tests and 17 API tests passed. All six deterministic
lifecycle regressions also passed with real Linux loopback. Serve clippy (`--all-targets`),
format and diff checks passed. Initial socket attempts were blocked by the sandbox;
scoped loopback access was then approved and those tests ran. No root/GPU/wasm/plugin
baseline was repeated; their inputs are unchanged. The README addition only prepends a
fork/setup entry; the upstream text below it is preserved byte-for-byte.

At that lifecycle handoff, fresh independent review, Mac lifecycle and installed-plugin/IAB
proof were pending. Later `0921280` installation/discovery passed; workspace editing failed
closed as recorded above. The later exact `0078487` passed R6 source review and the
installed qualification at the start of this record; those results do not rewrite the
earlier revision's proof.

## Remaining limits

- The current desktop chat's tool-catalog reload was not verified; qualification used
  fresh CLI app-server model sessions and the actual Codex in-app browser.
- Support is the experimental Codex 0.162.1 contract and exact profiles above, not universal
  host support or OS sandboxing. Idle host permission changes are not detected instantly.
- Distribution is source-only / local build, with no packaged installer (signed or
  unsigned) or public marketplace listing. Other hosts, an MCP App panel and a remote Z13
  canvas are unqualified.
- No live GPU-loss or physical crash/power-loss test. Untested renderer and interaction
  surfaces in the B1/B2a records remain unqualified beyond the specific observations above.
- Concurrent native/external writers remain outside the guarantee; no multiplayer/cloud.
