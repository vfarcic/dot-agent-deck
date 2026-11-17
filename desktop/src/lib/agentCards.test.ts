import { describe, expect, it } from "vitest";
import type { AgentCardEntry, AgentCardsSnapshot } from "../types";
import { acceptCardsSnapshot, exactTextSegments, exactTextString, placedCardKey, promptPreview, pruneCardResults, reconcileCards, type CardResults, type PlacedCard } from "./agentCards";

/*
 * PRD #2374 review round 1 — the pure halves of the cards' safety fixes: the
 * exact-text preview (F3), which snapshots a deck's cards may be replaced by
 * (B6), and the bound on the answers the webview keeps (F5).
 */

function snapshot(instanceId: string, revision: number): AgentCardsSnapshot {
  return { deckId: "deck", instanceId, revision, entries: [], catalogErrors: [] };
}

describe("exactTextSegments (F3)", () => {
  it("gives back every character it was given, and escapes only the invisible ones", () => {
    for (const prompt of ["Use\tmain", "Run tests. Then delete the private branch.", "abc‮def", "a​b﻿c­d\r\ne", "plain\nlines\n", "emoji 👩‍💻 stay"]) {
      const segments = exactTextSegments(prompt);
      expect(segments.map((segment) => segment.raw).join("")).toBe(prompt);
      for (const segment of segments) {
        if (segment.escaped) expect(segment.shown).toMatch(/^\\(t|r|u\{[0-9A-F]{4,}\})$/);
        else expect(segment.shown).toBe(segment.raw);
      }
    }
    expect(exactTextString("Use\tmain")).toBe("Use\\tmain");
    expect(exactTextString("abc‮def")).toBe("abc\\u{202E}def");
    expect(exactTextString("a b")).toBe("a\\u{2029}b");
    // A zero-width joiner inside an emoji is a format character too: shown, not applied.
    expect(exactTextString("👩‍💻")).toBe("👩\\u{200D}💻");
  });

  it("the card's short preview escapes the same way and collapses nothing", () => {
    expect(promptPreview("a  b\tc")).toBe("a  b\\tc");
    expect(promptPreview("x".repeat(70))).toBe(`${"x".repeat(59)}…`);
  });
});

describe("acceptCardsSnapshot and reconcileCards (B6)", () => {
  const connected = { connected: true, incarnation: "A" };

  it("takes a deck's cards only from the connected daemon process, never an older revision of it", () => {
    expect(acceptCardsSnapshot(undefined, snapshot("A", 1), connected)).toBe(true);
    // Not connected, or not in the fleet at all.
    expect(acceptCardsSnapshot(undefined, snapshot("A", 1), { connected: false, incarnation: "A" })).toBe(false);
    expect(acceptCardsSnapshot(undefined, snapshot("A", 1), undefined)).toBe(false);
    // A delayed push or read older than what is held.
    expect(acceptCardsSnapshot(snapshot("A", 5), snapshot("A", 4), connected)).toBe(false);
    expect(acceptCardsSnapshot(snapshot("A", 5), snapshot("A", 5), connected)).toBe(true);
    expect(acceptCardsSnapshot(snapshot("A", 5), snapshot("A", 6), connected)).toBe(true);
    // Another process's cards while the connection names A: a late answer of the predecessor, or a successor the fleet has not named yet.
    expect(acceptCardsSnapshot(undefined, snapshot("B", 9), connected)).toBe(false);
    // The successor's first list may carry a lower revision than its predecessor's last.
    expect(acceptCardsSnapshot(snapshot("A", 50), snapshot("B", 2), { connected: true, incarnation: "B" })).toBe(true);
  });

  it("drops the cards of a deck that left, disconnected, or was replaced while it stayed connected", () => {
    const held = { a: { ...snapshot("A", 3), deckId: "a" }, b: { ...snapshot("B", 3), deckId: "b" }, c: { ...snapshot("C", 3), deckId: "c" }, d: { ...snapshot("D", 3), deckId: "d" } };
    const connections = new Map([
      ["a", { connected: true, incarnation: "A" }],
      ["b", { connected: true, incarnation: "B2" }],
      ["c", { connected: false, incarnation: "C" }],
    ]);
    expect(Object.keys(reconcileCards(held, connections))).toEqual(["a"]);
    const unchanged = { a: held.a };
    expect(reconcileCards(unchanged, connections)).toBe(unchanged);
  });
});

describe("pruneCardResults (F5)", () => {
  function card(key: string, tokens: string[]): PlacedCard {
    return { deckId: "deck", entry: { key, instance_id: "A", actions: tokens.map((token) => ({ token })) } as unknown as AgentCardEntry };
  }

  it("keeps only the answers of cards still listed and of action versions they still offer", () => {
    const kept = card("p/1", ["t1", "t2"]);
    const results: Record<string, CardResults> = {
      [placedCardKey(kept)]: { t1: { outcome: "pending" }, gone: { outcome: "pending" } },
      [placedCardKey(card("p/9", []))]: { t9: { outcome: "pending" } },
    };
    const pruned = pruneCardResults(results, [kept]);
    expect(pruned).toEqual({ [placedCardKey(kept)]: { t1: { outcome: "pending" } } });
    expect(pruneCardResults(pruned, [kept])).toBe(pruned);
  });
});
