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
 * The dialogs of the app's own that can come up over the browser. A native
 * webview is drawn above everything the main webview renders, so while one of
 * these is open the page is hidden rather than left covering it. The agent's
 * pane is excluded: the browser is opened over it.
 */
function coveredByDialog(self: Element | null): boolean {
  for (const modal of Array.from(document.querySelectorAll("[aria-modal='true']"))) {
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
    observer.observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ["aria-modal"] });
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
 * rectangle on open and whenever it moves, and closes the page when it
 * unmounts, so the page can never outlive the screen that shows it.
 *
 * **`Escape` closes it, and only it.** The listener is on `window` in the
 * capture phase and stops the event there, so the agent's pane, Settings and
 * the deck's own `Escape` handlers — all bound later on the same target — do
 * not also answer the key. With the page focused the key never reaches this
 * document at all; the page's own script turns an unused Escape into a close
 * (`pr_browser::ESCAPE_SCRIPT`) and {@link PrBrowserHost.onClosed} reports it.
 */
export function PullRequestBrowser({ session, host, zoom, onClose, onBack, onOpenExternal, onClosedByPage }: {
  session: PullRequestBrowserSession;
  host: PrBrowserHost;
  /** The app's zoom level. Not a factor anything multiplies by (see `bounds`): a change of it moves the frame, which is reported again. */
  zoom: number;
  onClose: () => void;
  onBack: () => void;
  onOpenExternal: () => void;
  /** The page closed itself; the session is over and nothing needs closing. */
  onClosedByPage: () => void;
}) {
  const overlayRef = useInertBackground<HTMLDivElement>(true);
  const frameRef = useRef<HTMLDivElement>(null);
  const covered = useCovered(overlayRef);
  const [failure, setFailure] = useState<string>();

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

  /* Open on the URL, and again on a new one (another agent's PR). */
  useLayoutEffect(() => {
    const rect = boundsRef.current();
    if (!rect) return;
    setFailure(undefined);
    host.open(session.url, rect).catch((error: unknown) => setFailure(error instanceof Error ? error.message : String(error)));
  }, [host, session.url]);

  /* Closed when this frame goes, whatever took it away. */
  useEffect(() => () => { void host.close().catch(() => undefined); }, [host]);

  /* Follow the frame. */
  useEffect(() => {
    const frame = frameRef.current;
    if (!frame) return;
    const report = () => {
      const rect = boundsRef.current();
      if (rect) void host.setBounds(rect).catch(() => undefined);
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

  useEffect(() => { void host.setVisible(!covered).catch(() => undefined); }, [covered, host]);

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
            <button type="button" className="pr-browser-control" onClick={onBack} title="Back" aria-label="Back"><ArrowLeft size={14} aria-hidden="true" /><span>Back</span></button>
            <button type="button" className="pr-browser-control" onClick={onOpenExternal} title="Open in your browser" aria-label="Open in browser"><ExternalLink size={14} aria-hidden="true" /><span>Open in browser</span></button>
            <button type="button" className="pr-browser-control" onClick={onClose} title="Close (Esc)" aria-label="Close pull request"><X size={14} aria-hidden="true" /><span>Close</span></button>
          </div>
        </header>
        <div ref={frameRef} className="pr-browser-frame" data-testid="pr-browser-frame">
          {failure ? (
            <p className="pr-browser-notice" role="alert">{failure}</p>
          ) : !host.available ? (
            <p className="pr-browser-notice">GitHub's page for {title} opens here in the app. Use Open in browser to see it now.</p>
          ) : null}
        </div>
      </section>
    </div>
  );
}

/** Whether the open browser is hidden under one of the app's dialogs — read at voice dispatch time. */
export function pullRequestBrowserCovered(): boolean {
  return document.querySelector("[data-pr-browser][data-covered]") !== null;
}
