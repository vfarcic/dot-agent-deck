import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, createFixtureStartedAgent } from "../data/fixture";
import type { AgentSession, ConnectionView, DeckDirectoryListing, DeckSnapshot, NewAgentOptions, NewAgentOrchestrations } from "../types";
import { LaunchCleanupError } from "../lib/actionError";
import type { NewAgentDraft } from "../lib/newAgentDraft";
import { NewAgentDialog, DRAFT_DIRECTORY_GONE, DRAFT_MODE_GONE, DRAFT_RESTORED, draftDeckGone, draftOtherDeck, type NewAgentRuntime } from "./NewAgentDialog";

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

function renderDialog(runtime: FakeRuntime, props: { initialDeckId?: string; appearTimeoutMs?: number; draft?: NewAgentDraft } = {}) {
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

/**
 * The only eligible deck is chosen on open, so its home is listed without a
 * key; browse into a directory with no subdirectories and use it.
 */
async function reachForm() {
  await currentPath("/home/dev");
  fireEvent.keyDown(directoryList(), { key: "j" });
  fireEvent.keyDown(directoryList(), { key: "Enter" });
  await currentPath("/home/dev/beta");
  fireEvent.keyDown(directoryList(), { key: "Enter" });
  await currentPath("/home/dev/beta/leaf");
  fireEvent.keyDown(directoryList(), { key: "Enter" });
  await waitFor(() => expect(screen.getByTestId("new-agent-name")).toBeEnabled());
}

describe("New agent dialog — one surface (PRD #1223, the voice-first redesign)", () => {
  /**
   * Scenario: open the dialog over a fleet with one eligible deck. Every
   * control is on screen at once — the daemon field, the directory browser, and
   * Mode, Name, Command and Start — with no Next, no Back and no step to
   * pass. The deck is chosen already and its home is listed without a key;
   * the form's fields wait, disabled, until a directory is chosen.
   */
  it("mounts every control at once, and the form waits for a directory", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);

    expect(deckList()).toBeVisible();
    expect(screen.getByTestId("new-agent-directory-panel")).toBeVisible();
    expect(screen.getByTestId("new-agent-form")).toBeVisible();
    for (const id of ["new-agent-mode-none", "new-agent-name", "new-agent-command", "new-agent-start"]) {
      expect(screen.getByTestId(id)).toBeDisabled();
    }
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    const flow = screen.getByTestId("new-agent-dialog");
    expect(flow).not.toHaveAttribute("data-step");
    expect(within(flow).queryByRole("button", { name: /^(next|back)$/i })).toBeNull();
    expect(screen.queryByTestId("new-agent-deck-next")).toBeNull();

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
    expect(runtime.newAgentOptions).toHaveBeenCalledWith(LOCAL);

    fireEvent.keyDown(directoryList(), { key: " " });
    for (const id of ["new-agent-mode-none", "new-agent-name", "new-agent-command", "new-agent-start"]) {
      expect(screen.getByTestId(id)).toBeEnabled();
    }
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev");
  });

  /**
   * Scenario: open the dialog over two eligible decks. Nothing is chosen for
   * the user, nothing is listed, the directory panel says to choose a daemon
   * first, and focus is on the daemon field — the first control left unsatisfied.
   */
  it("focuses the daemon field and lists nothing while no deck is chosen", async () => {
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE)] });
    renderDialog(runtime);

    await waitFor(() => expect(deckList()).toHaveFocus());
    expect(screen.getByTestId("new-agent-directory-idle")).toHaveTextContent("Choose a daemon");
    expect(screen.queryByTestId("new-agent-directory-list")).toBeNull();
    expect(runtime.listDirectories).not.toHaveBeenCalled();
    expect(runtime.newAgentOptions).not.toHaveBeenCalled();
  });

  /**
   * Scenario: the focus order, as the wizard's steps had it. With the daemon
   * preselected, focus opens on the directory browser; confirming a directory
   * moves it to Name. The tab stops run deck → directory → Mode → Name →
   * Command → Start in document order — no Agent picker (PRD #1223 removed it
   * from both clients).
   */
  it("moves focus deck → browser → form, and tabs through them in that order", async () => {
    renderDialog(fakeRuntime());
    await currentPath("/home/dev");
    await waitFor(() => expect(directoryList()).toHaveFocus());

    fireEvent.keyDown(directoryList(), { key: " " });
    await waitFor(() => expect(screen.getByTestId("new-agent-name")).toHaveFocus());

    const flow = screen.getByTestId("new-agent-dialog");
    const stops = Array.from(flow.querySelectorAll<HTMLElement>("button, input, select, [tabindex='0']"))
      .filter((element) => !element.hasAttribute("disabled") && element.getAttribute("aria-label") !== "Close new agent")
      .map((element) => element.getAttribute("data-testid"));
    expect(stops).toEqual([
      "new-agent-deck-list",
      "new-agent-filter",
      "new-agent-directory-list",
      "new-agent-use-directory",
      "new-agent-mode-none",
      "new-agent-name",
      "new-agent-command",
      "new-agent-discard",
      "new-agent-start",
    ]);
  });

  /**
   * Scenario: choose the local deck, use its home, type a Name — then choose
   * the other deck. What hung off the first deck goes: the chosen directory,
   * its listing and its options, and the new deck is asked for its own. The
   * typed Name is kept, because it was the user's.
   */
  it("re-derives the directory and the options from a newly chosen deck, keeping a typed Name", async () => {
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE)] });
    renderDialog(runtime, { initialDeckId: LOCAL });
    await currentPath("/home/dev");
    fireEvent.keyDown(directoryList(), { key: " " });
    await waitFor(() => expect(screen.getByTestId("new-agent-name")).toBeEnabled());
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "mine" } });

    fireEvent.click(deckList().querySelector(`[data-deck-id="${REMOTE}"]`)!);

    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    expect(screen.getByTestId("new-agent-name")).toBeDisabled();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenLastCalledWith(REMOTE, undefined);
    expect(runtime.newAgentOptions).toHaveBeenLastCalledWith(REMOTE);
    expect(screen.getByTestId("new-agent-chosen-deck")).toHaveTextContent(`dev@${REMOTE}`);
    expect(deckList().querySelector("[data-chosen='true']")).toHaveAttribute("data-deck-id", REMOTE);
  });

  /**
   * Scenario: with the daemon preselected and listed, press Enter on the daemon
   * field — the wizard's one keystroke. The deck is already chosen, so nothing
   * is asked again: focus just moves on to the browser.
   */
  it("treats Enter on the already chosen deck as moving on, not as a new choice", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    await currentPath("/home/dev");
    deckList().focus();

    fireEvent.keyDown(deckList(), { key: "Enter" });

    await waitFor(() => expect(directoryList()).toHaveFocus());
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
    expect(runtime.newAgentOptions).toHaveBeenCalledTimes(1);
  });
});

