/**
 * Issue #1403 — put text on the system clipboard.
 *
 * Inside the app this is the clipboard-manager plugin's `write_text`, which
 * writes from Rust through the operating system's own clipboard, so it does
 * not depend on whether a given webview lets the page use
 * `navigator.clipboard` (`desktop/src-tauri/Cargo.toml` has why that is not
 * the same answer on every platform). The capability grants that one command
 * and no read.
 *
 * The command is invoked by name through `@tauri-apps/api/core`, which the app
 * already depends on, rather than through `@tauri-apps/plugin-clipboard-manager`:
 * that package is a one-call wrapper around exactly this, and leaving it out
 * leaves no npm half to drift from the crate's version.
 *
 * Outside the app (the browser tier and `vite` preview, which have no Tauri
 * bridge) it falls back to the web API, which is all a browser has.
 */
export async function writeClipboardText(text: string): Promise<void> {
  const write = ++writesStarted;
  newestText = text;
  await platformWrite(text);
  settled(write);
}

/*
 * Issue #1705 — the clipboard ends on the NEWEST write, whoever made it.
 *
 * Each write is an import and an IPC round trip, and two in flight at once can
 * finish in either order: an agent's copy, a Reader's Copy all, a notice's
 * Copy button. A write that finishes after a newer one has already finished
 * landed last and left older text on the clipboard, so the newest text is
 * written again. While the newest write is still in flight nothing is done:
 * it lands after this one anyway. Every writer in the app goes through here,
 * which is why the repair lives here and not in any one caller's queue.
 */
let writesStarted = 0;
let newestFinished = 0;
let newestText = "";

function settled(write: number): void {
  if (write > newestFinished) {
    newestFinished = write;
    return;
  }
  if (newestFinished === writesStarted) void writeClipboardText(newestText).catch(() => undefined);
}

/** Loaded once and shared, so writes started together wait on one import. */
let pluginInvoke: Promise<typeof import("@tauri-apps/api/core")["invoke"]> | undefined;

async function platformWrite(text: string): Promise<void> {
  if (window.__TAURI_INTERNALS__) {
    pluginInvoke ??= import("@tauri-apps/api/core").then((core) => core.invoke);
    const invoke = await pluginInvoke;
    await invoke("plugin:clipboard-manager|write_text", { text });
    return;
  }
  await navigator.clipboard.writeText(text);
}
