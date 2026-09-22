import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, createFixtureStartedAgent } from "../data/fixture";
import type { AgentSession, ConnectionView, DeckDirectoryListing, DeckSnapshot, NewAgentOptions, NewAgentOrchestrations } from "../types";
import { NewAgentDialog, type NewAgentRuntime } from "./NewAgentDialog";

const LOCAL = "deck-000000000000aaaa";
const REMOTE = "deck-000000000000bbbb";

function deck(deckId: string, patch: Partial<ConnectionView> = {}, agents: AgentSession[] = []): DeckSnapshot {
  return { ...createFixtureSnapshot("connected"), agents, connection: { status: "connected", deckId, socketPath: `dev@${deckId}`, deckKind: "remote", ...patch } };
}

/**
 * A deck's tree, as the fake deck answers it. HOME's parent is deliberately a
 * path no trimming of HOME produces, so "up" can be seen to send the reply's
 * `parent` rather than a string the dialog built.
 */
const TREE: Record<string, Extract<DeckDirectoryListing, { kind: "listing" }>> = {
  "": {
    kind: "listing",
    path: "/home/dev",
    displayPath: "/home/dev",
    parent: "/canonical-parent-of-home",
    entries: [
      { path: "/home/dev/Alpha-project", displayName: "Alpha-project", isProject: true },
      { path: "/home/dev/beta", displayName: "beta", isProject: false },
    ],
    truncated: false,
  },
  "/home/dev": undefined as never,
  "/home/dev/beta": { kind: "listing", path: "/home/dev/beta", displayPath: "/home/dev/beta", parent: "/home/dev", entries: [{ path: "/home/dev/beta/leaf", displayName: "leaf", isProject: false }], truncated: false },
  "/home/dev/Alpha-project": { kind: "listing", path: "/home/dev/Alpha-project", displayPath: "/home/dev/Alpha-project", parent: "/home/dev", entries: [], truncated: false },
  "/home/dev/beta/leaf": { kind: "listing", path: "/home/dev/beta/leaf", displayPath: "/home/dev/beta/leaf", parent: "/home/dev/beta", entries: [], truncated: false },
  "/canonical-parent-of-home": { kind: "listing", path: "/canonical-parent-of-home", displayPath: "/canonical-parent-of-home", entries: [{ path: "/home/dev", displayName: "dev", isProject: false }], truncated: false },
  "/typed/link/": { kind: "listing", path: "/real/target", displayPath: "/real/target", parent: "/real", entries: [], truncated: false },
};
TREE["/home/dev"] = TREE[""];

const DECK_OPTIONS: NewAgentOptions = {
  kind: "deck",
  agents: [
    { id: "claude", displayName: "ClaudeCode", defaultCommand: "claude" },
    { id: "pi", displayName: "Pi", defaultCommand: "pi --thinking" },
  ],
  experimental: false,
  authoringKinds: [],
};

function fakeRuntime(overrides: Partial<NewAgentRuntime> = {}) {
  const runtime = {
    fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE, { status: "disconnected" })],
    runAction: vi.fn(async () => ({ ok: true, agentId: "7" })),
    clearError: vi.fn(),
    listDirectories: vi.fn(async (_deckId: string, path?: string): Promise<DeckDirectoryListing> => {
      const listing = TREE[path ?? ""];
      if (!listing) throw new Error("daemon returned error: unresolved: that path did not resolve to a readable directory on this daemon");
      return structuredClone(listing);
    }),
    newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => structuredClone(DECK_OPTIONS)),
    ...overrides,
  };
  return runtime;
}

type FakeRuntime = ReturnType<typeof fakeRuntime>;

function renderDialog(runtime: FakeRuntime, props: { initialDeckId?: string; appearTimeoutMs?: number } = {}) {
  const onClose = vi.fn();
  const onAppeared = vi.fn();
  const onNotAppeared = vi.fn();
  const element = (current: FakeRuntime) => <NewAgentDialog runtime={current} onClose={onClose} onAppeared={onAppeared} onNotAppeared={onNotAppeared} {...props} />;
  const utils = render(element(runtime));
  return { ...utils, onClose, onAppeared, onNotAppeared, rerenderWith: (next: FakeRuntime) => utils.rerender(element(next)) };
}

const deckList = () => screen.getByTestId("new-agent-deck-list");
const directoryList = () => screen.getByTestId("new-agent-directory-list");
const activeRow = () => directoryList().querySelector("[aria-selected='true']")?.getAttribute("data-path");
const currentPath = async (path: string) => expect(await screen.findByTestId("new-agent-current-path")).toHaveTextContent(path);

/** Confirm the only eligible deck and browse into a directory with no subdirectories, then use it. */
async function reachForm() {
  fireEvent.keyDown(deckList(), { key: "Enter" });
  await currentPath("/home/dev");
  fireEvent.keyDown(directoryList(), { key: "j" });
  fireEvent.keyDown(directoryList(), { key: "Enter" });
  await currentPath("/home/dev/beta");
  fireEvent.keyDown(directoryList(), { key: "Enter" });
  await currentPath("/home/dev/beta/leaf");
  fireEvent.keyDown(directoryList(), { key: "Enter" });
  await screen.findByTestId("new-agent-form");
}

