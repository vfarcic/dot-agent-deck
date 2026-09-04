import { describe, expect, it } from "vitest";
import {
  applyZoomIntent,
  clampZoom,
  DEFAULT_ZOOM,
  formatZoom,
  stepZoom,
  ZOOM_LEVELS,
  zoomIntentFromKey,
  type ZoomKeyEvent,
} from "./zoom";

/** A keydown with no modifiers, so each test names only what it is about. */
function key(overrides: Partial<ZoomKeyEvent> & { key: string }): ZoomKeyEvent {
  return { metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...overrides };
}

describe("ZOOM_LEVELS", () => {
  it("is ascending, contains the default, and holds both measured bounds", () => {
    expect([...ZOOM_LEVELS]).toEqual([...ZOOM_LEVELS].sort((a, b) => a - b));
    expect(ZOOM_LEVELS).toContain(DEFAULT_ZOOM);
    // Both ends are load-bearing, not decorative: 3.0 is the last level that
    // keeps 1024 / level above the stylesheet's own `min-width: 320px`, and
    // 0.75 is where the 6.8px agent footer reaches 5.1px. A change to either
    // end should have to come past this assertion and the reasoning in the
    // module docs.
    expect(ZOOM_LEVELS[0]).toBe(0.75);
    expect(ZOOM_LEVELS[ZOOM_LEVELS.length - 1]).toBe(3);
    expect(1024 / 3).toBeGreaterThan(320);
  });
});

describe("clampZoom", () => {
  it("keeps a level that is already on the ladder", () => {
    for (const level of ZOOM_LEVELS) expect(clampZoom(level)).toBe(level);
  });

  it("snaps an off-ladder level to the nearest rung", () => {
    expect(clampZoom(1.3)).toBe(1.25);
    expect(clampZoom(1.4)).toBe(1.5);
    expect(clampZoom(2.9)).toBe(3);
  });

  /**
   * A value exactly between two rungs resolves to the LOWER one, because the
   * nearest-rung search uses a strict `<` over an ascending ladder so the first
   * candidate wins a tie.
   *
   * Pinned because `ZoomLevel::snap` in `desktop/src-tauri/src/settings.rs` is a
   * second implementation of this, and a tie is the one input where the two
   * could differ while both looked correct — `<=` on either side would resolve
   * upward. Disagreement means the app launches at one level (Rust snapped the
   * document) while the Settings row reads another (this snapped it), which is
   * the specific failure the duplicated-ladder comments warn about.
   */
  it("resolves an exact tie downward, as the Rust copy does", () => {
    expect(clampZoom(1.05)).toBe(1.0);
    expect(clampZoom(0.825)).toBe(0.75);
    expect(clampZoom(2.25)).toBe(2.0);
  });

  it("saturates rather than failing outside the ladder", () => {
    expect(clampZoom(0.01)).toBe(0.75);
    expect(clampZoom(99)).toBe(3);
    expect(clampZoom(-5)).toBe(0.75);
  });

  // The whole reason `clampZoom` guards with `Number.isFinite` rather than a
  // range check. `NaN` is the case that bites: every comparison against it is
  // false, so an unguarded nearest-rung search returns the FIRST level — 75% —
  // and a corrupt `desktop.toml` would silently shrink the app instead of
  // reading as the default.
  it("answers the default for anything that is not a usable number", () => {
    expect(clampZoom(Number.NaN)).toBe(DEFAULT_ZOOM);
    expect(clampZoom(Number.POSITIVE_INFINITY)).toBe(DEFAULT_ZOOM);
    expect(clampZoom(Number.NEGATIVE_INFINITY)).toBe(DEFAULT_ZOOM);
    expect(clampZoom(undefined)).toBe(DEFAULT_ZOOM);
    expect(clampZoom(null)).toBe(DEFAULT_ZOOM);
    expect(clampZoom("1.5")).toBe(DEFAULT_ZOOM);
    expect(clampZoom({})).toBe(DEFAULT_ZOOM);
  });
});

describe("stepZoom", () => {
  it("walks one rung in each direction", () => {
    expect(stepZoom(1, "in")).toBe(1.1);
    expect(stepZoom(1, "out")).toBe(0.9);
    expect(stepZoom(1.25, "in")).toBe(1.5);
    expect(stepZoom(1.25, "out")).toBe(1.1);
  });

  // Saturating rather than wrapping. Holding the key down at the top must stay
  // at the top; wrapping to 75% would be the worst possible response to "make
  // this bigger".
  it("saturates at both ends instead of wrapping", () => {
    expect(stepZoom(3, "in")).toBe(3);
    expect(stepZoom(0.75, "out")).toBe(0.75);
  });

  it("steps from the nearest rung when handed an off-ladder level", () => {
    expect(stepZoom(1.3, "in")).toBe(1.5);
    expect(stepZoom(Number.NaN, "in")).toBe(1.1);
  });

  it("reaches both ends by repeated stepping and then stops", () => {
    let level = DEFAULT_ZOOM;
    for (let i = 0; i < ZOOM_LEVELS.length * 2; i += 1) level = stepZoom(level, "in");
    expect(level).toBe(3);
    for (let i = 0; i < ZOOM_LEVELS.length * 2; i += 1) level = stepZoom(level, "out");
    expect(level).toBe(0.75);
  });
});

