import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AntigravityNoticeDialog } from "../components/AntigravityNoticeDialog";
import { StarPromptDialog } from "../components/StarPromptDialog";
import { NOTICE_KEYS_ARM_MS } from "../lib/antigravityNotice";
import { STAR_KEYS_ARM_MS } from "../lib/starPrompt";
import { OVERLAY_LAYER, useInertBackground, VOICE_PEER_PROPS } from "./useInertBackground";

/**
 * PRD #211 — regressions for `useReminderKeys`, the keyboard the Antigravity
 * notice and the star reminder share: the two exceptions to its window-wide
 * fence (an overlay drawn over the reminder, and the voice surface), and that
 * reminders shown one after another leave no window listener behind.
 */

/** Wait until a reminder's keys choose. */
async function keysReady(dialog: HTMLElement): Promise<void> {
  await waitFor(() => expect(dialog).toHaveAttribute("data-keys-ready", "true"), { timeout: Math.max(NOTICE_KEYS_ARM_MS, STAR_KEYS_ARM_MS) * 5 });
}

/** An overlay drawn over the reminder, with a field of its own that takes keys. */
function Above({ onKey }: { onKey: (key: string) => void }) {
  const ref = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.dialog);
  return <div ref={ref} data-testid="above"><input data-testid="above-field" onKeyDown={(event) => onKey(event.key)} /></div>;
}

/** A control in the voice surface, which handles its own keys. */
function VoicePeer({ onKey }: { onKey: (key: string) => void }) {
  return <div {...VOICE_PEER_PROPS}><button data-testid="voice-choice" onKeyDown={(event) => onKey(event.key)}>1 choice</button></div>;
}

describe("a reminder's keyboard", () => {
  /** Scenario: the Antigravity notice is open and armed, and another dialog is drawn over it. The user types `d`, `l` and `Escape` into that dialog's field: each reaches the field, and the notice makes no choice. */
  it("leaves its keys to an overlay drawn over it", async () => {
    const onChoose = vi.fn();
    const onKey = vi.fn();
    function Host() {
      const [covered, setCovered] = useState(false);
      return <>
        <AntigravityNoticeDialog onChoose={onChoose} />
        <button data-testid="cover" onClick={() => setCovered(true)}>cover</button>
        {covered && <Above onKey={onKey} />}
      </>;
    }
    render(<Host />);
    const dialog = screen.getByRole("alertdialog");
    await keysReady(dialog);
    fireEvent.click(screen.getByTestId("cover"), { detail: 1 });
    const field = screen.getByTestId("above-field");
    for (const key of ["d", "l", "Escape"]) fireEvent.keyDown(field, { key });
    expect(onKey.mock.calls.map(([key]) => key)).toEqual(["d", "l", "Escape"]);
    expect(onChoose).not.toHaveBeenCalled();
  });

  /** Scenario: the star reminder is open and armed while the voice surface shows a numbered choice. The user presses `1` and `d` on that choice's button: both reach it, and the reminder makes no choice. */
  it("lets a key reach a voice surface control", async () => {
    const onChoose = vi.fn();
    const onKey = vi.fn();
    render(<><StarPromptDialog onChoose={onChoose} /><VoicePeer onKey={onKey} /></>);
    const dialog = screen.getByRole("alertdialog");
    await keysReady(dialog);
    const choice = screen.getByTestId("voice-choice");
    fireEvent.keyDown(choice, { key: "1" });
    fireEvent.keyDown(choice, { key: "d" });
    expect(onKey.mock.calls.map(([key]) => key)).toEqual(["1", "d"]);
    expect(onChoose).not.toHaveBeenCalled();
  });

  describe("its window listeners", () => {
    /** The capture-phase keydown and keyup listeners on `window` now, kept by wrapping its add and remove. */
    const capture = { keydown: new Set<EventListenerOrEventListenerObject>(), keyup: new Set<EventListenerOrEventListenerObject>() };
    const isCapture = (options?: boolean | AddEventListenerOptions | EventListenerOptions) => (typeof options === "boolean" ? options : options?.capture === true);

    afterEach(() => {
      vi.restoreAllMocks();
      capture.keydown.clear();
      capture.keyup.clear();
    });

    /** Scenario: the Antigravity notice and the star reminder each open and are answered with `l`, twice in turn. Each one on screen adds one capture keydown and one capture keyup listener to the window, each answer takes them away again, and once all four are gone a window shortcut gets its key. */
    it("leaves none behind across reminders shown one after another", async () => {
      const add = window.addEventListener.bind(window);
      const remove = window.removeEventListener.bind(window);
      vi.spyOn(window, "addEventListener").mockImplementation((type, listener, options) => {
        if ((type === "keydown" || type === "keyup") && isCapture(options)) capture[type].add(listener);
        add(type, listener, options);
      });
      vi.spyOn(window, "removeEventListener").mockImplementation((type, listener, options) => {
        if ((type === "keydown" || type === "keyup") && isCapture(options)) capture[type].delete(listener);
        remove(type, listener, options);
      });
      const counts = () => [capture.keydown.size, capture.keyup.size];

      const order = ["notice", "star", "notice", "star"] as const;
      function Host() {
        const [at, setAt] = useState(0);
        const next = () => setAt((value) => value + 1);
        if (order[at] === "notice") return <AntigravityNoticeDialog onChoose={next} />;
        if (order[at] === "star") return <StarPromptDialog onChoose={next} />;
        return <p data-testid="none">none</p>;
      }

      const baseline = counts();
      render(<Host />);
      for (const shown of order) {
        const dialog = screen.getByRole("alertdialog");
        expect(dialog.dataset.testid === "star-prompt-dialog", shown).toBe(shown === "star");
        expect(counts(), `${shown} on screen`).toEqual([baseline[0] + 1, baseline[1] + 1]);
        await keysReady(dialog);
        fireEvent.keyDown(dialog, { key: "l" });
      }
      expect(screen.getByTestId("none")).toBeVisible();
      expect(counts(), "after the last answer").toEqual(baseline);

      const shortcut = vi.fn();
      window.addEventListener("keydown", shortcut);
      try {
        fireEvent.keyDown(document.body, { key: "?" });
        expect(shortcut).toHaveBeenCalledOnce();
      } finally {
        window.removeEventListener("keydown", shortcut);
      }
    });
  });
});
