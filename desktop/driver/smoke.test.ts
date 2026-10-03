// Issue #953 — the driver rung: the real Tauri window, driven through
// tauri-driver and WebKitWebDriver, against a real daemon.
//
// Narrow on purpose. Each scenario is here for something the browser tier
// under `desktop/e2e` structurally cannot reach — it drives the built web
// bundle with fixture data, so it has no IPC, no `desktop://` event stream, no
// daemon and no PTY — and nothing is here that the browser tier already
// answers. `docs/develop/desktop-gui.md` ("The driver tier") has what these
// cover and, at the same length, what they do not.

import assert from "node:assert/strict";
import { basename } from "node:path";
import { test } from "node:test";
import { type Deck, waitFor, withDeck } from "./harness.ts";
import { CONTROL, ENTER, type Element, META, SHIFT } from "./webdriver.ts";

const connected = '[data-testid="daemon-group"][data-deck-connected="yes"]';

/// Scenario: open the real window with no daemon on its socket and see the
/// overview say the deck is disconnected, then start a real daemon on that
/// socket and see the same window connect to it by itself — deck counter 1/1,
/// connected lamp, a daemon identity and the empty-deck note.
test("handshake_001 the window connects to a daemon that appears after it opened", async () => {
  await withDeck("handshake_001", { daemonFirst: false }, async (deck) => {
    // The disconnected screen is the answer to a real `desktop_bootstrap`
    // connect-only probe over IPC, which found nothing on the sandbox socket.
    await deck.element('[data-testid="overview-disconnected"]', "the overview's disconnected note");
    await deck.textOf('[data-testid="overview-count-decks"]', (t) => t.includes("0/1"), "the deck counter to read 0/1");

    await deck.startDaemon();

    // Nothing is clicked: the Rust watcher re-probes while disconnected and
    // publishes the result on the `desktop://` event stream, so this is the
    // handshake plus the first snapshot arriving through both halves of the
    // bridge.
    const group = await deck.element(connected, "the deck group to report connected");
    const daemonId = await deck.session.attribute(group, "data-daemon-id");
    assert.match(daemonId ?? "", /^deck-[0-9a-f]{16}$/, "the connected group names the daemon it handshook with");
    await deck.textOf('[data-testid="overview-count-decks"]', (t) => t.includes("1/1"), "the deck counter to read 1/1");
    await deck.element(".connection-lamp.connection-connected", "a connected lamp");
    // The daemon's own snapshot says it runs no agents, and the window says so.
    await deck.element('[data-testid="overview-first-run"]', "the no-agents-yet note from the daemon's snapshot");
    assert.equal(await deck.session.find('[data-testid="overview-disconnected"]'), null);
  });
});

/// Scenario: with a real daemon running, start a shell agent through the real
/// New agent dialog, wait for its pane to open on a live terminal, type a
/// command into xterm and see the shell's computed answer come back on screen;
/// then close the pane and find the agent's row on the overview, and in the
/// daemon's own `daemon status`.
test("terminal_001 a shell started from New agent runs a typed command in a real PTY", async () => {
  const command = "bash --noprofile --norc";
  await withDeck("terminal_001", { daemonFirst: true, defaultCommand: command }, async (deck) => {
    await deck.element(connected, "the deck group to report connected");

    await deck.session.click(await deck.element('[data-testid="overview-new-agent"]'));
    await deck.element('[data-testid="new-agent-dialog"]', "the New agent dialog");
    // The directory browser opens where the DAEMON says: `default_dir` is in
    // the sandbox deck's own config.toml, and the window only learns it by
    // asking the deck over IPC.
    await deck.textOf(
      '[data-testid="new-agent-current-path"]',
      (t) => t.trim() === deck.project,
      `the browser to open at the deck's default_dir ${deck.project}`,
    );
    await deck.session.click(await deck.element('[data-testid="new-agent-use-directory"]'));
    // Same for the command: the field is seeded from the deck's own
    // `default_command`, which is a second round trip to the daemon.
    const commandField = await deck.element('[data-testid="new-agent-command"]');
    await waitForValue(deck, commandField, command, "the Command field to carry the deck's default_command");
    await waitForValue(
      deck,
      await deck.element('[data-testid="new-agent-name"]'),
      basename(deck.project),
      "the Name field to default to the directory's name",
    );

    await deck.session.click(await deck.element('[data-testid="new-agent-start"]'));

    // The pane opens by itself once the fleet lists the new agent, which is
    // the daemon's snapshot coming back on the event stream.
    await deck.element('[data-testid="agent-pane-overlay"]', "the new agent's pane to open");
    await deck.element(
      '[data-testid="agent-pane-overlay"] .agent-terminal-stack[data-terminal-state="attached"]',
      "the pane's terminal to attach",
    );
    // The shell's first prompt, which only a live PTY can have drawn.
    await waitFor("the shell's prompt to reach xterm", async () =>
      (await deck.terminalTexts()).some(({ text }) => text.includes("$")),
    );

    const input = await deck.element(
      '[data-testid="agent-pane-overlay"] textarea.xterm-helper-textarea:not([disabled])',
      "xterm's input to be enabled",
    );
    // The shell computes 6*7 itself. The typed line reads `$((6*7))`, so the
    // only way `-42-` reaches the screen is keystrokes → IPC → daemon → PTY →
    // bash, and its output back through the event stream into xterm.
    await deck.session.type(input, `echo dad-driver-$((6*7))-ok${ENTER}`);
    await waitFor("the shell's output to reach xterm", async () =>
      (await deck.terminalTexts()).some(({ text }) => text.includes("dad-driver-42-ok")),
    );

    // The pane's own Back to dashboard control, not Escape: focus is in xterm
    // now, and a terminal rightly hands Escape to the shell.
    await deck.session.click(
      await deck.element(
        '[data-testid="agent-pane-overlay"] button.agent-pane-control[aria-label="Back to dashboard"]',
        "the pane's Back to dashboard control",
      ),
    );
    await waitFor("the pane to close", async () => (await deck.session.find('[data-testid="agent-pane-overlay"]')) === null);
    await deck.textOf(
      '[data-testid^="overview-agent-"]',
      (t) => t.includes(basename(deck.project)),
      "the agent's overview row, named after its directory",
    );
    await deck.textOf('[data-testid="overview-count-agents"]', (t) => t.includes("1"), "the agent counter to read 1");

    // And the daemon agrees, asked by a different client.
    const agents = await deck.daemonAgents();
    assert.equal(agents.length, 1, `the daemon runs exactly the one agent: ${JSON.stringify(agents)}`);
    assert.equal(agents[0].cwd, deck.project);
  });
});