describe("New agent dialog — closing (PRD #1223 U2)", () => {
  /**
   * Scenario: the dialog has no Cancel button — the header's X is the one
   * close control, as on every other sheet here — and the X, Esc and a
   * backdrop click each close it.
   */
  it("has no Cancel and closes by the X, Esc and the backdrop", async () => {
    const runtime = fakeRuntime();
    const { onClose } = renderDialog(runtime);
    expect(within(screen.getByTestId("new-agent-dialog")).queryByRole("button", { name: /cancel/i })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Close new agent" }));
    fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" });
    fireEvent.mouseDown(screen.getByTestId("new-agent-backdrop"));
    expect(onClose).toHaveBeenCalledTimes(3);
  });

  /**
   * Scenario: `q` closes the dialog only from the directory browser, as the
   * TUI picker's does. Typed into Name, Command or the filter it is a letter,
   * and nothing closes.
   */
  it("closes on q only while focus is in the directory browser", async () => {
    const runtime = fakeRuntime();
    const { onClose } = renderDialog(runtime);
    await reachForm();

    for (const id of ["new-agent-name", "new-agent-command", "new-agent-filter"]) {
      fireEvent.keyDown(screen.getByTestId(id), { key: "q" });
    }
    expect(onClose).not.toHaveBeenCalled();

    fireEvent.keyDown(directoryList(), { key: "q" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

describe("New agent dialog — deck field (PRD #1223 M4)", () => {
  /**
   * Scenario: open the flow over a fleet of four decks — one connected, one
   * disconnected with its own message, one still waiting to report and one
   * incompatible. Every deck is listed; the three that cannot take a spawn are
   * disabled and each says why, and clicking one of them lists nothing.
   */
  it("lists every deck and disables the ones that cannot take a spawn, with the reason", async () => {
    const runtime = fakeRuntime({
      fleet: [
        deck(LOCAL, { deckKind: "local" }),
        deck(REMOTE, { status: "disconnected", message: "ssh: connect to host build-box port 22: Connection refused" }),
        deck("deck-000000000000cccc", { status: "loading", pending: true }),
        deck("deck-000000000000dddd", { status: "error", message: "Protocol handshake failed." }),
      ],
    });
    renderDialog(runtime);
    await currentPath("/home/dev");

    const options = within(deckList()).getAllByRole("option");
    expect(options).toHaveLength(4);
    expect(options[0]).not.toHaveAttribute("aria-disabled");
    expect(options[1]).toHaveAttribute("aria-disabled", "true");
    expect(options[1]).toHaveTextContent("Connection refused");
    expect(options[2]).toHaveTextContent("This daemon has not reported yet.");
    expect(options[3]).toHaveTextContent("Protocol handshake failed.");
    fireEvent.click(options[1]);
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
    expect(deckList().querySelector("[data-chosen='true']")).toHaveAttribute("data-deck-id", LOCAL);
  });

  /**
   * Scenario: with exactly one deck able to take a spawn, the field opens with
   * it chosen, and THAT deck is asked for its home directory — no key pressed.
   */
  it("chooses the only eligible deck on open and lists its home", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);

    expect(within(deckList()).getAllByRole("option")[0]).toHaveAttribute("aria-selected", "true");
    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
  });

  /**
   * Scenario: two decks can take a spawn and the flow was opened from the
   * remote one's header. That deck is chosen and its home listed.
   */
  it("chooses the daemon the flow was opened from", async () => {
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE)] });
    renderDialog(runtime, { initialDeckId: REMOTE });

    expect(deckList().querySelector("[aria-selected='true']")).toHaveAttribute("data-deck-id", REMOTE);
    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(REMOTE, undefined);
    expect(screen.getByTestId("new-agent-chosen-deck")).toHaveTextContent(`dev@${REMOTE}`);
  });

  /**
   * Scenario: two decks can take a spawn and a disconnected one sits between
   * them. Nothing is preselected; `j` moves to the first eligible deck and
   * again past the disconnected one to the second, and Enter chooses the daemon
   * the cursor is on — moving the cursor alone chooses nothing.
   */
  it("preselects nothing between two eligible decks and moves over disabled ones", async () => {
    const third = "deck-000000000000eeee";
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE, { status: "disconnected" }), deck(third)] });
    renderDialog(runtime);

    expect(deckList().querySelector("[aria-selected='true']")).toBeNull();
    fireEvent.keyDown(deckList(), { key: "j" });
    expect(deckList().querySelector("[aria-selected='true']")).toHaveAttribute("data-deck-id", LOCAL);
    fireEvent.keyDown(deckList(), { key: "ArrowDown" });
    expect(deckList().querySelector("[aria-selected='true']")).toHaveAttribute("data-deck-id", third);
    expect(runtime.listDirectories).not.toHaveBeenCalled();
    fireEvent.keyDown(deckList(), { key: "Enter" });

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(third, undefined);
    await waitFor(() => expect(directoryList()).toHaveFocus());
  });
});

describe("New agent dialog — directory browser (PRD #1223 M4)", () => {
  /**
   * Scenario: browse with the TUI picker's keys. The home listing opens with
   * the cursor on its first subdirectory, the project marked; `j` moves to
   * `beta` and Enter lists it by the path the daemon gave. Left goes up by the
   * reply's `parent` and lands the cursor back on `beta`; Backspace goes up
   * again by home's `parent` — a path no trimming of home produces. Space then
   * uses that directory, and the form names it after its last component.
   */
  it("moves, enters, goes up through the reply's parent, and confirms with Space", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
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
   * That directory is confirmed — the form opens on it and asks the daemon for
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

  /** Scenario: a listing the daemon cut short says so. */
  it("shows that a truncated listing is incomplete", async () => {
    const runtime = fakeRuntime({
      listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => ({ ...structuredClone(TREE[""]), truncated: true })),
    });
    renderDialog(runtime);

    expect(await screen.findByTestId("new-agent-truncated")).toBeVisible();
  });

  /**
   * Scenario (PRD #1223 U1): the directory browser has no typed-path field, and a
   * truncated listing's hint does not tell the user to type one.
   */
  it("offers no typed path, and a truncated listing does not suggest one", async () => {
    const runtime = fakeRuntime({
      listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => ({ ...structuredClone(TREE[""]), truncated: true })),
    });
    renderDialog(runtime);

    const hint = await screen.findByTestId("new-agent-truncated");
    expect(hint.textContent ?? "").not.toMatch(/type/i);
    expect(screen.queryByTestId("new-agent-path")).toBeNull();
    expect(screen.queryByRole("textbox", { name: "Path" })).toBeNull();
  });
});

describe("New agent dialog — going up (PRD #1223 U3)", () => {
  /**
   * Scenario: browse into `beta`. The browser offers Use this directory and
   * no Up button; the `..` row is what goes up, and clicking it lists the
   * reply's parent with the cursor back on `beta`. The footer carries Discard
   * (#1247) and Start and nothing else — no Back, since there is no step to go
   * back to.
   */
  it("goes up by the .. row, with no Up button anywhere", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    await currentPath("/home/dev");
    fireEvent.keyDown(directoryList(), { key: "j" });
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await currentPath("/home/dev/beta");

    const flow = screen.getByTestId("new-agent-dialog");
    expect(within(flow).queryByRole("button", { name: /^up$/i })).toBeNull();
    expect(within(flow.querySelector("footer")!).getAllByRole("button").map((button) => button.textContent?.trim())).toEqual(["Discard", "Create agent"]);
    expect(within(screen.getByTestId("new-agent-directory-panel")).getAllByRole("button").map((button) => button.textContent?.trim())).toEqual(["Use this directory"]);
    const up = within(directoryList()).getAllByRole("option")[0];
    expect(up).toHaveTextContent("..");
    fireEvent.click(up);
    await currentPath("/home/dev");
    expect(activeRow()).toBe("/home/dev/beta");
  });
});

