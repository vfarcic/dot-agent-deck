// M1.3 frontend: render the daemon connection state, list agents, and host a
// LIVE embedded terminal (xterm.js) for the attached agent. PTY bytes cross the
// Tauri IPC boundary base64-encoded (both directions) so arbitrary control
// bytes stay exact. Uses the global Tauri API (`withGlobalTauri: true`) and the
// vendored xterm.js UMD globals (`Terminal`, `FitAddon`) — no bundler.

const els = {
  conn: document.querySelector(".conn"),
  dot: document.getElementById("status-dot"),
  label: document.getElementById("status-label"),
  detail: document.getElementById("status-detail"),
  retry: document.getElementById("retry"),
  refresh: document.getElementById("refresh"),
  agentList: document.getElementById("agent-list"),
  paneTitle: document.getElementById("pane-title"),
  detach: document.getElementById("detach"),
  terminal: document.getElementById("terminal"),
  overlay: document.getElementById("overlay"),
};

let term = null;
let fitAddon = null;
let attachedId = null;
let agentsById = new Map();

// ---- base64 byte helpers (byte-exact PTY transport) ----
function b64ToBytes(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}
function strToB64(s) {
  // xterm onData yields a "binary string" (each char code is one byte). Mask to
  // a byte and btoa so any stray >255 unit can't throw.
  let bin = "";
  for (let i = 0; i < s.length; i++) bin += String.fromCharCode(s.charCodeAt(i) & 0xff);
  return btoa(bin);
}

// ---- connection status ----
function row(term, value) {
  const dt = document.createElement("dt");
  dt.textContent = term;
  const dd = document.createElement("dd");
  dd.textContent = value;
  els.detail.append(dt, dd);
}

function renderConnection(state) {
  els.detail.replaceChildren();
  els.retry.hidden = true;
  els.conn.classList.remove("ok", "err");

  switch (state.status) {
    case "connecting":
      els.label.textContent = "Connecting…";
      break;
    case "connected":
      els.conn.classList.add("ok");
      els.label.textContent = "Connected";
      row("Protocol", `v${state.protocol_version}`);
      if (state.daemon_version) row("Daemon", state.daemon_version);
      refreshAgents();
      break;
    case "version-mismatch":
      els.conn.classList.add("err");
      els.label.textContent = "Version mismatch";
      row("GUI", `v${state.local}`);
      row("Daemon", state.remote == null ? "too old" : `v${state.remote}`);
      els.retry.hidden = false;
      break;
    case "disconnected":
    default:
      els.conn.classList.add("err");
      els.label.textContent = "Not connected";
      row("Reason", state.reason || "unknown");
      els.retry.hidden = false;
      break;
  }
}

// ---- agent list ----
async function refreshAgents() {
  const { core } = window.__TAURI__;
  let agents = [];
  try {
    agents = await core.invoke("agents");
  } catch (err) {
    console.error("agents invoke failed", err);
    return;
  }
  agentsById = new Map(agents.map((a) => [a.id, a]));
  els.agentList.replaceChildren();
  if (agents.length === 0) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = "No agents running.";
    els.agentList.append(li);
    return;
  }
  for (const a of agents) {
    const li = document.createElement("li");
    const btn = document.createElement("button");
    btn.dataset.id = a.id;
    if (a.id === attachedId) btn.classList.add("active");
    const name = document.createElement("span");
    name.textContent = a.label;
    const id = document.createElement("span");
    id.className = "aid";
    id.textContent = `id ${a.id}`;
    btn.append(name, id);
    btn.addEventListener("click", () => attachTo(a.id));
    li.append(btn);
    els.agentList.append(li);
  }
}

function markActive(id) {
  for (const btn of els.agentList.querySelectorAll("button")) {
    btn.classList.toggle("active", btn.dataset.id === id);
  }
}