describe("New agent dialog — deck step (PRD #1223 M4)", () => {
  /**
   * Scenario: open the flow over a fleet of four decks — one connected, one
   * disconnected with its own message, one still waiting to report and one
   * incompatible. Every deck is listed; the three that cannot take a spawn are
   * disabled and each says why, and clicking one of them starts nothing.
   */
  it("lists every deck and disables the ones that cannot take a spawn, with the reason", () => {
    const runtime = fakeRuntime({
      fleet: [
        deck(LOCAL, { deckKind: "local" }),
        deck(REMOTE, { status: "disconnected", message: "ssh: connect to host build-box port 22: Connection refused" }),
        deck("deck-000000000000cccc", { status: "loading", pending: true }),
        deck("deck-000000000000dddd", { status: "error", message: "Protocol handshake failed." }),
      ],
    });
    renderDialog(runtime);

    const options = within(deckList()).getAllByRole("option");
    expect(options).toHaveLength(4);
    expect(options[0]).not.toHaveAttribute("aria-disabled");
    expect(options[1]).toHaveAttribute("aria-disabled", "true");
    expect(options[1]).toHaveTextContent("Connection refused");
    expect(options[2]).toHaveTextContent("This deck has not reported yet.");
    expect(options[3]).toHaveTextContent("Protocol handshake failed.");
    fireEvent.click(options[1]);
    expect(runtime.listDirectories).not.toHaveBeenCalled();
    expect(screen.getByTestId("new-agent-dialog")).toHaveAttribute("data-step", "deck");
  });

  /**
   * Scenario: with exactly one deck able to take a spawn, the step opens with
   * it selected, and a single Enter confirms it: the directory step asks THAT
   * deck for its home directory.
   */
  it("preselects the only eligible deck and confirms it with one Enter", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);

    expect(within(deckList()).getAllByRole("option")[0]).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(deckList(), { key: "Enter" });

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
  });

  /**
   * Scenario: two decks can take a spawn and the flow was opened from the
   * remote one's header. That deck is preselected, and Enter lists its home.
   */
  it("preselects the deck the flow was opened from", async () => {
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE)] });
    renderDialog(runtime, { initialDeckId: REMOTE });

    expect(deckList().querySelector("[aria-selected='true']")).toHaveAttribute("data-deck-id", REMOTE);
    fireEvent.keyDown(deckList(), { key: "Enter" });

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(REMOTE, undefined);
    expect(screen.getByTestId("new-agent-chosen-deck")).toHaveTextContent(`dev@${REMOTE}`);
  });

  /**
   * Scenario: two decks can take a spawn and a disconnected one sits between
   * them. Nothing is preselected and Next is disabled; `j` moves to the first
   * eligible deck and again past the disconnected one to the second, and Enter
   * confirms the deck the cursor is on.
   */
  it("preselects nothing between two eligible decks and moves over disabled ones", async () => {
    const third = "deck-000000000000eeee";
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE, { status: "disconnected" }), deck(third)] });
    renderDialog(runtime);

    expect(deckList().querySelector("[aria-selected='true']")).toBeNull();
    expect(screen.getByTestId("new-agent-deck-next")).toBeDisabled();
    fireEvent.keyDown(deckList(), { key: "j" });
    expect(deckList().querySelector("[aria-selected='true']")).toHaveAttribute("data-deck-id", LOCAL);
    fireEvent.keyDown(deckList(), { key: "ArrowDown" });
    expect(deckList().querySelector("[aria-selected='true']")).toHaveAttribute("data-deck-id", third);
    fireEvent.keyDown(deckList(), { key: "Enter" });

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(third, undefined);
  });
});

