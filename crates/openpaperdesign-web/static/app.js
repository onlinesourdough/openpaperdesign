// Page glue for openpaperdesign-web: loads the active page of the served project and mounts the
// editor on it (editor.js: DOM input, toolbar, frame loop, saving and polling), with saves
// revision-checked over HTTP (If-Match). The editor lifecycle (gating, fallback, saving) is in
// session.js. The home view's in-panel editor mounts the same editor (panel.js).
import init, { start } from './openpaperdesign_web.js';
import { Session } from './session.js';
import { discardStep, frameStep, saveStep } from './page.js';
import { opdBrowserEnv, opdIsMac, opdMountEditor } from './editor.js';

// State for people and for automated checks (read-only from outside).
const state = { pageId: null };
window.__opd = state;

const $ = (id) => document.getElementById(id);
// Long text is cut with an ellipsis; the full text is in the tooltip.
const setText = (el, s) => { el.textContent = s; el.title = s; };
const status = (s, error = false) => { setText($('status'), s); $('status').classList.toggle('error', error); };
const renderer = (s, error = false) => { setText($('renderer'), s); $('renderer').classList.toggle('error', error); };

function token() {
  // The URL fragment carries the per-launch token. Keep it for reloads in this tab only and
  // take it out of the address bar.
  const fromHash = new URLSearchParams(location.hash.slice(1)).get('token');
  if (fromHash) {
    sessionStorage.setItem('opd-token', fromHash);
    history.replaceState(null, '', location.pathname + location.search);
  }
  return fromHash || sessionStorage.getItem('opd-token');
}

const TOKEN = token();

async function api(path, opts = {}) {
  const headers = new Headers(opts.headers || {});
  headers.set('Authorization', `Bearer ${TOKEN}`);
  return fetch(path, { ...opts, headers, cache: 'no-store' });
}

function fail(msg) {
  state.ready = false;
  state.error = msg;
  status(msg, true);
  renderer(`Not running: ${msg}`, true);
  console.error(msg);
}

async function main() {
  if (!TOKEN) return fail('Missing token: open the URL printed by openpaperdesign-serve.');
  await init();
  const pres = await api('/api/project');
  if (!pres.ok) return fail(`Could not load the project (${pres.status}).`);
  const project = await pres.json();
  const meta = project.pages.find((p) => p.id === project.active_page) || project.pages[0];
  const pageUrl = `/api/pages/${encodeURIComponent(meta.id)}`;
  const fetchPage = async () => {
    const res = await api(pageUrl);
    if (!res.ok) throw new Error(`Could not load the page (${res.status}).`);
    return { revision: res.headers.get('ETag'), text: await res.text() };
  };
  state.pageId = meta.id;
  $('title').textContent = `${project.name} — ${meta.name}`;
  document.title = `${project.name} — openpaperdesign`;

  const seed = crypto.getRandomValues(new Uint32Array(2));
  const forceWebgl = new URLSearchParams(location.search).get('backend') === 'webgl';
  const first = await fetchPage();
  const m = opdMountEditor({
    lib: { Session, frameStep, saveStep, discardStep },
    canvas: $('canvas'),
    status,
    renderer,
    conflictBox: $('conflict'),
    tools: [...document.querySelectorAll('[data-tool]')].map((b) => [b, b.dataset.tool]),
    actions: [...document.querySelectorAll('[data-action]')].map((b) => [b, b.dataset.action]),
    boot: (canvas, page, webgl) => {
      const r = canvas.getBoundingClientRect();
      return start(canvas, page.text, meta.id, meta.name, seed[0], seed[1], opdIsMac(navigator), webgl,
        r.width, r.height, devicePixelRatio);
    },
    transport: {
      fetchPage,
      put: async (snapshot, revision) => {
        const res = await api(pageUrl, {
          method: 'PUT', body: snapshot,
          headers: { 'If-Match': revision, 'Content-Type': 'application/json' },
        });
        return { ok: res.ok, status: res.status, revision: res.headers.get('ETag') };
      },
      revision: async () => {
        const res = await api(`${pageUrl}/revision`);
        if (!res.ok) throw new Error(`revision ${res.status}`);
        return res.headers.get('ETag');
      },
    },
    firstPage: first,
    forceWebgl,
    state,
    onReady: (session) => {
      window.__opdEditor = session.ed;
      // Tell this page's own server that the page loaded and the renderer is Ready, so the
      // home view can tell an actual load from a navigation request. It carries nothing else.
      api('/api/ready', { method: 'POST' }).catch(() => {});
    },
    log: (msg) => console.error(msg),
    env: opdBrowserEnv(window),
  });
  window.__opdSession = m.session;
  await m.started;
  window.__opdEditor = m.session.ed;
  window.addEventListener('beforeunload', (e) => {
    if (m.session.ed && m.session.ed.dirty()) e.preventDefault();
  });
}

main().catch((e) => fail(`Failed to start: ${e && e.message ? e.message : e}`));
