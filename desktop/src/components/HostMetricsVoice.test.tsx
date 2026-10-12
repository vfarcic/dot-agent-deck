import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureFleet, createFixtureSnapshot, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResolvedParamDto, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import { chooseHostDeck, HOST_VOICE_NONE, HOST_VOICE_NOT_SHOWN } from "../lib/hostMetrics";
import type { DeckRuntimeState, DeckSnapshot } from "../types";
import { VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

/*
 * PR #1672 — the Host popover by voice: the `show_host_metrics` row dispatches
 * the registry entry the Host button's click runs, and the dashboard decides
 * which daemon's popover opens.
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

/** A daemon param as Rust resolves the row's optional `deck_ref`. */
function deck(value: string, label: string): VoiceResolvedParamDto {
  return { name: "deck", kind: "deck_ref", spoken: label, value, label };
}

/** What the stub backend answers for each utterance: the row, with the daemon it resolved. */
const SAID: Record<string, VoiceResolvedParamDto[]> = {
  "show host": [],
  "show host for the build box": [deck(FIXTURE_REMOTE_DAEMON_ID, "dev@build-box")],
  "host for the runner": [deck(FIXTURE_UNREACHABLE_DAEMON_ID, "ci@runner-7")],
};

function runtime(voice: ReturnType<typeof microphone>, fleet: DeckSnapshot[]) {
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => {
    const params = SAID[transcript];
    if (params) {
      return { backend: "stub", resolveMs: 21, outcome: {
        kind: "dispatch", transcript, action: "show_host_metrics", invoke: "showHostMetrics", params,
        sentence: "Showing the Host figures.",
      } };
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
    resolveVoice, ...voice,
  } as unknown as DeckRuntimeState;
}

async function turnOnVoice() {
  await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
  expect(screen.getByTestId("voice-trigger")).toHaveAttribute("aria-pressed", "true");
}

async function speak(voice: ReturnType<typeof microphone>, words: string) {
  voice.deliver(words);
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

function section(deckId: string): HTMLElement {
  const found = Array.from(document.querySelectorAll<HTMLElement>("[data-testid='daemon-group']")).find((group) => group.getAttribute("data-daemon-id") === deckId);
  if (!found) throw new Error(`no daemon section for ${deckId}`);
  return found;
}

describe("the Host popover by voice (PR #1672)", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => vi.useRealTimers());

  /** Scenario: with one daemon on the dashboard, “show host” opens that daemon's Host popover, the same one its Host button opens. */
  it("opens the only daemon's Host figures", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice, [createFixtureSnapshot("docs")])} initialView={{ kind: "overview" }} />);
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    await turnOnVoice();
    await speak(voice, "show host");
    expect(screen.getByTestId("host-metrics-popover")).toBeInTheDocument();
    expect(screen.getByTestId("host-metrics-button")).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Showing the Host figures.");
  });

  /** Scenario: with two connected daemons and one disconnected, “show host” opens nothing and asks which daemon; “show host for the build box” opens only that daemon's popover; naming the disconnected daemon says it is not connected and opens nothing. */
  it("asks which daemon among several and opens the one named", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice, createFixtureFleet("fleet"))} initialView={{ kind: "overview" }} />);
    await turnOnVoice();

    await speak(voice, "show host");
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/Several daemons are on the dashboard: say which one, like “host Local daemon”/);

    await speak(voice, "show host for the build box");
    expect(within(section(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("host-metrics-popover")).toBeInTheDocument();
    expect(within(section(FIXTURE_DAEMON_ID)).queryByTestId("host-metrics-popover")).toBeNull();
    expect(screen.getAllByTestId("host-metrics-popover")).toHaveLength(1);

    await speak(voice, "host for the runner");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/is not connected, so it has no Host figures to show/);
    // The popover already open stays as it was.
    expect(within(section(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("host-metrics-popover")).toBeInTheDocument();
  });

  /** Scenario: the Host button's own click opens its popover through the same action, one popover at a time, and a second click closes it. */
  it("keeps the button's click on the same action", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice, createFixtureFleet("fleet"))} initialView={{ kind: "overview" }} />);
    const local = within(section(FIXTURE_DAEMON_ID)).getByTestId("host-metrics-button");
    const remote = within(section(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("host-metrics-button");
    fireEvent.click(local);
    expect(within(section(FIXTURE_DAEMON_ID)).getByTestId("host-metrics-popover")).toBeInTheDocument();
    fireEvent.pointerDown(remote);
    fireEvent.click(remote);
    expect(within(section(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("host-metrics-popover")).toBeInTheDocument();
    expect(screen.getAllByTestId("host-metrics-popover")).toHaveLength(1);
    fireEvent.click(remote);
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
  });

  /** Scenario: the build box's Host popover is open when that daemon disconnects, and then again when it stops sending figures; each time it comes back, its Host button is there and the popover stays closed until it is clicked. */
  it("does not reopen a popover whose daemon went away and came back", async () => {
    const voice = microphone();
    const fleet = createFixtureFleet("fleet");
    const at = fleet.findIndex((deck) => deck.connection.deckId === FIXTURE_REMOTE_DAEMON_ID);
    const remote = fleet[at];
    const shell = (decks: DeckSnapshot[]) => <DeckShell runtime={runtime(voice, decks)} initialView={{ kind: "overview" }} />;
    const replaced = (deck: DeckSnapshot) => fleet.map((candidate, index) => (index === at ? deck : candidate));
    const view = render(shell(fleet));
    fireEvent.click(within(section(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("host-metrics-button"));
    expect(within(section(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("host-metrics-popover")).toBeInTheDocument();

    for (const away of [
      { ...remote, connection: { ...remote.connection, status: "disconnected" as const } },
      { ...remote, hostMetrics: undefined },
    ]) {
      view.rerender(shell(replaced(away)));
      expect(within(section(FIXTURE_REMOTE_DAEMON_ID)).queryByTestId("host-metrics-button")).toBeNull();
      view.rerender(shell(fleet));
      expect(within(section(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("host-metrics-button")).toHaveAttribute("aria-expanded", "false");
      expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    }
  });
});

describe("which daemon's Host figures a request opens (PR #1672)", () => {
  const local = { deckId: "local", name: "Local daemon", connected: true, hasFigures: true };
  const remote = { deckId: "remote", name: "build box", connected: true, hasFigures: true };

  /** Scenario: a named daemon opens when it shows a Host button; one that is not connected, has sent no figures yet, or is not on the dashboard is refused in words; with none named the only daemon showing a button opens, several are asked about, and none is said. */
  it("opens a named or the only daemon, and refuses the rest in words", () => {
    expect(chooseHostDeck([local, remote], "remote")).toEqual({ open: "remote" });
    expect(chooseHostDeck([local, { ...remote, connected: false }], "remote")).toEqual({ refused: "“build box” is not connected, so it has no Host figures to show." });
    expect(chooseHostDeck([local, { ...remote, hasFigures: false }], "remote")).toEqual({ refused: "“build box” has not sent its Host figures yet." });
    expect(chooseHostDeck([local], "all-daemons")).toEqual({ refused: HOST_VOICE_NOT_SHOWN });
    expect(chooseHostDeck([local, { ...remote, connected: false }], undefined)).toEqual({ open: "local" });
    expect(chooseHostDeck([local, remote], undefined)).toEqual({ refused: "Several daemons are on the dashboard: say which one, like “host Local daemon”." });
    expect(chooseHostDeck([{ ...local, connected: false }], undefined)).toEqual({ refused: HOST_VOICE_NONE });
  });
});
