import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { ArrowLeft, ExternalLink, GitPullRequest, X } from "lucide-react";
import { useInertBackground } from "../hooks/useInertBackground";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { PrBrowserBounds, PrBrowserHost } from "../lib/prBrowser";

/** What is open: whose pull request, and where. */
export type PullRequestBrowserSession = {
  deckId: string;
  agentId: string;
  /** What the deck calls the agent, for the toolbar. Daemon text, so bounded at render. */
  agentLabel: string;
  number: number;
  url: string;
};

/**
 * What counts as one of the app's dialogs: anything that says it is one. Every
 * dialog the app draws is mounted only while it is open, so a dialog in the
 * document is a dialog on screen.
 *
 * The role, not only `aria-modal`: the voice surface's "What you can say" list
 * is a `role="dialog"` drawn over the whole screen and deliberately NOT
 * `aria-modal` (it fences nothing, so it must not claim to — see its note in
 * `VoiceControlPanel`). It covers the page all the same, and the page drawn
 * over it would hide the list the user just asked for.
 */
const DIALOG_SELECTOR = "[role='dialog'], [role='alertdialog'], [aria-modal='true']";

/**
 * The dialogs of the app's own that can come up over the browser. A native
 * webview is drawn above everything the main webview renders, so while one of
 * these is open the page is hidden rather than left covering it. The agent's
 * pane is excluded: the browser is opened over it.
 */
function coveredByDialog(self: Element | null): boolean {
  for (const modal of Array.from(document.querySelectorAll(DIALOG_SELECTOR))) {
    if (modal === self || self?.contains(modal) || modal.contains(self)) continue;
    if (modal.classList.contains("agent-pane-overlay")) continue;
    return true;
  }
  return false;
}

/** Whether one of the app's dialogs is over the browser, kept current as dialogs come and go. */
function useCovered(ref: React.RefObject<HTMLElement | null>): boolean {
  const [covered, setCovered] = useState(false);
  useEffect(() => {
    const check = () => setCovered(coveredByDialog(ref.current));
    check();
    const observer = new MutationObserver(check);
    observer.observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ["aria-modal", "role"] });
    return () => observer.disconnect();
  }, [ref]);
  return covered;
}

/**
 * PRD #1401 — GitHub's pull request page, inside the app, over the agent's
 * screen.
 *
 * The toolbar is the app's: Back, Open in browser and Close are buttons here,
 * outside the page, so the page cannot draw over them or press them. The frame
 * below it is where Rust draws the page; this component reports the frame's
 * rectangle on open and whenever it moves.
 *
 * **What it names is what the page shows.** `session` is the pull request on
 * screen; another one asked for while it is open arrives as `request`, which
 * this component asks the host to switch the page to and hands the outcome to
 * `onSwitch` — the owner names the new pull request only once the page is on
 * it. Closing is the owner's too (`onClose`), and the frame goes only once the
 * host has closed the page, so a page the platform would not close keeps its
 * frame and its Close.
 *
 * **`Escape` closes it, and only it.** The listener is on `window` in the
 * capture phase and stops the event there, so the agent's pane, Settings and
 * the deck's own `Escape` handlers — all bound later on the same target — do
 * not also answer the key. With the page focused the key never reaches this
 * document at all; the page's own script turns an unused Escape into a close
 * (`pr_browser::ESCAPE_SCRIPT`) and {@link PrBrowserHost.onClosed} reports it.
 */
