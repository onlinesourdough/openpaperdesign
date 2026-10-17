// The in-panel editor module as openpaperdesign-mcp assembles it (`panel_module` in
// crates/openpaperdesign-serve/src/launcher.rs): the built wasm-bindgen glue, then session.js,
// page.js, editor.js and panel.js, in that order, as one ES module, imported here for real.
// Needs the built glue (./scripts/build-web.sh writes target/openpaperdesign-web/): without it
// these tests fail; they never skip.
//
// What is real here: the module's parsing and linking, the renderer part transfer and its
// checks, the page transport, Session and the editor mount, against a fake MCP Apps parent
// that answers like the server's in-panel tools. What is not: the browser, the host, the GPU.
// The renderer's compile and draw are replaced by an explicit stand-in engine (WebAssembly in
// Node proves nothing about a host's CSP or graphics), so nothing here shows Ready in a host.
// Run: node --test crates/openpaperdesign-web/tests/panel.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const STATIC = new URL('../static/', import.meta.url);
const GLUE = new URL('../../../target/openpaperdesign-web/openpaperdesign_web.js', import.meta.url);
const OWN = ['session.js', 'page.js', 'editor.js', 'panel.js'];
const IDENTITY = '"00112233445566ff"';

// Same parts, order and identity line as `panel_module`.
function assemble(identity = IDENTITY) {
  let out = '// The in-panel editor (assembled by openpaperdesign-mcp from its packaged files).\n'
    + `const OPD_WASM_IDENTITY = ${JSON.stringify(identity)};\n`;
  const parts = [['openpaperdesign_web.js', readFileSync(GLUE, 'utf8')],
    ...OWN.map((f) => [f, readFileSync(new URL(f, STATIC), 'utf8')])];
  for (const [name, text] of parts) out += `\n// ---- ${name} ----\n${text}\n`;
  return out;
}
const asModule = (source) => import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
const SOURCE = assemble();
const mod = await asModule(SOURCE);
const flush = async () => { for (let i = 0; i < 8; i++) await new Promise((r) => setImmediate(r)); };

