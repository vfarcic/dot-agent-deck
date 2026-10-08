import { beforeEach, describe, expect, it } from "vitest";
import {
  clearDashboardFilter,
  currentDashboardFilter,
  DASHBOARD_FILTER_TEXT_MAX,
  readStoredDashboardFilter,
  setDashboardFilter,
} from "./dashboardFilter";

beforeEach(() => {
  window.sessionStorage.clear();
  setDashboardFilter(clearDashboardFilter());
});

describe("dashboard filter storage", () => {
  /// Scenario: set a filter whose search text is far longer than the cap, by typing or by voice. The window session keeps only the first DASHBOARD_FILTER_TEXT_MAX characters, both in storage and in the filter read back.
  it("caps the search text it stores", () => {
    const long = "a".repeat(DASHBOARD_FILTER_TEXT_MAX * 3);
    setDashboardFilter({ ...clearDashboardFilter(), text: long });
    expect(currentDashboardFilter().text).toBe("a".repeat(DASHBOARD_FILTER_TEXT_MAX));
    const raw = Array.from({ length: window.sessionStorage.length }, (_, at) => window.sessionStorage.getItem(window.sessionStorage.key(at) ?? "") ?? "").join("");
    expect(raw).not.toContain("a".repeat(DASHBOARD_FILTER_TEXT_MAX + 1));
  });

  /// Scenario: read back a stored filter whose text was written longer than the cap, by an older build or by hand. The text comes back cut to the cap and the other facets are kept.
  it("caps the search text it reads back", () => {
    const raw = JSON.stringify({ kinds: ["dispatcher"], statuses: [], agentTypes: [], daemonIds: [], text: "b".repeat(DASHBOARD_FILTER_TEXT_MAX + 50) });
    const filter = readStoredDashboardFilter(raw);
    expect(filter.text).toHaveLength(DASHBOARD_FILTER_TEXT_MAX);
    expect(filter.kinds).toEqual(["dispatcher"]);
  });
});