export function PullRequestBrowser<S extends PullRequestBrowserSession>({ session, request, onSwitch, host, zoom, problem, onClose, onBack, onOpenExternal, onClosedByPage }: {
  /** The pull request the page shows. */
  session: S;
  /** Another pull request asked for while this one is open; the page switches to it. */
  request?: S;
  /** The switch to `request` is under way: `work` settles when the page is on it, or rejects with why not. */
  onSwitch: (request: S, work: Promise<void>) => void;
  host: PrBrowserHost;
  /** The app's zoom level. Not a factor anything multiplies by (see `bounds`): a change of it moves the frame, which is reported again. */
  zoom: number;
  /**
   * Why the last thing asked of the page did not happen — Back, a spoken
   * scroll, or the hand-off to the system browser — in the host's own words.
   * Shown under the toolbar rather than in the frame, because the page is drawn
   * over the frame and would hide it.
   */
  problem?: string;
  onClose: () => void;
  onBack: () => void;
  onOpenExternal: () => void;
  /** The page closed itself; the session is over and nothing needs closing. */
  onClosedByPage: () => void;
}) {
  const overlayRef = useInertBackground<HTMLDivElement>(true);
  const frameRef = useRef<HTMLDivElement>(null);
  const covered = useCovered(overlayRef);
  /* Why the first page did not open, against the address it was for, so a
     later switch that succeeds is not shown under an old failure. */
  const [failure, setFailure] = useState<{ url: string; text: string }>();
  /* Why the page could not follow the frame, or hide under a dialog; cleared
     by the next of each that succeeds. Shown under the toolbar, because the
     page is drawn over the frame. */
  const [placement, setPlacement] = useState<{ bounds?: string; visible?: string }>({});

  /* The frame and the page it sits in, both in CSS pixels; Rust places the
     browser in proportion to the page's real size (`pr_browser::Bounds`),
     which absorbs the app's zoom and any scaling the engine applies. */
  const bounds = (): PrBrowserBounds | undefined => {
    const frame = frameRef.current;
    if (!frame) return undefined;
    const rect = frame.getBoundingClientRect();
    return {
      x: Math.max(0, rect.left),
      y: Math.max(0, rect.top),
      width: Math.max(1, rect.width),
      height: Math.max(1, rect.height),
      viewportWidth: Math.max(1, window.innerWidth),
      viewportHeight: Math.max(1, window.innerHeight),
    };
  };
  const boundsRef = useRef(bounds);
  boundsRef.current = bounds;

  /* Open on the pull request this frame was drawn for. Later ones arrive as
     `request` (below), not as a new `session`, which the owner sets only
     once the page shows it. */
  const firstUrl = useRef(session.url);
  useLayoutEffect(() => {
    const rect = boundsRef.current();
    if (!rect) return;
    const url = firstUrl.current;
    setFailure(undefined);
    host.open(url, rect).catch((error: unknown) => setFailure({ url, text: problemText(error) }));
  }, [host]);

  /* Switch the page to another pull request. */
  const onSwitchRef = useRef(onSwitch);
  onSwitchRef.current = onSwitch;
  useLayoutEffect(() => {
    if (!request) return;
    const rect = boundsRef.current();
    if (!rect) return;
    onSwitchRef.current(request, host.open(request.url, rect));
  }, [host, request]);

  /* A safety net, not the close: the owner closes the page before it takes
     the frame away (`onClose`), so here it is already gone and this does
     nothing. It closes one only when the whole shell goes, and then there is
     no screen left to say a failure on, so it is printed. */
  useEffect(() => () => {
    void host.close().catch((error: unknown) => console.error(`dot-agent-deck-desktop: the pull request page did not close with the app's screen: ${problemText(error)}`));
  }, [host]);

  /* Follow the frame. */
  useEffect(() => {
    const frame = frameRef.current;
    if (!frame) return;
    const report = () => {
      const rect = boundsRef.current();
      if (rect) {
        host.setBounds(rect).then(
          () => setPlacement((was) => (was.bounds === undefined ? was : { ...was, bounds: undefined })),
          (error: unknown) => setPlacement((was) => ({ ...was, bounds: problemText(error) })),
        );
      }
    };
    /* Once now as well: a zoom change re-runs this, and the frame it moved
       may not change the size the observer watches. */
    report();
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(report) : undefined;
    observer?.observe(frame);
    window.addEventListener("resize", report);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", report);
    };
  }, [host, zoom]);

  useEffect(() => {
    host.setVisible(!covered).then(
      () => setPlacement((was) => (was.visible === undefined ? was : { ...was, visible: undefined })),
      (error: unknown) => setPlacement((was) => ({ ...was, visible: problemText(error) })),
    );
  }, [covered, host]);

  const onClosedByPageRef = useRef(onClosedByPage);
  onClosedByPageRef.current = onClosedByPage;
  useEffect(() => host.onClosed(() => onClosedByPageRef.current()), [host]);

  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  useEffect(() => {
    if (covered) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopImmediatePropagation();
      onCloseRef.current();
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [covered]);

  const title = `Pull request #${session.number}`;
  return (
    <div
      ref={overlayRef}
      className="pr-browser-overlay"
      data-testid="pr-browser"
      data-pr-browser=""
      data-covered={covered || undefined}
      role="dialog"
      aria-modal="true"
      aria-label={`${title} for ${displayText(session.agentLabel, DISPLAY_LIMITS.name)}`}
      tabIndex={-1}
    >
      <section className="pr-browser-panel">
        <header className="pr-browser-toolbar">
          <div className="pr-browser-title">
            <GitPullRequest size={14} aria-hidden="true" />
            <strong>{title}</strong>
            <span>{displayText(session.agentLabel, DISPLAY_LIMITS.name)}</span>
          </div>
          <div className="pr-browser-actions">
            {/* The app's own controls, as every other overlay draws them: text
                actions as compact secondary buttons, and Close as the icon
                button in the corner that Settings and New agent use. */}
            <button type="button" className="button secondary compact" onClick={onBack} title="Back" aria-label="Back"><ArrowLeft size={13} aria-hidden="true" /><span>Back</span></button>
            <button type="button" className="button secondary compact" onClick={onOpenExternal} title="Open in your browser" aria-label="Open in browser"><ExternalLink size={13} aria-hidden="true" /><span>Open in browser</span></button>
            <button type="button" className="icon-button" onClick={onClose} title="Close (Esc)" aria-label="Close pull request"><X size={16} aria-hidden="true" /></button>
          </div>
        </header>
        {[problem, placement.bounds, placement.visible].map((text, index) => text && (
          <p key={index} className="pr-browser-problem" role="alert" title={displayText(text, DISPLAY_LIMITS.message)}>{displayText(text, DISPLAY_LIMITS.message)}</p>
        ))}
        <div ref={frameRef} className="pr-browser-frame" data-testid="pr-browser-frame">
          {failure?.url === session.url ? (
            <p className="pr-browser-notice" role="alert">{failure.text}</p>
          ) : !host.available ? (
            <p className="pr-browser-notice">GitHub's page for {title} opens here in the app. Use Open in browser to see it now.</p>
          ) : null}
        </div>
      </section>
    </div>
  );
}

function problemText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Whether the open browser is hidden under one of the app's dialogs — read at voice dispatch time. */
export function pullRequestBrowserCovered(): boolean {
  return document.querySelector("[data-pr-browser][data-covered]") !== null;
}