describe("New agent dialog — directory step (PRD #1223 M4)", () => {
  /**
   * Scenario: browse with the TUI picker's keys. The home listing opens with
   * the cursor on its first subdirectory, the project marked; `j` moves to
   * `beta` and Enter lists it by the path the deck gave. Left goes up by the
   * reply's `parent` and lands the cursor back on `beta`; Backspace goes up
   * again by home's `parent` — a path no trimming of home produces. Space then
   * uses that directory, and the form names it after its last component.
   */
  it("moves, enters, goes up through the reply's parent, and confirms with Space", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    fireEvent.keyDown(deckList(), { key: "Enter" });
    await currentPath("/home/dev");

    expect(activeRow()).toBe("/home/dev/Alpha-project");
    expect(within(directoryList()).getAllByTestId("new-agent-project-mark")).toHaveLength(1);
    fireEvent.keyDown(directoryList(), { key: "j" });
    expect(activeRow()).toBe("/home/dev/beta");
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await currentPath("/home/dev/beta");
    expect(runtime.listDirectories).toHaveBeenLastCalledWith(LOCAL, "/home/dev/beta");

    fireEvent.keyDown(directoryList(), { key: "ArrowLeft" });
    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenLastCalledWith(LOCAL, "/home/dev");
    expect(activeRow()).toBe("/home/dev/beta");

    fireEvent.keyDown(directoryList(), { key: "Backspace" });
    await currentPath("/canonical-parent-of-home");
    expect(runtime.listDirectories).toHaveBeenLastCalledWith(LOCAL, "/canonical-parent-of-home");

    fireEvent.keyDown(directoryList(), { key: " " });
    expect(await screen.findByTestId("new-agent-dir")).toHaveTextContent("/canonical-parent-of-home");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("canonical-parent-of-home");
  });

  /**
   * Scenario: enter a directory that has no subdirectories and press Enter.
   * That directory is confirmed — the form opens on it and asks the deck for
   * its options — rather than Enter doing nothing.
   */
  it("confirms a directory with no subdirectories on Enter", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    await reachForm();

    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/beta/leaf");
    expect(runtime.newAgentOptions).toHaveBeenCalledWith(LOCAL);
  });

  /**
   * Scenario: press `/` in the listing, which puts the caret in the filter;
   * typing `ALP` leaves `..` and the one matching directory. Escape in the
   * filter clears it and returns to the listing without closing the dialog.
   */
  it("filters with / and clears the filter on Escape without closing", async () => {
    const runtime = fakeRuntime();
    const { onClose } = renderDialog(runtime);
    fireEvent.keyDown(deckList(), { key: "Enter" });
    await currentPath("/home/dev");

    fireEvent.keyDown(directoryList(), { key: "/" });
    const filter = screen.getByTestId("new-agent-filter");
    expect(filter).toHaveFocus();
    fireEvent.change(filter, { target: { value: "ALP" } });
    expect(within(directoryList()).getAllByRole("option").map((row) => row.getAttribute("data-path"))).toEqual(["/canonical-parent-of-home", "/home/dev/Alpha-project"]);

    fireEvent.keyDown(filter, { key: "Escape" });
    expect(filter).toHaveValue("");
    expect(within(directoryList()).getAllByRole("option")).toHaveLength(3);
    expect(directoryList()).toHaveFocus();
    expect(onClose).not.toHaveBeenCalled();
  });

  /** Scenario: a listing the deck cut short says so. */
  it("shows that a truncated listing is incomplete", async () => {
    const runtime = fakeRuntime({
      listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => ({ ...structuredClone(TREE[""]), truncated: true })),
    });
    renderDialog(runtime);
    fireEvent.keyDown(deckList(), { key: "Enter" });

    expect(await screen.findByTestId("new-agent-truncated")).toBeVisible();
  });

  /**
   * Scenario: type into the path field, including the picker's own letters —
   * they stay in the field and move nothing. Submitting sends the text to the
   * deck exactly as typed, trailing slash included; the deck answers with its
   * canonical spelling, the keyboard returns to the listing, and Space carries
   * that spelling into the form, which names the agent after it.
   */
  it("sends a typed path verbatim and carries the deck's canonical reply", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    fireEvent.keyDown(deckList(), { key: "Enter" });
    await currentPath("/home/dev");

    const path = screen.getByTestId("new-agent-path");
    fireEvent.keyDown(path, { key: "j" });
    fireEvent.keyDown(path, { key: "l" });
    expect(activeRow()).toBe("/home/dev/Alpha-project");
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
    fireEvent.change(path, { target: { value: "/typed/link/" } });
    fireEvent.submit(path.closest("form")!);

    await currentPath("/real/target");
    expect(runtime.listDirectories).toHaveBeenLastCalledWith(LOCAL, "/typed/link/");
    await waitFor(() => expect(directoryList()).toHaveFocus());
    fireEvent.keyDown(directoryList(), { key: " " });
    expect(await screen.findByTestId("new-agent-dir")).toHaveTextContent("/real/target");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("target");
  });

  /** Scenario: a typed path the deck refuses keeps the listing on screen and shows the deck's refusal. */
  it("shows a refused typed path inline and keeps the listing", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    fireEvent.keyDown(deckList(), { key: "Enter" });
    await currentPath("/home/dev");

    const path = screen.getByTestId("new-agent-path");
    fireEvent.change(path, { target: { value: "/no/such/dir" } });
    fireEvent.submit(path.closest("form")!);

    expect(await screen.findByTestId("new-agent-directory-error")).toHaveTextContent("unresolved");
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent("/home/dev");
  });
});

