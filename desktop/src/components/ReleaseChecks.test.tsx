import commandTable from "../../src-tauri/src/voice/commands.toml?raw";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import words from "../../../tests/fixtures/inbox-display.json";
import { DeckShell } from "../App";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResultDto, type VoiceStatusDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";
import { VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

// Test-only DTO/action assumptions are documented in fixtures/release-check/desktop.md.
// The shell, components, action registry and voice dispatcher are production code.
// Only external runtime operations and microphone/resolver results are substituted.
const registryDispatch = vi.hoisted(() => vi.fn<(id: string) => void>());
vi.mock("../lib/voiceActions", async (original) => {
  const actual = await original<typeof import("../lib/voiceActions")>();
  // Replace the callable on the same object dispatchVoiceAction closes over,
  // so both direct clicks and the dynamic voice dispatcher are observed.
  for (const [id, originalEntry] of Object.entries(actual.VOICE_ACTIONS)) {
    const entry = originalEntry as unknown as { run: (...args: unknown[]) => unknown };
    const run = entry.run;
    entry.run = (...args: unknown[]) => {
      registryDispatch(id);
      return run(...args);
    };
  }
  return actual;
});
vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <div className="terminal-viewport" role="group" aria-label="Fixture terminal" tabIndex={-1}><textarea aria-label="Fixture terminal input" /></div> }));
import { VOICE_ACTIONS } from "../lib/voiceActions";

const shared = words.releaseChecks;
const identity = "0123456789abcdef";
const secondIdentity = "fedcba9876543210";
const release = "2.1.313";
const olderRelease = "2.1.312";
// One budget in both clients, measured AFTER escaping, including the marker.
const inspectionCodePoints = 16_384;
const truncationMarker = (bytes: number) => shared.previewTruncated.replace("{bytes}", String(bytes));
type Row = {
  agent: string; release: string; identity: string; state: string | null; progress: string;
  queuePosition?: number; queuedAtMs?: number; fact?: string; reason?: string;
};
type Request = { type: string; deckId?: string; agent?: string; release?: string; mode?: string; dir?: string };
type Reply = { ok: boolean; releaseChecks?: Row[]; mode?: string; files?: { name: string; content: string }[] };
function row(state: string | null = "passed", extra: Partial<Row> = {}): Row {
  return { agent: "claude", release, identity, state, progress: "complete", queuedAtMs: 1767225600000, ...extra };
}
function fill(template: string, values: Record<string, string> = {}): string {
  return template.replace(/\{(\w+)\}/g, (token, key: string) => ({ agent: "Claude Code", release, ...values })[key] ?? token);
}

const files = [
  { name: "record.json", content: '{"verdict":"passed"}' },
  { name: "trace.txt", content: "trace from the isolated check\nsecond line" },
  { name: "README", content: "Exported for local inspection; share only after reviewing." },
];

function mount(options: { rows?: Row[]; mode?: string; capabilities?: string[] | null; cause?: string; files?: typeof files; error?: string } = {}) {
  const snapshot = createFixtureSnapshot("connected");
  const selected = snapshot.agents[0];
  Object.assign(selected, {
    paneId: "release-check-pane", incarnation: "release-check-generation", status: "idle", turn: "idle",
    cli: options.cause === "no_reference" ? "codex" : "claude",
    agentType: options.cause === "no_reference" ? "codex" : "claude_code", agentVersion: release, displayName: "Release check worker", role: "Release check worker",
    inbox: { queued: 1, fetched: 0, undeliverable: 0, wake: "doorbell", wakeHold: "cannot_type",
      holdDetail: { cause: `release_check_${options.cause ?? "tripped"}`, agent: options.cause === "no_reference" ? "codex" : "claude", release, fact: "layout" } },
  });
  snapshot.agents = [selected];
  const records = structuredClone(options.rows ?? [row()]);
  // Keep adapter-owned records separate: mutating its store must not update
  // the rendered snapshot without production consuming the accepted reply.
  Object.assign(snapshot, { releaseChecks: structuredClone(records), releaseCheckMode: options.mode ?? "auto" });
  if (options.capabilities !== null) Object.assign(snapshot.connection, { capabilities: options.capabilities ?? ["release_check"] });
  const deckId = snapshot.connection.deckId!;
  const requests: Request[] = [];
  const runAction = vi.fn(async (request: Request): Promise<Reply> => {
    // Reject accidental identity selectors/extra fields at the external boundary.
    const allowed: Record<string, string[]> = {
      release_check_run: ["type", "deckId", "agent"], release_check_cancel: ["type", "deckId"],
      release_check_forget: ["type", "deckId", "agent", "release"],
      release_check_export_preview: ["type", "deckId", "agent", "release"],
      release_check_export: ["type", "deckId", "agent", "release", "dir"], release_check_mode: ["type", "deckId", "mode"],
    };
    if (!allowed[request.type]) throw new Error(`Unexpected release-check operation: ${request.type}`);
    expect(Object.keys(request).sort()).toEqual([...allowed[request.type]].sort());
    expect(request.deckId).toBe(deckId);
    requests.push(structuredClone(request));
    if (options.error) throw new Error(options.error);
    if (request.type === "release_check_export_preview") return { ok: true, files: options.files ?? files };
    if (request.type === "release_check_forget") {
      for (const record of records) if (record.agent === request.agent && record.release === request.release) record.state = "forgotten";
    }
    if (request.type === "release_check_run") {
      records[0] = row(null, { progress: "running", queuePosition: 1 });
    }
    if (request.type === "release_check_cancel" || (request.type === "release_check_mode" && request.mode === "off")) {
      for (const record of records) if (record.progress === "running" || (request.mode === "off" && record.progress === "queued")) Object.assign(record, { state: "inconclusive", progress: "complete", reason: "runner_stopped" });
    }
    return { ok: true, releaseChecks: structuredClone(records), ...(request.mode ? { mode: request.mode } : {}) };
  });
  const runtime = {
    mode: "fixture", desktopFeatures: fixtureDesktopFeatures("?experimental=1"), snapshot, fleet: [snapshot],
    terminalData: {}, clearError: vi.fn(), runAction,
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined), reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("not used"); }),
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS) })),
    saveSettings: vi.fn(async (settings) => settings), setZoom: vi.fn(async (level: number) => level),
  } as unknown as DeckRuntimeState;
  let said: string | undefined;
  let outcome: VoiceResultDto["outcome"];
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({
    state, capturedMs: 900, maxMs: 30_000, capped: false, available: true, backend: "remote",
  });
  Object.assign(runtime, {
    voiceStart: vi.fn(async () => status("recording")),
    voiceStatus: vi.fn(async () => status(said ? "done" : "recording")),
    voiceStop: vi.fn(async () => {
      const transcript = said!; said = undefined;
      return { outcome: { kind: "heard", transcript, sentence: transcript }, transcribeMs: 1, backend: "stub", audioMs: 900 };
    }),
    voiceCancel: vi.fn(async () => status("idle")), voiceCommands: vi.fn(async () => []),
    resolveVoice: vi.fn(async (): Promise<VoiceResultDto> => ({ outcome, resolveMs: 1, backend: "stub" })),
  });
  const view = render(<DeckShell runtime={runtime} initialView={{ kind: "deck" }} />);
  return {
    ...view, requests, runAction, runtime, deckId,
    async speak(phrase: string, invoke: string, action: string) {
      outcome = { kind: "dispatch", transcript: phrase, invoke, action, params: [], sentence: "Release check action selected." };
      if (screen.getByTestId("voice-trigger").getAttribute("aria-pressed") !== "true") {
        await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
      }
      said = phrase;
      await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
      // Establish that RED is downstream of voice resolution, rather than
      // an idle synthetic microphone that never delivered an utterance.
      expect(runtime.resolveVoice).toHaveBeenCalledWith(phrase);
    },
  };
}

