// The home view (src/home.html) against a synthetic MCP Apps host: a fake parent window and a
// minimal fake DOM. Protocol and state behaviour only; this is not a real Codex host or
// browser, and it proves nothing about rendering or real accessibility.
// Run: node --test crates/openpaperdesign-serve/tests/home.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const html = readFileSync(new URL('../src/home.html', import.meta.url), 'utf8');
// The view's one inline module. Its first line is the editor slot (`opdCreatePanel = null`,
// filled in by an app-library server); the rest is a plain script, so it runs here as one, in
// a fresh context. Tests that need the in-panel editor put a recording fake in the slot; the
// real module (renderer glue, session, page, editor, panel) is checked in
// crates/openpaperdesign-web/tests/panel.test.mjs.
const MODULE_OPEN = '<script type="module">';
const PANEL_SLOT = 'const opdCreatePanel = null; // __OPD_PANEL_MODULE__';
const script = html.slice(html.lastIndexOf(MODULE_OPEN) + MODULE_OPEN.length, html.lastIndexOf('</script>'));
assert.ok(script.startsWith('\n' + PANEL_SLOT + '\n'), 'the editor slot is the module\'s first line');
const flush = async () => { for (let i = 0; i < 4; i++) await new Promise((r) => setImmediate(r)); };
const TOKEN = 'ab'.repeat(32);
const URL_OK = `http://127.0.0.1:41234/#token=${TOKEN}`;
// `ui/request-display-mode` only from the in-panel editor's Full screen button, when offered.
const ALLOWED = new Set(['ui/initialize', 'ui/notifications/initialized', 'ui/open-link', 'tools/call', 'ui/request-display-mode']);
const INVENTORY = 'openpaperdesign_projects';
// Every server tool the view may call: the inventory, the host folder picker and the four
// direct actions. The server admits the actions only with the call's own workspace context.
const TOOLS = new Set([INVENTORY, 'openpaperdesign_choose_folder', 'openpaperdesign_ui_canvases', 'openpaperdesign_ui_create',
  'openpaperdesign_ui_open', 'openpaperdesign_ui_editor']);
// What the view may call when an app-library server serves it: only the library's own actions.
const LIB_LIST = 'openpaperdesign_app_canvases';
const LIB_TOOLS = new Set([LIB_LIST, 'openpaperdesign_app_create', 'openpaperdesign_app_open', 'openpaperdesign_app_editor',
  'openpaperdesign_app_close', 'openpaperdesign_app_import', 'openpaperdesign_app_export']);

// Fictional fixtures. Two local projects share a name; Acme has two folders.
const ACME = { source: 'codex-app-server', host: 'this computer', id: 'prj_fixture_acme', name: 'Acme site', position: 0, recencyAt: null, roots: [{ path: '/w/acme' }, { path: '/w/acme-docs' }] };
const BETA = { source: 'codex-app-server', host: 'this computer', id: 'prj_fixture_beta', name: 'Acme site', position: 1, recencyAt: null, roots: [{ path: '/w/beta' }] };
const NOTES = { source: 'codex-app-server', host: 'this computer', id: 'prj_fixture_notes', name: 'Field notes', position: 2, recencyAt: null, roots: [{ path: '/w/notes' }] };
const LISTED = { available: true, source: 'codex-app-server', host: 'this computer', complete: true, projects: [ACME, BETA, NOTES] };

function el(tag = 'div', doc) {
  const attrs = {};
  const e = { tag, id: '', title: '', hidden: false, disabled: false, className: '', value: '', type: '', onclick: null,
    onsubmit: null, onkeydown: null, children: [] };
  e.append = (...c) => { e.children.push(...c); };
  e.setAttribute = (k, v) => { attrs[k] = String(v); };
  e.getAttribute = (k) => (k in attrs ? attrs[k] : null);
  e.focus = () => { if (doc) doc.activeElement = e; };
  // Setting textContent clears children, as in the DOM.
  let text = '';
  Object.defineProperty(e, 'textContent', { get: () => text, set: (v) => { text = String(v); e.children = []; } });
  return e;
}
const all = (e) => [e, ...e.children.flatMap(all)];
const texts = (e) => all(e).filter((n) => n.tag !== 'svg' && n.tag !== 'path').map((n) => n.textContent).filter(Boolean);
const buttons = (e) => all(e).filter((n) => n.tag === 'button');

const IDS = ['back', 'title', 'menu-button', 'menu', 'm-new', 'm-current', 'm-folder', 'm-import', 'm-refresh', 'live', 'notice',
  'import-file', 'home', 'inventory', 'projects', 'pick', 'pick-title', 'pick-list', 'new-form', 'new-title', 'name', 'where-field', 'where',
  'create', 'cancel-new', 'folder-help', 'folder-title', 'folder-refresh', 'folder-done', 'result', 'result-title',
  'result-state', 'result-fields', 'result-list', 'result-actions', 'canvas', 'edit-here', 'reopen', 'force', 'close', 'export', 'canvas-state',
  'result-details', 'result-meta', 'result-detail', 'app', 'editor', 'ed-title', 'ed-status', 'ed-select', 'ed-hand', 'ed-rect',
  'ed-ellipse', 'ed-undo', 'ed-redo', 'ed-delete', 'ed-stage', 'ed-canvas', 'ed-conflict', 'ed-close', 'ed-leave', 'ed-export', 'ed-full',
  'ed-browser', 'ed-note', 'ed-details', 'ed-meta', 'ed-renderer'];
// The view as an app-library server serves it (its one mode slot filled in).
const libScript = script.replace("'__OPD_MODE__'", "'library'");
// ...with an in-panel editor in its editor slot: the test's recording fake (`fakePanel`).
const panelScript = libScript.replace(PANEL_SLOT, 'const opdCreatePanel = __opdTestPanel;');

// `clock: true` gives the view a live, deterministic clock (`tick(ms)` runs the timers set for
// that delay; `runAllTimers()` runs every timer set so far). Without it timers never fire.
// Adapted from the R11 reviewer's timer-active probe harness.
function mount({ library = false, panel = null, clock = false } = {}) {
  const timers = [];
  const setTimeout = clock ? (fn, ms) => { timers.push({ fn, ms }); return timers.length; } : () => 0;
  const tick = (ms) => {
    const due = timers.filter((t) => t.ms === ms);
    for (const t of due) timers.splice(timers.indexOf(t), 1);
    for (const t of due) t.fn();
  };
  const runAllTimers = () => { for (const t of timers.splice(0)) t.fn(); };
  const document = { activeElement: null };
  const els = Object.fromEntries(IDS.map((id) => [id, el('div', document)]));
  for (const id of ['back', 'menu', 'pick', 'new-form', 'folder-help', 'result', 'canvas', 'edit-here', 'reopen', 'force', 'close', 'export',
    'm-import', 'import-file', 'result-details', 'result-meta', 'result-detail', 'editor', 'ed-leave', 'ed-full']) {
    els[id].hidden = true;
  }
  const props = {};
  const rootAttrs = {};
  const documentElement = { style: { setProperty: (k, v) => { props[k] = v; } }, setAttribute: (k, v) => { rootAttrs[k] = String(v); } };
  const sent = [];
  let listener = null;
  // Copy into this realm so deep equality is not confused by the VM context's prototypes.
  const parent = { postMessage: (m) => sent.push(JSON.parse(JSON.stringify(m))) };
  const forbidden = () => { throw new Error('storage or network access'); };
  const window = { parent, addEventListener: (n, fn) => { if (n === 'message') listener = fn; } };
  for (const k of ['localStorage', 'sessionStorage', 'indexedDB', 'fetch', 'XMLHttpRequest']) {
    Object.defineProperty(window, k, { get: forbidden });
  }
  Object.assign(document, {
    getElementById: (id) => els[id], createElement: (tag) => el(tag, document),
    createElementNS: (_ns, tag) => el(tag, document), documentElement,
  });
  Object.defineProperty(document, 'cookie', { get: forbidden, set: forbidden });
  const source = panel ? panelScript : library ? libScript : script;
  vm.runInNewContext(source, { window, document, setTimeout, JSON, String, RegExp, Array, Map, Promise, Object, Number, Math,
    __opdTestPanel: panel });
  const from = (m, source = parent) => listener({ source, data: { jsonrpc: '2.0', ...m } });
  const last = (method) => [...sent].reverse().find((m) => m.method === method);
  const count = (method) => sent.filter((m) => m.method === method).length;
  return { els, sent, from, last, count, parent, document, props, rootAttrs, timers, tick, runAllTimers };
}

const toolText = (obj) => ({ content: [{ type: 'text', text: JSON.stringify(obj) }] });

/// Initialize, then answer the inventory call (if any) with `inventory`.
async function initialized({ caps = {}, context = {}, inventory = LISTED } = {}) {
  const v = mount();
  v.from({ id: v.sent[0].id, result: { protocolVersion: '2026-01-26', hostCapabilities: caps, hostContext: context } });
  await flush();
  const call = v.last('tools/call');
  if (call && inventory !== null) {
    v.from(inventory.error ? { id: call.id, error: inventory.error } : { id: call.id, result: inventory.isError ? inventory : toolText(inventory) });
    await flush();
  }
  return v;
}

const methods = (v) => v.sent.filter((m) => m.method).map((m) => m.method);
const onlyAllowed = (v) => {
  assert.ok(methods(v).every((m) => ALLOWED.has(m)), methods(v).join());
  for (const m of v.sent.filter((m) => m.method === 'tools/call')) assert.ok(TOOLS.has(m.params.name), m.params.name);
  assert.equal(v.count('ui/message'), 0, 'no message to the agent, ever');
};
// The view's direct actions (every tools/call except the inventory), in order.
const actions = (v) => v.sent.filter((m) => m.method === 'tools/call' && m.params.name !== INVENTORY).map((m) => m.params);
const lastAction = (v) => [...v.sent].reverse().find((m) => m.method === 'tools/call' && m.params.name !== INVENTORY);
// Answer a direct action: a result object, a refusal (isError text) or a JSON-RPC error.
async function answer(v, reply, call = lastAction(v)) {
  if (reply.error) v.from({ id: call.id, error: reply.error });
  else if (reply.refused !== undefined) v.from({ id: call.id, result: { isError: true, content: [{ type: 'text', text: reply.refused }] } });
  else v.from({ id: call.id, result: toolText(reply) });
  await flush();
}
async function ack(v, method = 'ui/open-link', reply = { result: {} }) {
  v.from({ id: v.last(method).id, ...reply });
  await flush();
}
const result = (obj, isError = false) => ({ method: 'ui/notifications/tool-result', params: { content: [{ type: 'text', text: typeof obj === 'string' ? obj : JSON.stringify(obj) }], isError } });
// The rows of the project list, as [name text, hint text, open button, details button].
const rows = (v, list = 'projects') => v.els[list].children.filter((li) => li.children.length).map((li) => {
  const row = li.children[0];
  const [open, more] = row.children;
  const [name, hint] = open.children[1].children;
  return { name: name.textContent, hint: hint ? hint.textContent : '', open, more, li };
});
const row = (v, name, hint = '') => {
  const r = rows(v).find((x) => x.name === name && x.hint === hint);
  assert.ok(r, `row ${name} ${hint}`);
  return r;
};
const NO_CONTEXT = "This view's request carries no Codex workspace context, so OpenPaperDesign can't read or change project files from here. Nothing was read or changed. (Missing Codex workspace context.)";

// ---- connection, inventory, rows ----

test('initialize claims only display modes; the menu waits for Codex; then the inventory is read', async () => {
  const v = mount();
  assert.equal(v.sent[0].method, 'ui/initialize');
  assert.deepEqual(v.sent[0].params.appCapabilities, { availableDisplayModes: ['inline', 'fullscreen'] });
  assert.ok(v.els['menu-button'].disabled, 'no action before Codex answers');
  v.from({ id: v.sent[0].id, result: { hostCapabilities: {} } });
  await flush();
  assert.deepEqual(methods(v), ['ui/initialize', 'ui/notifications/initialized', 'tools/call']);
  assert.deepEqual(v.last('tools/call').params, { name: INVENTORY, arguments: {} });
  assert.ok(!v.els['menu-button'].disabled);
  assert.deepEqual(rows(v).map((r) => r.name), ["This chat's project"], 'this chat\'s project is usable before the list arrives');
  onlyAllowed(v);
});

test('rendered for a model result, the view does not read the inventory', async () => {
  const v = await initialized({ context: { toolInfo: { tool: { name: 'openpaperdesign_status' } } } });
  assert.deepEqual(methods(v), ['ui/initialize', 'ui/notifications/initialized']);
  const home = await initialized({ context: { toolInfo: { tool: { name: 'openpaperdesign_home' } } } });
  assert.ok(methods(home).includes('tools/call'));
});