test('the assembled module links, imports nothing and is safe inside the view\'s script element', async () => {
  for (const name of ['opdCreatePanel', 'opdMountEditor', 'opdBrowserEnv', 'Session', 'frameStep', 'saveStep', 'discardStep', 'start']) {
    assert.equal(typeof mod[name], 'function', name);
  }
  assert.equal(typeof mod.default, 'function', 'the glue\'s own init (__wbg_init)');
  const lower = SOURCE.toLowerCase();
  for (const bad of ['</script', '<script', '<!--']) assert.ok(!lower.includes(bad), bad);
  assert.ok(!SOURCE.split('\n').some((l) => /^\s*import[\s{(]/.test(l)), 'no import declaration or dynamic import');
  for (const f of OWN) {
    const text = readFileSync(new URL(f, STATIC), 'utf8');
    for (const banned of ['eval(', 'new Function', 'import(', 'fetch(', 'XMLHttpRequest', 'localStorage', 'sessionStorage', 'indexedDB', 'ui/message']) {
      assert.ok(!text.includes(banned), `${f}: ${banned}`);
    }
  }
  assert.ok(readFileSync(new URL('panel.js', STATIC), 'utf8').includes('__wbg_init({ module_or_path: bytes })'),
    'the glue is initialized with the transferred bytes, by its own init');
});

test('the home view\'s module with the editor in its slot parses and links as one module', async () => {
  const html = readFileSync(new URL('../../openpaperdesign-serve/src/home.html', import.meta.url), 'utf8');
  const open = '<script type="module">';
  const slot = 'const opdCreatePanel = null; // __OPD_PANEL_MODULE__';
  const home = html.slice(html.lastIndexOf(open) + open.length, html.lastIndexOf('</script>'));
  assert.equal(home.split(slot).length, 2, 'exactly one editor slot');
  // A function replacement: the glue's text is inserted as it is (no `$` patterns).
  const filled = home.replace(slot, () => SOURCE).replace("'__OPD_MODE__'", "'library'");
  // Parsing and linking succeed (a duplicate name or an import would be a SyntaxError before
  // anything ran); it then stops at the first DOM use, as there is no document here.
  await assert.rejects(asModule(filled), (e) => e.name === 'ReferenceError' && /document/.test(e.message));
});

// ---- a fake MCP Apps parent answering like the server's in-panel tools ----

const BINDING = '0123456789abcdef.7';
const PART = 262144;
const T = {
  ASSET: 'openpaperdesign_app_editor_asset', PAGE: 'openpaperdesign_app_page', REVISION: 'openpaperdesign_app_page_revision',
  SAVE: 'openpaperdesign_app_page_save', READY: 'openpaperdesign_app_ready', RELEASE: 'openpaperdesign_app_page_release',
};
const ok = (v) => ({ result: { content: [{ type: 'text', text: JSON.stringify(v) }] } });
const no = (status, error, revision = null) => ({ result: { isError: true, content: [{ type: 'text', text: JSON.stringify({ error, status, revision }) }] } });

function fakeServer(wasm = new Uint8Array(600000).fill(0x61)) {
  const srv = { calls: [], page: { id: 'p1', name: 'Page 1', text: '{"page":1}', revision: '"r1"' }, mounts: new Set(), next: 0,
    wasm, identity: IDENTITY, override: {}, holds: {} };
  srv.part = (i) => {
    const parts = Math.ceil(srv.wasm.length / PART);
    const start = i * PART;
    const slice = srv.wasm.subarray(start, Math.min(start + PART, srv.wasm.length));
    return { asset: 'openpaperdesign_web_bg.wasm', identity: srv.identity, bytes: srv.wasm.length, parts, part: i, offset: start,
      part_bytes: slice.length, data: Buffer.from(slice).toString('base64') };
  };
  srv.rpc = async (method, params) => {
    assert.equal(method, 'tools/call', 'the editor only calls tools');
    const { name, arguments: a } = params;
    srv.calls.push([name, JSON.parse(JSON.stringify(a))]);
    if (srv.holds[name]) await srv.holds[name];
    if (srv.override[name]) return srv.override[name](a);
    const pinned = () => a.binding === BINDING && a.page_id === srv.page.id && srv.mounts.has(a.mount);
    switch (name) {
      case T.ASSET: return ok(srv.part(a.part));
      case T.PAGE:
        if (a.binding !== BINDING) return no(410, 'stale');
        if (a.page_id === undefined) {
          srv.mounts.add(++srv.next);
          return ok({ binding: BINDING, canvas: { name: 'Plan' }, page: { id: srv.page.id, name: srv.page.name }, revision: srv.page.revision, text: srv.page.text, mount: srv.next });
        }
        if (!pinned()) return no(410, 'not mounted');
        return ok({ binding: BINDING, canvas: { name: 'Plan' }, page: { id: srv.page.id, name: srv.page.name }, revision: srv.page.revision, text: srv.page.text, mount: a.mount });
      case T.REVISION:
        if (!pinned()) return no(410, 'not mounted');
        return ok({ binding: BINDING, page_id: srv.page.id, revision: srv.page.revision });
      case T.SAVE:
        if (!pinned()) return no(410, 'not mounted');
        if (a.if_match !== srv.page.revision) return no(409, 'the page changed since it was loaded; nothing was written', srv.page.revision);
        srv.page.text = a.text;
        srv.page.revision = `"s${srv.calls.length}"`;
        return ok({ binding: BINDING, page_id: srv.page.id, saved: true, revision: srv.page.revision });
      case T.READY: return ok({ binding: BINDING, ready: true });
      case T.RELEASE: return ok({ binding: BINDING, released: srv.mounts.delete(a.mount) });
      default: throw new Error('unexpected tool ' + name);
    }
  };
  srv.named = (name) => srv.calls.filter(([n]) => n === name).map(([, a]) => a);
  return srv;
}

// A minimal DOM: listeners honour AbortSignal, so disposal can be checked.
function fakeEl(doc, tag = 'div') {
  const e = {
    tag, attrs: {}, children: [], classes: new Set(), ownerDocument: doc, listeners: [], focused: 0, captured: null, textContent: '',
    addEventListener(type, fn, o = {}) {
      const l = { type, fn };
      e.listeners.push(l);
      if (o.signal) o.signal.addEventListener('abort', () => { e.listeners = e.listeners.filter((x) => x !== l); });
    },
    fire(type, ev = {}) {
      for (const l of [...e.listeners]) if (l.type === type) l.fn(Object.assign({ preventDefault() {}, stopPropagation() { ev.stopped = true; } }, ev));
    },
    setAttribute(k, v) { e.attrs[k] = String(v); },
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 600 }),
    focus() { e.focused += 1; },
    setPointerCapture(id) { e.captured = id; },
    hasPointerCapture(id) { return e.captured === id; },
    releasePointerCapture(id) { if (e.captured === id) e.captured = null; },
    cloneNode() { return fakeEl(doc, tag); },
    replaceWith(n) { e.replacedBy = n; },
    replaceChildren() { e.children = []; },
    append(...c) { e.children.push(...c); },
    classList: { add: (c) => e.classes.add(c), remove: (c) => e.classes.delete(c) },
  };
  return e;
}

