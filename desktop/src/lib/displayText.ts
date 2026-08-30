/**
 * Display-only sanitising and bounding for daemon-supplied text.
 *
 * Every string the overview renders — display names, working directories,
 * orchestration and role names, active tool names and details, the daemon's own
 * status message — originates in an agent process, so it is attacker-influenced
 * by prompt injection from any file an agent reads. The daemon-side scrub does
 * NOT close this: `src/daemon_client.rs` calls `strip_control_chars`, whose
 * `char::is_control` test covers general category `Cc` only, so the bidi
 * formatting codepoints (category `Cf`) reach the webview intact.
 *
 * This module is the render seam's own defence, and it is deliberately a
 * DISPLAY copy: grouping, sorting and identity keys keep the raw values, so
 * sanitising can never merge two agents or lose one. That is also why it lives
 * here rather than in the Rust DTO — sanitising upstream would corrupt the keys.
 *
 * The policy mirrors `src/untrusted_text.rs::strip_control_and_bidi`
 * character for character. Keep it that way: that module's own header records
 * that the bug class came from two copies of the policy drifting apart.
 */

/**
 * Every character the overview refuses to render, enumerated rather than
 * approximated because one missed override is all a spoof needs:
 *
 * - `\u0000`–`\u001F` and `\u007F`–`\u009F` — the C0 and C1 controls, exactly
 *   what Rust's `char::is_control` (category `Cc`) covers. NUL, DEL, newline,
 *   and the `\u001B` that starts every ANSI escape are all in here.
 * - the bidi formatting / override codepoints `is_bidi_format_char` names:
 *   `U+202A`–`U+202E` (LRE, RLE, PDF, LRO, RLO), `U+2066`–`U+2069` (LRI, RLI,
 *   FSI, PDI), `U+200E` (LRM), `U+200F` (RLM) and `U+061C` (ALM). These are
 *   category `Cf`, so no "is this a control character" test catches them — and
 *   a single `U+202E` in a display name visually reverses that name and can
 *   swallow the inline siblings printed after it, the COORDINATOR badge
 *   included, on a screen whose entire purpose is telling one agent from
 *   another.
 *
 * Zero-width joiners and the other default-ignorable format characters
 * (`U+200B` ZWSP, `U+200C` ZWNJ, `U+200D` ZWJ, `U+FEFF`) are deliberately NOT
 * stripped. Three reasons: they cannot reorder or reverse text, so they do not
 * produce the spoof this filter exists to stop; they are load-bearing in
 * legitimate names — ZWJ builds emoji sequences and ZWNJ changes the word in
 * Persian, Arabic and several Indic scripts, so stripping them corrupts real
 * data; and widening the policy beyond the Rust one would make the TUI and the
 * desktop disagree about the same daemon string, which is the divergence
 * `untrusted_text.rs` was written to end. The residual — a name made entirely
 * of invisible characters rendering as a blank cell — is bounded by the length
 * clamps below and by the row still carrying status, CLI and tool columns.
 */
const UNSAFE_DISPLAY_CHARS = /[\u0000-\u001F\u007F-\u009F\u061C\u200E\u200F\u202A-\u202E\u2066-\u2069]/g;

/**
 * Character budgets. `DesktopAgentDto` is a TypeScript *assertion* about a shape
 * the daemon supplies, not a validated one, so the frontend does not trust the
 * lengths it claims: every rendered copy is clamped before React sees it.
 */
export const DISPLAY_LIMITS = {
  /**
   * Names: display name, orchestration name, group title, role name. 128
   * matches the daemon's own `DISPLAY_NAME_MAX_LEN` (`src/agent_pty.rs:203`),
   * which is 128 *bytes* — so any name the daemon itself would accept passes
   * through this untouched, and only something that never went through its
   * rename validation is ever clamped.
   */
  name: 128,
  /** Working directories and socket paths. Long enough for a real worktree path. */
  path: 120,
  /** Tool names are identifiers (`edit`, `bash`, `grep`), never prose. */
  toolName: 32,
  /**
   * The active tool's detail. For Bash/shell events this is the tool's first
   * COMMAND LINE (`src/hook.rs:182-232`), so an unbounded copy is both a
   * disclosure risk in screenshots and demo recordings and unreadable at
   * fifteen rows. Short on purpose; the full value is one hover away.
   */
  toolDetail: 60,
  /** Any `title` attribute: generous, because hover is where the full value lives. */
  title: 512,
  /** The daemon's own connection message. */
  message: 240,
} as const;

/** Drop every control and bidi character. Nothing else is touched. */
export function sanitizeText(value: string): string {
  return value.replace(UNSAFE_DISPLAY_CHARS, "");
}

/**
 * Clamp to `max` characters, counted by code point so a surrogate pair is never
 * split into a lone half. An elision marker is appended when anything was cut,
 * so a truncated value never passes itself off as complete.
 */
export function clampText(value: string, max: number): string {
  const chars = Array.from(value);
  return chars.length <= max ? value : `${chars.slice(0, max).join("")}…`;
}

/** Sanitise, then clamp. The display copy of any daemon-supplied string. */
export function displayText(value: string, max: number): string {
  return clampText(sanitizeText(value), max);
}

/** The display copy for a `title` attribute — sanitised exactly like the text. */
export function displayTitle(value: string): string {
  return displayText(value, DISPLAY_LIMITS.title);
}

/**
 * Replace a leading home directory with `~`.
 *
 * The webview has no `$HOME` — reading the real one would mean a
 * `desktop/src-tauri/` change, which PRD #745 iteration 1 is not allowed to
 * make — so this matches the *shape* of a home directory rather than the
 * actual one: `/home/<user>`, `/Users/<user>`, `/root`, and the Windows
 * `C:\Users\<user>`. That is enough for both things this is for: the path gets
 * shorter, and a username stops appearing in screenshots and demo recordings.
 * The full path always stays in the row's `title`, so nothing is hidden — only
 * abbreviated.
 */
export function homeRelative(path: string): string {
  return path
    .replace(/^\/(?:home|Users)\/[^/]+(?=\/|$)/, "~")
    .replace(/^\/root(?=\/|$)/, "~")
    .replace(/^[A-Za-z]:[\\/]Users[\\/][^\\/]+(?=[\\/]|$)/, "~");
}

/** The display copy of a working directory: sanitised, home-relative, clamped. */
export function displayPath(path: string, max: number = DISPLAY_LIMITS.path): string {
  return clampText(homeRelative(sanitizeText(path)), max);
}

/**
 * A short label for a daemon. `AgentSession.daemonId` is the socket path, which
 * routinely embeds a uid or a username (`/run/user/1000/…`), and a group header
 * is not the place for either. The last path segment identifies the daemon
 * among the handful a machine runs; the full socket path stays on hover, and
 * the raw value stays the identity key.
 */
export function shortDaemonLabel(socketPath: string): string {
  const clean = sanitizeText(socketPath);
  const trimmed = clean.replace(/[\\/]+$/, "");
  const base = trimmed.slice(Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\")) + 1);
  return clampText(base || clean, DISPLAY_LIMITS.name);
}
