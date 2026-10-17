// The editor mount shared by the browser page (app.js) and the home view's in-panel editor
// (panel.js): DOM input, the toolbar, the frame loop, saving and polling for one Session, all
// scoped to that editor's own canvas and buttons, and one disposal that undoes all of it.
// It imports nothing: the session, the page steps, the renderer's boot, the page transport and
// the browser environment are passed in, so the same code runs as a file (app.js imports it)
// and inlined in the home view's one module (where the launcher assembles it).

export const OPD_SAVE_POLL_MS = 300;
// How often to look for changes made elsewhere (an agent over MCP, another tab or view).
export const OPD_REVISION_POLL_MS = 1000;
// The only toolbar actions an editor button may call.
const OPD_ACTIONS = ['undo', 'redo', 'delete'];

/** The browser environment the mount uses (timers, frames, observers), from a window. */
export function opdBrowserEnv(win) {
  return {
    setInterval: (fn, ms) => win.setInterval(fn, ms),
    clearInterval: (id) => win.clearInterval(id),
    requestAnimationFrame: (fn) => win.requestAnimationFrame(fn),
    cancelAnimationFrame: (id) => win.cancelAnimationFrame(id),
    ResizeObserver: win.ResizeObserver,
    AbortController: win.AbortController,
    devicePixelRatio: () => win.devicePixelRatio,
  };
}

/** Whether the keyboard shortcuts are the Mac ones. */
export function opdIsMac(nav) {
  const p = (nav && ((nav.userAgentData && nav.userAgentData.platform) || nav.platform)) || '';
  return /mac|iphone|ipad/i.test(p);
}

/**
 * Mount one editor. opts: {
 *   lib: { Session, frameStep, saveStep, discardStep },
 *   canvas,                          the canvas element (inside the editor's own root)
 *   status(text, error), renderer(text, error), conflictBox
 *   tools: [[button, tool]], actions: [[button, action]]
 *   boot(canvas, page, webgl) -> Promise<editor>
 *   transport: { fetchPage(), put(snapshot, revision), revision() }
 *   firstPage: { text, revision }, forceWebgl, state
 *   onReady(session)                 the renderer is Ready (once per mount)
 *   log(message), env                (see opdBrowserEnv)
 * }
 * Returns { session, started, drain(), resume(), dispose() }. `started` settles once the
 * renderer has booted (or failed); Ready comes later, from the frame loop.
 */
