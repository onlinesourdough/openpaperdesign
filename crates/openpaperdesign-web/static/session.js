// The page's editor lifecycle: startup, WebGPU-to-WebGL fallback, input gating and saving.
// No DOM or wasm specifics: everything it touches is passed in, so it is tested in plain Node
// (crates/openpaperdesign-web/tests/session.test.mjs).
//
// Rules:
// - Editing input, toolbar actions and saves only reach the editor once it is Ready (validated,
//   a frame presented, no GPU problem) and no fallback is under way. A fallback can only happen
//   before Ready, so no edit made in this tab can be lost or resurrected by it.
// - A fallback waits for a save in flight, then reopens the page as the server has it now
//   (content and revision), on a fresh canvas with its own resize observer.
// - A save's answer is only applied to the editor that took the snapshot.
// - Changes made elsewhere (an agent over MCP, another tab) are polled for. They replace the
//   page in place only when this tab has nothing unsaved (`can_reload`). Otherwise nothing is
//   reloaded: this tab's next save is based on an older revision, the server refuses it (409,
//   nothing written), and the unsaved state stays on screen until the person explicitly
//   discards it (`discard`), which loads the server's version in place. Editing is suspended
//   while that load is under way, so no edit can be made that it would then replace.
// - Leaving (`drain`) first stops input and polling, waits for a save in flight and saves what
//   is left; it reports 'clean' only when the editor has nothing unsaved and no gesture or text
//   edit under way. Anything else (a conflict, a failed or unanswered save, an unfinished edit)
//   keeps the editor; `resume` gives it back to the person.
// - `dispose` ends the session for good, at any point: every later step of a boot, fallback,
//   poll, discard or save finds it disposed and does nothing more. An editor that finishes
//   booting afterwards is freed at once: no observer, canvas, Ready or save comes from it.

export class Session {
  /**
   * deps: {
   *   boot(canvas, page, webgl) -> Promise<editor>   page = { text, revision }
   *   fetchPage() -> Promise<page>                    the page as the server has it now
   *   replaceCanvas(old) -> canvas                    a fresh canvas in place of `old`
   *   observeResize(canvas, onResize) -> { disconnect() }
   *   put(snapshot, revision) -> Promise<{ ok, status, revision }>
   *   revision() -> Promise<string>                   the page's current revision (polling)
   *   log(message)                                    console.error
   * }
   */
  constructor(deps, state = {}) {
    this.deps = deps;
    this.state = Object.assign(state, {
      ready: false, error: null, fallback: null, backend: null, revision: null,
      saves: 0, saving: false, conflict: false, switching: false,
      reloads: 0, behind: false, discarding: false,
    });
    this.ed = null;
    this.canvas = null;
    this.observer = null;
    this.forceWebgl = false;
    this.errorReported = false;
    // Bumped whenever something that can change the editor's page starts (a save, a poll, a
    // discard): an answer only applies if nothing started after its own request.
    this.generation = 0;
    this.polling = false;
    this.saveDone = Promise.resolve();
    this.discardDone = Promise.resolve();
    // Set by `drain` (no input or polling while leaving) and `dispose` (for good).
    this.closing = false;
    this.disposed = false;
    // Once Ready, edits were possible: a later error (which clears `ready`) can't make an
    // editor with unsaved work look clean (`drain`).
    this.wasReady = false;
  }

  /** May the user edit right now? */
  editable() {
    const s = this.state;
    return Boolean(this.ed) && !this.disposed && !this.closing && s.ready && !s.switching
      && !s.error && !s.discarding;
  }

  /** Run `fn(editor)` only when editing is allowed (input, toolbar). */
  edit(fn) {
    if (!this.editable()) return undefined;
    return fn(this.ed);
  }

  /** Run `fn(editor)` when there is an editor that isn't being replaced (resize, frames). */
  withEditor(fn) {
    if (!this.ed || this.disposed || this.state.switching) return undefined;
    return fn(this.ed);
  }

  async start(canvas, page, forceWebgl) {
    this.canvas = canvas;
    this.forceWebgl = forceWebgl;
    this.state.revision = page.revision;
    try {
      await this.open(page, forceWebgl);
    } catch (e) {
      if (this.disposed) return;
      if (forceWebgl) throw e;
      await this.fallBack(e && e.message ? e.message : String(e));
    }
  }