describe("New agent dialog — the daemon's default directory (PRD #1223)", () => {
  /**
   * Scenario: the daemon's config names `default_dir = "/home/dev/beta"`. The
   * browser opens THERE — not in home, and without listing home first — and
   * its `..` row still walks above it, back to home and beyond.
   */
  it("opens the browser in the daemon's default directory, and .. still walks above it", async () => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(DECK_OPTIONS), defaultDir: "/home/dev/beta" })) });
    renderDialog(runtime);

    await currentPath("/home/dev/beta");
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, "/home/dev/beta");
    expect(activeRow()).toBe("/home/dev/beta/leaf");

    const up = within(directoryList()).getAllByRole("option")[0];
    expect(up).toHaveTextContent("..");
    fireEvent.click(up);
    await currentPath("/home/dev");
    expect(activeRow()).toBe("/home/dev/beta");
  });

  /**
   * Scenario: no default directory is configured — or the daemon is older than
   * the setting and never names one. The browser opens in the daemon user's
   * home, as it always has.
   */
  it.each([
    ["no default directory is configured", DECK_OPTIONS],
    ["the daemon is older than the options query", { kind: "unsupported", desktopAgents: [] } as NewAgentOptions],
  ])("opens in home when %s", async (_case, options) => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => structuredClone(options)) });
    renderDialog(runtime);

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
  });

  /**
   * Scenario: the daemon named a default directory that is gone by the time it
   * is listed. The browser falls back to home rather than opening on an error.
   */
  it("falls back to home when the default directory no longer lists", async () => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(DECK_OPTIONS), defaultDir: "/home/dev/vanished" })) });
    renderDialog(runtime);

    await currentPath("/home/dev");
    expect(vi.mocked(runtime.listDirectories).mock.calls.map(([, path]) => path)).toEqual(["/home/dev/vanished", undefined]);
    expect(screen.queryByTestId("new-agent-directory-error")).toBeNull();
  });

  /**
   * Scenario: the options query itself fails (the daemon's query pool is busy).
   * The browser still opens, in home, so it never waits on a setting.
   */
  it("still lists home when the options query fails", async () => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => { throw new Error("daemon returned error: busy: too many new-agent queries"); }) });
    renderDialog(runtime);

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
  });
});

describe("New agent dialog — form (PRD #1223 M4)", () => {
  /**
   * Scenario: reach the form against three decks' options. Command is the
   * deck's configured default command when it has one — ahead of the last
   * command — then the last command started on that daemon, then blank.
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
   * Scenario: reach the form and look for an Agent picker. There is none — no
   * select, no "Agent" field, no `auto` — because it saved one word of typing
   * (every default command is the agent's bare binary name), its `auto` meant
   * nothing and its label went stale against an edited Command (PRD #1223).
   * What the agent runs is the Command field, typed as the user wants it.
   */
  it("has no Agent picker: Command is what the agent runs", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    await reachForm();

    expect(screen.queryByTestId("new-agent-agent")).toBeNull();
    const form = screen.getByTestId("new-agent-form");
    expect(form.querySelector("select")).toBeNull();
    expect(within(form).queryByText(/^Agent$/)).toBeNull();
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "pi --thinking" } });
    expect(screen.getByTestId("new-agent-command")).toHaveValue("pi --thinking");
  });

  /**
   * Scenario: submit the form. The start names the daemon captured at the daemon
   * step, the directory the daemon returned, and the Name and Command as they
   * stand; a blank Command sends no command at all, which starts the daemon's
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

  /**
   * Scenario: the Name is typed with surrounding spaces, then blanked. The
   * start sends it trimmed, as the TUI's `resolve_display_name` trims a plain
   * agent's Name, and a whitespace-only Name sends no name at all.
   */
  it("trims the Name of a plain agent, as the TUI does, and sends none when it is blank", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime);
    await reachForm();
    await waitFor(() => expect(runtime.newAgentOptions).toHaveBeenCalled());

    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "  reviewer  " } });
    fireEvent.submit(screen.getByTestId("new-agent-form"));
    await waitFor(() => expect(runtime.runAction).toHaveBeenCalledTimes(1));
    expect(runtime.runAction).toHaveBeenLastCalledWith(expect.objectContaining({ type: "start_agent", displayName: "reviewer" }));

    const blank = fakeRuntime();
    cleanupAndRender(blank);
    await reachForm();
    await waitFor(() => expect(blank.newAgentOptions).toHaveBeenCalled());
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "   " } });
    fireEvent.submit(screen.getByTestId("new-agent-form"));
    await waitFor(() => expect(blank.runAction).toHaveBeenCalledTimes(1));
    expect(blank.runAction).toHaveBeenCalledWith({ type: "start_agent", deckId: LOCAL, cwd: "/home/dev/beta/leaf" });
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
   * Scenario: the daemon refuses the start. The dialog stays open on the form
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

describe("New agent dialog — a daemon that leaves mid-flow (PRD #1223 M4)", () => {
  const GONE = `that daemon is not one this app is observing: ${LOCAL}`;

  /**
   * Scenario: the chosen deck leaves the fleet before its home is listed. The
   * refusal is shown at the daemon field, focus goes back there, nothing is
   * chosen in its place, and no other deck is asked for anything.
   */
  it("clears the flow back to the daemon field when the listing is refused for a departed deck", async () => {
    const runtime = fakeRuntime({ listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => { throw new Error(GONE); }) });
    renderDialog(runtime);

    expect(await screen.findByTestId("new-agent-deck-notice")).toHaveTextContent("that daemon is not one this app is observing");
    await waitFor(() => expect(deckList()).toHaveFocus());
    expect(deckList().querySelector("[data-chosen='true']")).toBeNull();
    expect(screen.queryByTestId("new-agent-chosen-deck")).toBeNull();
    expect(screen.getByTestId("new-agent-directory-idle")).toBeVisible();
    expect(runtime.listDirectories).toHaveBeenCalledTimes(1);
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
  });

  /**
   * Scenario: the chosen deck leaves between choosing a directory and the
   * start. The start's refusal clears the directory panel and the form — no
   * listing, no chosen directory, a blank Name — refocuses the daemon field and
   * shows the refusal there. It is the wizard's return to its deck step, with
   * no navigation.
   */
  it("clears the directory panel and the form when the start is refused for a departed deck", async () => {
    const runtime = fakeRuntime({ runAction: vi.fn(async () => { throw new Error(GONE); }) });
    renderDialog(runtime);
    await reachForm();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "typed" } });

    fireEvent.click(screen.getByTestId("new-agent-start"));

    expect(await screen.findByTestId("new-agent-deck-notice")).toHaveTextContent("that daemon is not one this app is observing");
    await waitFor(() => expect(deckList()).toHaveFocus());
    expect(screen.queryByTestId("new-agent-directory-list")).toBeNull();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("");
    expect(screen.getByTestId("new-agent-name")).toBeDisabled();
    expect(screen.queryByTestId("new-agent-error")).toBeNull();
    expect(runtime.runAction).toHaveBeenCalledTimes(1);

    // Choosing the daemon again starts over on it.
    fireEvent.keyDown(deckList(), { key: "Enter" });
    await currentPath("/home/dev");
    expect(screen.queryByTestId("new-agent-deck-notice")).toBeNull();
  });
});

