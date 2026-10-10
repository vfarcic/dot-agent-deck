import { describe, expect, it } from "vitest";
import { formatDisk, formatGib, formatLoad, formatSampleAge, hostRoleLabel } from "./hostMetrics";

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

  /** Scenario: the three fixed roles read as their labels, and a role a newer daemon adds keeps its own name. */
  it("labels roles and passes unknown ones through", () => {
    expect(hostRoleLabel("working_root")).toBe("Working root");
    expect(hostRoleLabel("worktree_parent")).toBe("Worktree parent");
    expect(hostRoleLabel("temp_root")).toBe("Temp root");
    expect(hostRoleLabel("scratch")).toBe("scratch");
  });
});
