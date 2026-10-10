import { createContext } from "react";

/**
 * PRD #1401 — the in-app pull request browser, as the app drives it.
 *
 * The page itself is a native child webview Rust creates (`pr_browser.rs`); the
 * app draws the frame it sits in and the toolbar around it, and moves it with
 * these calls. Every call flows from the app to the page: the page can call
 * none of them (no capability names its webview, and every app command refuses
 * a webview other than the main one). The one thing that comes back is
 * {@link PrBrowserHost.onClosed} — the page's own Escape, which Rust turns into
 * a close and announces with no data.
 */
export interface PrBrowserHost {
  /** Whether a page can be shown at all — false outside the app (the browser tier, `vite` preview). */
  available: boolean;
  /** Show `url` over `bounds`, or move the open page there. */
  open(url: string, bounds: PrBrowserBounds): Promise<void>;
  /** The frame moved or resized. */
  setBounds(bounds: PrBrowserBounds): Promise<void>;
  /** Hide the page while one of the app's dialogs is over it — a native webview draws above them all. */
  setVisible(visible: boolean): Promise<void>;
  /** The toolbar's Back: one step back in the page's history. */
  back(): Promise<void>;
  /** A spoken scroll. */
  scroll(move: PrBrowserScroll): Promise<void>;
  /** Open in browser: the page on screen in the system browser; the in-app page closes. */
  openExternal(): Promise<void>;
  /** Close the page. Harmless when none is open. */
  close(): Promise<void>;
  /** Settings → Sign out of GitHub. */
  signOut(): Promise<void>;
  /** The page closed itself (its Escape). Returns the unsubscribe. */
  onClosed(listener: () => void): () => void;
}

/** The frame, and the size of the page it sits in, all in the main webview's CSS pixels (`pr_browser::Bounds`). */
export type PrBrowserBounds = { x: number; y: number; width: number; height: number; viewportWidth: number; viewportHeight: number };

export type PrBrowserScroll = "down" | "up" | "top" | "bottom";

/** What Rust emits to the main webview when the page closed itself. */
export const PR_BROWSER_CLOSED_EVENT = "pr-browser://closed";

/** The host inside the app: each call is one of `lib.rs`'s `desktop_pr_browser_*` commands. */
export function tauriPrBrowser(): PrBrowserHost {
  /* One call at a time, in the order they were made. Tauri runs async commands
     concurrently, and an open, a close and an open again (a pane re-mounted)
     must not land as open, open, close. */
  let queue: Promise<unknown> = Promise.resolve();
  const call = (command: string, args?: Record<string, unknown>): Promise<void> => {
    const next = queue.then(async () => {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke(command, args);
    });
    queue = next.catch(() => undefined);
    return next;
  };
  return {
    available: true,
    open: (url, bounds) => call("desktop_pr_browser_open", { url, bounds }),
    setBounds: (bounds) => call("desktop_pr_browser_bounds", { bounds }),
    setVisible: (visible) => call("desktop_pr_browser_visible", { visible }),
    back: () => call("desktop_pr_browser_back"),
    scroll: (move) => call("desktop_pr_browser_scroll", { scroll: move }),
    openExternal: () => call("desktop_pr_browser_open_external"),
    close: () => call("desktop_pr_browser_close"),
    signOut: () => call("desktop_pr_browser_sign_out"),
    onClosed: (listener) => {
      let unlisten: (() => void) | undefined;
      let stopped = false;
      void import("@tauri-apps/api/event").then(({ listen }) => listen(PR_BROWSER_CLOSED_EVENT, () => listener())).then((stop) => {
        if (stopped) stop();
        else unlisten = stop;
      });
      return () => {
        stopped = true;
        unlisten?.();
      };
    },
  };
}

/**
 * Outside the app there is no webview to put a page in. The frame still opens
 * — so the toolbar, Escape and voice behave the same in the browser tier — and
 * says where the page would be; "Open in browser" opens it in a tab.
 */
export function unavailablePrBrowser(): PrBrowserHost {
  let shown: string | undefined;
  const nothing = async () => undefined;
  return {
    available: false,
    open: async (url) => { shown = url; },
    setBounds: nothing,
    setVisible: nothing,
    back: nothing,
    scroll: nothing,
    openExternal: async () => { if (shown) window.open(shown, "_blank", "noopener,noreferrer"); },
    close: async () => { shown = undefined; },
    signOut: nothing,
    onClosed: () => () => undefined,
  };
}

/** The host for the running app: Tauri's when there is one. */
export function defaultPrBrowser(): PrBrowserHost {
  return typeof window !== "undefined" && window.__TAURI_INTERNALS__ ? tauriPrBrowser() : unavailablePrBrowser();
}

/** Where the shell and Settings read the host from; tests provide a fake. */
export const PrBrowserHostContext = createContext<PrBrowserHost>(defaultPrBrowser());

/**
 * Open the in-app browser on one agent's pull request — what the badge calls.
 * Provided by the shell, which owns the browser; absent elsewhere (a deck
 * rendered on its own in a test), where the badge opens nothing.
 */
export const OpenPullRequest = createContext<((target: { deckId: string; agentId: string }) => void) | undefined>(undefined);