function card() { return within(screen.getByRole("article", { name: "Release check worker" })); }
function details() { return screen.getByRole("dialog", { name: "Release checks" }); }
function click(name: string | RegExp, root?: HTMLElement) { fireEvent.click((root ? within(root) : screen).getByRole("button", { name })); }
function openDetails() { click("Details", screen.getByRole("article", { name: "Release check worker" })); return details(); }
function selectRecord(shortIdentity: string) { click(new RegExp(shortIdentity), details()); }
async function preview() {
  click("Export", details());
  const dialog = await screen.findByRole("dialog", { name: "Export release check" });
  expect(dialog).toHaveTextContent(shared.exportInspection);
  return dialog;
}

beforeEach(() => {
  window.localStorage.clear(); window.sessionStorage.clear(); registryDispatch.mockClear();
});
afterEach(() => { vi.useRealTimers(); });

describe("inbox/release-check/004 — desktop Details and controls", () => {
  /// Scenario: Open Details for each journal state; the selected row shows its own result in the shared TUI words, even though the pane has a tripped/layout hold.
  it.each([
    [row("passed"), shared.details.passed],
    [row("differs", { fact: "layout" }), fill(shared.details.differs, { fact: shared.facts.layout })],
    [row("inconclusive", { reason: "launch" }), fill(shared.details.inconclusive, { reason: shared.reasons.launch })],
    [row("unavailable", { reason: "no_process_containment" }), fill(shared.details.unavailable, { reason: shared.reasons.no_process_containment })],
    [row("revoked", { fact: "layout" }), fill(shared.details.revoked, { fact: shared.facts.layout })],
    [row("forgotten"), shared.details.forgotten],
    [row("future_state"), /unknown|unrecognised|not understood/i],
    [row("passed", { progress: "future_progress" }), /unknown|unrecognised|not understood/i],
  ])("displays selected result %j", (record, expected) => {
    mount({ rows: [record as Row] });
    const dialog = openDetails();
    expect(dialog).toHaveTextContent(expected as string | RegExp);
    expect(dialog).toHaveTextContent(release);
    expect(dialog).toHaveTextContent(identity);
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckDetails");
  });

  /// Scenario: Open Details on a queued or running check, including a last-place and absent queue position; show progress and only the position the daemon actually supplied.
  it.each([
    ["running", 1, shared.details.running], ["queued", 2, shared.details.queued],
    ["queued", 8, shared.details.queued], ["queued", undefined, shared.details.queued],
  ] as const)("shows %s progress at %s", (progress, queuePosition, expected) => {
    mount({ rows: [row("passed", { state: null, progress, queuePosition })] });
    const dialog = openDetails();
    expect(dialog).toHaveTextContent(expected);
    if (queuePosition === undefined) expect(dialog).toHaveTextContent(/queue position (?:not available|unknown)/i);
    else expect(dialog).toHaveTextContent(new RegExp(`queue position[: ]+${queuePosition}\\b`, "i"));
  });

  /// Scenario: Select an inconclusive row, an unavailable row, then rows with unfamiliar fact/reason values. Each explanation follows the selected record rather than the pane's layout diagnostic.
  it("uses each selected record's own diagnostic and tolerant fallbacks", () => {
    mount({ rows: [row("inconclusive", { reason: "launch" }), row("unavailable", { identity: secondIdentity, reason: "no_process_containment" }),
      row("differs", { identity: "aaaaaaaaaaaaaaaa", fact: "future_fact" }), row("inconclusive", { identity: "bbbbbbbbbbbbbbbb", reason: "future_reason" })] });
    openDetails();
    for (const [selected, expected] of [
      [identity, fill(shared.details.inconclusive, { reason: shared.reasons.launch })],
      [secondIdentity, fill(shared.details.unavailable, { reason: shared.reasons.no_process_containment })],
      ["aaaaaaaaaaaaaaaa", fill(shared.details.differs, { fact: shared.facts.other })],
      ["bbbbbbbbbbbbbbbb", fill(shared.details.inconclusive, { reason: shared.reasons.other })],
    ]) {
      selectRecord(selected);
      expect(details()).toHaveTextContent(expected);
      expect(details()).not.toHaveTextContent(shared.facts.layout);
    }
  });

  /// Scenario: In ask mode an unchecked release waits for approval with the proposed shared sentence. Re-check from its card starts checking and changes the visible Details state.
  it("approves ask mode from the hold line and shows the running reply", async () => {
    const app = mount({ rows: [], mode: "ask", cause: "running" });
    expect(card().getByTestId(/inbox-notice/)).toHaveTextContent(fill(shared.card.approval_proposal));
    click("Re-check", screen.getByRole("article", { name: "Release check worker" }));
    await waitFor(() => expect(app.requests).toEqual([{ type: "release_check_run", deckId: app.deckId, agent: "claude" }]));
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckRun");
    expect(openDetails()).toHaveTextContent(shared.details.running);
  });

  /// Scenario: Select an older identity and press Re-check; the external request names the agent only and the current release starts running in Details.
  it("rechecks the current identity even when an older release is selected", async () => {
    const app = mount({ rows: [row(), row("passed", { release: "2.1.312", identity: secondIdentity })] });
    openDetails(); selectRecord(secondIdentity);
    expect(details()).toHaveTextContent(shared.details.recheck_current);
    click("Re-check", details());
    await waitFor(() => expect(app.requests).toEqual([{ type: "release_check_run", deckId: app.deckId, agent: "claude" }]));
    expect(details()).toHaveTextContent(shared.details.running);
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckRun");
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckSelect");
  });

  /// Scenario: Select a completed row while a different check runs; Cancel opens a confirmation focused on shared Back, so default activation preserves the running check. Reopen and explicitly accept to stop the running check without an identity selector.
  it("cancels the running check independently of row selection", async () => {
    const app = mount({ rows: [row("passed", { release: "2.1.312" }), row("passed", { identity: secondIdentity, state: null, progress: "running", queuePosition: 1 })] });
    openDetails(); selectRecord(identity);
    expect(details()).toHaveTextContent(shared.details.cancel_running);
    click("Cancel", details());
    expect(screen.getByRole("alertdialog", { name: /cancel.*check/i })).toHaveTextContent(shared.cancelConfirmation);
    const dismiss = within(screen.getByRole("alertdialog", { name: /cancel.*check/i })).getByRole("button", { name: shared.confirmDismiss });
    expect(dismiss).toHaveFocus();
    // jsdom does not synthesize a native button click from keyDown(Enter).
    // Activate the focused button explicitly to model that default action.
    fireEvent.click(document.activeElement!);
    expect(screen.queryByRole("alertdialog", { name: /cancel.*check/i })).toBeNull();
    expect(app.requests).toEqual([]);
    click("Cancel", details()); click("Cancel check", screen.getByRole("alertdialog", { name: /cancel.*check/i }));
    await waitFor(() => expect(app.requests).toEqual([{ type: "release_check_cancel", deckId: app.deckId }]));
    selectRecord(secondIdentity);
    expect(details()).toHaveTextContent(fill(shared.details.inconclusive, { reason: shared.reasons.runner_stopped }));
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckCancel");
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckConfirmCancel");
  });

  /// Scenario: Select an older release with one or two identities; the multi-identity confirmation names its release-wide scope. Escape and default Back activation preserve the records, then explicit acceptance forgets the selected older release, preserves the current release, and offers only Re-check and Close on forgotten records.
  it.each([false, true])("forgets release-wide, several identities=%s", async (several) => {
    const app = mount({ rows: [row(), row("passed", { release: olderRelease, identity: secondIdentity }), ...(several ? [row("passed", { release: olderRelease, identity: "aaaaaaaaaaaaaaaa" })] : [])] });
    openDetails(); selectRecord(secondIdentity); click("Forget", details());
    let confirmation = screen.getByRole("alertdialog", { name: /forget/i });
    expect(within(confirmation).getByRole("button", { name: shared.confirmDismiss })).toHaveFocus();
    expect(confirmation).toHaveTextContent(fill(shared.forgetConfirmation, { release: olderRelease }));
    expect(confirmation).toHaveTextContent(olderRelease);
    if (several) expect(confirmation).toHaveTextContent(fill(shared.details.forget_release, { release: olderRelease }));
    else expect(confirmation).not.toHaveTextContent(fill(shared.details.forget_release, { release: olderRelease }));
    fireEvent.keyDown(confirmation, { key: "Escape" });
    expect(details()).toHaveTextContent(shared.details.passed);
    expect(app.requests).toEqual([]);
    click("Forget", details()); confirmation = screen.getByRole("alertdialog", { name: /forget/i });
    expect(within(confirmation).getByRole("button", { name: shared.confirmDismiss })).toHaveFocus();
    fireEvent.click(document.activeElement!);
    expect(screen.queryByRole("alertdialog", { name: /forget/i })).toBeNull();
    expect(app.requests).toEqual([]);
    click("Forget", details()); confirmation = screen.getByRole("alertdialog", { name: /forget/i });
    expect(app.requests).toEqual([]);
    click("Forget records", confirmation);
    await waitFor(() => expect(app.requests).toEqual([{ type: "release_check_forget", deckId: app.deckId, agent: "claude", release: olderRelease }]));
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckForget");
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckConfirmForget");
    for (const selected of several ? [secondIdentity, "aaaaaaaaaaaaaaaa"] : [secondIdentity]) {
      selectRecord(selected);
      expect(details()).toHaveTextContent(shared.details.forgotten);
      expect(within(details()).getByRole("button", { name: "Re-check" })).toBeEnabled();
      expect(within(details()).getByRole("button", { name: "Close" })).toBeEnabled();
      for (const label of ["Forget", "Export", "Cancel"]) expect(within(details()).queryByRole("button", { name: label })).toBeNull();
    }
    selectRecord(identity);
    expect(details()).toHaveTextContent(shared.details.passed);
    expect(details()).toHaveTextContent(release);
  });

  /// Scenario: A no-reference or off hold has no usable Re-check; Details explains how to proceed using the shared words.
  it.each(["no_reference", "off"] as const)("gives advice without Re-check for %s", (cause) => {
    mount({ rows: [], cause, mode: cause === "off" ? "off" : "auto" });
    const dialog = openDetails();
    expect(dialog).toHaveTextContent(fill(shared.details[cause], { agent: cause === "no_reference" ? "Codex" : "Claude Code" }));
    expect(within(dialog).queryByRole("button", { name: "Re-check" })).toBeNull();
    expect(card().queryByRole("button", { name: "Re-check" })).toBeNull();
  });

  /// Scenario: An older daemon supplies no release-check capability, an empty list or an unrelated one, even though records are present. Card controls are withheld and agent settings explains the upgrade without sending an operation.
  it.each([{ capabilities: null }, { capabilities: [] }, { capabilities: ["inbox_action"] }])
  ("withholds release-check controls without capability $capabilities", ({ capabilities }) => {
    const app = mount({ capabilities });
    expect(card().queryByRole("button", { name: "Details" })).toBeNull();
    expect(card().queryByRole("button", { name: "Re-check" })).toBeNull();
    fireEvent.keyDown(document.body, { key: "p" });
    expect(screen.getByText(shared.unsupported)).toBeVisible();
    expect(screen.queryByRole("combobox", { name: "Release checks" })).toBeNull();
    expect(app.requests).toEqual([]);
  });

  /// Scenario: Change the release-check setting through auto, ask and off. Each accepted reply is displayed in the shared words, and changing it while no check runs needs no cancellation confirmation.
  it("shows and applies auto, ask and off through the registry", async () => {
    const app = mount(); fireEvent.keyDown(document.body, { key: "p" });
    const setting = screen.getByRole("combobox", { name: "Release checks" });
    for (const mode of ["ask", "off", "auto"] as const) {
      fireEvent.change(setting, { target: { value: mode } });
      await waitFor(() => expect(app.requests.at(-1)).toEqual({ type: "release_check_mode", deckId: app.deckId, mode }));
      expect(setting).toHaveValue(mode);
      expect(screen.getByText(shared.modes[mode], { exact: true })).toBeVisible();
      expect(registryDispatch).toHaveBeenCalledWith(`releaseCheck${mode[0].toUpperCase()}${mode.slice(1)}`);
    }
  });

  /// Scenario: Choosing off while a check runs asks before cancelling it. Escape and default Back activation leave ask and the running check intact; explicit acceptance shows off and the runner-stopped reply.
  it("confirms off during a running check and applies the cancellation reply", async () => {
    const app = mount({ mode: "ask", rows: [row("passed", { state: null, progress: "running", queuePosition: 1 })] });
    fireEvent.keyDown(document.body, { key: "p" });
    const setting = screen.getByRole("combobox", { name: "Release checks" });
    fireEvent.change(setting, { target: { value: "off" } });
    const confirmation = screen.getByRole("alertdialog", { name: /release checks off/i });
    expect(confirmation).toHaveTextContent(shared.offConfirmation);
    expect(within(confirmation).getByRole("button", { name: shared.confirmDismiss })).toHaveFocus();
    expect(app.requests).toEqual([]);
    fireEvent.keyDown(confirmation, { key: "Escape" });
    expect(setting).toHaveValue("ask");
    expect(app.requests).toEqual([]);
    fireEvent.change(setting, { target: { value: "off" } });
    expect(within(screen.getByRole("alertdialog", { name: /release checks off/i })).getByRole("button", { name: shared.confirmDismiss })).toHaveFocus();
    fireEvent.click(document.activeElement!);
    expect(screen.queryByRole("alertdialog", { name: /release checks off/i })).toBeNull();
    expect(setting).toHaveValue("ask"); expect(app.requests).toEqual([]);
    fireEvent.change(setting, { target: { value: "off" } });
    click("Turn off", screen.getByRole("alertdialog", { name: /release checks off/i }));
    await waitFor(() => expect(app.requests).toEqual([{ type: "release_check_mode", deckId: app.deckId, mode: "off" }]));
    expect(setting).toHaveValue("off");
    click("Close", screen.getByRole("dialog", { name: /agent details/i }));
    expect(openDetails()).toHaveTextContent(shared.reasons.runner_stopped);
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckOff");
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckConfirmOff");
  });

  /// Scenario: Turn off while a check is queued but not running. As in the TUI, no confirmation is needed; the accepted reply shows off and the queued check stopped.
  it("turns off a queued check without a confirmation", async () => {
    const app = mount({ mode: "ask", rows: [row(null, { progress: "queued", queuePosition: 2 })] });
    fireEvent.keyDown(document.body, { key: "p" });
    const setting = screen.getByRole("combobox", { name: "Release checks" });
    fireEvent.change(setting, { target: { value: "off" } });
    expect(screen.queryByRole("alertdialog")).toBeNull();
    await waitFor(() => expect(app.requests).toEqual([{ type: "release_check_mode", deckId: app.deckId, mode: "off" }]));
    expect(setting).toHaveValue("off");
    click("Close", screen.getByRole("dialog", { name: /agent details/i }));
    expect(openDetails()).toHaveTextContent(shared.reasons.runner_stopped);
  });

  /// Scenario: Press p while a terminal wrapper, its input or a directory field has focus. It must preserve the active view; the same key on the dashboard opens agent settings as a positive control.
  it("keeps the plain p shortcut out of terminals and text fields", async () => {
    mount();
    for (const target of [screen.getByRole("group", { name: "Fixture terminal" }), screen.getByRole("textbox", { name: "Fixture terminal input" })]) {
      target.focus(); fireEvent.keyDown(target, { key: "p" });
      expect(screen.queryByRole("dialog", { name: /agent details/i })).toBeNull();
    }
    openDetails(); click("Continue", await preview());
    const input = screen.getByRole("textbox", { name: "Directory" });
    input.focus(); fireEvent.keyDown(input, { key: "p" });
    expect(screen.queryByRole("dialog", { name: /agent details/i })).toBeNull();
    expect(input).toBeVisible();
    click("Cancel export"); click("Close", details());
    input.blur(); fireEvent.keyDown(document.body, { key: "p" });
    expect(screen.getByRole("combobox", { name: "Release checks" })).toBeVisible();
  });
});