function fakeEnv() {
  const env = {
    intervals: new Map(), frames: [], next: 1,
    setInterval(fn, ms) { const id = env.next++; env.intervals.set(id, { fn, ms }); return id; },
    clearInterval(id) { env.intervals.delete(id); },
    requestAnimationFrame(fn) { const id = env.next++; env.frames.push([id, fn]); return id; },
    cancelAnimationFrame(id) { env.frames = env.frames.filter(([i]) => i !== id); },
    ResizeObserver: class { constructor(cb) { this.cb = cb; } observe() {} disconnect() { this.disconnected = true; } },
    AbortController,
    devicePixelRatio: () => 1,
  };
  env.frame = (t) => { const f = env.frames.shift(); if (f) f[1](t); };
  env.every = (ms) => { for (const { fn, ms: m } of [...env.intervals.values()]) if (m === ms) fn(); };
  return env;
}

// The stand-in engine: records the bytes it is given and boots a mock editor.
function fakeEngine() {
  const eng = { inits: [], editors: [] };
  eng.init = async (bytes) => { eng.inits.push(bytes); };
  eng.start = async (canvas, text, pageId, pageName) => {
    const ed = { canvas, text, pageId, pageName, calls: [], pending: undefined, isReady: false, freed: false, canReload: true,
      backend() { return 'mock (test)'; }, frame() {}, gpu_error() { return undefined; }, ready() { return this.isReady; },
      tool() { return 'select'; }, set_tool(t) { this.calls.push('tool:' + t); }, resize() {}, modifiers() {},
      pointer_down() { this.calls.push('down'); }, pointer_move() {}, pointer_up() { this.calls.push('up'); }, wheel() {},
      key_down(k) { this.calls.push('key:' + k); return true; }, key_up() {}, undo() { this.calls.push('undo'); }, redo() {}, delete() {},
      save_snapshot() { const s = this.pending; this.pending = undefined; return s; },
      save_succeeded() { this.calls.push('saved'); }, save_failed() { this.calls.push('save_failed'); },
      dirty() { return this.pending !== undefined; }, can_reload() { return this.canReload && this.pending === undefined; },
      reload(t) { this.calls.push('reload:' + t); }, free() { this.freed = true; } };
    eng.editors.push(ed);
    return ed;
  };
  return eng;
}

function setup(srv = fakeServer()) {
  const doc = { createElement: (t) => fakeEl(doc, t) };
  const env = fakeEnv();
  const engine = fakeEngine();
  const shown = { status: [], renderer: [] };
  const refs = {
    canvas: fakeEl(doc, 'canvas'),
    status: (t, error) => shown.status.push(error ? 'ERR ' + t : t),
    renderer: (t, error) => shown.renderer.push(error ? 'ERR ' + t : t),
    conflict: fakeEl(doc),
    tools: [['select', fakeEl(doc, 'button')], ['rect', fakeEl(doc, 'button')]].map(([t, b]) => [b, t]),
    actions: [[fakeEl(doc, 'button'), 'undo']],
  };
  const panel = mod.opdCreatePanel({ rpc: srv.rpc, identity: IDENTITY, engine, env, nav: { platform: 'Linux' } });
  return { srv, env, engine, refs, shown, panel, doc };
}

