import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ZoomPanel } from "./ZoomPanel";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "../lib/bridge";
import { ZOOM_LEVELS } from "../lib/zoom";
import type { RuntimeMode } from "../types";

function renderPanel(overrides: Partial<DesktopSettingsDto> = {}, mode: RuntimeMode = "live", saveError?: string) {
  const onSave = vi.fn();
  render(
    <ZoomPanel
      settings={{ ...DEFAULT_DESKTOP_SETTINGS, ...overrides }}
      onSave={onSave}
      saveError={saveError}
      mode={mode}
    />,
  );
  return { onSave };
}

describe("ZoomPanel", () => {
  it("shows the stored level, which no other surface in the app displays", () => {
    renderPanel({ zoom: { level: 1.5 } });
    expect(screen.getByLabelText("Zoom")).toHaveValue("1.5");
    expect(screen.getByRole("option", { name: "150%" })).toBeInTheDocument();
  });

  it("offers every ladder level as a whole percent, in order", () => {
    renderPanel();
    const options = screen.getAllByRole("option").map((option) => option.textContent);
    expect(options).toEqual(["75%", "90%", "100%", "110%", "125%", "150%", "175%", "200%", "250%", "300%"]);
    expect(options).toHaveLength(ZOOM_LEVELS.length);
  });

  /**
   * The panel's only channel is `onSave`, and it must send the WHOLE document.
   * A panel that sent just its own section would drop every section this build's
   * UI has not loaded — the guarantee `settingsContract.ts` states and the
   * reason the appearance mode is asserted here rather than ignored.
   */
  it("saves the whole document with only its own section replaced", () => {
    const { onSave } = renderPanel({ appearance: { mode: "dark" }, zoom: { level: 1 } });
    fireEvent.change(screen.getByLabelText("Zoom"), { target: { value: "2" } });
    expect(onSave).toHaveBeenCalledWith({
      ...DEFAULT_DESKTOP_SETTINGS,
      appearance: { mode: "dark" },
      zoom: { level: 2 },
    });
  });

  // There is deliberately no test that drives the change handler with an
  // off-ladder value. `fireEvent.change` cannot set a `<select>` to a value
  // that is not one of its options, so such a test asserts nothing — the
  // handler's `clampZoom` is unreachable through this control by construction,
  // and the snapping itself is covered in `lib/zoom.test.ts`.
  it("reads an off-ladder stored level as the rung it behaves as", () => {
    renderPanel({ zoom: { level: 1.3 } });
    expect(screen.getByLabelText("Zoom")).toHaveValue("1.25");
  });

  /**
   * A browser has its own zoom, and `Ctrl -` there is not reliably
   * preventable — so `useZoom` does not bind the keys in the preview and this
   * control has nothing to drive. Disabling it and saying where the zoom
   * actually is beats a control that persists a level and does nothing.
   */
  it("disables itself in the browser preview and says where the zoom is", () => {
    renderPanel({}, "fixture");
    expect(screen.getByLabelText("Zoom")).toBeDisabled();
    expect(screen.getByText(/browser's own zoom/i)).toBeVisible();
  });

  it("carries no hint in the packaged app, where the control works", () => {
    renderPanel({}, "live");
    expect(screen.getByLabelText("Zoom")).toBeEnabled();
    expect(screen.queryByText(/browser's own zoom/i)).not.toBeInTheDocument();
  });

  // The choice stays applied; what failed is making it survive a restart, and
  // saying so is more use than silently reverting a choice the user just made.
  //
  // The sentence is composed by `useDesktopSettings` and rendered verbatim here
  // (issue #1072), so the test passes the composed string rather than a bare
  // cause — the panel no longer knows which of the two conditions it is showing.
  it("reports a failed save as an alert without reverting the choice", () => {
    renderPanel({ zoom: { level: 2 } }, "live", "This change is applied, but saving it failed, so it will not survive a restart. disk full");
    expect(screen.getByRole("alert")).toHaveTextContent(/will not survive a restart\. disk full/i);
    expect(screen.getByLabelText("Zoom")).toHaveValue("2");
  });

  /**
   * Scenario (issue #1072): the settings document on disk cannot be read, so
   * the app is on defaults and every save is refused. The panel must render
   * that sentence as it stands — a "saving it failed" preamble would be a claim
   * about a save the user has not made.
   */
  it("renders an unreadable-document message without a failed-save preamble", () => {
    renderPanel({}, "live", "The desktop settings file cannot be read: line 3, column 9 is not valid settings. This session is using default settings, and nothing will be saved over the file until it is fixed or removed.");
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent("line 3, column 9");
    expect(alert).not.toHaveTextContent("saving it failed");
  });

  // `docs/develop/desktop-gui.md`'s heading rule: a section heading is chrome
  // for telling sections apart, and the section list beside the panel already
  // names this one. Pinned the same way the Appearance panel's is.
  it("carries no heading of its own", () => {
    renderPanel();
    expect(screen.queryByRole("heading")).not.toBeInTheDocument();
  });
});