  async open(page, webgl) {
    const ed = await this.deps.boot(this.canvas, page, webgl);
    if (this.disposed) {
      // Left while it booted: this editor never becomes the session's.
      ed.free();
      return;
    }
    this.ed = ed;
    this.state.revision = page.revision;
    this.state.backend = ed.backend();
    this.observer = this.deps.observeResize(this.canvas, () => this.resized());
  }

  resized() {
    this.withEditor((ed) => this.deps.resize && this.deps.resize(ed, this.canvas));
  }

  /** WebGPU failed before Ready: reopen with WebGL2 on a fresh canvas, visibly. */
  async fallBack(reason) {
    const s = this.state;
    s.switching = true;
    try {
      this.deps.log(`WebGPU failed, falling back to WebGL: ${reason}`);
      s.fallback = reason;
      await this.saveDone;
      if (this.disposed) return;
      const page = await this.deps.fetchPage();
      if (this.disposed) return;
      if (this.observer) { this.observer.disconnect(); this.observer = null; }
      if (this.ed) { this.ed.free(); this.ed = null; }
      this.canvas = this.deps.replaceCanvas(this.canvas);
      await this.open(page, true);
    } finally {
      s.switching = false;
    }
  }

  /**
   * One animation frame. Returns 'ready' the first time the editor becomes Ready, 'error'
   * once when a problem has ended editing (including a fallback that failed asynchronously),
   * 'stopped' on every frame after that, 'fallback' when a fallback was started, else
   * undefined.
   */
  frame(t) {
    const s = this.state;
    if (this.disposed) return 'stopped';
    if (s.error) {
      if (this.errorReported) return 'stopped';
      this.errorReported = true;
      return 'error';
    }
    if (s.switching || !this.ed) return undefined;
    this.ed.frame(t);
    const gpuError = this.ed.gpu_error();
    if (gpuError) {
      if (!s.ready && !this.forceWebgl && !s.fallback) {
        this.fallBack(gpuError).catch((e) => this.fail(`Renderer failed: ${e && e.message ? e.message : e}`));
        return 'fallback';
      }
      this.fail(`Renderer error: ${gpuError}`);
      this.errorReported = true;
      return 'error';
    }
    if (!s.ready && this.ed.ready()) {
      s.ready = true;
      this.wasReady = true;
      return 'ready';
    }
    return undefined;
  }

  fail(msg) {
    if (this.disposed) return;
    this.state.ready = false;
    this.state.error = msg;
    this.deps.log(msg);
  }

  /**
   * Look for a newer revision on the server, only while the editor is clean. Returns
   * 'reloaded' when the page was replaced in place, 'behind' when it changed but this tab got
   * unsaved work while the request was under way (its next save will be a conflict), else
   * undefined.
   */
  async poll() {
    const s = this.state;
    // Only a clean editor polls: with unsaved work, a gesture or a text edit nothing could be
    // loaded anyway, so no request is made.
    if (this.polling || !this.editable() || s.saving || s.conflict || !this.ed.can_reload()) {
      return undefined;
    }
    this.polling = true;
    try {
      return await this.pollOnce();
    } finally {
      this.polling = false;
    }
  }

  async pollOnce() {
    const s = this.state;
    const gen = ++this.generation;
    const ed = this.ed;
    // Apply an answer only to the editor that asked, if no save, discard or other load
    // started since this request, and nothing is unsaved.
    const current = () => this.ed === ed && gen === this.generation && this.editable()
      && !s.saving && !s.conflict;
    const rev = await this.deps.revision();
    if (!current()) return undefined;
    if (rev === s.revision) {
      s.behind = false;
      return undefined;
    }
    if (!ed.can_reload()) {
      s.behind = true;
      return 'behind';
    }
    const page = await this.deps.fetchPage();
    if (!current() || !ed.can_reload()) {
      s.behind = true;
      return 'behind';
    }
    ed.reload(page.text);
    s.revision = page.revision;
    s.reloads += 1;
    s.behind = false;
    return 'reloaded';
  }