async function readyEditor(t = setup()) {
  const h = t.panel.open({ binding: BINDING, refs: t.refs, onStatus: () => {} });
  assert.equal(await h.started, 'mounted');
  const ed = t.engine.editors.at(-1);
  ed.isReady = true;
  t.env.frame(16);
  await flush();
  return Object.assign(t, { h, ed });
}

test('the renderer comes in order, part after part, checked, and is compiled once with exactly those bytes', async () => {
  const wasm = new Uint8Array(600000);
  for (let i = 0; i < wasm.length; i++) wasm[i] = (i * 7) & 0xff;
  const t = setup(fakeServer(wasm));
  const h = t.panel.open({ binding: BINDING, refs: t.refs, onStatus: () => {} });
  assert.equal(await h.started, 'mounted');
  assert.deepEqual(t.srv.calls.map(([n, a]) => (n === T.ASSET ? `asset ${a.part}` : n)).slice(0, 4),
    ['asset 0', 'asset 1', 'asset 2', T.PAGE], 'three parts, in order, then the page');
  assert.equal(t.engine.inits.length, 1);
  assert.deepEqual(t.engine.inits[0], wasm, 'exactly the packaged bytes');
  assert.deepEqual(t.srv.named(T.PAGE)[0], { binding: BINDING }, 'the first read names only the binding');
  const ed = t.engine.editors[0];
  assert.equal(ed.pageId, 'p1');
  assert.equal(ed.text, '{"page":1}');
  // A second editor in the same view reuses the compiled renderer.
  await h.dispose();
  const h2 = t.panel.open({ binding: BINDING, refs: t.refs, onStatus: () => {} });
  assert.equal(await h2.started, 'mounted');
  assert.equal(t.srv.named(T.ASSET).length, 3, 'no second transfer');
  assert.equal(t.engine.inits.length, 1);
});

