// The home view's in-panel editor: the same renderer (WebAssembly), Session and editor mount
// as the browser page, talking to the app library's server through the view's own MCP Apps
// bridge instead of HTTP. It only runs inlined in the home view's one module, after the
// wasm-bindgen glue, session.js, page.js and editor.js (openpaperdesign-mcp assembles them in
// that order; see `panel_module` in launcher.rs), whose names it uses: `__wbg_init`, `start`,
// `Session`, `frameStep`, `saveStep`, `discardStep`, `opdMountEditor`, `opdBrowserEnv`,
// `opdIsMac` and `OPD_WASM_IDENTITY`. Nothing is fetched by URL, evaluated or stored.
//
// - The renderer comes from the server in fixed, numbered parts (`openpaperdesign_app_editor_
//   asset`), one after another. Every part must name the same renderer (identity, size, part
//   count) as this view's module was built with, come in order and have exactly its length;
//   anything else stops the transfer with the exact reason. The glue's own init then compiles
//   the bytes. Whether the host allows that (its content security policy) and gives the view
//   WebGL2 or WebGPU is up to the host: a refusal is shown as it came.
// - Page calls name the view's binding, then the page id and mount number its first read
//   returned. The server compares the binding and mount before it reads or writes anything
//   and checks the page id is a page of the canvas; keeping to that one page is this
//   editor's own rule (the server doesn't tie a mount to a page). Saves are
//   revision-checked exactly as the browser's; a save whose answer never came is treated as
//   unknown (it is retried, and a conflict shows if it was written meanwhile), never as saved.

const OPD_PANEL = Object.freeze({
  ASSET: 'openpaperdesign_app_editor_asset',
  PAGE: 'openpaperdesign_app_page',
  REVISION: 'openpaperdesign_app_page_revision',
  SAVE: 'openpaperdesign_app_page_save',
  READY: 'openpaperdesign_app_ready',
  RELEASE: 'openpaperdesign_app_page_release',
  ASSET_NAME: 'openpaperdesign_web_bg.wasm',
  PART_BYTES: 262144,
  MAX_PARTS: 64,
  MAX_BYTES: 16 * 1024 * 1024,
  // The server's own limit for a page message (`MAX_PANEL_MESSAGE`), and the allowance within
  // it for what a host adds around a forwarded request (`HOST_OVERHEAD`).
  MAX_MESSAGE: 4 * 1024 * 1024,
  HOST_OVERHEAD: 16 * 1024,
});
// Why a transfer or load stopped when the person left first.
const OPD_LEFT = 'left';

/**
 * opts: {
 *   rpc(method, params, wait) -> Promise<reply>   the view's MCP Apps request (never a message)
 *   identity        the renderer this module was built with (default OPD_WASM_IDENTITY)
 *   engine, lib, mount, env, nav                   defaults: this module's glue, session,
 *                                                  page steps, editor mount and window
 * }
 * Returns { open({ binding, refs, onStatus, onReady }) -> handle }, where handle is
 * { started, drain(), resume(), dispose() }.
 */
