/**
 * The theme dimension of the docs screenshots (PR #2491): every image of a
 * changed screen comes in a light and a dark version. Pure, so vitest checks
 * it (`themes.test.ts`) without a browser; `support.ts` reads the run's
 * environment through it.
 */

/** The two themes a screenshot is taken in. */
export type Theme = "light" | "dark";

/**
 * `paired` writes `<scenario>-<client>-<theme>.png`, one file per theme;
 * `single` writes `<scenario>-<client>.png` in the run's one theme, the names
 * the landing page and `docs/installation.md` reference.
 */
export type Naming = "paired" | "single";

/** `DAD_DOCS_SCREENSHOTS_THEMES` (`THEMES_ENV` in `xtask/screenshots/src/lib.rs`), comma-separated; unset or empty is both. */
export function parseThemes(raw: string | undefined): Theme[] {
  if (raw === undefined || raw.trim() === "") return ["light", "dark"];
  const themes = raw.split(",").map((theme) => theme.trim());
  for (const theme of themes) {
    if (theme !== "light" && theme !== "dark") throw new Error(`DAD_DOCS_SCREENSHOTS_THEMES names an unknown theme: ${JSON.stringify(theme)}`);
  }
  return [...new Set(themes as Theme[])];
}

/** `DAD_DOCS_SCREENSHOTS_NAMING` (`NAMING_ENV`); unset or empty is `paired`, and `single` needs exactly one theme. */
export function parseNaming(raw: string | undefined, themes: readonly Theme[]): Naming {
  if (raw === undefined || raw === "" || raw === "paired") return "paired";
  if (raw !== "single") throw new Error(`DAD_DOCS_SCREENSHOTS_NAMING is not paired or single: ${JSON.stringify(raw)}`);
  if (themes.length !== 1) throw new Error("DAD_DOCS_SCREENSHOTS_NAMING=single needs exactly one theme");
  return "single";
}

/** The file one scenario, client and theme is written to — the same names `Scenario::themed_image_file` expects. */
export function imageName(scenario: string, client: "tui" | "desktop", theme: Theme, naming: Naming): string {
  return naming === "single" ? `${scenario}-${client}.png` : `${scenario}-${client}-${theme}.png`;
}
