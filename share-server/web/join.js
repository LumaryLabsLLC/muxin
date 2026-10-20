// Joins a Muxin share from the browser: the link's ticket (after the #)
// names the share, its secret and its certificate. This page connects to
// the sharing computer over WebRTC, refuses any other certificate, proves
// it has the link, then shows what's shared: a session, or a machine's
// sessions and screen. See src/share in Muxin.
"use strict";

(() => {
  // MORE: part of a frame too big for one message; the rest follows on its channel.
  const JSON_KIND = 10, DATA = 0, EXIT = 2, SIZE = 3, CLOSE = 11, MORE = 12;
  // The screen's frames: whole ones, input back, what changed (agent/src/desktop.rs).
  const FRAME = 20, INPUT = 21, TILES = 23;
  const $ = (id) => document.getElementById(id);
  const enc = new TextEncoder();
  const dec = new TextDecoder();

  // The ticket, out of the address bar (kept for this tab only, so a reload
  // works and a screenshot of the page doesn't show it).
  function readTicket() {
    let fragment = location.hash.slice(1);
    if (fragment) {
      try { sessionStorage.setItem("muxin-share", fragment); } catch (_) {}
      history.replaceState(null, "", location.pathname);
    } else {
      try { fragment = sessionStorage.getItem("muxin-share") || ""; } catch (_) {}
    }
    const [version, data] = fragment.split(".");
    if (version !== "1" || !data) return null;
    let bytes;
    try {
      const b64 = data.replace(/-/g, "+").replace(/_/g, "/");
      bytes = Uint8Array.from(atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4)), (c) => c.charCodeAt(0));
    } catch (_) { return null; }
    if (bytes.length !== 80) return null;
    return { fragment, id: bytes.slice(0, 16), secret: bytes.slice(16, 48), fingerprint: hex(bytes.slice(48, 80)) };
  }

  const hex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  const room = (id) => btoa(String.fromCharCode(...id)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

  // The one SHA-256 fingerprint in an SDP (null when none, or several differ).
  function fingerprint(sdp) {
    let found = null;
    for (const line of sdp.split(/\r?\n/)) {
      const m = /^a=fingerprint:(\S+) (.+)$/.exec(line.trim());
      if (!m) continue;
      if (m[1].toLowerCase() !== "sha-256") return null;
      const value = m[2].trim().replace(/:/g, "").toLowerCase();
      if (value.length !== 64 || (found && found !== value)) return null;
      found = value;
    }
    return found;
  }

  async function hmac(secret, text) {
    const key = await crypto.subtle.importKey("raw", secret, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
    return new Uint8Array(await crypto.subtle.sign("HMAC", key, enc.encode(text)));
  }

  function frame(channel, kind, payload) {
    const f = new Uint8Array(5 + payload.length);
    new DataView(f.buffer).setUint32(0, channel);
    f[4] = kind;
    f.set(payload, 5);
    return f;
  }

  const sections = ["start", "wait", "watch", "done"];
  function show(name) {
    for (const s of sections) $(s).hidden = s !== name;
  }

  function finish(title, sub) {
    state.over = true;
    if (state.pc) try { state.pc.close(); } catch (_) {}
    clearInterval(state.clock);
    $("session-actions").hidden = true;
    $("done-title").textContent = title;
    $("done-sub").textContent = sub || "";
    show("done");
  }

  const state = {
    over: false, pc: null, dc: null, term: null, canType: false, canControl: false, info: null, clock: 0, name: "",
    // Channels open (number → {kind: "session" | "desktop", name}), and the next one.
    channels: new Map(), next: 1,
    // A shared machine's sessions; what's showing ({kind, name}).
    sessions: [], showing: null, session: 0, desktop: 0,
  };

  function minutesLeft(endsAt) {
    const s = endsAt - Date.now() / 1000;
    if (s <= 0) return "Ending";
    if (s < 3600) return `${Math.max(1, Math.round(s / 60))} min left`;
    const h = Math.round(s / 3600);
    return `${h} hour${h === 1 ? "" : "s"} left`;
  }

  const agents = { claude: "Claude Code", codex: "Codex", opencode: "OpenCode", gemini: "Gemini CLI", amp: "Amp", pi: "Pi", shell: "Shell" };

  function showInfo(info) {
    state.info = info;
    const what = agents[info.agent] || "A session";
    const title = info.machine_share ? info.title : info.title || what;
    const sub = info.machine_share ? `A machine · shared by ${info.by}` : `${what} on ${info.machine} · shared by ${info.by}`;
    $("session-title").textContent = title;
    $("session-sub").textContent = sub;
    $("session").hidden = false;
    $("wait-title").textContent = `Join ${title}`;
    $("wait-sub").textContent = sub;
    const facts = [];
    if (info.machine_share) {
      facts.push(info.can_type ? "Its sessions: you can watch and type" : "Its sessions: you can watch");
      if (info.desktop === "view") facts.push("Its screen: you can watch");
      if (info.desktop === "control") facts.push(info.can_control ? "Its screen: you can watch and use it" : "Its screen: you can watch, and use it when they let you");
      if (info.ports.length) facts.push(`${info.ports.length} server${info.ports.length === 1 ? "" : "s"}: open them in the Muxin app`);
    } else {
      facts.push(info.can_type ? "You can watch and type" : "You can watch");
    }
    facts.push(`Ends in ${minutesLeft(info.ends_at).replace(" left", "")}`, `Joining as ${state.name}`);
    $("facts").replaceChildren(...facts.map((f) => Object.assign(document.createElement("li"), { textContent: f })));
    $("facts").hidden = false;
    $("code-help").textContent = `Read it to ${info.by} so they know it's you.`;
    clearInterval(state.clock);
    state.clock = setInterval(() => { $("time-left").textContent = minutesLeft(info.ends_at); }, 15000);
    $("time-left").textContent = minutesLeft(info.ends_at);
  }

  // The bar under what's showing, and the pill: what you can do there.
  function updateNote() {
    const by = state.info ? state.info.by : "they";
    const desktop = state.showing && state.showing.kind === "desktop";
    $("mode-pill").textContent = desktop ? (state.canControl ? "In control" : "Watching") : state.canType ? "Typing" : "Watching";
    $("term-note").textContent = desktop
      ? state.canControl ? `Your mouse and keys work on ${state.info.title}.` : `You're watching. Only ${by} can use it.`
      : state.canType ? "You can type. Keys go to their session." : `You're watching. Only ${by} can type.`;
    if (state.term) state.term.options.disableStdin = !state.canType;
    $("canvas").classList.toggle("control", state.canControl);
    $("kbd").hidden = !(desktop && state.canControl);
  }

  function makeTerminal() {
    const term = new window.Terminal({
      cols: 80, rows: 24, scrollback: 5000, convertEol: false, cursorBlink: true, disableStdin: true,
      fontFamily: 'ui-monospace, "SF Mono", Menlo, Consolas, monospace', fontSize: 13,
      theme: { background: "#1a1e3c", foreground: "#eeecfb", cursor: "#eeecfb", selectionBackground: "#3d3a7a" },
    });
    term.open($("term"));
    const keys = (bytes) => {
      if (state.canType && state.session && state.dc && state.dc.readyState === "open") state.dc.send(frame(state.session, DATA, bytes));
    };
    term.onData((data) => keys(enc.encode(data)));
    term.onBinary((data) => keys(Uint8Array.from(data, (c) => c.charCodeAt(0) & 255)));
    state.term = term;
    if (window.ResizeObserver) new ResizeObserver(fitText).observe($("term"));
  }

  // The session keeps the sharer's size: its text is sized to fit the width
  // here (13px at most; on a phone, 9px at least, and it scrolls sideways).
  function fitText() {
    const term = state.term;
    if (!term) return;
    const box = $("term");
    const width = box.clientWidth - 24;
    if (width <= 0) return;
    const size = Math.max(9, Math.min(13, Math.floor((width / (term.cols * 0.6)) * 2) / 2));
    if (term.options.fontSize !== size) term.options.fontSize = size;
  }

  function send(obj) {
    if (state.dc && state.dc.readyState === "open") state.dc.send(frame(0, JSON_KIND, enc.encode(JSON.stringify(obj))));
  }

  function openChannel(op, extra, kind, name) {
    const channel = state.next++;
    state.channels.set(channel, { kind, name });
    send(Object.assign({ op, channel }, extra));
    return channel;
  }

  function closeChannel(channel) {
    if (!channel || !state.channels.has(channel)) return;
    state.channels.delete(channel);
    if (state.dc && state.dc.readyState === "open") state.dc.send(frame(channel, CLOSE, new Uint8Array(0)));
  }

  // What the page shows: a session (in the one terminal) or the screen.
  function showSession(name) {
    if (state.showing && state.showing.kind === "session" && state.showing.name === name && state.session) return;
    closeChannel(state.session);
    state.showing = { kind: "session", name };
    state.term.reset();
    state.session = openChannel("watch", { session: name }, "session", name);
    closeScreen();
    $("quality-wrap").hidden = true;
    $("kbd").hidden = true;
    $("term").hidden = false;
    $("screen").hidden = true;
    $("empty").hidden = true;
    renderTabs();
    updateNote();
    state.term.focus();
  }

  function showDesktop() {
    state.showing = { kind: "desktop" };
    if (!state.desktop) {
      $("screen-wait").textContent = "Starting their screen…";
      $("screen-wait").hidden = false;
      state.desktop = openChannel("desktop", { quality: $("quality").value }, "desktop", "");
    }
    $("quality-wrap").hidden = false;
    $("term").hidden = true;
    $("screen").hidden = false;
    $("empty").hidden = true;
    renderTabs();
    updateNote();
    $("canvas").focus();
  }

  // The screen isn't showing: its stream stops (it starts again when shown).
  function closeScreen() {
    letGo();
    closeChannel(state.desktop);
    state.desktop = 0;
  }

  function showEmpty(text) {
    state.showing = null;
    closeScreen();
    $("quality-wrap").hidden = true;
    $("kbd").hidden = true;
    $("term").hidden = true;
    $("screen").hidden = true;
    $("empty").textContent = text;
    $("empty").hidden = false;
    renderTabs();
  }

  function tab(label, pressed, onClick, extra) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "tab" + (extra && extra.exited ? " exited" : "");
    b.setAttribute("aria-pressed", pressed ? "true" : "false");
    if (extra && extra.busy) b.append(Object.assign(document.createElement("i"), { className: "busy" }));
    b.append(Object.assign(document.createElement("span"), { textContent: label }));
    b.addEventListener("click", onClick);
    return b;
  }

  function renderTabs() {
    const info = state.info;
    if (!info || !info.machine_share) return;
    const items = state.sessions.map((s) => {
      const label = s.title || agents[s.agent] || s.name;
      const on = state.showing && state.showing.kind === "session" && state.showing.name === s.name;
      return tab(label, on, () => showSession(s.name), { busy: s.busy, exited: s.status === "exited" });
    });
    if (info.desktop !== "none") items.push(tab("Screen", state.showing && state.showing.kind === "desktop", showDesktop));
    if (info.ports.length) {
      items.push(Object.assign(document.createElement("span"), { className: "gap" }));
      items.push(Object.assign(document.createElement("span"), { className: "hint", textContent: `${info.ports.length} server${info.ports.length === 1 ? "" : "s"} shared: open them in the Muxin app` }));
    }
    $("tabs").replaceChildren(...items);
    $("tabs").hidden = false;
  }

  function admitted() {
    show("watch");
    $("session-actions").hidden = false;
    makeTerminal();
    if (state.info && state.info.machine_share) {
      showEmpty(`Looking at ${state.info.title}'s sessions…`);
    } else {
      showSession("");
    }
    updateNote();
  }

  // ---- The screen: patches painted as they come, in order; input back.

  const canvas = () => $("canvas");
  // What arrived and isn't painted yet. While a paint runs, newer screens
  // wait here; the next paint takes them all at once and skips the parts a
  // later one covers, so a slow device shows the newest screen, never one
  // that falls further and further behind.
  let waiting = [];
  let painting = false;

  function paint(kind, body) {
    waiting.push([kind, body.slice()]);
    if (!painting) paintWaiting();
  }

  // The patches of a TILES message: [x, y, w, h, jpeg].
  function patchesOf(copy) {
    const v = new DataView(copy.buffer);
    const n = v.getUint16(4), patches = [];
    let o = 6;
    for (let i = 0; i < n && o + 12 <= copy.length; i++) {
      const len = v.getUint32(o + 8);
      patches.push([v.getUint16(o), v.getUint16(o + 2), v.getUint16(o + 4), v.getUint16(o + 6), copy.subarray(o + 12, o + 12 + len)]);
      o += 12 + len;
    }
    return { w: v.getUint16(0), h: v.getUint16(2), patches };
  }

  async function paintWaiting() {
    painting = true;
    while (waiting.length) {
      const batch = waiting;
      waiting = [];
      const c = canvas(), ctx = c.getContext("2d");
      // A whole frame replaces everything before it.
      const lastFrame = batch.map(([k]) => k).lastIndexOf(FRAME);
      if (lastFrame >= 0) {
        const copy = batch[lastFrame][1];
        const img = copy.length > 4 ? await createImageBitmap(new Blob([copy.subarray(4)], { type: "image/jpeg" })).catch(() => null) : null;
        if (img) {
          if (c.width !== img.width || c.height !== img.height) { c.width = img.width; c.height = img.height; }
          ctx.drawImage(img, 0, 0);
          img.close();
        }
      }
      // Then the patches after it, in order, without those a later one covers.
      let size = null, patches = [];
      for (const [k, copy] of batch.slice(lastFrame + 1)) {
        if (k !== TILES) continue;
        const t = patchesOf(copy);
        if (!size || size.w !== t.w || size.h !== t.h) { size = t; patches = []; }
        const covers = (a, b) => a[0] <= b[0] && a[1] <= b[1] && a[0] + a[2] >= b[0] + b[2] && a[1] + a[3] >= b[1] + b[3];
        patches = patches.filter((old) => !t.patches.some((p) => covers(p, old)));
        patches.push(...t.patches);
      }
      if (size) {
        if (c.width !== size.w || c.height !== size.h) { c.width = size.w; c.height = size.h; }
        const images = await Promise.all(patches.map((p) => createImageBitmap(new Blob([p[4]], { type: "image/jpeg" })).catch(() => null)));
        images.forEach((img, i) => { if (img) { ctx.drawImage(img, patches[i][0], patches[i][1]); img.close(); } });
      }
      $("screen-wait").hidden = true;
    }
    painting = false;
  }

  function input(event) {
    if (!state.canControl || !state.desktop || !state.dc || state.dc.readyState !== "open") return;
    state.dc.send(frame(state.desktop, INPUT, enc.encode(JSON.stringify(event))));
  }

  // Where the pointer is, in the screen's own pixels.
  function at(e) {
    const c = canvas(), r = c.getBoundingClientRect();
    if (!r.width || !r.height) return null;
    const x = ((e.clientX - r.left) / r.width) * c.width, y = ((e.clientY - r.top) / r.height) * c.height;
    return x < 0 || y < 0 || x >= c.width || y >= c.height ? null : { x, y };
  }

  // X keysyms: the key's place for letters, digits and signs (Shift goes
  // separately), its name for the rest.
  const codes = { Minus: "-", Equal: "=", BracketLeft: "[", BracketRight: "]", Backslash: "\\", Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/", Backquote: "`", Space: " " };
  const named = {
    Enter: 0xff0d, Backspace: 0xff08, Tab: 0xff09, Escape: 0xff1b, Delete: 0xffff, Insert: 0xff63, Home: 0xff50, End: 0xff57,
    PageUp: 0xff55, PageDown: 0xff56, ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54,
    Shift: 0xffe1, Control: 0xffe3, Alt: 0xffe9, CapsLock: 0xffe5,
  };
  function keysym(e) {
    if (/^Key[A-Z]$/.test(e.code)) return e.code.charCodeAt(3) + 32;
    if (/^Digit[0-9]$/.test(e.code)) return e.code.charCodeAt(5);
    if (codes[e.code]) return codes[e.code].charCodeAt(0);
    // ⌘ is ⌘ on a Mac; elsewhere it works as Ctrl (copy, paste, ... as on a Mac).
    if (e.key === "Meta") return state.info && state.info.mac ? 0xffeb : 0xffe3;
    if (/^F([1-9]|1[0-2])$/.test(e.key)) return 0xffbe + Number(e.key.slice(1)) - 1;
    if (named[e.key]) return named[e.key];
    return e.key.length === 1 && e.key.charCodeAt(0) <= 0xff ? e.key.charCodeAt(0) : null;
  }

  const held = { keys: new Set(), buttons: new Set() };
  function letGo() {
    for (const k of held.keys) input({ t: "key", keysym: k, down: false });
    for (const b of held.buttons) input({ t: "button", b, down: false, x: 0, y: 0 });
    held.keys.clear();
    held.buttons.clear();
  }

  function wireScreen() {
    // How it looks: smoother and sharper takes more of the connection. The
    // choice is kept for next time; a new stream starts at it.
    try { const q = localStorage.getItem("muxin-quality"); if (q && [...$("quality").options].some((o) => o.value === q)) $("quality").value = q; } catch (_) {}
    $("quality").addEventListener("change", () => {
      try { localStorage.setItem("muxin-quality", $("quality").value); } catch (_) {}
      if (!state.desktop) return;
      closeChannel(state.desktop);
      state.desktop = 0;
      showDesktop();
    });
    const c = canvas();
    let lastMove = 0, rest = 0;
    c.addEventListener("mousemove", (e) => {
      const p = at(e);
      if (!p || performance.now() - lastMove < 8) return;
      lastMove = performance.now();
      input({ t: "move", x: p.x, y: p.y });
    });
    const button = (e, down) => {
      const b = { 0: 1, 1: 2, 2: 3 }[e.button];
      if (!b || !state.canControl) return;
      e.preventDefault();
      if (down) c.focus();
      const p = at(e) || { x: 0, y: 0 };
      if (down) held.buttons.add(b); else held.buttons.delete(b);
      input({ t: "button", b, down, x: p.x, y: p.y });
    };
    c.addEventListener("mousedown", (e) => button(e, true));
    window.addEventListener("mouseup", (e) => { if (held.buttons.size) button(e, false); });
    c.addEventListener("contextmenu", (e) => { if (state.canControl) e.preventDefault(); });
    c.addEventListener("wheel", (e) => {
      const p = at(e);
      if (!p || !state.canControl) return;
      e.preventDefault();
      rest += e.deltaMode === 1 ? e.deltaY : e.deltaY / 40;
      const notches = Math.trunc(rest);
      if (notches) { rest -= notches; input({ t: "scroll", dy: notches, dx: 0, x: p.x, y: p.y }); }
    }, { passive: false });
    const key = (e, down) => {
      if (!state.canControl) return;
      const sym = keysym(e);
      if (sym == null) return;
      e.preventDefault();
      if (down) held.keys.add(sym); else held.keys.delete(sym);
      input({ t: "key", keysym: sym, down });
    };
    c.addEventListener("keydown", (e) => key(e, true));
    c.addEventListener("keyup", (e) => key(e, false));
    c.addEventListener("blur", letGo);
    window.addEventListener("blur", letGo);
    wireTouch(c);
  }

  // Phones and tablets: a tap clicks, a long press right-clicks, one finger
  // drags (the mouse held down), two fingers scroll. While watching only,
  // touch scrolls and zooms the page as usual.
  function wireTouch(c) {
    let start = null, dragging = false, long = 0, twoY = null, rest = 0;
    const point = (t) => at({ clientX: t.clientX, clientY: t.clientY });
    const clickAt = (p, b) => {
      input({ t: "move", x: p.x, y: p.y });
      input({ t: "button", b, down: true, x: p.x, y: p.y });
      input({ t: "button", b, down: false, x: p.x, y: p.y });
    };
    c.addEventListener("touchstart", (e) => {
      if (!state.canControl) return;
      e.preventDefault();
      clearTimeout(long);
      if (e.touches.length === 2) {
        start = null;
        twoY = (e.touches[0].clientY + e.touches[1].clientY) / 2;
        return;
      }
      const t = e.touches[0], p = point(t);
      if (!p) return;
      start = { x: t.clientX, y: t.clientY, p, done: false };
      dragging = false;
      long = setTimeout(() => {
        if (start && !dragging) { clickAt(start.p, 3); start.done = true; }
      }, 550);
    }, { passive: false });
    c.addEventListener("touchmove", (e) => {
      if (!state.canControl) return;
      e.preventDefault();
      if (e.touches.length === 2 && twoY != null) {
        const y = (e.touches[0].clientY + e.touches[1].clientY) / 2;
        const p = point(e.touches[0]) || { x: 0, y: 0 };
        rest += (twoY - y) / 24;
        twoY = y;
        const notches = Math.trunc(rest);
        if (notches) { rest -= notches; input({ t: "scroll", dy: notches, dx: 0, x: p.x, y: p.y }); }
        return;
      }
      if (!start || start.done) return;
      const t = e.touches[0], p = point(t);
      if (!p) return;
      if (!dragging && Math.hypot(t.clientX - start.x, t.clientY - start.y) > 10) {
        dragging = true;
        clearTimeout(long);
        input({ t: "move", x: start.p.x, y: start.p.y });
        input({ t: "button", b: 1, down: true, x: start.p.x, y: start.p.y });
        held.buttons.add(1);
      }
      if (dragging) input({ t: "move", x: p.x, y: p.y });
    }, { passive: false });
    const end = (e) => {
      if (!state.canControl) return;
      clearTimeout(long);
      if (e.touches.length) return;
      twoY = null;
      if (!start) return;
      const t = e.changedTouches[0], p = point(t) || start.p;
      if (dragging) {
        input({ t: "button", b: 1, down: false, x: p.x, y: p.y });
        held.buttons.delete(1);
      } else if (!start.done) {
        clickAt(start.p, 1);
      }
      start = null;
      dragging = false;
    };
    c.addEventListener("touchend", end);
    c.addEventListener("touchcancel", end);

    // A keyboard for touch screens: what's typed goes to the shared screen.
    const field = $("kbd-input");
    $("kbd").addEventListener("click", () => field.focus());
    field.addEventListener("input", (e) => {
      const text = e.data || field.value;
      if (text && state.canControl) input({ t: "text", s: text });
      field.value = "";
    });
    field.addEventListener("keydown", (e) => {
      const sym = { Enter: 0xff0d, Backspace: 0xff08, Tab: 0xff09, Escape: 0xff1b, ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54 }[e.key];
      if (!sym || !state.canControl) return;
      e.preventDefault();
      input({ t: "key", keysym: sym, down: true });
      input({ t: "key", keysym: sym, down: false });
    });
  }

  function onChannel(channel, kind, body) {
    const ch = state.channels.get(channel);
    if (!ch) return;
    if (ch.kind === "desktop") {
      if (kind === TILES || kind === FRAME) paint(kind, body);
      else if (kind === CLOSE) {
        state.channels.delete(channel);
        state.desktop = 0;
        $("screen-wait").textContent = "Their screen stopped.";
        $("screen-wait").hidden = false;
      }
      return;
    }
    if (channel !== state.session) return;
    const machine = state.info && state.info.machine_share;
    if (kind === DATA) state.term.write(body);
    else if (kind === SIZE && body.length === 4) {
      const v = new DataView(body.buffer, body.byteOffset);
      state.term.resize(Math.max(10, v.getUint16(0)), Math.max(4, v.getUint16(2)));
      fitText();
    } else if (kind === EXIT || kind === CLOSE) {
      if (kind === CLOSE) { state.channels.delete(channel); state.session = 0; }
      if (!machine) return finish("The session ended", "");
      state.term.write("\r\n\x1b[2m[The session ended]\x1b[0m\r\n");
    }
  }

  // Frames that came in parts, per channel, until their last part.
  const pieces = new Map();

  function onFrame(buf) {
    const bytes = new Uint8Array(buf);
    if (bytes.length < 5) return;
    const channel = new DataView(bytes.buffer, bytes.byteOffset).getUint32(0);
    const kind = bytes[4];
    let body = bytes.subarray(5);
    if (kind === MORE) {
      const parts = pieces.get(channel) || [];
      parts.push(body);
      pieces.set(channel, parts);
      return;
    }
    const parts = pieces.get(channel);
    if (parts) {
      pieces.delete(channel);
      parts.push(body);
      const whole = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
      let at = 0;
      for (const p of parts) { whole.set(p, at); at += p.length; }
      body = whole;
    }
    if (channel !== 0) return onChannel(channel, kind, body);
    if (kind !== JSON_KIND) return;
    let m;
    try { m = JSON.parse(dec.decode(body)); } catch (_) { return; }
    const clean = (s, n) => String(s || "").replace(/[\u0000-\u001f\u007f-\u009f]/g, "").slice(0, n);
    const ports = (list) => (Array.isArray(list) ? list.slice(0, 32) : []).map((p) => ({ port: Number(p.port) || 0, name: clean(p.name, 60) })).filter((p) => p.port > 0);
    const desktop = (d) => (d === "view" || d === "control" ? d : "none");
    switch (m.event) {
      case "share":
        showInfo({
          machine_share: m.kind === "machine", title: clean(m.title, 120), agent: clean(m.agent, 40), machine: clean(m.machine, 80), by: clean(m.by, 80),
          can_type: !!m.can_type, desktop: desktop(m.desktop), can_control: !!m.can_control, mac: !!m.mac, ports: ports(m.ports), ends_at: Number(m.ends_at) || 0,
        });
        break;
      case "waiting":
        $("wait-status").textContent = `Waiting for ${state.info ? state.info.by : "them"} to let you in…`;
        break;
      case "admitted":
        state.canType = !!m.can_type;
        state.canControl = !!m.can_control;
        admitted();
        break;
      case "sessions":
        state.sessions = (Array.isArray(m.sessions) ? m.sessions.slice(0, 200) : []).map((s) => ({
          name: clean(s.name, 120), title: clean(s.title, 120), agent: clean(s.agent, 40), status: clean(s.status, 20), busy: !!s.busy,
        }));
        if (!state.showing) {
          const first = state.sessions.find((s) => s.status !== "exited");
          if (first) showSession(first.name);
          else if (state.info.desktop !== "none") showDesktop();
          else showEmpty(`No sessions are running on ${state.info.title}.`);
        }
        renderTabs();
        break;
      case "typing":
        state.canType = !!m.allowed;
        updateNote();
        break;
      case "control":
        state.canControl = !!m.allowed;
        if (!state.canControl) letGo();
        updateNote();
        break;
      case "access":
        state.info.desktop = desktop(m.desktop);
        state.info.ports = ports(m.ports);
        state.canControl = !!m.can_control;
        if (state.info.desktop === "none" && state.showing && state.showing.kind === "desktop") {
          const first = state.sessions.find((s) => s.status !== "exited");
          if (first) showSession(first.name); else showEmpty("They stopped sharing the screen.");
        }
        renderTabs();
        updateNote();
        break;
      case "refused": {
        const ch = state.channels.get(Number(m.channel));
        state.channels.delete(Number(m.channel));
        if (ch && ch.kind === "desktop") {
          state.desktop = 0;
          $("screen-wait").textContent = clean(m.error, 200);
          $("screen-wait").hidden = false;
        } else if (ch && Number(m.channel) === state.session) {
          state.session = 0;
          state.term.write(`\r\n\x1b[2m${clean(m.error, 200)}\x1b[0m\r\n`);
        }
        break;
      }
      case "denied":
        finish("You weren't let in", "Ask them for a new link if that wasn't meant to happen.");
        break;
      case "ended":
        finish(m.why === "removed" ? "You were removed" : "The share ended", m.why === "expired" ? "Its time ran out." : "");
        break;
      case "error":
        finish("Couldn't join", clean(m.error, 200));
        break;
    }
  }

  // The share server (this page's own): it only passes the offer and the
  // answer between the two sides.
  async function api(method, path, body) {
    const res = await fetch(`/share/v1${path}`, {
      method,
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: body ? JSON.stringify(body) : undefined,
      cache: "no-store",
      credentials: "omit",
      referrerPolicy: "no-referrer",
    });
    let data = {};
    try { data = await res.json(); } catch (_) {}
    if (!res.ok) throw new Error(data.error || "The share server couldn't be reached.");
    return data;
  }

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // The addresses that matter, in the description (no trickling: one offer,
  // one answer): this computer's, its public one when there's a STUN
  // server, the relay's when there's a relay. They come within moments;
  // the end of gathering can take much longer.
  function gathered(pc, ice) {
    const urls = ice.flatMap((s) => [].concat(s.urls || []));
    const wantPublic = urls.some((u) => u.startsWith("stun:"));
    const wantRelay = urls.some((u) => u.startsWith("turn:") || u.startsWith("turns:"));
    const seen = new Set();
    return new Promise((resolve) => {
      let done = false;
      const finish = () => { if (!done) { done = true; resolve(); } };
      const check = () => {
        if (seen.has("host") && (!wantPublic || seen.has("srflx")) && (!wantRelay || seen.has("relay"))) setTimeout(finish, 100);
      };
      pc.addEventListener("icecandidate", (e) => {
        if (!e.candidate) return finish();
        const m = / typ (\w+)/.exec(e.candidate.candidate);
        if (m) seen.add(m[1]);
        check();
      });
      pc.addEventListener("icegatheringstatechange", () => { if (pc.iceGatheringState === "complete") finish(); });
      setTimeout(finish, 2500);
    });
  }

  async function connect(ticket) {
    show("wait");
    $("wait-status").textContent = "Connecting…";
    const id = room(ticket.id);
    try {
      const r = await api("GET", `/rooms/${id}`);
      if (!r.open) return finish("The share ended", "It isn't open right now.");
      const pc = new RTCPeerConnection({ iceServers: Array.isArray(r.ice) ? r.ice : [] });
      state.pc = pc;
      const dc = pc.createDataChannel("muxin");
      dc.binaryType = "arraybuffer";
      state.dc = dc;
      pc.onconnectionstatechange = () => {
        if (pc.connectionState === "failed" && !state.over) finish("Couldn't connect", "No way through to the computer sharing it.");
        if (pc.connectionState === "connected") {
          pc.getStats().then((stats) => {
            stats.forEach((p) => {
              if (p.type === "candidate-pair" && p.state === "succeeded" && p.nominated) {
                const local = stats.get(p.localCandidateId), remote = stats.get(p.remoteCandidateId);
                const relayed = (local && local.candidateType === "relay") || (remote && remote.candidateType === "relay");
                $("term-path").textContent = relayed ? "Through the relay · encrypted" : "Peer to peer · encrypted";
              }
            });
          }).catch(() => {});
        }
      };
      dc.onopen = async () => {
        const mine = fingerprint(pc.localDescription.sdp);
        if (!mine) return finish("Couldn't join", "This browser didn't offer a certificate.");
        const proof = hex(await hmac(ticket.secret, `muxin-share-v1\n${ticket.fingerprint}\n${mine}`));
        const tag = await hmac(ticket.secret, `muxin-share-code-v1\n${mine}`);
        const code = String(new DataView(tag.buffer).getUint32(0) % 1000000).padStart(6, "0");
        $("code").textContent = `${code.slice(0, 3)} ${code.slice(3)}`;
        $("code-box").hidden = false;
        $("wait-status").textContent = "Asking to join…";
        send({ op: "hello", v: 1, name: state.name, client: "browser", proof });
      };
      dc.onmessage = (e) => onFrame(e.data);
      dc.onclose = () => { if (!state.over) finish("The share ended", "The connection closed."); };
      const iceReady = gathered(pc, Array.isArray(r.ice) ? r.ice : []);
      await pc.setLocalDescription(await pc.createOffer());
      await iceReady;
      const offer = await api("POST", `/rooms/${id}/offers`, { sdp: pc.localDescription.sdp });
      // The answer, once the sharing computer picks the offer up.
      const until = Date.now() + 30000;
      while (!state.over) {
        if (Date.now() > until) return finish("Couldn't connect", "The computer sharing it didn't answer.");
        await sleep(700);
        const a = await api("GET", `/rooms/${id}/offers/${offer.id}`);
        if (!a.open) return finish("The share ended", "It isn't open right now.");
        if (!a.answer) continue;
        // Only the share's own certificate: the server can't stand in.
        if (fingerprint(a.answer) !== ticket.fingerprint) return finish("Couldn't join", "That share's connection couldn't be verified.");
        await pc.setRemoteDescription({ type: "answer", sdp: a.answer });
        break;
      }
    } catch (e) {
      if (!state.over) finish("Couldn't join", e.message || "The share server couldn't be reached.");
      return;
    }
    setTimeout(() => {
      if (!state.over && (!state.dc || state.dc.readyState !== "open")) finish("Couldn't connect", "The computer sharing it didn't answer.");
    }, 25000);
  }

  function start() {
    const ticket = readTicket();
    if (!ticket || !window.RTCPeerConnection || !crypto.subtle) {
      $("done-title").textContent = ticket ? "This browser can't join" : "That link isn't complete";
      $("done-sub").textContent = ticket ? "Try a current Chrome, Safari, Firefox or Edge." : "Ask for the link again, and copy all of it.";
      show("done");
      return;
    }
    $("open-app").href = `muxin://join?server=${encodeURIComponent(location.origin)}#${ticket.fragment}`;
    show("start");
    try { $("name").value = localStorage.getItem("muxin-name") || ""; } catch (_) {}
    $("name").focus();
    $("name").addEventListener("input", () => { $("name-error").hidden = true; });
    $("join-form").addEventListener("submit", (e) => {
      e.preventDefault();
      const name = $("name").value.trim();
      if (!name) { $("name-error").hidden = false; return; }
      state.name = name.slice(0, 40);
      try { localStorage.setItem("muxin-name", state.name); } catch (_) {}
      connect(ticket);
    });
    wireScreen();
    $("cancel").addEventListener("click", () => { send({ op: "bye" }); finish("You left", ""); });
    $("leave").addEventListener("click", () => { send({ op: "bye" }); finish("You left", ""); });
  }

  // Another link opened in this tab: start over with it.
  window.addEventListener("hashchange", () => {
    if (location.hash.length > 1) location.reload();
  });

  start();
})();