// ---- terminal ----
function ensureTerminal() {
  if (term) return;
  term = new Terminal({
    convertEol: false,
    cursorBlink: true,
    fontFamily:
      'ui-monospace, SFMono-Regular, Menlo, Consolas, "DejaVu Sans Mono", monospace',
    fontSize: 13,
    scrollback: 10000,
    theme: { background: "#000000" },
  });
  fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open(els.terminal);

  // Keystrokes → daemon (KIND_STREAM_IN).
  term.onData((data) => {
    if (!attachedId) return;
    window.__TAURI__.core
      .invoke("terminal_input", { agentId: attachedId, data: strToB64(data) })
      .catch((err) => console.error("terminal_input failed", err));
  });

  // Resize → coalesced daemon Resize.
  term.onResize(({ cols, rows }) => {
    if (!attachedId) return;
    window.__TAURI__.core
      .invoke("terminal_resize", { agentId: attachedId, rows, cols })
      .catch((err) => console.error("terminal_resize failed", err));
  });

  const ro = new ResizeObserver(() => {
    if (term && attachedId) {
      try {
        fitAddon.fit();
      } catch (_) {
        /* terminal not visible yet */
      }
    }
  });
  ro.observe(els.terminal);
}

async function attachTo(agentId) {
  const { core } = window.__TAURI__;
  ensureTerminal();

  // Tear down a prior attachment on the shell side, then reset the screen.
  if (attachedId && attachedId !== agentId) {
    try {
      await core.invoke("detach");
    } catch (_) {
      /* ignore */
    }
  }
  term.reset();
  attachedId = agentId;
  const label = agentsById.get(agentId)?.label ?? agentId;
  els.paneTitle.textContent = `${label}  ·  id ${agentId}`;
  els.detach.hidden = false;
  els.overlay.hidden = true;
  markActive(agentId);

  try {
    await core.invoke("attach", { agentId });
  } catch (err) {
    term.writeln(`\r\n\x1b[31mAttach failed: ${err}\x1b[0m`);
    return;
  }
  // Fit now that the pane is visible, then push the initial size to the daemon.
  requestAnimationFrame(() => {
    try {
      fitAddon.fit();
      core.invoke("terminal_resize", {
        agentId,
        rows: term.rows,
        cols: term.cols,
      });
    } catch (err) {
      console.error("initial fit/resize failed", err);
    }
    term.focus();
  });
}

async function detachCurrent() {
  if (!attachedId) return;
  try {
    await window.__TAURI__.core.invoke("detach");
  } catch (_) {
    /* ignore */
  }
  attachedId = null;
  els.detach.hidden = true;
  els.paneTitle.textContent = "No terminal attached";
  els.overlay.hidden = false;
  markActive(null);
}

// ---- bootstrap ----
async function init() {
  const tauri = window.__TAURI__;
  if (!tauri) {
    els.label.textContent = "Open via the desktop app";
    return;
  }
  const { event, core } = tauri;

  await event.listen("connection-state", (e) => renderConnection(e.payload));

  await event.listen("terminal-output", (e) => {
    if (term && e.payload.agent_id === attachedId) {
      term.write(b64ToBytes(e.payload.data));
    }
  });

  await event.listen("terminal-exit", (e) => {
    if (e.payload.agent_id === attachedId && term) {
      term.writeln("\r\n\x1b[33m[stream ended]\x1b[0m");
      attachedId = null;
      els.detach.hidden = true;
      markActive(null);
    }
  });

  els.retry.addEventListener("click", () => core.invoke("reconnect"));
  els.refresh.addEventListener("click", () => refreshAgents());
  els.detach.addEventListener("click", () => detachCurrent());
  window.addEventListener("resize", () => {
    if (term && attachedId) {
      try {
        fitAddon.fit();
      } catch (_) {
        /* ignore */
      }
    }
  });

  try {
    renderConnection(await core.invoke("connection_state"));
  } catch (err) {
    console.error("connection_state invoke failed", err);
  }
}

init();
