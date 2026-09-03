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
 *
 * `displayActivity` and `displayUptime` at the bottom of the file are the same
 * seam for a daemon-supplied INSTANT rather than a string (PRD #745 M9, M11):
 * the daemon sends epoch milliseconds and the webview decides how they read, so
 * the relative wording, the rounding and the clock-skew rule all live here and
 * none of them is baked into the daemon's contract.
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
 * of invisible characters rendering as a blank cell — is closed by
 * `displayIdentity` below, which substitutes a visible label rather than by
 * stripping one more character. It was previously recorded as bounded by "the
 * row still carrying status, CLI and tool columns", which is false: CSS hides
 * the uptime, CLI and working-directory columns below 1180px and the tool
 * columns below 680px, so a narrow window leaves two same-status rows with
 * nothing to tell them apart.
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
  /**
   * The last user prompt (PRD #745 M8). Free-form operator text an agent's own
   * output can influence, and the longest thing on the screen: the daemon
   * bounds it at 64 KiB per agent (`daemon_client.rs`'s `MAX_FIRST_PROMPT_BYTES`),
   * which at fifteen rows is a megabyte of DOM text for a cell a reader sees
   * about sixty characters of.
   *
   * 160 is chosen against what the column can actually show. The prompt track
   * is the widest on the row and still fits roughly sixty characters at its
   * rendered size, so the ellipsis a reader sees is CSS's, not the clamp's —
   * the clamp exists for the pathological value, not the ordinary one. It sits
   * below `message` (240, which appears once per screen) because a prompt
   * appears on every row of a fifteen-row table, and well above `toolDetail`
   * (60) because a prompt is a sentence rather than an identifier and its first
   * clause is the part worth reading. The full value stays one hover away under
   * the `title` budget.
   */
  prompt: 160,
  /** Any `title` attribute: generous, because hover is where the full value lives. */
  title: 512,
  /** The daemon's own connection message. */
  message: 240,
  /**
   * Identity values that reach a DOM attribute, an IDREF or a React key —
   * `domIdentity` is the seam. Generous next to the others because these are
   * percent-encoded composites (`<kind>:<encoded id>`, `<encoded daemonId>:<encoded
   * agentId>`) and encoding can triple a path: a real socket path plus an agent
   * id lands around 60 characters, so nothing a healthy daemon reports is ever
   * clamped here. It exists for the malformed case, where the only bound today
   * is the 16 MiB protocol frame.
   */
  domIdentity: 160,
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
 * Characters that occupy no visual space when rendered: Unicode whitespace, the
 * default-ignorable format characters `UNSAFE_DISPLAY_CHARS` deliberately KEEPS
 * (`U+200B` ZWSP, `U+200C` ZWNJ, `U+200D` ZWJ, `U+2060` WJ, `U+FEFF`), the
 * variation selectors and the tag block.
 *
 * This set is deliberately WIDER than the stripped set, and that is not a
 * divergence from `src/untrusted_text.rs`: nothing here is removed from what
 * renders. It only answers "would a reader see anything?", so a character can
 * be listed as invisible AND still be rendered verbatim — which is exactly what
 * a ZWJ emoji sequence needs. Widening the *stripped* set is the thing that
 * would corrupt real names and make the TUI and the desktop disagree.
 */
const INVISIBLE_ONLY =
  /^[\s\u00AD\u034F\u115F\u1160\u17B4\u17B5\u180B-\u180E\u200B-\u200F\u2060-\u206F\u3164\uFE00-\uFE0F\uFEFF\uFFA0\u{E0000}-\u{E0FFF}]*$/u;

/**
 * True when `value` renders as nothing a reader can see — empty, whitespace, or
 * made entirely of invisible characters. A string containing ONE visible
 * character is not blank however much invisible padding surrounds it.
 */
export function rendersBlank(value: string): boolean {
  return INVISIBLE_ONLY.test(value);
}

/**
 * The display copy of an IDENTITY string — a display name, a group title —
 * falling back to `fallback` when the daemon's value renders as nothing at all.
 *
 * A name made entirely of retained default-ignorable characters is a spoofing
 * primitive on a screen whose whole purpose is telling one agent from another:
 * it renders as a blank cell, and two names differing only by such a character
 * render identically. The row's other columns do NOT rescue it — CSS hides
 * uptime, CLI and working directory below 1180px and the tool columns below
 * 680px, so a narrow window can leave two same-status rows genuinely
 * indistinguishable.
 *
 * The fix is a visible fallback rather than a wider filter: stripping ZWJ and
 * ZWNJ would corrupt emoji sequences and Persian, Arabic and Indic
 * orthography, and would put this module out of step with the Rust policy it
 * mirrors. Blankness is judged BEFORE the clamp, so a long invisible name is
 * not rescued into "visible" by the elision marker the clamp appends.
 */
export function displayIdentity(value: string, max: number, fallback: string): string {
  const clean = sanitizeText(value);
  return rendersBlank(clean) ? fallback : clampText(clean, max);
}

/**
 * A 32-bit FNV-1a digest, base 36. Not a security primitive and not required to
 * be one: its only job is to keep two over-long identities that share a prefix
 * from collapsing onto one DOM id once they are clamped. Where a hostile daemon
 * forces a collision it costs a duplicate `data-testid` on a screen already
 * being lied to; leaving the identities unbounded costs the webview.
 */
function digest(value: string): string {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(36);
}

/**
 * The bounded copy of an identity that reaches a DOM attribute, an IDREF or a
 * React key.
 *
 * `DesktopAgentDto.id` and `daemonId` carry no frontend validation and no
 * clamp — they are bounded only by the 16 MiB protocol frame, and
 * `encodeURIComponent` expands them by up to three — so a malformed daemon
 * could make React allocate and reconcile enormous keys and attributes on every
 * snapshot and freeze the webview. Grouping and the composite `(daemonId,
 * agentId)` identity keep the RAW values, exactly as before; only the copies
 * that reach React are bounded here, so nothing this does can merge two agents
 * or two groups. Over-budget values keep a digest of the whole original, so two
 * identities sharing a prefix stay two DOM ids.
 */
export function domIdentity(value: string, max: number = DISPLAY_LIMITS.domIdentity): string {
  const clean = sanitizeText(value);
  const chars = Array.from(clean);
  return chars.length <= max ? clean : `${chars.slice(0, max).join("")}~${digest(value)}`;
}

/**
 * How far a daemon-supplied instant may run ahead of the webview's clock before
 * this module stops trying to say how long ago it was.
 *
 * The two clocks are genuinely different clocks. `SessionSnapshot.last_activity_ms`
 * is a high-water mark of `AgentEvent.timestamp`s, and those are stamped by
 * whichever hook process emitted the event (`src/hook.rs`) — not by the daemon,
 * and certainly not by the webview. So a small positive skew is the ordinary
 * case, not a fault, and one minute absorbs it comfortably: NTP-synced hosts sit
 * inside a few milliseconds, and a container with a lazily-stepped clock inside
 * a second or two.
 *
 * `AgentRecord.spawned_at_ms` (M11) is stamped by the daemon itself rather than
 * by a hook process, so it is skewed against the webview by one clock rather
 * than two — a strictly narrower gap, comfortably inside the same tolerance.
 * ONE tolerance for both is deliberate: a second policy would be a second thing
 * to keep true, and the interesting cases (a daemon on another host, a
 * container whose clock steps) move both instants together.
 */
export const CLOCK_SKEW_TOLERANCE_MS = 60_000;

/** A minute, an hour and a day in milliseconds — the buckets below. */
const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;

/**
 * The widest instant a JavaScript `Date` can represent (±100,000,000 days from
 * the epoch). The daemon's field is an `i64`, whose range is nine orders of
 * magnitude wider, so a malformed or hostile value can sit outside this — and
 * `new Date(x).toISOString()` throws `RangeError` for one that does.
 */
const MAX_DATE_MS = 8.64e15;

/** What the last-activity column renders: a short label, and the exact instant on hover. */
export interface ActivityDisplay {
  label: string;
  title: string;
}

/**
 * The last-activity column's display copy: how long ago the daemon last saw
 * this agent do something, plus the exact UTC instant for the hover.
 *
 * Returns `undefined` for everything this cannot honestly express, and the
 * column renders **nothing at all** for that — no dash, no placeholder,
 * consistent with every other honest column on the overview (PRD #745 M9).
 * Three cases reach it:
 *
 * 1. **The daemon reported no instant.** A record with no live snapshot, and in
 *    particular every agent under a daemon that has restarted — it persists no
 *    session state, so it has no activity times rather than a set of freshly
 *    minted ones. "I do not know" is the honest answer and blank is how the
 *    screen says it.
 * 2. **The value is not a finite number**, which a TypeScript DTO cannot rule
 *    out: `DesktopAgentDto` is an assertion about a shape, not a validated one.
 * 3. **The instant is in the future by more than {@link CLOCK_SKEW_TOLERANCE_MS}.**
 *    This is the clock-skew decision, and it is deliberate in both directions.
 *    A negative "ago" is a bug a user sees, so it is never rendered. But nor is
 *    a future stamp quietly rewritten to "just now": `last_activity` is
 *    PRODUCER-supplied and unclamped by design — it is the ordering evidence
 *    `AppState::supersedes_generation` weighs, and the daemon has a test for a
 *    report stamped ten years out
 *    (`a_disowned_generations_far_future_session_cannot_pin_its_pane`) — so
 *    "just now" for a value hours or years ahead would be exactly the fabricated
 *    reading this PRD refuses. Within the tolerance the skew is ordinary and
 *    "just now" is true; beyond it, the webview does not know and says nothing.
 *
 * `now` is injectable so the buckets and the skew rule are testable without a
 * fake timer.
 */
export function displayActivity(lastActivityMs: number | undefined, now: number = Date.now()): ActivityDisplay | undefined {
  const usable = relativeTo(lastActivityMs, now);
  return usable && { label: elapsedLabel(usable.elapsed), title: usable.title };
}

/**
 * ONE clock-skew and usability policy, shared by every daemon-supplied instant
 * this module relativises — `undefined` for anything it cannot honestly express,
 * and otherwise the milliseconds elapsed plus the exact instant for the hover.
 *
 * It is factored out rather than copied because the three refusals ARE the
 * policy, and a second copy of them is a second thing to keep true: the daemon
 * reported nothing; the value is not a finite number (a TypeScript DTO is an
 * assertion about a shape, not a validated one); or the instant is ahead of the
 * webview's clock by more than {@link CLOCK_SKEW_TOLERANCE_MS}, where a
 * negative "ago" is a bug a user sees and a fabricated "just now" is the same
 * lie in nicer clothes. The out-of-`Date`-range guard sits here too: the
 * daemon's fields are `i64`s, whose range reaches nine orders of magnitude
 * further than a `Date`'s, and `toISOString()` throws for one that does.
 *
 * What is deliberately NOT shared is the WORDING. `displayActivity` says "3m
 * ago" because it names a moment in the past; `displayUptime` says "3m" because
 * it names a span still running, and "3m ago" would be a different and wrong
 * claim about it. Sharing the policy and forking the vocabulary is the split
 * the two functions exist to make.
 */
function relativeTo(instantMs: number | undefined, now: number): { elapsed: number; title: string } | undefined {
  if (instantMs === undefined || !Number.isFinite(instantMs)) return undefined;
  // Outside `Date`'s range there is no instant to show and `toISOString()`
  // would throw, so there is nothing honest to render.
  if (Math.abs(instantMs) > MAX_DATE_MS) return undefined;
  const elapsed = now - instantMs;
  if (elapsed < -CLOCK_SKEW_TOLERANCE_MS) return undefined;
  // The hover's ISO string is built HERE, inside the guard that makes it safe:
  // the range check above is the only thing standing between this call and a
  // `RangeError`, so keeping the two together means neither caller can hold one
  // without the other.
  return { elapsed, title: new Date(instantMs).toISOString() };
}

/**
 * The uptime column's display copy: how long this agent's process has been
 * running, plus the exact UTC spawn instant for the hover (PRD #745 M11).
 *
 * The instant is `AgentRecord.spawned_at_ms` — when the daemon forked the
 * child, which is an observation rather than an inference, and unlike
 * `SessionState.started_at` it exists for an agent that has never emitted a
 * hook event. Absence renders NOTHING, exactly as `displayActivity`'s does, and
 * for the same three reasons {@link relativeTo} enumerates. There is no
 * fallback anywhere on this path: a daemon that did not spawn the agent has no
 * uptime to report, and inventing one is the failure the PRD's original
 * duration rejection was about.
 *
 * **What the number MEANS follows from where it comes from, with no flag
 * needed.** A restarted orchestration worker is a fresh spawn with a fresh
 * record, so it reads as the age of its current iteration; a role nobody has
 * restarted keeps its original record, so it reads as its whole lifetime.
 *
 * `now` is injectable so the buckets and the skew rule are testable without a
 * fake timer.
 */
export function displayUptime(spawnedAtMs: number | undefined, now: number = Date.now()): ActivityDisplay | undefined {
  const usable = relativeTo(spawnedAtMs, now);
  return usable && { label: uptimeLabel(usable.elapsed), title: usable.title };
}

/**
 * One unit, largest that fits, floored — the reading a fleet overview wants is
 * "quiet for a while" versus "moving", not a precise interval. Floored rather
 * than rounded so the label never claims more elapsed time than has actually
 * passed.
 *
 * A tolerated negative elapsed (the ordinary-skew band) lands in the first
 * bucket, so it reads `just now` rather than as a negative count.
 */
function elapsedLabel(elapsed: number): string {
  if (elapsed < MINUTE_MS) return "just now";
  if (elapsed < HOUR_MS) return `${Math.floor(elapsed / MINUTE_MS)}m ago`;
  if (elapsed < DAY_MS) return `${Math.floor(elapsed / HOUR_MS)}h ago`;
  return `${Math.floor(elapsed / DAY_MS)}d ago`;
}

/**
 * The same buckets as {@link elapsedLabel}, worded as a SPAN rather than as a
 * point in the past: `5m`, `2h`, `3d`. No `ago`, because the interval named is
 * still running.
 *
 * Under a minute reads `<1m` rather than `just now`, and the difference is not
 * decoration. `just now` says when something happened; this column says how
 * long something has been going, and "just now" is not an answer to that. `<1m`
 * is, and it is honest about the floor rather than rounding a forty-second
 * process up to a minute. A tolerated negative elapsed (the ordinary-skew band)
 * lands in it too, so a slightly-future spawn stamp reads `<1m` rather than as
 * a negative count.
 */
function uptimeLabel(elapsed: number): string {
  if (elapsed < MINUTE_MS) return "<1m";
  if (elapsed < HOUR_MS) return `${Math.floor(elapsed / MINUTE_MS)}m`;
  if (elapsed < DAY_MS) return `${Math.floor(elapsed / HOUR_MS)}h`;
  return `${Math.floor(elapsed / DAY_MS)}d`;
}