test('a failed initialize keeps the menu disabled, says so and sends nothing else', async () => {
  const v = mount();
  v.from({ id: v.sent[0].id, error: { code: -32603, message: 'at /Users/someone/private', data: { token: TOKEN } } });
  await flush();
  assert.deepEqual(methods(v), ['ui/initialize']);
  assert.ok(v.els['menu-button'].disabled);
  assert.match(v.els.inventory.textContent, /Couldn't connect to Codex \(code -32603\)/);
  const shown = v.els.inventory.textContent + v.els.live.textContent;
  assert.ok(!shown.includes('/Users/') && !shown.includes(TOKEN));
});

test('rows show a name, a generic icon and only a short hint; same names stay distinguishable', async () => {
  const v = await initialized();
  assert.equal(v.els.inventory.hidden, true, 'no status line in the normal state');
  assert.deepEqual(rows(v).map((r) => [r.name, r.hint]),
    [["This chat's project", ''], ['Acme site', 'acme'], ['Acme site', 'beta'], ['Field notes', '']]);
  for (const r of rows(v)) {
    assert.equal(r.open.children[0].tag, 'svg', 'an icon (generic: Codex sends none)');
    assert.equal(r.open.children[0].getAttribute('aria-hidden'), 'true');
    assert.ok(!/\/w\//.test(r.hint), 'no paths in the ordinary list');
  }
  assert.equal(row(v, 'Acme site', 'acme').open.title, '/w/acme\n/w/acme-docs', 'folders as an optional tooltip');
  assert.deepEqual(actions(v), [], 'listing runs no action');
});

test('details are keyboard-reachable buttons that reveal folders, source, host and id', async () => {
  const v = await initialized();
  const r = row(v, 'Acme site', 'acme');
  assert.equal(r.more.getAttribute('aria-label'), 'Details for Acme site, acme');
  assert.equal(r.more.getAttribute('aria-expanded'), 'false');
  const panel = r.li.children[1];
  assert.equal(panel.hidden, true);
  assert.equal(r.more.getAttribute('aria-controls'), panel.id);
  r.more.onclick();
  assert.equal(panel.hidden, false);
  assert.equal(r.more.getAttribute('aria-expanded'), 'true');
  assert.deepEqual(texts(panel), ['Folder 1', '/w/acme', 'Folder 2', '/w/acme-docs', 'Source', 'codex-app-server · this computer', 'Codex project id', 'prj_fixture_acme']);
  assert.deepEqual(actions(v), [], 'details run nothing');
  r.more.onclick();
  assert.equal(panel.hidden, true);
});

test('equal folder names fall back to a short id', async () => {
  const one = { ...BETA, id: 'prj_aaaaaa_111111', roots: [{ path: '/x/site' }] };
  const two = { ...BETA, id: 'prj_bbbbbb_222222', roots: [{ path: '/y/site' }] };
  const v = await initialized({ inventory: { ...LISTED, projects: [one, two] } });
  assert.deepEqual(rows(v).slice(1).map((r) => r.hint), ['…111111', '…222222']);
});

test('unavailable, empty and partial inventories say so; this chat\'s project stays usable', async () => {
  for (const [inventory, want] of [
    [{ available: false, reason: 'codex_not_found' }, /the codex command wasn't found/],
    [{ available: false, reason: 'unsupported' }, /doesn't share its project list/],
    [{ available: false, reason: 'protocol' }, /isn't available here\. You can still use this chat's project\./],
    [{ isError: true, content: [{ type: 'text', text: 'busy' }] }, /^Couldn't read your Codex projects\. Refresh/],
    [{ error: { code: -32601, message: 'no such tool at /private' } }, /^Couldn't read your Codex projects \(code -32601\)\./],
  ]) {
    const v = await initialized({ inventory });
    assert.match(v.els.inventory.textContent, want);
    assert.equal(v.els.inventory.hidden, false);
    assert.ok(!v.els.inventory.textContent.includes('/private'));
    assert.deepEqual(rows(v).map((r) => r.name), ["This chat's project"]);
  }
  const none = await initialized({ inventory: { ...LISTED, projects: [] } });
  assert.match(none.els.inventory.textContent, /^Codex has no saved projects on this computer yet\./);
  const partial = await initialized({ inventory: { ...LISTED, complete: false } });
  assert.equal(partial.els.inventory.textContent, 'Showing the first 3 Codex projects.');
});

test('an empty but partial inventory says more may exist, not that there are none', async () => {
  const v = await initialized({ inventory: { ...LISTED, projects: [], complete: false } });
  assert.match(v.els.inventory.textContent, /stopped early and returned none; more may exist/);
  assert.doesNotMatch(v.els.inventory.textContent, /no saved projects/);
});

// ---- direct actions: exact tool, arguments, order and the result shown ----

const CANVASES = {
  workspace: '/w/acme', design_present: true, complete: true,
  canvases: [
    { project: 'design/Alpha', path: '/w/acme/design/Alpha', bound: false, canvas: { id: 'c-alpha', name: 'Alpha', pages: 2, modified: 1 } },
    { project: 'design/Beta', path: '/w/acme/design/Beta', bound: true, canvas: { id: 'c-beta', name: 'Beta', pages: 1, modified: 2 } },
  ],
  skipped: [{ project: 'design/Escape', problem: 'a symbolic link (not followed)' }],
};
const EDITOR = (name, action, extra = {}) => ({ url: URL_OK, workspace: '/w/acme', project: 'design/' + name, project_path: '/w/acme/design/' + name, canvas: { id: 'c-' + name, name, pages: 1 }, ...(action ? { action } : {}), ...extra });
const fields = (v) => v.els['result-fields'].children.map((c) => c.textContent);
// What Details holds for a result, or null while Details is hidden.
const meta = (v) => (v.els['result-details'].hidden ? null : v.els['result-meta'].children.map((c) => c.textContent));

test('a one-folder project opens directly: one canvases action for exactly its folder, then its canvases', async () => {
  const v = await initialized();
  row(v, 'Acme site', 'beta').open.onclick();
  await flush();
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_canvases', arguments: { root: '/w/beta' } }]);
  assert.equal(v.els.notice.textContent, 'Opening Acme site · beta…');
  assert.ok(rows(v).every((r) => r.open.disabled) && v.els['menu-button'].disabled, 'busy while it runs');
  row(v, 'Field notes').open.onclick();
  await flush();
  assert.equal(actions(v).length, 1, 'nothing runs twice');
  await answer(v, { ...CANVASES, workspace: '/w/beta' });
  assert.equal(v.els['result-title'].textContent, 'Canvases');
  assert.equal(v.document.activeElement, v.els['result-title']);
  assert.deepEqual(fields(v), ['Folder', 'beta']);
  assert.deepEqual(rows(v, 'result-list').map((r) => [r.name, r.hint]), [['Alpha', '2 pages'], ['Beta', '1 page · current']]);
  assert.equal(v.els['result-list'].children[2].textContent, "Skipped 1 folder that isn't a usable canvas.");
  onlyAllowed(v);
});

test('several folders are asked, never guessed; the chosen folder is the action\'s exact root', async () => {
  const v = await initialized();
  row(v, 'Acme site', 'acme').open.onclick();
  await flush();
  assert.deepEqual(actions(v), [], 'ambiguity is asked first');
  assert.equal(v.els.pick.hidden, false);
  assert.equal(v.els['pick-title'].textContent, 'Which folder of Acme site · acme?');
  assert.equal(v.document.activeElement, v.els['pick-title']);
  const picks = rows(v, 'pick-list');
  assert.deepEqual(picks.map((p) => [p.name, p.hint]), [['acme', '/w/acme'], ['acme-docs', '/w/acme-docs']]);
  picks[1].open.onclick();
  await flush();
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_canvases', arguments: { root: '/w/acme-docs' } }]);
});

test('Escape or Back leaves the folder choice without running anything', async () => {
  const v = await initialized();
  row(v, 'Acme site', 'acme').open.onclick();
  v.els.pick.onkeydown({ key: 'Escape', preventDefault() {} });
  assert.equal(v.els.home.hidden, false);
  assert.equal(v.document.activeElement, v.els.title);
  row(v, 'Acme site', 'acme').open.onclick();
  v.els.back.onclick();
  assert.equal(v.els.home.hidden, false);
  assert.deepEqual(actions(v), []);
});

test('this chat\'s project asks for its own canvases with no folder argument', async () => {
  const v = await initialized();
  row(v, "This chat's project").open.onclick();
  await flush();
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_canvases', arguments: {} }]);
});

test('a project with no folder says so and runs nothing', async () => {
  const EMPTY = { ...NOTES, id: 'prj_fixture_empty', name: 'Empty', roots: [] };
  const v = await initialized({ inventory: { ...LISTED, projects: [EMPTY] } });
  row(v, 'Empty').open.onclick();
  assert.equal(v.els.notice.textContent, 'Empty has no folder in Codex.');
  assert.deepEqual(actions(v), []);
});

test('opening a listed canvas binds it directly, shows its returned identity and asks Codex to open its exact link', async () => {
  const v = await initialized();
  row(v, 'Field notes').open.onclick();
  await flush();
  await answer(v, CANVASES);
  rows(v, 'result-list')[0].open.onclick();
  await flush();
  assert.deepEqual(actions(v).at(-1), { name: 'openpaperdesign_ui_open', arguments: { root: '/w/acme', project: 'design/Alpha' } });
  await answer(v, EDITOR('Alpha', 'opened'));
  assert.equal(v.els['result-title'].textContent, 'Alpha');
  assert.equal(v.els['result-state'].textContent, 'Opening the canvas…');
  assert.deepEqual(fields(v), ['Canvas', 'Alpha (1 page)', 'Folder', 'acme']);
  assert.deepEqual(meta(v), ['Project', 'design/Alpha', 'Folder path', '/w/acme', 'Address', 'local, port 41234']);
  assert.ok(!JSON.stringify(texts(v.els.result)).includes(TOKEN), 'the token is never shown');
  assert.equal(v.count('ui/open-link'), 1, 'opened right away');
  assert.deepEqual(v.last('ui/open-link').params, { url: URL_OK });
  await ack(v);
  assert.match(v.els['canvas-state'].textContent, /doesn't confirm it loaded/, 'an acknowledgement is not a load');
  assert.equal(v.els.canvas.hidden, false, 'and can be opened again');
  v.els.canvas.onclick();
  await flush();
  assert.equal(v.count('ui/open-link'), 2);
  onlyAllowed(v);
});

test('New canvas creates directly in the exact chosen folder, then opens the created canvas', async () => {
  const lone = await initialized({ inventory: { ...LISTED, projects: [] } });
  lone.els['m-new'].onclick();
  assert.equal(lone.els['where-field'].hidden, true, 'no destination choice without projects');
  const v = await initialized();
  v.els['menu-button'].onclick();
  v.els['m-new'].onclick();
  assert.equal(v.els['new-form'].hidden, false);
  assert.equal(v.document.activeElement, v.els.name);
  assert.deepEqual(v.els.where.children.map((o) => [o.value, o.textContent]),
    [['', "This chat's project"], ['0', 'Acme site · acme · acme'], ['1', 'Acme site · acme · acme-docs'], ['2', 'Acme site · beta'], ['3', 'Field notes']]);
  v.els.name.value = '';
  v.els['new-form'].onsubmit({ preventDefault() {} });
  await flush();
  assert.equal(v.els.notice.textContent, 'Name the canvas first.');
  assert.deepEqual(actions(v), [], 'no name: nothing runs');
  v.els.name.value = '  Landing "page"\u0007 `x`  ';
  v.els.where.value = '1';
  v.els['new-form'].onsubmit({ preventDefault() {} });
  await flush();
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_create', arguments: { name: 'Landing page x', root: '/w/acme-docs' } }]);
  assert.equal(v.els.notice.textContent, 'Creating "Landing page x" in Acme site · acme · acme-docs…');
  await answer(v, EDITOR('Landing page x', 'created', { workspace: '/w/acme-docs', project: 'design/Landing page x' }));
  assert.equal(v.els['result-title'].textContent, 'Landing page x');
  assert.equal(v.els['result-state'].textContent, 'Created. Opening the canvas…');
  assert.deepEqual(fields(v), ['Canvas', 'Landing page x (1 page)', 'Folder', 'acme-docs']);
  assert.deepEqual(v.last('ui/open-link').params, { url: URL_OK });
  // This chat's project: no root.
  const w = await initialized();
  w.els['m-new'].onclick();
  w.els.name.value = 'Solo';
  w.els['new-form'].onsubmit({ preventDefault() {} });
  await flush();
  assert.deepEqual(actions(w), [{ name: 'openpaperdesign_ui_create', arguments: { name: 'Solo' } }]);
  onlyAllowed(v);
});

test('Open current canvas reopens directly; a delivered status offers the same for its own folder', async () => {
  const v = await initialized();
  v.els['m-current'].onclick();
  await flush();
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_editor', arguments: {} }]);
  await answer(v, EDITOR('Beta', 'reopened'));
  assert.equal(v.els['result-title'].textContent, 'Beta');
  assert.equal(v.count('ui/open-link'), 1);
  v.from(result({ workspace_state: 'pinned', workspace_usable: true, workspace: '/w/acme', workspace_permission_mode: 'managed-workspace-write', bound_project: '/w/acme/design/Landing' }));
  assert.equal(v.els['result-state'].textContent, 'A canvas is bound in this chat.');
  assert.deepEqual(fields(v), ['Canvas', 'Landing', 'Folder', 'acme']);
  assert.ok(!texts(v.els.result).some((t) => /managed-workspace-write/.test(t)), 'no permission profile');
  assert.equal(v.els.reopen.hidden, false);
  v.els.reopen.onclick();
  await flush();
  assert.deepEqual(actions(v).at(-1), { name: 'openpaperdesign_ui_editor', arguments: { root: '/w/acme' } });
  v.from(result({ workspace_state: 'pinned', workspace_usable: true, workspace: '/w/acme', bound_project: null }));
  assert.equal(v.els.reopen.hidden, true);
  assert.equal(v.els['result-state'].textContent, 'No canvas is bound in this chat yet.');
});

// ---- refusals, failures and late replies never look like success ----

test('a refusal without workspace context is plain, keeps the reason under Details and opens nothing', async () => {
  const v = await initialized();
  row(v, 'Acme site', 'beta').open.onclick();
  await flush();
  await answer(v, { refused: NO_CONTEXT });
  assert.equal(v.els['result-title'].textContent, "Can't open project files here");
  assert.equal(v.els['result-state'].className, 'bad');
  assert.match(v.els['result-state'].textContent, /nothing was opened or changed\. OpenPaperDesign can't open project files from here yet\.$/);
  assert.doesNotMatch(v.els['result-state'].textContent, /chat|agent|paste/i, 'no unverified recovery route is promised');
  assert.equal(v.els['result-detail'].textContent, NO_CONTEXT);
  assert.equal(v.els['result-details'].hidden, false);
  assert.equal(v.els.canvas.hidden, true);
  assert.equal(v.count('ui/open-link'), 0);
  assert.ok(!v.els['menu-button'].disabled, 'usable again');
  onlyAllowed(v);
});

test('a switch that needs confirmation asks first, then repeats the same action with force', async () => {
  const v = await initialized();
  v.els['m-new'].onclick();
  v.els.name.value = 'Second';
  v.els.where.value = '2';
  v.els['new-form'].onsubmit({ preventDefault() {} });
  await flush();
  await answer(v, { refused: 'The browser editor for the bound project was opened (open_editor). … then call again with force: true.' });
  assert.equal(v.els['result-title'].textContent, 'Switch canvases?');
  assert.equal(v.els.force.hidden, false);
  assert.equal(v.count('ui/open-link'), 0);
  v.els.force.onclick();
  await flush();
  assert.deepEqual(actions(v), [
    { name: 'openpaperdesign_ui_create', arguments: { name: 'Second', root: '/w/beta' } },
    { name: 'openpaperdesign_ui_create', arguments: { name: 'Second', root: '/w/beta', force: true } },
  ]);
  await answer(v, EDITOR('Second', 'created'));
  assert.equal(v.els['result-title'].textContent, 'Second');
  assert.equal(v.els.force.hidden, true);
});

test('errors, unreadable answers and invalid links never show success', async () => {
  for (const [reply, title, state] of [
    [{ error: { code: -32601, message: 'no' } }, 'Not done', /didn't run the request \(code -32601\)/],
    [{ refused: 'design/Landing already exists; bind it with bind_project' }, 'Not done', /already exists/],
  ]) {
    const v = await initialized();
    v.els['m-current'].onclick();
    await flush();
    await answer(v, reply);
    assert.equal(v.els['result-title'].textContent, title);
    assert.match(v.els['result-state'].textContent, state);
    assert.equal(v.els.canvas.hidden, true);
  }
  // An answer that isn't JSON, or isn't the expected shape.
  for (const text of ['not json', '"a string"']) {
    const v = await initialized();
    v.els['m-current'].onclick();
    await flush();
    v.from({ id: lastAction(v).id, result: { content: [{ type: 'text', text }] } });
    await flush();
    assert.equal(v.els['result-title'].textContent, 'Not done', text);
    assert.equal(v.els.canvas.hidden, true);
  }
  const shape = await initialized();
  row(shape, 'Field notes').open.onclick();
  await flush();
  await answer(shape, { workspace: '/w/notes' });
  assert.equal(shape.els['result-title'].textContent, 'Not done', 'canvases without a list');
  // An editor answer whose link isn't the exact loopback form.
  for (const bad of ['https://127.0.0.1:1/#token=' + TOKEN, 'http://127.0.0.1:1/x#token=' + TOKEN,
    'http://localhost:1/#token=' + TOKEN, 'http://127.0.0.1:1/#token=short', URL_OK + '&x=1', 'javascript:alert(1)']) {
    const v = await initialized();
    v.els['m-current'].onclick();
    await flush();
    await answer(v, { ...EDITOR('Alpha', 'reopened'), url: bad });
    assert.equal(v.els.canvas.hidden, true, bad);
    assert.match(v.els['result-state'].textContent, /won't be opened/);
    assert.equal(v.count('ui/open-link'), 0, bad);
  }
});

test('a late or reordered action reply never changes a newer view', async () => {
  // Back before the reply.
  const v = await initialized();
  row(v, 'Field notes').open.onclick();
  await flush();
  const call = lastAction(v);
  v.els.back.onclick();
  await answer(v, CANVASES, call);
  assert.equal(v.els.home.hidden, false, 'still home');
  assert.equal(v.els.result.hidden, true);
  assert.equal(v.els.live.textContent, 'Projects');
  // A delivered result before the reply.
  const w = await initialized();
  w.els['m-current'].onclick();
  await flush();
  const pending = lastAction(w);
  w.from(result(CANVASES));
  assert.ok(!w.els['menu-button'].disabled, 'the new view is not busy');
  await answer(w, EDITOR('Alpha', 'reopened'), pending);
  assert.equal(w.els['result-title'].textContent, 'Canvases', 'the late editor is not shown');
  assert.equal(w.count('ui/open-link'), 0, 'and its link is not opened');
});

test('a pending open of Alpha neither blocks Beta nor reports under it', async () => {
  const v = await initialized();
  v.from(result(EDITOR('Alpha')));
  v.els.canvas.onclick();
  await flush();
  const alpha = v.last('ui/open-link');
  v.from(result(EDITOR('Beta')));
  assert.equal(v.els.canvas.disabled, false, 'Beta\'s button is usable');
  v.from({ id: alpha.id, error: { code: -32000, message: 'x' } });
  await flush();
  assert.equal(v.els['canvas-state'].textContent, '', 'Alpha\'s failure is not shown under Beta');
  assert.equal(v.els['result-title'].textContent, 'Beta');
});

test('late open replies after cancellation or Back change nothing', async () => {
  const v = await initialized();
  v.from(result(EDITOR('Alpha')));
  v.els.canvas.onclick();
  await flush();
  v.from({ method: 'ui/notifications/tool-cancelled', params: { reason: 'user' } });
  await ack(v);
  assert.equal(v.els['canvas-state'].textContent, '');
  assert.equal(v.els.live.textContent, 'Cancelled');
  v.from(result(EDITOR('Alpha')));
  v.els.canvas.onclick();
  await flush();
  v.els.back.onclick();
  await ack(v);
  assert.equal(v.els.live.textContent, 'Projects');
  v.els.canvas.onclick();
  await flush();
  assert.equal(v.count('ui/open-link'), 2, 'no link after Back');
});

test('a late inventory reply after a result renders quietly and announces nothing', async () => {
  const v = await initialized({ inventory: null });
  const call = v.last('tools/call');
  v.from(result({ workspace_state: 'pinned', workspace_usable: true, workspace: '/w/acme', bound_project: null }));
  const announced = v.els.live.textContent;
  v.from({ id: call.id, result: toolText(LISTED) });
  await flush();
  assert.equal(v.els.live.textContent, announced);
  v.els.back.onclick();
  assert.equal(rows(v).length, 4, 'the list is there when you go back');
});

test('a delivered home result revokes an earlier editor link', async () => {
  const v = await initialized();
  v.from(result(EDITOR('Alpha')));
  v.from(result({ surface: 'home', version: '0.1.0' }));
  assert.equal(v.els.home.hidden, false);
  v.els.canvas.onclick();
  v.els.reopen.onclick();
  await flush();
  assert.equal(v.count('ui/open-link'), 0);
  assert.deepEqual(actions(v), []);
});

test('a delivered editor result is shown but not opened until the person asks', async () => {
  const v = await initialized();
  v.from(result(EDITOR('Beta')));
  assert.equal(v.els['result-title'].textContent, 'Beta');
  assert.equal(v.count('ui/open-link'), 0);
  v.els.canvas.onclick();
  assert.ok(v.els.canvas.disabled);
  await flush();
  assert.deepEqual(v.last('ui/open-link').params, { url: URL_OK });
});

// ---- the folder picker ----

test('Open a folder uses the host picker; a chosen folder is only the next action\'s exact root', async () => {
  const v = await initialized();
  v.els['m-folder'].onclick();
  await flush();
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_choose_folder', arguments: {} }]);
  await answer(v, { status: 'selected', folder: { path: '/w/picked', name: 'picked' }, access: 'none' });
  assert.deepEqual(actions(v).at(-1), { name: 'openpaperdesign_ui_canvases', arguments: { root: '/w/picked' } });
  assert.equal(v.els.notice.textContent, 'Opening picked…');
  await answer(v, { refused: NO_CONTEXT });
  assert.equal(v.els['result-title'].textContent, "Can't open project files here", 'a pick alone grants nothing');
  onlyAllowed(v);
});

test('picker answers other than a folder run nothing further', async () => {
  for (const [reply, want] of [
    [{ status: 'declined' }, 'No folder was chosen.'],
    [{ status: 'cancelled' }, 'No folder was chosen.'],
    [{ status: 'timeout' }, 'The folder picker closed without a choice.'],
    [{ status: 'opaque' }, "Codex returned a folder OpenPaperDesign can't use here. Nothing was opened."],
    [{ status: 'unavailable', code: -32601 }, "Codex couldn't show its folder picker (code -32601)."],
    [{ status: 'selected', folder: { name: 'no path' } }, 'No folder was chosen.'],
  ]) {
    const v = await initialized();
    v.els['m-folder'].onclick();
    await flush();
    await answer(v, reply);
    assert.equal(v.els.notice.textContent, want, JSON.stringify(reply));
    assert.equal(actions(v).length, 1, JSON.stringify(reply));
  }
  const v = await initialized();
  v.els['m-folder'].onclick();
  await flush();
  await answer(v, { status: 'unsupported', reason: 'x' });
  assert.equal(v.els['folder-help'].hidden, false, 'without a picker: the add-in-Codex hint');
  assert.equal(v.document.activeElement, v.els['folder-title']);
  v.els['folder-refresh'].onclick();
  await flush();
  assert.equal(v.els.home.hidden, false);
  assert.deepEqual(v.last('tools/call').params, { name: INVENTORY, arguments: {} });
});

// ---- the New menu ----

test('the New menu opens on its labelled button, moves with arrows and closes on Escape with focus back', async () => {
  const v = await initialized();
  const button = v.els['menu-button'];
  assert.match(html, /id="menu-button" class="btn" type="button" aria-haspopup="menu" aria-expanded="false" aria-controls="menu"/);
  button.onclick();
  assert.equal(v.els.menu.hidden, false);
  assert.equal(button.getAttribute('aria-expanded'), 'true');
  assert.equal(v.document.activeElement, v.els['m-new']);
  const key = (k) => v.els.menu.onkeydown({ key: k, preventDefault() {} });
  key('ArrowDown');
  assert.equal(v.document.activeElement, v.els['m-current']);
  key('End');
  assert.equal(v.document.activeElement, v.els['m-refresh']);
  key('ArrowDown');
  assert.equal(v.document.activeElement, v.els['m-new'], 'wraps around');
  key('ArrowUp');
  assert.equal(v.document.activeElement, v.els['m-refresh']);
  key('Home');
  assert.equal(v.document.activeElement, v.els['m-new']);
  key('Escape');
  assert.equal(v.els.menu.hidden, true);
  assert.equal(button.getAttribute('aria-expanded'), 'false');
  assert.equal(v.document.activeElement, button);
  button.onclick();
  key('Tab');
  assert.equal(v.els.menu.hidden, true, 'Tab closes it too');
  assert.deepEqual(actions(v), [], 'the menu itself runs nothing');
});

test('an older inventory reply never replaces a newer one', async () => {
  const v = await initialized({ inventory: null });
  const first = v.last('tools/call');
  v.els['m-refresh'].onclick();
  await flush();
  const second = v.last('tools/call');
  v.from({ id: second.id, result: toolText({ ...LISTED, projects: [NOTES] }) });
  await flush();
  v.from({ id: first.id, result: toolText(LISTED) });
  await flush();
  assert.deepEqual(rows(v).map((r) => r.name), ["This chat's project", 'Field notes']);
});

// ---- host presentation ----

test('host theme and standard style variables apply as presentation only, with partial updates merged', async () => {
  const v = await initialized({ context: { theme: 'dark', styles: { variables: {
    '--color-background-primary': '#101010', '--color-text-primary': 'rgb(240 240 240)', '--font-sans': 'Inter, system-ui',
    '--color-text-secondary': 'url(https://example.invalid/x)', '--color-border-primary': 'red; background: url(x)',
    '--made-up': '#000', '--color-ring-primary': 'x'.repeat(201),
  } } } });
  assert.equal(v.rootAttrs['data-theme'], 'dark');
  assert.deepEqual(v.props, { '--color-background-primary': '#101010', '--color-text-primary': 'rgb(240 240 240)', '--font-sans': 'Inter, system-ui' });
  v.from({ method: 'ui/notifications/host-context-changed', params: { theme: 'light' } });
  assert.equal(v.rootAttrs['data-theme'], 'light');
  assert.equal(v.props['--color-background-primary'], '#101010', 'partial update keeps earlier styles');
  const before = { ...v.props };
  v.from({ method: 'ui/notifications/host-context-changed', params: { styles: { css: { fonts: '@import url(https://fonts.invalid/x.css);' } } } });
  assert.deepEqual(v.props, before, 'font-loading CSS is never applied');
  v.from({ method: 'ui/notifications/host-context-changed', params: { theme: 'dark' } }, { postMessage() {} });
  assert.equal(v.rootAttrs['data-theme'], 'light', 'another window is ignored');
  assert.equal(v.count('tools/call'), 1, 'host context grants nothing and triggers no call');
});

// ---- delivered model results ----

test('a delivered canvases result lists canvases; bound means current; opening one is the direct action', async () => {
  const v = await initialized();
  v.from(result(CANVASES));
  assert.equal(v.els['result-title'].textContent, 'Canvases');
  assert.deepEqual(fields(v), ['Folder', 'acme'], 'concise identity in view');
  assert.deepEqual(meta(v), ['Folder path', '/w/acme'], 'the exact folder under Details');
  assert.ok(!texts(v.els.result).some((t) => /open now/.test(t)), 'binding is not a loaded editor');
  rows(v, 'result-list')[1].open.onclick();
  await flush();
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_open', arguments: { root: '/w/acme', project: 'design/Beta' } }]);
});

test('empty and cut-off canvas lists say which they are', async () => {
  const v = await initialized();
  v.from(result({ ...CANVASES, canvases: [], skipped: [] }));
  assert.match(v.els['result-state'].textContent, /^No canvases under design\/ in this project yet\./);
  v.from(result({ ...CANVASES, canvases: [], skipped: [], complete: false }));
  assert.match(v.els['result-state'].textContent, /stopped early before finding any canvases in design\/; more may exist/);
  v.from(result({ ...CANVASES, complete: false }));
  assert.equal(v.els['result-state'].textContent, 'The first 2 canvases in this project:');
});

test('a refused or retired status shows plain recovery and no folder', async () => {
  const v = await initialized();
  v.from(result({ workspace_state: 'retired', workspace_usable: false, workspace: '/w/old', workspace_problem: 'Codex workspace changed', bound_project: null }));
  assert.equal(v.els['result-state'].className, 'bad');
  assert.match(v.els['result-state'].textContent, /^This chat can't use a project folder\./);
  assert.deepEqual(fields(v), []);
  assert.equal(v.els['result-detail'].textContent, 'Codex workspace changed');
  assert.ok(!texts(v.els.result).includes('/w/old'));
});

test('a failed step reports only that step; it never claims earlier work was undone', async () => {
  const v = await initialized();
  v.from(result('The browser editor is not built in.\nmore', true));
  const text = v.els['result-state'].textContent;
  assert.doesNotMatch(text, /Nothing was changed|nothing changed|rolled back|undone/i);
  assert.match(text, /last step didn't work\. Anything it finished before that may already have happened; check the chat/);
  assert.equal(v.els['result-detail'].textContent, 'The browser editor is not built in.');
});

test('messages from other windows are ignored', async () => {
  const v = mount();
  v.from({ id: v.sent[0].id, result: { hostCapabilities: {} } }, { postMessage() {} });
  await flush();
  assert.deepEqual(methods(v), ['ui/initialize']);
  v.from(result({ url: URL_OK }), { postMessage() {} });
  assert.equal(v.els.canvas.hidden, true);
});

// ---- R2 protections, kept ----

const submitNew = async (v) => { v.els['new-form'].onsubmit({ preventDefault() {} }); await flush(); };
const options = (v) => v.els.where.children.map((o) => o.textContent);

test('a refresh while New canvas is open never changes what a shown destination sends (R2-F1)', async () => {
  const OTHER = { ...BETA, roots: [{ path: '/w/other' }] };
  for (const [kind, next, nextOptions] of [
    ['reorder', [NOTES, BETA, ACME], ["This chat's project", 'Field notes', 'Acme site · beta', 'Acme site · acme · acme', 'Acme site · acme · acme-docs']],
    ['removal', [ACME], ["This chat's project", 'Acme site · acme', 'Acme site · acme-docs']],
    ['root replacement', [OTHER, ACME, NOTES], ["This chat's project", 'Acme site · other', 'Acme site · acme · acme', 'Acme site · acme · acme-docs', 'Field notes']],
  ]) {
    const v = await initialized();
    v.els['m-refresh'].onclick();
    await flush();
    const pending = v.last('tools/call');
    v.els['m-new'].onclick();
    v.els.where.value = '2';
    v.els.name.value = 'Draft';
    const shown = v.els.where.children.find((o) => o.value === '2').textContent;
    assert.equal(shown, 'Acme site · beta', kind);
    v.from({ id: pending.id, result: toolText({ ...LISTED, projects: next }) });
    await flush();
    assert.equal(v.els['new-form'].hidden, false, 'the open form stays as it was');
    await submitNew(v);
    assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_create', arguments: { name: 'Draft', root: '/w/beta' } }], kind);
    await answer(v, EDITOR('Draft', 'created'));
    v.els['m-new'].onclick();
    assert.deepEqual(options(v), nextOptions, 'the next form uses the refreshed list');
  }
});

test('a late inventory reply leaves the visible canvas rows to their own action (R2-F2)', async () => {
  for (const [kind, reply, title] of [
    ['succeeded', EDITOR('Alpha', 'opened'), 'Alpha'],
    ['refused', { refused: NO_CONTEXT }, "Can't open project files here"],
    ['failed', { error: { code: -32601, message: 'no' } }, 'Not done'],
  ]) {
    const v = await initialized({ inventory: null });
    const inventory = v.last('tools/call');
    v.from(result(CANVASES));
    const list = rows(v, 'result-list');
    list[0].open.onclick();
    await flush();
    assert.ok(list.every((r) => r.open.disabled) && v.els['menu-button'].disabled, kind);
    v.from({ id: inventory.id, result: toolText(LISTED) });
    await flush();
    assert.equal(v.els.result.hidden, false, 'the canvases stay in view');
    assert.ok(rows(v).every((r) => r.open.disabled), 'the hidden home\'s new rows wait for the same action');
    assert.ok(list.every((r) => r.open.disabled), 'the visible rows are still owned by the pending action');
    assert.ok(list.every((r) => !r.more.disabled), 'details only show, so they stay usable');
    await answer(v, reply);
    assert.equal(v.els['result-title'].textContent, title, kind);
    assert.ok(!v.els['menu-button'].disabled, kind);
    assert.ok(rows(v).every((r) => !r.open.disabled), kind);
    assert.equal(actions(v).length, 1, 'nothing was resent');
  }
});

test('destinations with equal folder names show the shortest distinguishing path (R2-F4)', async () => {
  const SITE = { ...BETA, id: 'prj_fixture_site', name: 'Site', roots: [{ path: '/work/current/site' }, { path: '/archive/site' }] };
  const v = await initialized({ inventory: { ...LISTED, projects: [SITE] } });
  assert.deepEqual(rows(v).map((r) => [r.name, r.hint]), [["This chat's project", ''], ['Site', '2 folders']], 'the list stays quiet');
  v.els['m-new'].onclick();
  assert.deepEqual(v.els.where.children.slice(1).map((o) => [o.textContent, o.title]),
    [['Site · current/site', '/work/current/site'], ['Site · archive/site', '/archive/site']]);
  v.els.where.value = '1';
  v.els.name.value = 'Draft';
  await submitNew(v);
  assert.deepEqual(actions(v), [{ name: 'openpaperdesign_ui_create', arguments: { name: 'Draft', root: '/archive/site' } }]);
  v.els.back.onclick();
  row(v, 'Site', '2 folders').open.onclick();
  assert.deepEqual(rows(v, 'pick-list').map((r) => [r.name, r.hint]),
    [['current/site', '/work/current/site'], ['archive/site', '/archive/site']], 'the folder question matches');
});

test('results keep exact paths and the port under Details, cleared for each new result (R2-F5)', async () => {
  const v = await initialized();
  const inView = () => texts(v.els['result-fields']).concat(texts(v.els['result-state']), texts(v.els['result-list'])).join('\n');
  for (const r of [{ workspace_state: 'pinned', workspace_usable: true, workspace: '/w/acme', bound_project: '/w/acme/design/Alpha' }, CANVASES, EDITOR('Alpha')]) {
    v.from(result(r));
    assert.doesNotMatch(inView(), /\/w\/|41234/, 'no full folder or port in the normal view');
    assert.equal(v.els['result-details'].hidden, false, 'Details is there to check them');
    assert.equal(v.els['result-details'].open, false, 'and starts closed');
    assert.ok(meta(v).includes('/w/acme'));
    v.els['result-details'].open = true;
  }
  assert.ok(!JSON.stringify(meta(v)).includes(TOKEN));
  v.from(result('The browser editor is not built in.', true));
  assert.deepEqual(meta(v), [], 'no earlier result\'s metadata');
  assert.equal(v.els['result-meta'].hidden, true);
  assert.equal(v.els['result-detail'].hidden, false);
  assert.equal(v.els['result-details'].open, false);
  v.from(result({ workspace_state: 'pinned', workspace_usable: true, workspace: '/w/acme', bound_project: null }));
  assert.equal(v.els['result-detail'].textContent, '', 'no earlier refusal reason');
  assert.equal(v.els['result-detail'].hidden, true);
  assert.deepEqual(meta(v), ['Folder path', '/w/acme']);
  v.from({ method: 'ui/notifications/tool-cancelled', params: {} });
  assert.equal(meta(v), null, 'nothing to show, so no Details');
});

// ---- static ----

test('the view sends no chat message, calls only its own tools, stores nothing and embeds its one font locally', () => {
  assert.ok(!script.includes('ui/message'), 'no message to the agent');
  assert.ok(!/id="prompt"|Paste this/.test(html), 'no paste prompt');
  // Tool names at call sites: act('…', …) and the inventory's tools/call.
  const named = new Set([...script.matchAll(/(?:act\(|name: )'(openpaperdesign_[a-z_]+)'/g)].map((m) => m[1]));
  assert.deepEqual([...named].sort(), [...TOOLS, ...LIB_TOOLS].sort(), 'exactly these server tools');
  for (const banned of ['resources/read', 'localStorage', 'sessionStorage', 'indexedDB', 'document.cookie', 'fetch(', 'XMLHttpRequest', 'innerHTML',
    'openpaperdesign_status\'', 'bind_project\'', 'create_project\'', 'open_editor\'', '@import']) {
    assert.ok(!html.includes(banned), banned);
  }
  const style = html.slice(0, html.indexOf('</style>'));
  assert.deepEqual(style.match(/url\([^)]*\)/g), ['url("data:font/woff2;base64,__OPD_WORDMARK_FONT__")'], 'the only url() is the served font slot');
  assert.ok(!/https?:\/\/(?!127\.0\.0\.1)/.test(style), 'no external stylesheet or font');
  assert.match(style, /\.wordmark \{ font: 400 16px\/1\.2 "Geist Pixel", monospace;/);
  assert.match(html, /<footer><span class="wordmark">onlinesourdough<\/span><\/footer>/);
  assert.match(html, /aria-label="Back to projects"/);
  assert.equal(html.split('__OPD_MODE__').length, 2, 'exactly one mode slot');
});

// ---- served by an app-library server (--app-library) ----

const ID1 = '0123456789abcdef';
const ID2 = 'fedcba9876543210';
const LIBRARY_LIST = { storage: 'app-library', complete: true, skipped: [], canvases: [
  { id: ID1, name: 'Landing', pages: 2, modified: 2, open: false },
  { id: ID2, name: 'Notes', pages: 1, modified: 1, open: true },
  { id: '../escape', name: 'Not an id', pages: 1 }] };
// Each editor result names its binding; the view sends it back with close, export and polls.
const B1 = `${ID1}.1`;
const B2 = `${ID2}.2`;
const LIB_EDITOR = (action, id = ID1, extra = {}) => ({ action, storage: 'app-library', url: URL_OK, browser_editor: 'built in',
  binding: id === ID1 ? B1 : B2, canvas: { id, name: id === ID1 ? 'Landing' : 'Notes', pages: 1 },
  location: `/data/library/v1/canvases/${id}`, editor_loaded: false, browser_saves: 0, ...extra });
const SWITCH_REFUSAL = (shown) => 'The browser editor for the bound project was opened ... then call again with force: true.'
  + ` (open canvas: ${shown})`;

// A library view as a complete build serves it: with the in-panel editor (a recording stand-in,
// `fakePanel`, as `v.fake`). `{ panel: false }`: a build without the editor module.
async function libraryView(list = LIBRARY_LIST, { panel = true, clock = false } = {}) {
  const fake = panel ? fakePanel() : null;
  const v = mount({ library: true, panel: fake, clock });
  v.fake = fake;
  v.from({ id: v.sent[0].id, result: { protocolVersion: '2026-01-26', hostCapabilities: {}, hostContext: {} } });
  await flush();
  const call = v.last('tools/call');
  assert.deepEqual(call.params, { name: LIB_LIST, arguments: {} }, 'the library list, not the Codex inventory');
  v.from(list.error ? { id: call.id, error: list.error } : { id: call.id, result: toolText(list) });
  await flush();
  return v;
}
const libOnly = (v) => {
  assert.ok(methods(v).every((m) => ALLOWED.has(m)), methods(v).join());
  for (const m of v.sent.filter((m) => m.method === 'tools/call')) assert.ok(LIB_TOOLS.has(m.params.name), m.params.name);
  assert.equal(v.count('ui/message'), 0, 'no message to the agent, ever');
};
// The latest library action (every tools/call except the list).
const libAction = (v) => [...v.sent].reverse().find((m) => m.method === 'tools/call' && m.params.name !== LIB_LIST);
const libCalls = (v, name) => v.sent.filter((m) => m.method === 'tools/call' && m.params.name === name).length;

test('library: the app\'s canvases are listed; Open hands the canvas to its one web editor link, with no message', async () => {
  assert.notEqual(libScript, script, 'the mode slot is filled');
  const v = await libraryView();
  assert.deepEqual(rows(v).map((r) => [r.name, r.hint]), [['Landing', '2 pages'], ['Notes', '1 page · open here']],
    'server ids only; no "This chat\'s project" row');
  assert.equal(v.els['m-folder'].hidden, true, 'no folder picker');
  assert.equal(v.els['m-import'].hidden, false);
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_open', arguments: { id: ID1 } },
    'the primary action asks for nothing but the canvas');
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  assert.equal(v.count('ui/open-link'), 1, 'exactly one opening request');
  assert.equal(v.last('ui/open-link').params.url, URL_OK, 'the result\'s own same-binding loopback link');
  assert.equal(v.els['result-state'].textContent, 'Opening the editor…');
  assert.ok(!v.els.close.hidden && !v.els.export.hidden && !v.els.canvas.hidden);
  assert.equal(v.els['edit-here'].hidden, false, 'the optional in-panel editor is offered (this build has it)');
  assert.equal(v.fake.handles.length, 0, 'but never opened unasked');
  const shown = ['result-fields', 'result-meta', 'result-state', 'notice', 'live'].flatMap((id) => texts(v.els[id])).join('\n');
  assert.ok(shown.includes(ID1) && !shown.includes(TOKEN), 'the canvas id under Details, never the token');
  await ack(v);
  assert.match(v.els['canvas-state'].textContent, /doesn't confirm it loaded/, 'an acknowledgement is not a load');
  assert.equal(libCalls(v, 'openpaperdesign_app_editor'), 0, 'no load is claimed without the editor\'s own report');
  libOnly(v);
});

test('library: an answer for another canvas, a link that isn\'t loopback, or a reply after Back opens nothing', async () => {
  const v = await libraryView();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened', ID2), libAction(v));
  assert.equal(v.count('ui/open-link'), 0);
  assert.match(v.els['result-state'].textContent, /another canvas/);
  v.els.back.onclick();
  await flush();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened', ID1, { url: 'https://example.com/#token=' + TOKEN }), libAction(v));
  assert.equal(v.count('ui/open-link'), 0);
  assert.match(v.els['result-state'].textContent, /isn't a local OpenPaperDesign link/);
  v.els.back.onclick();
  await flush();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  const pending = libAction(v);
  v.els.back.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), pending);
  assert.equal(v.count('ui/open-link'), 0, 'a late reply belongs to the view it started in');
  // A delivered result is shown, never opened by itself.
  v.from(result(LIB_EDITOR('opened')));
  assert.equal(v.count('ui/open-link'), 0);
  assert.equal(v.fake.handles.length, 0);
  libOnly(v);
});

test('library: New creates directly with a name only; an unanswered create is never retried', async () => {
  const v = await libraryView();
  v.els['m-new'].onclick();
  assert.equal(v.els['where-field'].hidden, true, 'library canvases have no destination choice');
  v.els.name.value = 'Pitch';
  await submitNew(v);
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_create', arguments: { name: 'Pitch' } });
  await answer(v, LIB_EDITOR('created'), libAction(v));
  assert.equal(v.els['result-state'].textContent, 'Created and saved in your library. Opening the editor…');
  assert.equal(v.count('ui/open-link'), 1);
  assert.equal(v.last('ui/open-link').params.url, URL_OK);
  const w = await libraryView();
  w.els['m-new'].onclick();
  w.els.name.value = 'Pitch';
  await submitNew(w);
  await answer(w, { error: { code: -32001, message: 'lost' } }, libAction(w));
  assert.equal(w.els['result-title'].textContent, 'Not sure it was created');
  assert.match(w.els['result-state'].textContent, /check your canvases before trying again/);
  assert.equal(libCalls(w, 'openpaperdesign_app_create'), 1, 'not retried');
  assert.equal(w.last('tools/call').params.name, LIB_LIST, 'the list is read again instead');
  libOnly(v);
  libOnly(w);
});

test('library: Close names this view\'s binding, asks the person first when the server does, and claims no save', async () => {
  const v = await libraryView();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els.close.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_close', arguments: { binding: B1 } },
    'close names the binding this view showed');
  await answer(v, { refused: 'The editor of this canvas was opened. ... then close again with force: true.' }, libAction(v));
  assert.equal(v.els['result-title'].textContent, 'Close the canvas?');
  assert.equal(v.els.force.textContent, 'Close anyway');
  v.els.force.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_close', arguments: { binding: B1, force: true } },
    'the confirmation is for the same binding');
  await answer(v, { action: 'closed', canvas: { id: ID1 } }, libAction(v));
  assert.equal(v.els['result-title'].textContent, 'Canvas closed');
  assert.match(v.els['result-state'].textContent, /not saved by closing/);
  assert.equal(v.last('tools/call').params.name, LIB_LIST);
  libOnly(v);
});

test('library: export and import move only bytes, and say plainly what this view could not do', async () => {
  const v = await libraryView();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els.export.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_export', arguments: { binding: B1 } });
  await answer(v, { action: 'exported', binding: B1, canvas: { id: ID1, name: 'Landing' }, content: 'canvas-only',
    file_name: 'Landing.opdcanvas.json', bytes: 38, payload: '{"format":"openpaperdesign-canvas"}' }, libAction(v));
  assert.match(v.els.notice.textContent, /can't download files here, so no file was saved/, 'no claimed download');
  v.els['m-import'].onclick();
  assert.match(v.els.notice.textContent, /can't open files here\. Nothing was imported/);
  let picked = 0;
  v.els['import-file'].click = () => { picked += 1; };
  v.els['m-import'].onclick();
  assert.equal(picked, 1);
  v.els['import-file'].files = [{ size: 33 * 1024 * 1024, name: 'big.json', text: async () => 'x' }];
  await v.els['import-file'].onchange();
  assert.match(v.els.notice.textContent, /larger than 32 MB/);
  v.els['import-file'].files = [{ size: 9, name: '/Users/someone/secret.json', text: async () => 'FILE-TEXT' }];
  await v.els['import-file'].onchange();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_import', arguments: { payload: 'FILE-TEXT' } },
    'only the text, never the file name or a path');
  await answer(v, LIB_EDITOR('imported', ID2), libAction(v));
  assert.equal(v.els['result-state'].textContent, 'Imported as a new canvas in your library. Opening the editor…');
  assert.equal(v.count('ui/open-link'), 2, 'one opening request per opened canvas');
  libOnly(v);
});

test('library: without a built editor nothing is opened, and the canvas is still reported saved', async () => {
  const v = await libraryView();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened', ID1, { browser_editor: 'not built' }), libAction(v));
  assert.equal(v.count('ui/open-link'), 0);
  assert.equal(v.fake.handles.length, 0);
  assert.equal(v.els.canvas.hidden, true);
  assert.equal(v.els['edit-here'].hidden, true);
  assert.match(v.els['canvas-state'].textContent, /built without the browser editor/);
});

test('library: a switch confirmation names exactly the binding the refusal showed', async () => {
  const v = await libraryView();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_open', arguments: { id: ID1 } }, 'no force at first');
  await answer(v, { refused: SWITCH_REFUSAL(B2) }, libAction(v));
  assert.equal(v.els['result-title'].textContent, 'Switch canvases?');
  assert.equal(v.els.force.hidden, false);
  v.els.force.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_open', arguments: { id: ID1, force: true, replace: B2 } });
  // The server finds another view changed the open canvas meanwhile: refused, shown as such.
  await answer(v, { refused: 'The canvas this view shows isn\'t the one open in this OpenPaperDesign window any more: another view changed it. Nothing was done, and the open canvas is unchanged.' }, libAction(v));
  assert.equal(v.count('ui/open-link'), 0);
  assert.match(v.els['result-state'].textContent, /another view changed it/);
  // New canvas the same way.
  v.els.back.onclick();
  await flush();
  v.els['m-new'].onclick();
  v.els.name.value = 'Pitch';
  await submitNew(v);
  await answer(v, { refused: SWITCH_REFUSAL(B1) }, libAction(v));
  v.els.force.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_create', arguments: { name: 'Pitch', force: true, replace: B1 } });
  libOnly(v);
});

test('library: an export answer for another binding is never offered as a download', async () => {
  const v = await libraryView();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els.export.onclick();
  await flush();
  await answer(v, { action: 'exported', binding: B2, canvas: { id: ID2, name: 'Notes' }, content: 'canvas-only',
    file_name: 'Notes.opdcanvas.json', bytes: 38, payload: '{"format":"openpaperdesign-canvas"}' }, libAction(v));
  assert.match(v.els['result-state'].textContent, /another canvas than the one shown here, so nothing was downloaded/);
  assert.doesNotMatch(v.els.notice.textContent, /download/);
  libOnly(v);
});

test('library: a file read that outlives its view or its selection sends nothing and says nothing', async () => {
  const v = await libraryView();
  const deferred = () => {
    let resolve;
    let reject;
    const p = new Promise((res, rej) => { resolve = res; reject = rej; });
    return { p, resolve, reject };
  };
  const choose = (read) => {
    v.els['import-file'].files = [{ size: 9, name: 'canvas.json', text: () => read }];
    return v.els['import-file'].onchange();
  };
  const imports = () => libCalls(v, 'openpaperdesign_app_import');
  // Back while the read is pending: the late text is dropped.
  const a = deferred();
  const readA = choose(a.p);
  v.els.back.onclick();
  await flush();
  a.resolve('LATE-TEXT');
  await readA;
  await flush();
  assert.equal(imports(), 0, 'no import after Back');
  // A late read error after Back leaves the newer view's notice alone.
  const b = deferred();
  const readB = choose(b.p);
  v.els.back.onclick();
  await flush();
  b.reject(new Error('the file went away'));
  await readB;
  await flush();
  assert.doesNotMatch(v.els.notice.textContent, /couldn't read/);
  assert.equal(imports(), 0);
  // A newer selection in the same view wins; the older read, finishing later, sends nothing.
  const c = deferred();
  const readC = choose(c.p);
  await choose(Promise.resolve('NEW-TEXT'));
  await flush();
  assert.equal(imports(), 1);
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_import', arguments: { payload: 'NEW-TEXT' } });
  c.resolve('OLD-TEXT');
  await readC;
  await flush();
  assert.equal(imports(), 1, 'the superseded read sends nothing');
  // A read error for the current selection is still reported.
  const w = await libraryView();
  w.els['import-file'].files = [{ size: 9, name: 'canvas.json', text: () => Promise.reject(new Error('unreadable')) }];
  await w.els['import-file'].onchange();
  assert.match(w.els.notice.textContent, /couldn't read that file\. Nothing was imported/);
  libOnly(v);
  libOnly(w);
});

test('the workspace view offers no library import', async () => {
  const v = await initialized();
  assert.equal(v.els['m-import'].hidden, true);
  assert.equal(v.els['m-folder'].hidden, false);
  assert.ok(v.sent.every((m) => m.method !== 'tools/call' || !LIB_TOOLS.has(m.params.name)));
});

// ---- the in-panel editor (an app-library view whose build has the editor module) ----
// The editor itself (renderer transfer, page transport, Session, mount) is panel.js, tested
// against the real module in crates/openpaperdesign-web/tests/panel.test.mjs. Here a recording
// stand-in with the factory's shape takes its place, to check what the view does around it.

// The in-panel editor's own tools (panel.js calls them through the view's rpc).
const PANEL_TOOLS = new Set([...LIB_TOOLS, 'openpaperdesign_app_editor_asset', 'openpaperdesign_app_page',
  'openpaperdesign_app_page_revision', 'openpaperdesign_app_page_save', 'openpaperdesign_app_ready', 'openpaperdesign_app_page_release']);
const panelOnly = (v) => {
  assert.ok(methods(v).every((m) => ALLOWED.has(m)), methods(v).join());
  for (const m of v.sent.filter((m) => m.method === 'tools/call')) assert.ok(PANEL_TOOLS.has(m.params.name), m.params.name);
  assert.equal(v.count('ui/message'), 0, 'no message to the agent, ever');
};

function fakePanel() {
  const log = [];
  const handles = [];
  const factory = (opts) => {
    factory.rpc = opts.rpc;
    log.push('create');
    return {
      open(o) {
        let settle;
        const h = {
          o, nextDrain: 'clean', drainGate: null,
          started: new Promise((resolve, reject) => { settle = { resolve, reject }; }),
          drain: async () => { log.push('drain'); if (h.drainGate) await h.drainGate; return h.nextDrain; },
          resume: () => { log.push('resume'); },
          dispose: async () => { log.push('dispose'); },
        };
        h.started.catch(() => {});
        h.finish = () => settle.resolve('mounted');
        h.fail = (e) => settle.reject(e);
        handles.push(h);
        log.push('open ' + o.binding);
        return h;
      },
    };
  };
  factory.log = log;
  factory.handles = handles;
  return factory;
}

async function panelView(context = {}) {
  const fake = fakePanel();
  const v = mount({ library: true, panel: fake });
  v.from({ id: v.sent[0].id, result: { protocolVersion: '2026-01-26', hostCapabilities: {}, hostContext: context } });
  await flush();
  v.from({ id: v.last('tools/call').id, result: toolText(LIBRARY_LIST) });
  await flush();
  return { v, fake };
}
// A library editor result for the view's own editor: no link.
const PANEL_EDITOR = (action, id = ID1, extra = {}) => {
  const e = LIB_EDITOR(action, id, { panel: true, panel_editor: 'built in', open_in: 'this view', ...extra });
  delete e.url;
  return e;
};
// Answer the list the view reads again on its way home.
async function relist(v) {
  const call = v.last('tools/call');
  assert.equal(call.params.name, LIB_LIST);
  v.from({ id: call.id, result: toolText(LIBRARY_LIST) });
  await flush();
}
// Open a canvas (the primary hand-off to its web editor link), then choose the optional
// in-panel editor for the same binding.
async function openInPanel(v) {
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els['edit-here'].onclick();
  await flush();
  await answer(v, PANEL_EDITOR('opened'), libAction(v));
}

test('panel: "Edit in this view" mounts the optional in-panel editor on the shown binding, with no message', async () => {
  const { v, fake } = await panelView();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_open', arguments: { id: ID1 } });
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  assert.equal(v.count('ui/open-link'), 1, 'the primary hand-off');
  assert.deepEqual(fake.log, ['create'], 'no in-panel editor unasked');
  v.els['edit-here'].onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_editor', arguments: { binding: B1, panel: true } },
    'asked for the binding this result showed');
  await answer(v, PANEL_EDITOR('opened'), libAction(v));
  assert.equal(v.els.editor.hidden, false);
  assert.equal(v.els.result.hidden, true);
  assert.equal(v.els['menu-button'].hidden, true, 'Back and Close are the only ways out');
  assert.equal(v.els.app.getAttribute('data-view'), 'editor');
  assert.equal(v.count('ui/open-link'), 1, 'the in-panel editor itself opens no link');
  assert.deepEqual(fake.log, ['create', 'open ' + B1]);
  const h = fake.handles[0];
  assert.equal(h.o.binding, B1, 'the editor edits exactly the binding this result named');
  assert.equal(h.o.refs.canvas, v.els['ed-canvas']);
  assert.equal(h.o.refs.conflict, v.els['ed-conflict']);
  // Copied into this realm (the view's arrays have the VM context's prototypes).
  assert.deepEqual(Array.from(h.o.refs.tools, ([b, t]) => [b, t]), [[v.els['ed-select'], 'select'], [v.els['ed-hand'], 'hand'],
    [v.els['ed-rect'], 'rect'], [v.els['ed-ellipse'], 'ellipse']]);
  assert.deepEqual(Array.from(h.o.refs.actions, ([, a]) => a), ['undo', 'redo', 'delete']);
  assert.equal(v.els['ed-status'].textContent, 'Loading the editor…');
  assert.equal(v.els['ed-title'].textContent, 'Landing');
  // The factory got the view's own request function: its calls go to the host, as the view's.
  fake.rpc('tools/call', { name: 'openpaperdesign_app_page', arguments: { binding: B1 } });
  assert.deepEqual(v.last('tools/call').params, { name: 'openpaperdesign_app_page', arguments: { binding: B1 } });
  h.o.onStatus('Ready');
  assert.equal(v.els['ed-status'].textContent, 'Ready');
  panelOnly(v);
});

test('panel: New, Import and Open current canvas stay web-editor-first even when the module is there', async () => {
  const { v, fake } = await panelView();
  v.els['m-new'].onclick();
  v.els.name.value = 'Pitch';
  await submitNew(v);
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_create', arguments: { name: 'Pitch' } });
  await answer(v, LIB_EDITOR('created'), libAction(v));
  assert.equal(v.els['result-state'].textContent, 'Created and saved in your library. Opening the editor…');
  assert.equal(v.count('ui/open-link'), 1);
  assert.equal(fake.handles.length, 0, 'no in-panel editor unasked');
  v.els.back.onclick();
  await flush();
  v.els['import-file'].files = [{ size: 9, name: 'canvas.json', text: async () => 'FILE-TEXT' }];
  await v.els['import-file'].onchange();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_import', arguments: { payload: 'FILE-TEXT' } });
  await answer(v, { refused: 'nope' }, libAction(v));
  assert.equal(v.count('ui/open-link'), 1, 'a refusal opens nothing');
  v.els.back.onclick();
  await flush();
  v.els['m-current'].onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_editor', arguments: {} });
  panelOnly(v);
});

test('panel: Back saves what is left first, then lets the editor go; the canvas stays open', async () => {
  const { v, fake } = await panelView();
  await openInPanel(v);
  v.els.back.onclick();
  await flush();
  assert.deepEqual(fake.log.slice(2), ['drain', 'dispose'], 'drained before it is let go');
  assert.equal(v.els.home.hidden, false);
  assert.equal(v.els['menu-button'].hidden, false);
  assert.equal(libCalls(v, 'openpaperdesign_app_close'), 0, 'Back closes nothing');
  // Another canvas opens in a fresh editor.
  await relist(v);
  await openInPanel(v);
  assert.equal(fake.handles.length, 2);
  panelOnly(v);
});

test('panel: an editor with a change that isn\'t saved stays until the person leaves without saving', async () => {
  const { v, fake } = await panelView();
  await openInPanel(v);
  const h = fake.handles[0];
  for (const [reason, want] of [['conflict', /changed elsewhere/], ['busy', /edit is still under way/],
    ['failed', /couldn't be saved yet/], ['unreachable', /may not be saved/], ['error', /renderer stopped/]]) {
    h.nextDrain = reason;
    v.els.back.onclick();
    await flush();
    assert.equal(v.els.editor.hidden, false, reason);
    assert.match(v.els['ed-status'].textContent, want, reason);
    assert.equal(v.els['ed-status'].className, 'ed-status bad');
    assert.equal(v.els['ed-leave'].hidden, false);
  }
  h.nextDrain = 'conflict';
  v.els['ed-close'].onclick();
  await flush();
  assert.equal(libCalls(v, 'openpaperdesign_app_close'), 0, 'not closed while a change is unsaved');
  assert.ok(!fake.log.includes('dispose'), 'never let go silently');
  assert.equal(fake.log.filter((e) => e === 'resume').length, 6, 'editing goes on each time');
  // The explicit discard, for the Close the person asked for.
  await v.els['ed-leave'].onclick();
  await flush();
  assert.equal(fake.log.at(-1), 'dispose');
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_close', arguments: { binding: B1 } });
  panelOnly(v);
});

test('panel: Close drains and lets go, then closes this binding, asking first if another editor may have edits', async () => {
  const { v, fake } = await panelView();
  await openInPanel(v);
  v.els['ed-close'].onclick();
  await flush();
  assert.deepEqual(fake.log.slice(2), ['drain', 'dispose']);
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_close', arguments: { binding: B1 } }, 'not forced');
  await answer(v, { refused: 'The editor of this canvas was opened (its browser tab, or the editor in another OpenPaperDesign view). ... then close again with force: true.' }, libAction(v));
  assert.equal(v.els['result-title'].textContent, 'Close the canvas?');
  v.els.force.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_close', arguments: { binding: B1, force: true } });
  await answer(v, { action: 'closed', canvas: { id: ID1 } }, libAction(v));
  assert.equal(v.els['result-title'].textContent, 'Canvas closed');
  panelOnly(v);
});

test('panel: the host\'s teardown is answered only after the editor was drained and let go', async () => {
  const { v, fake } = await panelView();
  await openInPanel(v);
  const h = fake.handles[0];
  let open;
  h.drainGate = new Promise((r) => { open = r; });
  v.from({ id: 'td-1', method: 'ui/resource-teardown', params: {} });
  await flush();
  const replied = () => v.sent.some((m) => m.id === 'td-1' && m.result);
  assert.equal(replied(), false, 'still draining');
  open();
  await flush();
  assert.deepEqual(fake.log.slice(2), ['drain', 'dispose']);
  assert.equal(replied(), true);
  // Without an editor it is answered at once.
  const w = await panelView();
  w.v.from({ id: 'td-2', method: 'ui/resource-teardown', params: {} });
  await flush();
  assert.ok(w.v.sent.some((m) => m.id === 'td-2' && m.result));
});

test('panel: a model result or cancellation never replaces an editor', async () => {
  const { v } = await panelView();
  await openInPanel(v);
  v.from(result(LIB_EDITOR('opened', ID2)));
  v.from({ method: 'ui/notifications/tool-cancelled', params: {} });
  await flush();
  assert.equal(v.els.editor.hidden, false);
  assert.equal(v.count('ui/open-link'), 1, 'only the primary hand-off, before the panel');
});

test('panel: a start failure is shown as it came; Back still leaves', async () => {
  const { v, fake } = await panelView();
  await openInPanel(v);
  fake.handles[0].fail(new Error('WebAssembly.instantiate(): Refused to compile or instantiate WebAssembly module'));
  await flush();
  assert.match(v.els['ed-status'].textContent, /^The editor couldn't start here: WebAssembly\.instantiate\(\): Refused to compile/);
  assert.equal(v.els['ed-status'].className, 'ed-status bad');
  v.els.back.onclick();
  await flush();
  assert.equal(v.els.home.hidden, false);
});

test('panel: the browser editor is a secondary action, for the same binding and only a local link', async () => {
  const { v } = await panelView();
  await openInPanel(v);
  v.els['ed-browser'].onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_editor', arguments: { binding: B1 } });
  await answer(v, LIB_EDITOR('reopened'), libAction(v));
  assert.equal(v.last('ui/open-link').params.url, URL_OK);
  await ack(v);
  assert.match(v.els['ed-note'].textContent, /doesn't confirm it loaded/);
  assert.equal(v.els.editor.hidden, false, 'the panel editor stays');
  // A link for another binding, or not a local one, is never opened.
  v.els['ed-browser'].onclick();
  await flush();
  await answer(v, LIB_EDITOR('reopened', ID2), libAction(v));
  v.els['ed-browser'].onclick();
  await flush();
  await answer(v, LIB_EDITOR('reopened', ID1, { url: 'https://example.com/#token=' + TOKEN }), libAction(v));
  assert.equal(v.count('ui/open-link'), 2, 'the primary hand-off and this one');
  assert.match(v.els['ed-note'].textContent, /nothing was opened/);
  panelOnly(v);
});

test('panel: Full screen only when the host offers it', async () => {
  const plain = await panelView();
  await openInPanel(plain.v);
  assert.equal(plain.v.els['ed-full'].hidden, true);
  const { v } = await panelView({ availableDisplayModes: ['inline', 'fullscreen'] });
  await openInPanel(v);
  assert.equal(v.els['ed-full'].hidden, false);
  v.els['ed-full'].onclick();
  await flush();
  assert.deepEqual(v.last('ui/request-display-mode').params, { mode: 'fullscreen' });
  v.from({ id: v.last('ui/request-display-mode').id, result: { mode: 'fullscreen' } });
  await flush();
  assert.equal(v.els['ed-full'].textContent, 'Exit full screen');
  // Leaving goes back to the inline panel.
  v.els.back.onclick();
  await flush();
  assert.deepEqual(v.last('ui/request-display-mode').params, { mode: 'inline' });
  panelOnly(v);
});

test('library: without the in-panel module, New and Open still hand off to the web editor, and no in-panel choice is shown', async () => {
  assert.ok(libScript.includes('const opdCreatePanel = null;'), 'the slot as a build without the module leaves it');
  const v = await libraryView(LIBRARY_LIST, { panel: false });
  assert.deepEqual(rows(v).map((r) => [r.name, r.hint]), [['Landing', '2 pages'], ['Notes', '1 page · open here']]);
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_open', arguments: { id: ID1 } });
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  assert.equal(v.count('ui/open-link'), 1);
  assert.equal(v.last('ui/open-link').params.url, URL_OK);
  assert.equal(v.els['edit-here'].hidden, true, 'no in-panel choice without the module');
  v.els['edit-here'].onclick();
  await flush();
  assert.equal(libCalls(v, 'openpaperdesign_app_editor'), 0, 'and nothing is asked for');
  v.els.back.onclick();
  v.els['m-new'].onclick();
  v.els.name.value = 'Pitch';
  await submitNew(v);
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_create', arguments: { name: 'Pitch' } });
  await answer(v, LIB_EDITOR('created'), libAction(v));
  assert.equal(v.count('ui/open-link'), 2);
  libOnly(v);
});

// ---- R11: what the home says about a browser opening, and which results it opens ----
// These run with the view's timers live (`clock: true`). Their sequences are adapted from the
// R11 reviewer's probes (home-probes.mjs, home-binding-probes.mjs).

async function openedInBrowser() {
  const v = await libraryView(LIBRARY_LIST, { panel: false, clock: true });
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  assert.equal(v.count('ui/open-link'), 1);
  return v;
}
// Run every timer the view has set, again for any it sets meanwhile, and settle.
async function letTimersRun(v) {
  for (let i = 0; i < 5; i++) {
    v.runAllTimers();
    await flush();
  }
}
const readinessPolls = (v) => libCalls(v, 'openpaperdesign_app_editor');

test('R11-F1 control: the clock is live here: an unanswered opening request times out and says so', async () => {
  const v = await openedInBrowser();
  assert.equal(v.els['canvas-state'].textContent, 'Asking Codex to open the canvas…');
  v.tick(60000);
  await flush();
  assert.equal(v.els['canvas-state'].textContent, 'Codex didn\'t answer. Press Open canvas to try again.');
  assert.equal(v.els.canvas.disabled, false);
  libOnly(v);
});

test('R11-F1: an acknowledged opening is reported as a request only; nothing later claims it loaded', async () => {
  const v = await openedInBrowser();
  await ack(v);
  const said = v.els['canvas-state'].textContent;
  assert.match(said, /doesn't confirm it loaded/);
  await letTimersRun(v);
  assert.equal(readinessPolls(v), 0, 'no readiness poll: the editor itself shows Ready');
  assert.equal(v.els['canvas-state'].textContent, said);
  assert.doesNotMatch(v.els.live.textContent, /loaded this canvas/);
  libOnly(v);
});

test('R11-F1: a failed opening stays failed after the timers run', async () => {
  for (const reply of [{ result: { isError: true } }, { error: { code: -32603, message: 'not opened' } }]) {
    const v = await openedInBrowser();
    await ack(v, 'ui/open-link', reply);
    assert.match(v.els['canvas-state'].textContent, /^Codex didn't open the link/);
    await letTimersRun(v);
    assert.equal(readinessPolls(v), 0);
    assert.match(v.els['canvas-state'].textContent, /^Codex didn't open the link/, JSON.stringify(reply));
  }
});

test('R11-F1: a reply to an earlier opening attempt never replaces a later attempt\'s failure', async () => {
  const v = await openedInBrowser();
  const first = v.last('ui/open-link');
  v.els.canvas.onclick(); // a retry while the first request is unanswered
  await flush();
  assert.equal(v.count('ui/open-link'), 2);
  await ack(v, 'ui/open-link', { error: { code: -32603, message: 'not opened' } });
  const failed = v.els['canvas-state'].textContent;
  assert.match(failed, /^Codex didn't open the link \(code -32603\)/);
  v.from({ id: first.id, result: {} }); // the first request is acknowledged late
  await flush();
  await letTimersRun(v);
  assert.equal(v.els['canvas-state'].textContent, failed);
  assert.equal(readinessPolls(v), 0);
});

test('R11-F1: readiness the binding got elsewhere (an earlier tab, the optional panel) is never this opening\'s', async () => {
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  // The result carries the binding's shared readiness and saves from another editor.
  await answer(v, LIB_EDITOR('opened', ID1, { editor_loaded: true, browser_saves: 3 }), libAction(v));
  await ack(v);
  await letTimersRun(v);
  const shown = ['result-state', 'canvas-state', 'live', 'notice'].flatMap((id) => texts(v.els[id])).join('\n');
  assert.doesNotMatch(shown, /loaded this canvas|Saved from the editor/);
  assert.match(v.els['canvas-state'].textContent, /doesn't confirm it loaded/);
  assert.equal(readinessPolls(v), 0);
  assert.equal(v.fake.handles.length, 0);
  libOnly(v);
});

test('R11-F2: a library result is opened only with a valid binding of its own canvas', async () => {
  for (const [what, extra] of [
    ['an omitted binding', { binding: undefined }],
    ['an invalid binding', { binding: 'invalid' }],
    ['another canvas\'s binding', { binding: B2 }],
    ['no canvas id', { canvas: { name: 'Landing', pages: 1 } }],
  ]) {
    const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
    row(v, 'Landing', '2 pages').open.onclick();
    await flush();
    await answer(v, LIB_EDITOR('opened', ID1, extra), libAction(v));
    assert.equal(v.count('ui/open-link'), 0, `${what}: no navigation`);
    assert.match(v.els['result-state'].textContent, /canvas and binding don't match/, what);
    for (const id of ['canvas', 'edit-here', 'close', 'export']) assert.equal(v.els[id].hidden, true, `${what}: ${id} stays hidden`);
    // No action is left targeting anything.
    v.els.close.onclick();
    v.els.export.onclick();
    v.els['edit-here'].onclick();
    await letTimersRun(v);
    for (const t of ['openpaperdesign_app_close', 'openpaperdesign_app_export', 'openpaperdesign_app_editor']) {
      assert.equal(libCalls(v, t), 0, `${what}: ${t}`);
    }
    libOnly(v);
  }
  // The valid control: the same answer with its own binding opens once and enables its actions.
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  assert.equal(v.count('ui/open-link'), 1);
  for (const id of ['canvas', 'edit-here', 'close', 'export']) assert.equal(v.els[id].hidden, false, id);
  v.els.close.onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_close', arguments: { binding: B1 } });
});

test('R11-F2: "Edit in this view" accepts exactly the binding it asked for, not another opening of its canvas', async () => {
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els['edit-here'].onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_editor', arguments: { binding: B1, panel: true } });
  await answer(v, PANEL_EDITOR('opened', ID1, { binding: `${ID1}.9` }), libAction(v));
  assert.equal(v.fake.handles.length, 0, 'no editor for another opening');
  assert.match(v.els['result-state'].textContent, /another opening of this canvas/);
  assert.equal(v.els.close.hidden, true, 'no action targets either binding');
  // The control: asking again from a valid result, with its own binding answered.
  v.els.back.onclick();
  await flush();
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els['edit-here'].onclick();
  await flush();
  await answer(v, PANEL_EDITOR('opened'), libAction(v));
  assert.equal(v.fake.handles.length, 1);
  assert.equal(v.fake.handles[0].o.binding, B1);
});

// ---- R12: the in-panel editor's browser button, and library answers without their label ----
// Live timers again. The panel opening sequence, the unlabelled Open and "Edit in this view"
// answers and the exact-binding/Back control are adapted from the R12 reviewer's probes
// (r12-probes.mjs).

// An ordinary way the editor's buttons come back while an opening is pending: Back is refused
// because a change isn't saved yet, and editing goes on.
async function refusedBack(v) {
  v.fake.handles.at(-1).nextDrain = 'failed';
  v.els.back.onclick();
  await flush();
  assert.equal(v.els.editor.hidden, false);
  assert.equal(v.els['ed-browser'].disabled, false, 'a refused Back gives the buttons back');
}

test('R12-F1: the in-panel editor\'s browser button keeps a newer failure when an older acknowledgement arrives', async () => {
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  await openInPanel(v);
  const first = v.els['ed-browser'].onclick();
  await flush();
  assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_editor', arguments: { binding: B1 } });
  await answer(v, LIB_EDITOR('reopened'), libAction(v));
  const firstOpening = v.last('ui/open-link');
  assert.equal(v.els['ed-browser'].disabled, true, 'busy until Codex answers this opening');
  await refusedBack(v);
  const second = v.els['ed-browser'].onclick();
  await flush();
  await answer(v, LIB_EDITOR('reopened'), libAction(v));
  const secondOpening = v.last('ui/open-link');
  assert.notEqual(firstOpening.id, secondOpening.id);
  await ack(v, 'ui/open-link', { error: { code: -32603, message: 'second opening failed' } });
  await second;
  const failed = v.els['ed-note'].textContent;
  assert.equal(failed, "Codex didn't open the link (code -32603).");
  v.from({ id: firstOpening.id, result: {} }); // the first opening is acknowledged late
  await first;
  await letTimersRun(v);
  assert.equal(v.els['ed-note'].textContent, failed, 'an earlier opening never replaces the later failure');
  assert.equal(v.els['ed-browser'].disabled, false);
  // A valid retry is still possible, and still only a request.
  const retry = v.els['ed-browser'].onclick();
  await flush();
  await answer(v, LIB_EDITOR('reopened'), libAction(v));
  await ack(v);
  await retry;
  assert.match(v.els['ed-note'].textContent, /doesn't confirm it loaded/);
  panelOnly(v);
});

test('R12-F1: a link fetched for an earlier press is never opened once a newer press owns the button', async () => {
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  await openInPanel(v);
  const before = v.count('ui/open-link');
  const first = v.els['ed-browser'].onclick();
  await flush();
  const firstFetch = libAction(v);
  assert.equal(v.els['ed-browser'].disabled, true);
  await refusedBack(v);
  const second = v.els['ed-browser'].onclick();
  await flush();
  const secondFetch = libAction(v);
  assert.notEqual(firstFetch.id, secondFetch.id);
  await answer(v, LIB_EDITOR('reopened'), secondFetch);
  await ack(v, 'ui/open-link', { result: { isError: true } });
  await second;
  const failed = v.els['ed-note'].textContent;
  assert.equal(failed, "Codex didn't open the link.");
  await answer(v, LIB_EDITOR('reopened'), firstFetch); // the first press's link comes back late
  // Checked before waiting for that press: had it opened its link, it would wait for Codex.
  assert.equal(v.count('ui/open-link'), before + 1, 'the earlier press opens nothing');
  await first;
  await letTimersRun(v);
  assert.equal(v.count('ui/open-link'), before + 1);
  assert.equal(v.els['ed-note'].textContent, failed);
  // An unanswered opening still says so.
  const third = v.els['ed-browser'].onclick();
  await flush();
  await answer(v, LIB_EDITOR('reopened'), libAction(v));
  v.tick(60000);
  await third;
  assert.equal(v.els['ed-note'].textContent, "Codex didn't answer.");
  panelOnly(v);
});

test('R12-F1 control: the browser button opens only its own binding, and Back drops both its link and its opening', async () => {
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  await openInPanel(v);
  const wrong = v.els['ed-browser'].onclick();
  await flush();
  const before = v.count('ui/open-link');
  await answer(v, LIB_EDITOR('reopened', ID1, { binding: `${ID1}.9` }), libAction(v));
  await wrong;
  assert.equal(v.count('ui/open-link'), before);
  assert.match(v.els['ed-note'].textContent, /nothing was opened/);
  const valid = v.els['ed-browser'].onclick();
  await flush();
  await answer(v, LIB_EDITOR('reopened'), libAction(v));
  const opening = v.last('ui/open-link');
  assert.equal(v.count('ui/open-link'), before + 1);
  v.els.back.onclick();
  await flush();
  assert.equal(v.els.home.hidden, false);
  const note = v.els['ed-note'].textContent;
  v.from({ id: opening.id, error: { code: -32603, message: 'late failure' } });
  await valid;
  assert.equal(v.els['ed-note'].textContent, note, 'Back drops this opening\'s reply');
  // A link asked for before Back is never opened after it.
  await relist(v);
  await openInPanel(v);
  const opened = v.count('ui/open-link');
  const late = v.els['ed-browser'].onclick();
  await flush();
  const fetch = libAction(v);
  v.els.back.onclick();
  await flush();
  assert.equal(v.els.home.hidden, false);
  await answer(v, LIB_EDITOR('reopened'), fetch);
  await late;
  await letTimersRun(v);
  assert.equal(v.count('ui/open-link'), opened);
  panelOnly(v);
});

// What a library action did with its answer.
const outcome = (v) => ({
  navigations: v.count('ui/open-link'),
  mounts: v.fake ? v.fake.handles.length : 0,
  workspace: fields(v).includes('Folder'),
  enabled: ['canvas', 'edit-here', 'close', 'export'].filter((id) => !v.els[id].hidden),
});
// The library actions that hand a canvas to its web editor, each started in a fresh view. A
// start isn't awaited: its action waits for the answer the test then gives (a flush exposes the
// request, Import's file read included).
const LIB_ROUTES = [
  ['Open', (v) => row(v, 'Landing', '2 pages').open.onclick()],
  ['New', (v) => { v.els['m-new'].onclick(); v.els.name.value = 'Pitch'; v.els['new-form'].onsubmit({ preventDefault() {} }); }],
  ['Open current canvas', (v) => v.els['m-current'].onclick()],
  ['Import', (v) => {
    v.els['import-file'].files = [{ size: 9, name: 'canvas.json', text: async () => 'FILE-TEXT' }];
    return v.els['import-file'].onchange();
  }],
];

test('R12-F2: a known library action checks its answer whatever storage the answer claims', async () => {
  // Collected first, so each case fails on its own.
  const observations = [];
  for (const storage of [undefined, 'workspace']) {
    for (const [route, start] of LIB_ROUTES) {
      for (const binding of [undefined, 'invalid', B2]) {
        const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
        start(v);
        await flush();
        // With workspace fields too, so a workspace presentation would show.
        await answer(v, LIB_EDITOR('opened', ID1, { storage, binding, workspace: '/w/acme', project: 'design/Landing' }), libAction(v));
        const o = outcome(v);
        observations.push({ storage: String(storage), route, binding: String(binding), ...o,
          refused: /canvas and binding don't match/.test(v.els['result-state'].textContent) });
        libOnly(v);
      }
    }
  }
  assert.equal(observations.length, 24);
  assert.deepEqual(observations.filter((o) => o.navigations || o.mounts || o.workspace || o.enabled.length || !o.refused), []);
  // Controls: the same routes, unlabelled, with the answer's own binding: a library result.
  for (const [route, start] of LIB_ROUTES) {
    const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
    start(v);
    await flush();
    await answer(v, LIB_EDITOR('opened', ID1, { storage: undefined }), libAction(v));
    assert.equal(v.count('ui/open-link'), 1, route);
    assert.deepEqual(fields(v), ['Canvas', 'Landing (1 page)', 'Stored in', 'OpenPaperDesign library'], route);
    v.els.close.onclick();
    await flush();
    assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_close', arguments: { binding: B1 } }, route);
    libOnly(v);
  }
  // A primary Open names no binding, so a newer opening of the same canvas is its answer...
  const g = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  row(g, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(g, LIB_EDITOR('opened', ID1, { storage: undefined, binding: `${ID1}.9` }), libAction(g));
  assert.equal(g.count('ui/open-link'), 1);
  g.els.close.onclick();
  await flush();
  assert.deepEqual(libAction(g).params, { name: 'openpaperdesign_app_close', arguments: { binding: `${ID1}.9` } });
  // ...but never the in-panel editor, which it didn't ask for.
  const p = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  row(p, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(p, PANEL_EDITOR('opened', ID1, { storage: undefined }), libAction(p));
  assert.deepEqual(outcome(p), { navigations: 0, mounts: 0, workspace: false, enabled: [] });
  assert.match(p.els['result-state'].textContent, /another way of opening this canvas/);
  libOnly(p);
});

test('R12-F2: "Edit in this view" accepts only its own binding, in this view, whatever storage the answer claims', async () => {
  const cases = [
    ['another opening, as a link', LIB_EDITOR('reopened', ID1, { storage: undefined, binding: `${ID1}.9` })],
    ['another opening, in this view', PANEL_EDITOR('opened', ID1, { storage: undefined, binding: `${ID1}.9` })],
    ['an invalid binding', PANEL_EDITOR('opened', ID1, { storage: 'workspace', binding: 'invalid' })],
    ['no binding', PANEL_EDITOR('opened', ID1, { storage: undefined, binding: undefined })],
    ['another canvas', PANEL_EDITOR('opened', ID2, { storage: undefined })],
    ['its own binding, but as a link', LIB_EDITOR('reopened', ID1, { storage: undefined })],
  ];
  // Collected first, so each case fails on its own.
  const observations = [];
  for (const [what, reply] of cases) {
    const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
    row(v, 'Landing', '2 pages').open.onclick();
    await flush();
    await answer(v, LIB_EDITOR('opened'), libAction(v));
    const initial = v.count('ui/open-link');
    v.els['edit-here'].onclick();
    await flush();
    assert.deepEqual(libAction(v).params, { name: 'openpaperdesign_app_editor', arguments: { binding: B1, panel: true } });
    await answer(v, reply, libAction(v));
    observations.push({ what, extraNavigations: v.count('ui/open-link') - initial, mounts: v.fake.handles.length,
      enabled: ['canvas', 'edit-here', 'close', 'export'].filter((id) => !v.els[id].hidden) });
    libOnly(v);
  }
  assert.deepEqual(observations.filter((o) => o.extraNavigations || o.mounts || o.enabled.length), []);
  // The control: its own binding, unlabelled, mounts on it and opens nothing more.
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els['edit-here'].onclick();
  await flush();
  await answer(v, PANEL_EDITOR('opened', ID1, { storage: undefined }), libAction(v));
  assert.equal(v.fake.handles.length, 1);
  assert.equal(v.fake.handles[0].o.binding, B1);
  assert.equal(v.count('ui/open-link'), 1);
  panelOnly(v);
});

test('R12-F2: a library view treats a delivered editor result as a library one; a workspace view keeps its own', async () => {
  const v = await libraryView(LIBRARY_LIST, { panel: true, clock: true });
  v.from(result(LIB_EDITOR('opened', ID1, { storage: undefined, binding: 'invalid', workspace: '/w/acme' })));
  assert.match(v.els['result-state'].textContent, /canvas and binding don't match/);
  assert.deepEqual(outcome(v), { navigations: 0, mounts: 0, workspace: false, enabled: [] });
  v.from(result(LIB_EDITOR('opened', ID1, { storage: undefined })));
  assert.deepEqual(fields(v), ['Canvas', 'Landing (1 page)', 'Stored in', 'OpenPaperDesign library']);
  assert.equal(v.count('ui/open-link'), 0, 'a delivered result is shown, not opened');
  assert.equal(v.els.canvas.hidden, false);
  libOnly(v);
  // The workspace view: its unlabelled results are workspace results, as before.
  const w = await initialized();
  w.from(result(EDITOR('Alpha')));
  assert.deepEqual(fields(w), ['Canvas', 'Alpha (1 page)', 'Folder', 'acme']);
  w.els['m-current'].onclick();
  await flush();
  await answer(w, EDITOR('Beta', 'reopened'));
  assert.equal(w.count('ui/open-link'), 1);
  assert.deepEqual(fields(w), ['Canvas', 'Beta (1 page)', 'Folder', 'acme']);
  onlyAllowed(w);
});

// ---- R13: leaving the in-panel editor ends its browser opening, with the real panel module ----
// The exact crates/openpaperdesign-web/static/panel.js lifecycle and transport (read here, so
// this needs that file); only the engine, the editor mount and the platform check are stand-ins.
// Nothing renders. Adapted from the R13 reviewer's r13-real-panel-probe.mjs.
let realPanelModule = null;
async function realPanelView() {
  if (!realPanelModule) {
    const source = readFileSync(new URL('../../openpaperdesign-web/static/panel.js', import.meta.url), 'utf8');
    realPanelModule = import('data:text/javascript;base64,'
      + Buffer.from('const opdIsMac = () => false;\n' + source).toString('base64'));
  }
  const panel = await realPanelModule;
  const editor = { nextDrain: 'clean', disposed: false, handle: null };
  const factory = ({ rpc }) => {
    const real = panel.opdCreatePanel({
      rpc, identity: 'r13-fixture', nav: {}, lib: {}, env: {},
      engine: { init: async () => {}, start: () => {} },
      mount: () => ({ started: Promise.resolve(), drain: async () => editor.nextDrain,
        resume: () => {}, dispose: () => { editor.disposed = true; } }),
    });
    return { open: (o) => { editor.handle = real.open(o); return editor.handle; } };
  };
  const v = mount({ library: true, panel: factory, clock: true });
  v.from({ id: v.sent[0].id, result: { protocolVersion: '2026-01-26', hostCapabilities: {}, hostContext: {} } });
  await flush();
  await answer(v, LIBRARY_LIST, v.last('tools/call'));
  row(v, 'Landing', '2 pages').open.onclick();
  await flush();
  await answer(v, LIB_EDITOR('opened'), libAction(v));
  v.els['edit-here'].onclick();
  await flush();
  await answer(v, PANEL_EDITOR('opened'), libAction(v));
  const asset = v.last('tools/call');
  assert.equal(asset.params.name, 'openpaperdesign_app_editor_asset');
  await answer(v, { asset: 'openpaperdesign_web_bg.wasm', identity: 'r13-fixture', bytes: 1,
    parts: 1, part: 0, offset: 0, part_bytes: 1, data: 'AA==' }, asset);
  const page = v.last('tools/call');
  assert.equal(page.params.name, 'openpaperdesign_app_page');
  await answer(v, { binding: B1, page: { id: 'p1', name: 'Page 1' }, text: '{}', revision: '"r1"', mount: 7 }, page);
  assert.equal(await editor.handle.started, 'mounted');
  return { v, editor };
}

test('R13-F1: once Back, Close, a discard or teardown lets the editor go, its late link or acknowledgement does nothing, while the real release is pending', async () => {
  // Collected first, so each case fails on its own.
  const observations = [];
  for (const departure of ['back', 'close', 'discard', 'teardown']) {
    for (const stage of ['link', 'acknowledgement']) {
      const { v, editor } = await realPanelView();
      const opening = v.els['ed-browser'].onclick();
      await flush();
      const fetch = libAction(v);
      assert.deepEqual(fetch.params, { name: 'openpaperdesign_app_editor', arguments: { binding: B1 } });
      if (stage === 'acknowledgement') await answer(v, LIB_EDITOR('reopened'), fetch); // its opening is unanswered
      const sent = v.count('ui/open-link');
      let leaving = null;
      if (departure === 'back') leaving = v.els.back.onclick();
      else if (departure === 'close') leaving = v.els['ed-close'].onclick();
      else if (departure === 'discard') {
        editor.nextDrain = 'failed';
        await v.els.back.onclick();
        assert.equal(v.els['ed-leave'].hidden, false, 'leaving without saving is offered');
        leaving = v.els['ed-leave'].onclick();
      } else v.from({ id: 'td-r13', method: 'ui/resource-teardown', params: {} });
      await flush();
      // The mount is gone; the server hasn't answered its release yet.
      const release = v.last('tools/call');
      assert.deepEqual(release.params, { name: 'openpaperdesign_app_page_release', arguments: { binding: B1, mount: 7 } }, departure);
      assert.equal(editor.disposed, true, departure);
      const note = v.els['ed-note'].textContent;
      if (stage === 'link') await answer(v, LIB_EDITOR('reopened'), fetch);
      else await ack(v, 'ui/open-link', { error: { code: -32603, message: 'late failure' } });
      const o = { departure, stage, extraNavigations: v.count('ui/open-link') - sent, noteChanged: v.els['ed-note'].textContent !== note };
      if (o.extraNavigations) await ack(v); // settle what shouldn't have been sent
      await opening;
      await answer(v, { binding: B1, released: true }, release);
      if (leaving) await leaving;
      await flush();
      o.left = departure === 'close'
        ? JSON.stringify(libAction(v).params) === JSON.stringify({ name: 'openpaperdesign_app_close', arguments: { binding: B1 } })
        : departure === 'teardown' ? v.sent.some((m) => m.id === 'td-r13' && m.result) : !v.els.home.hidden;
      observations.push(o);
      panelOnly(v);
    }
  }
  assert.equal(observations.length, 8);
  assert.deepEqual(observations.filter((o) => o.extraNavigations || o.noteChanged || !o.left), []);
});

test('R13-F1 control: with the real panel, a refused Back keeps the editor and its opening; a retry and a clean Back still work', async () => {
  const { v, editor } = await realPanelView();
  const before = v.count('ui/open-link');
  const opening = v.els['ed-browser'].onclick();
  await flush();
  const fetch = libAction(v);
  editor.nextDrain = 'failed';
  await v.els.back.onclick();
  assert.equal(v.els.editor.hidden, false);
  assert.equal(editor.disposed, false, 'a refused Back lets nothing go');
  await answer(v, LIB_EDITOR('reopened'), fetch);
  assert.equal(v.count('ui/open-link'), before + 1, 'the editor stayed, so its opening goes on');
  await ack(v);
  await opening;
  assert.match(v.els['ed-note'].textContent, /doesn't confirm it loaded/);
  const retry = v.els['ed-browser'].onclick();
  await flush();
  await answer(v, LIB_EDITOR('reopened'), libAction(v));
  await ack(v, 'ui/open-link', { error: { code: -32603, message: 'not opened' } });
  await retry;
  assert.equal(v.els['ed-note'].textContent, "Codex didn't open the link (code -32603).");
  assert.equal(v.count('ui/open-link'), before + 2);
  editor.nextDrain = 'clean';
  const back = v.els.back.onclick();
  await flush();
  const release = v.last('tools/call');
  assert.deepEqual(release.params, { name: 'openpaperdesign_app_page_release', arguments: { binding: B1, mount: 7 } });
  await answer(v, { binding: B1, released: true }, release);
  await back;
  assert.equal(v.els.home.hidden, false);
  panelOnly(v);
});