describe("inbox/release-check/004 — desktop export inspection", () => {
  /// Scenario: Select an older release with two identities and inspect every proposed export file before continuing to a directory. Both preview and write select that older release while the current release stays passed; nothing is written until manual confirmation.
  it("previews, opens every file, then writes only after continuing and confirming", async () => {
    const app = mount({ rows: [row(), row("passed", { release: olderRelease, identity: secondIdentity }), row("passed", { release: olderRelease, identity: "aaaaaaaaaaaaaaaa" })] });
    openDetails(); selectRecord(secondIdentity);
    let dialog = await preview();
    expect(dialog).toHaveTextContent(fill(shared.details.export_release, { release: olderRelease }));
    expect(app.requests).toEqual([{ type: "release_check_export_preview", deckId: app.deckId, agent: "claude", release: olderRelease }]);
    for (const file of files) {
      click(`Open ${file.name}`, dialog);
      expect(screen.getByRole("region", { name: "File contents" }).textContent).toBe(file.content);
      click("Back to files"); dialog = screen.getByRole("dialog", { name: "Export release check" });
    }
    click("Continue", dialog);
    fireEvent.change(screen.getByRole("textbox", { name: "Directory" }), { target: { value: "/chosen/export" } });
    expect(app.requests).toHaveLength(1);
    click("Export files");
    await waitFor(() => expect(app.requests).toEqual([
      { type: "release_check_export_preview", deckId: app.deckId, agent: "claude", release: olderRelease },
      { type: "release_check_export", deckId: app.deckId, agent: "claude", release: olderRelease, dir: "/chosen/export" },
    ]));
    expect(screen.getByText(/exported/i)).toBeVisible();
    selectRecord(identity);
    expect(details()).toHaveTextContent(shared.details.passed);
    expect(details()).toHaveTextContent(release);
    for (const action of ["releaseCheckExport", "releaseCheckInspect", "releaseCheckDirectory", "releaseCheckExportContinue", "releaseCheckExportWrite"]) expect(registryDispatch).toHaveBeenCalledWith(action);
  });

  /// Scenario: Cancel Export from the file list, an opened file or the directory prompt; none of these paths writes an export.
  it.each(["list", "inspection", "directory"])("cancels from %s without writing", async (stage) => {
    const app = mount(); openDetails(); const dialog = await preview();
    if (stage === "inspection") click("Open trace.txt", dialog);
    if (stage === "directory") { click("Continue", dialog); fireEvent.change(screen.getByRole("textbox", { name: "Directory" }), { target: { value: "/cancelled" } }); }
    click("Cancel export");
    expect(details()).toBeVisible();
    expect(app.requests).toEqual([{ type: "release_check_export_preview", deckId: app.deckId, agent: "claude", release }]);
    expect(registryDispatch).toHaveBeenCalledWith("releaseCheckExportCancel");
  });
});

