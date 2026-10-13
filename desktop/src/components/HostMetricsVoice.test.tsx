import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureFleet, createFixtureSnapshot, fixtureHostMetrics, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResolvedParamDto, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import { chooseHostDeck, HOST_VOICE_NONE, HOST_VOICE_NOT_SHOWN } from "../lib/hostMetrics";
import { VOICE_ACTIONS, type VoiceDispatchTarget } from "../lib/voiceActions";
import type { DeckRuntimeState, DeckSnapshot } from "../types";
import { VOICE_NOTHING_TO_CLOSE, VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

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

/** What resolves to the `close` row: the popover's own words, and the bare forms. */
const CLOSE = ["close", "close it", "close host", "close resource usage", "hide resource usage"];

/** The other rows the precedence cases open things with. */
const OTHER: Record<string, { action: string; invoke: string; sentence: string; params?: VoiceResolvedParamDto[] }> = {
  "open settings": { action: "open_settings", invoke: "openSettings", sentence: "Opening settings." },
  "what can I say?": { action: "list_commands", invoke: "showVoiceCommands", sentence: "Here is what you can say." },
  "open the planner": { action: "open_agent", invoke: "openAgent", sentence: "Opening Planner.", params: [{ name: "agent", kind: "agent_ref", spoken: "planner", value: "planner", label: "Planner" }] },
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
    if (CLOSE.includes(transcript)) {
      return { backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript, action: "close", invoke: "closeTopmost", params: [], sentence: "Closed." } };
    }
    const other = OTHER[transcript];
    if (other) {
      return { backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript, action: other.action, invoke: other.invoke, params: other.params ?? [], sentence: other.sentence } };
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
    newAgentOptions: vi.fn(async () => ({ kind: "deck", agents: [], experimental: false, authoringKinds: [], lastCommand: "claude" })),
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

/** The fixture daemon with its agents (Planner among them) and its Host figures. */
const withPlanner = () => [{ ...createFixtureSnapshot("connected"), hostMetrics: fixtureHostMetrics("local") }];

describe("closing the Host popover by voice (PR #1672 round 10)", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => vi.useRealTimers());

  /** Scenario: with the Host popover open on the dashboard, each of "close", "close it", "close host", "close resource usage" and "hide resource usage" closes it and says "Closed."; the dashboard stays, and a "close" with nothing open then says there is nothing to close. */
  it("closes the open popover and leaves the dashboard", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice, [createFixtureSnapshot("docs")])} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    for (const words of CLOSE) {
      await speak(voice, "show host");
      expect(screen.getByTestId("host-metrics-popover")).toBeInTheDocument();
      await speak(voice, words);
      expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
      expect(screen.getByTestId("host-metrics-button")).toHaveAttribute("aria-expanded", "false");
      expect(screen.getByTestId("voice-report")).toHaveTextContent("Closed.");
      expect(screen.getByTestId("overview-table-region")).toBeVisible();
    }
    await speak(voice, "close");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(VOICE_NOTHING_TO_CLOSE);
  });

  /** Scenario: open the Host popover by voice, then — each by voice, so nothing presses outside it — Settings, an agent's pane and the list of spoken commands over it. Each "close" takes the top one: the list, the pane, Settings, and only then the popover. */
  it("closes what is over the popover first", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice, withPlanner())} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    await speak(voice, "show host");
    await speak(voice, "open settings");
    await speak(voice, "open the planner");
    await speak(voice, "what can I say?");
    const settings = () => screen.queryByRole("dialog", { name: "Settings" });
    expect(screen.getByTestId("voice-help")).toBeInTheDocument();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    expect(settings()).not.toBeNull();
    expect(screen.getByTestId("host-metrics-popover")).toBeInTheDocument();

    await speak(voice, "close");
    expect(screen.queryByTestId("voice-help")).toBeNull();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    await speak(voice, "close it");
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(settings()).not.toBeNull();
    await speak(voice, "close");
    expect(settings()).toBeNull();
    expect(screen.getByTestId("host-metrics-popover")).toBeInTheDocument();
    await speak(voice, "close host");
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Closed.");
  });

  /** Scenario: with the Host popover open, the New agent dialog opens over it from its button; "close" closes the dialog and leaves the popover, and the next "close" closes the popover. */
  it("closes the New agent dialog before the popover", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice, withPlanner())} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    await speak(voice, "show host");
    fireEvent.click(screen.getByTestId("daemon-new-agent"));
    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(screen.getByTestId("host-metrics-popover")).toBeInTheDocument();
    await speak(voice, "close");
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
    expect(screen.getByTestId("host-metrics-popover")).toBeInTheDocument();
    await speak(voice, "close");
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
  });

  /** Scenario: with the Host popover open, an agent's Close button asks for confirmation over the dashboard; "close" then leaves the popover open behind that confirmation rather than closing something the user cannot see. */
  it("does not reach behind a stop confirmation", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice, withPlanner())} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    await speak(voice, "show host");
    fireEvent.click(screen.getAllByRole("button", { name: /^Close .* agent$/ })[0]);
    expect(screen.getByRole("alertdialog")).toBeInTheDocument();
    await speak(voice, "close");
    expect(screen.getByTestId("host-metrics-popover")).toBeInTheDocument();
    expect(screen.getByRole("alertdialog")).toBeInTheDocument();
  });
});

describe("closeTopmost's precedence with the Host popover (PR #1672 round 10)", () => {
  /** A context with the always-present members recorded, plus whatever layers are open. */
  function context(open: Partial<Record<"dismissVoiceOverlay" | "closeNewAgent" | "closeSettings" | "closeHostMetrics", true>>) {
    const calls: string[] = [];
    const record = (name: string) => () => { calls.push(name); return undefined; };
    const built = {
      closeAgentView: record("closeAgentView"),
      reportNothingToClose: record("reportNothingToClose"),
      reportRefused: record("reportRefused"),
      ...Object.fromEntries(Object.keys(open).map((name) => [name, record(name)])),
    };
    return { calls, built };
  }
  const run = (open: Parameters<typeof context>[0], agentViewOpen = false) => {
    const { calls, built } = context(open);
    VOICE_ACTIONS.closeTopmost.run(built as never, { agentViewOpen } as VoiceDispatchTarget);
    return calls;
  };

  /** Scenario: the popover alone is closed; under every other layer it waits, closing only once each of them has gone. */
  it("closes the popover only when nothing else is open", () => {
    expect(run({ closeHostMetrics: true })).toEqual(["closeHostMetrics"]);
    expect(run({ closeHostMetrics: true, closeSettings: true })).toEqual(["closeSettings"]);
    expect(run({ closeHostMetrics: true }, true)).toEqual(["closeAgentView"]);
    expect(run({ closeHostMetrics: true, closeNewAgent: true })).toEqual(["closeNewAgent"]);
    expect(run({ closeHostMetrics: true, dismissVoiceOverlay: true })).toEqual(["dismissVoiceOverlay"]);
    expect(run({})).toEqual(["reportNothingToClose"]);
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