export function opdCreatePanel(opts) {
  const P = OPD_PANEL;
  const rpc = opts.rpc;
  const identity = opts.identity !== undefined ? opts.identity : OPD_WASM_IDENTITY;
  const engine = opts.engine || {
    init: (bytes) => __wbg_init({ module_or_path: bytes }),
    start,
  };
  const lib = opts.lib || { Session, frameStep, saveStep, discardStep };
  const mountEditor = opts.mount || opdMountEditor;
  const env = opts.env || opdBrowserEnv(window);
  const mac = opdIsMac(opts.nav !== undefined ? opts.nav : navigator);
  // Opened editors that haven't been left: the renderer transfer goes on while one wants it.
  const wanting = new Set();
  let loaded = null;

  // One tools/call: { ok: value }, { refused: { error, status, revision } } or { fail: why }.
  // `unknown` marks an answer that never came (the call may have run).
  async function call(name, args, wait = 60000) {
    const r = await rpc('tools/call', { name, arguments: args }, wait);
    if (r.timeout) return { fail: 'Codex didn\'t answer', unknown: true };
    if (r.error) {
      const code = typeof r.error.code === 'number' ? ' (code ' + r.error.code + ')' : '';
      return { fail: 'Codex didn\'t run the request' + code, unknown: true };
    }
    const c = r.result && Array.isArray(r.result.content)
      ? r.result.content.find((b) => b && b.type === 'text') : null;
    const text = c && typeof c.text === 'string' ? c.text : '';
    let v = null;
    try { v = JSON.parse(text); } catch { v = null; }
    if (r.result && r.result.isError) {
      return { refused: v && typeof v === 'object' ? v : { error: text || 'refused', status: 0 } };
    }
    if (v && typeof v === 'object') return { ok: v };
    return { fail: 'Codex sent an answer OpenPaperDesign can\'t read' };
  }
  const describe = (r) => (r.refused ? String(r.refused.error || 'refused') : String(r.fail));

  const decode = (data) => {
    const bin = atob(data);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  };
  const utf8Length = (s) => new TextEncoder().encode(s).length;
  // A tools/call request as this view sends it (the widest id), plus the host allowance: the
  // same shape and numbers as the server's `request_bytes`.
  const requestBytes = (name, args) => utf8Length(JSON.stringify({
    jsonrpc: '2.0', id: Number.MAX_SAFE_INTEGER, method: 'tools/call', params: { name, arguments: args },
  })) + P.HOST_OVERHEAD;

  // The renderer's bytes, part after part, each checked against the first and against this
  // module's identity. Stops as soon as no opened editor wants it any more.
  async function transfer() {
    const live = () => wanting.size > 0;
    let parts = null;
    let total = 0;
    const chunks = [];
    for (let i = 0; parts === null || i < parts; i++) {
      if (!live()) throw new Error(OPD_LEFT);
      const r = await call(P.ASSET, { part: i });
      if (!live()) throw new Error(OPD_LEFT);
      if (!r.ok) throw new Error('The renderer could not be sent (part ' + i + '): ' + describe(r));
      const v = r.ok;
      if (v.asset !== P.ASSET_NAME || typeof identity !== 'string' || v.identity !== identity) {
        throw new Error('Renderer part ' + i + ' belongs to another build of OpenPaperDesign. Close and reopen OpenPaperDesign.');
      }
      if (!Number.isInteger(v.bytes) || v.bytes < 1 || v.bytes > P.MAX_BYTES
        || !Number.isInteger(v.parts) || v.parts > P.MAX_PARTS || v.parts !== Math.ceil(v.bytes / P.PART_BYTES)) {
        throw new Error('Renderer part ' + i + ' has an unexpected size; nothing was loaded.');
      }
      if (parts === null) {
        parts = v.parts;
        total = v.bytes;
      } else if (v.parts !== parts || v.bytes !== total) {
        throw new Error('Renderer part ' + i + ' doesn\'t match the earlier parts; nothing was loaded.');
      }
      const want = i < parts - 1 ? P.PART_BYTES : total - P.PART_BYTES * (parts - 1);
      if (v.part !== i || v.offset !== i * P.PART_BYTES || v.part_bytes !== want || typeof v.data !== 'string') {
        throw new Error('Renderer part ' + i + ' came out of order or incomplete; nothing was loaded.');
      }
      let bytes;
      try { bytes = decode(v.data); } catch { bytes = null; }
      if (!bytes || bytes.length !== want) {
        throw new Error('Renderer part ' + i + ' isn\'t the length it says; nothing was loaded.');
      }
      chunks.push(bytes);
    }
    const out = new Uint8Array(total);
    let at = 0;
    for (const c of chunks) { out.set(c, at); at += c.length; }
    return out;
  }

  // The renderer, transferred and compiled once per view; a failed or abandoned attempt can
  // be tried again by the next opened editor.
  function renderer() {
    if (!loaded) {
      loaded = (async () => { await engine.init(await transfer()); })();
      loaded.catch(() => { loaded = null; });
    }
    return loaded;
  }

  /** Open the canvas `binding` in this view's editor (`refs`: its canvas, status, buttons). */
  function open({ binding, refs, onStatus, onReady }) {
    const h = { left: false, mounted: null, mount: null, released: false };
    wanting.add(h);
    const live = () => !h.left;
    const say = (text, error) => { if (live() && onStatus) onStatus(text, Boolean(error)); };
    const release = async () => {
      if (h.mount === null || h.released) return;
      h.released = true;
      await call(P.RELEASE, { binding, mount: h.mount }, 5000).catch(() => {});
    };
    h.started = (async () => {
      say('Loading the editor…');
      try {
        await renderer();
      } catch (e) {
        if (!live()) return 'left';
        throw e;
      }
      if (!live()) return 'left';
      say('Opening the page…');
      const first = await call(P.PAGE, { binding });
      if (first.ok && Number.isInteger(first.ok.mount)) h.mount = first.ok.mount;
      if (!live()) { await release(); return 'left'; }
      if (!first.ok) throw new Error(describe(first));
      const v = first.ok;
      if (v.binding !== binding || !v.page || typeof v.page.id !== 'string' || typeof v.page.name !== 'string'
        || typeof v.text !== 'string' || typeof v.revision !== 'string' || !Number.isInteger(v.mount) || v.mount < 1) {
        await release();
        throw new Error('Codex answered with another canvas or page than the one shown here, so it wasn\'t opened.');
      }
      const pageId = v.page.id;
      const pageName = v.page.name;
      // Every later call names exactly this binding, page and mount.
      const ids = { binding, page_id: pageId, mount: v.mount };
      const fetchPage = async () => {
        const r = await call(P.PAGE, ids);
        if (!r.ok) throw new Error(describe(r));
        const p = r.ok;
        if (p.binding !== binding || !p.page || p.page.id !== pageId || typeof p.text !== 'string'
          || typeof p.revision !== 'string') {
          throw new Error('Codex answered with another page than this one.');
        }
        return { text: p.text, revision: p.revision };
      };
      const put = async (snapshot, revision) => {
        const args = Object.assign({}, ids, { if_match: revision, text: snapshot });
        // Too large for the panel as the request it would be: refused here, unsent; the
        // change stays unsaved on screen.
        if (requestBytes(P.SAVE, args) > P.MAX_MESSAGE) return { ok: false, status: 413, revision: null };
        const r = await call(P.SAVE, args, 130000);
        if (r.ok) {
          const s = r.ok;
          if (s.saved !== true || s.binding !== binding || s.page_id !== pageId || typeof s.revision !== 'string') {
            throw new Error('unreadable save answer');
          }
          return { ok: true, status: 200, revision: s.revision };
        }
        if (r.refused) {
          const st = Number.isInteger(r.refused.status) ? r.refused.status : 0;
          return { ok: false, status: st, revision: typeof r.refused.revision === 'string' ? r.refused.revision : null };
        }
        // No answer: it may or may not have been written. Never shown as saved.
        throw new Error(r.fail);
      };
      const revision = async () => {
        const r = await call(P.REVISION, ids);
        if (!r.ok || r.ok.page_id !== pageId || typeof r.ok.revision !== 'string') {
          throw new Error(r.ok ? 'Codex answered for another page.' : describe(r));
        }
        return r.ok.revision;
      };
      const seed = crypto.getRandomValues(new Uint32Array(2));
      h.mounted = mountEditor({
        lib,
        canvas: refs.canvas,
        status: refs.status,
        renderer: refs.renderer,
        conflictBox: refs.conflict,
        tools: refs.tools,
        actions: refs.actions,
        boot: (canvas, page, webgl) => {
          const r = canvas.getBoundingClientRect();
          return engine.start(canvas, page.text, pageId, pageName, seed[0], seed[1], mac, webgl,
            r.width, r.height, env.devicePixelRatio());
        },
        transport: { fetchPage, put, revision },
        firstPage: { text: v.text, revision: v.revision },
        forceWebgl: false,
        onReady: () => {
          if (!live()) return;
          // The server learns it too (as the browser editor's /api/ready); this view's own
          // Ready came from its own renderer, not from that.
          call(P.READY, { binding, mount: v.mount }).catch(() => {});
          if (onReady) onReady({ pageId, pageName });
        },
        log: (msg) => console.error(msg),
        env,
      });
      await h.mounted.started;
      return live() ? 'mounted' : 'left';
    })();
    // Leaving: whatever is left to save is saved first (see Session.drain).
    h.drain = async () => (h.mounted && !h.left ? h.mounted.drain() : 'clean');
    h.resume = () => { if (h.mounted && !h.left) h.mounted.resume(); };
    // For good: the mount is undone and the server told this editor is gone. Saves nothing.
    h.dispose = async () => {
      if (h.left) return;
      h.left = true;
      wanting.delete(h);
      if (h.mounted) h.mounted.dispose();
      await release();
    };
    return h;
  }

  return { open };
}