// Expected strings are literal oracles; no production sanitiser generates them.
const attacks: [string, string, string][] = [
  ["CSI erase/cursor/colour", "before\x1b[2J\x1b[H\x1b[31mRED\x1b[0mafter", "before\\x1b[2J\\x1b[H\\x1b[31mRED\\x1b[0mafter"],
  ["OSC title", "before\x1b]0;forged title\x07after", "before\\x1b]0;forged title\\x07after"],
  ["OSC 52 clipboard", "before\x1b]52;c;YXR0YWNr\x1b\\after", "before\\x1b]52;c;YXR0YWNr\\x1b\\after"],
  ["C1 CSI", "before\x9b2Jafter", "before\\x9b2Jafter"],
  ["CR/backspace/DEL", "before\rreplace\b\x7fafter", "before\\x0dreplace\\x08\\x7fafter"],
  ["HTML", '<img src=x onerror="alert(1)"><script>alert(2)</script>&', '<img src=x onerror="alert(1)"><script>alert(2)</script>&'],
  ["bidi overrides and isolates", "a\u061c\u200e\u200f\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069z", "a\\u{61c}\\u{200e}\\u{200f}\\u{202a}\\u{202b}\\u{202c}\\u{202d}\\u{202e}\\u{2066}\\u{2067}\\u{2068}\\u{2069}z"],
  ["zero width", "a\u200b\u200c\u200d\u2060\ufeffz", "a\\u{200b}\\u{200c}\\u{200d}\\u{2060}\\u{feff}z"],
  ["Unicode separators", "a\u2028b\u2029z", "a\\u{2028}b\\u{2029}z"],
  ["Cf soft hyphen and tags", "a\u00ad\u{e0001}\u{e0061}\u{e007f}z", "a\\u{ad}\\u{e0001}\\u{e0061}\\u{e007f}z"],
  ["Cf invisible operators and deprecated formats", "a\u2061\u2062\u2063\u2064\u206a\u206f\ufff9\ufffa\ufffbz", "a\\u{2061}\\u{2062}\\u{2063}\\u{2064}\\u{206a}\\u{206f}\\u{fff9}\\u{fffa}\\u{fffb}z"],
  ["default-ignorable marks and variation selectors", "a\u034f\ufe0f\u{e0100}z", "a\\u{34f}\\u{fe0f}\\u{e0100}z"],
  ["default-ignorable Hangul fillers", "a\u115f\u1160\u3164\uffa0z", "a\\u{115f}\\u{1160}\\u{3164}\\u{ffa0}z"],
  ["Zs non-ASCII spaces", "a\u00a0\u1680\u2000\u2007\u202f\u205f\u3000z", "a\\u{a0}\\u{1680}\\u{2000}\\u{2007}\\u{202f}\\u{205f}\\u{3000}z"],
  ["lone UTF-16 surrogates", "a\ud800b\udfffz", "a\\u{d800}b\\u{dfff}z"],
  ["printable/LF/backslashes", "é界🙂\\literal\nsecond", "é界🙂\\literal\nsecond"],
];
// LF and ordinary space are the explicit Cc/Zs exceptions. Cf and default
// ignorables cover invisible text beyond the named regression examples.
const forbidden = /(?!(?:\n| ))[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\p{Zs}\p{Default_Ignorable_Code_Point}\p{Surrogate}]/u;
function inertDom(root: HTMLElement) {
  expect(root.textContent).not.toMatch(forbidden);
  for (const node of [root, ...root.querySelectorAll("*")]) for (const attr of node.attributes) {
    // A controlled input may mirror its editable raw value into this attribute.
    // It is input, unlike title/aria-label and the separate destination echo.
    if (node instanceof HTMLInputElement && attr.name === "value") continue;
    expect(attr.value).not.toMatch(forbidden);
  }
  expect(root.querySelector("script, img, iframe, object")).toBeNull();
}