export function opdMountEditor(opts) {
  const { lib, transport, env } = opts;
  const abort = new env.AbortController();
  const signal = abort.signal;
  let disposed = false;
  let raf = 0;
  const timers = [];
  // The pointer this editor captured, released on disposal.
  let capture = null;
  const rectOf = (c) => c.getBoundingClientRect();
  // Every listener goes away with one abort.
  const on = (el, type, fn, o = {}) => el.addEventListener(type, fn, Object.assign({}, o, { signal }));

  const session = new lib.Session({
    boot: opts.boot,
    fetchPage: transport.fetchPage,
    replaceCanvas: (old) => {
      const fresh = old.cloneNode(false);
      old.replaceWith(fresh);
      wire(fresh);
      return fresh;
    },
    observeResize: (canvas, onResize) => {
      const ro = new env.ResizeObserver(onResize);
      ro.observe(canvas);
      return ro;
    },
    resize: (ed, canvas) => {
      const r = rectOf(canvas);
      ed.resize(r.width, r.height, env.devicePixelRatio());
    },
    put: transport.put,
    revision: transport.revision,
    log: opts.log,
  }, opts.state || {});

  // ---- input: only reaches the editor while editing is allowed (see session.js).
  const mods = (ed, e) => ed.modifiers(e.shiftKey, e.ctrlKey, e.altKey, e.metaKey);
  const local = (c, e) => { const r = rectOf(c); return [e.clientX - r.left, e.clientY - r.top]; };
  function wire(c) {
    on(c, 'pointerdown', (e) => {
      e.preventDefault();
      session.edit((ed) => {
        c.focus();
        c.setPointerCapture(e.pointerId);
        capture = [c, e.pointerId];
        mods(ed, e);
        ed.pointer_down(...local(c, e), e.button, e.timeStamp);
      });
    });
    on(c, 'pointermove', (e) => session.edit((ed) => {
      mods(ed, e);
      ed.pointer_move(...local(c, e), e.timeStamp);
    }));
    const up = (e) => {
      if (capture && capture[1] === e.pointerId) capture = null;
      session.edit((ed) => { mods(ed, e); ed.pointer_up(...local(c, e), e.button, e.timeStamp); });
    };
    on(c, 'pointerup', up);
    on(c, 'pointercancel', up);
    on(c, 'contextmenu', (e) => e.preventDefault());
    on(c, 'wheel', (e) => {
      e.preventDefault();
      session.edit((ed) => {
        mods(ed, e);
        const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? rectOf(c).height : 1;
        ed.wheel(e.deltaX * k, e.deltaY * k, ...local(c, e));
      });
    }, { passive: false });
    // The canvas's keys are the editor's: the view around it (where Escape means Back) never
    // sees them.
    on(c, 'keydown', (e) => {
      e.stopPropagation();
      if (session.edit((ed) => { mods(ed, e); return ed.key_down(e.key, e.code, e.repeat); })) {
        e.preventDefault();
      }
    });
    on(c, 'keyup', (e) => {
      e.stopPropagation();
      session.edit((ed) => { mods(ed, e); ed.key_up(e.key, e.code); });
    });
    on(c, 'blur', () => session.withEditor((ed) => ed.modifiers(false, false, false, false)));
  }

  // ---- toolbar
  const tools = opts.tools || [];
  for (const [b, tool] of tools) {
    on(b, 'click', () => session.edit((ed) => { ed.set_tool(tool); session.canvas.focus(); }));
  }
  for (const [b, action] of opts.actions || []) {
    if (!OPD_ACTIONS.includes(action)) continue;
    on(b, 'click', () => session.edit((ed) => { ed[action](); session.canvas.focus(); }));
  }
  const syncToolbar = () => session.withEditor((ed) => {
    const t = ed.tool();
    for (const [b, tool] of tools) b.setAttribute('aria-pressed', String(tool === t));
  });

  const showRenderer = () => {
    const s = session.state;
    opts.renderer(s.fallback
      ? `WebGPU failed: ${s.fallback} — using WebGL (${s.backend})`
      : `Renderer: ${s.backend}`, Boolean(s.fallback));
  };
  // What the editor shows (see page.js). Nothing is shown any more once it is disposed.
  const ui = {
    // Ready only once the renderer validated, presented a frame, and has no GPU problem.
    ready: () => {
      if (disposed) return;
      showRenderer();
      opts.status('Ready');
      session.canvas.focus();
      if (opts.onReady) opts.onReady(session);
    },
    // Terminal: shown once, in red, and the frame loop stops.
    fatal: (msg) => {
      if (disposed) return;
      opts.status(msg, true);
      opts.renderer(`Renderer failed: ${msg}`, true);
      opts.log(msg);
    },
    status: (text) => { if (!disposed) opts.status(text); },
    tick: syncToolbar,
    // The conflict banner. The unsaved state stays on screen until the person discards it;
    // the button reloads the server's version in place.
    conflict: () => {
      if (disposed) return;
      const c = opts.conflictBox;
      c.replaceChildren();
      c.append('This page was changed elsewhere (for example by an agent) since your unsaved '
        + 'change. Nothing was overwritten, and your change is not saved. ');
      const b = c.ownerDocument.createElement('button');
      b.type = 'button';
      b.textContent = 'Reload (discard my unsaved change)';
      on(b, 'click', () => lib.discardStep(session, ui));
      c.append(b);
      c.classList.add('show');
    },
    hideConflict: () => {
      const c = opts.conflictBox;
      c.classList.remove('show');
      c.replaceChildren();
    },
  };

  wire(opts.canvas);
  const started = (async () => {
    await session.start(opts.canvas, opts.firstPage, Boolean(opts.forceWebgl));
    if (disposed) return false;
    showRenderer();
    opts.status('Starting renderer…');
    // ---- saving: one request at a time, always with the revision last seen.
    timers.push(env.setInterval(() => { if (!disposed) lib.saveStep(session, ui); }, OPD_SAVE_POLL_MS));
    // ---- changes made elsewhere: reload in place when nothing is unsaved here.
    timers.push(env.setInterval(async () => {
      if (disposed) return;
      try {
        const r = await session.poll();
        if (disposed || session.state.error) return; // a terminal error stays on screen
        if (r === 'reloaded') opts.status('Updated with changes made elsewhere');
        else if (r === 'behind') opts.status('Changed elsewhere; saving your edit will report a conflict');
      } catch {
        // Unreachable; the save loop reports that.
      }
    }, OPD_REVISION_POLL_MS));
    const frame = (t) => {
      raf = 0;
      if (disposed) return;
      if (lib.frameStep(session, ui, t)) raf = env.requestAnimationFrame(frame);
    };
    raf = env.requestAnimationFrame(frame);
    return true;
  })();

  /** Undo the whole mount, once: listeners, timers, the frame loop, the pointer, the editor. */
  function dispose() {
    if (disposed) return;
    disposed = true;
    abort.abort();
    for (const t of timers) env.clearInterval(t);
    timers.length = 0;
    if (raf) env.cancelAnimationFrame(raf);
    raf = 0;
    if (capture) {
      const [c, id] = capture;
      capture = null;
      try {
        if (c.hasPointerCapture && c.hasPointerCapture(id)) c.releasePointerCapture(id);
      } catch {
        // The canvas is already gone.
      }
    }
    session.dispose();
  }

  return {
    session,
    started,
    drain: () => session.drain(),
    resume: () => session.resume(),
    dispose,
    get disposed() { return disposed; },
  };
}
