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
import { ENTER, type Element } from "./webdriver.ts";

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

    // The pane's own Close control, not Escape: focus is in xterm now, and a
    // terminal rightly hands Escape to the shell.
    await deck.session.click(
      await deck.element(
        '[data-testid="agent-pane-overlay"] button.agent-pane-control[aria-label^="Close "]',
        "the pane's Close control",
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

async function waitForValue(deck: Deck, element: Element, expected: string, description: string): Promise<void> {
  await waitFor(description, async () => (await deck.session.property(element, "value")) === expected);
}