describe("New agent dialog — form (PRD #1223 M4)", () => {
  /**
   * Scenario: reach the form against three decks' options. Command is the
   * deck's configured default command when it has one — ahead of the last
   * command — then the last command started on that deck, then blank.
   */
  it.each([
    ["the default command", { defaultCommand: "opencode", lastCommand: "claude" }, "opencode"],
    ["the last command", { lastCommand: "claude --model haiku" }, "claude --model haiku"],
    ["blank", {}, ""],
  ])("prefills Command with %s", async (_case, patch, expected) => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(DECK_OPTIONS), ...patch })) });
    renderDialog(runtime);
    await reachForm();

    await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue(expected));
    expect(screen.getByTestId("new-agent-name")).toHaveValue("leaf");
  });

  /**
   * Scenario: type a command of your own, then pick Pi in the Agent picker —
   * Command is overwritten with Pi's default command. Going back to `auto`
   * leaves it as it is.
   */
  it("overwrites Command with the chosen agent's default command", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    await reachForm();
    const agent = screen.getByTestId("new-agent-agent");
    await waitFor(() => expect(within(agent).getAllByRole("option")).toHaveLength(3));

    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "my-own-agent --flag" } });
    fireEvent.change(agent, { target: { value: "pi" } });
    expect(screen.getByTestId("new-agent-command")).toHaveValue("pi --thinking");
    fireEvent.change(agent, { target: { value: "auto" } });
    expect(screen.getByTestId("new-agent-command")).toHaveValue("pi --thinking");
  });

  /**
   * Scenario: submit the form. The start names the deck captured at the deck
   * step, the directory the deck returned, and the Name and Command as they
   * stand; a blank Command sends no command at all, which starts the deck's
   * default shell.
   */
  it("starts on the captured deck with the form's values, and a blank Command sends none", async () => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(DECK_OPTIONS), lastCommand: "claude" })) });
    renderDialog(runtime);
    await reachForm();
    await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("claude"));

    fireEvent.click(screen.getByTestId("new-agent-start"));
    await waitFor(() => expect(runtime.runAction).toHaveBeenCalledTimes(1));
    expect(runtime.runAction).toHaveBeenCalledWith({ type: "start_agent", deckId: LOCAL, cwd: "/home/dev/beta/leaf", command: "claude", displayName: "leaf" });
  });

  it("sends no command for a blank Command", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    await reachForm();
    await waitFor(() => expect(runtime.newAgentOptions).toHaveBeenCalled());

    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "   " } });
    fireEvent.submit(screen.getByTestId("new-agent-form"));
    await waitFor(() => expect(runtime.runAction).toHaveBeenCalledTimes(1));
    expect(runtime.runAction).toHaveBeenCalledWith({ type: "start_agent", deckId: LOCAL, cwd: "/home/dev/beta/leaf", displayName: "leaf" });
  });

  /**
   * Scenario: the deck refuses the start. The dialog stays open on the form
   * with the refusal inline, the Name and Command exactly as entered, Start
   * available again — and the runtime's global copy of the error is dropped.
   */
  it("keeps the dialog open with an inline error and the values entered", async () => {
    const runtime = fakeRuntime({ runAction: vi.fn(async () => { throw new Error("daemon returned error: invalid-path: cwd must be absolute"); }) });
    const { onClose } = renderDialog(runtime);
    await reachForm();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "my-name" } });
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "claude --model haiku" } });

    fireEvent.click(screen.getByTestId("new-agent-start"));

    expect(await screen.findByTestId("new-agent-error")).toHaveTextContent("invalid-path: cwd must be absolute");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("my-name");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("claude --model haiku");
    expect(screen.getByTestId("new-agent-start")).toBeEnabled();
    expect(runtime.clearError).toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });
});

describe("New agent dialog — a deck that leaves mid-flow (PRD #1223 M4)", () => {
  const GONE = `that deck is not one this app is observing: ${LOCAL}`;

  /**
   * Scenario: the chosen deck leaves the fleet before its home is listed. The
   * refusal takes the flow back to the deck step and says why; no other deck
   * is asked for anything.
   */
  it("returns to the deck step when the listing is refused for a departed deck", async () => {
    const runtime = fakeRuntime({ listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => { throw new Error(GONE); }) });
    renderDialog(runtime);
    fireEvent.keyDown(deckList(), { key: "Enter" });

    expect(await screen.findByTestId("new-agent-deck-notice")).toHaveTextContent("that deck is not one this app is observing");
    expect(screen.getByTestId("new-agent-dialog")).toHaveAttribute("data-step", "deck");
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
  });

  /** Scenario: the chosen deck leaves between the form and the start. The start's refusal takes the flow back to the deck step. */
  it("returns to the deck step when the start is refused for a departed deck", async () => {
    const runtime = fakeRuntime({ runAction: vi.fn(async () => { throw new Error(GONE); }) });
    renderDialog(runtime);
    await reachForm();

    fireEvent.click(screen.getByTestId("new-agent-start"));

    expect(await screen.findByTestId("new-agent-deck-notice")).toHaveTextContent("that deck is not one this app is observing");
    expect(screen.getByTestId("new-agent-dialog")).toHaveAttribute("data-step", "deck");
    expect(runtime.runAction).toHaveBeenCalledTimes(1);
  });
});

describe("New agent dialog — older decks (PRD #1223 M5)", () => {
  /**
   * Scenario: the deck has no listing verb. The directory step offers the
   * typed path alone and says why; the typed path goes to the form verbatim,
   * with no second listing request, and the form names it after its last
   * component.
   */
  it("offers only the typed path on a deck without the listing verb", async () => {
    const runtime = fakeRuntime({ listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => ({ kind: "unsupported" })) });
    renderDialog(runtime);
    fireEvent.keyDown(deckList(), { key: "Enter" });

    expect(await screen.findByTestId("new-agent-no-browse")).toBeVisible();
    expect(screen.queryByTestId("new-agent-directory-list")).toBeNull();
    const path = screen.getByTestId("new-agent-path");
    await waitFor(() => expect(path).toHaveFocus());
    fireEvent.change(path, { target: { value: "/srv/work/repo" } });
    fireEvent.submit(path.closest("form")!);

    expect(await screen.findByTestId("new-agent-dir")).toHaveTextContent("/srv/work/repo");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("repo");
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: the deck has no options query. Command is prefilled from this
   * app's memory of the deck's last command, and the Agent picker offers this
   * app's own registry, saying that is what it is.
   */
  it("falls back to this app's memory and registry on a deck without the options query", async () => {
    const runtime = fakeRuntime({
      newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ kind: "unsupported", desktopAgents: [{ id: "codex", displayName: "Codex", defaultCommand: "codex" }], lastCommand: "codex --model gpt-5.6-sol" })),
    });
    renderDialog(runtime);
    await reachForm();

    expect(await screen.findByTestId("new-agent-desktop-registry")).toHaveTextContent("The list is this app's own.");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("codex --model gpt-5.6-sol");
    expect(within(screen.getByTestId("new-agent-agent")).getAllByRole("option").map((option) => option.textContent)).toEqual(["auto", "Codex"]);
  });
});