describe("applyZoomIntent", () => {
  it("resets to the default from either direction", () => {
    expect(applyZoomIntent(3, "reset")).toBe(DEFAULT_ZOOM);
    expect(applyZoomIntent(0.75, "reset")).toBe(DEFAULT_ZOOM);
    expect(applyZoomIntent(DEFAULT_ZOOM, "reset")).toBe(DEFAULT_ZOOM);
  });

  it("defers to stepZoom for the two directional intents", () => {
    expect(applyZoomIntent(1, "in")).toBe(stepZoom(1, "in"));
    expect(applyZoomIntent(1, "out")).toBe(stepZoom(1, "out"));
  });
});

describe("formatZoom", () => {
  it("renders whole percents for every ladder level", () => {
    expect(ZOOM_LEVELS.map(formatZoom)).toEqual([
      "75%", "90%", "100%", "110%", "125%", "150%", "175%", "200%", "250%", "300%",
    ]);
  });

  it("shows the rung an off-ladder level behaves as, not the level itself", () => {
    expect(formatZoom(1.3)).toBe("125%");
    expect(formatZoom(Number.NaN)).toBe("100%");
  });
});

describe("zoomIntentFromKey", () => {
  it("reads the unshifted characters under either modifier", () => {
    expect(zoomIntentFromKey(key({ key: "=", metaKey: true }))).toBe("in");
    expect(zoomIntentFromKey(key({ key: "=", ctrlKey: true }))).toBe("in");
    expect(zoomIntentFromKey(key({ key: "-", metaKey: true }))).toBe("out");
    expect(zoomIntentFromKey(key({ key: "-", ctrlKey: true }))).toBe("out");
    expect(zoomIntentFromKey(key({ key: "0", metaKey: true }))).toBe("reset");
    expect(zoomIntentFromKey(key({ key: "0", ctrlKey: true }))).toBe("reset");
  });

  // `Cmd Shift =` is how a lot of people actually type "zoom in", and it is
  // what a keyboard with `+` as a shifted character reports.
  it("reads the shifted characters, which is what `+` and `_` are", () => {
    expect(zoomIntentFromKey(key({ key: "+", metaKey: true, shiftKey: true }))).toBe("in");
    expect(zoomIntentFromKey(key({ key: "_", metaKey: true, shiftKey: true }))).toBe("out");
  });

  it("reads the numpad by position, which is the one thing `key` cannot carry", () => {
    expect(zoomIntentFromKey({ key: "Unidentified", code: "NumpadAdd", ctrlKey: true })).toBe("in");
    expect(zoomIntentFromKey({ key: "Insert", code: "Numpad0", ctrlKey: true })).toBe("reset");
    expect(zoomIntentFromKey({ key: "End", code: "NumpadSubtract", ctrlKey: true })).toBe("out");
  });

  // The inverse of the numpad case, and the reason the character keys are NOT
  // matched by `code`. `code: "Equal"` is the physical US `=` position, which
  // carries `´` on a German layout — binding it would claim a key the user
  // never associates with zoom, and `event.key` already covers every layout
  // where `=` is typeable at all.
  it("ignores the physical positions of the character keys", () => {
    expect(zoomIntentFromKey({ key: "´", code: "Equal", ctrlKey: true })).toBeUndefined();
    expect(zoomIntentFromKey({ key: "ß", code: "Minus", ctrlKey: true })).toBeUndefined();
    expect(zoomIntentFromKey({ key: "à", code: "Digit0", ctrlKey: true })).toBeUndefined();
  });

  it("requires a modifier, so plain typing is never a zoom", () => {
    expect(zoomIntentFromKey(key({ key: "-" }))).toBeUndefined();
    expect(zoomIntentFromKey(key({ key: "=" }))).toBeUndefined();
    expect(zoomIntentFromKey(key({ key: "0" }))).toBeUndefined();
    expect(zoomIntentFromKey(key({ key: "-", shiftKey: true }))).toBeUndefined();
  });

  // `Alt Cmd -` and its neighbours belong to the window manager on macOS.
  it("declines when Alt is held", () => {
    expect(zoomIntentFromKey(key({ key: "-", metaKey: true, altKey: true }))).toBeUndefined();
    expect(zoomIntentFromKey({ key: "+", code: "NumpadAdd", ctrlKey: true, altKey: true })).toBeUndefined();
  });

  it("declines the keys the rest of the app already binds", () => {
    for (const bound of ["k", "K", "Escape", "?", "1", "2", "3", "4", "j"]) {
      expect(zoomIntentFromKey(key({ key: bound, metaKey: true }))).toBeUndefined();
      expect(zoomIntentFromKey(key({ key: bound, ctrlKey: true }))).toBeUndefined();
    }
  });

  // Issue #826 is `App.tsx`'s handler throwing on a keydown whose target is not
  // an element, because it casts `event.target` to `HTMLElement` and calls
  // `.matches` on it. This matcher never reads `target`, so it cannot reproduce
  // that — pinned here so a later "tidy-up" that starts consulting the target
  // has to come past a test that says why not to.
  it("never consults the event target", () => {
    const withHostileTarget = { key: "=", ctrlKey: true, target: window } as unknown as ZoomKeyEvent;
    expect(() => zoomIntentFromKey(withHostileTarget)).not.toThrow();
    expect(zoomIntentFromKey(withHostileTarget)).toBe("in");
  });

  it("survives a real KeyboardEvent, not only a hand-built object", () => {
    const event = new KeyboardEvent("keydown", { key: "=", code: "Equal", ctrlKey: true });
    expect(zoomIntentFromKey(event)).toBe("in");
    expect(zoomIntentFromKey(new KeyboardEvent("keydown", { key: "a", ctrlKey: true }))).toBeUndefined();
  });
});
