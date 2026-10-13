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
  return write(text, false);
}

/*
 * Issue #1705 — the clipboard ends on the NEWEST successful write, whoever
 * made it.
 *
 * Each write is an import and an IPC round trip, and two in flight at once can
 * finish in either order: an agent's copy, a Reader's Copy all, a notice's
 * Copy button. So every write is numbered, and whenever one settles the state
 * is checked: if the write that landed last is not the newest one that
 * succeeded, older text is on the clipboard, and the newest successful text is
 * written again. While any write newer than that is still in flight nothing is
 * done — it lands after anyway, or fails, and is checked when it settles. A
 * failed write changes nothing on the clipboard, so it counts as settled but
 * never as landed. A repair that itself fails is not retried, so a clipboard
 * that keeps refusing cannot loop. Every writer in the app goes through here,
 * which is why the repair lives here and not in any one caller's queue.
 */
let writesStarted = 0;
const inFlight = new Set<number>();
let newestSucceeded: { write: number; text: string } | undefined;
let lastLanded = 0;

async function write(text: string, repair: boolean): Promise<void> {
  const number = ++writesStarted;
  inFlight.add(number);
  try {
    await platformWrite(text);
  } catch (error) {
    inFlight.delete(number);
    if (!repair) checkNewestLanded();
    throw error;
  }
  inFlight.delete(number);
  lastLanded = number;
  if (!newestSucceeded || number > newestSucceeded.write) newestSucceeded = { write: number, text };
  checkNewestLanded();
}

function checkNewestLanded(): void {
  const newest = newestSucceeded;
  if (!newest || lastLanded === newest.write) return;
  for (const number of inFlight) if (number > newest.write) return;
  void write(newest.text, true).catch(() => undefined);
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