describe("New agent dialog — after the start (PRD #1223 M5)", () => {
  /**
   * Scenario: the deck accepts the start and returns agent `7`. The other deck
   * already runs an agent `7`, and the target deck does not list its own yet —
   * so nothing opens and the dialog says it is waiting. When the target deck's
   * fleet entry lists `7`, the pane is asked for exactly once, by the
   * composite identity.
   */
  it("opens the pane only once the target deck lists the agent", async () => {
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE, {}, [createFixtureStartedAgent({ id: "7", daemonId: REMOTE })])] });
    const { onAppeared, rerenderWith } = renderDialog(runtime, { initialDeckId: LOCAL });
    await reachForm();

    fireEvent.click(screen.getByTestId("new-agent-start"));
    expect(await screen.findByTestId("new-agent-waiting")).toBeVisible();
    expect(onAppeared).not.toHaveBeenCalled();

    rerenderWith({ ...runtime, fleet: [deck(LOCAL, { deckKind: "local" }, [createFixtureStartedAgent({ id: "7", daemonId: LOCAL })]), runtime.fleet[1]] });

    await waitFor(() => expect(onAppeared).toHaveBeenCalledTimes(1));
    expect(onAppeared).toHaveBeenCalledWith({ deckId: LOCAL, agentId: "7" });
  });

  /**
   * Scenario: the deck accepts the start and never lists the agent. Once the
   * bound passes, the flow reports the agent as started on that deck and not
   * yet listed — and a listing that arrives afterwards opens nothing.
   */
  it("gives up after the bound and reports the agent as started but not listed", async () => {
    const runtime = fakeRuntime();
    const { onAppeared, onNotAppeared, rerenderWith } = renderDialog(runtime, { appearTimeoutMs: 30 });
    await reachForm();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "worker" } });

    fireEvent.click(screen.getByTestId("new-agent-start"));

    await waitFor(() => expect(onNotAppeared).toHaveBeenCalledTimes(1));
    expect(onNotAppeared).toHaveBeenCalledWith({ deckName: "Local deck", agentName: "worker" });
    await act(async () => {
      rerenderWith({ ...runtime, fleet: [deck(LOCAL, { deckKind: "local" }, [createFixtureStartedAgent({ id: "7", daemonId: LOCAL })]), runtime.fleet[1]] });
    });
    expect(onAppeared).not.toHaveBeenCalled();
  });
});