/// Scenario: in a shell agent's pane, print a line, click into the terminal and
/// select the line by dragging the mouse across it, and press Ctrl+Shift+C, then find exactly that line on the
/// system clipboard; select a second line and copy it with Cmd (Meta)+C the
/// same way. The copies are made over a half-typed command, and pressing Enter
/// afterwards runs it unchanged — so neither copy sent the agent an interrupt or
/// a single stray byte. Last, the control: plain Ctrl+C over a selection still
/// interrupts, and throws a half-typed command away.
test("terminal_002 selected terminal text copies to the clipboard without reaching the agent", async () => {
  const command = "bash --noprofile --norc";
  await withDeck("terminal_002", { daemonFirst: true, defaultCommand: command }, async (deck) => {
    await deck.element(connected, "the deck group to report connected");
    const input = await openShellPane(deck);
    await deck.traceTerminals();

    // Computed by the shell, so the line on screen, and on the clipboard, can
    // only have come from the PTY rather than from what was typed.
    await deck.session.type(input, `echo dad-copy-$((6*7))-ok${ENTER}`);
    const first = "dad-copy-42-ok";
    await waitFor(`${first} on its own row`, () => deck.rowSpan(first));

    // Half a command, not yet run: an interrupt would throw it away, and any
    // byte the gesture leaked would land in it.
    await deck.session.type(input, "echo still-typing-$((2+3))");
    await deck.selectRow(first);
    await deck.session.chord([CONTROL, SHIFT, "c"]);
    await waitFor(`the clipboard to hold ${first}`, async () => (await deck.clipboardText()) === first);

    // macOS's gesture. Its engine is out of this tier's reach (WKWebView has no
    // WebDriver), but the chord is decided in the page, and this runs that code.
    await deck.session.type(input, ENTER);
    const second = "still-typing-5";
    await waitFor(`${second} on its own row, the half-typed command run unchanged`, () => deck.rowSpan(second));
    await deck.selectRow(second);
    await deck.session.chord([META, "c"]);
    await waitFor(`the clipboard to hold ${second}`, async () => (await deck.clipboardText()) === second);

    // The control. Plain Ctrl+C belongs to the agent even while text is
    // selected: bash abandons the half-typed line and prints a fresh prompt.
    await deck.session.type(input, "echo must-not-run");
    await deck.selectRow(first);
    await deck.session.chord([CONTROL, "c"]);
    await deck.session.type(input, `echo after-interrupt-$((3+4))${ENTER}`);
    await waitFor("the next command's output", async () => (await deck.rowSpan("after-interrupt-7")) !== undefined);
    const text = (await deck.terminalTexts()).map(({ text }) => text).join("\n");
    assert.ok(!text.split("\n").includes("must-not-run"), `Ctrl+C did not interrupt the half-typed command:\n${text}`);
    assert.equal(await deck.clipboardText(), second, "plain Ctrl+C copied the selection");
  });
});

