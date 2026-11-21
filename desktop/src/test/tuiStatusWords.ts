import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect } from "vitest";

/**
 * Issue #2411: the TUI's word for each status the daemon reports, keyed by the
 * wire word the desktop receives — read from the Rust source rather than
 * restated, so the desktop cannot drift from it again. `session_status_name`
 * (the desktop crate's dto) gives each `SessionStatus` its wire word;
 * `status_style` (`src/ui.rs`) gives the word the TUI's card shows for it.
 * Every value is lower-cased, the case the desktop's labels carry before CSS
 * capitalises them.
 */
export function tuiStatusWords(): Record<string, string> {
  const dto = readFileSync(resolve("src-tauri/src/dto.rs"), "utf8");
  const ui = readFileSync(resolve("../src/ui.rs"), "utf8");
  const body = (source: string, fn: string) => {
    const start = source.indexOf(`fn ${fn}(`);
    expect(start, `fn ${fn} not found`).toBeGreaterThan(-1);
    return source.slice(start, source.indexOf("\n}\n", start));
  };
  const wire = new Map([...body(dto, "session_status_name").matchAll(/SessionStatus::(\w+) => "(\w+)"/g)].map(([, variant, word]) => [variant, word]));
  const shown = new Map([...body(ui, "status_style").matchAll(/SessionStatus::(\w+) => \("([^"]+)"/g)].map(([, variant, word]) => [variant, word]));
  expect(wire.size, "session_status_name lists no variant").toBeGreaterThan(0);
  expect([...shown.keys()].sort()).toEqual([...wire.keys()].sort());
  return Object.fromEntries([...wire].map(([variant, word]) => [word, shown.get(variant)!.toLowerCase()]));
}
