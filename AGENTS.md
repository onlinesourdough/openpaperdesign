# AGENTS.md — openpaperdesign

Revision: 0.40 · Updated: 2026-10-17

openpaperdesign is a fork of the
[michaelshimeles/paper-rs](https://github.com/michaelshimeles/paper-rs) repository. The goal is an editable paper-rs canvas inside an agent host, such as Codex's
built-in browser or an MCP App panel ([issue #1](https://github.com/onlinesourdough/openpaperdesign/issues/1)).
It is not affiliated with or endorsed by Paper (paper.design). Do not suggest that it is.

## Upstream and credits

- Keep upstream licenses, notices and history: `LICENSE-MIT`, `LICENSE-APACHE`,
  `THIRD_PARTY.md`, `assets/fonts/OFL.txt` (Inter, SIL OFL 1.1). Any distributed build must
  carry the applicable notices.
- Keep diffs to upstream-owned paths (`src/`, `tests/`, root `Cargo.toml`/`Cargo.lock`,
  `README.md`, `PROGRESS.md`) minimal. Put new code in separate adapter crates under
  `crates/`, which reuse upstream modules via `#[path]`. If an upstream file has to change,
  give the reason in the commit.
- Upstream source changes so far (keep this list current):
  - `src/render/gpu.rs`: the image texture is sampled in uniform control flow (B1, WebGPU
    validation in browsers).
  - `src/history.rs`, `src/editor/pages.rs`: `#[derive(Clone)]` on `History`, `PageStash`
    and `PageSlot` (B2a review round 5). Derive-only, no behaviour change. It lets the agent
    host restore the exact editor state after a failed MCP call.
  - `src/mcp/exec.rs`: `pub fn with_no_stretch` gives access to the process-wide record of
    containers that don't stretch their children (B2a final review). Accessor only, no
    behaviour change. The host copies and restores its own pages' entries with the editor. A
    newly activated project drops stale entries for its page ids. `Server::retire` first
    closes admission and drains the current writer/rollback, then releases every page id
    the host held. Queued and retained old handles cannot execute or save after retirement.
- Upstream sync: `git fetch upstream`, merge the upstream default branch into a branch cut
  from `main`, run the checks below and land it with a PR into `main`. Do not rewrite upstream
  history.

## Working rules

- Stage explicit paths only. Never use `git add -A` or `git add .`, because sandboxed sessions
  can show untracked placeholder dotfiles (`.bashrc`, `.gitconfig`, `.claude/`, ...) at the
  repo root that must not be committed.
- Every GitHub write (issues, PRs, comments) targets `onlinesourdough/openpaperdesign`
  explicitly (`gh ... --repo onlinesourdough/openpaperdesign`), never the upstream repo.
- The tests regenerate the tracked screenshots in `docs/screenshots/`. Restore them
  (`git checkout -- docs/screenshots`) unless a change is meant to update them.
- Commits use Conventional Commits and keep the actual contributors' co-author trailers.

## Toolchain and checks

Rust 1.95.0. A task-local toolchain may be staged in `.toolchain/` (gitignored):

```sh
export RUSTUP_HOME=$PWD/.toolchain/rustup CARGO_HOME=$PWD/.toolchain/cargo PATH=$PWD/.toolchain/cargo/bin:$PATH
```

```sh
cargo build --locked --all-targets
cargo test --locked -- --test-threads=1        # parallel run: see the known SIGSEGV below
PAPER_RS_REQUIRE_GPU=1 cargo test --locked --test headless
cargo check --locked -p openpaperdesign-core -p openpaperdesign-web --target wasm32-unknown-unknown
cargo test --locked -p openpaperdesign-serve -p openpaperdesign-web
node --test crates/openpaperdesign-web/tests/session.test.mjs   # page lifecycle; Node >= 22
node --test crates/openpaperdesign-web/tests/panel.test.mjs     # in-panel editor module; needs build-web output
node --test plugin/tests/plugin.test.mjs                        # plugin manifests and skills
node --test crates/openpaperdesign-serve/tests/home.test.mjs    # sidebar home view protocol
(cd plugin/tests/design && python3 -m unittest)                 # ported design-method tests
./scripts/build-web.sh                         # browser bundle; needs wasm-bindgen CLI 0.2.129
cargo clippy --locked --all-targets
cargo fmt --all --check
```

Add `--offline` when the cargo cache is pre-fetched and the network is off. Known baseline:
the default parallel `cargo test` can SIGSEGV in `tests/mcp.rs` during Vulkan instance setup
when two screenshot tests initialize a GPU at the same time. Serial runs pass. See
`docs/openpaperdesign/feasibility-2026-10-12.md`. When `PAPER_RS_REQUIRE_GPU` is unset, the
GPU tests skip themselves if there is no adapter. The MCP screenshot tests return early without
that guard, so a pass is not proof that rendering happened.

## Browser editing (issue #1)

`openpaperdesign-serve --project <dir>` serves one project to a browser on loopback
(`docs/openpaperdesign/browser-edit-b1.md`). Its revision checks protect only writers that go
through it. Never describe it as protecting a concurrently running native app or other
external writer.

Agents edit the same project over MCP at `/mcp` on that server, with the per-launch token as
`Authorization: Bearer <token>`. Only the bound project is reachable: export and project
creation are refused, and images must be `data:` URIs
(`docs/openpaperdesign/agent-edit-b2a.md`). Never write the token into repository files or commit it.
Conflicts are strict, and nothing merges. A browser save on a stale revision is refused (409).
An MCP session must read a page before it changes it (no first-call exception). Reads are
per page: only a successful call that returned that page counts. Failed calls, and calls
not attributable to one page, acknowledge nothing. The page list (names, order) is read only
via `get_basic_info`. A change is refused as a `conflict` unless every page it writes (and
the page list, if it changes it) is read at its current revision. The browser polls only
while its editor is clean. Page ids come only from the manifest, must be safe file names,
must differ in more than ASCII case (macOS file systems alias them), and must not share
their first six characters, ignoring case (node ids are `<prefix>:<n>`). A call that returns
an error changes nothing, in memory (the editor state, undo history and selection included)
or on disk. Each save (agent call or browser) is one commit that is rolled back on any error,
directory syncs included. If the rollback fails, the answer names the files that may be
inconsistent, and the server refuses every further write. That state is in memory only and
clears on restart, so check the named files first. Commits are not crash-atomic: a crash
mid-commit can leave a mix of old and new files (each one complete). A call that couldn't start within the timeout is never run; a
started call is always waited for; reads count only once their result was delivered. Inline SVG may not
contain `<image>`/`<feImage>`.
The dropped three-way-merge experiment is at the local tag `experiment/b2a-three-way-merge`;
don't bring it back without an explicit decision (collaboration is issue #2).

## Codex plugin

`plugin/openpaperdesign/` is the local Codex plugin (`docs/openpaperdesign/plugin-b2b.md`). Its
MCP server, `openpaperdesign-mcp`, must be on PATH. It embeds the web bundle at build time, so
run `./scripts/build-web.sh` before `cargo install`. `plugin/openpaperdesign/plugin.json` is the
only manifest. Codex discovers `skills/` and `mcp.json` itself, so the manifest has no top-level
`skills`, `mcpServers`, `apps` or `interface`. Codex presentation goes in `extensions.com.openai`
(`interface.shortDescription` ≤ 30 characters). Don't add a `.codex-plugin/` overlay.
`mcp.json` is explicit `type: "stdio"`, args `["--app-library", "${PLUGIN_DATA}"]` (the
app library, below), with no `cwd` or `env`. Workspace modes (`--codex-context`, `--workspace`)
are separately started invocations that keep every guard below; the three flags are mutually
exclusive. In `--codex-context` mode the launcher pins canonical workspace/identity from Codex 0.162.1's experimental
`codex/sandbox-state-meta` in `RequestContext.meta` on real model tool calls. Direct
app-server calls omit it; arguments are never authority. Accept only the observed managed
workspace-write shape or exact `{"type":"disabled"}` profile; never infer missing grants.
Managed network must be `restricted` or `enabled`; neither changes adapter networking.
Root `.git` may be an opaque regular file for a Git worktree: never read or follow its
pointer. Protected project paths and symlinked metadata remain refused.
Read-only/unsupported profiles cannot bind or open a writable browser. Plugin scope and
protected-path checks apply even when the host sandbox is disabled. Missing, changed or
no-longer-writable context after pinning permanently retires the binding through the same
writer fence; caller cancellation cannot interrupt retirement. Checks are per call, not
instantaneous detection of idle host permission changes, and provide no OS sandboxing.
Standalone `--workspace`/cwd mode is separate; reject conflicting flags. The launcher
refuses a moved or replaced workspace. It creates
`design/<name>/` itself without following links. A failed `bind_project` must leave the
current binding working. Replacement preparation must not change the active host's layout
registry. On a successful switch, retire the old server before activating the replacement;
HTTP sessions, stdio clones and browser saves share that fence. Same-project binding is a
no-op. Never claim browser edits are saved on a switch or exit: once
`open_editor` was called, switching needs `force: true`. The summary bind returns runs in a
one-off session and is not the agent's read: instructions must have the agent read the
target page (`get_tree_summary` root with its `pageId`) before its first change. The ported
audit rejects duplicate evidence (JSON keys, repeated records). Keep the ported `design` and `review-design` skills' provenance notes and
LICENSE files, and the plugin's `NOTICE.md`, intact.

Sidebar home (issue #6, `docs/openpaperdesign/plugin-b2b.md`): four things are answered
before workspace admission:
- `openpaperdesign_home`, which refuses arguments.
- The home view resource (`HOME_URI`, currently `ui://openpaperdesign/home-3.html`).
- The read-only native inventory `openpaperdesign_projects` (only `projectId`).
- `openpaperdesign_choose_folder` (`{}` only): the host directory picker
  (`openai/elicitation/create`, `directory` resource input) for a client that advertises
  `extensions["openai/elicitation"].form` on a protocol before 2026-07-28. Its answer is input
  only: never opened, listed, looked up or treated as a workspace or grant.

They read or change none of the session's context, binding, project, listener or files. This
named exemption covers only those four (plus the fixed refusal of the app-library tool names,
which reads and changes nothing); never route another tool around admission. The home resource
URI is versioned: hosts may cache a view by its URI, so any incompatible change to the view's
code, presentation or the responses it expects gets a new URI in `HOME_URI` (every tool and
resource registration uses it) and the URI tests. No earlier URI is served or aliased. History:
`home.html`; `home-2` added the in-panel editor; `home-3` (2026-10-17) is the browser-first
home, where New/Open/Import/Open current canvas open the web editor link and the panel is
optional.
The plugin's icon assets are static SVGs in `plugin/openpaperdesign/assets/` (no script,
`foreignObject`, style, text or external reference; at least 48×48 intrinsic). The launcher
embeds `icon.svg`/`icon-dark.svg` at build time, so rebuild after changing them.
`openpaperdesign_canvases` (read-only, bounded list of `design/` projects) and `open_editor`'s
workspace/project/canvas identity are ordinary admitted calls, not exemptions. `open_editor`
reads the bound manifest only after the bound root's identity and entries pass again
(`Server::bound_manifest`). Otherwise it refuses and marks nothing as opened. The home is
both a global and a thread entry.

The inventory starts `codex app-server` from PATH for one bounded exchange. It sends only
`initialize` (`experimentalApi`), `initialized`, and `project/list` or `project/read`, with
schema checks, a deadline and kill-on-drop. An MCP cancellation (`RequestContext.ct`) kills
and waits for its child before the inventory is free again. Never add thread, turn, settings, auth or project
writes to it. It keeps Codex's namespace and ids, and every root. A listed project or root is
discovery, never authority: never join it with other ids by name, and never pick a root for
the person.

The view's basic actions call the server directly (owner correction, 2026-10-14): never
`ui/message`, a paste prompt or a model turn. The app-only `openpaperdesign_ui_canvases`,
`_ui_create`, `_ui_open` and `_ui_editor` are admitted only from that call's own
`codex/sandbox-state-meta` (the same `Workspace::from_meta` as model calls), and a named folder
must equal the admitted workspace exactly. A refused direct action never pins; an unpinned
session is pinned only by an accepted one (a listing under its lock, or a create/open inside the
bind handoff that installs the binding). Each pin and binding records whose authority admitted
it (`Owner::Model` or `Owner::Direct`). A view call carrying the host's context key whose
context is invalid, downgraded or names another workspace is an observed revocation: when the
authority in force is the view's own, it retires the session through the same
`retire_session` fence as a model call (admission closed; started writes finish, queued and
late HTTP, stdio and browser writers refused). A view call without that key observes nothing,
and no view call retires what a model call admitted. Revocation is seen only on calls, for
both origins. They reuse the model path's create/bind/editor code and its one bound project;
keep a single binding per process (no pool or second UI binding). Project creation (both
origins) acquires the binding's listener before writing anything; a later failure before the
binding is installed removes, under the same lock, only what is still provably that
creation's: files it finished writing with the same identity and bytes, and folders it created
with the same identity at their canonical path, once empty. Anything replaced, changed, moved,
unknown or unobservable (only NotFound counts as gone) stays and is named (never existing `design/`, direction files, tokens or assets).
The check-then-remove path race, crashes and other writers remain unprotected. Observed so far: app-view calls
carry no workspace context, so these actions are refused on that host; that is the honest
result, not a reason to borrow model context, use a picked folder or fall back to messages. The
view stores nothing (no URL, token or path) and opens only the exact loopback editor URL. Host theme and style variables (MCP Apps
`HostContext.theme`/`styles.variables`) change presentation only: apply a fixed list of
standard names with plain values, never `styles.css.fonts`, and never treat them as a
workspace or permission. A listed or picked folder travels only as the action's exact `root`.
Several folders are asked, never picked. Results and busy state belong to the view that started
them: a newer result, cancellation or Back drops late replies and old editor links. Request
buttons belong to the list that rendered them; the New form sends only the destinations it
showed (copied when it opens). Result paths and the port stay under Details, cleared per
result. The wordmark font
(Geist Pixel Square, OFL, `crates/openpaperdesign-serve/assets/`) is embedded unmodified at
serve time; never load a font by URL.

App library (issue #6, owner admission 2026-10-16, `crates/openpaperdesign-serve/src/library*.rs`,
`launcher_app.rs`): `--app-library <dir>` is a separate process mode for canvases the app owns.
Codex's Agent Plugin loader expands the literal `${PLUGIN_DATA}` argument (no shell); the root
must be an existing real folder given as a plain absolute path, never `/`, `$HOME`, inside
`$PLUGIN_ROOT`, a `.plugin-data` fallback or `plugins/cache` (not durable), and is pinned by
identity. Library validation is its own code: never build a `Workspace` for it, never loosen
workspace protected-path rules, never use cwd, a picked or listed folder, model metadata or a
tool argument as a path. Layout `<root>/library/v1/canvases/<id>/` (a standard paper-rs project,
the only live copy) and `locks/<id>.lock`; ids are 16 hex digits made by the server; names are
data. Listing creates and locks nothing and is bounded (entries and manifest bytes). In that
process only the home, its resource, `openpaperdesign_status` (library report), the read-only
inventory and the app-only `openpaperdesign_app_{canvases,create,open,editor,close,import,export}`
and in-panel editor tools (`_app_editor_asset`, `_app_page`, `_app_page_revision`,
`_app_page_save`, `_app_ready`, `_app_page_release`), plus the agent's library controls and
binding-guarded upstream design tools (below),
are answered, with strict argument keys and types; every project route (including the
`_ui_*` actions, the picker and the upstream design tools) is refused without admission or
state change, and a workspace process refuses the `_app_*` and `_library_*` names the same way. One binding per
process (the page-id layout registry forbids a second slot). Each canvas needs a permanent
per-id lease: nonblocking `File::try_lock` on a regular single-link lock file, taken before
`Project::open` (its stale-temp cleanup) or any write, rechecked (handle and path identity, link
count, library folders) by a guard on every browser and MCP access; a failed check fences that
canvas for good, and the next action retires it. Lock files are never removed, truncated or
replaced, and never exported. The lease moves into the binding and is dropped only after
retirement drained it, also at the normal end of the service: when stdio closes or the service
fails, `main` runs `Launcher::shutdown`, which refuses any later create/bind/install and retires
the final binding (started writer and rollback drained, queued and late writers refused) in an
owned task spawned before any wait, even for the state lock, before its lease drops (both
modes); same id is a checked no-op; busy, invalid or failing targets keep the
current canvas and URL. It is advisory: no protection from other programs, no OS sandbox.
Create/import write through the checked commit with every new folder's parent synced, finish
preparation before the old binding retires, run in an owned task, remove only what is still
theirs (naming what stays or may not be durably removed) and are never retried; a lost reply is
recovered from the list. The canvas file (`library_payload.rs`, version 1, canvas-only) is
bounded and fully validated before a fresh identity is published (duplicate keys, unknown
fields, contained manifest/page exactly `paper-rs-project` 1 / `paper-rs-page` 3 checked raw
before any upstream parsing or migration, each page's JSON matching what upstream writes back
from it so no nested field is silently dropped (only the writer's known omissions at their
defaults may be explicit: page `camera: null`; node `parent: null`, `children: []`,
`layout: null`, `instance: null`, `component: false`), ids, case/prefix collisions, page
correspondence, node ids at most 2^53 - 1 and
`next_id` at most 2^53 checked before upstream parsing, one rooted forest walked iteratively
from the top level with matching parents and at most 256 levels, exact image set); images
come only from the file or the canvas's own `assets/`, never the process image registry. Export
reads one saved snapshot under the lease and the writer fence and says it holds saved content
without direction or tokens. Companions (`DESIGN.md`, tokens) are unsupported in version 1 and
refused; project-bound export is not offered. A new blank canvas infers no design system. The
editor reports readiness itself (`POST /api/ready`); an open-link acknowledgement is never a
load. Close retires and saves nothing in browser memory; it needs `force` once the link was
given. Views share the one binding: each binding has an identity (`binding`, `<id>.<n>`) in
editor results; close, export and readiness polls send it, and a confirmed switch sends the
`replace` binding its refusal named; all are compared under the state lock before any effect,
and a stale one is refused with the current canvas, files, URL and lease untouched. It is a
stale-view check, never a credential or authority. An import's file read belongs to the view
generation and selection that started it; a superseded read sends nothing and shows nothing.
Every image of an imported or exported canvas file is fully decoded with the `image` crate
(declared in `openpaperdesign-serve` with the root's exact requirement), never through
`assets::register`, before anything is published; hash, signature, base64 and exact-set checks
stay. The browser canvas still doesn't draw image fills.

Agents on library canvases (owner direction 2026-10-17, `launcher_library_agent.rs`): the
model sees `openpaperdesign_library_{canvases,current,create,open,close,editor}` and
upstream's offered design tools under their own names, descriptions and schemas, each with a
required `binding` (the open canvas meant). The binding is compared under the state lock after
the canvas/library/lease re-check, removed, and the call goes to `Server::call_tool` with this
process's session, exactly as in workspace mode: same policy, per-page read-before-write
acknowledgements, one commit with rollback/`Broken`, lifecycle fence for queued and late calls.
A caller-supplied session field is refused. A stale or missing binding is refused with no
effect. The model library controls take no `force`/`replace`: a canvas with a mounted panel or a
given link is never switched or closed by the agent. Only `_library_editor` hands out the
loopback link (a link given); no other agent result carries the URL or token. Cross-process
ownership stays the lease's (busy is refused, never transferred). The panel and browser editor
see agent commits through their revision polls; their stale saves are 409 conflicts. Never add
a parallel editing API, drawing DSL or document format here: reuse upstream's commands and
renderer, and keep the adapter to binding, authority, transport and hosting.

In-panel editor (owner direction 2026-10-16, `static/{editor,panel}.js`, `launcher_app.rs`):
the library home can edit a canvas in its own MCP App view with the same Rust/WASM renderer,
`Session` and editor mount as the browser page (`editor.js`, import-free, shared by `app.js`).
The launcher inlines one fixed module into the view's one `<script type="module">` (glue,
`session.js`, `page.js`, `editor.js`, `panel.js`, in that order) and refuses to embed text with
`</script`, `<script`, `<!--` or an import; no eval, URL import, outbound domain, CSP, CORS or
framing change. The renderer comes only from the packaged wasm, in sequential 256 KiB parts
(integer `part`, identity/size/count/length checked, 16 MiB cap) and goes to the glue's own
`__wbg_init({module_or_path: bytes})`. Page calls name the view's `binding`, then the `page_id`
and `mount` of the first read (the manifest's active page, else its first). Binding and mount
are compared under the state lock, which is released before the writer fence (never
reversed); `page_id` is only checked as a manifest page (a mount isn't tied to a page; the view
keeps to its own). They use the same `Server::{page, page_revision, save_page, mark_ready}` as
HTTP: one transaction, structured 409/rollback/`Broken`, bounded queue. A save carries its
mount's lifetime flag, checked again under the writer fence; a release clears it under the
state lock, then waits for the fence before it is acknowledged, so a queued save of that mount
never runs and a started one finishes first (the binding stays). A cancelled save never starts:
cancellation is observed before and during admission and before the fence is taken. A started
save always finishes, and retirement waits. A save writes only its page and the manifest's
`modified`. Panel page messages are capped at 4 MiB as sent: a read as the whole tool result in
its JSON-RPC response, a save as its whole `tools/call` request, each escaping counted, plus a
fixed 16 KiB host allowance (not a known host limit), separately from the 64 MiB page limit;
larger pages stay browser-only, untouched, and no mount is registered for them. `panel: true` on create/open/import/
editor returns no link and gives none; mounted panels and a given link both make Close/Switch
need `force`. Back and Close drain (wait for the save in flight, save what is left, require
`can_reload`); conflict, failed/unanswered save or an edit under way keep the editor until an
explicit discard. Teardown uses the same drain, but an abrupt host removal or crash saves
nothing. Ready is the mount's own renderer's; Saved only a matching commit answer. The panel is
optional (owner decision 2026-10-17): the home offers it only as "Edit in this view" for a
shown canvas when its build has the module, and the server refuses `panel` requests its build
can't serve before any lock, file, lease, switch or mount. Host CSP for WebAssembly,
WebGL2/WebGPU, message limits, input and focus in the native panel are unproven.

Home and the web editor (owner decision 2026-10-17): the home stays the entry (overview, native
project inventory, library list, New/Open/Import/Close/Export). Its primary New/Open/Import and
"Open current canvas" hand the canvas to its one loopback web editor: the result's own
same-binding link, checked against the loopback pattern and asked to open once (`ui/open-link`)
in Codex's browser; an ordinary browser on this computer can use the same link. That counts as a
link given, so Close/Switch then ask first. Whether a result is a library one comes from the
view's own mode and the library action that asked, never from the reply's `storage` label alone.
A library result is used only if its canvas id is valid and its binding is valid and names that
canvas (and is exactly the binding an action named, when it named one), in the presentation the
action asked for (link or panel); otherwise nothing opens and no action targets it. The home
reports only what its opening request did (each attempt on its own, for Open canvas and the
panel's browser button alike, from link fetch to Codex's answer; a late reply never replaces a
later one) and never infers a load: the binding's shared `editor_loaded` can come from an
earlier tab or the panel, so the editor itself shows Ready and Saved. The home owns
routing and lifecycle; the web editor (browser/WASM, upstream engine) owns editing; the server
owns the document, conflicts and authority. Don't add a second library home or editor.
Native qualification is pending.

## Model profile

- Implementation: `claudeAgent` `claude-opus-5-5`, effort `high`, fastMode `false`.
- AgentOps, research and independent review: `codex` `gpt-6.1-sol`, reasoningEffort `max`, serviceTier `default`.
- Owner-selected defaults and native project/AgentOps readback verified 2026-10-13.
  Native project configuration owns the
  implementation default; thread selections own AgentOps and review. Preserve the actual
  settings of an in-flight turn and apply changes at its next safe continuation.