describe("New agent dialog — authoring agents (PRD #1223 M7)", () => {
  const AUTHORING: NewAgentOptions = { ...DECK_OPTIONS, authoringKinds: ["schedule", "schedule-issues", "dispatcher"] };
  const optionsOf = (patch: Partial<Extract<NewAgentOptions, { kind: "deck" }>> = {}) => vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(AUTHORING), ...patch } as NewAgentOptions));
  const modeLabels = () => within(screen.getByTestId("new-agent-modes")).getAllByRole("button").map((chip) => chip.textContent);

  /**
   * Scenario: reach the form on a deck that lists all three authoring kinds
   * with its experimental flag off. The Mode row offers No mode, schedule and
   * dispatcher — not `schedule: issues`, which the TUI shows only with the
   * flag. With the flag on, it is offered between the other two.
   */
  it.each([
    ["off", false, ["No mode", "schedule", "dispatcher"]],
    ["on", true, ["No mode", "schedule", "schedule: issues", "dispatcher"]],
  ])("offers the authoring chips the deck lists, schedule: issues only with the deck's flag %s", async (_flag, experimental, expected) => {
    renderDialog(fakeRuntime({ newAgentOptions: optionsOf({ experimental }) }));
    await reachForm();

    await waitFor(() => expect(modeLabels()).toEqual(expected));
    expect(screen.getByTestId("new-agent-mode-none")).toHaveAttribute("aria-pressed", "true");
    expect(screen.queryByTestId("new-agent-authoring-withheld")).toBeNull();
  });

  /**
   * Scenario: two decks that cannot compose a seed. One predates the options
   * query altogether, the other answers it but lists no authoring kind. On
   * both the Mode row offers No mode alone, and the form says why the
   * authoring agents are missing.
   */
  it.each([
    ["an older deck", vi.fn(async (): Promise<NewAgentOptions> => ({ kind: "unsupported", desktopAgents: DECK_OPTIONS.kind === "deck" ? DECK_OPTIONS.agents : [] })), "does not report which authoring agents"],
    ["a deck that composes no seed", optionsOf({ authoringKinds: [], experimental: true }), "cannot compose authoring seeds"],
  ])("withholds every authoring chip on %s and says why", async (_case, newAgentOptions, reason) => {
    renderDialog(fakeRuntime({ newAgentOptions }));
    await reachForm();

    expect(await screen.findByTestId("new-agent-authoring-withheld")).toHaveTextContent(reason);
    expect(modeLabels()).toEqual(["No mode"]);
  });

  /**
   * Scenario: on a deck with no configured default command, choose schedule
   * and leave Command blank. The start carries the authoring kind and
   * resolves the blank Command to `claude` — the TUI's fallback — rather than
   * sending none, which would start the deck's default shell. The Command
   * field still shows what the user left there.
   */
  it("resolves a blank Command to claude for an authoring agent", async () => {
    const runtime = fakeRuntime({ newAgentOptions: optionsOf() });
    renderDialog(runtime);
    await reachForm();
    fireEvent.click(await screen.findByTestId("new-agent-mode-schedule"));
    expect(screen.getByTestId("new-agent-mode-schedule")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("new-agent-command")).toHaveAttribute("placeholder", "Empty starts claude");

    fireEvent.click(screen.getByTestId("new-agent-start"));

    await waitFor(() => expect(runtime.runAction).toHaveBeenCalledTimes(1));
    expect(runtime.runAction).toHaveBeenCalledWith({ type: "start_agent", deckId: LOCAL, cwd: "/home/dev/beta/leaf", command: "claude", displayName: "leaf", authoringKind: "schedule" });
    expect(screen.getByTestId("new-agent-command")).toHaveValue("");
  });

  /**
   * Scenario: on a deck whose host configures `default_command`, clear the
   * prefilled Command, move to dispatcher with the Right arrow and start. The
   * blank Command resolves to the configured command, trimmed; a typed
   * command, by contrast, is sent as it stands.
   */
  it("resolves a blank Command to the deck's default command, and sends a typed one as it is", async () => {
    const runtime = fakeRuntime({ newAgentOptions: optionsOf({ defaultCommand: "  opencode --model mini  " }) });
    renderDialog(runtime);
    await reachForm();
    await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("  opencode --model mini  "));
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "" } });
    const none = await screen.findByTestId("new-agent-mode-none");
    fireEvent.keyDown(none, { key: "ArrowRight" });
    fireEvent.keyDown(screen.getByTestId("new-agent-mode-schedule"), { key: "ArrowRight" });
    expect(screen.getByTestId("new-agent-mode-dispatcher")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("new-agent-mode-dispatcher")).toHaveFocus();

    fireEvent.click(screen.getByTestId("new-agent-start"));
    await waitFor(() => expect(runtime.runAction).toHaveBeenCalledTimes(1));
    expect(runtime.runAction).toHaveBeenLastCalledWith(expect.objectContaining({ command: "opencode --model mini", authoringKind: "dispatcher" }));

    // Left wraps back past No mode onto the last chip; a typed command is sent verbatim.
    const typed = fakeRuntime({ newAgentOptions: optionsOf({ experimental: true }) });
    cleanupAndRender(typed);
    await reachForm();
    fireEvent.keyDown(await screen.findByTestId("new-agent-mode-none"), { key: "ArrowLeft" });
    expect(screen.getByTestId("new-agent-mode-dispatcher")).toHaveAttribute("aria-pressed", "true");
    fireEvent.keyDown(screen.getByTestId("new-agent-mode-dispatcher"), { key: "ArrowLeft" });
    expect(screen.getByTestId("new-agent-mode-schedule-issues")).toHaveAttribute("aria-pressed", "true");
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "codex --full-auto" } });
    fireEvent.click(screen.getByTestId("new-agent-start"));
    await waitFor(() => expect(typed.runAction).toHaveBeenCalledTimes(1));
    expect(typed.runAction).toHaveBeenCalledWith(expect.objectContaining({ command: "codex --full-auto", authoringKind: "schedule-issues" }));
  });

  /**
   * Scenario: the deck refuses the authoring start — here as an older deck
   * that cannot compose the seed would. The dialog stays open on the form with
   * the refusal inline, and the Mode, Name and Command are all as they were,
   * so Start can be pressed again.
   */
  it("keeps the dialog open with the refusal inline and every value kept", async () => {
    const runtime = fakeRuntime({
      newAgentOptions: optionsOf(),
      runAction: vi.fn(async () => { throw new Error("This deck cannot start a `schedule` agent: it predates daemon-composed authoring seeds. Nothing was started."); }),
    });
    const { onClose } = renderDialog(runtime);
    await reachForm();
    fireEvent.click(await screen.findByTestId("new-agent-mode-schedule"));
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "nightly" } });
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "claude --model haiku" } });

    fireEvent.click(screen.getByTestId("new-agent-start"));

    expect(await screen.findByTestId("new-agent-error")).toHaveTextContent("cannot start a `schedule` agent");
    expect(screen.getByTestId("new-agent-mode-schedule")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("nightly");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("claude --model haiku");
    expect(screen.getByTestId("new-agent-start")).toBeEnabled();
    expect(onClose).not.toHaveBeenCalled();
  });

  /**
   * Scenario: an authoring start the deck accepts. The pane opens only once
   * the target deck's fleet entry lists the new agent — the same wait a plain
   * agent gets.
   */
  it("opens the authoring agent's pane once the deck lists it", async () => {
    const runtime = fakeRuntime({ newAgentOptions: optionsOf() });
    const { onAppeared, rerenderWith } = renderDialog(runtime);
    await reachForm();
    fireEvent.click(await screen.findByTestId("new-agent-mode-dispatcher"));
    fireEvent.click(screen.getByTestId("new-agent-start"));
    expect(await screen.findByTestId("new-agent-waiting")).toBeVisible();

    rerenderWith({ ...runtime, fleet: [deck(LOCAL, { deckKind: "local" }, [createFixtureStartedAgent({ id: "7", daemonId: LOCAL })]), runtime.fleet[1]] });

    await waitFor(() => expect(onAppeared).toHaveBeenCalledWith({ deckId: LOCAL, agentId: "7" }));
  });

  /**
   * Scenario: choose schedule, go back to the directory step and confirm a
   * directory again. The form it opens starts on No mode, as every fresh TUI
   * form does.
   */
  it("opens every confirmed directory's form on No mode", async () => {
    renderDialog(fakeRuntime({ newAgentOptions: optionsOf() }));
    await reachForm();
    fireEvent.click(await screen.findByTestId("new-agent-mode-schedule"));

    fireEvent.click(screen.getByRole("button", { name: /Back/ }));
    fireEvent.click(await screen.findByTestId("new-agent-use-directory"));

    await waitFor(() => expect(screen.getByTestId("new-agent-mode-none")).toHaveAttribute("aria-pressed", "true"));
    expect(screen.getByTestId("new-agent-mode-schedule")).toHaveAttribute("aria-pressed", "false");
  });
});

