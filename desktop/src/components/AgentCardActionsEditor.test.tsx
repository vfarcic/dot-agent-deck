import "../styles.css";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import { deckName } from "../lib/displayText";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type DeckRuntimeState } from "../types";
import { AgentOverview } from "./AgentOverview";

/*
 * PRD #2374 decision 2026-11-24 (1), issue #2399 on PR #2491 — the desktop
 * app's editor for card actions (`desktop/agent-cards/053`–`059`). TDD (RED):
 * written before the editor exists.
 *
 * The contract these hold the implementation to:
 * - the Cards toolbar has a "Card actions" button (`agent-cards-actions-editor-open`)
 *   that opens a dialog named "Card actions" (`agent-card-actions-editor`);
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
 *   sends", "When agents should offer it", "Ask before sending" and "Offer on
 *   every card"; its icon is chosen with "Choose icon", which opens a
 *   searchable grid (listbox "Icons", search box "Search icons") of the icons
 *   the app bundles; "Save" sends the edit;
 * - removing asks first, in the editor's own confirmation, named "Remove <label>?".
 */

const INSTANCE = "daemon-instance-editor";
const REMOTE_INSTANCE = "daemon-instance-editor-remote";
const USER_CONFIG = "/home/user/.config/dot-agent-deck/config.toml";

type EditableAction = {
  name: string; label: string; description: string; prompt: string; icon: string | null; placement: string;
  confirm: boolean; always: boolean; source: "default" | "user"; default: boolean; overridden: boolean; hidden: boolean;
};
type EditableCatalog = { actions: EditableAction[]; errors: string[]; user_config: string | null };
type Edit = { kind: "add" | "update"; action: Record<string, unknown> } | { kind: "remove" | "hide" | "restore"; name: string };

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
function setup({ catalogEdit = true, remote, refuse }: { catalogEdit?: boolean; remote?: { catalogEdit: boolean; catalog: EditableCatalog }; refuse?: string } = {}) {
  const [local, far] = createFixtureFleet("fleet");
  const fleet = [{ ...local, connection: { ...local.connection, incarnation: INSTANCE } }];
  if (remote) fleet.push({ ...far, connection: { ...far.connection, incarnation: REMOTE_INSTANCE } });
  const localCard = cardFor("1", "coder", INSTANCE);
  fleet[0] = { ...fleet[0], agents: [createFixtureStartedAgent({ id: "1", daemonId: FIXTURE_DAEMON_ID, displayName: "coder", command: "claude", cwd: "/w" })] };
  const catalogs: Record<string, EditableCatalog> = { [FIXTURE_DAEMON_ID]: structuredClone(CATALOG) };
  if (remote) catalogs[FIXTURE_REMOTE_DAEMON_ID] = structuredClone(remote.catalog);
  const readCardCatalog = vi.fn(async (deckId: string) => structuredClone(catalogs[deckId]));
  const editCardCatalog = vi.fn(async (deckId: string, edit: Edit) => {
    if (refuse) throw new Error(refuse);
    const catalog = catalogs[deckId];
    if (edit.kind === "add") catalog.actions.push(editable(String(edit.action.name), edit.action as Partial<EditableAction>));
    if (edit.kind === "update") catalog.actions = catalog.actions.map((entry) => (entry.name === edit.action.name ? { ...entry, ...(edit.action as Partial<EditableAction>), source: "user", overridden: entry.default } : entry));
    if (edit.kind === "remove") catalog.actions = catalog.actions.filter((entry) => entry.name !== edit.name);
    if (edit.kind === "hide") catalog.actions = catalog.actions.map((entry) => (entry.name === edit.name ? { ...entry, hidden: true } : entry));
    if (edit.kind === "restore") catalog.actions = catalog.actions.map((entry) => (entry.name === edit.name ? { ...entry, hidden: false, overridden: false, source: "default" } : entry));
    return structuredClone(catalog);
  });
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
  render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
  return { readCardCatalog, editCardCatalog, localName: deckName(fleet[0].connection), remoteName: remote ? deckName(fleet[1].connection) : "" };
}

async function openEditor() {
  await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Card actions" })); });
  return screen.getByRole("dialog", { name: "Card actions" });
}

const rows = (editor: HTMLElement) => within(editor).queryAllByTestId("agent-card-actions-editor-row");
const row = (editor: HTMLElement, name: string) => {
  const found = rows(editor).find((node) => node.getAttribute("data-action") === name);
  if (!found) throw new Error(`no editor row for ${name}`);
  return found;
};
const field = (scope: HTMLElement, label: string | RegExp) => within(scope).getByLabelText(label) as HTMLInputElement;

