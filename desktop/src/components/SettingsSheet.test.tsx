import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { DEFAULT_DESKTOP_SETTINGS } from "../lib/bridge";
import type { SettingsPanelProps, SettingsSection } from "../lib/settingsContract";
import { SETTINGS_SECTIONS } from "../lib/settingsRegistry";
import { SettingsSheet } from "./SettingsSheet";

/**
 * The section column's two states (PRD #803 M3).
 *
 * These render the sheet directly rather than through `ControlDeck`, because
 * the property under test is about the registry's *length* and the real
 * registry has exactly one row. Passing stub sections is what makes the
 * two-section layout reachable today: without it, "the column comes back when a
 * second section lands" is a claim no test can check until #741 or #802 lands,
 * and whoever writes that section has to trust a comment.
 *
 * The stubs are deliberately not the real Appearance panel — a section is an
 * `id`, a `label`, an icon and a component, and nothing about the column knows
 * or cares what a panel renders.
 */
function stubSection(id: string, label: string): SettingsSection {
  return {
    id,
    label,
    icon: () => <svg data-testid={`icon-${id}`} />,
    component: ({ settings }: SettingsPanelProps) => (
      <div data-testid={`stub-body-${id}`}>{label} panel, mode {settings.appearance.mode}</div>
    ),
  };
}

function renderSheet(sections?: SettingsSection[]) {
  const onClose = vi.fn();
  const onSave = vi.fn();
  render(
    <SettingsSheet
      open
      onClose={onClose}
      settings={DEFAULT_DESKTOP_SETTINGS}
      onSave={onSave}
      loaded
      mode="live"
      path="/home/dev/.config/dot-agent-deck/desktop.toml"
      sections={sections}
    />,
  );
  return { onClose, onSave };
}

describe("SettingsSheet section column", () => {
  it("renders a column of sections, and switches panels, once there are two", () => {
    renderSheet([stubSection("alpha", "Alpha"), stubSection("beta", "Beta")]);

    const nav = screen.getByRole("navigation", { name: "Settings sections" });
    const entries = within(nav).getAllByRole("button");
    expect(entries).toHaveLength(2);
    expect(entries[0]).toHaveTextContent("Alpha");
    expect(entries[1]).toHaveTextContent("Beta");
    expect(screen.getByTestId("settings-layout")).not.toHaveClass("is-single");

    // The first section is active, and its panel is the one rendered.
    expect(screen.getByTestId("settings-section-alpha")).toHaveAttribute("aria-current", "page");
    expect(screen.getByTestId("settings-panel-alpha")).toBeVisible();
    expect(screen.getByTestId("stub-body-alpha")).toBeVisible();
    expect(screen.queryByTestId("stub-body-beta")).not.toBeInTheDocument();

    // Choosing the second one moves both the selection and the panel.
    fireEvent.click(screen.getByTestId("settings-section-beta"));
    expect(screen.getByTestId("settings-section-beta")).toHaveAttribute("aria-current", "page");
    expect(screen.getByTestId("settings-section-alpha")).not.toHaveAttribute("aria-current");
    expect(screen.getByTestId("settings-panel-beta")).toBeVisible();
    expect(screen.queryByTestId("stub-body-alpha")).not.toBeInTheDocument();
  });

  it("drops the column entirely with one section and renders the panel full width", () => {
    renderSheet([stubSection("alpha", "Alpha")]);

    // No column, and no nav entry to click — a one-row list beside a panel
    // reads as unfinished work, so the panel takes the whole sheet instead.
    expect(screen.queryByRole("navigation", { name: "Settings sections" })).not.toBeInTheDocument();
    expect(screen.queryByTestId("settings-section-alpha")).not.toBeInTheDocument();
    expect(screen.getByTestId("settings-layout")).toHaveClass("is-single");

    // The panel itself is unaffected: same registry row, same component.
    expect(screen.getByTestId("settings-panel-alpha")).toBeVisible();
    expect(screen.getByTestId("stub-body-alpha")).toBeVisible();
  });

  it("collapses for the real registry today, because it has one section", () => {
    // The bridge between the two tests above and what a user actually sees.
    // When #741 or #802 adds a row this flips on its own and the assertion
    // becomes the one in the two-section test — nothing here has to change.
    expect(SETTINGS_SECTIONS).toHaveLength(1);
    renderSheet();
    expect(screen.getByTestId("settings-layout")).toHaveClass("is-single");
    expect(screen.queryByRole("navigation", { name: "Settings sections" })).not.toBeInTheDocument();
    expect(screen.getByTestId(`settings-panel-${SETTINGS_SECTIONS[0].id}`)).toBeVisible();
    expect(screen.getByRole("group", { name: "Appearance" })).toBeVisible();
  });

  it("says there is nothing to show if the registry is ever emptied", () => {
    renderSheet([]);
    expect(screen.queryByRole("navigation", { name: "Settings sections" })).not.toBeInTheDocument();
    expect(screen.getByText("No settings sections are registered.")).toBeVisible();
  });
});
