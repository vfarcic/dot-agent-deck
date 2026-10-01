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
  if (window.__TAURI_INTERNALS__) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("plugin:clipboard-manager|write_text", { text });
    return;
  }
  await navigator.clipboard.writeText(text);
}
