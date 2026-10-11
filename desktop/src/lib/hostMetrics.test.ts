import { describe, expect, it } from "vitest";
import { fixedLikeRust, formatDisk, formatGib, formatLoad, formatSampleAge, HOST_MAX_SHOWN_AGE_MS, hostRoleLabel } from "./hostMetrics";

const GiB = 1024 ** 3;

describe("host metrics wording (PRD #1258 M4)", () => {
  /** Scenario: whole GiB values drop the trailing `.0`, fractional ones keep one decimal, and an absent figure reads `unknown` rather than zero. */
  it("formats GiB without inventing zeros", () => {
    expect(formatGib(128 * GiB)).toBe("128 GiB");
    expect(formatGib(7.5 * GiB)).toBe("7.5 GiB");
    expect(formatGib(undefined)).toBe("unknown");
    expect(formatDisk(undefined, 16 * GiB)).toBe("unknown free of 16 GiB total");
  });

  /** Scenario: load and core count are each independently `unknown`, and the age is the daemon's own milliseconds. */
  it("formats load, cores and age the way the TUI overlay does", () => {
    expect(formatLoad(0.75, 8)).toBe("0.75 across 8 cores");
    expect(formatLoad(undefined, undefined)).toBe("unknown across unknown cores");
    expect(formatSampleAge(1500)).toBe("1500 ms");
  });

  /** Scenario: numbers are written as the TUI's Rust `format!` writes them — exact ties to the even neighbour, negative zero signed, every digit from 1e21 up — while values that are not exact ties round as `toFixed` does. */
  it("writes numbers exactly as the TUI's Rust formatter does", () => {
    // Exact ties, which `toFixed` rounds away from zero.
    expect(fixedLikeRust(0.125, 2)).toBe("0.12");
    expect(fixedLikeRust(0.375, 2)).toBe("0.38");
    expect(fixedLikeRust(0.625, 2)).toBe("0.62");
    expect(fixedLikeRust(0.875, 2)).toBe("0.88");
    expect(fixedLikeRust(1.25, 1)).toBe("1.2");
    expect(fixedLikeRust(2.75, 1)).toBe("2.8");
    expect(fixedLikeRust(-0.125, 2)).toBe("-0.12");
    // Not exact ties in binary, so both formatters agree with `toFixed`.
    expect(fixedLikeRust(1.005, 2)).toBe("1.00");
    expect(fixedLikeRust(0.285, 2)).toBe("0.28");
    expect(fixedLikeRust(0.35, 1)).toBe("0.3");
    // Signs and magnitudes `toFixed` writes differently.
    expect(fixedLikeRust(-0, 2)).toBe("-0.00");
    expect(fixedLikeRust(-0.001, 2)).toBe("-0.00");
    expect(fixedLikeRust(1e21, 2)).toBe("1000000000000000000000.00");
    expect(fixedLikeRust(2 ** 64 / GiB, 1)).toBe("17179869184.0");
    expect(formatGib(1.25 * GiB)).toBe("1.2 GiB");
    expect(formatLoad(1 / 8, 8)).toBe("0.12 across 8 cores");
    expect(formatSampleAge(2 ** 64)).toBe(`${HOST_MAX_SHOWN_AGE_MS} ms`);
  });

  /** Scenario: the three fixed roles read as their labels, and a role a newer daemon adds keeps its own name. */
  it("labels roles and passes unknown ones through", () => {
    expect(hostRoleLabel("working_root")).toBe("Working root");
    expect(hostRoleLabel("worktree_parent")).toBe("Worktree parent");
    expect(hostRoleLabel("temp_root")).toBe("Temp root");
    expect(hostRoleLabel("scratch")).toBe("scratch");
  });
});
