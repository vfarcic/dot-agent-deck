// Issue #1356 — the fleet view against two real daemons: the local deck, and a
// second daemon the window reaches as a configured remote deck through its own
// `ssh -N -L` tunnel to a loopback `sshd` the harness runs (`FarDeck` in
// `harness.ts` says how, and why the window runs under `nss_wrapper`).
//
// Every other multi-deck assertion in the repository is against fixtures
// (`?fixture=1&state=fleet` in the browser tier); this is the one that watches
// the real window fold two real daemons' snapshots into one screen.
// `docs/develop/desktop-gui.md` ("The driver tier") has what it covers and
// what it does not.

import assert from "node:assert/strict";
import { test } from "node:test";
import { type Deck, FarDeck, waitFor, withDeck } from "./harness.ts";

/** One deck's section on the overview, as the page renders it. */
type Section = {
  daemonId: string;
  connected: string;
  identity: string;
  lamp: string;
  rows: string[];
};

/** Every deck section on the overview, in page order. */
function sections(deck: Deck): Promise<Section[]> {
  return deck.session.execute<Section[]>(
    `return Array.from(document.querySelectorAll('[data-testid="daemon-group"]')).map((group) => ({
       daemonId: group.getAttribute("data-daemon-id") || "",
       connected: group.getAttribute("data-deck-connected") || "",
       identity: (group.querySelector('[data-testid="daemon-identity"]') || {}).innerText || "",
       lamp: (group.querySelector(".daemon-group-header .connection-lamp") || {}).className || "",
       rows: Array.from(group.querySelectorAll('[data-testid^="overview-agent-"]')).map((row) => row.innerText),
     }));`,
  );
}

/** The deck counter's figure, e.g. `2/2`. */
async function deckCount(deck: Deck): Promise<string> {
  return deck.session.execute<string>(
    `const e = document.querySelector('[data-testid="overview-count-decks"] strong'); return e ? e.innerText.trim() : "";`,
  );
}

/** Wait until the overview's sections satisfy `accept`, failing with what it last showed. */
async function sectionsWhere(deck: Deck, description: string, accept: (seen: Section[], count: string) => boolean): Promise<Section[]> {
  let last: unknown;
  try {
    return await waitFor(description, async () => {
      const seen = await sections(deck);
      const count = await deckCount(deck);
      last = { count, seen };
      return accept(seen, count) ? seen : undefined;
    });
  } catch (error) {
    throw new Error(`${(error as Error).message}; last seen: ${JSON.stringify(last)}`);
  }
}

const holds = (section: Section | undefined, label: string) => !!section?.rows.some((row) => row.includes(label));

/// Scenario: start two real daemons, the local deck and a second one the window
/// reaches as a remote deck over its own ssh tunnel, with one bash agent on
/// each that both daemons number the same; open the window with All Decks
/// selected and see one section per deck, each holding only its own deck's
/// row, and the deck counter read 2/2. Then stop the far daemon and see that
/// deck degrade on its own: its section stays on screen, reads disconnected
/// and lists no agents, the local deck keeps its own row and its connected
/// lamp, and the counter reads 1/2.
test("fleet_001 All Decks shows two real daemons apart, and one stopping is that deck's problem alone", async () => {
  const command = "bash --noprofile --norc";
  // Labels distinctive enough that finding one in the other deck's section is
  // unambiguous.
  const nearLabel = "tango-near-19";
  const farLabel = "sierra-far-73";
  await withDeck("fleet_001", { daemonFirst: true, farDeck: true }, async (deck) => {
    const far = deck.far as FarDeck;
    await deck.startAgent(command, deck.project, nearLabel);
    await far.startAgent(command, far.project, farLabel);

    // Agent ids are per-daemon and monotonic, so each daemon's first agent
    // carries the same id. Required rather than assumed: a pass must not come
    // from two ids that happened not to collide.
    const [near] = await deck.daemonAgents();
    const [remote] = await far.daemonAgents();
    assert.ok(near && remote, "each daemon runs its agent");
    assert.equal(near.agent_id, remote.agent_id, "the two daemons gave their agents the same id");

    const both = await sectionsWhere(
      deck,
      "both decks connected, each section holding its own agent's row and the counter at 2/2",
      (seen, count) =>
        count === "2/2" &&
        seen.length === 2 &&
        seen.every((section) => section.connected === "yes" && section.rows.length === 1) &&
        seen.some((section) => holds(section, nearLabel)) &&
        seen.some((section) => holds(section, farLabel)),
    );
    const nearSection = both.find((section) => holds(section, nearLabel)) as Section;
    const farSection = both.find((section) => holds(section, farLabel)) as Section;
    assert.notEqual(nearSection, farSection, `one section holds both rows: ${JSON.stringify(both)}`);
    assert.ok(!holds(nearSection, farLabel), `the local deck's section holds the far deck's row: ${JSON.stringify(both)}`);
    assert.ok(!holds(farSection, nearLabel), `the far deck's section holds the local deck's row: ${JSON.stringify(both)}`);
    assert.equal(farSection.identity, FarDeck.NAME, "the far deck's section is named after its deck-list row");
    assert.match(nearSection.daemonId, /\S/, "the local deck's section names its deck");
    assert.match(farSection.daemonId, /\S/, "the far deck's section names its deck");
    assert.notEqual(nearSection.daemonId, farSection.daemonId, "the two sections name the same deck");
    for (const section of both) assert.match(section.lamp, /\bconnection-connected\b/, `${section.identity}'s lamp`);

    await far.stopDaemon();

    // The far deck's problem, on its own: its section stays (it is still in
    // the fleet's denominator), reads disconnected and lists no agents — what
    // it runs is unknown now, not its last list — while the local deck keeps
    // exactly its own row and its lamp, and the counter says 1 of 2 rather
    // than dropping the silent deck to read 1/1.
    const after = await sectionsWhere(deck, "the far deck alone to degrade and the counter to read 1/2", (seen, count) => {
      const nearNow = seen.find((section) => section.daemonId === nearSection.daemonId);
      const farNow = seen.find((section) => section.daemonId === farSection.daemonId);
      return (
        count === "1/2" &&
        seen.length === 2 &&
        nearNow?.connected === "yes" &&
        nearNow.rows.length === 1 &&
        holds(nearNow, nearLabel) &&
        /\bconnection-connected\b/.test(nearNow.lamp) &&
        farNow?.connected === "no" &&
        farNow.rows.length === 0 &&
        /\bconnection-disconnected\b/.test(farNow.lamp)
      );
    });
    const farAfter = after.find((section) => section.daemonId === farSection.daemonId) as Section;
    assert.equal(farAfter.identity, FarDeck.NAME, "the degraded section is still the far deck's");
    const nearAfter = after.find((section) => section.daemonId === nearSection.daemonId) as Section;
    assert.ok(!holds(nearAfter, farLabel), `the local deck's section took the stopped deck's row: ${JSON.stringify(after)}`);
  });
});