/// Scenario: in a shell agent's pane, with another window in front of the app,
/// press on a printed line in the terminal — the press that brings the window
/// to the front — and drag across the line, and find exactly that line
/// selected. First with the deck alone, where coming to the front changes
/// nothing; then with a stand-in TUI on the same agent that the person used
/// last, so coming to the front resizes the agent to the window's own pane
/// while the button is still down, and the drag must keep its selection anyway.
test("terminal_003 a drag that starts with the press focusing the window keeps its selection", async () => {
  const command = "bash --noprofile --norc";
  await withDeck("terminal_003", { daemonFirst: true, defaultCommand: command }, async (deck) => {
    await deck.element(connected, "the deck group to report connected");
    const input = await openShellPane(deck);
    await deck.traceTerminals();
    const [agent] = await deck.daemonAgents();
    // The grid the pane fits, once the daemon has applied it. The window leaves
    // and regains the focus first. On GitHub runners the grid has held at the
    // spawn size, 80x24, until the window's first focus-in that the window
    // itself saw — even with the page already reporting focus (PR #1505's
    // first two CI runs) — and the control below would see that resize rather
    // than none.
    await deck.setWindowFocus(false);
    await deck.setWindowFocus(true);
    await deck.gridSettled();
    const [own] = await deck.grids();

    // The control: the same gesture when coming to the front resizes nothing,
    // because the window is the only client and already the one sized for.
    await deck.session.type(input, `echo dad-alone-$((6*7))-ok${ENTER}`);
    await deck.setWindowFocus(false);
    await dragFocusingTheWindow(deck, "dad-alone-42-ok");
    assert.deepEqual((await deck.grids())[0], own, "coming to the front resized the grid with no other client");

    // A TUI the person used last, smaller than the pane, so it sizes the agent.
    await deck.setWindowFocus(false);
    const tui = await deck.standInClient(agent.agent_id, 12, 70);
    try {
      await waitFor("the grid to take the stand-in TUI's size", async () => {
        const [cols, rows] = (await deck.grids())[0];
        return cols === 70 && rows === 12;
      });
      await deck.session.type(input, `echo dad-front-$((6*7))-ok${ENTER}`);
      await dragFocusingTheWindow(deck, "dad-front-42-ok", own);
    } finally {
      tui.close();
    }
  });
});

/**
 * Issue #1457 — press on the row whose text is exactly `text`, bring the window
 * to the front while the button is held, finish the drag across that row and
 * wait for xterm to hold it as the selection. With `resizedTo`, the focus-in
 * must resize the grid to that `[cols, rows]` before the drag finishes, and the
 * drag ends on the row where it now is, which is where a person's pointer
 * would follow it.
 */
async function dragFocusingTheWindow(deck: Deck, text: string, resizedTo?: [number, number]): Promise<void> {
  const start = await waitFor(`${text} on its own row`, () => deck.rowSpan(text));
  await deck.session.pressAt(start.from);
  await deck.setWindowFocus(true);
  if (resizedTo) {
    await waitFor(`the window's focus claim to resize the grid to ${resizedTo.join("x")}`, async () => {
      const [cols, rows] = (await deck.grids())[0];
      return cols === resizedTo[0] && rows === resizedTo[1];
    });
  }
  const end = await waitFor(`${text} on its own row after the focus-in`, () => deck.rowSpan(text));
  await deck.session.releaseAt(end.to);
  await waitFor(`the drag to select ${text}`, () => deck.hasSelection(text), 15_000).catch(async (error: Error) => {
    throw new Error(`${error.message}; xterm holds ${JSON.stringify(await deck.selections())}`);
  });
}

/** Start `default_command` from New agent and return its pane's enabled xterm input, at the shell's first prompt. */
async function openShellPane(deck: Deck): Promise<Element> {
  await deck.session.click(await deck.element('[data-testid="overview-new-agent"]'));
  await deck.textOf(
    '[data-testid="new-agent-current-path"]',
    (t) => t.trim() === deck.project,
    `the browser to open at the deck's default_dir ${deck.project}`,
  );
  await deck.session.click(await deck.element('[data-testid="new-agent-use-directory"]'));
  await waitForValue(deck, await deck.element('[data-testid="new-agent-name"]'), basename(deck.project), "the Name field");
  await deck.session.click(await deck.element('[data-testid="new-agent-start"]'));
  await deck.element(
    '[data-testid="agent-pane-overlay"] .agent-terminal-stack[data-terminal-state="attached"]',
    "the pane's terminal to attach",
  );
  await waitFor("the shell's prompt to reach xterm", async () =>
    (await deck.terminalTexts()).some(({ text }) => text.includes("$")),
  );
  return deck.element(
    '[data-testid="agent-pane-overlay"] textarea.xterm-helper-textarea:not([disabled])',
    "xterm's input to be enabled",
  );
}

async function waitForValue(deck: Deck, element: Element, expected: string, description: string): Promise<void> {
  await waitFor(description, async () => (await deck.session.property(element, "value")) === expected);
}