describe("New agent dialog — older decks (PRD #1223 M5)", () => {
  /**
   * Scenario (PRD #1223 U1): a connected deck that does not advertise the
   * listing verb carries the crate's `newAgentReason`. It is listed disabled
   * with that reason, is not chosen even when the flow was opened from it —
   * the one eligible deck is — and neither a click nor the keys choose it.
   */
  it("disables a daemon without the listing verb in the daemon field, with the crate's reason", async () => {
    const reason = "This deck does not advertise list-directories, so it cannot be browsed for a directory to start in. Create agents on it from the TUI on its host, or upgrade the daemon.";
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE, { newAgentReason: reason })] });
    renderDialog(runtime, { initialDeckId: REMOTE });

    const options = within(deckList()).getAllByRole("option");
    expect(options[1]).toHaveAttribute("aria-disabled", "true");
    expect(options[1]).toHaveTextContent(reason);
    expect(options[1]).toHaveAttribute("aria-selected", "false");
    fireEvent.click(options[1]);
    fireEvent.keyDown(deckList(), { key: "j" });
    fireEvent.keyDown(deckList(), { key: "Enter" });
    // The local deck, the only eligible one, is what the field chose on open.
    // Awaited: the first listing follows the options answer (PRD #1223's
    // `defaultDir`), so it lands a microtask after the choice.
    await waitFor(() => expect(vi.mocked(runtime.listDirectories).mock.calls.map(([deckId]) => deckId)).toEqual([LOCAL]));
    expect(deckList().querySelector("[data-chosen='true']")).toHaveAttribute("data-deck-id", LOCAL);
  });

  /**
   * Scenario: a daemon that answers the listing `unsupported` anyway — replaced
   * by an older build between its handshake and the request — says it cannot be
   * browsed, offers no way to type a path, and puts focus back on the daemon
   * field, since another deck is the one thing left to do. There is no Back.
   */
  it("points back at the daemon field when a daemon answers the listing unsupported", async () => {
    const runtime = fakeRuntime({ listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => ({ kind: "unsupported" })) });
    renderDialog(runtime);

    expect(await screen.findByTestId("new-agent-no-browse")).toHaveTextContent("Choose another daemon");
    expect(screen.queryByTestId("new-agent-directory-list")).toBeNull();
    expect(screen.queryByTestId("new-agent-path")).toBeNull();
    expect(screen.queryByTestId("new-agent-directory-back")).toBeNull();
    await waitFor(() => expect(deckList()).toHaveFocus());
    expect(screen.getByTestId("new-agent-start")).toBeDisabled();
    expect(runtime.runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: the daemon has no options query. Command is prefilled from this
   * app's memory of the daemon's last command. (The fallback registry is still
   * this app's own, for voice's "use codex"; there is no picker to label.)
   */
  it("falls back to this app's memory and registry on a daemon without the options query", async () => {
    const runtime = fakeRuntime({
      newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ kind: "unsupported", desktopAgents: [{ id: "codex", displayName: "Codex", defaultCommand: "codex" }], lastCommand: "codex --model gpt-5.6-sol" })),
    });
    renderDialog(runtime);
    await reachForm();

    await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("codex --model gpt-5.6-sol"));
    expect(screen.queryByTestId("new-agent-desktop-registry")).toBeNull();
    expect(screen.queryByText(/this app's own/)).toBeNull();
  });
});

describe("New agent dialog — after the start (PRD #1223 M5)", () => {
  /**
   * Scenario: the daemon accepts the start and returns agent `7`. The other deck
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
   * Scenario: the daemon accepts the start and never lists the agent. Once the
   * bound passes, the flow reports the agent as started on that daemon and not
   * yet listed — and a listing that arrives afterwards opens nothing.
   */
  it("gives up after the bound and reports the agent as started but not listed", async () => {
    const runtime = fakeRuntime();
    const { onAppeared, onNotAppeared, rerenderWith } = renderDialog(runtime, { appearTimeoutMs: 30 });
    await reachForm();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "worker" } });

    fireEvent.click(screen.getByTestId("new-agent-start"));

    await waitFor(() => expect(onNotAppeared).toHaveBeenCalledTimes(1));
    expect(onNotAppeared).toHaveBeenCalledWith({ deckName: "Local daemon", agentName: "worker" });
    await act(async () => {
      rerenderWith({ ...runtime, fleet: [deck(LOCAL, { deckKind: "local" }, [createFixtureStartedAgent({ id: "7", daemonId: LOCAL })]), runtime.fleet[1]] });
    });
    expect(onAppeared).not.toHaveBeenCalled();
  });
});

describe("New agent dialog — a start in flight (PRD #1223 audit F5)", () => {
  /** A `runAction` whose answer the test releases by hand. */
  function heldStart() {
    let settle: { resolve: (value: { ok: boolean; agentId?: string }) => void; reject: (cause: unknown) => void } | undefined;
    const runAction = vi.fn(() => new Promise<{ ok: boolean; agentId?: string }>((resolve, reject) => { settle = { resolve, reject }; }));
    return { runAction, settle: () => settle! };
  }

  /**
   * Scenario: Start is pressed and the daemon has not answered. The header's
   * close button is disabled and says why, and neither Esc nor a backdrop
   * click closes the dialog. Once the daemon refuses the start, the
   * refusal is shown and every way out works again.
   */
  it("cannot be closed until the daemon answers the start", async () => {
    const held = heldStart();
    const runtime = fakeRuntime({ runAction: held.runAction });
    const { onClose } = renderDialog(runtime);
    await reachForm();

    fireEvent.click(screen.getByTestId("new-agent-start"));
    expect(await screen.findByTestId("new-agent-starting")).toHaveTextContent("Waiting for the daemon to answer the start");
    const close = screen.getByRole("button", { name: "Close new agent" });
    expect(close).toBeDisabled();
    expect(close).toHaveAttribute("title", expect.stringContaining("Waiting for the daemon to answer the start"));
    fireEvent.click(close);
    fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" });
    fireEvent.mouseDown(screen.getByTestId("new-agent-backdrop"));
    expect(onClose).not.toHaveBeenCalled();

    await act(async () => held.settle().reject(new Error("the daemon did not answer the start within 15s")));

    expect(await screen.findByTestId("new-agent-error")).toHaveTextContent("did not answer the start within 15s");
    expect(screen.queryByTestId("new-agent-starting")).toBeNull();
    expect(screen.getByRole("button", { name: "Close new agent" })).toBeEnabled();
    fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" });
    fireEvent.mouseDown(screen.getByTestId("new-agent-backdrop"));
    fireEvent.click(screen.getByRole("button", { name: "Close new agent" }));
    expect(onClose).toHaveBeenCalledTimes(3);
  });

  /**
   * Scenario (Greptile's review of PR #1235): press Start and let the daemon hold
   * its answer. Every control in the dialog is disabled at once, so no tab stop
   * is left inside it and none outside either — the background is inert — and
   * the dialog itself is the focus target that is left. The sentence explaining
   * why nothing answers is a live region, which is the only way a screen reader
   * learns it: a disabled close button announces neither itself nor the `title` that
   * carries the same explanation for a sighted user.
   *
   * **What this tier proves is the markup, not the focus outcome.** Measured by
   * disabling the hook: this test still passes without it, because jsdom does
   * not blur an element that becomes disabled, so focus simply stays on the
   * Start button that was pressed. In a real engine that button is blurred and
   * `useInertBackground` — which re-marks on every commit and moves focus back
   * inside when it has left — is what stops focus landing on `<body>` behind an
   * inert screen. The assertion below is therefore that focus is still in the
   * dialog, which holds in both, and the fence itself is pinned where it is
   * observable: `AgentOverviewNewAgent.test.tsx` for the marking, and
   * `desktop/e2e/new-agent.spec.ts` for what the marking does.
   */
  it("keeps focus and an announcement in the dialog while every control is disabled", async () => {
    const held = heldStart();
    const runtime = fakeRuntime({ runAction: held.runAction });
    renderDialog(runtime);
    await reachForm();

    fireEvent.click(screen.getByTestId("new-agent-start"));
    const starting = await screen.findByTestId("new-agent-starting");
    expect(starting).toHaveAttribute("role", "status");

    const flow = screen.getByTestId("new-agent-dialog");
    const focusable = Array.from(flow.querySelectorAll<HTMLElement>("button, input, select, textarea"));
    expect(focusable.length).toBeGreaterThan(0);
    expect(focusable.every((control) => control.hasAttribute("disabled"))).toBe(true);
    expect(flow).toHaveAttribute("tabindex", "-1");
    expect(flow.contains(document.activeElement)).toBe(true);

    await act(async () => held.settle().reject(new Error("the daemon did not answer the start within 15s")));
    await screen.findByTestId("new-agent-error");
    expect(screen.getByRole("button", { name: "Close new agent" })).toBeEnabled();
  });

  /**
   * Scenario: the overview drops the dialog for its own reasons while an
   * orchestration launch is in flight, and the launch then fails with roles
   * it could not confirm stopped. The runtime has already filed that failure
   * under its global error — now the only copy of it — and the gone dialog
   * must leave it there rather than clear it.
   */
  it("leaves the runtime's global error alone when a start fails after the dialog is gone", async () => {
    const held = heldStart();
    const runtime = fakeRuntime({ runAction: held.runAction, newAgentOrchestrations: vi.fn(async (): Promise<NewAgentOrchestrations> => ({ kind: "project", path: "/home/dev/Alpha-project", displayPath: "/home/dev/Alpha-project", displayName: "Alpha-project", orchestrations: [{ name: "loop", displayName: "loop", default: true, roles: [{ name: "planner", displayName: "planner", start: true }] }] })) });
    const { unmount } = renderDialog(runtime);
    await currentPath("/home/dev");
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await currentPath("/home/dev/Alpha-project");
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    fireEvent.click(await screen.findByTestId("new-agent-mode-orch:loop"));
    fireEvent.click(screen.getByTestId("new-agent-start"));
    await screen.findByTestId("new-agent-starting");

    unmount();
    await act(async () => held.settle().reject(new LaunchCleanupError("failed to start orchestration role builder: refused; cleanup could not confirm stop for 1 of 1", ["planner"])));

    expect(runtime.clearError).not.toHaveBeenCalled();
  });

  /** Scenario: the same, for a plain agent's start. */
  it("leaves the global error alone for a plain start that fails after the dialog is gone", async () => {
    const held = heldStart();
    const runtime = fakeRuntime({ runAction: held.runAction });
    const { unmount } = renderDialog(runtime);
    await reachForm();
    fireEvent.click(screen.getByTestId("new-agent-start"));
    await screen.findByTestId("new-agent-starting");

    unmount();
    await act(async () => held.settle().reject(new Error("deck refused")));

    expect(runtime.clearError).not.toHaveBeenCalled();
  });
});

