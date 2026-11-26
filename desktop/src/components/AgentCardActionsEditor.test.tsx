import "../styles.css";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import { deckName, domIdentity } from "../lib/displayText";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type DeckRuntimeState } from "../types";
import { AgentOverview } from "./AgentOverview";

/*
 * PRD #2374 decision 2026-11-24 (1), issue #2399 on PR #2491 — the desktop
 * app's editor for card actions (`desktop/agent-cards/053`–`059`). TDD (RED):
 * written before the editor exists.
 *
 * The contract these hold the implementation to:
 * - each daemon's header has a ⋯ menu (`daemon-more`, named "More for <daemon>")
 *   whose Card actions item opens a dialog named "Card actions: <daemon>"
 *   (`agent-card-actions-editor`), bound to that daemon, with no Daemon picker
 *   (PR #2491 decision 2026-11-25; the Cards toolbar's Card actions button
 *   and the editor's picker are gone);
 * - the runtime reads a daemon's editable catalog with
 *   `readCardCatalog(deckId)` and changes it with `editCardCatalog(deckId, edit)`,
 *   both answering the daemon's `EditableCardCatalog` (`actions`, `errors`,
 *   `user_config`); a refused edit rejects with the daemon's reason;
 * - a deck's cards snapshot says whether its daemon advertised the editor
 *   (`catalogEdit`); without it the editor is read-only for that daemon and
 *   sends it nothing;
 * - each listed action is a row (`agent-card-actions-editor-row`, `data-action`
 *   its name, `data-source` `deck` or `user`) with buttons named "Edit <label>",
 *   "Remove <label>" (the user's own), "Hide <label>" (a deck default in use)
 *   and "Restore <label>" (a deck default hidden or overridden);
 * - the form's fields are labelled Name (fixed once created), Label, "Text it
 *   sends", "Tell agents when to offer it", "Ask before sending" and "Offer on
 *   every card"; its icon is chosen with "Choose icon", which opens a
 *   searchable grid (listbox "Icons", search box "Search icons") of the icons
 *   the app bundles; "Save" sends the edit;
 * - removing asks first, in the editor's own confirmation, named "Remove <label>?".
 *
 * Issue #2399 review (findings 4, 5, 6 and 10) added `065`–`067` and tightened
 * `053`, `055` and `058`: each row shows its source in words
 * (`agent-card-actions-editor-source`); a failed read or a read with problems
 * says so; the form has a Placement choice; the editor opens on the local
 * daemon wherever it is listed; and answers that arrive after the daemon
 * changed are dropped.
 */

const INSTANCE = "daemon-instance-editor";
const REMOTE_INSTANCE = "daemon-instance-editor-remote";
const USER_CONFIG = "/home/user/.config/dot-agent-deck/config.toml";

type EditableAction = {
  name: string; label: string; description: string; prompt: string; icon: string | null; placement: string;
  confirm: boolean; always: boolean; source: "default" | "user"; default: boolean; overridden: boolean; hidden: boolean;
};
type EditableCatalog = { actions: EditableAction[]; errors: string[]; user_config: string | null };
type Edit = { kind: "add" | "update"; action: Record<string, unknown>; base?: EditableAction | null } | { kind: "remove" | "hide" | "restore"; name: string; base?: EditableAction | null };

const editable = (name: string, overrides: Partial<EditableAction> = {}): EditableAction => ({
  name, label: name, description: `Offer when ${name} applies.`, prompt: `Do ${name}.`, icon: null, placement: "card",
  confirm: false, always: false, source: "user", default: false, overridden: false, hidden: false, ...overrides,
});

const GO_AHEAD = editable("go-ahead", { label: "Go ahead", description: "Offer when you are waiting for the user's approval to continue.", prompt: "Go ahead.", icon: "play", source: "default", default: true });
const MERGE = editable("merge", { label: "Merge it", description: "Offer when your work is ready to merge.", prompt: "Merge it, following this project's own merge procedure, then update your card.", icon: "git-merge", source: "default", default: true, hidden: true });
const SHIP = editable("ship", { label: "Ship it", description: "Offer when it is ready.", prompt: "Ship it.\nThen update your card.", icon: "rocket", confirm: true });
const CATALOG: EditableCatalog = { actions: [GO_AHEAD, MERGE, SHIP], errors: [], user_config: USER_CONFIG };

const goAhead: AgentCardAction = {
  id: "go-ahead", label: "Go ahead", kind: "prompt", prompt: "Go ahead.", prompt_source: "catalog", text_input: false, confirm: false,
  confirm_detail: null, icon: "play", placement: "card", custom: false, source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: "tok-go",
};

function cardFor(id: string, name: string, instance: string): AgentCardEntry {
  return {
    key: `pane-${id}/${id}`, instance_id: instance, pane_id: `pane-${id}`, agent_id: id, agent_type: "claude_code", name,
    status: "idle", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: null, fields: [], questions: [], actions: [goAhead], primary_action_id: "go-ahead",
    facts: { source: "deck", pull_request: null }, revision: 1, updated_at: "2026-11-24T00:00:00Z",
  };
}

/**
 * The real dashboard on Cards, with one daemon (or two, `remote`), each
 * advertising the editor unless `catalogEdit` says otherwise, and a stubbed
 * catalog per daemon that `editCardCatalog` changes as a daemon would answer.
 */
type Pending = { deckId: string; kind: "read" | "edit"; resolve: () => void; reject: (why: string) => void; fail: (error: unknown) => void };