describe("inbox/release-check/004 — recorded output is inert display text", () => {
  /// Scenario: Open a hostile trace containing terminal commands, HTML, bidi or invisible separators. The inspection region shows literal printable escapes, preserves ordinary text and line feeds, and creates no executable HTML or control-bearing DOM attributes.
  it.each(attacks)("escapes %s in file inspection", async (_name, content, expected) => {
    const app = mount({ files: [{ name: "trace.txt", content }] }); openDetails(); const dialog = await preview();
    const shellBefore = screen.getByRole("article", { name: "Release check worker" }).textContent;
    click("Open trace.txt", dialog);
    expect(screen.getByRole("region", { name: "File contents" }).textContent).toBe(expected);
    inertDom(document.body);
    expect(screen.getByRole("article", { name: "Release check worker" }).textContent).toBe(shellBefore);
    expect(app.requests.filter((request) => request.type === "release_check_export")).toEqual([]);
  });

  /// Scenario: Inspect a trace containing each C0/C1 control and DEL. Every byte except LF appears as its lowercase two-digit escape; LF remains a line boundary, and literal backslashes stay literal.
  it("escapes the complete C0/C1/DEL byte set", async () => {
    const codes = [...Array.from({ length: 32 }, (_, i) => i), ...Array.from({ length: 33 }, (_, i) => 127 + i)];
    const content = codes.map((code) => `[${code}:${String.fromCharCode(code)}]`).join("|");
    const expected = codes.map((code) => `[${code}:${code === 10 ? "\n" : `\\x${code.toString(16).padStart(2, "0")}`}]`).join("|");
    const app = mount({ files: [{ name: "trace.txt", content }] }); openDetails(); click("Open trace.txt", await preview());
    expect(screen.getByRole("region", { name: "File contents" }).textContent).toBe(expected);
    inertDom(document.body);
  });

  /// Scenario: An adapter supplies a million-character multibyte line with an invisible prefix and trailing sentinel. Inspection stays within 16,384 Unicode code points after escaping, including a marker giving the whole file's byte count and explaining that export retains it; the tail is absent and no write occurs.
  it("bounds very long inspection text after escaping", async () => {
    const content = "\u202e" + "界".repeat(1_000_000) + "END_OF_HOSTILE_TRACE";
    const app = mount({ files: [{ name: "trace.txt", content }] });
    openDetails(); click("Open trace.txt", await preview());
    const text = screen.getByRole("region", { name: "File contents" }).textContent!;
    expect(text.startsWith("\\u{202e}")).toBe(true);
    expect(Array.from(text).length).toBeLessThanOrEqual(inspectionCodePoints);
    expect(text).toContain(truncationMarker(new TextEncoder().encode(content).length));
    expect(text).not.toContain("END_OF_HOSTILE_TRACE"); inertDom(document.body);
    expect(app.requests).toHaveLength(1);
  });

  /// Scenario: Hostile adapter file names and error messages are shown with the same escapes as recorded output. HTML stays literal and no executable markup is created, including in portal dialogs.
  it.each(["name", "error"])("escapes adapter %s", async (source) => {
    const raw = '<img src=x>\x1b]52;c;xx\x07\x9b2J\r\b\x7f\u202e\u200b\u2028\u2029';
    const expected = '<img src=x>\\x1b]52;c;xx\\x07\\x9b2J\\x0d\\x08\\x7f\\u{202e}\\u{200b}\\u{2028}\\u{2029}';
    const app = mount(source === "name" ? { files: [{ name: raw, content: "ordinary content" }] } : { error: raw });
    openDetails(); click("Export", details());
    await waitFor(() => expect(document.body).toHaveTextContent(expected));
    inertDom(document.body);
    expect(app.requests).toHaveLength(1);
  });

  /// Scenario: Receive a million-character adapter filename or error. Its visible diagnostic has a truncation marker, omits the tail and stays within 16,384 code points (plus the filename button's Open prefix).
  it.each(["name", "error"])("bounds long adapter %s", async (source) => {
    const raw = "\u202e" + "界".repeat(1_000_000) + "END_OF_DIAGNOSTIC";
    const app = mount(source === "name" ? { files: [{ name: raw, content: "ordinary" }] } : { error: raw });
    openDetails(); click("Export", details());
    const diagnostic = source === "name" ? await screen.findByRole("button", { name: /^Open / }) : await screen.findByRole("alert");
    const text = diagnostic.textContent!;
    expect(text).toContain("\\u{202e}"); expect(text).toMatch(/truncated/i);
    expect(Array.from(text).length).toBeLessThanOrEqual(inspectionCodePoints + (source === "name" ? 5 : 0));
    expect(text).toContain(shared.displayTruncated);
    expect(text).not.toContain("END_OF_DIAGNOSTIC"); inertDom(document.body);
    expect(app.requests).toHaveLength(1);
  });

  /// Scenario: Type a hostile destination into the directory field. Its editable raw value is input, but the echoed directory and its root attributes use inert text; attempting to write is refused with an explanation before an adapter write is sent.
  it("escapes the echoed directory and refuses control-bearing input", async () => {
    const raw = '/export/<img src=x>\x1b]52;c;xx\x07\x9b2J\b\x7f\u202e\u200b\u2028\u2029';
    const expected = '/export/<img src=x>\\x1b]52;c;xx\\x07\\x9b2J\\x08\\x7f\\u{202e}\\u{200b}\\u{2028}\\u{2029}';
    const app = mount(); openDetails(); click("Continue", await preview());
    const input = screen.getByRole("textbox", { name: "Directory" });
    fireEvent.change(input, { target: { value: raw } });
    expect(input).toHaveValue(raw);
    expect(screen.getByRole("region", { name: "Export destination" }).textContent).toBe(expected);
    inertDom(screen.getByRole("region", { name: "Export destination" })); inertDom(document.body);
    click("Export files"); await act(async () => { await Promise.resolve(); });
    expect(app.requests).toHaveLength(1);
    expect(screen.getByRole("alert")).toHaveTextContent(/directory.*(?:control|invalid)|(?:control|invalid).*directory/i);
    expect(screen.getByRole("alert")).toHaveTextContent(shared.invalidDirectory);
    click("Cancel export"); expect(app.requests).toHaveLength(1);
  });

  /// Scenario: A pasted newline is refused before the native single-line input can silently strip it, and a typed C1 control is also refused. The form explains the invalid directory and sends only the read-only preview request.
  it.each(["/export/line\nbreak", "/export/\u0085control"])("rejects typed control directory %j", async (raw) => {
    const app = mount(); openDetails(); click("Continue", await preview());
    const input = screen.getByRole("textbox", { name: "Directory" });
    if (raw.includes("\n")) {
      fireEvent.change(input, { target: { value: "/valid/export" } });
      fireEvent.paste(input, { clipboardData: { getData: () => raw } });
      // With a valid value already present, a missing paste validator cannot
      // pass merely because the empty directory would have been rejected.
      expect(screen.getByRole("alert")).toHaveTextContent(/directory.*(?:control|invalid)|(?:control|invalid).*directory/i);
    } else {
      fireEvent.change(input, { target: { value: raw } });
    }
    click("Export files"); await act(async () => { await Promise.resolve(); });
    expect(app.requests).toHaveLength(1);
    expect(screen.getByRole("alert")).toHaveTextContent(/directory.*(?:control|invalid)|(?:control|invalid).*directory/i);
    expect(screen.getByRole("alert")).toHaveTextContent(shared.invalidDirectory);
    inertDom(document.body);
  });

  /// Scenario: Type a million-character destination. The raw editable value remains available, while its separate presentation copy stays within 16,384 code points, marks truncation and omits the tail; cancellation writes nothing.
  it("bounds the echoed directory", async () => {
    const app = mount(); openDetails(); click("Continue", await preview());
    const raw = "/export/" + "界".repeat(1_000_000) + "END_OF_DIRECTORY";
    const input = screen.getByRole("textbox", { name: "Directory" });
    fireEvent.change(input, { target: { value: raw } }); expect(input).toHaveValue(raw);
    const echo = screen.getByRole("region", { name: "Export destination" });
    expect(Array.from(echo.textContent!).length).toBeLessThanOrEqual(inspectionCodePoints);
    expect(echo).toHaveTextContent(/truncated/i); expect(echo).not.toHaveTextContent("END_OF_DIRECTORY");
    expect(echo).toHaveTextContent(shared.displayTruncated);
    inertDom(echo); inertDom(document.body); click("Cancel export"); expect(app.requests).toHaveLength(1);
  });

  /// Scenario: Choose a destination with bidi and zero-width characters but no control characters. Its echo and success message use printable escapes, while the confirmed external write receives the exact raw path.
  it("allows bidi and zero-width destinations with an escaped echo", async () => {
    const app = mount(); openDetails(); click("Continue", await preview());
    const raw = "/export/\u202e\u200b";
    const escaped = "/export/\\u{202e}\\u{200b}";
    fireEvent.change(screen.getByRole("textbox", { name: "Directory" }), { target: { value: raw } });
    expect(screen.getByRole("region", { name: "Export destination" }).textContent).toBe(escaped);
    inertDom(document.body);
    click("Export files");
    await waitFor(() => expect(app.requests).toEqual([
      { type: "release_check_export_preview", deckId: app.deckId, agent: "claude", release },
      { type: "release_check_export", deckId: app.deckId, agent: "claude", release, dir: raw },
    ]));
    expect(details()).toHaveTextContent(escaped);
    inertDom(document.body);
  });
});