describe("New agent dialog — authoring agents (PRD #1223 M7)", () => {
  const AUTHORING: NewAgentOptions = { ...DECK_OPTIONS, authoringKinds: ["schedule", "schedule-issues", "dispatcher"] };
  const optionsOf = (patch: Partial<Extract<NewAgentOptions, { kind: "deck" }>> = {}) => vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(AUTHORING), ...patch } as NewAgentOptions));
  const modeLabels = () => within(screen.getByTestId("new-agent-modes")).getAllByRole("button").map((chip) => chip.textContent);

  /**
   * Scenario: reach the form on a daemon that lists all three authoring kinds
   * with its experimental flag off. The Mode row offers No mode, schedule and
   * dispatcher — not `schedule: issues`, which the TUI shows only with the
   * flag. With the flag on, it is offered between the other two.
   */
  it.each([
    ["off", false, ["No mode", "schedule", "dispatcher"]],
    ["on", true, ["No mode", "schedule", "schedule: issues", "dispatcher"]],
  ])("offers the authoring chips the daemon lists, schedule: issues only with the daemon's flag %s", async (_flag, experimental, expected) => {
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
    ["a daemon that composes no seed", optionsOf({ authoringKinds: [], experimental: true }), "cannot compose authoring seeds"],
  ])("withholds every authoring chip on %s and says why", async (_case, newAgentOptions, reason) => {
    renderDialog(fakeRuntime({ newAgentOptions }));
    await reachForm();

    expect(await screen.findByTestId("new-agent-authoring-withheld")).toHaveTextContent(reason);
    expect(modeLabels()).toEqual(["No mode"]);
  });

  /**
   * Scenario: on a daemon with no configured default command, choose schedule
   * and leave Command blank. The start carries the authoring kind and
   * resolves the blank Command to `claude` — the TUI's fallback — rather than
   * sending none, which would start the daemon's default shell. The Command
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
   * Scenario: on a daemon whose host configures `default_command`, clear the
   * prefilled Command, move to dispatcher with the Right arrow and start. The
   * blank Command resolves to the configured command, trimmed; a typed
   * command, by contrast, is sent as it stands.
   */
  it("resolves a blank Command to the daemon's default command, and sends a typed one as it is", async () => {
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
   * Scenario: the daemon refuses the authoring start — here as an older deck
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
   * Scenario: an authoring start the daemon accepts. The pane opens only once
   * the target deck's fleet entry lists the new agent — the same wait a plain
   * agent gets.
   */
  it("opens the authoring agent's pane once the daemon lists it", async () => {
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
   * Scenario: choose schedule, type Pi's command, then go up in the browser
   * and use that directory instead. The Mode goes back to No mode, as every
   * fresh TUI form does, and the Name follows the new directory — while
   * Command, which hangs off the daemon rather than the directory, stays.
   */
  it("re-derives Mode and Name from a newly confirmed directory, keeping Command", async () => {
    renderDialog(fakeRuntime({ newAgentOptions: optionsOf() }));
    await reachForm();
    fireEvent.click(await screen.findByTestId("new-agent-mode-schedule"));
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "pi --thinking" } });

    fireEvent.keyDown(directoryList(), { key: "h" });
    await currentPath("/home/dev/beta");
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));

    await waitFor(() => expect(screen.getByTestId("new-agent-mode-none")).toHaveAttribute("aria-pressed", "true"));
    expect(screen.getByTestId("new-agent-mode-schedule")).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/beta");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("beta");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("pi --thinking");
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

  /** A live orchestration role on a daemon, carrying `title` (or none) and running in `cwd`. */
  function orchestrationRole(id: string, daemonId: string, title: string | undefined, name = "loop", cwd?: string): AgentSession {
    return {
      ...createFixtureStartedAgent({ id, daemonId }),
      tab: { kind: "orchestration", name, displayTitle: title, roleName: "planner", roleIndex: 0, isStartRole: true, cwd, orchestrationId: `run-${id}` },
      inOrchestration: true,
      isStartRole: true,
    };
  }

  /** With the only eligible deck chosen on open, enter the project directory — the first entry — and use it. */
  async function reachProjectForm() {
    await currentPath("/home/dev");
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await currentPath(PROJECT);
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await waitFor(() => expect(screen.getByTestId("new-agent-name")).toBeEnabled());
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
   * Scenario: two decks expose the SAME path, and only the second holds a
   * project there. Browse to it on the first deck — no orchestrations asked,
   * the listing marked it as no project — then switch to a daemon whose HOME is
   * that path, so nothing re-lists it. The second deck is still asked, and its
   * modes appear. The markers come from a daemon's own listings, so carrying
   * them across a daemon switch hid modes the user does have (Qodo on PR #1235).
   */
  it("re-asks for orchestrations after a daemon switch, rather than trusting the old deck's marker", async () => {
    const SHARED = "/home/dev/shared";
    const listing = (path: string, entries: { path: string; displayName: string; isProject: boolean }[], parent?: string) =>
      ({ kind: "listing", path, displayPath: path, parent, entries, truncated: false }) as DeckDirectoryListing;
    const runtime = fakeRuntime({
      fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE)],
      // The REMOTE deck's home IS the shared path, so choosing it there never
      // re-lists its parent and never re-marks it.
      listDirectories: vi.fn(async (deckId: string, path?: string): Promise<DeckDirectoryListing> =>
        path === SHARED || (path === undefined && deckId === REMOTE)
          ? listing(SHARED, [], "/home/dev")
          : listing("/home/dev", [{ path: SHARED, displayName: "shared", isProject: false }])),
      newAgentOrchestrations: orchestrationsOf(),
    });
    renderDialog(runtime, { initialDeckId: LOCAL });

    await currentPath("/home/dev");
    fireEvent.keyDown(directoryList(), { key: "Enter" });
    await currentPath(SHARED);
    fireEvent.keyDown(directoryList(), { key: " " });
    await waitFor(() => expect(screen.getByTestId("new-agent-name")).toBeEnabled());
    expect(runtime.newAgentOrchestrations).not.toHaveBeenCalled();

    fireEvent.click(deckList().querySelector(`[data-deck-id="${REMOTE}"]`)!);
    await currentPath(SHARED);
    fireEvent.keyDown(directoryList(), { key: " " });
    await waitFor(() => expect(screen.getByTestId("new-agent-name")).toBeEnabled());

    expect(runtime.newAgentOrchestrations).toHaveBeenCalledWith(REMOTE, SHARED);
    expect(await chip()).toHaveTextContent("Orch: loop");
  });

  /**
   * Scenario: the chosen deck cannot launch an orchestration from this flow —
   * it lacks the project verbs or cannot start a role with its configured
   * command. No chip is offered and the daemon's own reason is shown instead.
   */
  it("withholds the chips with the daemon's reason", async () => {
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
    expect(screen.getByTestId("new-agent-start")).toHaveTextContent("Activate orchestration");

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
  it("refuses a Name that is a live orchestration's title on that daemon", async () => {
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
   * captured deck with the daemon's own project path and orchestration name,
   * the Name as the run's title, the resolved config revision, and no command
   * or task. Once that daemon lists the START role the reply named, its pane is
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

  /**
   * Scenario (PRD #1223 audit F2): the project defines `loop` twice and
   * `solo` once. The two `loop` chips are shown disabled, with a reason that
   * says to rename one, and neither a click nor the arrow keys can select
   * one; `solo` can be chosen and is what the launch sends. No
   * `start_orchestration` ever names `loop`.
   */
  it("shows namesake orchestrations disabled with the reason and never launches one", async () => {
    const orchestration = (name: string, start: string) => ({ name, displayName: name, default: false, roles: [{ name: start, displayName: start, start: true }] });
    const runtime = fakeRuntime({
      newAgentOrchestrations: orchestrationsOf({ ...PROJECT_ORCHESTRATIONS, orchestrations: [orchestration("loop", "planner"), orchestration("solo", "worker"), orchestration("loop", "reviewer")] }),
      runAction: vi.fn(async () => ({ ok: true, agentId: "21" })),
    });
    renderDialog(runtime);
    await reachProjectForm();

    const namesakes = await screen.findAllByTestId(/^new-agent-mode-ambiguous-/);
    expect(namesakes).toHaveLength(2);
    for (const namesake of namesakes) {
      expect(namesake).toBeDisabled();
      expect(namesake).toHaveTextContent("Orch: loop");
      expect(namesake).toHaveAttribute("title", "This project defines more than one orchestration named loop; rename one to activate it here.");
      fireEvent.click(namesake);
    }
    expect(screen.getAllByTestId("new-agent-orchestration-ambiguous")).toHaveLength(1);
    expect(screen.getByTestId("new-agent-orchestration-ambiguous")).toHaveTextContent("rename one to activate it here");
    expect(screen.queryByTestId("new-agent-mode-orch:loop")).toBeNull();
    expect(screen.getByTestId("new-agent-mode-none")).toHaveAttribute("aria-pressed", "true");

    // Right from `No mode` lands on `solo` — the arrow keys skip the namesakes — and a second Right wraps back.
    fireEvent.keyDown(screen.getByTestId("new-agent-modes"), { key: "ArrowRight" });
    expect(screen.getByTestId("new-agent-mode-orch:solo")).toHaveAttribute("aria-pressed", "true");
    fireEvent.keyDown(screen.getByTestId("new-agent-modes"), { key: "ArrowRight" });
    expect(screen.getByTestId("new-agent-mode-none")).toHaveAttribute("aria-pressed", "true");
    fireEvent.keyDown(screen.getByTestId("new-agent-modes"), { key: "ArrowRight" });

    fireEvent.click(screen.getByTestId("new-agent-start"));

    await screen.findByTestId("new-agent-waiting");
    expect(runtime.runAction).toHaveBeenCalledTimes(1);
    expect(runtime.runAction).toHaveBeenCalledWith(expect.objectContaining({ type: "start_orchestration", orchestration: "solo" }));
  });

  /**
   * Scenario (PRD #1223 audit F6): a launch of roles with 128-character names
   * fails, and its rollback could not confirm two of them stopped. The crate's
   * sentence names every started role before it gets to the cleanup, so the
   * 240-character copy cuts that part off; the dialog shows the warning on its
   * own, ABOVE the clamped sentence, from the roles the crate sent as data —
   * and more of the sentence stays readable behind "Detail".
   */
  /** Scenario: Shows a cleanup warning ahead of the clamped error, and the full sentence on demand. */
  it("shows a cleanup warning ahead of the clamped error, and the full sentence on demand", async () => {
    const long = (prefix: string) => `${prefix}${"x".repeat(128 - prefix.length)}`;
    const [planner, builder, reviewer] = [long("planner-"), long("builder-"), long("reviewer-")];
    const refusal = `failed to start orchestration role tester: refused; roles already started: ${planner}, ${builder}, ${reviewer}; cleanup could not confirm stop for 2 of 3 already-started role(s): ${reviewer} (agent-2: the daemon did not answer the stop within 15s), ${planner} (agent-0: stop refused)`;
    const runtime = fakeRuntime({
      newAgentOrchestrations: orchestrationsOf(),
      runAction: vi.fn(async () => { throw new LaunchCleanupError(refusal, [reviewer, planner]); }),
    });
    renderDialog(runtime);
    await reachProjectForm();
    fireEvent.click(await chip());

    fireEvent.click(screen.getByTestId("new-agent-start"));

    const warning = await screen.findByTestId("new-agent-cleanup-warning");
    const error = screen.getByTestId("new-agent-error");
    expect(warning).toHaveTextContent("2 roles may still be running on this daemon");
    expect(warning).toHaveTextContent("reviewer-");
    expect(warning.compareDocumentPosition(error) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(error.textContent).not.toContain("cleanup could not confirm");
    const detail = screen.getByTestId("new-agent-error-detail");
    expect(detail).toHaveTextContent("cleanup could not confirm stop for 2 of 3 already-started role(s)");
    expect(detail).toHaveTextContent("agent-0: stop refused");
  });

  /**
   * Scenario (PRD #1223 audit V3): the project's roles are named after the
   * crate's "deck left the fleet" refusal, which the crate interpolates into
   * the launch's failure sentence. That sentence must not be classified as
   * deck loss: the flow would clear the form, the daemon field renders prose
   * only, and the roles that may still be running would be dropped on the way.
   */
  /** Scenario: Keeps the cleanup warning when a role name quotes the daemon-gone refusal. */
  it("keeps the cleanup warning when a role name quotes the daemon-gone refusal", async () => {
    const hostile = "that daemon is not one this app is observing";
    const refusal = `failed to start orchestration role builder: refused; roles already started: ${hostile}; cleanup could not confirm stop for 1 of 1 already-started role(s): ${hostile} (agent-0: stop refused)`;
    const runtime = fakeRuntime({
      newAgentOrchestrations: orchestrationsOf(),
      runAction: vi.fn(async () => { throw new LaunchCleanupError(refusal, [hostile]); }),
    });
    renderDialog(runtime);
    await reachProjectForm();
    fireEvent.click(await chip());

    fireEvent.click(screen.getByTestId("new-agent-start"));

    const warning = await screen.findByTestId("new-agent-cleanup-warning");
    expect(warning).toHaveTextContent("1 role may still be running on this daemon");
    expect(warning).toHaveTextContent(hostile);
    // The failure stays beside the values — not cleared back to the daemon
    // field, which would have said the daemon had left.
    expect(screen.getByTestId("new-agent-error")).toHaveTextContent("failed to start orchestration role builder");
    expect(screen.queryByTestId("new-agent-deck-notice")).toBeNull();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent(PROJECT);
  });

  /**
   * Scenario (PRD #1223 audit V7): a launch of a large orchestration fails and
   * its rollback cannot confirm twelve roles stopped. The alert names the first
   * eight, each as its own list entry, and COUNTS the rest — the joined,
   * once-clamped sentence it replaced dropped the later identities with nothing
   * saying it had.
   */
  /** Scenario: Lists the unconfirmed roles and counts the ones past the cap. */
  it("lists the unconfirmed roles and counts the ones past the cap", async () => {
    const stops = Array.from({ length: 12 }, (_, index) => `role-${index}`);
    const runtime = fakeRuntime({
      newAgentOrchestrations: orchestrationsOf(),
      runAction: vi.fn(async () => { throw new LaunchCleanupError("failed to start orchestration role tester: refused", stops); }),
    });
    renderDialog(runtime);
    await reachProjectForm();
    fireEvent.click(await chip());

    fireEvent.click(screen.getByTestId("new-agent-start"));

    const warning = await screen.findByTestId("new-agent-cleanup-warning");
    expect(warning).toHaveTextContent("12 roles may still be running on this daemon");
    expect(within(warning).getAllByRole("listitem").map((item) => item.textContent)).toEqual(stops.slice(0, 8));
    expect(screen.getByTestId("new-agent-cleanup-warning-overflow")).toHaveTextContent("…and 4 more");
  });

  /**
   * Scenario (PRD #1223 audit F6): a failure whose rollback was confirmed is
   * a plain rejection, and a short one — so there is no warning and no "Full
   * detail" to open.
   */
  it("shows no cleanup warning or detail view for a short failure the rollback confirmed", async () => {
    const runtime = fakeRuntime({ newAgentOrchestrations: orchestrationsOf(), runAction: vi.fn(async () => { throw new Error("failed to start orchestration role builder: refused; stopped 1 already-started role(s)"); }) });
    renderDialog(runtime);
    await reachProjectForm();
    fireEvent.click(await chip());

    fireEvent.click(screen.getByTestId("new-agent-start"));

    expect(await screen.findByTestId("new-agent-error")).toHaveTextContent("stopped 1 already-started role(s)");
    expect(screen.queryByTestId("new-agent-cleanup-warning")).toBeNull();
    expect(screen.queryByTestId("new-agent-error-detail")).toBeNull();
  });
});

/** Unmount whatever is rendered and render the dialog afresh over `runtime`. */
function cleanupAndRender(runtime: FakeRuntime) {
  cleanup();
  return renderDialog(runtime);
}

describe("New agent dialog — a draft that survives a close (issue 1247)", () => {
  const LEAF = { path: "/home/dev/beta/leaf", displayPath: "/home/dev/beta/leaf" };
  /** A draft on the local deck, with `leaf` chosen and both fields edited. */
  const saved = (patch: Partial<NewAgentDraft> = {}): NewAgentDraft => ({
    deckId: LOCAL,
    deckName: "Local daemon",
    browsing: LEAF.path,
    directory: LEAF,
    mode: "none",
    name: "mine",
    nameTouched: true,
    command: "pi --fast",
    commandTouched: true,
    ...patch,
  });
  const restored = () => screen.queryByTestId("new-agent-restored");
  const pressedMode = () => screen.getByTestId("new-agent-modes").querySelector("[aria-pressed='true']")?.getAttribute("data-mode");

  const CLOSES: Record<string, () => void> = {
    Escape: () => fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" }),
    "a backdrop click": () => fireEvent.mouseDown(screen.getByTestId("new-agent-backdrop")),
    "the close button": () => fireEvent.click(screen.getByRole("button", { name: "Close new agent" })),
    "the browser's q": () => fireEvent.keyDown(directoryList(), { key: "q" }),
  };

  /**
   * Scenario: choose a directory, type a Name, edit the Command, then close
   * the dialog by each route it has. Every one hands the form back as a draft
   * — its deck, the directory, and both edits marked as edits — instead of
   * closing with nothing, which is what threw a filled form away.
   */
  it.each(Object.keys(CLOSES))("hands the form back as a draft when closed by %s", async (route) => {
    const { onClose } = renderDialog(fakeRuntime());
    await reachForm();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "mine" } });
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "pi --fast" } });
    // `q` is a letter in a text field, so the browser's own key needs focus there.
    if (route === "the browser's q") directoryList().focus();

    CLOSES[route]();

    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onClose.mock.calls[0][0]).toEqual(saved());
  });

  /**
   * Scenario: fill the form and press Discard. The dialog closes and hands
   * back nothing, so the next open is a fresh form.
   */
  it("hands back nothing on Discard", async () => {
    const { onClose } = renderDialog(fakeRuntime());
    await reachForm();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "mine" } });

    fireEvent.click(screen.getByRole("button", { name: "Discard new agent" }));

    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onClose.mock.calls[0][0]).toBeUndefined();
  });

  /**
   * Scenario: close an untouched dialog. There is nothing worth keeping
   * beyond the one deck a fresh open would choose anyway, so the draft is only
   * that deck, and reopening it says nothing about a restore.
   */
  it("keeps only the deck of an untouched form, and restores it silently", async () => {
    const { onClose } = renderDialog(fakeRuntime());
    await currentPath("/home/dev");
    fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" });
    const draft = onClose.mock.calls[0][0] as NewAgentDraft;
    expect(draft).toMatchObject({ deckId: LOCAL, nameTouched: false, commandTouched: false, mode: "none" });
    expect(draft.directory).toBeUndefined();

    cleanup();
    renderDialog(fakeRuntime(), { draft });
    await currentPath("/home/dev");
    expect(restored()).toBeNull();
  });

  /**
   * Scenario: open the dialog with a draft. The deck is asked again for its
   * options and for the saved directory's listing, the directory is chosen
   * again by the path the deck returns, and the typed Name and Command are put
   * back as typed. A notice says the form was restored and how to clear it.
   */
  it("replays a draft against fresh answers from the deck", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime, { draft: saved() });

    await waitFor(() => expect(screen.getByTestId("new-agent-dir")).toHaveTextContent(LEAF.path));
    expect(runtime.newAgentOptions).toHaveBeenCalledWith(LOCAL);
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, LEAF.path);
    await currentPath(LEAF.path);
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("pi --fast");
    expect(screen.getByTestId("new-agent-name")).toBeEnabled();
    expect(restored()).toHaveTextContent(DRAFT_RESTORED);
  });

  /**
   * Scenario: the draft's Command was the deck's seed, untouched, and the
   * deck's default command has changed since. The reopened form shows what
   * the deck says NOW, while an untouched Name follows the restored directory.
   */
  it("reseeds an untouched Command and Name rather than restoring stale ones", async () => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(DECK_OPTIONS), defaultCommand: "claude --new" })) });
    renderDialog(runtime, { draft: saved({ name: "old", nameTouched: false, command: "old", commandTouched: false }) });

    await waitFor(() => expect(screen.getByTestId("new-agent-dir")).toHaveTextContent(LEAF.path));
    expect(screen.getByTestId("new-agent-command")).toHaveValue("claude --new");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("leaf");
  });

  /**
   * Scenario: the saved directory is gone from the deck. The restore says so,
   * chooses nothing, and opens the browser where a fresh form would — the
   * deck's home — keeping the typed Name and Command.
   */
  it("drops a directory the deck no longer lists", async () => {
    renderDialog(fakeRuntime(), { draft: saved({ browsing: "/gone", directory: { path: "/gone", displayPath: "/gone" } }) });

    await currentPath("/home/dev");
    expect(await screen.findByText(DRAFT_DIRECTORY_GONE)).toBeInTheDocument();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("pi --fast");
    expect(screen.queryByTestId("new-agent-directory-error")).toBeNull();
  });

  /**
   * Scenario: the saved directory is gone and the draft had chosen a Mode on
   * it. Both losses are named — the directory and the Mode — and the form is
   * back on No mode.
   */
  it("names a saved Mode dropped along with its directory", async () => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(DECK_OPTIONS), authoringKinds: ["dispatcher"] })) });
    renderDialog(runtime, { draft: saved({ browsing: "/gone", directory: { path: "/gone", displayPath: "/gone" }, mode: "dispatcher" }) });

    await currentPath("/home/dev");
    expect(await screen.findByText(DRAFT_DIRECTORY_GONE)).toBeInTheDocument();
    expect(screen.getByText(DRAFT_MODE_GONE)).toBeInTheDocument();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
  });

  /**
   * Scenario: the deck can no longer list directories at all. The restore
   * names the dropped directory and Mode, and the panel says the deck cannot
   * list, as it does on a fresh form — rather than the directory vanishing
   * without a word.
   */
  it("names the dropped directory when the deck can no longer list", async () => {
    const runtime = fakeRuntime({ listDirectories: vi.fn(async (): Promise<DeckDirectoryListing> => ({ kind: "unsupported" })) });
    renderDialog(runtime, { draft: saved({ mode: "dispatcher" }) });

    expect(await screen.findByText(DRAFT_DIRECTORY_GONE)).toBeInTheDocument();
    expect(screen.getByText(DRAFT_MODE_GONE)).toBeInTheDocument();
    expect(screen.getByTestId("new-agent-no-browse")).toBeInTheDocument();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
  });

  /**
   * Scenario: the draft was on the remote deck, which has since disconnected.
   * It is not chosen — the field falls back to the only deck that can take a
   * spawn — its directory is never asked of any deck, and a notice names the
   * deck that was dropped. The typed Name survives.
   */
  it("drops a deck that can no longer take a spawn, keeping typed edits", async () => {
    const runtime = fakeRuntime();
    renderDialog(runtime, { draft: saved({ deckId: REMOTE, deckName: "dev@build-box" }) });

    await currentPath("/home/dev");
    expect(restored()).toHaveTextContent(draftDeckGone("dev@build-box", true));
    expect(screen.getByTestId("new-agent-chosen-deck")).toBeVisible();
    expect(runtime.listDirectories).not.toHaveBeenCalledWith(REMOTE, expect.anything());
    expect(runtime.listDirectories).not.toHaveBeenCalledWith(expect.anything(), LEAF.path);
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
  });

  /**
   * Scenario: a draft on the remote deck, and the dialog opened from the local
   * deck's header. The deck asked for wins; the draft keeps only its typed
   * Name and Command, as a change of deck in the field does, and a notice
   * says its directory was not restored.
   */
  it("lets the deck the flow was opened for outrank the draft's", async () => {
    const runtime = fakeRuntime({ fleet: [deck(LOCAL, { deckKind: "local" }), deck(REMOTE)] });
    renderDialog(runtime, { initialDeckId: LOCAL, draft: saved({ deckId: REMOTE, deckName: "dev@build-box" }) });

    await currentPath("/home/dev");
    expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, undefined);
    expect(runtime.newAgentOptions).not.toHaveBeenCalledWith(REMOTE);
    expect(restored()).toHaveTextContent(draftOtherDeck("dev@build-box"));
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
  });

  /**
   * Scenario: the draft chose the project's `loop` orchestration with the
   * Name untouched, and a run titled `Alpha-project-orchestrator-1` is now
   * live on the deck. The chip is chosen again once the fresh orchestrations
   * offer it, and the Name is regenerated against the live titles — not the
   * one saved — exactly as a click on the chip would.
   */
  it("chooses a saved orchestration chip again once offered, regenerating an untouched Name", async () => {
    const PROJECT = "/home/dev/Alpha-project";
    const live: AgentSession = {
      ...createFixtureStartedAgent({ id: "41", daemonId: LOCAL }),
      tab: { kind: "orchestration", name: "loop", displayTitle: "Alpha-project-orchestrator-1", roleName: "planner", roleIndex: 0, isStartRole: true, orchestrationId: "run-41" },
      inOrchestration: true,
      isStartRole: true,
    };
    const runtime = fakeRuntime({
      fleet: [deck(LOCAL, { deckKind: "local" }, [live]), deck(REMOTE, { status: "disconnected" })],
      newAgentOrchestrations: vi.fn(async (): Promise<NewAgentOrchestrations> => ({ kind: "project", path: PROJECT, displayPath: PROJECT, displayName: "Alpha-project", orchestrations: [{ name: "loop", displayName: "loop", default: true, roles: [{ name: "planner", displayName: "planner", start: true }] }] })),
    });
    renderDialog(runtime, { draft: saved({ browsing: PROJECT, directory: { path: PROJECT, displayPath: PROJECT }, mode: "orch:loop", name: "Alpha-project-orchestrator-1", nameTouched: false }) });

    await waitFor(() => expect(pressedMode()).toBe("orch:loop"));
    expect(screen.getByTestId("new-agent-name")).toHaveValue("Alpha-project-orchestrator-2");
    expect(screen.queryByTestId("new-agent-title-taken")).toBeNull();
  });

  /**
   * Scenario: the draft chose `dispatcher`, and the deck no longer reports it
   * can compose that authoring agent. The form comes back on No mode and says
   * the saved Mode is not offered, rather than holding a chip nobody can see.
   */
  it("drops a saved Mode chip the fresh answers do not offer", async () => {
    renderDialog(fakeRuntime(), { draft: saved({ mode: "dispatcher" }) });

    expect(await screen.findByText(DRAFT_MODE_GONE)).toBeInTheDocument();
    expect(pressedMode()).toBe("none");
    expect(screen.queryByTestId("new-agent-mode-dispatcher")).toBeNull();
  });

  /**
   * Scenario: the same draft on a deck that still composes `dispatcher`. The
   * chip is pressed again and nothing is reported missing.
   */
  it("chooses a saved authoring chip again while the deck offers it", async () => {
    const runtime = fakeRuntime({ newAgentOptions: vi.fn(async (): Promise<NewAgentOptions> => ({ ...structuredClone(DECK_OPTIONS), authoringKinds: ["dispatcher"] })) });
    renderDialog(runtime, { draft: saved({ mode: "dispatcher" }) });

    await waitFor(() => expect(pressedMode()).toBe("dispatcher"));
    expect(screen.queryByText(DRAFT_MODE_GONE)).toBeNull();
  });

  /**
   * Scenario: reopen with a draft whose listing the deck is slow to answer,
   * and close again before it does. The draft handed back is the one saved —
   * its directory and Mode included — not a form the restore had not reached.
   */
  it("keeps a restore still in flight when closed again", async () => {
    const runtime = fakeRuntime({ listDirectories: vi.fn(() => new Promise<DeckDirectoryListing>(() => undefined)) });
    const { onClose } = renderDialog(runtime, { draft: saved({ mode: "dispatcher" }) });
    await waitFor(() => expect(runtime.listDirectories).toHaveBeenCalledWith(LOCAL, LEAF.path));

    fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" });

    expect(onClose.mock.calls[0][0]).toEqual(saved({ mode: "dispatcher" }));
  });

  /**
   * Scenario: start the agent; the deck accepts it and has not listed it yet,
   * so the dialog is waiting. Closing now hands back nothing: that form has
   * been started, and restoring it would invite a second start.
   */
  it("keeps nothing once the deck has accepted the start", async () => {
    const { onClose } = renderDialog(fakeRuntime());
    await reachForm();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "mine" } });
    fireEvent.click(screen.getByTestId("new-agent-start"));
    await screen.findByTestId("new-agent-waiting");

    fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" });

    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onClose.mock.calls[0][0]).toBeUndefined();
  });
});
