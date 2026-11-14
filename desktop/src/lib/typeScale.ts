/**
 * The terminal's font size, in CSS pixels, and the reference the app's own type
 * scale is built on (issue #2312). `styles.css` declares `--text-base` as this
 * same value and `typeScale.test.ts` fails if the two differ, so the agent's
 * output and the app's ordinary text cannot drift apart.
 */
export const TERMINAL_FONT_SIZE = 13.5;
