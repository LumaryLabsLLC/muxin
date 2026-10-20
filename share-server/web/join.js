// Joins a Muxin share from the browser: the link's ticket (after the #)
// names the share, its secret and its certificate. This page connects to
// the sharing computer over WebRTC, refuses any other certificate, proves
// it has the link, then shows the session. See src/share in Muxin.
"use strict";

(() => {
  const JSON_KIND = 10, DATA = 0, EXIT = 2, SIZE = 3, CLOSE = 11;
  const SESSION = 1;
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

  const state = { over: false, pc: null, dc: null, term: null, canType: false, info: null, clock: 0, name: "" };

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
    $("session-title").textContent = info.title || what;
    $("session-sub").textContent = `${what} on ${info.machine} · shared by ${info.by}`;
    $("session").hidden = false;
    $("wait-title").textContent = `Join ${info.title || what}`;
    $("wait-sub").textContent = `${what} on ${info.machine} · shared by ${info.by}`;
    $("fact-can").textContent = info.can_type ? "You can watch and type" : "You can watch";
    $("fact-ends").textContent = `Ends in ${minutesLeft(info.ends_at).replace(" left", "")}`;
    $("fact-name").textContent = `Joining as ${state.name}`;
    $("facts").hidden = false;
    $("code-help").textContent = `Read it to ${info.by} so they know it's you.`;
    clearInterval(state.clock);
    state.clock = setInterval(() => { $("time-left").textContent = minutesLeft(info.ends_at); }, 15000);
    $("time-left").textContent = minutesLeft(info.ends_at);
  }

  function setTyping(allowed) {
    state.canType = allowed;
    $("mode-pill").textContent = allowed ? "Typing" : "Watching";
    const by = state.info ? state.info.by : "they";
    $("term-note").textContent = allowed ? "You can type. Keys go to their session." : `You're watching. Only ${by} can type.`;
    if (state.term) state.term.options.disableStdin = !allowed;
  }

  function openTerminal() {
    show("watch");
    $("session-actions").hidden = false;
    const term = new window.Terminal({
      cols: 80, rows: 24, scrollback: 5000, convertEol: false, cursorBlink: true, disableStdin: true,
      fontFamily: 'ui-monospace, "SF Mono", Menlo, Consolas, monospace', fontSize: 13,
      theme: { background: "#1a1e3c", foreground: "#eeecfb", cursor: "#eeecfb", selectionBackground: "#3d3a7a" },
    });
    term.open($("term"));
    term.onData((data) => {
      if (state.canType && state.dc && state.dc.readyState === "open") state.dc.send(frame(SESSION, DATA, enc.encode(data)));
    });
    term.onBinary((data) => {
      if (!state.canType || !state.dc || state.dc.readyState !== "open") return;
      state.dc.send(frame(SESSION, DATA, Uint8Array.from(data, (c) => c.charCodeAt(0) & 255)));
    });
    state.term = term;
    setTyping(state.canType);
  }

  function send(obj) {
    if (state.dc && state.dc.readyState === "open") state.dc.send(frame(0, JSON_KIND, enc.encode(JSON.stringify(obj))));
  }

  function onFrame(buf) {
    const bytes = new Uint8Array(buf);
    if (bytes.length < 5) return;
    const channel = new DataView(bytes.buffer, bytes.byteOffset).getUint32(0);
    const kind = bytes[4];
    const body = bytes.subarray(5);
    if (channel === SESSION) {
      if (!state.term) return;
      if (kind === DATA) state.term.write(body);
      else if (kind === SIZE && body.length === 4) {
        const v = new DataView(body.buffer, body.byteOffset);
        state.term.resize(Math.max(10, v.getUint16(0)), Math.max(4, v.getUint16(2)));
      } else if (kind === EXIT) finish("The session ended", "");
      else if (kind === CLOSE) finish("The session ended", "");
      return;
    }
    if (channel !== 0 || kind !== JSON_KIND) return;
    let m;
    try { m = JSON.parse(dec.decode(body)); } catch (_) { return; }
    const clean = (s, n) => String(s || "").replace(/[\u0000-\u001f\u007f-\u009f]/g, "").slice(0, n);
    switch (m.event) {
      case "share":
        showInfo({ title: clean(m.title, 120), agent: clean(m.agent, 40), machine: clean(m.machine, 80), by: clean(m.by, 80), can_type: !!m.can_type, ends_at: Number(m.ends_at) || 0 });
        break;
      case "waiting":
        $("wait-status").textContent = `Waiting for ${state.info ? state.info.by : "them"} to let you in…`;
        break;
      case "admitted":
        state.canType = !!m.can_type;
        openTerminal();
        send({ op: "watch", channel: SESSION });
        break;
      case "typing":
        setTyping(!!m.allowed);
        break;
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
    $("cancel").addEventListener("click", () => { send({ op: "bye" }); finish("You left", ""); });
    $("leave").addEventListener("click", () => { send({ op: "bye" }); finish("You left", ""); });
  }

  // Another link opened in this tab: start over with it.
  window.addEventListener("hashchange", () => {
    if (location.hash.length > 1) location.reload();
  });

  start();
})();