function setup({ catalogEdit = true, remote, refuse, remoteFirst = false, deferred = false, snapshotReads = false, readFails, catalogErrors = [] }: {
  catalogEdit?: boolean; remote?: { catalogEdit: boolean; catalog: EditableCatalog }; refuse?: string;
  /** The remote daemon is first in the fleet, so "the first" and "the local one" differ. */
  remoteFirst?: boolean;
  /** Reads and edits wait in `pending` until the test settles them, in any order. */
  deferred?: boolean;
  /** A read answers the catalog as it was when the read was asked, however late it is settled (issue #2399 re-review N1). */
  snapshotReads?: boolean;
  /** The local daemon's read is refused with this reason. */
  readFails?: string;
  /** The problems the local daemon's catalog read reports. */
  catalogErrors?: string[];
} = {}) {
  const [local, far] = createFixtureFleet("fleet");
  const fleet = [{ ...local, connection: { ...local.connection, incarnation: INSTANCE } }];
  if (remote) fleet.push({ ...far, connection: { ...far.connection, incarnation: REMOTE_INSTANCE } });
  const localCard = cardFor("1", "coder", INSTANCE);
  fleet[0] = { ...fleet[0], agents: [createFixtureStartedAgent({ id: "1", daemonId: FIXTURE_DAEMON_ID, displayName: "coder", command: "claude", cwd: "/w" })] };
  const catalogs: Record<string, EditableCatalog> = { [FIXTURE_DAEMON_ID]: { ...structuredClone(CATALOG), errors: [...catalogErrors] } };
  if (remote) catalogs[FIXTURE_REMOTE_DAEMON_ID] = structuredClone(remote.catalog);
  const pending: Pending[] = [];
  const settle = <T,>(deckId: string, kind: Pending["kind"], answer: () => T): Promise<T> => (deferred
    ? new Promise<T>((resolve, reject) => { pending.push({ deckId, kind, resolve: () => resolve(answer()), reject: (why) => reject(new Error(why)), fail: reject }); })
    : Promise.resolve().then(answer));
  const readCardCatalog = vi.fn((deckId: string) => {
    const asked = structuredClone(catalogs[deckId]);
    return settle(deckId, "read", () => {
      if (readFails && deckId === FIXTURE_DAEMON_ID) throw new Error(readFails);
      return snapshotReads ? asked : structuredClone(catalogs[deckId]);
    });
  });
  const editCardCatalog = vi.fn((deckId: string, edit: Edit) => settle(deckId, "edit", () => applyEdit(deckId, edit)));
  const applyEdit = (deckId: string, edit: Edit) => {
    if (refuse) throw new Error(refuse);
    const catalog = catalogs[deckId];
    if (edit.kind === "add") catalog.actions.push(editable(String(edit.action.name), edit.action as Partial<EditableAction>));
    if (edit.kind === "update") catalog.actions = catalog.actions.map((entry) => (entry.name === edit.action.name ? { ...entry, ...(edit.action as Partial<EditableAction>), source: "user", overridden: entry.default } : entry));
    if (edit.kind === "remove") catalog.actions = catalog.actions.filter((entry) => entry.name !== edit.name);
    if (edit.kind === "hide") catalog.actions = catalog.actions.map((entry) => (entry.name === edit.name ? { ...entry, hidden: true } : entry));
    if (edit.kind === "restore") catalog.actions = catalog.actions.map((entry) => (entry.name === edit.name ? { ...entry, hidden: false, overridden: false, source: "default" } : entry));
    return structuredClone(catalog);
  };
  const agentCards: Record<string, unknown> = {
    [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 1, entries: [localCard], catalogErrors: [], tree: true, batch: true, catalogEdit },
  };
  if (remote) {
    const remoteCard = cardFor("1", "remote-coder", REMOTE_INSTANCE);
    fleet[1] = { ...fleet[1], agents: [createFixtureStartedAgent({ id: "1", daemonId: FIXTURE_REMOTE_DAEMON_ID, displayName: "remote-coder", command: "claude", cwd: "/w" })] };
    agentCards[FIXTURE_REMOTE_DAEMON_ID] = { deckId: FIXTURE_REMOTE_DAEMON_ID, instanceId: REMOTE_INSTANCE, revision: 1, entries: [remoteCard], catalogErrors: [], tree: true, batch: true, catalogEdit: remote.catalogEdit };
  }
  const live = {
    mode: "fixture", snapshot: fleet[0], fleet, terminalData: {}, clearError: vi.fn(), runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined), setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined), listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(), saveSettings: vi.fn(), testEndpoint: vi.fn(), secretStatus: vi.fn(), storeSecret: vi.fn(), forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level), desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards, invokeCardAction: vi.fn(), invokeCardActionBatch: vi.fn(), readCardCatalog, editCardCatalog,
  } as unknown as DeckRuntimeState;
  const localName = deckName(fleet[0].connection);
  const remoteName = remote ? deckName(fleet[1].connection) : "";
  if (remoteFirst) fleet.reverse();
  const view = render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
  /* Issue #2399 review 5: the dashboard redrawn with no daemon's cards, then with them again. */
  const daemonsGone = () => view.rerender(<AgentOverview runtime={{ ...live, agentCards: {} } as DeckRuntimeState} onNavigate={vi.fn()} />);
  const daemonsBack = () => view.rerender(<AgentOverview runtime={{ ...live } as DeckRuntimeState} onNavigate={vi.fn()} />);
  return { readCardCatalog, editCardCatalog, localName, remoteName, pending, catalogs, daemonsGone, daemonsBack };
}

/** The editor's accessible name: "Card actions: " and the name of the daemon it is bound to (PR #2491 decision 2026-11-25). */
const EDITOR_NAME = /^Card actions: /;
const editorDialog = () => screen.getByRole("dialog", { name: EDITOR_NAME });
/** A daemon's section on the dashboard, by its deck id. */
const daemonSection = (deckId: string) => {
  const found = screen.getAllByTestId("daemon-group").find((node) => node.getAttribute("data-daemon-id") === domIdentity(deckId));
  if (!found) throw new Error(`no daemon section for ${deckId}`);
  return found;
};
/** Open the editor from `deckId`'s ⋯ menu, as a user does: ⋯, then Card actions. */
async function openEditor(deckId: string = FIXTURE_DAEMON_ID) {
  const section = daemonSection(deckId);
  fireEvent.click(within(section).getByTestId("daemon-more"));
  await act(async () => { fireEvent.click(within(section).getByRole("menuitem", { name: "Card actions" })); });
  return editorDialog();
}

const rows = (editor: HTMLElement) => within(editor).queryAllByTestId("agent-card-actions-editor-row");
const row = (editor: HTMLElement, name: string) => {
  const found = rows(editor).find((node) => node.getAttribute("data-action") === name);
  if (!found) throw new Error(`no editor row for ${name}`);
  return found;
};
const field = (scope: HTMLElement, label: string | RegExp) => within(scope).getByLabelText(label) as HTMLInputElement;
/** The source a row SHOWS (issue #2399 review, finding 6): its visible label's text, not an attribute. */
const shownSource = (listed: HTMLElement) => {
  const label = within(listed).getByTestId("agent-card-actions-editor-source");
  expect(label).toBeVisible();
  return label.textContent ?? "";
};

beforeEach(() => {
  window.localStorage.clear();
  window.localStorage.setItem("dot-agent-deck.overview.mode", "cards");
  act(() => setDashboardFilter(clearDashboardFilter()));
});
afterEach(() => { cleanup(); window.localStorage.clear(); });

