import { describe, expect, it } from "vitest";
import { MAX_STORED_ROLE_ORDER, parseStoredRoleOrder, planStoredRoleOrder, roleOrderFor } from "./roleOrder";

describe("saved role order (issue #1045 audit finding 1)", () => {
  /** Scenario: a legacy value whose order is a string is not migrated, the legacy key is cleared, and the default order is used. */
  it("refuses a malformed legacy value instead of copying it to the new key", () => {
    const plan = planStoredRoleOrder(null, JSON.stringify({ order: "abc" }));

    expect(plan).toEqual({ removeLegacy: true });
    expect(roleOrderFor(plan.order, ["planner", "builder"])).toEqual(["planner", "builder"]);
  });

  /** Scenario: stored values that are not a plain object holding an order array fall back to the default. */
  it("refuses values that are not a plain object with an order array", () => {
    for (const raw of ["not json", "null", "42", "\"order\"", "[\"planner\"]", "{}", JSON.stringify({ order: { 0: "planner" } })]) {
      expect(parseStoredRoleOrder(raw), raw).toBeUndefined();
    }
    expect(parseStoredRoleOrder(null)).toBeUndefined();
  });

  /** Scenario: an order longer than the bound is refused whole rather than truncated, and is not migrated. */
  it("refuses an oversize array", () => {
    const within = Array.from({ length: MAX_STORED_ROLE_ORDER }, (_, index) => `role-${index}`);
    expect(parseStoredRoleOrder(JSON.stringify({ order: within }))).toHaveLength(MAX_STORED_ROLE_ORDER);
    expect(parseStoredRoleOrder(JSON.stringify({ order: [...within, "one-more"] }))).toBeUndefined();

    expect(planStoredRoleOrder(null, JSON.stringify({ order: [...within, "one-more"] }))).toEqual({ removeLegacy: true });
  });

  /** Scenario: a repeated role id is kept once, at its first position. */
  it("drops duplicates, keeping the first occurrence", () => {
    expect(parseStoredRoleOrder(JSON.stringify({ order: ["builder", "planner", "builder", "planner"] }))).toEqual(["builder", "planner"]);
  });

  /** Scenario: numbers, nulls and objects inside the order are dropped and the string entries kept. */
  it("drops non-string entries", () => {
    expect(parseStoredRoleOrder(JSON.stringify({ order: [1, "builder", null, { id: "x" }, ["planner"], "planner"] }))).toEqual(["builder", "planner"]);
  });

  /** Scenario: a valid legacy order is written to the new key in normalized form and the legacy key removed. */
  it("migrates a valid legacy value in normalized form", () => {
    const plan = planStoredRoleOrder(null, JSON.stringify({ order: ["builder", 7, "planner", "builder"], extra: true }));

    expect(plan).toEqual({ order: ["builder", "planner"], write: JSON.stringify({ order: ["builder", "planner"] }), removeLegacy: true });
  });

  /** Scenario: with nothing stored under either key there is nothing to write or remove. */
  it("does nothing when neither key holds a value", () => {
    expect(planStoredRoleOrder(null, null)).toEqual({ removeLegacy: false });
  });

  /** Scenario: the new key wins over the legacy one, and a malformed new value falls back without writing anything. */
  it("reads the new key first and validates it the same way", () => {
    expect(planStoredRoleOrder(JSON.stringify({ order: ["planner"] }), JSON.stringify({ order: ["builder"] }))).toEqual({ order: ["planner"], removeLegacy: false });
    expect(planStoredRoleOrder(JSON.stringify({ order: "abc" }), null)).toEqual({ order: undefined, removeLegacy: false });
  });

  /** Scenario: the rendered order keeps only ids of existing profiles, so it is never longer than the profile list. */
  it("bounds the rendered order by the profiles that exist", () => {
    expect(roleOrderFor(["ghost", "builder", "phantom", "planner"], ["planner", "builder", "reviewer"])).toEqual(["builder", "planner"]);
    expect(roleOrderFor(["ghost"], ["planner", "builder"])).toEqual(["planner", "builder"]);
    expect(roleOrderFor([], [])).toEqual([]);
  });
});