const voiceCases = [
  ["Re-check", "releaseCheckRun", "release_check_run"], ["Details", "releaseCheckDetails", "release_check_details"],
  ["Cancel release check", "releaseCheckCancel", "release_check_cancel"], ["Forget release check", "releaseCheckForget", "release_check_forget"],
  ["Export release check", "releaseCheckExport", "release_check_export"],
  ["Release checks auto", "releaseCheckAuto", "release_check_auto"], ["Release checks ask", "releaseCheckAsk", "release_check_ask"],
  ["Release checks off", "releaseCheckOff", "release_check_off"],
] as const;
// Read the actual shipped table; do not create a second table that can pass while commands.toml is absent.
const commandBlocks = commandTable.split(/^\[\[commands\]\]\s*$/m).slice(1);
const manualAcceptances = ["releaseCheckConfirmCancel", "releaseCheckConfirmForget", "releaseCheckConfirmOff", "releaseCheckExportWrite"] as const;

describe("inbox/release-check/004 — desktop voice parity", () => {
  /// Scenario: Enumerate the proposed release-check actions. Each spoken action has a callable voice-enabled registry entry and a shipped command row; local inspection/form/confirmation controls have an explicit fitness reason instead.
  it.each(voiceCases)("classifies %s and links it to the shipped command table", (phrase, invoke, id) => {
    const actions = VOICE_ACTIONS as Record<string, { voice?: boolean; no_voice?: string; run: unknown }>;
    expect(actions[invoke], `${phrase}: missing registry entry ${invoke}`).toBeDefined();
    expect(actions[invoke].voice).toBe(true); expect(actions[invoke].run).toBeTypeOf("function");
    expect(commandBlocks.find((block) => new RegExp(`^id\\s*=\\s*"${id}"`, "m").test(block)), `${phrase}: missing commands.toml row`).toMatch(new RegExp(`^invoke\\s*=\\s*"${invoke}"`, "m"));
  });

  /// Scenario: The local inspection and destination controls are registry actions too. A control kept off voice carries a written fitness reason rather than silently creating a second dispatch path.
  it("classifies local inspection and destination controls", () => {
    const actions = VOICE_ACTIONS as Record<string, { voice?: boolean; no_voice?: string; run: unknown }>;
    for (const invoke of ["releaseCheckSelect", "releaseCheckInspect", "releaseCheckDirectory", "releaseCheckExportContinue", "releaseCheckExportCancel"]) {
      expect(actions[invoke], `missing classification for ${invoke}`).toBeDefined();
      expect(actions[invoke].voice === true || (actions[invoke].no_voice?.trim().length ?? 0) >= 25).toBe(true);
    }
  });

  /// Scenario: Inspect each consequential acceptance separately from its voice-enabled opener. Acceptance must carry a substantial no_voice reason and the shipped command table must contain no row invoking it.
  it.each(manualAcceptances)("keeps %s manual with no spoken command", (invoke) => {
    const actions = VOICE_ACTIONS as Record<string, { voice?: boolean; no_voice?: string; run: unknown }>;
    expect(actions[invoke], `missing classification for ${invoke}`).toBeDefined();
    expect(actions[invoke].voice).not.toBe(true);
    expect(actions[invoke].no_voice?.trim().length ?? 0).toBeGreaterThanOrEqual(25);
    expect(actions[invoke].run).toBeTypeOf("function");
    expect(commandBlocks.filter((block) => new RegExp(`^[\\t ]*invoke\\s*=\\s*["']${invoke}["']`, "m").test(block))).toEqual([]);
  });

  /// Scenario: Deliver a synthetic resolved utterance through the real voice panel for each release-check command. It crosses the same registry entry as the control and opens the same visible state; destructive actions wait for a manual confirmation before any runtime operation.
  it.each(voiceCases)("dispatches the spoken phrase %s", async (phrase, invoke, id) => {
    vi.useFakeTimers();
    const app = mount({ mode: "ask", rows: [row("passed", { state: null, progress: "running", queuePosition: 1 }), row("passed", { identity: secondIdentity })] });
    if (!["releaseCheckDetails", "releaseCheckRun"].includes(invoke)) openDetails();
    registryDispatch.mockClear();
    await app.speak(phrase, invoke, id);
    expect(registryDispatch).toHaveBeenCalledWith(invoke);
    if (invoke === "releaseCheckDetails") expect(details()).toBeVisible();
    if (invoke === "releaseCheckRun") { expect(app.requests).toEqual([{ type: "release_check_run", deckId: app.deckId, agent: "claude" }]); expect(openDetails()).toHaveTextContent(shared.details.running); }
    if (invoke === "releaseCheckExport") {
      expect(screen.getByRole("dialog", { name: "Export release check" })).toHaveTextContent(shared.exportInspection);
      expect(app.requests).toEqual([{ type: "release_check_export_preview", deckId: app.deckId, agent: "claude", release }]);
    }
    if (["releaseCheckCancel", "releaseCheckForget", "releaseCheckOff"].includes(invoke)) {
      expect(app.requests).toEqual([]);
      const confirmation = screen.getByRole("alertdialog", { name: invoke === "releaseCheckCancel" ? /cancel.*check/i : invoke === "releaseCheckForget" ? /forget/i : /release checks off/i });
      expect(confirmation).toBeVisible();
      if (invoke === "releaseCheckForget") expect(confirmation).toHaveTextContent(fill(shared.details.forget_release));
      if (invoke === "releaseCheckOff") expect(confirmation).toHaveTextContent(shared.offConfirmation);
      click(invoke === "releaseCheckCancel" ? "Cancel check" : invoke === "releaseCheckForget" ? "Forget records" : "Turn off", confirmation);
      await act(async () => { await Promise.resolve(); });
      expect(app.requests).toEqual([invoke === "releaseCheckCancel" ? { type: "release_check_cancel", deckId: app.deckId }
        : invoke === "releaseCheckForget" ? { type: "release_check_forget", deckId: app.deckId, agent: "claude", release }
          : { type: "release_check_mode", deckId: app.deckId, mode: "off" }]);
      expect(details()).toHaveTextContent(invoke === "releaseCheckForget" ? shared.details.forgotten : shared.reasons.runner_stopped);
    }
    if (["releaseCheckAuto", "releaseCheckAsk"].includes(invoke)) {
      const mode = invoke === "releaseCheckAuto" ? "auto" : "ask";
      expect(app.requests).toEqual([{ type: "release_check_mode", deckId: app.deckId, mode }]);
      expect(app.container).toHaveTextContent(shared.modes[mode]);
    }
  });

  /// Scenario: With Details closed, spoken Forget opens Details and asks about the selected pane's current release, even when an older release is first in the journal. It sends nothing before manual acceptance and preserves the older release.
  it("targets the current release for spoken Forget with Details closed", async () => {
    vi.useFakeTimers();
    const app = mount({ rows: [row("passed", { release: olderRelease, identity: secondIdentity }), row()] });
    await app.speak("Forget release check", "releaseCheckForget", "release_check_forget");
    const confirmation = screen.getByRole("alertdialog", { name: /forget/i });
    expect(confirmation).toHaveTextContent(release); expect(app.requests).toEqual([]);
    click("Forget records", confirmation); await act(async () => { await Promise.resolve(); });
    expect(app.requests).toEqual([{ type: "release_check_forget", deckId: app.deckId, agent: "claude", release }]);
    expect(details()).toHaveTextContent(shared.details.forgotten);
    selectRecord(secondIdentity); expect(details()).toHaveTextContent(shared.details.passed);
    expect(details()).toHaveTextContent(olderRelease);
  });

  /// Scenario: Open each confirmation or final export form, then deliver yes and confirm through the real voice dispatcher, even with a synthetic resolver attempting the manual acceptance. The prompt and records remain unchanged and the operation occurs only after a manual click.
  it.each(manualAcceptances)("refuses spoken acceptance of %s", async (acceptance) => {
    vi.useFakeTimers();
    const app = mount({ mode: "ask", rows: [row(null, { progress: "running", queuePosition: 1 })] });
    openDetails();
    let prompt: HTMLElement;
    let label: string;
    if (acceptance === "releaseCheckExportWrite") {
      click("Continue", await preview());
      fireEvent.change(screen.getByRole("textbox", { name: "Directory" }), { target: { value: "/manual/export" } });
      prompt = screen.getByRole("dialog", { name: "Export release check" }); label = "Export files";
    } else {
      if (acceptance === "releaseCheckConfirmOff") await app.speak("Release checks off", "releaseCheckOff", "release_check_off");
      else click(acceptance === "releaseCheckConfirmCancel" ? "Cancel" : "Forget", details());
      prompt = screen.getByRole("alertdialog", { name: acceptance === "releaseCheckConfirmCancel" ? /cancel.*check/i : acceptance === "releaseCheckConfirmForget" ? /forget/i : /release checks off/i });
      label = acceptance === "releaseCheckConfirmCancel" ? "Cancel check" : acceptance === "releaseCheckConfirmForget" ? "Forget records" : "Turn off";
    }
    const before = structuredClone(app.requests);
    const text = prompt.textContent;
    for (const phrase of ["yes", "confirm"]) {
      await app.speak(phrase, acceptance, `unshipped_${acceptance}`);
      expect(app.requests).toEqual(before);
      expect(prompt).toBeVisible(); expect(prompt.textContent).toBe(text);
      expect(within(prompt).getByRole("button", { name: label })).toBeEnabled();
    }
    click(label, prompt); await act(async () => { await Promise.resolve(); });
    expect(app.requests).toHaveLength(before.length + 1);
    expect(app.requests.at(-1)?.type).toBe(acceptance === "releaseCheckConfirmCancel" ? "release_check_cancel" : acceptance === "releaseCheckConfirmForget" ? "release_check_forget" : acceptance === "releaseCheckConfirmOff" ? "release_check_mode" : "release_check_export");
    if (acceptance === "releaseCheckExportWrite") expect(screen.getByText(/exported/i)).toBeVisible();
    else expect(details()).toHaveTextContent(acceptance === "releaseCheckConfirmForget" ? shared.details.forgotten : shared.reasons.runner_stopped);
  });
});