test('a wrong, mixed, out-of-order, short or oversized part stops the transfer with the reason; nothing is compiled', async () => {
  for (const [what, fault, want] of [
    ['another build', (p) => ({ ...p, identity: '"ffffffffffffffff"' }), /another build/],
    ['out of order', (p) => ({ ...p, part: p.part + 1 }), /out of order or incomplete/],
    ['short data', (p) => ({ ...p, data: p.data.slice(0, 8) }), /isn't the length it says/],
    ['changed count', (p, i) => (i === 1 ? { ...p, bytes: p.bytes + PART, parts: p.parts + 1 } : p), /doesn't match the earlier parts/],
    ['oversized', (p) => ({ ...p, bytes: 17 * 1024 * 1024, parts: 68 }), /unexpected size/],
    ['fractional size', (p) => ({ ...p, bytes: p.bytes + 0.5 }), /unexpected size/],
    ['not base64', (p) => ({ ...p, data: '%%%%' }), /isn't the length it says/],
  ]) {
    const srv = fakeServer();
    srv.override[T.ASSET] = (a) => ok(fault(srv.part(a.part), a.part));
    const t = setup(srv);
    const h = t.panel.open({ binding: BINDING, refs: t.refs, onStatus: () => {} });
    await assert.rejects(h.started, want, what);
    assert.equal(t.engine.inits.length, 0, `${what}: nothing compiled`);
    assert.equal(srv.named(T.PAGE).length, 0, `${what}: no page read`);
  }
  // A refused part is reported as the server said it.
  const srv = fakeServer();
  srv.override[T.ASSET] = () => ({ result: { isError: true, content: [{ type: 'text', text: 'This OpenPaperDesign command was built without the browser editor' }] } });
  const t = setup(srv);
  await assert.rejects(t.panel.open({ binding: BINDING, refs: t.refs }).started, /could not be sent \(part 0\): This OpenPaperDesign command was built without/);
  // A compile refusal (as a host's CSP would give) comes through as it came.
  const c = setup();
  c.engine.init = async () => { throw new Error('WebAssembly.instantiate(): Refused to compile or instantiate WebAssembly module because \'unsafe-eval\' is not allowed'); };
  await assert.rejects(c.panel.open({ binding: BINDING, refs: c.refs }).started, /Refused to compile/);
  assert.equal(c.srv.named(T.PAGE).length, 0);
});

test('leaving during the transfer stops it; leaving during the first read releases the mount it got', async () => {
  const srv = fakeServer();
  let release;
  srv.holds[T.ASSET] = new Promise((r) => { release = r; });
  const t = setup(srv);
  const h = t.panel.open({ binding: BINDING, refs: t.refs });
  await flush();
  await h.dispose();
  release();
  assert.equal(await h.started, 'left');
  assert.equal(srv.named(T.ASSET).length, 1, 'no part after leaving');
  assert.equal(t.engine.inits.length, 0);
  assert.equal(srv.named(T.PAGE).length, 0);
  // The page read under way.
  const s2 = fakeServer();
  let answer;
  s2.holds[T.PAGE] = new Promise((r) => { answer = r; });
  const t2 = setup(s2);
  const h2 = t2.panel.open({ binding: BINDING, refs: t2.refs });
  await flush();
  assert.equal(s2.named(T.PAGE).length, 1);
  await h2.dispose();
  answer();
  assert.equal(await h2.started, 'left');
  await flush();
  assert.deepEqual(s2.named(T.RELEASE), [{ binding: BINDING, mount: 1 }], 'the mount it was given is released');
  assert.equal(t2.engine.editors.length, 0, 'no editor booted');
});

test('an edit is saved through the page tool, pinned to its page and mount; Ready is the renderer\'s own', async () => {
  const t = await readyEditor();
  assert.ok(t.shown.status.includes('Ready'));
  assert.deepEqual(t.srv.named(T.READY), [{ binding: BINDING, mount: 1 }]);
  // Real input through the real mount, then the save loop.
  t.refs.canvas.fire('pointerdown', { pointerId: 3, button: 0, clientX: 5, clientY: 5, timeStamp: 1 });
  assert.equal(t.refs.canvas.captured, 3);
  t.refs.canvas.fire('pointerup', { pointerId: 3, button: 0, clientX: 9, clientY: 9, timeStamp: 2 });
  assert.deepEqual(t.ed.calls.slice(0, 2), ['down', 'up']);
  t.ed.pending = '{"page":1,"edit":true}';
  t.env.every(300);
  await flush();
  assert.deepEqual(t.srv.named(T.SAVE), [{ binding: BINDING, page_id: 'p1', mount: 1, if_match: '"r1"', text: '{"page":1,"edit":true}' }]);
  assert.equal(t.srv.page.text, '{"page":1,"edit":true}', 'the canonical page has it');
  assert.equal(t.shown.status.at(-1), 'Saved');
  assert.ok(t.ed.calls.includes('saved'));
  // Polls name the same page and mount.
  t.env.every(1000);
  await flush();
  assert.deepEqual(t.srv.named(T.REVISION).at(-1), { binding: BINDING, page_id: 'p1', mount: 1 });
  // The canvas's keys are the editor's, not the view's (its Escape is Back).
  const key = { key: 'Escape', code: 'Escape', repeat: false };
  t.refs.canvas.fire('keydown', key);
  assert.equal(key.stopped, true);
  assert.ok(t.ed.calls.includes('key:Escape'));
});

test('leaving waits for the save under way and saves what is left; then everything is undone once', async () => {
  const t = await readyEditor();
  let answer;
  t.srv.holds[T.SAVE] = new Promise((r) => { answer = r; });
  t.ed.pending = '{"v":1}';
  t.env.every(300);
  await flush();
  t.ed.pending = '{"v":2}'; // a completed edit after that snapshot
  const leaving = t.h.drain();
  t.refs.canvas.fire('pointerdown', { pointerId: 1, button: 0, clientX: 1, clientY: 1, timeStamp: 3 });
  assert.ok(!t.ed.calls.includes('down'), 'no input while leaving');
  delete t.srv.holds[T.SAVE];
  answer();
  assert.equal(await leaving, 'clean');
  assert.deepEqual(t.srv.named(T.SAVE).map((a) => a.text), ['{"v":1}', '{"v":2}']);
  assert.equal(t.srv.page.text, '{"v":2}');
  await t.h.dispose();
  await t.h.dispose();
  assert.equal(t.ed.freed, true);
  assert.equal(t.env.intervals.size, 0, 'save and poll timers stopped');
  assert.equal(t.env.frames.length, 0, 'frame loop stopped');
  assert.equal(t.refs.canvas.listeners.length, 0, 'canvas listeners removed');
  assert.ok(t.refs.tools.every(([b]) => b.listeners.length === 0), 'toolbar listeners removed');
  assert.deepEqual(t.srv.named(T.RELEASE), [{ binding: BINDING, mount: 1 }], 'released once');
  // Nothing more reaches it.
  t.ed.pending = '{"v":3}';
  t.env.every(300);
  await flush();
  assert.equal(t.srv.named(T.SAVE).length, 2);
});

test('a conflict, an unanswered save or a page too large for the panel is never shown as saved, and keeps the editor', async () => {
  // Conflict: the page changed elsewhere.
  const c = await readyEditor();
  c.srv.page.revision = '"elsewhere"';
  c.ed.pending = '{"mine":1}';
  c.env.every(300);
  await flush();
  assert.equal(c.shown.status.at(-1), 'Not saved: conflict');
  assert.ok(c.refs.conflict.classes.has('show'), 'the banner with its explicit discard');
  assert.equal(await c.h.drain(), 'conflict');
  assert.equal(c.ed.freed, false);
  assert.ok(!c.shown.status.includes('Saved'));
  // No answer (the host timed out): unknown, retried, never Saved.
  const u = await readyEditor();
  u.srv.override[T.SAVE] = () => ({ timeout: true });
  u.ed.pending = '{"mine":1}';
  u.env.every(300);
  await flush();
  assert.equal(u.shown.status.at(-1), 'Save failed (server unreachable); retrying');
  u.ed.pending = '{"mine":1}'; // the mock forgets on failure; the real editor keeps it dirty
  assert.equal(await u.h.drain(), 'unreachable');
  assert.ok(!u.shown.status.includes('Saved'));
  // Too large: refused here, unsent.
  const l = await readyEditor();
  l.ed.pending = 'x'.repeat(4 * 1024 * 1024 + 1);
  l.env.every(300);
  await flush();
  assert.equal(l.srv.named(T.SAVE).length, 0, 'not sent');
  assert.equal(l.shown.status.at(-1), 'Save failed; retrying');
});

test('a save is bounded as the whole request it would be (envelope, escaping and host allowance), not its arguments', async () => {
  const MAX = 4 * 1024 * 1024;
  const bytes = (s) => new TextEncoder().encode(s).length;
  const t = await readyEditor();
  // Escaping-heavy: the arguments alone fit, the request as sent (plus the allowance) doesn't.
  const quotes = '"'.repeat(MAX / 2 - 4096);
  const args = { binding: BINDING, page_id: 'p1', mount: 1, if_match: '"r1"', text: quotes };
  assert.ok(bytes(JSON.stringify(args)) < MAX, 'the arguments alone fit');
  t.ed.pending = quotes;
  t.env.every(300);
  await flush();
  assert.equal(t.srv.named(T.SAVE).length, 0, 'not sent');
  assert.equal(t.shown.status.at(-1), 'Save failed; retrying');
  // Plain text just under the arguments-only limit: not sent either.
  t.ed.pending = 'x'.repeat(MAX - 1024);
  t.env.every(300);
  await flush();
  assert.equal(t.srv.named(T.SAVE).length, 0);
  // Well within the whole-request limit: sent, and saved.
  const fits = 'x'.repeat(MAX - 64 * 1024);
  t.ed.pending = fits;
  t.env.every(300);
  await flush();
  assert.equal(t.srv.named(T.SAVE).length, 1);
  assert.equal(t.srv.page.text, fits);
  assert.equal(t.shown.status.at(-1), 'Saved');
});

test('a stale binding or released mount is refused by the server and never applied', async () => {
  const t = await readyEditor();
  t.srv.mounts.clear(); // another view closed or replaced the canvas
  t.ed.pending = '{"mine":1}';
  t.env.every(300);
  await flush();
  assert.equal(t.srv.page.text, '{"page":1}', 'nothing written');
  assert.equal(t.shown.status.at(-1), 'Save failed; retrying');
  t.env.every(1000);
  await flush();
  assert.ok(!t.ed.calls.some((c) => c.startsWith('reload')), 'a refused poll loads nothing');
});