beforeEach(() => {
  window.localStorage.clear();
  window.localStorage.setItem("dot-agent-deck.overview.mode", "cards");
  act(() => setDashboardFilter(clearDashboardFilter()));
});
afterEach(() => { cleanup(); window.localStorage.clear(); });

describe("the card actions editor (PRD #2374, 2026-11-24)", () => {
  /// Scenario: On the Cards view, press Card actions. The editor reads the local daemon's catalog and lists Go ahead and a hidden Merge it as deck defaults and Ship it as the user's, each with its label, source and the exact text it sends, names the user config it edits, offers only the buttons each source allows, and lists none of the deck's own Reply, Open or Close.
  it("desktop/agent-cards/053: the editor lists each card action with where it came from", async () => {
    const calls = setup();
    expect(screen.getByTestId("agent-cards-actions-editor-open")).toHaveAccessibleName("Card actions");
    const editor = await openEditor();
    expect(editor).toHaveAttribute("data-testid", "agent-card-actions-editor");
    expect(calls.readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID);
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "ship"]);
    expect(rows(editor).map((node) => node.getAttribute("data-source"))).toEqual(["deck", "deck", "user"]);
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

  /// Scenario: In the editor, press Add action, fill in the name, label, the text it sends over two lines and when agents should offer it, tick Ask before sending, then press Choose icon, search the icon grid for "rock" and pick the rocket. Save sends one add with exactly those values to the local daemon, and the list then shows the new action as the user's.
  it("desktop/agent-cards/054: adding an action with an icon from the picker sends one add and lists it", async () => {
    const calls = setup();
    const editor = await openEditor();
    fireEvent.click(within(editor).getByRole("button", { name: "Add action" }));
    fireEvent.change(field(editor, "Name"), { target: { value: "deploy" } });
    fireEvent.change(field(editor, "Label"), { target: { value: "Deploy it" } });
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "Deploy it.\nThen update your card." } });
    fireEvent.change(field(editor, "When agents should offer it"), { target: { value: "Offer when the change can be deployed." } });
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
    });
    const listed = row(screen.getByRole("dialog", { name: "Card actions" }), "deploy");
    expect(listed).toHaveAttribute("data-source", "user");
    expect(within(listed).getByText("Deploy it", { exact: true })).toBeInTheDocument();
  });

  /// Scenario: Edit the user's Ship it: its name is shown but cannot be changed, and the form holds its label, multi-line text, description, icon and confirmation; changing the label and saving sends one update carrying the rest unchanged. Then edit the deck's Go ahead and change its text: saving sends an update of the same name, which the list shows as the user's version of the deck default with Restore offered.
  it("desktop/agent-cards/055: editing keeps the name fixed and an edited default becomes the user's version", async () => {
    const calls = setup();
    let editor = await openEditor();
    fireEvent.click(within(row(editor, "ship")).getByRole("button", { name: "Edit Ship it" }));
    const name = field(editor, "Name");
    expect(name.value).toBe("ship");
    expect(name.readOnly || name.disabled).toBe(true);
    expect(field(editor, "Label").value).toBe("Ship it");
    expect(field(editor, "Text it sends").value).toBe("Ship it.\nThen update your card.");
    expect(field(editor, "When agents should offer it").value).toBe("Offer when it is ready.");
    expect(field(editor, "Ask before sending")).toBeChecked();
    expect(within(editor).getByTestId("agent-card-actions-editor-icon")).toHaveAttribute("data-icon", "rocket");
    fireEvent.change(field(editor, "Label"), { target: { value: "Ship it now" } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, {
      kind: "update",
      action: expect.objectContaining({ name: "ship", label: "Ship it now", prompt: "Ship it.\nThen update your card.", description: "Offer when it is ready.", icon: "rocket", confirm: true }),
    });

    editor = screen.getByRole("dialog", { name: "Card actions" });
    fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Edit Go ahead" }));
    fireEvent.change(field(editor, "Text it sends"), { target: { value: "Carry on, then update your card." } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, {
      kind: "update",
      action: expect.objectContaining({ name: "go-ahead", label: "Go ahead", prompt: "Carry on, then update your card.", icon: "play" }),
    });
    const go = row(screen.getByRole("dialog", { name: "Card actions" }), "go-ahead");
    expect(go).toHaveAttribute("data-source", "user");
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
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, { kind: "remove", name: "ship" });
    editor = screen.getByRole("dialog", { name: "Card actions" });
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge"]);

    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "hide", name: "go-ahead" });
    editor = screen.getByRole("dialog", { name: "Card actions" });
    await act(async () => { fireEvent.click(within(row(editor, "merge")).getByRole("button", { name: "Restore Merge it" })); });
    expect(calls.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, { kind: "restore", name: "merge" });
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
    fireEvent.change(field(editor, "When agents should offer it"), { target: { value: "Offer always." } });
    await act(async () => { fireEvent.click(within(editor).getByRole("button", { name: "Save" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledTimes(1);
    expect(within(editor).getByRole("alert")).toHaveTextContent("bad name; use 1 to 40 lowercase letters");
    expect(field(editor, "Name").value).toBe("Bad Name");
    expect(field(editor, "Text it sends").value).toBe("Do it.");
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "ship"]);
  });

  /// Scenario: With a local and a remote daemon connected, open the editor: it edits the local daemon by default and says so; choosing the remote daemon reads that daemon's catalog and lists it, and an edit made then goes to the remote daemon only.
  it("desktop/agent-cards/058: with several daemons the editor edits the one picked, the local one by default", async () => {
    const remoteCatalog: EditableCatalog = { actions: [GO_AHEAD, { ...MERGE, hidden: false }, editable("deploy", { label: "Deploy it" })], errors: [], user_config: "/srv/deck/config.toml" };
    const calls = setup({ remote: { catalogEdit: true, catalog: remoteCatalog } });
    let editor = await openEditor();
    const picker = within(editor).getByRole("combobox", { name: "Daemon" }) as HTMLSelectElement;
    expect(picker.selectedOptions[0]).toHaveTextContent(calls.localName);
    expect(calls.readCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID);
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "ship"]);
    const remoteOption = [...picker.options].find((option) => option.textContent?.includes(calls.remoteName));
    expect(remoteOption).toBeDefined();
    await act(async () => { fireEvent.change(picker, { target: { value: remoteOption!.value } }); });
    expect(calls.readCardCatalog).toHaveBeenLastCalledWith(FIXTURE_REMOTE_DAEMON_ID);
    editor = screen.getByRole("dialog", { name: "Card actions" });
    expect(rows(editor).map((node) => node.getAttribute("data-action"))).toEqual(["go-ahead", "merge", "deploy"]);
    expect(within(editor).getByText("/srv/deck/config.toml", { exact: false })).toBeInTheDocument();
    await act(async () => { fireEvent.click(within(row(editor, "go-ahead")).getByRole("button", { name: "Hide Go ahead" })); });
    expect(calls.editCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, { kind: "hide", name: "go-ahead" });
  });

  /// Scenario: Open the editor for a daemon from an older release that does not offer it, alone and then beside one that does. For the older daemon the editor says its release cannot be edited from the app, offers no Add, Edit, Remove, Hide or Restore, and sends that daemon nothing; picking the newer daemon edits it as usual.
  it("desktop/agent-cards/059: a daemon without the editor is shown read-only and sent nothing", async () => {
    let calls = setup({ catalogEdit: false });
    let editor = await openEditor();
    expect(within(editor).getByTestId("agent-card-actions-editor-readonly")).toHaveTextContent(/cannot be edited from the app/);
    expect(within(editor).queryByRole("button", { name: "Add action" })).toBeNull();
    expect(within(editor).queryByRole("button", { name: /^(Edit|Remove|Hide|Restore) / })).toBeNull();
    expect(calls.readCardCatalog).not.toHaveBeenCalled();
    expect(calls.editCardCatalog).not.toHaveBeenCalled();
    cleanup();

    calls = setup({ catalogEdit: false, remote: { catalogEdit: true, catalog: CATALOG } });
    editor = await openEditor();
    expect(within(editor).getByTestId("agent-card-actions-editor-readonly")).toBeInTheDocument();
    const picker = within(editor).getByRole("combobox", { name: "Daemon" }) as HTMLSelectElement;
    const remoteOption = [...picker.options].find((option) => option.textContent?.includes(calls.remoteName));
    await act(async () => { fireEvent.change(picker, { target: { value: remoteOption!.value } }); });
    editor = screen.getByRole("dialog", { name: "Card actions" });
    expect(within(editor).queryByTestId("agent-card-actions-editor-readonly")).toBeNull();
    expect(within(editor).getByRole("button", { name: "Add action" })).toBeInTheDocument();
    expect(calls.readCardCatalog.mock.calls.map(([deckId]) => deckId)).toEqual([FIXTURE_REMOTE_DAEMON_ID]);
  });
});
