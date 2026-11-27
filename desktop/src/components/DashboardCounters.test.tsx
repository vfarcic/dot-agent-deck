import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureFleet, createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import type { DeckRuntimeState, DeckSnapshot } from "../types";
import { countersWords } from "./DashboardCounters";
import { VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

/*
 * Issue #2312 follow-up — the dashboard's counters are one button that names
 * what needs someone and opens every counter: on hover (CSS), on a click, or
 * by voice through `show_dashboard_counters`; a second click, Escape, a press
 * outside it or a spoken `close` closes it.
 */

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <div data-testid={`terminal-${agentId}`} />,
}));

function microphone() {
  const queue: string[] = [];
  let recording = false;
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({
    state, capturedMs: 900, maxMs: 30_000, capped: false, speech: queue.length > 0,
    available: true, backend: "remote",
  });
  return {
    deliver: (text: string) => queue.push(text),
    voiceStart: vi.fn(async () => { recording = true; return status("recording"); }),
    voiceStatus: vi.fn(async () => status(recording && queue.length ? "done" : "recording")),
    voiceStop: vi.fn(async (): Promise<VoiceTranscriptionDto> => {
      recording = false;
      const transcript = queue.shift() ?? "";
      return { outcome: { kind: "heard", transcript, sentence: `Heard: “${transcript}”.` }, transcribeMs: 11, backend: "stub", audioMs: 900 };
    }),
    voiceCancel: vi.fn(async () => { recording = false; return status("idle"); }),
  };
}