  /**
   * The person chose "Reload (discard my unsaved change)": load the page as the server has it
   * now, in place (no navigation, so no leave-page guard), and resume saving and polling.
   * Returns 'discarded', or undefined when there is nothing to do or the answer is stale.
   */
  async discard() {
    const s = this.state;
    const ed = this.ed;
    if (!ed || s.switching || s.error || s.saving || s.discarding) return undefined;
    const gen = ++this.generation;
    // No input, toolbar action or save reaches the editor until this is done.
    s.discarding = true;
    let done;
    this.discardDone = new Promise((r) => { done = r; });
    try {
      const page = await this.deps.fetchPage();
      // Apply only to the editor that asked, and only if nothing else started meanwhile.
      if (this.ed !== ed || gen !== this.generation || s.switching || s.error || s.saving) {
        return undefined;
      }
      ed.reload(page.text);
      s.revision = page.revision;
      s.conflict = false;
      s.behind = false;
      s.reloads += 1;
      return 'discarded';
    } finally {
      s.discarding = false;
      done();
    }
  }

  /** Save the editor's pending change, if any. Returns what happened, for the status line. */
  async save() {
    if (!this.editable()) return undefined;
    return this.saveNow();
  }

  /** `save`, also while leaving (`drain`): never after `dispose`, an error or before Ready. */
  async saveNow() {
    const s = this.state;
    if (!this.ed || this.disposed || !s.ready || s.switching || s.error || s.discarding
      || s.saving || s.conflict) {
      return undefined;
    }
    const ed = this.ed;
    const snapshot = ed.save_snapshot();
    if (snapshot === undefined) return undefined;
    s.saving = true;
    this.generation += 1;
    let done;
    this.saveDone = new Promise((r) => { done = r; });
    try {
      const res = await this.deps.put(snapshot, s.revision);
      if (this.ed !== ed) return 'stale';
      if (res.ok) {
        s.saves += 1;
        ed.save_succeeded();
        s.revision = res.revision;
        return 'saved';
      }
      ed.save_failed();
      if (res.status === 409) {
        s.conflict = true;
        return 'conflict';
      }
      return 'failed';
    } catch {
      if (this.ed === ed) ed.save_failed();
      return 'unreachable';
    } finally {
      s.saving = false;
      done();
    }
  }

  /**
   * Leaving the editor (Back, Close, the view going away): stop input and polling, wait for a
   * save or discard under way, then save any change that is left. Returns 'clean' only when
   * nothing unsaved remains and no gesture or text edit is under way (`can_reload`); a missing
   * snapshot alone doesn't mean clean. Otherwise the reason: 'conflict', 'failed',
   * 'unreachable' (no answer: it may or may not have been written), 'busy' (an edit still
   * under way), 'error' (the renderer stopped with unsaved changes) or 'stale'. Input stays
   * stopped until `resume`.
   */
  async drain() {
    const s = this.state;
    this.closing = true;
    // A poll or discard answer that arrives now no longer applies.
    this.generation += 1;
    await this.saveDone;
    await this.discardDone;
    // Edits are only possible once Ready: an editor that never got there has none. One that
    // was Ready and then stopped (an error clears `ready`) is judged by what it still holds.
    if (this.disposed || !this.ed || !this.wasReady) return 'clean';
    for (let i = 0; i < 2; i++) {
      if (s.conflict) return 'conflict';
      if (s.error || !s.ready) return this.ed.can_reload() ? 'clean' : 'error';
      const r = await this.saveNow();
      if (this.disposed || !this.ed) return 'stale';
      if (r === undefined) break;
      if (r !== 'saved') return r;
    }
    return this.ed.can_reload() ? 'clean' : 'busy';
  }

  /** After a `drain` that didn't come out clean: editing (and polling) go on. */
  resume() {
    if (!this.disposed) this.closing = false;
  }

  /**
   * End this session for good: no input, frame, poll, save or answer reaches an editor any
   * more, and the editor and its resize observer are released, once. An editor still booting
   * is freed as soon as it arrives (`open`). Unsaved changes are not saved by this: `drain`
   * first.
   */
  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    this.closing = true;
    this.generation += 1;
    this.state.ready = false;
    if (this.observer) { this.observer.disconnect(); this.observer = null; }
    const ed = this.ed;
    this.ed = null;
    if (ed) ed.free();
  }
}
