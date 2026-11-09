import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_DESKTOP_SETTINGS, mapDesktopSnapshot, normalizeDesktopSettings, questionNotifyAfterMinutes, type DesktopSettingsDto, type DesktopSnapshotDto } from "../lib/bridge";
import type { AgentSession } from "../types";
import { AgentTile } from "./AgentTile";
import { NotificationsPanel } from "./NotificationsPanel";

vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

type DtoAgent = DesktopSnapshotDto["agents"][number];

const RAISED = 1_767_225_600_000;

function liveAgent(overrides: Partial<DtoAgent>): AgentSession {
  const agent = {
    id: "7", paneId: "pane-7", displayName: "reviewer", agentType: "claude_code" as const, cliName: "claude",
    incarnation: "question-process-a", rows: 24, cols: 80, status: "needs_input" as const, toolCount: 3,
    cwd: "/work/project", tab: { kind: "dashboard" as const },
    ...overrides,
  } as DtoAgent;
  return mapDesktopSnapshot({
    connection: { status: "connected", deckId: "deck-0000000000002075", socketPath: "/tmp/test.sock",
      deckKind: "local", clientProtocolVersion: 11, clientBuildVersion: "test" },
    agents: [agent], protocolVersion: 11, source: "daemon",
  }).agents[0];
}

function tile(agent: AgentSession) {
  return render(<AgentTile agent={agent} presentation="tile" showDetails={false} mode="live"
    selected={false} tab="terminal" evidence={[]} onSelect={vi.fn()}
    onTabChange={vi.fn()} onTerminalInput={vi.fn(async () => {})}
    onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()}
    onRename={vi.fn(async () => {})} />);
}

const QUESTION = {
  id: "q-1", kind: "choice", channel: "held", answerable: true,
  questions: [{ prompt: "Which colour?", multiSelect: false, options: [] }],
};

describe("a tile with a question waiting (issue #2075)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(RAISED + 12 * 60_000);
  });
  afterEach(() => vi.useRealTimers());

  /**
   * Scenario (question/desktop/037): an agent has asked a question twelve
   * minutes ago and nobody has answered; its tile shows a QUESTION badge
   * reading "QUESTION 12m", which reads "QUESTION 13m" a minute later, and a
   * tile with no question shows no badge.
   */
  it("question/desktop/037: shows QUESTION and how long it has waited, and keeps counting", () => {
    tile(liveAgent({ pendingQuestion: { ...QUESTION, raisedAtMs: RAISED } } as Partial<DtoAgent>));
    expect(screen.getByTestId("question-badge-7")).toHaveTextContent(/^QUESTION 12m$/);
    act(() => { vi.advanceTimersByTime(60_000); });
    expect(screen.getByTestId("question-badge-7")).toHaveTextContent(/^QUESTION 13m$/);
  });

  it("question/desktop/037: a tile with no question has no badge, and an untimed question has no age", () => {
    const { unmount } = tile(liveAgent({}));
    expect(screen.queryByTestId("question-badge-7")).toBeNull();
    unmount();
    tile(liveAgent({ pendingQuestion: QUESTION } as Partial<DtoAgent>));
    expect(screen.getByTestId("question-badge-7")).toHaveTextContent(/^QUESTION$/);
  });
});

describe("the Notifications setting (issue #2075)", () => {
  /**
   * Scenario (question/desktop/038): Settings → Notifications reads "After 10
   * minutes" on a document that says nothing about it; choosing Off saves
   * `[questions] notify = 0`; a settings document without the section keeps
   * it absent through the bridge, so a save from this window never writes a
   * default over the file, and a hand-edited value is kept and offered.
   */
  it("question/desktop/038: defaults to ten minutes, saves the choice, and keeps an absent section absent", () => {
    const onSave = vi.fn();
    render(<NotificationsPanel settings={DEFAULT_DESKTOP_SETTINGS} onSave={onSave} mode="live" />);
    const select = screen.getByTestId("question-notify") as HTMLSelectElement;
    expect(select.value).toBe("10");
    expect(select.selectedOptions[0]).toHaveTextContent("After 10 minutes");
    fireEvent.change(select, { target: { value: "0" } });
    expect(onSave).toHaveBeenCalledWith({ ...DEFAULT_DESKTOP_SETTINGS, questions: { notify: 0 } });

    expect(normalizeDesktopSettings({ version: 1, appearance: { mode: "dark" } }).questions).toBeUndefined();
    expect(normalizeDesktopSettings({ version: 1, questions: { notify: 7 } }).questions).toEqual({ notify: 7 });
    expect(normalizeDesktopSettings({ version: 1, questions: { notify: -1 } }).questions).toEqual({ notify: 10 });
    expect(questionNotifyAfterMinutes(DEFAULT_DESKTOP_SETTINGS)).toBe(10);

    const handEdited: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS, questions: { notify: 7 } };
    render(<NotificationsPanel settings={handEdited} onSave={vi.fn()} mode="live" />);
    const edited = screen.getAllByTestId("question-notify")[1] as HTMLSelectElement;
    expect(edited.value).toBe("7");
    expect(edited.selectedOptions[0]).toHaveTextContent("After 7 minutes");
  });
});