function runtime(voice: ReturnType<typeof microphone>, fleet: DeckSnapshot[]) {
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => {
    if (transcript === "show the counters") {
      return { backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript, action: "show_dashboard_counters", invoke: "showDashboardCounters", params: [], sentence: "Showing the counters." } };
    }
    if (transcript === "close") {
      return { backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript, action: "close", invoke: "closeTopmost", params: [], sentence: "Closed." } };
    }
    return { backend: "stub", resolveMs: 21, outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` } };
  });
  return {
    mode: "fixture", desktopFeatures: fixtureDesktopFeatures(), snapshot: fleet[0], fleet,
    terminalData: {}, clearError: vi.fn(), runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined), reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: undefined })),
    saveSettings: vi.fn(async () => structuredClone(DEFAULT_DESKTOP_SETTINGS)),
    setZoom: vi.fn(async (level: number) => level),
    voiceCommands: vi.fn(async () => []),
    listDirectories: vi.fn(async () => ({ kind: "listing", path: "/home/dev", displayPath: "/home/dev", parent: "/home", entries: [], truncated: false })),
    newAgentOptions: vi.fn(async () => ({ kind: "deck", agents: [], authoringKinds: [], lastCommand: "claude" })),
    resolveVoice, ...voice,
  } as unknown as DeckRuntimeState;
}

async function speak(voice: ReturnType<typeof microphone>, words: string) {
  voice.deliver(words);
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

const counters = () => screen.getByTestId("dashboard-counters");
const button = () => screen.getByTestId("dashboard-counters-button");

describe("the dashboard's counters button (issue #2312 follow-up)", () => {
  beforeEach(() => { window.localStorage.clear(); });

  /** Scenario: the button reads the total and only what needs someone; with nothing to act on it reads the total alone. */
  it("names the total and what needs someone", () => {
    expect(countersWords({ known: true, agents: 6, needsInput: 1, errors: 2, decksUp: 1, decksTotal: 2 })).toEqual({
      total: "6 agents", totalCount: "6", totalWord: "agents",
      alerts: [
        { count: "1", word: "needs input", text: "1 needs input", className: "count-needs-input" },
        { count: "2", word: "error", text: "2 error", className: "count-error" },
        { count: "1/2", word: "daemons", text: "1/2 daemons", className: "count-failed" },
      ],
      label: "6 agents, 1 needs input, 2 error, 1/2 daemons",
    });
    expect(countersWords({ known: true, agents: 1, needsInput: 0, errors: 0, decksUp: 1, decksTotal: 1 })).toMatchObject({ total: "1 agent", alerts: [], label: "1 agent" });
    // No daemon answering: the total is not known and is not a zero.
    expect(countersWords({ known: false, agents: 0, needsInput: 0, errors: 0, decksUp: 0, decksTotal: 1 }).label).toBe("— agents, 0/1 daemons");
  });

  /** Scenario: on the fleet fixture the counters row is gone from the top bar; the button opens the popover with every counter on a click, and a second click, Escape and a press outside each close it. */
  it("opens on a click and closes on a second click, Escape or a press outside", () => {
    render(<DeckShell runtime={runtime(microphone(), createFixtureFleet("fleet"))} initialView={{ kind: "overview" }} />);
    expect(button()).toHaveAttribute("aria-expanded", "false");
    expect(counters()).not.toHaveClass("is-open");
    expect(button()).toHaveAccessibleName(/^Counters: \d+ agents/);

    fireEvent.click(button());
    expect(button()).toHaveAttribute("aria-expanded", "true");
    expect(counters()).toHaveClass("is-open");
    for (const name of ["agents", "working", "thinking", "compacting", "needs-input", "error", "waiting", "idle", "groups", "decks"]) {
      expect(screen.getByTestId("dashboard-counters-popover")).toContainElement(screen.getByTestId(`overview-count-${name}`));
    }

    fireEvent.click(button());
    expect(counters()).not.toHaveClass("is-open");

    fireEvent.click(button());
    fireEvent.keyDown(button(), { key: "Escape" });
    expect(counters()).not.toHaveClass("is-open");
    expect(button()).toHaveFocus();

    fireEvent.click(button());
    fireEvent.pointerDown(document.body);
    expect(counters()).not.toHaveClass("is-open");
  });

  /** Scenario: with the mouse over the button the counters show; a click opens them and a second click closes them with the mouse still there, so they are hidden and the button says so; leaving and coming back shows them again. */
  it("a second click hides the counters even while the mouse stays over the button", () => {
    render(<DeckShell runtime={runtime(microphone(), createFixtureFleet("fleet"))} initialView={{ kind: "overview" }} />);
    fireEvent.pointerOver(button(), { pointerType: "mouse" });
    expect(counters()).toHaveClass("is-shown");
    expect(button()).toHaveAttribute("aria-expanded", "true");

    fireEvent.click(button());
    expect(counters()).toHaveClass("is-open");
    fireEvent.click(button());
    expect(counters()).not.toHaveClass("is-shown");
    expect(button()).toHaveAttribute("aria-expanded", "false");

    fireEvent.pointerOut(button(), { pointerType: "mouse" });
    fireEvent.pointerOver(button(), { pointerType: "mouse" });
    expect(counters()).toHaveClass("is-shown");
  });

  /** Scenario: with an agent waiting for input in the fixture, the button itself carries the amber needs-input count, so it is visible without opening the popover. */
  it("shows a needs-input count on the button itself", () => {
    const fleet = createFixtureFleet("fleet");
    const waiting = fleet.filter((deck) => deck.connection.status === "connected").flatMap((deck) => deck.agents).filter((agent) => agent.status === "needs_input").length;
    expect(waiting).toBeGreaterThan(0);
    render(<DeckShell runtime={runtime(microphone(), fleet)} initialView={{ kind: "overview" }} />);
    expect(button()).toHaveTextContent(`${waiting} needs input`);
  });

  describe("by voice", () => {
    beforeEach(() => { vi.useFakeTimers(); });
    afterEach(() => vi.useRealTimers());

    /** Scenario: with voice on, “show the counters” opens the popover the button opens and reports it; “close” then closes it. */
    it("opens with “show the counters” and closes with “close”", async () => {
      const voice = microphone();
      render(<DeckShell runtime={runtime(voice, [createFixtureSnapshot("docs")])} initialView={{ kind: "overview" }} />);
      await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });

      await speak(voice, "show the counters");
      expect(counters()).toHaveClass("is-open");
      expect(button()).toHaveAttribute("aria-expanded", "true");
      expect(screen.getByTestId("voice-report")).toHaveTextContent("Showing the counters.");

      await speak(voice, "close");
      expect(counters()).not.toHaveClass("is-open");
    });

    /** Scenario: with the New agent dialog open, a “show the counters” that arrives opens nothing behind the dialog. */
    it("does not open the counters behind the New agent dialog", async () => {
      const voice = microphone();
      render(<DeckShell runtime={runtime(voice, [createFixtureSnapshot("docs")])} initialView={{ kind: "overview" }} />);
      await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
      await act(async () => { fireEvent.click(screen.getByTestId("overview-new-agent")); });
      expect(screen.getByRole("dialog")).toBeInTheDocument();

      await speak(voice, "show the counters");
      expect(counters()).not.toHaveClass("is-open");
    });
  });
});
