/**
 * Issue #2075 — post one operating-system notification.
 *
 * Inside the app this is the notification plugin's `notify`, invoked by name
 * through `@tauri-apps/api/core` exactly as `clipboard.ts` invokes the
 * clipboard plugin, so there is no `@tauri-apps/plugin-notification` npm half
 * to drift from the crate's version. The capability grants that one command.
 *
 * Outside the app (the browser tier and the `vite` preview, which have no Tauri
 * bridge) it uses the web Notification API when the page may, and otherwise
 * does nothing: the tile's badge still shows the question waiting.
 *
 * Never throws. A notification that cannot be shown is not worth an error the
 * user did not ask about; the badge is the record. Resolves `false` when it
 * could not post, so the caller can try again later.
 */
let pluginInvoke: Promise<typeof import("@tauri-apps/api/core")["invoke"]> | undefined;

export async function postOsNotification(title: string, body: string): Promise<boolean> {
  try {
    if (window.__TAURI_INTERNALS__) {
      pluginInvoke ??= import("@tauri-apps/api/core").then((core) => core.invoke);
      const invoke = await pluginInvoke;
      await invoke("plugin:notification|notify", { options: { title, body } });
      return true;
    }
    if (typeof Notification !== "undefined" && Notification.permission === "granted") {
      new Notification(title, { body });
      return true;
    }
    return false;
  } catch {
    // See the module comment: the badge is the record.
    return false;
  }
}