describe("the card actions editor (PRD #2374, 2026-11-24)", () => {
  /// Scenario: On the Cards view, press Card actions. The editor reads the local daemon's catalog and lists Go ahead and a hidden Merge it as deck defaults and Ship it as the user's, each with its label, a visible source label and the exact text it sends, names the user config it edits, offers only the buttons each source allows, and lists none of the deck's own Reply, Open or Close.
  it("desktop/agent-cards/053: the editor lists each card action with where it came from", async () => {
    const calls = setup();
    expect(screen.queryByTestId("agent-cards-actions-editor-open"), "the Cards toolbar's button is gone").toBeNull();
    expect(within(daemonSection(FIXTURE_DAEMON_ID)).getByTestId("daemon-more")).toHaveAccessibleName(`More for ${calls.localName}`);
    const editor = await openEditor();
    expect(editor).toHaveAttribute("data-testid", "agent-card-actions-editor");
    expect(editor).toHaveAccessibleName(`Card actions: ${calls.localName}`);
    expect(within(editor).queryByRole("combobox", { name: "Daemon" })).toBeNull();
    expect(calls.readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, INSTANCE);
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "ship"]);
    expect(rows(editor).map((node) => node.getAttribute("data-source"))).toEqual(["deck", "deck", "user"]);
    for (const name of ["go-ahead", "merge"]) {
      expect(shownSource(row(editor, name)), name).toMatch(/\bdeck\b/i);
      expect(shownSource(row(editor, name)), name).not.toMatch(/\b(your|yours|user)\b/i);
    }
    expect(shownSource(row(editor, "ship"))).toMatch(/\b(yours|user)\b/i);
    expect(shownSource(row(editor, "ship"))).not.toMatch(/\bdeck\b/i);
    expect(within(editor).getByText(USER_CONFIG, { exact: false })).toBeInTheDocument();
    const ship = within(row(editor, "ship"));
    expect(ship.getByText("Ship it", { exact: true })).toBeInTheDocument();
    expect(ship.getByText(/Ship it\.\s*Then update your card\./)).toBeInTheDocument();
    expect(ship.getByRole("button", { name: "Edit Ship it" })).toBeInTheDocument();
    expect(ship.getByRole("button", { name: "Remove Ship it" })).toBeInTheDocument();
    expect(ship.queryByRole("button", { name: /^(Hide|Restore) / })).toBeNull();
    const go = within(row(editor, "go-ahead"));
    expect(go.getByRole("button", { name: "Edit Go ahead" })).toBeInTheDocument();
    expect(go.getByRole("button", { name: "Hide Go ahead" })).toBeInTheDocument();
    expect(go.queryByRole("button", { name: "Remove Go ahead" })).toBeNull();
    expect(go.queryByRole("button", { name: "Restore Go ahead" })).toBeNull();
    const merge = within(row(editor, "merge"));
    expect(merge.getByText(/hidden/i)).toBeInTheDocument();
    expect(merge.getByRole("button", { name: "Restore Merge it" })).toBeInTheDocument();
    expect(merge.queryByRole("button", { name: "Hide Merge it" })).toBeNull();
    expect(merge.queryByRole("button", { name: "Remove Merge it" })).toBeNull();
    for (const own of ["reply", "open", "close", "close-keep-worktree"]) expect(rows(editor).some((node) => node.getAttribute("data-action") === own), own).toBe(false);
    expect(calls.editCardCatalog).not.toHaveBeenCalled();
  });

  /// Scenario: In the editor, press Add action, fill in the name, label, the text it sends over two lines and Tell agents when to offer it, tick Ask before sending, then press Choose icon, search the icon grid for "rock" and pick the rocket. Save sends one add with exactly those values to the local daemon, and the list then shows the new action as the user's.
  it("desktop/agent-cards/054: adding an action with an icon from the picker sends one add and lists it", async () => {
    const calls = setup();
    const editor = await openEditor();
    fireEvent.click(within(editor).getByRole("button", { name: "Add action" }));
    fireEvent.change(field(editor, "Name"), { target: { value: "deploy" } });
    fireEvent.change(field(editor, "Label"), { target: { value: "Deploy it" } });
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "Deploy it.\nThen update your card." } });
    fireEvent.change(field(editor, "Tell agents when to offer it"), { target: { value: "Offer when the change can be deployed." } });
    expect(field(editor, "Tell agents when to offer it")).toHaveAccessibleDescription("Agents read this to decide when to put the action on their card; it is not shown on the card.");
    fireEvent.click(field(editor, "Ask before sending"));
    expect(field(editor, "Offer on every card")).not.toBeChecked();

    fireEvent.click(within(editor).getByRole("button", { name: "Choose icon" }));
    const grid = within(editor).getByRole("listbox", { name: "Icons" });
    const all = within(grid).getAllByRole("option").map((option) => option.getAttribute("data-icon"));
    for (const bundled of ["play", "git-merge", "rocket"]) expect(all).toContain(bundled);
    fireEvent.change(within(editor).getByRole("searchbox", { name: "Search icons" }), { target: { value: "rock" } });
    const found = within(grid).getAllByRole("option").map((option) => option.getAttribute("data-icon"));
    expect(found).toContain("rocket");
    expect(found).not.toContain("git-merge");
    fireEvent.click(within(grid).getByRole("option", { name: "rocket" }));
    expect(within(editor).queryByRole("listbox", { name: "Icons" })).toBeNull();
    expect(within(editor).getByTestId("agent-card-actions-editor-icon")).toHaveAttribute("data-icon", "rocket");

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, {
      kind: "add",
      action: expect.objectContaining({
        name: "deploy", label: "Deploy it", prompt: "Deploy it.\nThen update your card.",
        description: "Offer when the change can be deployed.", icon: "rocket", confirm: true, always: false,
      }),
    }, INSTANCE);
    const listed = row(editorDialog(), "deploy");
    expect(listed).toHaveAttribute("data-source", "user");
    expect(within(listed).getByText("Deploy it", { exact: true })).toBeInTheDocument();
  });

  /// Scenario: Edit the user's Ship it: its name is shown but cannot be changed, and the form holds its label, multi-line text, description, icon and confirmation; changing the label and saving sends one update carrying the rest unchanged. Then edit the deck's Go ahead and change its text: saving sends an update of the same name, which the list shows, in words, as the user's version of the deck default with Restore offered.
  it("desktop/agent-cards/055: editing keeps the name fixed and an edited default becomes the user's version", async () => {
    const calls = setup();
    let editor = await openEditor();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    const name = field(editor, "Name");
    expect(name.value).toBe("ship");
    expect(name.readOnly || name.disabled).toBe(true);
    expect(field(editor, "Label").value).toBe("Ship it");
    expect(field(editor, "Text it sends").value).toBe("Ship it.\nThen update your card.");
    expect(field(editor, "Tell agents when to offer it").value).toBe("Offer when it is ready.");
    expect(field(editor, "Ask before sending")).toBeChecked();
    expect(within(editor).getByTestId("agent-card-actions-editor-icon")).toHaveAttribute("data-icon", "rocket");
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, {
      kind: "update",
      action: expect.objectContaining({ name: "ship", label: "Ship it now", prompt: "Ship it.\nThen update your card.", description: "Offer when it is ready.", icon: "rocket", confirm: true }),
      base: SHIP,
    }, INSTANCE);

    editor = editorDialog();
    fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Edit Go ahead" }));
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "Carry on, then update your card." } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, {
      kind: "update",
      action: expect.objectContaining({ name: "go-ahead", label: "Go ahead", prompt: "Carry on, then update your card.", icon: "play" }),
      base: GO_AHEAD,
    }, INSTANCE);
    const go = row(editorDialog(), "go-ahead");
    expect(go).toHaveAttribute("data-source", "user");
    // An overridden default says both: it is the deck's, in the user's version.
    expect(shownSource(go)).toMatch(/\bdeck\b/i);
    expect(shownSource(go)).toMatch(/\b(your|yours|user)\b/i);
    expect(within(go).getByRole("button", { name: "Restore Go ahead" })).toBeInTheDocument();
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(2);
  });

  /// Scenario: Press Remove on the user's Ship it: the editor asks first, naming the action, and Cancel sends nothing; confirming sends one remove and the row goes. Hide on the deck's Go ahead sends one hide, and Restore on the hidden Merge it sends one restore.
  it("desktop/agent-cards/056: removing asks first; hiding and restoring a default each send their edit", async () => {
    const calls = setup();
    let editor = await openEditor();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Remove Ship it" }));
    let confirm = screen.getByRole("alertdialog", { name: "Remove Ship it?" });
    fireEvent.click(within(confirm).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("alertdialog", { name: "Remove Ship it?" })).toBeNull();
    expect(calls.editCardCatalog).not.toHaveBeenCalled();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Remove Ship it" }));
    confirm = screen.getByRole("alertdialog", { name: "Remove Ship it?" });
    await act(async () => { fireEvent.click(within(confirm).getByRole("button", { name: "Remove" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, { kind: "remove", name: "ship", base: SHIP }, INSTANCE);
    editor = editorDialog();
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge"]);

    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "hide", name: "go-ahead", base: GO_AHEAD }, INSTANCE);
    editor = editorDialog();
    await act(async () => { fireEvent.click(within(row(editor, "merge")).getByRole("button", { name: "Restore Merge it" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "restore", name: "merge", base: MERGE }, INSTANCE);
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(3);
  });

  /// Scenario: Add an action the daemon refuses, as it refuses a name with capitals. The editor shows the daemon's reason, keeps the form open with what was typed, and the list is unchanged.
  it("desktop/agent-cards/057: a refused edit shows the daemon's reason and keeps the form", async () => {
    const reason = "card-catalog-edit: catalog.actions \"Bad Name\": bad name; use 1 to 40 lowercase letters, digits and '-', starting with a letter or digit";
    const calls = setup({ refuse: reason });
    const editor = await openEditor();
    fireEvent.click(within(editor).getByRole("button", { name: "Add action" }));
    fireEvent.change(field(editor, "Name"), { target: { value: "Bad Name" } });
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "Do it." } });
    fireEvent.change(field(editor, "Tell agents when to offer it"), { target: { value: "Offer always." } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(1);
    expect(within(editor).getByRole("alert")).toHaveTextContent("bad name; use 1 to 40 lowercase letters");
    expect(field(editor, "Name").value).toBe("Bad Name");
    expect(field(editor, "Text it sends").value).toBe("Do it.");
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "ship"]);
  });

  /// Scenario: With a remote and then a local daemon on the dashboard, in that order, open Card actions from the local daemon's ⋯ menu: the editor is titled with the local daemon's name, has no Daemon picker and reads the local catalog. Close it and open Card actions from the remote daemon's ⋯ menu: it is titled with the remote daemon's name, lists that daemon's catalog, and an edit made then goes to the remote daemon only.
  it("desktop/agent-cards/058: each daemon's ⋯ menu opens the editor bound to that daemon, with no Daemon picker", async () => {
    const remoteCatalog: EditableCatalog = { actions: [GO_AHEAD, { ...MERGE, hidden: false }, editable("deploy", { label: "Deploy it" })], errors: [], user_config: "/srv/deck/config.toml" };
    const calls = setup({ remote: { catalogEdit: true, catalog: remoteCatalog }, remoteFirst: true });
    let editor = await openEditor(FIXTURE_DAEMON_ID);
    expect(editor).toHaveAccessibleName(`Card actions: ${calls.localName}`);
    expect(within(editor).queryByRole("combobox", { name: "Daemon" })).toBeNull();
    expect(calls.readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, INSTANCE);
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "ship"]);
    fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close"));
    expect(screen.queryByRole("dialog", { name: EDITOR_NAME })).toBeNull();

    editor = await openEditor(FIXTURE_REMOTE_DAEMON_ID);
    expect(editor).toHaveAccessibleName(`Card actions: ${calls.remoteName}`);
    expect(within(editor).queryByRole("combobox", { name: "Daemon" })).toBeNull();
    expect(calls.readCardCatalog).toHaveBeenLastCalledWith(FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE);
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "deploy"]);
    expect(within(editor).getByText("/srv/deck/config.toml", { exact: false })).toBeInTheDocument();
    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, { kind: "hide", name: "go-ahead", base: GO_AHEAD }, REMOTE_INSTANCE);
  });

  /// Scenario: Open the editor for a daemon from an older release that does not offer it, alone and then beside one that does. The older daemon's ⋯ menu still offers Card actions; its editor says its release cannot be edited from the app, offers no Add, Edit, Remove, Hide or Restore, and sends that daemon nothing; the newer daemon's ⋯ menu opens an editor that edits it as usual.
  it("desktop/agent-cards/059: a daemon without the editor is shown read-only and sent nothing", async () => {
    let calls = setup({ catalogEdit: false });
    expect(within(daemonSection(FIXTURE_DAEMON_ID)).getByTestId("daemon-more")).toBeInTheDocument();
    let editor = await openEditor();
    expect(editor).toHaveAccessibleName(`Card actions: ${calls.localName}`);
    expect(within(editor).getByTestId("agent-card-actions-editor-readonly")).toHaveTextContent(/cannot be edited from the app/);
    expect(within(editor).queryByRole("button", { name: "Add action" })).toBeNull();
    expect(within(editor).queryByRole("button", { name: /^(Edit|Remove|Hide|Restore) / })).toBeNull();
    expect(calls.readCardCatalog).not.toHaveBeenCalled();
    expect(calls.editCardCatalog).not.toHaveBeenCalled();
    cleanup();

    calls = setup({ catalogEdit: false, remote: { catalogEdit: true, catalog: CATALOG } });
    editor = await openEditor(FIXTURE_DAEMON_ID);
    expect(within(editor).getByTestId("agent-card-actions-editor-readonly")).toBeInTheDocument();
    fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close"));
    editor = await openEditor(FIXTURE_REMOTE_DAEMON_ID);
    expect(editor).toHaveAccessibleName(`Card actions: ${calls.remoteName}`);
    expect(within(editor).queryByTestId("agent-card-actions-editor-readonly")).toBeNull();
    expect(within(editor).getByRole("button", { name: "Add action" })).toBeInTheDocument();
    expect(calls.readCardCatalog.mock.calls.map(([deckId]) => deckId)).toEqual([FIXTURE_REMOTE_DAEMON_ID]);
  });
  /// Scenario: Open the editor on a daemon whose catalog read reports a problem in the file: the problem is shown, with what to do about it, beside the actions. Then open it on a daemon whose read fails: the editor shows the reason and lists nothing — not the deck defaults, not an empty list — and offers no Add.
  it("desktop/agent-cards/065: a catalog read with problems, or one that fails, says so instead of showing a healthy catalog", async () => {
    const problem = "catalog: the user config /home/user/.config/dot-agent-deck/config.toml could not be read, so the deck defaults apply: it is not UTF-8 text";
    let calls = setup({ catalogErrors: [problem] });
    let editor = await openEditor();
    const shown = within(editor).getByText(problem);
    expect(shown).toBeVisible();
    expect(within(editor).getByTestId("agent-card-actions-editor-problems")).toHaveTextContent(/fix/i);
    expect(rows(editor).length).toBeGreaterThan(0);
    expect(calls.editCardCatalog).not.toHaveBeenCalled();
    cleanup();

    const why = "the deck did not answer within 10s";
    calls = setup({ readFails: why });
    editor = await openEditor();
    const alert = within(editor).getByRole("alert");
    expect(alert).toBeVisible();
    expect(alert).toHaveTextContent(why);
    expect(alert).toHaveTextContent(/could not be read/);
    expect(rows(editor)).toHaveLength(0);
    expect(within(editor).queryByRole("button", { name: "Add action" })).toBeNull();
    expect(calls.editCardCatalog).not.toHaveBeenCalled();
  });

  /// Scenario: Edit Ship it, set its Placement to the overlay only and tick Offer on every card, and save: the update carries both. Reopen it: the form shows that placement and the box ticked; untick it and save: the update carries `always: false` and keeps the placement. A new action added with a non-default placement carries it too. The two choices read Show on the card and Only in the Card tab, and an action whose file says `compact` shows as on the card and keeps `compact` when saved without choosing again.
  it("desktop/agent-cards/066: the form round-trips Placement and Offer on every card", async () => {
    const calls = setup();
    let editor = await openEditor();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    const placement = () => within(editorDialog()).getByRole("combobox", { name: "Placement" }) as HTMLSelectElement;
    expect(placement().value).toBe("card");
    expect([...placement().options].map((option) => option.value)).toEqual(expect.arrayContaining(["card", "detail"]));
    fireEvent.change(placement(), { target: { value: "detail" } });
    expect(field(editor, "Offer on every card")).not.toBeChecked();
    fireEvent.click(field(editor, "Offer on every card"));
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "update", action: expect.objectContaining({ name: "ship", placement: "detail", always: true }), base: expect.objectContaining({ name: "ship" }) }, INSTANCE);

    editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    expect(placement().value).toBe("detail");
    expect(field(editor, "Offer on every card")).toBeChecked();
    fireEvent.click(field(editor, "Offer on every card"));
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "update", action: expect.objectContaining({ name: "ship", placement: "detail", always: false }), base: expect.objectContaining({ name: "ship" }) }, INSTANCE);

    editor = editorDialog();
    fireEvent.click(within(editor).getByRole("button", { name: "Add action" }));
    fireEvent.change(field(editor, "Name"), { target: { value: "deploy" } });
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "Deploy it." } });
    fireEvent.change(field(editor, "Tell agents when to offer it"), { target: { value: "Offer when it can be deployed." } });
    fireEvent.change(placement(), { target: { value: "detail" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "add", action: expect.objectContaining({ name: "deploy", placement: "detail", always: false }) }, INSTANCE);
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(3);

    // Issue #2399 review 11: the two choices say what each does, and an action whose file says `compact`
    // shows as on the card and keeps `compact` when saved without choosing again.
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "ship" ? { ...entry, placement: "compact" } : entry));
    editor = editorDialog();
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    expect([...placement().options].map((option) => [option.value, option.textContent])).toEqual([["card", "Show on the card"], ["detail", "Only in the Card tab"]]);
    expect(placement().value).toBe("card");
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "update", action: expect.objectContaining({ name: "ship", placement: "compact" }), base: expect.objectContaining({ name: "ship" }) }, INSTANCE);
  });

  /// Scenario: With a local and a remote daemon whose answers arrive late and out of order, open the local daemon's editor, close it and open the remote daemon's from its ⋯ menu before the local read answers: the remote catalog is shown, and the local answer arriving afterwards does not replace it. Save an edit on the local daemon and move to the remote one the same way before the save answers: the late answer changes nothing shown, and the next edit goes to the remote daemon only. A form open when the editor moves is gone — the read-only daemon's editor offers no Save — so nothing typed for one daemon is written to another.
  it("desktop/agent-cards/067: answers that arrive after the daemon changed are dropped, and every write goes to the daemon shown", async () => {
    const remoteCatalog: EditableCatalog = { actions: [GO_AHEAD, editable("deploy", { label: "Deploy it" })], errors: [], user_config: "/srv/deck/config.toml" };
    const calls = setup({ remote: { catalogEdit: true, catalog: remoteCatalog }, deferred: true });
    const settle = async (deckId: string, kind: Pending["kind"]) => {
      const at = calls.pending.findIndex((waiting) => waiting.deckId === deckId && waiting.kind === kind);
      expect(at, `a pending ${kind} on ${deckId}`).toBeGreaterThanOrEqual(0);
      const [waiting] = calls.pending.splice(at, 1);
      await act(async () => { waiting.resolve(); });
    };
    const shownNames = () => rows(editorDialog()).map((node) => node.getAttribute("data-action"));
    /* Moving to another daemon's card actions: close the editor and open Card actions from that daemon's ⋯ menu (PR #2491 decision 2026-11-25: no picker). */
    const choose = async (deckId: string) => {
      fireEvent.click(within(editorDialog()).getByTestId("agent-card-actions-editor-close"));
      await openEditor(deckId);
    };

    await openEditor();
    await choose(FIXTURE_REMOTE_DAEMON_ID);
    await settle(FIXTURE_REMOTE_DAEMON_ID, "read");
    expect(shownNames()).toEqual(["go-ahead", "deploy"]);
    await settle(FIXTURE_DAEMON_ID, "read");
    expect(shownNames(), "the late local answer is dropped").toEqual(["go-ahead", "deploy"]);
    expect(within(editorDialog()).getByText("/srv/deck/config.toml", { exact: false })).toBeInTheDocument();

    // A form open on the remote daemon is closed when the daemon changes.
    let editor = editorDialog();
    fireEvent.click(within(row(editor, "deploy")).getByRole("button", { name: "Edit Deploy it" }));
    expect(within(editor).getByRole("button", { name: "Save" })).toBeInTheDocument();
    await choose(FIXTURE_DAEMON_ID);
    editor = editorDialog();
    expect(within(editor).queryByRole("button", { name: "Save" })).toBeNull();
    await settle(FIXTURE_DAEMON_ID, "read");
    expect(shownNames()).toEqual(["go-ahead", "merge", "ship"]);

    // A save on the local daemon whose answer arrives after switching to the remote one.
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, expect.objectContaining({ kind: "update" }), INSTANCE);
    await choose(FIXTURE_REMOTE_DAEMON_ID);
    await settle(FIXTURE_REMOTE_DAEMON_ID, "read");
    await settle(FIXTURE_DAEMON_ID, "edit");
    expect(shownNames(), "the late local save changes nothing shown").toEqual(["go-ahead", "deploy"]);
    editor = editorDialog();
    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_REMOTE_DAEMON_ID, { kind: "hide", name: "go-ahead", base: GO_AHEAD }, REMOTE_INSTANCE);
    expect(calls.editCardCatalog.mock.calls.map(([deckId]) => deckId)).toEqual([FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID]);
    cleanup();

    // Switching to a daemon that cannot be edited, with a form open: no Save, nothing sent, and switching back opens no stale form.
    const second = setup({ remote: { catalogEdit: false, catalog: remoteCatalog } });
    editor = await openEditor();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Typed for the local daemon" } });
    fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close"));
    editor = await openEditor(FIXTURE_REMOTE_DAEMON_ID);
    expect(within(editor).getByTestId("agent-card-actions-editor-readonly")).toBeInTheDocument();
    expect(within(editor).queryByRole("button", { name: "Save" })).toBeNull();
    expect(within(editor).queryByDisplayValue("Typed for the local daemon")).toBeNull();
    fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close"));
    editor = await openEditor(FIXTURE_DAEMON_ID);
    expect(within(editor).queryByDisplayValue("Typed for the local daemon")).toBeNull();
    expect(second.editCardCatalog).not.toHaveBeenCalled();
    expect(second.readCardCatalog.mock.calls.map(([deckId]) => deckId)).toEqual([FIXTURE_DAEMON_ID, FIXTURE_DAEMON_ID]);
  });

  /** Settle the first pending request of `kind` on `deckId`, the way its daemon answers. */
  const settleFirst = async (calls: ReturnType<typeof setup>, deckId: string, kind: Pending["kind"], how: (waiting: Pending) => void = (waiting) => waiting.resolve()) => {
    const at = calls.pending.findIndex((waiting) => waiting.deckId === deckId && waiting.kind === kind);
    expect(at, `a pending ${kind} on ${deckId}`).toBeGreaterThanOrEqual(0);
    const [waiting] = calls.pending.splice(at, 1);
    await act(async () => { how(waiting); });
  };
  const lost = () => Object.assign(new Error("the deck did not answer within 15s; it may have been saved, so check the list before saving again"), { outcome: "unknown" });

  /// Scenario: Hide Go ahead and have the deck's answer get lost after it was sent: the editor says Go ahead MAY have been hidden (not that it was not), reads the list once more without sending the hide again, and shows what the daemon then holds. Edit Ship it, type a new label and Save, and lose that answer too: the form stays open with the typed label and says it may have been saved, and the list is read once more. Press Refresh: the list is read again and the form keeps what it holds. A plain refusal still says the action was not hidden.
  it("desktop/agent-cards/071: an edit whose answer was lost says it may have been saved, keeps the draft and reads the list again; Refresh reads it on demand", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    let editor = editorDialog();

    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    // The daemon did save it: the list it answers next has Go ahead hidden.
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "go-ahead" ? { ...entry, hidden: true } : entry));
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    editor = editorDialog();
    expect(within(editor).getByRole("alert")).toHaveTextContent("Go ahead may have been hidden");
    expect(within(editor).getByRole("alert")).not.toHaveTextContent("was not hidden");
    expect(calls.readCardCatalog).toHaveBeenCalledTimes(2);
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(row(editor, "go-ahead")).toHaveClass("is-hidden");
    expect(calls.editCardCatalog, "the hide is never sent again").toHaveBeenCalledTimes(1);

    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    const form = within(editor).getByTestId("agent-card-actions-editor-form");
    expect(within(form).getByRole("alert")).toHaveTextContent("may have been saved");
    expect(field(editor, "Label").value, "the draft is kept").toBe("Ship it now");
    expect(calls.readCardCatalog).toHaveBeenCalledTimes(3);
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(field(editor, "Label").value).toBe("Ship it now");

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    expect(calls.readCardCatalog).toHaveBeenCalledTimes(4);
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(field(editor, "Label").value, "Refresh keeps the form").toBe("Ship it now");
    fireEvent.click(within(editor).getByRole("button", { name: "Cancel" }));

    await act(async () => { fireEvent.click(within(row(editor, "merge")).getByRole("button", { name: "Restore Merge it" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.reject("the user config changed since the editor read it"));
    expect(within(editor).getByRole("alert")).toHaveTextContent("Merge it was not restored");
    expect(calls.readCardCatalog, "a refusal wrote nothing, so nothing is read again").toHaveBeenCalledTimes(4);
  });

  /// Scenario: Edit Ship it, change its label and press Save; while the save is under way the form's fields are locked, no other action's Edit or Add opens another form, and Cancel closes this one. When the save is then refused, its reason is not attached to any form opened since; a new Add form opens empty, with no error.
  it("desktop/agent-cards/072: a save's answer belongs to the form it was sent from, and no other form opens while it is saved", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(field(editor, "Label")).toBeDisabled();
    fireEvent.change(field(editor, "Label"), { target: { value: "typed while saving" } });
    expect(field(editor, "Label").value).toBe("Ship it now");
    expect(within(row(editor, "go-ahead")).getByRole("button", { name: "Edit Go ahead" })).toBeDisabled();
    fireEvent.click(within(editor).getByRole("button", { name: "Cancel" }));
    expect(within(editor).queryByTestId("agent-card-actions-editor-form")).toBeNull();
    expect(within(editor).getByRole("button", { name: "Add action" })).toBeDisabled();

    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.reject("SHIP-REFUSED-2399"));
    expect(screen.queryByText(/SHIP-REFUSED-2399/)).toBeNull();
    fireEvent.click(within(editor).getByRole("button", { name: "Add action" }));
    const form = within(editor).getByTestId("agent-card-actions-editor-form");
    expect(within(form).queryByRole("alert")).toBeNull();
    expect(field(editor, "Name").value).toBe("");
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(1);
  });

  /// Scenario: Open the editor, then have every daemon leave the dashboard: the editor goes. When the daemon comes back, the still-open editor reads its card actions again and lists them, rather than coming back empty.
  it("desktop/agent-cards/073: the editor reads the catalog again when a daemon comes back after every daemon left", async () => {
    const calls = setup();
    await openEditor();
    expect(calls.readCardCatalog).toHaveBeenCalledTimes(1);
    await act(async () => { calls.daemonsGone(); });
    expect(screen.queryByRole("dialog", { name: EDITOR_NAME })).toBeNull();
    await act(async () => { calls.daemonsBack(); });
    const editor = editorDialog();
    expect(calls.readCardCatalog).toHaveBeenCalledTimes(2);
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "ship"]);
  });

  /// Scenario: Press Remove on Ship it. The confirmation takes focus, and while it is open the editor behind it is inert, so the keyboard cannot reach its other buttons; Escape closes only the confirmation, sends nothing and gives focus back to the Remove button that opened it.
  it("desktop/agent-cards/074: the removal's confirmation is modal over the editor", async () => {
    const calls = setup();
    const editor = await openEditor();
    const remove = within(row(editor, "ship")).getByRole("button", { name: "Remove Ship it" });
    act(() => remove.focus());
    fireEvent.click(remove);
    const confirm = screen.getByRole("alertdialog", { name: "Remove Ship it?" });
    expect(confirm.contains(document.activeElement), "focus moves into the confirmation").toBe(true);
    expect(editor.closest("[inert]"), "the editor behind it is inert").not.toBeNull();
    expect(confirm.closest("[inert]")).toBeNull();
    fireEvent.keyDown(confirm, { key: "Escape" });
    expect(screen.queryByRole("alertdialog", { name: "Remove Ship it?" })).toBeNull();
    expect(editorDialog()).toBeInTheDocument();
    expect(editor.closest("[inert]")).toBeNull();
    expect(remove).toHaveFocus();
    expect(calls.editCardCatalog).not.toHaveBeenCalled();
  });

  const shown = (editor: HTMLElement, name: string) => !row(editor, name).classList.contains("is-hidden");
  const staleReason = '"ship" changed since the editor read it (another program or editor saved the user config); nothing was saved: press Refresh to read it again, then make the change again';

  /// Scenario: Press Refresh and, while that read is still out, hide Go ahead; the hide's answer arrives first and Go ahead reads as hidden. When the Refresh's answer then arrives, carrying the list as it was before the hide, Go ahead stays hidden. Then press Refresh again and, while it is out, have a hide of Merge it lose its answer, which reads the list once more: that newer read answers first with Merge it hidden, and the older Refresh answering afterwards with Merge it shown changes nothing.
  it("desktop/agent-cards/077: a read that began before a newer write or read answers is dropped", async () => {
    const calls = setup({ deferred: true, snapshotReads: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    expect(shown(editor, "go-ahead")).toBe(true);

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit");
    expect(shown(editor, "go-ahead"), "the hide's answer").toBe(false);
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(shown(editor, "go-ahead"), "the Refresh that began before the hide answered does not undo it").toBe(false);
    expect(within(editor).getByRole("button", { name: /Refresh/ }), "the Refresh is over").toBeEnabled();

    // A newer read supersedes an older one: Merge it is shown, then hidden by an edit whose answer is lost.
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "merge" ? { ...entry, hidden: false } : entry));
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(shown(editor, "merge")).toBe(true);
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    await act(async () => { fireEvent.click(within(row(editor, "merge")).getByRole("button", { name: "Hide Merge it" })); });
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "merge" ? { ...entry, hidden: true } : entry));
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    const reads = calls.pending.filter((waiting) => waiting.kind === "read");
    expect(reads, "the Refresh and the check after the lost answer").toHaveLength(2);
    const [older, newer] = reads;
    calls.pending.splice(0, calls.pending.length);
    await act(async () => { newer.resolve(); });
    expect(shown(editor, "merge"), "the newer read").toBe(false);
    await act(async () => { older.resolve(); });
    expect(shown(editor, "merge"), "the older Refresh answering last changes nothing").toBe(false);
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(2);
  });

  /// Scenario: Edit Ship it, type a new label and Save, and lose the answer: the form says the change may have been saved and keeps the typed label, and while the list is read again the editor says it is reading it, not that it was read. That read fails: the failure is shown beside the form's message, the form keeps the typed label and Refresh can still be pressed. Pressing it reads the list.
  it("desktop/agent-cards/078: a failed reading-again after an uncertain save is shown beside the form's message", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    const form = within(editor).getByTestId("agent-card-actions-editor-form");
    expect(within(form).getByRole("alert")).toHaveTextContent("may have been saved");
    const note = () => within(editor).getByTestId("agent-card-actions-editor-reread");
    expect(note()).toHaveTextContent("Reading the list again");
    expect(editor.textContent, "nothing says it was read again before it was").not.toMatch(/was read again/);

    await settleFirst(calls, FIXTURE_DAEMON_ID, "read", (waiting) => waiting.reject("DECK-GONE-2399"));
    expect(note()).toHaveAttribute("role", "alert");
    expect(note()).toHaveTextContent("could not be read again: DECK-GONE-2399");
    expect(note()).toHaveTextContent("Whether the change was saved is not known");
    expect(within(form).getByRole("alert"), "the form's own message stays").toHaveTextContent("may have been saved");
    expect(editor.textContent).not.toMatch(/was read again/);
    expect(field(editor, "Label").value, "the draft is kept").toBe("Ship it now");
    const refresh = within(editor).getByRole("button", { name: /Refresh/ });
    expect(refresh).toBeEnabled();
    await act(async () => { fireEvent.click(refresh); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(within(editor).queryByTestId("agent-card-actions-editor-reread"), "a Refresh that read the list leaves no failure").toBeNull();
    expect(field(editor, "Label").value).toBe("Ship it now");
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(1);
  });

  /// Scenario: Edit Ship it and change its label; another program changes Ship it's text in the file meanwhile, so the daemon refuses the Save because Ship it changed since the editor read it. The form keeps the typed label and shows the reason, which names Refresh. Pressing Refresh lists the new text and keeps the typed label, and saving again sends the action as just read, so the daemon can check it against the file.
  it("desktop/agent-cards/079: an edit carries the action as the editor read it, and Refresh brings that up to date", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, expect.objectContaining({ kind: "update", base: SHIP }), INSTANCE);
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "ship" ? { ...entry, prompt: "THEIRS-2399: ship it." } : entry));
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.reject(staleReason));
    const form = within(editor).getByTestId("agent-card-actions-editor-form");
    expect(within(form).getByRole("alert")).toHaveTextContent("changed since the editor read it");
    expect(within(form).getByRole("alert")).toHaveTextContent("press Refresh");
    expect(field(editor, "Label").value).toBe("Ship it now");

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(within(row(editor, "ship")).getByText("THEIRS-2399: ship it.", { exact: false })).toBeInTheDocument();
    expect(field(editor, "Label").value, "Refresh keeps the draft").toBe("Ship it now");
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, expect.objectContaining({
      kind: "update",
      action: expect.objectContaining({ name: "ship", label: "Ship it now" }),
      base: expect.objectContaining({ name: "ship", prompt: "THEIRS-2399: ship it." }),
    }), INSTANCE);
  });

  /** Another program changes Ship it in the daemon's file: the next read lists it changed. */
  const changeShip = (calls: ReturnType<typeof setup>, change: Partial<EditableAction>) => {
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "ship" ? { ...entry, ...change } : entry));
  };

  /// Scenario: Press Refresh and, while that read is still out, press Remove on Ship it; the Refresh answers with Ship it unchanged, so the confirmation stays open, and Cancel closes it. Press Refresh again and Remove Ship it again; this time another program has relabelled it "Deploy production" and the Refresh answers with that. The confirmation asking "Remove Ship it?" closes, nothing is sent, and the editor says Ship it changed and was not removed. Pressing Remove on the relabelled row asks "Remove Deploy production?", and confirming sends that version as the base.
  it("desktop/agent-cards/133: a Refresh that changes the action a removal is asking about closes the confirmation and says so", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Remove Ship it" }));
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const unchanged = screen.getByRole("alertdialog", { name: "Remove Ship it?" });
    expect(within(editor).queryByRole("alert"), "a Refresh that read Ship it unchanged says nothing").toBeNull();
    fireEvent.click(within(unchanged).getByRole("button", { name: "Cancel" }));

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Remove Ship it" }));
    expect(screen.getByRole("alertdialog", { name: "Remove Ship it?" })).toBeInTheDocument();
    changeShip(calls, { label: "Deploy production" });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const asking = screen.queryByRole("alertdialog", { name: "Remove Ship it?" });
    /* Were it still asking, confirming is what the user would press. */
    if (asking) await act(async () => { fireEvent.click(within(asking).getByRole("button", { name: "Remove" })); });
    expect(calls.editCardCatalog, "an action the confirmation did not show is never removed").not.toHaveBeenCalled();
    expect(asking, "the confirmation of Ship it closes").toBeNull();
    expect(within(editor).getByRole("alert")).toHaveTextContent(/Ship it changed .*not removed/);
    expect(within(row(editor, "ship")).getByText("Deploy production", { exact: true })).toBeInTheDocument();

    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Remove Deploy production" }));
    await act(async () => { fireEvent.click(within(screen.getByRole("alertdialog", { name: "Remove Deploy production?" })).getByRole("button", { name: "Remove" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, { kind: "remove", name: "ship", base: { ...SHIP, label: "Deploy production" } }, INSTANCE);
  });

  /// Scenario: Press Refresh and, while that read is still out, press Edit on Ship it and type a new label; the Refresh answers with Ship it unchanged, and the form says nothing. Press Refresh again; another program has changed Ship it's text meanwhile, and the Refresh answers with it. The form keeps the typed label and says Ship it changed since the form was opened and that Save replaces it with what the form holds; Save then sends the action as just read, so the daemon checks it against the file.
  it("desktop/agent-cards/134: a Refresh that changes the action an open form edits says so in the form", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const form = within(editor).getByTestId("agent-card-actions-editor-form");
    expect(within(form).queryByRole("alert"), "a Refresh that read Ship it unchanged says nothing").toBeNull();

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    changeShip(calls, { prompt: "THEIRS-2491: ship it." });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(within(row(editor, "ship")).getByText("THEIRS-2491: ship it.", { exact: false })).toBeInTheDocument();
    expect(field(editor, "Label").value, "the draft is kept").toBe("Ship it now");
    expect(within(form).getByRole("alert"), "the user is told before a Save replaces the change").toHaveTextContent(/Ship it changed since this form was opened.*Save replaces it/);

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, expect.objectContaining({
      kind: "update",
      action: expect.objectContaining({ name: "ship", label: "Ship it now" }),
      base: { ...SHIP, prompt: "THEIRS-2491: ship it." },
    }), INSTANCE);
  });

  const formAlert = (editor: HTMLElement) => within(within(editor).getByTestId("agent-card-actions-editor-form")).getByRole("alert");
  const CHANGED_UNDER_FORM = /Ship it changed since this form was opened.*Save replaces it/;

  /// Scenario: Edit Ship it, empty its text and press Save: the daemon refuses it and the form shows why. Type a corrected text, then press Hide Go ahead and lose that hide's answer, so the list is read once more; another program has changed Ship it's text meanwhile. The form's message becomes the alert that Ship it changed since the form was opened, in place of the old refusal, before Save sends the action as just read.
  it("desktop/agent-cards/136: a reading-again after another edit's lost answer tells an open form its action changed, whatever message it showed", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.reject("VALIDATION-2491: the text it sends is empty"));
    expect(formAlert(editor)).toHaveTextContent("VALIDATION-2491");
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "My corrected draft" } });

    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    changeShip(calls, { prompt: "OTHER-WRITER-DO-NOT-OVERWRITE" });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(field(editor, "Text it sends").value, "the draft is kept").toBe("My corrected draft");
    expect(formAlert(editor), "the competing change is said in the form before Save").toHaveTextContent(CHANGED_UNDER_FORM);
    expect(formAlert(editor)).not.toHaveTextContent("VALIDATION-2491");

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, expect.objectContaining({
      kind: "update",
      action: expect.objectContaining({ name: "ship", prompt: "My corrected draft" }),
      base: { ...SHIP, prompt: "OTHER-WRITER-DO-NOT-OVERWRITE" },
    }), INSTANCE);
  });

  /// Scenario: Edit Ship it, type a new label and Save, and lose the answer: the form says it may have been saved. The list is read once more and shows Ship it with another program's text and its old label, not as the form sent it. The form's "may have been saved" message is replaced by the alert that Ship it changed since the form was opened, before Save sends the action as just read.
  it("desktop/agent-cards/137: an uncertain Save whose reading-again shows another change says Ship it changed", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    expect(formAlert(editor)).toHaveTextContent("may have been saved");
    changeShip(calls, { prompt: "OTHER-WRITER-2491: ship it." });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(field(editor, "Label").value, "the draft is kept").toBe("Ship it now");
    expect(formAlert(editor)).toHaveTextContent(CHANGED_UNDER_FORM);
    expect(formAlert(editor)).not.toHaveTextContent("may have been saved");

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, expect.objectContaining({
      kind: "update",
      action: expect.objectContaining({ name: "ship", label: "Ship it now" }),
      base: { ...SHIP, prompt: "OTHER-WRITER-2491: ship it." },
    }), INSTANCE);
  });

  /// Scenario: Edit Ship it, type a new label and Save, and lose the answer: the form says it may have been saved. The daemon did save it, so the list read once more shows Ship it exactly as the form sent it. The Save is treated as done: the form closes, the editor says Ship it now was saved, and nothing is sent again.
  it("desktop/agent-cards/138: an uncertain Save whose reading-again shows the form's own draft is treated as saved", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    changeShip(calls, { label: "Ship it now" });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    expect(formAlert(editor)).toHaveTextContent("may have been saved");
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(within(editor).queryByTestId("agent-card-actions-editor-form"), "a Save the list shows landed closes its form").toBeNull();
    expect(within(editor).getByTestId("agent-card-actions-editor-reread")).toHaveTextContent("Ship it now was saved");
    expect(within(row(editor, "ship")).getByText("Ship it now", { exact: true })).toBeInTheDocument();
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(1);
  });

  /// Scenario: Edit Ship it and type a new label. Another program changes Ship it's text and Refresh reads it: the form shows the alert that Ship it changed. Another program changes the text again and Refresh reads it again: the alert is shown afresh, as a new element, so it is announced again, and Save sends the action as last read.
  it("desktop/agent-cards/139: each change a read finds under an open form is alerted afresh", async () => {
    const calls = setup({ deferred: true });
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    changeShip(calls, { prompt: "THEIRS-ONE-2491." });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const first = formAlert(editor);
    expect(first).toHaveTextContent(CHANGED_UNDER_FORM);

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: /Refresh/ })); });
    changeShip(calls, { prompt: "THEIRS-TWO-2491." });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const second = formAlert(editor);
    expect(second).toHaveTextContent(CHANGED_UNDER_FORM);
    expect(second, "the second change is alerted by a new element, not the first one left in place").not.toBe(first);
    expect(first.isConnected).toBe(false);

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, expect.objectContaining({
      kind: "update",
      action: expect.objectContaining({ name: "ship", label: "Ship it now" }),
      base: { ...SHIP, prompt: "THEIRS-TWO-2491." },
    }), INSTANCE);
  });

  /* PR #2491 re-audit (hidden default): Merge it starts as the user's override of the deck default, whose text differs from the shipped one. */
  const MERGE_OVERRIDE: EditableAction = { ...MERGE, prompt: "AUDIT-2491: the user's own merge text.", source: "user", overridden: true, hidden: false };
  /** Open the editor on a catalog whose Merge it is `MERGE_OVERRIDE`, Edit Merge it, type the shipped text back and Save, and lose the answer. */
  const saveMergeBackToShipped = async () => {
    const calls = setup({ deferred: true });
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "merge" ? { ...MERGE_OVERRIDE } : entry));
    await openEditor();
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    const editor = editorDialog();
    fireEvent.click(within(row(editor, "merge")).getByRole("button", { name: "Edit Merge it" }));
    fireEvent.change(field(editor, "Text it sends"), { target: { value: MERGE.prompt } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    await settleFirst(calls, FIXTURE_DAEMON_ID, "edit", (waiting) => waiting.fail(lost()));
    expect(formAlert(editor)).toHaveTextContent("may have been saved");
    return { calls, editor };
  };
  /** Another program hides Merge it: the daemon drops the user's override and lists the deck's own values, hidden. */
  const hideMergeElsewhere = (calls: ReturnType<typeof setup>) => {
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "merge" ? { ...MERGE } : entry));
  };
  const MERGE_NOW_HIDDEN = /Merge it changed since this form was opened: it is now hidden.*Save replaces it/;

  /// Scenario: Merge it is the user's own version of the deck default. Edit Merge it, type the deck's own text back and Save, and lose the answer. Another program hides Merge it, so the list read once more shows the deck's own values, hidden. The form stays open with the alert that Merge it changed since it was opened and is now hidden, and the editor does not say Merge it was saved.
  it("desktop/agent-cards/140: an uncertain Save whose reading-again shows the action hidden is not treated as saved", async () => {
    const { calls, editor } = await saveMergeBackToShipped();
    hideMergeElsewhere(calls);
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(within(editor).queryByTestId("agent-card-actions-editor-form"), "a hidden action is not the Save landing").not.toBeNull();
    expect(formAlert(editor)).toHaveTextContent(MERGE_NOW_HIDDEN);
    expect(within(editor).getByTestId("agent-card-actions-editor-reread")).not.toHaveTextContent("was saved");
    expect(field(editor, "Text it sends").value, "the draft is kept").toBe(MERGE.prompt);

    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, expect.objectContaining({
      kind: "update",
      action: expect.objectContaining({ name: "merge", prompt: MERGE.prompt }),
      base: MERGE,
    }), INSTANCE);
  });

  /// Scenario: as 140, but after the answer is lost the user types a newer text before the list is read once more, which shows Merge it hidden by another program. The form keeps the newer text and shows the alert that Merge it changed and is now hidden, not the message that it was saved as the form sent it earlier.
  it("desktop/agent-cards/141: a draft typed into after an uncertain Save is told its action is now hidden, not that it was saved", async () => {
    const { calls, editor } = await saveMergeBackToShipped();
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "NEWER-2491: merge it." } });
    hideMergeElsewhere(calls);
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(field(editor, "Text it sends").value, "the draft is kept").toBe("NEWER-2491: merge it.");
    expect(formAlert(editor)).toHaveTextContent(MERGE_NOW_HIDDEN);
    expect(formAlert(editor)).not.toHaveTextContent("was saved as this form sent it earlier");
    expect(within(editor).getByTestId("agent-card-actions-editor-reread")).not.toHaveTextContent("was saved");
  });

  /// Scenario: as 140, but the daemon did save the form: the list read once more shows Merge it with the deck's own text as the user's version, in effect. The Save is treated as done: the form closes, the editor says Merge it was saved, and nothing is sent again.
  it("desktop/agent-cards/142: an uncertain Save of a deck default whose reading-again shows the user's version in effect is treated as saved", async () => {
    const { calls, editor } = await saveMergeBackToShipped();
    calls.catalogs[FIXTURE_DAEMON_ID].actions = calls.catalogs[FIXTURE_DAEMON_ID].actions.map((entry) => (entry.name === "merge" ? { ...MERGE, source: "user", overridden: true, hidden: false } : entry));
    await settleFirst(calls, FIXTURE_DAEMON_ID, "read");
    expect(within(editor).queryByTestId("agent-card-actions-editor-form"), "a Save the list shows landed closes its form").toBeNull();
    expect(within(editor).getByTestId("agent-card-actions-editor-reread")).toHaveTextContent("Merge it was saved");
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(1);
  });
});