describe("New agent dialog — orchestrations (PRD #1223 M6)", () => {
  const PROJECT = "/home/dev/Alpha-project";
  const PROJECT_ORCHESTRATIONS: NewAgentOrchestrations = {
    kind: "project",
    path: PROJECT,
    displayPath: PROJECT,
    displayName: "Alpha-project",
    configRevision: "rev-1",
    orchestrations: [
      {
        name: "loop",
        displayName: "loop",
        default: true,
        roles: [
          { name: "planner", displayName: "planner", start: true },
          { name: "builder", displayName: "builder", start: false },
        ],
      },
    ],
  };
  const orchestrationsOf = (answer: NewAgentOrchestrations = PROJECT_ORCHESTRATIONS) =>
    vi.fn(async (_deckId: string, _path: string): Promise<NewAgentOrchestrations> => structuredClone(answer));

  /** A live orchestration role on a deck, carrying `title` (or none) and running in `cwd`. */
  function orchestrationRole(id: string, daemonId: string, title: string | undefined, name = "loop", cwd?: string): AgentSession {
    return {
      ...createFixtureStartedAgent({ id, daemonId }),
      tab: { kind: "orchestration", name, displayTitle: title, roleName: "planner", roleIndex: 0, isStartRole: true, cwd, orchestrationId: `run-${id}` },
      inOrchestration: true,
      isStartRole: true,
    };
  }

  /** Confirm the only eligible deck, enter the project directory — the first entry — and use it. */
  async function reachProjectForm() {
    fireEvent.keyDown(deckList(), { key: "Enter" });
    await currentPath("/home/dev");
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await currentPath(PROJECT);
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await screen.findByTestId("new-agent-form");
  }

  const chip = () => screen.findByTestId("new-agent-mode-orch:loop");

  /**
   * Scenario: browse into a directory the listing marks as a project and use
   * it. The CHOSEN deck is asked for that directory's orchestrations, and the
   * Mode row offers `Orch: loop` right after No mode, as the TUI's cycler
   * does.
   */
  it("offers one chip per orchestration of a project directory, asked of the chosen deck", async () => {
    const runtime = fakeRuntime({ newAgentOrchestrations: orchestrationsOf() });
    renderDialog(runtime);
    await reachProjectForm();

    expect(await chip()).toHaveTextContent("Orch: loop");
    expect(runtime.newAgentOrchestrations).toHaveBeenCalledWith(LOCAL, PROJECT);
    const modes = within(screen.getByTestId("new-agent-modes")).getAllByRole("button").map((button) => button.getAttribute("data-mode"));
    expect(modes.slice(0, 2)).toEqual(["none", "orch:loop"]);
  });

  /**
   * Scenario: use a directory the listing marked as NOT a project. The deck is
   * not asked for orchestrations at all, and no orchestration chip appears.
   */
  it("asks nothing for a directory the listing marked as no project", async () => {
    const runtime = fakeRuntime({ newAgentOrchestrations: orchestrationsOf() });
    renderDialog(runtime);
    await reachForm();

    expect(runtime.newAgentOrchestrations).not.toHaveBeenCalled();
    expect(screen.queryByTestId("new-agent-mode-orch:loop")).toBeNull();
  });

  /**
   * Scenario: the chosen deck cannot launch an orchestration from this flow —
   * it lacks the project verbs or cannot start a role with its configured
   * command. No chip is offered and the deck's own reason is shown instead.
   */
  it("withholds the chips with the deck's reason", async () => {
    const reason = "This deck cannot start orchestration roles with their configured commands, so its orchestrations are not offered here.";
    renderDialog(fakeRuntime({ newAgentOrchestrations: orchestrationsOf({ kind: "unsupported", reason }) }));
    await reachProjectForm();

    expect(await screen.findByTestId("new-agent-orchestrations-withheld")).toHaveTextContent(reason);
    expect(screen.queryByTestId("new-agent-mode-orch:loop")).toBeNull();
  });

  /**
   * Scenario: the chosen deck already runs `Alpha-project-orchestrator-1`, and
   * ANOTHER deck runs `Alpha-project-orchestrator-2`. Selecting the chip with
   * an untouched Name prefills `Alpha-project-orchestrator-2` — the other
   * deck's titles do not count — and hides Command; No mode restores the
   * basename and Command. Once the Name is typed in, selecting the chip no
   * longer replaces it.
   */
  it("prefills the next free name against the chosen deck's live titles and hides Command", async () => {
    const runtime = fakeRuntime({
      newAgentOrchestrations: orchestrationsOf(),
      fleet: [
        deck(LOCAL, { deckKind: "local" }, [orchestrationRole("3", LOCAL, "Alpha-project-orchestrator-1")]),
        deck(REMOTE, { status: "disconnected" }, [orchestrationRole("4", REMOTE, "Alpha-project-orchestrator-2")]),
      ],
    });
    renderDialog(runtime);
    await reachProjectForm();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("Alpha-project");

    fireEvent.click(await chip());
    expect(screen.getByTestId("new-agent-name")).toHaveValue("Alpha-project-orchestrator-2");
    expect(screen.queryByTestId("new-agent-command")).toBeNull();
    expect(screen.getByTestId("new-agent-start")).toHaveTextContent("Start orchestration");

    fireEvent.click(screen.getByTestId("new-agent-mode-none"));
    expect(screen.getByTestId("new-agent-name")).toHaveValue("Alpha-project");
    expect(screen.getByTestId("new-agent-command")).toBeVisible();

    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "my-run" } });
    fireEvent.click(await chip());
    expect(screen.getByTestId("new-agent-name")).toHaveValue("my-run");
  });

  /**
   * Scenario: the chosen deck runs an orchestration titled `taken-run`, and
   * another whose title is its bare name `loop`. Typing `taken-run` refuses
   * the start inline with Start disabled; an empty Name — whose run would
   * take the orchestration's own name, `loop` — is refused the same way; any
   * other Name clears the refusal. Nothing is sent while it stands.
   */
  it("refuses a Name that is a live orchestration's title on that deck", async () => {
    const runtime = fakeRuntime({
      newAgentOrchestrations: orchestrationsOf(),
      fleet: [
        deck(LOCAL, { deckKind: "local" }, [orchestrationRole("3", LOCAL, "taken-run"), orchestrationRole("5", LOCAL, undefined, "loop")]),
        deck(REMOTE, { status: "disconnected" }),
      ],
    });
    renderDialog(runtime);
    await reachProjectForm();
    fireEvent.click(await chip());

    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "taken-run" } });
    expect(screen.getByTestId("new-agent-title-taken")).toHaveTextContent("already in use by a live orchestration");
    expect(screen.getByTestId("new-agent-start")).toBeDisabled();
    fireEvent.submit(screen.getByTestId("new-agent-form"));

    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "" } });
    expect(screen.getByTestId("new-agent-title-taken")).toBeVisible();

    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "fresh-run" } });
    expect(screen.queryByTestId("new-agent-title-taken")).toBeNull();
    expect(screen.getByTestId("new-agent-start")).toBeEnabled();
    expect(runtime.runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: launch the orchestration. One `start_orchestration` goes to the
   * captured deck with the deck's own project path and orchestration name,
   * the Name as the run's title, the resolved config revision, and no command
   * or task. Once that deck lists the START role the reply named, its pane is
   * opened.
   */
  it("launches on the captured deck and opens the start role's pane once it is listed", async () => {
    const runtime = fakeRuntime({ newAgentOrchestrations: orchestrationsOf(), runAction: vi.fn(async () => ({ ok: true, agentId: "12" })) });
    const { onAppeared, rerenderWith } = renderDialog(runtime);
    await reachProjectForm();
    fireEvent.click(await chip());

    fireEvent.click(screen.getByTestId("new-agent-start"));

    expect(await screen.findByTestId("new-agent-waiting")).toBeVisible();
    expect(runtime.runAction).toHaveBeenCalledWith({
      type: "start_orchestration",
      deckId: LOCAL,
      path: PROJECT,
      orchestration: "loop",
      displayTitle: "Alpha-project-orchestrator-1",
      configRevision: "rev-1",
    });
    expect(onAppeared).not.toHaveBeenCalled();

    rerenderWith({ ...runtime, fleet: [deck(LOCAL, { deckKind: "local" }, [orchestrationRole("12", LOCAL, "Alpha-project-orchestrator-1")]), runtime.fleet[1]] });

    await waitFor(() => expect(onAppeared).toHaveBeenCalledWith({ deckId: LOCAL, agentId: "12" }));
  });

  /**
   * Scenario: a later role is refused mid-launch. The deck's refusal — which
   * names the role that failed and the roles that had started — is shown
   * inline, the dialog stays open, and the chip and the Name are kept.
   */
  it("keeps the dialog open with a partial failure inline", async () => {
    const refusal = "failed to start orchestration role builder: start-prepared-agent failed; roles already started: planner; stopped 1 already-started role(s)";
    const runtime = fakeRuntime({ newAgentOrchestrations: orchestrationsOf(), runAction: vi.fn(async () => { throw new Error(refusal); }) });
    const { onClose } = renderDialog(runtime);
    await reachProjectForm();
    fireEvent.click(await chip());
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "nightly-run" } });

    fireEvent.click(screen.getByTestId("new-agent-start"));

    expect(await screen.findByTestId("new-agent-error")).toHaveTextContent("roles already started: planner");
    expect(screen.getByTestId("new-agent-mode-orch:loop")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("nightly-run");
    expect(screen.getByTestId("new-agent-start")).toBeEnabled();
    expect(onClose).not.toHaveBeenCalled();
    expect(runtime.clearError).toHaveBeenCalled();
  });
});

/** Unmount whatever is rendered and render the dialog afresh over `runtime`. */
function cleanupAndRender(runtime: FakeRuntime) {
  cleanup();
  return renderDialog(runtime);
}
