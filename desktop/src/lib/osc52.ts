/**
 * Issue #1705 — the text of an agent's OSC 52 set-clipboard request.
 *
 * An agent that turns on mouse reporting (Claude Code does) takes every drag in
 * its pane for itself, so xterm.js never holds a selection to copy. The agent
 * makes the selection and asks the terminal to copy it instead, with
 * `ESC ] 52 ; <targets> ; <base64> BEL` (or `ESC \`). xterm.js hands a
 * registered handler everything after `52;`, which is what this reads.
 *
 * WRITE ONLY. A `?` payload is a request to READ the clipboard back to the
 * agent, and it is never answered: anything the agent's output can make it
 * print could otherwise exfiltrate whatever the user last copied.
 *
 * Returns `undefined` — meaning "leave the clipboard as it is" — for anything
 * that is not a well-formed copy:
 *
 * - a target list holding a letter OSC 52 does not define (`c p q s 0-7`; an
 *   empty list is the default target and is accepted). Every valid target goes
 *   to the one system clipboard the app writes;
 * - a payload that is not base64, or whose bytes are not UTF-8 text;
 * - an empty payload. xterm itself clears the selection for one, but a
 *   terminal pane emptying the user's clipboard is a surprise nobody needs;
 * - text over {@link OSC52_MAX_TEXT_BYTES}. Dropped rather than truncated: a
 *   copy that silently lost its tail pastes as if it were whole.
 */
export const OSC52_MAX_TEXT_BYTES = 1024 * 1024;

const TARGETS = /^[cpqs0-7]*$/;

export function osc52CopyText(data: string): string | undefined {
  const separator = data.indexOf(";");
  if (separator < 0) return undefined;
  const targets = data.slice(0, separator);
  const payload = data.slice(separator + 1);
  if (!TARGETS.test(targets) || payload === "" || payload === "?") return undefined;
  // Every 4 base64 characters carry 3 bytes; refuse what cannot fit before
  // decoding it.
  if (payload.length > Math.ceil(OSC52_MAX_TEXT_BYTES / 3) * 4) return undefined;
  let binary: string;
  try {
    binary = atob(payload);
  } catch {
    return undefined;
  }
  if (binary.length === 0 || binary.length > OSC52_MAX_TEXT_BYTES) return undefined;
  const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0));
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return undefined;
  }
}
