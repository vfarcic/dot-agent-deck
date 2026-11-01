use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Serializes the whole read-preserve-write span over the plugin file, for
/// every install path in this module (PRD #381 audit, MEDIUM-2).
///
/// Locking the write alone would not be enough: [`auto_install_to`] reads the
/// existing `BINARY_PATH` back, decides whether to preserve it, and only then
/// writes. Two callers racing that span both read the same "before" state, and
/// the second one republishes a pin the first had just repaired. Same shape and
/// same reasoning as `hooks_manage::SETTINGS_LOCK` and
/// `codex_hooks_manage::INSTALL_LOCK`.
///
/// What it does NOT close is the cross-PROCESS race — two deck binaries
/// starting at the same instant, or a deck racing a hand-edit of the plugin —
/// and, unlike the sibling adapters (`agent_hook_config::lock_config`, issue
/// #1493's follow-up), this one deliberately takes no file lock for it. Those
/// adapters MERGE their entries into a file other writers also add to, so a
/// lost update there drops someone else's entry. This file is the deck's own
/// generated JavaScript, rewritten whole every time; the only thing read back
/// is the pinned `BINARY_PATH`, and two racing writers each publish a complete
/// plugin pinning a usable binary (the one they found, or their own). So the
/// loser loses nothing anyone else wrote, and the atomic publish already keeps
/// OpenCode from ever loading a torn file.
static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Take [`PLUGIN_LOCK`], recovering from a poisoned mutex rather than
/// panicking: a previous caller panicking mid-install says nothing about
/// whether the plugin file is usable now, and the read is re-done from disk
/// under the guard regardless.
fn lock_plugin() -> MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The FLAT plugin file dot-agent-deck installs under an OpenCode config root:
/// `<root>/plugin/dot-agent-deck.js`.
///
/// OpenCode discovers local plugins with a **one-level** glob
/// (`{plugin,plugins}/*.{ts,js}` — no `**`), so the plugin must be a file
/// directly under `plugin/`. The pre-existing nested layout
/// (`<root>/plugin/dot-agent-deck/index.js`, see [`stale_plugin_dir`]) sat a
/// directory too deep and was therefore never scanned — it silently never
/// loaded, so the agent's card stayed "No agent". This flat path is what the
/// glob actually matches.
fn plugin_file(root: &Path) -> PathBuf {
    root.join("plugin").join("dot-agent-deck.js")
}

/// The obsolete nested plugin directory (`<root>/plugin/dot-agent-deck/`) from
/// before the flat-file layout. OpenCode never loaded it, but existing users
/// still have it on disk; [`write_plugin`] deletes it when it installs the flat
/// file so every user converges to the discoverable layout with no manual step.
fn stale_plugin_dir(root: &Path) -> PathBuf {
    root.join("plugin").join("dot-agent-deck")
}

/// PRD #2050: the OpenCode generation an install is for, when it is known.
///
/// What it installs does not depend on it wherever a global config root
/// exists, so that a machine running both generations side by side, or a
/// spawn of each in turn, rewrites nothing: OpenCode 2 reloads its plugins
/// when the module file changes, which ends a live 2.x pane's turn (measured
/// on 2.2.5, [`remember_layout`]). In every layout:
///
/// - the server module, one file whose default export carries both
///   generations' implementations, goes in the existing global roots
///   ([`global_roots`]: `$XDG_CONFIG_HOME/opencode`, which 1.x loads, and
///   `$OPENCODE_CONFIG_DIR`, 2.x's global root when set), and the deck's own
///   copy in `~/.opencode` is removed once 2.x's global root holds the module,
///   since 2.x also scans `~/.opencode` for a project under `$HOME` and
///   refuses a second copy of the same plugin id (`Duplicate plugin ID`);
/// - both TUI halves are installed (the 1.x file listed in `tui.json`, the
///   2.x directory `plugins/dot-agent-deck-tui/`), which do not see each
///   other.
///
/// The one difference: when no global root exists yet, [`Layout::V2`]
/// creates 2.x's (a 2.x was detected, so OpenCode is installed and creates it
/// itself, and 2.x would not load a module only in `~/.opencode` for a project
/// outside `$HOME`), while [`Layout::V1`] and [`Layout::Unknown`] keep the
/// fan-out into every existing root. The TUI halves create the global root on
/// that install, so the next one moves the module there, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    V1,
    V2,
    Unknown,
}

impl Layout {
    /// The layout for a detected generation, [`Layout::Unknown`] for none.
    pub fn for_major(major: Option<crate::opencode_version::Major>) -> Self {
        match major {
            Some(crate::opencode_version::Major::V1) => Layout::V1,
            Some(crate::opencode_version::Major::V2) => Layout::V2,
            None => Layout::Unknown,
        }
    }
}

/// PRD #2050: OpenCode 2.x's TUI plugin directory under a config root. A
/// directory, because 2.x resolves a TUI half only from a directory plugin;
/// it holds no `server.js`/`index.js`, so 2.x does not load it as a server
/// plugin, and OpenCode 1.x's one-level file glob does not see it.
fn tui_v2_dir(root: &Path) -> PathBuf {
    root.join("plugins").join("dot-agent-deck-tui")
}

/// The 2.x TUI plugin file, `<root>/plugins/dot-agent-deck-tui/tui.js`.
fn tui_v2_file(root: &Path) -> PathBuf {
    tui_v2_dir(root).join("tui.js")
}

/// PRD #163 M1: route the home lookup through the platform seam instead of
/// reading `$HOME` directly, so that on Windows — where `$HOME` is normally
/// unset — the OpenCode plugin roots resolve under `%USERPROFILE%` instead of
/// being missed entirely.
///
/// PRD #163 review: the seam function is
/// [`crate::platform::paths::home_dir_with_tmp_fallback`], *not* `home_dir`,
/// because the raw read this replaced fell back to `/tmp` when `$HOME` was unset.
/// Unix behavior is therefore byte-for-byte what it was, in that case too.
fn home_dir() -> PathBuf {
    crate::platform::paths::home_dir_with_tmp_fallback()
}

/// `$XDG_CONFIG_HOME`, else `~/.config`. An empty or relative value is
/// ignored, as the XDG base-directory spec says
/// ([`crate::platform::paths::absolute_dir_from_env`]): taken literally, an
/// empty `XDG_CONFIG_HOME=` made the install write `opencode/` into the
/// current directory. (OpenCode itself, 1.18.35 and 2.2.5 alike, ignores an
/// empty value too, but resolves a relative one against its own working
/// directory, which the deck cannot know, so a relative value has no root
/// the deck could write into.) Read on every platform, as OpenCode reads it.
fn xdg_config_root(home: &Path) -> PathBuf {
    xdg_config_root_from(home, std::env::var_os("XDG_CONFIG_HOME").as_deref())
}

/// [`xdg_config_root`] for an explicit `XDG_CONFIG_HOME` value.
fn xdg_config_root_from(home: &Path, xdg_config_home: Option<&std::ffi::OsStr>) -> PathBuf {
    crate::platform::paths::absolute_dir_from_env(xdg_config_home)
        .unwrap_or_else(|| home.join(".config"))
}

/// PRD #2050: `$OPENCODE_CONFIG_DIR`, when it names an absolute directory.
///
/// Measured: OpenCode 2.2.5 takes it as its global config root instead of
/// `$XDG_CONFIG_HOME/opencode` (`opencode debug paths`), so the server module,
/// the 2.x TUI half and `cli.json` live there. OpenCode 1.18.35 keeps
/// `$XDG_CONFIG_HOME/opencode` as its global root and scans this directory as
/// an extra one, for plugins (`plugin/`) and `tui.json` alike, so for 1.x it
/// is one more root of the fan-out ([`candidate_roots`]). Both ignore an
/// empty value; a relative one is ignored here for the reason
/// [`xdg_config_root`] gives.
fn opencode_config_dir() -> Option<std::ffi::OsString> {
    std::env::var_os("OPENCODE_CONFIG_DIR")
}

/// PRD #2050: OpenCode 2.x's global config root: `$OPENCODE_CONFIG_DIR`, else
/// `$XDG_CONFIG_HOME/opencode` ([`opencode_config_dir`]).
fn v2_global_root() -> PathBuf {
    roots_from(
        &home_dir(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        opencode_config_dir().as_deref(),
    )
    .1
}

/// The OpenCode config roots for `home` and the values of `XDG_CONFIG_HOME`
/// and `OPENCODE_CONFIG_DIR`: the roots every install considers
/// ([`candidate_roots`]), and OpenCode 2.x's global root ([`v2_global_root`]).
fn roots_from(
    home: &Path,
    xdg_config_home: Option<&std::ffi::OsStr>,
    config_dir: Option<&std::ffi::OsStr>,
) -> (Vec<PathBuf>, PathBuf) {
    let xdg = xdg_config_root_from(home, xdg_config_home).join("opencode");
    let config_dir = crate::platform::paths::absolute_dir_from_env(config_dir);
    let mut roots = vec![xdg.clone(), home.join(".opencode")];
    if let Some(dir) = &config_dir
        && !roots.contains(dir)
    {
        roots.push(dir.clone());
    }
    (roots, config_dir.unwrap_or(xdg))
}

/// PRD #2050, review S3: the roots an install that knows no OpenCode version
/// writes the server module to: every root of `roots` but the legacy
/// `~/.opencode` (`roots[1]`, [`roots_from`]), unless that is also 2.x's
/// global root `v2_global`. OpenCode 1.x loads the XDG global root as well,
/// so these serve either generation, and 2.x, which also scans `~/.opencode`
/// for a project under `$HOME`, finds no duplicate there. The legacy root
/// gets a copy only from a layout that positively detected 1.x.
///
/// With `$OPENCODE_CONFIG_DIR` set these are the XDG root and that directory,
/// and neither generation meets a duplicate it refuses (review F6). Measured
/// on 2.2.5 with a probe plugin in both: `opencode debug config --standalone`
/// listed `$OPENCODE_CONFIG_DIR` as its only config source, and only that
/// copy loaded; the XDG copy is the one 1.x's global root needs. OpenCode
/// 1.18.35 loads both, as it does under [`Layout::V1`], and the module's own
/// duplicate-load guard keeps one of them inert.
fn global_roots(roots: &[PathBuf], v2_global: &Path) -> Vec<PathBuf> {
    roots
        .iter()
        .enumerate()
        .filter(|(n, root)| *n != 1 || root.as_path() == v2_global)
        .map(|(_, root)| root.clone())
        .collect()
}

/// The XDG-default OpenCode config root (`$XDG_CONFIG_HOME/opencode`, defaulting
/// to `$HOME/.config/opencode`). Used as the explicit-install fallback root when
/// no existing layout is found. Deliberately performs **no** existence checks: it
/// is only ever evaluated after the caller has already determined that none of the
/// candidate roots exist, so re-detecting them would be dead work.
fn xdg_default_root() -> PathBuf {
    let home = home_dir();
    xdg_config_root(&home).join("opencode")
}

/// All candidate OpenCode config roots, XDG first then legacy, **without** checking
/// existence — that is the caller's job. This is the single source of truth for which
/// layouts we touch, shared by `existing_plugin_artifacts` (uninstall), `auto_install`,
/// and `install`. Adding a future layout is a one-line change here.
///
/// PRD #2050: `$OPENCODE_CONFIG_DIR` ([`opencode_config_dir`]) comes last when
/// it names another directory, since OpenCode 1.x loads plugins from it too.
/// The first root stays `$XDG_CONFIG_HOME/opencode`, 1.x's global root, where
/// the 1.x TUI half goes; OpenCode 2.x's global root is [`v2_global_root`].
fn candidate_roots() -> Vec<PathBuf> {
    roots_from(
        &home_dir(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        opencode_config_dir().as_deref(),
    )
    .0
}

/// All plugin artifacts that currently exist on disk across candidate roots (XDG
/// and legacy): the flat `dot-agent-deck.js` file AND any obsolete nested
/// `dot-agent-deck/` directory. For uninstall — so it clears both the current
/// layout and the stale one an upgrade may have left behind.
fn existing_plugin_artifacts() -> Vec<PathBuf> {
    let mut artifacts = Vec::new();
    for root in candidate_roots() {
        let file = plugin_file(&root);
        if file.exists() {
            artifacts.push(file);
        }
        let stale = stale_plugin_dir(&root);
        if stale.is_dir() {
            artifacts.push(stale);
        }
    }
    artifacts
}

/// PRD #2050 (audit N4): the revision of the server module's template, which
/// the module declares as `DAD_TEMPLATE_REVISION` ([`template_revision`]).
/// **Bump it whenever the template's output changes**:
/// `the_template_revision_is_bumped_with_the_template` fails until it is.
///
/// An automatic install leaves a module of a later revision than this one
/// alone while the pin it carries is usable and is either another install or
/// this install replaced under a running process (audit N4,
/// [`running_other_than`]): after a deck upgrade, the older daemon still
/// running would otherwise write its older template back on its next
/// OpenCode spawn, since its pin is the same path, and OpenCode 2 reloads its
/// plugins on every such write. A deck rolled back at the same path is the
/// file at its pin, so it writes its own template. Only a daemon built with
/// this check keeps from downgrading: one built before it writes its own
/// template as it always did.
const PLUGIN_TEMPLATE_REVISION: u32 = 1;

/// Render the plugin JavaScript with `BINARY_PATH` pinned to `binary_path`.
///
/// The pin is JSON-escaped rather than interpolated raw, so a path containing a
/// quote or a backslash cannot break out of the string literal in the code
/// OpenCode loads.
///
/// **There is deliberately no fallback on the escape.** This was
/// `.unwrap_or_else(|_| "\"dot-agent-deck\"".to_string())`, whose only possible
/// behaviour would have been to write the BARE command name into a file another
/// program executes: Node's `execFileSync` resolves a bare `BINARY_PATH` through
/// the AGENT's `$PATH`, which is issue #536's exact vector. PRD #381's rule is
/// that losing the ability to identify ourselves must fail toward a MORE
/// specific path or not at all — never toward a less specific one — so the
/// literal is gone rather than made quieter, and must not come back as a "safe
/// default". The refusal it would otherwise stand in for already happens one
/// seam earlier, at `durable_binary_path()`; by the time we are here the path is
/// decided and the only step left, serializing a `&str` into a `String`, cannot
/// fail.
fn plugin_template(binary_path: &str) -> String {
    let binary_path_json =
        serde_json::to_string(binary_path).expect("serializing a &str to a String is infallible");
    let inbox = inbox_js(&format!("{SERVER_INBOX_JS}{SERVER_INBOX_V2_JS}"));
    let usage = USAGE_FILE_JS;
    let entry = format!("{SERVER_ENTRY_JS}{SERVER_V2_JS}");
    let revision = PLUGIN_TEMPLATE_REVISION;
    format!(
        r#"import {{ execFile, execFileSync, spawn }} from "child_process";
import {{ closeSync, constants as fsConstants, fstatSync, openSync, readFileSync, readlinkSync, writeSync }} from "fs";
import {{ isMainThread }} from "worker_threads";

// Duplicate-load guard. The installer fans out to EVERY candidate config root
// that exists (`$XDG_CONFIG_HOME/opencode` and `~/.opencode`) because it cannot
// know which one OpenCode reads. When both exist OpenCode loads both copies into
// one process, and every hook fired twice — observed in production as doubled
// `Received event` lines in the daemon log, i.e. two events per real action.
//
// Fixed here rather than by narrowing the install, because the fan-out is what
// guarantees we land in the root OpenCode actually uses; suppressing the second
// copy keeps that guarantee and costs one flag. First copy loaded wins; any
// later copy exports inert no-op hooks.
const DAD_GUARD = "__dotAgentDeckPluginLoaded";
const DAD_ALREADY_LOADED = globalThis[DAD_GUARD] === true;
globalThis[DAD_GUARD] = true;

// Which template this is (PLUGIN_TEMPLATE_REVISION in the deck's source): an
// automatic install leaves a module of a later revision alone.
const DAD_TEMPLATE_REVISION = {revision};
const BINARY_PATH = {binary_path_json};
const knownSessions = new Map();
// Pending user messages, by message id, until their text part arrives: at most
// MAX_PENDING_USER_MESSAGES, the oldest forgotten first (its prompt is then not
// reported).
const messageRoles = new Map();
const directorySessions = new Map();
// Session id -> the directory's first session it is reported as. At most
// MAX_SESSION_ALIASES, the oldest forgotten first: a forgotten alias is worked
// out again from directorySessions the next time its session reports.
const sessionAliases = new Map();
const MAX_PENDING_USER_MESSAGES = 64;
const MAX_SESSION_ALIASES = 256;
let shuttingDown = false;

// Issue #1678: the longest session or message id, in UTF-8 bytes, the plugin
// keeps in its child-session, alias and reply records — the daemon's own bound
// on a tracked subagent id. A longer session id is recognised by a fixed-size
// stand-in (sessionKey), never by a cut-down copy: the stand-in is derived
// from the whole id, so ids that differ only past a cut are not merged by
// truncation. It is a 64-bit digest, not a collision-free one: two ids of the
// same length can still share a stand-in, with negligible probability.
// Stand-ins begin with OVERSIZED_ID_PREFIX, and an id that itself begins with
// it is held by a stand-in too, so no stand-in equals an id held as itself. A
// child held by a stand-in is never reported active (no `subagent.start` or
// `subagent.stop`); its tool calls and permission prompts still carry subagent
// attribution, as its stand-in, so such children's prompts are told apart
// unless their stand-ins coincide.
const MAX_ID_BYTES = 256;
const OVERSIZED_ID_PREFIX = "oversized-id:";
const utf8Length = (text) =>
  typeof Buffer !== "undefined" ? Buffer.byteLength(text, "utf8") : new TextEncoder().encode(text).length;
const idFits = (id) =>
  typeof id === "string" && id.length > 0 && id.length <= MAX_ID_BYTES && utf8Length(id) <= MAX_ID_BYTES;
const digest = (text) => {{
  let a = 0x811c9dc5;
  let b = 0x9e3779b9;
  for (let i = 0; i < text.length; i += 1) {{
    const c = text.charCodeAt(i);
    a = Math.imul(a ^ c, 0x01000193);
    b = Math.imul(b ^ c, 0x5bd1e995);
  }}
  return (a >>> 0).toString(16).padStart(8, "0") + (b >>> 0).toString(16).padStart(8, "0");
}};
// Does session id `id` keep its own id: it fits in MAX_ID_BYTES and is not in
// the stand-ins' namespace?
const keepsOwnId = (id) => idFits(id) && !id.startsWith(OVERSIZED_ID_PREFIX);
// How the plugin holds session id `id`: the id itself when it keeps it,
// otherwise a fixed-size stand-in derived from all of it.
const sessionKey = (id) => {{
  if (typeof id !== "string" || !id) {{
    return undefined;
  }}
  return keepsOwnId(id) ? id : OVERSIZED_ID_PREFIX + id.length + ":" + digest(id);
}};

// PRD #1497: the assistant's reply, for the deck's voice "reading on". The text
// parts of each assistant message are kept as they stream in (the full text
// arrives on `message.part.updated`, and `message.part.delta` carries the
// increments in between); when the session goes idle, the text of its LAST
// assistant message is attached to that idle report as `reply`, once. Keyed by
// OpenCode's own (raw) session id, never by the directory alias below: a
// subagent's child session shares the root's directory, and its reply is not
// the user's turn. A reply is only published for a session KNOWN to be a root
// (no `parentID`) when it goes idle, learned from `session.created` /
// `session.updated` or, when neither was seen, asked of OpenCode's client in
// the background (lookUpAncestry); an unknown or child session's idle report
// carries no reply, and never waits for an answer.
//
// Bounded: a message retains at most MAX_REPLY_BYTES of UTF-8 text across all
// its parts (the daemon's own bound, so nothing kept here is cut there) and at
// most MAX_REPLY_PARTS parts; at most MAX_REPLY_MESSAGES messages and
// MAX_REPLY_SESSIONS sessions' reply state are tracked at once, the oldest
// forgotten first, and a deleted or disposed session's state is dropped.
const MAX_REPLY_BYTES = 8192;
const MAX_REPLY_PARTS = 256;
const MAX_REPLY_MESSAGES = 64;
const MAX_REPLY_SESSIONS = 256;
const ANCESTRY_LOOKUP_MS = 2000;
const MAX_ANCESTRY_LOOKUPS = 8;
const replyMessages = new Map();
const lastAssistantMessage = new Map();
const failedTurns = new Set();
const abortedTurns = new Set();
// Raw session id -> its parent's id, or "" for a root session.
const sessionParents = new Map();
let sessionClient = undefined;
// Ancestry lookups in flight, by raw session id (lookUpAncestry), how many of
// their requests have not settled, and a count bumped on every disposal so an
// answer to a lookup started before one is discarded.
const ancestryLookups = new Map();
let unsettledAncestryLookups = 0;
let disposalGeneration = 0;

// PRD #1497: DOT_AGENT_DECK_BIN, when it is an absolute path, names the binary
// to report through instead of BINARY_PATH (the `task run-all` sandbox sets it
// to the build under test). Anything else — unset, empty, a bare name or a
// relative path — is ignored, since execFileSync would resolve it through the
// agent's PATH or working directory (issue #536). Used verbatim, not trimmed,
// and read per event, like the deck's hook commands.
const isAbsoluteOverride = (value) =>
  process.platform === "win32"
    ? /^([A-Za-z]:[\\/]|[\\/]{{2}})/.test(value)
    : value.startsWith("/");
const hookBinary = () => {{
  const override = process.env.DOT_AGENT_DECK_BIN;
  return typeof override === "string" && isAbsoluteOverride(override) ? override : BINARY_PATH;
}};

const sendEvent = (payload) => {{
  try {{
    execFileSync(hookBinary(), ["hook", "--agent", "opencode"], {{
      input: JSON.stringify(payload),
      timeout: 5000,
      stdio: ["pipe", "ignore", "ignore"],
    }});
  }} catch (_) {{}}
}};

// PRD #1631: questions the deck may answer. For `permission.asked` and
// `question.asked` the plugin starts `await-answer`, which reports the question
// to the deck and waits for an answer, and replies through OpenCode's own API
// when one comes. `heldAnswers` maps OpenCode's request id to that child, so
// an answer given in OpenCode itself (`*.replied`, `question.rejected`) stops
// it. Nothing here blocks the event loop: the child is spawned, not exec'd.
const heldAnswers = new Map();

const stopHeldAnswer = (requestId) => {{
  const child = heldAnswers.get(requestId);
  if (child) {{
    heldAnswers.delete(requestId);
    try {{
      child.kill();
    }} catch (_) {{}}
  }}
}};

// Request ids OpenCode reported answered (`*.replied`, `question.rejected`),
// so a failed reply is not raised again for a prompt that is already gone.
// Bounded: only the most recent are kept.
const resolvedRequests = new Set();
const noteResolved = (requestId) => {{
  if (typeof requestId !== "string" || !requestId) {{
    return;
  }}
  resolvedRequests.add(requestId);
  if (resolvedRequests.size > 256) {{
    resolvedRequests.delete(resolvedRequests.values().next().value);
  }}
}};

// `client._client` is the SDK's underlying HTTP client. The plugin's v1 client
// has no question-reply method, and posting through it is what was measured to
// dismiss OpenCode's dialog (PRD #1631 follow-up d). An SDK internal, so it is
// used here and nowhere else. Resolves to what OpenCode made of the reply —
// `{{ delivered: true }}`, or `{{ delivered: false, detail }}` when there is no
// client to post through, the post throws, or OpenCode answers with an error
// (the SDK reports one as `error`, or a response that is not ok, rather than
// throwing) — which `await-answer --ack` passes to the deck.
const replyThroughOpenCode = async (client, answer) => {{
  // PRD #2050: on OpenCode 2.x the reply goes through its own API instead
  // (`v2Replier`).
  if (typeof client?.dadReply === "function") {{
    return client.dadReply(answer);
  }}
  const refused = (detail) => ({{ delivered: false, detail: String(detail).slice(0, 200) }});
  const http = client?._client;
  if (!http || typeof http.post !== "function") {{
    return refused("the plugin has no OpenCode client to reply through");
  }}
  const requestID = answer?.request_id;
  if (typeof requestID !== "string" || !requestID) {{
    return refused("the answer names no request");
  }}
  const url =
    answer.kind === "permission"
      ? "/permission/{{requestID}}/reply"
      : answer.kind === "question"
        ? "/question/{{requestID}}/reply"
        : null;
  if (!url) {{
    return refused("the answer is not a permission or a question reply");
  }}
  try {{
    const result = await http.post({{ url, path: {{ requestID }}, body: answer.body }});
    const status = result?.response?.status;
    if (result?.error !== undefined && result?.error !== null) {{
      return refused(status ? `OpenCode answered ${{status}}` : "OpenCode refused the reply");
    }}
    if (result?.response && result.response.ok === false) {{
      return refused(`OpenCode answered ${{status}}`);
    }}
    return {{ delivered: true }};
  }} catch (error) {{
    return refused(error?.message ?? "the reply could not be posted");
  }}
}};

// One `await-answer --ack` child per question: the payload goes in as the
// first line of its stdin, the reply OpenCode should get comes back as one
// line of stdout, and what OpenCode made of it goes back as one more stdin
// line, which the child passes to the deck — so the deck reports the answer
// sent only once OpenCode took it. When OpenCode did not, the question is
// raised again, so it is pending in the deck once more and can be answered.
// `fallback` reports the question plainly, so the card still shows it: it runs
// once when the question cannot be held — it names no request, or its child
// cannot be spawned — and once when the child fails before it is done with the
// question (it cannot be started, exits non-zero, or is killed by anything but
// this plugin). `await-answer` exits 0 on every outcome it handles, a refusal
// it reports plainly itself included, so a clean exit adds no second report.
const awaitAnswer = (client, payload, fallback) => {{
  const requestId = payload?.properties?.id;
  if (typeof requestId !== "string" || !requestId) {{
    fallback?.();
    return;
  }}
  stopHeldAnswer(requestId);
  resolvedRequests.delete(requestId);
  let child;
  try {{
    child = spawn(hookBinary(), ["await-answer", "--agent", "opencode", "--ack"], {{
      stdio: ["pipe", "pipe", "ignore"],
    }});
  }} catch (_) {{
    fallback?.();
    return;
  }}
  heldAnswers.set(requestId, child);
  // A child this plugin stopped (`stopHeldAnswer`, shutdown) or that printed
  // the deck's answer is no longer in `heldAnswers`, so its end is no failure.
  let failed = false;
  const failedEarly = (code, signal) => {{
    if (failed || shuttingDown || heldAnswers.get(requestId) !== child) {{
      return;
    }}
    if (code === 0 && !signal) {{
      return;
    }}
    failed = true;
    fallback?.();
  }};
  const forget = () => {{
    if (heldAnswers.get(requestId) === child) {{
      heldAnswers.delete(requestId);
    }}
  }};
  let out = "";
  let answered = false;
  const acknowledge = (ack) => {{
    try {{
      child.stdin?.end(JSON.stringify(ack) + "\n");
    }} catch (_) {{}}
  }};
  child.stdout?.on("data", async (chunk) => {{
    if (answered) {{
      return;
    }}
    out += chunk;
    const newline = out.indexOf("\n");
    if (newline < 0) {{
      return;
    }}
    answered = true;
    // The deck has answered: OpenCode's own `*.replied` for this request must
    // not stop the child while it waits to pass on the outcome.
    forget();
    let answer;
    try {{
      answer = JSON.parse(out.slice(0, newline));
    }} catch (_) {{
      acknowledge({{ delivered: false, detail: "the deck's answer could not be read" }});
      return;
    }}
    const ack = await replyThroughOpenCode(client, answer);
    acknowledge(ack);
    if (!ack.delivered && !shuttingDown && !resolvedRequests.has(requestId)) {{
      awaitAnswer(client, payload, fallback);
    }}
  }});
  child.on("error", () => {{
    failedEarly(null, "error");
    forget();
  }});
  child.on("close", (code, signal) => {{
    failedEarly(code, signal);
    forget();
  }});
  // A child that exits first turns a later write into an EPIPE on the stream,
  // which must not surface as an unhandled error inside OpenCode.
  child.stdin?.on("error", () => {{}});
  try {{
    child.stdin?.write(JSON.stringify(payload) + "\n");
  }} catch (_) {{}}
}};

const questionPayload = (event, directory) => {{
  const props = event?.properties ?? {{}};
  const cwd = directory ?? process.cwd();
  return {{
    session_id: normalizeSessionId(
      defaultSessionId(props.sessionID ?? props.sessionId),
      cwd
    ),
    event: event?.type ?? "question.unknown",
    properties: props,
    cwd,
  }};
}};

const defaultSessionId = (value) => (value ? value : "unknown");

const normalizeSessionId = (sessionId, directory) => {{
  const dirKey = directory ?? process.cwd();
  if (sessionId && sessionAliases.has(sessionId)) {{
    return sessionAliases.get(sessionId);
  }}
  if (sessionId && sessionId !== "unknown") {{
    const existing = directorySessions.get(dirKey);
    if (existing && existing !== sessionId) {{
      if (idFits(sessionId)) {{
        rememberBounded(sessionAliases, sessionId, existing, MAX_SESSION_ALIASES);
      }}
      return existing;
    }}
    directorySessions.set(dirKey, sessionId);
    return sessionId;
  }}
  const fallback = directorySessions.get(dirKey);
  if (fallback) {{
    return fallback;
  }}
  return sessionId ?? "unknown";
}};

const updateSessionInfo = (sessionId, directory, status) => {{
  if (!sessionId || sessionId === "unknown") {{
    return null;
  }}
  const existing = knownSessions.get(sessionId) ?? {{}};
  const cwd = directory ?? existing.cwd ?? process.cwd();
  const info = {{
    cwd,
    status: status ?? existing.status,
  }};
  knownSessions.set(sessionId, info);
  return info;
}};

const cleanupSessionMessages = (sessionId) => {{
  for (const [messageId, info] of messageRoles.entries()) {{
    if (info?.sessionId === sessionId) {{
      messageRoles.delete(messageId);
    }}
  }}
}};

// Issue #714: the structured fields of a `session.error`, so the deck can tell
// a provider quota or credit refusal from any other error by JSON keys (see
// `quota_signals::classify_opencode_error`). Every read is guarded and every
// value type-checked, so a changed OpenCode error shape degrades to sending no
// fields, which the deck reads as a plain error. Only allow-listed response
// headers are forwarded, each value bounded.
//
// The response body is NOT forwarded. It is parsed here, whole, and only the
// marker keys the classifier reads are forwarded, at the same paths, as
// `response_markers`: `type` and `name` at the top level, and `type`, `code`,
// `name`, `resets_at` and `resets_in_seconds` under `error`. Classifying a
// truncated copy instead would turn a long but valid provider error into
// invalid JSON and lose its markers (Qodo on PR #1346). Strings are bounded,
// numbers must be finite, and a body over MAX_PARSED_BODY characters is not
// parsed at all, which the deck reads as a plain error.
const MAX_PARSED_BODY = 1024 * 1024;
const markerString = (value) => (typeof value === "string" ? value.slice(0, 200) : undefined);
const markerNumber = (value) =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined;
const isPlainObject = (value) => !!value && typeof value === "object" && !Array.isArray(value);
const responseMarkers = (body) => {{
  if (typeof body !== "string" || body.length > MAX_PARSED_BODY) {{
    return undefined;
  }}
  let parsed;
  try {{
    parsed = JSON.parse(body);
  }} catch (_) {{
    return undefined;
  }}
  if (!isPlainObject(parsed)) {{
    return undefined;
  }}
  const put = (target, key, value) => {{
    if (value !== undefined) {{
      target[key] = value;
    }}
  }};
  const out = {{}};
  put(out, "type", markerString(parsed.type));
  put(out, "name", markerString(parsed.name));
  if (isPlainObject(parsed.error)) {{
    const error = {{}};
    put(error, "type", markerString(parsed.error.type));
    put(error, "code", markerString(parsed.error.code));
    put(error, "name", markerString(parsed.error.name));
    put(error, "resets_at", markerNumber(parsed.error.resets_at));
    put(error, "resets_in_seconds", markerNumber(parsed.error.resets_in_seconds));
    out.error = error;
  }}
  return out;
}};
const ERROR_HEADER_ALLOW = new Set(["retry-after", "retry-after-ms", "x-codex-rate-limit-reached-type"]);
const ERROR_HEADER_PREFIX = "anthropic-ratelimit-unified-";
const errorFields = (error) => {{
  const out = {{}};
  if (!error || typeof error !== "object") {{
    return out;
  }}
  if (typeof error.name === "string") {{
    out.error_name = error.name.slice(0, 100);
  }}
  const data = error.data && typeof error.data === "object" ? error.data : {{}};
  if (typeof data.message === "string") {{
    out.error_message = data.message.slice(0, 500);
  }}
  const markers = responseMarkers(data.responseBody);
  if (markers !== undefined) {{
    out.response_markers = markers;
  }}
  const headers =
    data.responseHeaders && typeof data.responseHeaders === "object" ? data.responseHeaders : {{}};
  const kept = {{}};
  let count = 0;
  for (const [name, value] of Object.entries(headers)) {{
    const key = String(name).toLowerCase();
    if (
      count < 32 &&
      typeof value === "string" &&
      (ERROR_HEADER_ALLOW.has(key) || key.startsWith(ERROR_HEADER_PREFIX))
    ) {{
      kept[key] = value.slice(0, 200);
      count += 1;
    }}
  }}
  if (count > 0) {{
    out.response_headers = kept;
  }}
  return out;
}};

const sessionPayload = (event, directory) => {{
  const props = event?.properties ?? {{}};
  const info = props.info ?? {{}};
  const status = props.status ?? {{}};
  const cwd = info.directory ?? props.directory ?? directory ?? process.cwd();
  return {{
    session_id: normalizeSessionId(
      defaultSessionId(props.sessionID ?? info.id),
      cwd
    ),
    event: event?.type ?? "session.unknown",
    status: status.type,
    cwd,
    ...(event?.type === "session.error" ? errorFields(props.error) : {{}}),
  }};
}};

const permissionPrompt = (props) =>
  props.prompt ??
  props.title ??
  props.message ??
  props.text ??
  props.question ??
  "";

const permissionPayload = (event, directory) => {{
  const props = event?.properties ?? {{}};
  const cwd = directory ?? process.cwd();
  return {{
    session_id: normalizeSessionId(
      defaultSessionId(props.sessionID ?? props.sessionId),
      cwd
    ),
    event: event?.type ?? "permission.unknown",
    prompt: permissionPrompt(props),
    cwd,
    // PRD #1631: the request a `*.replied` / `question.rejected` answers.
    request_id: props.requestID ?? props.id,
  }};
}};

const ensureSessionRegistered = (sessionId, directory, status, shouldEmitEvent = true) => {{
  if (!sessionId || sessionId === "unknown") {{
    return;
  }}
  const alreadyKnown = knownSessions.has(sessionId);
  const info = updateSessionInfo(sessionId, directory, status);
  if (!alreadyKnown && shouldEmitEvent) {{
    sendEvent({{
      session_id: sessionId,
      event: "session.created",
      status,
      cwd: info?.cwd ?? process.cwd(),
    }});
  }}
}};

const closeSession = (sessionId, directory, emitEvent = true, removeAlias = true) => {{
  if (!sessionId || sessionId === "unknown") {{
    return;
  }}
  const info = knownSessions.get(sessionId);
  knownSessions.delete(sessionId);
  cleanupSessionMessages(sessionId);
  if (removeAlias) {{
    for (const [alias, target] of sessionAliases.entries()) {{
      if (alias === sessionId || target === sessionId) {{
        sessionAliases.delete(alias);
      }}
    }}
    for (const [dirKey, id] of directorySessions.entries()) {{
      if (id === sessionId) {{
        directorySessions.delete(dirKey);
      }}
    }}
  }}
  if (emitEvent) {{
    sendEvent({{
      session_id: sessionId,
      event: "session.deleted",
      cwd: directory ?? info?.cwd ?? process.cwd(),
    }});
  }}
}};

const flushSessions = () => {{
  // A child still active when OpenCode goes away ends with it: its stop is
  // sent under the session its start was, before that session closes.
  for (const [childId, route] of activeSubagents.entries()) {{
    sendEvent({{
      session_id: route.sessionId,
      subagent_id: childId,
      event: "subagent.stop",
      cwd: route.cwd,
    }});
  }}
  activeSubagents.clear();
  childRoutes.clear();
  for (const [sessionId, info] of knownSessions.entries()) {{
    closeSession(sessionId, info?.cwd, true, true);
  }}
  forgetAllReplyState();
}};

const handleShutdown = () => {{
  if (shuttingDown) {{
    return;
  }}
  shuttingDown = true;
  flushSessions();
}};

process.once("exit", handleShutdown);
for (const signal of ["SIGINT", "SIGTERM"]) {{
  process.once(signal, handleShutdown);
}}

// `text` cut to at most `budget` bytes of UTF-8 at a code point boundary, with
// a lone surrogate replaced by U+FFFD (JSON would carry it as an escape the
// deck cannot decode). The string is walked lazily and the walk stops at the
// budget, so a huge input costs the budget, not its length.
const clipBytes = (text, budget) => {{
  let kept = "";
  let bytes = 0;
  if (typeof text !== "string" || budget <= 0) {{
    return {{ text: kept, bytes }};
  }}
  for (const ch of text) {{
    const cp = ch.codePointAt(0);
    const lone = cp >= 0xd800 && cp <= 0xdfff;
    const size = lone ? 3 : cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    if (bytes + size > budget) {{
      break;
    }}
    kept += lone ? "�" : ch;
    bytes += size;
  }}
  return {{ text: kept, bytes }};
}};

// Insert or refresh `key` in a Map (with `value`) or a Set, as its newest
// entry, forgetting the oldest past `cap` (MAX_REPLY_SESSIONS unless given).
const rememberBounded = (collection, key, value, cap = MAX_REPLY_SESSIONS) => {{
  collection.delete(key);
  if (collection instanceof Map) {{
    collection.set(key, value);
  }} else {{
    collection.add(key);
  }}
  while (collection.size > cap) {{
    collection.delete(collection.keys().next().value);
  }}
}};

const forgetReplyState = (sessionId) => {{
  lastAssistantMessage.delete(sessionId);
  failedTurns.delete(sessionId);
  abortedTurns.delete(sessionId);
  for (const [id, entry] of replyMessages.entries()) {{
    if (entry.sessionId === sessionId) {{
      replyMessages.delete(id);
    }}
  }}
}};

const forgetAllReplyState = () => {{
  disposalGeneration += 1;
  for (const sessionId of Array.from(ancestryLookups.keys())) {{
    cancelAncestryLookup(sessionId);
  }}
  replyMessages.clear();
  lastAssistantMessage.clear();
  failedTurns.clear();
  abortedTurns.clear();
  sessionParents.clear();
}};

// Issue #1678: a subagent's child session (one whose `parentID` is known) is
// reported as that subagent's work, never as the main session's. Its going
// active (`session.status` busy or retry) is a `subagent.start` naming the
// child's raw id, and its going idle, failing or being deleted is the paired
// `subagent.stop`, both filed under the session the child descends from. Only
// a transition is sent: a child already active sends no second start, and one
// that is not sends no stop, so a child that goes busy again later starts
// again. Its other session events and its user message (the prompt it was
// given) are not sent, and its tool calls and permission prompts carry
// `subagent_id`, so none of them counts as the main session's work or turn
// activity; a permission prompt still puts the card on Needs Input, and its
// answer (or the child's stop) takes the card back off it. A session whose
// ancestry is not known when it reports is still treated as the main
// session's.
//
// Where a child's reports go is kept apart from sessionParents, which forgets
// its oldest entries as other sessions are created: an active child is held in
// activeSubagents with the session its start was filed under until it stops,
// and the MAX_REPLY_SESSIONS children seen most recently in childRoutes, so
// neither is taken for the main session because its ancestry was forgotten.
// Both are keyed by sessionKey.
//
// Bounded: at most MAX_ACTIVE_SUBAGENTS children are reported active at once;
// a child past that sends neither its start nor its stop.
const MAX_ACTIVE_SUBAGENTS = 128;
const MAX_ANCESTRY_DEPTH = 32;
// Child key -> {{ root, sessionId, cwd }}: the root it descends from, and the
// session id and directory its `subagent.start` was sent with.
const activeSubagents = new Map();
// Child key -> the root it descends from.
const childRoutes = new Map();

const routeOf = (key) => activeSubagents.get(key)?.root ?? childRoutes.get(key);

// What session `key` (a sessionKey) is known to be: "child", "root", or
// undefined while its ancestry is not known.
const ancestryOfKey = (key) => {{
  if (key === undefined) {{
    return undefined;
  }}
  const parent = sessionParents.get(key);
  if (parent !== undefined) {{
    return parent === "" ? "root" : "child";
  }}
  return activeSubagents.has(key) || childRoutes.has(key) ? "child" : undefined;
}};

const isChildSession = (sessionId) => ancestryOfKey(sessionKey(sessionId)) === "child";

// The root `key` descends from (itself for a root), so a grandchild's work is
// filed under the main session too. Walks the known ancestry; where that is
// broken (an ancestor not known, or forgotten) the route recorded for `key`,
// or for the ancestor the walk stopped at, is used.
const rootKeyOf = (key) => {{
  let current = key;
  for (let depth = 0; depth < MAX_ANCESTRY_DEPTH; depth += 1) {{
    const parent = sessionParents.get(current);
    if (parent === "") {{
      return current;
    }}
    if (parent === undefined) {{
      break;
    }}
    current = parent;
  }}
  return routeOf(key) ?? routeOf(current) ?? current;
}};

// What a child's report is attributed to: its own id, or its stand-in for one
// that does not keep it (sessionKey), so two such children stay apart.
const subagentAttribution = (childId) => sessionKey(childId);

const subagentPayload = (event, childId, directory, rest) => {{
  const props = event?.properties ?? {{}};
  const cwd = props.info?.directory ?? props.directory ?? directory ?? process.cwd();
  return {{
    session_id: normalizeSessionId(rootKeyOf(sessionKey(childId)), cwd),
    subagent_id: subagentAttribution(childId),
    cwd,
    ...rest,
  }};
}};

// A child session's own session event: a start or stop on a transition,
// nothing otherwise.
const handleChildSessionEvent = (event, childId, directory) => {{
  const key = sessionKey(childId);
  const type = event?.type ?? "";
  const status = event?.properties?.status?.type;
  const active =
    (type === "session.status" || type === "session.status.updated") &&
    (status === "busy" || status === "retry");
  const ended =
    type === "session.idle" ||
    type === "session.error" ||
    type === "session.deleted" ||
    ((type === "session.status" || type === "session.status.updated") && status === "idle");
  const root = rootKeyOf(key);
  const running = activeSubagents.get(key);
  if (
    active &&
    !running &&
    keepsOwnId(childId) &&
    activeSubagents.size < MAX_ACTIVE_SUBAGENTS
  ) {{
    const payload = subagentPayload(event, childId, directory, {{ event: "subagent.start" }});
    activeSubagents.set(key, {{ root, sessionId: payload.session_id, cwd: payload.cwd }});
    childRoutes.delete(key);
    sendEvent(payload);
  }} else if (ended && running) {{
    activeSubagents.delete(key);
    sendEvent({{
      session_id: running.sessionId,
      subagent_id: childId,
      event: "subagent.stop",
      cwd: running.cwd,
    }});
  }}
  if (type === "session.deleted") {{
    forgetReplyState(childId);
    sessionParents.delete(key);
    childRoutes.delete(key);
    cancelAncestryLookup(childId);
  }} else if (!activeSubagents.has(key)) {{
    rememberBounded(childRoutes, key, root);
  }}
}};

// Drop the user messages still waiting for their text that child `key` sent:
// they are the prompt its subagent was given, not the user's.
const forgetChildPrompts = (key) => {{
  for (const [messageId, info] of messageRoles.entries()) {{
    if (info?.rawSessionKey === key) {{
      messageRoles.delete(messageId);
    }}
  }}
}};

// Record a session's ancestry from its info; a child's reply state and pending
// prompts are dropped, and where its reports go is recorded.
const noteSessionAncestry = (info) => {{
  const key = sessionKey(info?.id);
  if (key === undefined) {{
    return;
  }}
  const parent = typeof info.parentID === "string" && info.parentID ? sessionKey(info.parentID) : "";
  rememberBounded(sessionParents, key, parent);
  if (parent) {{
    forgetReplyState(info.id);
    forgetChildPrompts(key);
    if (!activeSubagents.has(key)) {{
      rememberBounded(childRoutes, key, rootKeyOf(key));
    }}
  }}
}};

// Stop waiting for `sessionId`'s ancestry lookup (it was deleted, or OpenCode
// disposed): its answer, if one still comes, is discarded.
const cancelAncestryLookup = (sessionId) => {{
  const lookup = ancestryLookups.get(sessionId);
  if (lookup) {{
    ancestryLookups.delete(sessionId);
    lookup.abort();
  }}
}};

// Ask OpenCode's client, in the background, whether `sessionId` is a root,
// when no session event named its ancestry; nothing waits for the answer, so
// a report is never delayed by it. One lookup per session at a time, and none
// started while MAX_ANCESTRY_LOOKUPS requests are unsettled, so a client that
// never answers cannot pile them up. One that has not answered within
// ANCESTRY_LOOKUP_MS is aborted through the request's `signal` (a client that
// ignores the signal still holds its session's slot and one of the
// MAX_ANCESTRY_LOOKUPS until it settles). An answer is recorded only if its
// session was not deleted, and OpenCode not disposed, while it was asked.
const lookUpAncestry = (sessionId) => {{
  const sessions = sessionClient?.session;
  if (
    !idFits(sessionId) ||
    sessionParents.has(sessionId) ||
    ancestryLookups.has(sessionId) ||
    unsettledAncestryLookups >= MAX_ANCESTRY_LOOKUPS ||
    typeof sessions?.get !== "function"
  ) {{
    return;
  }}
  const controller = typeof AbortController === "function" ? new AbortController() : undefined;
  let timer;
  const lookup = {{
    abort: () => {{
      clearTimeout(timer);
      try {{
        controller?.abort();
      }} catch (_) {{}}
    }},
  }};
  const disposal = disposalGeneration;
  ancestryLookups.set(sessionId, lookup);
  unsettledAncestryLookups += 1;
  timer = setTimeout(lookup.abort, ANCESTRY_LOOKUP_MS);
  timer?.unref?.();
  let request;
  try {{
    request = Promise.resolve(
      sessions.get({{ path: {{ id: sessionId }}, ...(controller ? {{ signal: controller.signal }} : {{}}) }})
    );
  }} catch (error) {{
    request = Promise.reject(error);
  }}
  request
    .then(
      (answer) => {{
        const info = answer?.data ?? answer;
        if (
          ancestryLookups.get(sessionId) === lookup &&
          disposal === disposalGeneration &&
          info?.id === sessionId
        ) {{
          noteSessionAncestry(info);
        }}
      }},
      () => {{}}
    )
    .finally(() => {{
      clearTimeout(timer);
      unsettledAncestryLookups -= 1;
      if (ancestryLookups.get(sessionId) === lookup) {{
        ancestryLookups.delete(sessionId);
      }}
    }});
}};

const replyMessage = (messageId, sessionId) => {{
  let message = replyMessages.get(messageId);
  if (!message) {{
    while (replyMessages.size >= MAX_REPLY_MESSAGES) {{
      const [oldest, evicted] = replyMessages.entries().next().value;
      replyMessages.delete(oldest);
      if (lastAssistantMessage.get(evicted.sessionId) === oldest) {{
        lastAssistantMessage.delete(evicted.sessionId);
      }}
    }}
    message = {{ sessionId, parts: new Map(), bytes: 0 }};
    replyMessages.set(messageId, message);
  }}
  return message;
}};

const recordAssistantMessage = (info) => {{
  const sessionId = info?.sessionID;
  if (!idFits(sessionId) || !idFits(info.id) || isChildSession(sessionId)) {{
    return;
  }}
  // Asked now, so the answer is usually in by the time the turn goes idle.
  lookUpAncestry(sessionId);
  const previous = lastAssistantMessage.get(sessionId);
  if (previous && previous !== info.id) {{
    replyMessages.delete(previous);
  }}
  rememberBounded(lastAssistantMessage, sessionId, info.id);
  replyMessage(info.id, sessionId);
}};

const recordReplyPart = (part) => {{
  if (part?.type !== "text" || part.synthetic || part.ignored || !idFits(part.id) || !idFits(part.messageID)) {{
    return;
  }}
  if ((part.sessionID !== undefined && !idFits(part.sessionID)) || isChildSession(part.sessionID)) {{
    return;
  }}
  const message = replyMessage(part.messageID, part.sessionID);
  const existing = message.parts.get(part.id);
  if (!existing && message.parts.size >= MAX_REPLY_PARTS) {{
    return;
  }}
  const others = message.bytes - (existing?.bytes ?? 0);
  const kept = clipBytes(part.text, MAX_REPLY_BYTES - others);
  message.parts.set(part.id, kept);
  message.bytes = others + kept.bytes;
}};

const recordReplyDelta = (props) => {{
  if (props?.field !== "text" || typeof props.delta !== "string") {{
    return;
  }}
  const message = replyMessages.get(props.messageID);
  const existing = message?.parts.get(props.partID);
  if (!existing) {{
    return;
  }}
  const added = clipBytes(props.delta, MAX_REPLY_BYTES - message.bytes);
  if (added.bytes === 0) {{
    return;
  }}
  message.parts.set(props.partID, {{
    text: existing.text + added.text,
    bytes: existing.bytes + added.bytes,
  }});
  message.bytes += added.bytes;
}};

const recordTurnError = (sessionId, error) => {{
  if (!idFits(sessionId) || isChildSession(sessionId)) {{
    return;
  }}
  rememberBounded(error?.name === "MessageAbortedError" ? abortedTurns : failedTurns, sessionId);
}};

// The reply owed for OpenCode session `sessionId`'s turn that just went idle,
// as the fields to add to that idle report, consumed so it is sent once. Only
// a session already known to be a root gets one; this never waits for a
// lookup, so the idle report is sent at once. A subagent's child session gets
// none. So does a session whose ancestry is not known yet, and that reply is
// dropped rather than sent later: the deck takes a reply only on a turn-end
// report, and a second idle report would end the turn twice. A session that
// neither a session event nor the client ever names stays replyless on
// purpose: an unknown session might be a subagent's, and its text is not the
// user's turn, so privacy wins over completeness.
//
// A known root's turn whose last assistant message has no text (it ended on a
// tool call, or said nothing) is still its turn end, and its idle report says
// so with an empty `reply`, so the deck can say the turn ended with nothing to
// read. Only the first idle report of a turn carries either: the state is
// consumed here, so the second report finds no assistant message and carries
// none.
const takeReply = (sessionId) => {{
  if (!sessionId) {{
    return {{}};
  }}
  const messageId = lastAssistantMessage.get(sessionId);
  const failed = failedTurns.has(sessionId);
  const aborted = abortedTurns.has(sessionId);
  const message = messageId ? replyMessages.get(messageId) : undefined;
  forgetReplyState(sessionId);
  if (!message || aborted) {{
    return {{}};
  }}
  if (sessionParents.get(sessionKey(sessionId)) !== "") {{
    lookUpAncestry(sessionId);
    return {{}};
  }}
  const text = Array.from(message.parts.values(), (part) => part.text).join("").trim();
  return failed ? {{ reply: text, reply_failed: true }} : {{ reply: text }};
}};

const isIdleReport = (payload) =>
  payload.event === "session.idle" ||
  (payload.event === "session.status" && payload.status === "idle");

const recordUserMessage = (event, directory) => {{
  const info = event?.properties?.info;
  const messageId = info?.id;
  if (!idFits(messageId)) {{
    return;
  }}
  // A message from a session id over the bound is not kept, so its prompt is
  // not reported.
  if (info?.sessionID !== undefined && info?.sessionID !== null && !idFits(info.sessionID)) {{
    messageRoles.delete(messageId);
    return;
  }}
  const role = (info?.role ?? "").toLowerCase();
  // Issue #1678: a child session's user message is the prompt its subagent
  // was given, not the user's.
  if (role !== "user" || isChildSession(info?.sessionID)) {{
    messageRoles.delete(messageId);
    if (role === "assistant") {{
      recordAssistantMessage(info);
    }}
    return;
  }}
  const dir = info?.directory ?? directory ?? process.cwd();
  // The session that sent it is kept as well, so a message whose session is
  // only later known to be a child is still recognised as the child's.
  rememberBounded(
    messageRoles,
    messageId,
    {{
      role,
      sessionId: normalizeSessionId(info.sessionID ?? null, dir),
      rawSessionKey: sessionKey(info.sessionID),
    }},
    MAX_PENDING_USER_MESSAGES
  );
}};

const emitUserPrompt = (sessionId, prompt, directory) => {{
  const text = (prompt ?? "").trim();
  if (!text) {{
    return;
  }}
  ensureSessionRegistered(sessionId, directory);
  const sessionInfo = knownSessions.get(sessionId);
  sendEvent({{
    session_id: sessionId,
    event: "session.prompt",
    prompt: text,
    cwd: directory ?? sessionInfo?.cwd ?? process.cwd(),
  }});
}};

const handleMessagePartUpdated = (event, directory) => {{
  const part = event?.properties?.part;
  if (!part?.messageID || part.type !== "text") {{
    return;
  }}
  const info = messageRoles.get(part.messageID);
  if (!info || info.role !== "user") {{
    recordReplyPart(part);
    return;
  }}
  if (!part.text) {{
    return;
  }}
  // Issue #1993: text OpenCode adds to a user message itself (a background
  // task's result, the turn that continues after a compaction, a file it
  // read for an attachment) is marked synthetic and is not what the user
  // typed. The message stays recorded, so its typed text is still reported.
  if (part.synthetic) {{
    return;
  }}
  // Issue #1678: its session may have become known as a child since the
  // message was recorded.
  if (isChildSession(info.rawSessionKey) || isChildSession(part.sessionID)) {{
    messageRoles.delete(part.messageID);
    return;
  }}
  const sessionId = normalizeSessionId(
    info.sessionId ?? defaultSessionId(event?.properties?.sessionID),
    directory
  );
  emitUserPrompt(sessionId, part.text, directory);
  messageRoles.delete(part.messageID);
}};

{inbox}
{usage}
{entry}
"#
    )
}

/// The server plugin's entry points, after its shared state and helpers
/// ([`plugin_template`]): the event handler both OpenCode generations feed
/// (1.x hands it its own events; 2.x's events are translated into the same
/// shape first, [`SERVER_V2_JS`]), the tool-call reports, and the 1.x
/// factory, whose behaviour is unchanged by PRD #2050. The module's default
/// export, at the end of [`SERVER_V2_JS`], carries both generations.
const SERVER_ENTRY_JS: &str = r#"// One OpenCode event, in OpenCode 1.x's shape (`{ type, properties }`), as
// the deck reports it. `client` is what a held question's answer is replied
// through (awaitAnswer).
const handleEvent = async (event, directory, client) => {
  const eventType = event?.type ?? "";
  if (
    eventType === "message.created" ||
    eventType === "message.updated"
  ) {
    recordUserMessage(event, directory);
    return;
  }
  if (eventType === "message.part.updated") {
    handleMessagePartUpdated(event, directory);
    return;
  }
  if (eventType === "message.part.delta") {
    recordReplyDelta(event?.properties);
    return;
  }
  if (
    eventType === "permission.asked" ||
    eventType === "permission.replied" ||
    eventType === "question.asked" ||
    eventType === "question.replied" ||
    eventType === "question.rejected"
  ) {
    // Issue #1678: a child session's prompt is its subagent's, filed under
    // the root with the child's id, like its tool calls. PRD #1631: and,
    // like the root's, the ask goes through `await-answer` so the deck can
    // answer it, and the reply names the request it answers. A question
    // form is the subagent's in the same way as a permission prompt.
    const props = event?.properties ?? {};
    const askingId = props.sessionID ?? props.sessionId;
    if (isChildSession(askingId)) {
      const payload = subagentPayload(undefined, askingId, directory, {
        event: eventType,
        prompt: permissionPrompt(props),
      });
      if (eventType === "permission.asked" || eventType === "question.asked") {
        awaitAnswer(client, { ...payload, properties: props }, () => sendEvent(payload));
        return;
      }
      payload.request_id = props.requestID ?? props.id;
      noteResolved(payload.request_id);
      stopHeldAnswer(payload.request_id);
      sendEvent(payload);
      return;
    }
  }
  if (eventType === "permission.asked" || eventType === "question.asked") {
    // PRD #1631: reported through `await-answer`, which also waits for
    // the deck's answer. A prompt that cannot be held, or whose child
    // fails before it is done with it, is reported plainly, with no
    // `properties`, so the card still shows it.
    const payload = questionPayload(event, directory);
    ensureSessionRegistered(payload.session_id, payload.cwd);
    const plain = permissionPayload(event, directory);
    awaitAnswer(client, payload, () => sendEvent(plain));
    return;
  }
  if (
    eventType === "permission.replied" ||
    eventType === "question.replied" ||
    eventType === "question.rejected"
  ) {
    const payload = permissionPayload(event, directory);
    noteResolved(payload.request_id);
    stopHeldAnswer(payload.request_id);
    ensureSessionRegistered(payload.session_id, payload.cwd);
    sendEvent(payload);
    return;
  }
  if (eventType === "server.instance.disposed") {
    flushSessions();
    return;
  }
  if (!event?.type?.startsWith("session.")) {
    return;
  }
  if (eventType === "session.created" || eventType === "session.updated") {
    noteSessionAncestry(event?.properties?.info);
  }
  const ownId = event?.properties?.sessionID ?? event?.properties?.info?.id;
  if (isChildSession(ownId)) {
    handleChildSessionEvent(event, ownId, directory);
    return;
  }
  const payload = sessionPayload(event, directory);
  if (event?.type === "session.deleted") {
    const deletedId = event?.properties?.info?.id ?? event?.properties?.sessionID;
    if (deletedId) {
      forgetReplyState(deletedId);
      sessionParents.delete(sessionKey(deletedId));
      cancelAncestryLookup(deletedId);
    }
    closeSession(payload.session_id, payload.cwd, false, false);
    return;
  }
  ensureSessionRegistered(
    payload.session_id,
    payload.cwd,
    payload.status,
    event?.type !== "session.created"
  );
  const rawSessionId = event?.properties?.sessionID;
  if (event?.type === "session.error") {
    recordTurnError(rawSessionId, event?.properties?.error);
  }
  sendEvent(isIdleReport(payload) ? { ...payload, ...takeReply(rawSessionId) } : payload);
};

// A tool call's start or end (`tool.execute.before` / `tool.execute.after`),
// as the deck reports it. A child session's call is its subagent's.
const reportTool = (eventName, sessionID, tool, args, callID, directory) => {
  if (isChildSession(sessionID)) {
    sendEvent(
      subagentPayload(undefined, sessionID, directory, {
        event: eventName,
        tool_name: tool,
        tool_input: args,
        call_id: callID,
      })
    );
    return;
  }
  const sessionId = normalizeSessionId(defaultSessionId(sessionID), directory);
  ensureSessionRegistered(sessionId, directory);
  sendEvent({
    session_id: sessionId,
    event: eventName,
    tool_name: tool,
    tool_input: args,
    call_id: callID,
    cwd: directory,
  });
};

// PRD #2050: an OpenCode 1.x loader that predates the `{ id, server }`
// default object calls every function export, deduplicated by export value,
// so it may call this factory twice — once as `DotAgentDeckPlugin`, once as
// the default object's `server` — with the same context. The second call
// with a context already seen returns inert hooks. A later instance (OpenCode
// re-running the plugin after disposing one) gets a new context and loads.
// 1.18.35 takes `default.server` alone (measured), so this never fires there.
const DAD_SEEN_CONTEXTS = typeof WeakSet === "function" ? new WeakSet() : undefined;
const V1_SERVER_CALLED = "__dotAgentDeckV1ServerCalled";

export const DotAgentDeckPlugin = async (ctx) => {
  // PRD #2050: this process runs OpenCode 1.x, which calls `server`. 1.19.5
  // also calls the module's 2.x `setup` (measured: after `server`, in the
  // full TUI's worker and in `--mini` once its first session exists), which
  // must then stay inert (setupV2).
  globalThis[V1_SERVER_CALLED] = true;
  // A second copy of this plugin in the same OpenCode process returns inert
  // hooks, so one real action produces one event. See DAD_GUARD above.
  if (DAD_ALREADY_LOADED) {
    return { event: async () => {} };
  }
  if (ctx && typeof ctx === "object" && DAD_SEEN_CONTEXTS) {
    if (DAD_SEEN_CONTEXTS.has(ctx)) {
      return { event: async () => {} };
    }
    DAD_SEEN_CONTEXTS.add(ctx);
  }

  const directory = ctx?.directory ?? process.cwd();
  sessionClient = ctx?.client;
  startInbox(ctx?.client);

  return {
    "chat.message": async (input, output) => interceptDoorbell(input, output),
    event: async (input) => {
      const event = input?.event;
      const eventType = event?.type ?? "";
      observeInbox(event);
      if (eventType === "message.updated") {
        recordUsage(event?.properties?.info);
      }
      if (eventType === "message.part.updated") {
        recordUsageStep(event?.properties?.part);
      }
      await handleEvent(event, directory, ctx?.client);
    },
    "tool.execute.before": async (input, output) =>
      reportTool("tool.execute.before", input?.sessionID, input?.tool, output?.args, input?.callID, directory),
    "tool.execute.after": async (input) =>
      reportTool("tool.execute.after", input?.sessionID, input?.tool, input?.args, input?.callID, directory),
  };
};

"#;

/// PRD #2050: the server plugin's OpenCode 2.x implementation, the default
/// export's `setup`. OpenCode 2.x has no 1.x-style hooks object: it hands
/// `setup` a context whose `event.subscribe` streams `{ type, data,
/// location }` events, and whose `tool.hook` / `session.hook` register
/// callbacks. Each event the deck reports is translated into the 1.x shape
/// and fed to the same handler 1.x uses ([`SERVER_ENTRY_JS`]), so the deck
/// receives the same `hook --agent opencode` payloads from either generation
/// and nothing in the daemon's protocol changes.
///
/// It never reports from OpenCode 2.x's shared background service (`serve
/// --service`), whose environment is not the pane's; the deck starts a 2.x it
/// can see with `--standalone`, whose private server inherits the pane's.
const SERVER_V2_JS: &str = r#"// --- OpenCode 2.x (PRD #2050) ---
//
// Where this runs, measured on 2.2.5: `opencode --standalone` (and `mini`,
// `run` with it) starts its own server as a child, `opencode serve --stdio
// --port 0`, which inherits the client's environment, so DOT_AGENT_DECK_PANE_ID
// and the deck's other variables are the pane's. Without `--standalone` the
// client attaches to the shared background service (`opencode serve
// --service`, read from 2.2.5's service launcher), started once with
// whichever environment came first and shared by every pane and project:
// there `process.env` is NOT the pane's, and every report would name the
// wrong pane or none. So this implementation stays inert in the background
// service, and reports everywhere else.
const v2ServerArgs = () => process.argv.slice(2);
const V2_SERVICE = () => v2ServerArgs().includes("--service");
// OpenCode 2.x's subcommands (2.2.5 `--help`); any other first word of the
// client's arguments is the top-level TUI (a flag, or a directory argument).
const V2_SUBCOMMANDS = new Set(["upgrade", "update", "uninstall", "acp", "api", "debug", "auth",
  "mcp", "plugin", "models", "stats", "mini", "run", "session", "service", "reload", "pair", "serve"]);
// Flags that take the next word as their value (2.2.5 `--help`); every other
// flag is a switch. Flags may come before the subcommand (`opencode --auto
// mini`, accepted by 2.2.5), so the subcommand is the first word that is
// neither (the deck's Rust reads it the same way: opencode_version's
// first_positional).
const V2_VALUE_FLAGS = new Set(["--server", "--session", "-s", "--prompt", "--log-level",
  "--completions", "--model", "-m", "--agent", "--replay-limit", "--port", "--hostname"]);
const v2FirstPositional = (args) => {
  for (let n = 0; n < args.length; n++) {
    const arg = args[n];
    if (typeof arg !== "string") return undefined;
    if (arg === "--") return args[n + 1];
    if (arg.startsWith("-") && arg.length > 1) {
      if (!arg.includes("=") && V2_VALUE_FLAGS.has(arg)) n++;
      continue;
    }
    return arg;
  }
  return undefined;
};
// An OpenCode binary's file name: `opencode`, or `opencode` and a version
// (`opencode2`, `opencode-1.18.35`), optionally `.exe` (opencode_version's
// opencode_name).
const v2OpencodeName = (path) => {
  const name = String(path ?? "").split("/").pop().replace(/\.exe$/, "");
  return /^opencode(?:[-.]?\d[\w.-]*)?$/.test(name);
};
// The parent of this `serve` process, read once: its arguments (program
// first) and whether it runs an OpenCode binary, by its executable or, only
// where that cannot be read (no /proc: macOS), its first argument. Undefined
// when its arguments cannot be read (no /proc and no `ps`). On macOS the
// arguments come from `ps`, split on whitespace, so a program path holding a
// space splits and its first argument names no OpenCode binary: the plugin
// then stays inert there (known, untested: no macOS host).
let v2ParentMemo;
const v2Parent = () => {
  if (v2ParentMemo !== undefined) return v2ParentMemo || undefined;
  v2ParentMemo = null;
  const ppid = process.ppid;
  if (!ppid || ppid <= 1) return undefined;
  let argv;
  try {
    argv = readFileSync(`/proc/${ppid}/cmdline`, "utf8").split("\0").filter(Boolean);
  } catch (_) {
    try {
      const text = execFileSync("ps", ["-o", "args=", "-p", String(ppid)], {
        timeout: 2000,
        stdio: ["ignore", "pipe", "ignore"],
        encoding: "utf8",
      });
      argv = text.trim().split(/\s+/).filter(Boolean);
    } catch (_) {}
  }
  if (!argv?.length) return undefined;
  // A binary replaced in place since the parent started (`opencode upgrade`,
  // OpenCode's own update, a reinstall) reads `…/opencode (deleted)` here on
  // Linux; the suffix is not part of the file's name.
  let exe;
  try {
    exe = String(readlinkSync(`/proc/${ppid}/exe`)).replace(/ \(deleted\)$/, "");
  } catch (_) {}
  v2ParentMemo = { argv, opencode: v2OpencodeName(exe !== undefined ? exe : argv[0]) };
  return v2ParentMemo;
};
// The arguments of the client this plugin serves. In a `serve` process they
// are its parent's (measured on 2.2.5: a standalone server's parent is the
// `opencode` / `opencode mini` / `opencode run` the user started); in any
// other process, its own. Undefined when the parent's cannot be read.
const v2ClientArgs = () => {
  const own = v2ServerArgs();
  if (v2FirstPositional(own) !== "serve") return own;
  return v2Parent()?.argv.slice(1);
};
// "tui" (the full TUI), a subcommand name, or undefined when the client's
// arguments cannot be read.
const v2ClientMode = () => {
  const args = v2ClientArgs();
  if (!args) return undefined;
  const first = v2FirstPositional(args);
  return typeof first === "string" && V2_SUBCOMMANDS.has(first) ? first : "tui";
};
// Audit L5: whether this process serves a pane's own OpenCode session, the
// only place the plugin reports from. A `serve` process does only when its
// parent runs an OpenCode binary in a session mode (the full TUI, `mini` or
// `run`): a `serve` a shell started (a server shared with `--server`), the
// background service under whatever argv a release gives it, or a parent
// that cannot be read is not one, and the plugin stays inert there. Any other
// process is a client running the plugin itself.
const V2_SESSION_MODES = new Set(["tui", "mini", "run"]);
const v2ServesPaneSession = () => {
  if (V2_SERVICE()) return false;
  if (v2FirstPositional(v2ServerArgs()) !== "serve") return true;
  const parent = v2Parent();
  return !!parent?.opencode && V2_SESSION_MODES.has(v2ClientMode());
};

// The 2.x shell tool is `shell`; 1.x called it `bash`, and the deck reads a
// tool named `shell` as Codex's joined argv (telemetry::operation). Its input
// is `{ command, workdir, timeout, background }` (measured on 2.2.5's tool
// list), the key `bash` carried in 1.x, so it is reported as `bash`.
const v2ToolName = (tool) => (tool === "shell" ? "bash" : tool);

// Issue #714 on 2.x: a failed provider response's allow-listed headers, by
// session, from `session.hook("http.response")` — 2.x's error no longer
// carries them. Only the headers errorFields forwards are kept, at most
// MAX_REPLY_SESSIONS sessions' worth.
const v2FailedHeaders = new Map();
const v2NoteResponse = (input) => {
  const response = input?.response;
  if (!response || response.ok !== false || !idFits(input?.sessionID)) return;
  const kept = {};
  let count = 0;
  try {
    response.headers?.forEach?.((value, name) => {
      const key = String(name).toLowerCase();
      if (count < 32 && typeof value === "string" && (ERROR_HEADER_ALLOW.has(key) || key.startsWith(ERROR_HEADER_PREFIX))) {
        kept[key] = value.slice(0, 200);
        count += 1;
      }
    });
  } catch (_) {}
  rememberBounded(v2FailedHeaders, input.sessionID, kept);
};

// Permission requests' sessions, by request id: 2.x's reply names both.
const v2PermissionSessions = new Map();

// What a held question's answer is replied through on 2.x (awaitAnswer's
// `client`): `permission.reply`, a public API, replaces 1.x's POST through
// the SDK internal. 2.x's server context has no form reply, so a form is
// never held (v2FormEvent) and never answered here.
const v2Replier = (ctx) => ({
  dadReply: async (answer) => {
    const refused = (detail) => ({ delivered: false, detail: String(detail).slice(0, 200) });
    if (answer?.kind !== "permission") return refused("only permission prompts are answered on OpenCode 2");
    const requestID = answer.request_id;
    const sessionID = v2PermissionSessions.get(requestID);
    const decision = answer.body?.reply;
    if (typeof requestID !== "string" || !sessionID) return refused("the answer names no known request");
    if (decision !== "once" && decision !== "always" && decision !== "reject") return refused("the answer is not a permission reply");
    try {
      await ctx.permission.reply({ sessionID, requestID, decision });
      return { delivered: true };
    } catch (error) {
      return refused(error?.message ?? "the reply could not be sent");
    }
  },
});

// Usage (PRD #1858) on 2.x: each step reports its own counts on
// `session.step.ended`, and its model on `session.step.started`. A message's
// cost is the sum of its steps', as 1.x's message cost was.
const v2StepModels = new Map();
const v2StepCounts = new Map();
const v2Costs = new Map();
const v2RecordStepStarted = (data) => {
  if (!USAGE_FILE_ON || !idFits(data?.assistantMessageID)) return;
  rememberBounded(v2StepModels, data.assistantMessageID, data.model?.id, MAX_USAGE_MESSAGES);
};
const v2RecordStepEnded = (data, failed) => {
  if (!USAGE_FILE_ON || !idFits(data?.assistantMessageID)) return;
  const id = data.assistantMessageID;
  const n = (v2StepCounts.get(id) ?? 0) + 1;
  rememberBounded(v2StepCounts, id, n, MAX_USAGE_MESSAGES);
  const cost = typeof data.cost === "number" && Number.isFinite(data.cost) ? data.cost : 0;
  const total = (v2Costs.get(id) ?? 0) + cost;
  rememberBounded(v2Costs, id, total, MAX_USAGE_MESSAGES);
  if (!failed) recordUsageStep({ type: "step-finish", messageID: id, id: `${id}:${n}`, tokens: data.tokens });
  recordUsage({
    role: "assistant",
    id,
    modelID: v2StepModels.get(id),
    sessionID: data.sessionID,
    tokens: failed ? undefined : data.tokens,
    cost: total,
    finish: failed ? undefined : data.finish,
    error: failed ? { name: data.error?.type, data: { statusCode: data.error?.status } } : undefined,
    time: failed ? {} : { completed: 1 },
  });
};

// Sessions reported working since they were last reported idle. Measured on
// 2.2.5 (`opencode run`): the event stream carries no `session.created`,
// `session.status` or `session.idle` at all; a turn is `session.execution.started`
// and ends `succeeded`, `failed` or `interrupted`. So a turn's start is
// reported as 1.x's `busy` status and its end as 1.x's `idle` status (after
// the error report for a failed or interrupted turn, as 1.x's `session.error`
// was followed by idle), once per turn: a session not in this set is not
// ended again, so a real idle event and an execution end cannot both end it.
const v2Busy = new Set();

// Measured on 2.2.5: no `session.created` reaches the plugin, so whether a
// session is a subagent's child is asked of OpenCode (`session.get`, at most
// V2_ANCESTRY_MS) the first time it reports, before its report is made.
const V2_ANCESTRY_MS = 1000;
const v2Ancestry = async (ctx, sessionID) => {
  const key = sessionKey(sessionID);
  if (key === undefined || sessionParents.has(key) || typeof ctx?.session?.get !== "function") return;
  let timer;
  try {
    const info = await Promise.race([
      Promise.resolve(ctx.session.get({ sessionID })),
      new Promise((resolve) => {
        timer = setTimeout(() => resolve(undefined), V2_ANCESTRY_MS);
        timer?.unref?.();
      }),
    ]);
    const answer = info?.data ?? info;
    if (answer?.id === sessionID && !sessionParents.has(key)) {
      noteSessionAncestry({ id: sessionID, parentID: answer.parentID });
    }
  } catch (_) {
  } finally {
    clearTimeout(timer);
  }
};

// A 2.x event's directory: where it happened, else where the server runs.
const v2Directory = (event, ctx) =>
  event?.location?.directory ?? event?.data?.location?.directory ?? ctx?.location?.directory ?? process.cwd();

// A 2.x event as the 1.x-shaped events handleEvent reports, in order (none
// for an event the deck takes no report from).
const v2Translate = (event, directory) => {
  const data = event?.data ?? {};
  const sessionID = data.sessionID;
  const idle = () =>
    v2Busy.has(sessionID) ? [{ type: "session.status", properties: { sessionID, status: { type: "idle" } } }] : [];
  switch (event?.type) {
    case "session.execution.succeeded":
      return idle();
    case "session.execution.failed":
    case "session.execution.interrupted":
      return [v2TranslateOne(event, directory), ...idle()];
    default: {
      const one = v2TranslateOne(event, directory);
      return one ? [one] : [];
    }
  }
};

// One 2.x event in 1.x's shape; undefined for one with no 1.x counterpart.
const v2TranslateOne = (event, directory) => {
  const data = event?.data ?? {};
  const sessionID = data.sessionID;
  switch (event?.type) {
    case "session.created":
      return { type: "session.created", properties: { info: { id: sessionID, parentID: data.parentID, directory } } };
    case "session.status":
      return { type: "session.status", properties: { sessionID, status: data.status } };
    case "session.idle":
      return { type: "session.idle", properties: { sessionID } };
    case "session.deleted":
      return { type: "session.deleted", properties: { sessionID, info: { id: sessionID } } };
    case "session.execution.started":
      return v2Busy.has(sessionID)
        ? undefined
        : { type: "session.status", properties: { sessionID, status: { type: "busy" } } };
    case "session.execution.failed": {
      // Issue #714: the same error fields 1.x's `session.error` carried, so
      // the deck classifies a quota or credit refusal the same way. 2.x
      // names the error `type` where 1.x had `name`.
      const error = data.error ?? {};
      const headers = v2FailedHeaders.get(sessionID);
      v2FailedHeaders.delete(sessionID);
      return {
        type: "session.error",
        properties: {
          sessionID,
          error: {
            name: error.type,
            data: { message: error.message, responseBody: error.response?.body, responseHeaders: headers },
          },
        },
      };
    }
    case "session.execution.interrupted":
      // An interrupted turn is 1.x's aborted message: the turn ending, which
      // the deck reads as Idle, not Error.
      return { type: "session.error", properties: { sessionID, error: { name: "MessageAbortedError" } } };
    case "permission.asked":
      if (typeof data.id === "string" && idFits(sessionID)) {
        rememberBounded(v2PermissionSessions, data.id, sessionID);
      }
      // 2.x renamed the properties the deck reads (`permission` → `action`,
      // `patterns` → `resources`, `always` → `save`, `tool.callID` →
      // `source.id`); they are given back their 1.x names, so the deck's
      // question builder reads either generation's prompt.
      return {
        type: "permission.asked",
        properties: {
          id: data.id,
          sessionID,
          permission: data.action,
          patterns: Array.isArray(data.resources) ? data.resources : [],
          always: Array.isArray(data.save) ? data.save : [],
          metadata: data.metadata,
          message: data.message,
          ...(data.source?.id ? { tool: { callID: data.source.id, messageID: data.source.messageID } } : {}),
        },
      };
    case "permission.replied":
      v2PermissionSessions.delete(data.requestID);
      return { type: "permission.replied", properties: { sessionID, requestID: data.requestID, reply: data.reply } };
    default:
      return undefined;
  }
};

// A question form (`form.*`). 2.x's server context has no form reply. In the
// full TUI the deck's TUI plugin holds a new form through `await-answer` and
// answers it (PRD #2050), so `form.created` is left to it here. Anywhere else
// — `opencode mini` loads no TUI plugin — the form is reported plainly, the
// card saying it is answered in OpenCode. Its reply or cancellation clears it
// either way.
const V2_FORM_IN_OPENCODE = "answer this form in OpenCode";
const v2FormEvent = (event, directory) => {
  const data = event?.data ?? {};
  if (event.type === "form.created" && v2ClientMode() === "tui") return;
  const form = event.type === "form.created" ? data.form ?? {} : data;
  const sessionID = form.sessionID;
  const requestID = form.id;
  const name = event.type === "form.created" ? "question.asked" : event.type === "form.replied" ? "question.replied" : "question.rejected";
  const title =
    name === "question.asked"
      ? (typeof form.title === "string" && form.title ? `${V2_FORM_IN_OPENCODE}: ${form.title}` : V2_FORM_IN_OPENCODE)
      : form.title;
  const props = { sessionID, id: requestID, title };
  if (isChildSession(sessionID)) {
    const payload = subagentPayload(undefined, sessionID, directory, { event: name, prompt: permissionPrompt(props) });
    if (name !== "question.asked") payload.request_id = requestID;
    sendEvent(payload);
    return;
  }
  const payload = permissionPayload({ type: name, properties: props }, directory);
  ensureSessionRegistered(payload.session_id, payload.cwd);
  sendEvent(payload);
};

// Assistant text (PRD #1497's reply) on 2.x: each text block's final text
// arrives on `session.text.ended`, numbered within its message by `ordinal`.
const v2RecordText = (data) => {
  if (!idFits(data?.assistantMessageID) || !idFits(data?.sessionID)) return;
  recordAssistantMessage({ id: data.assistantMessageID, sessionID: data.sessionID });
  recordReplyPart({
    type: "text",
    id: `${data.assistantMessageID}:${data.ordinal}`,
    messageID: data.assistantMessageID,
    sessionID: data.sessionID,
    text: data.text,
  });
};

// One 2.x event: its usage, inbox and reply bookkeeping, then the deck's
// report through the 1.x handler.
const v2HandleEvent = async (event, ctx, replier) => {
  const type = event?.type ?? "";
  const data = event?.data ?? {};
  const directory = v2Directory(event, ctx);
  observeInboxV2(event);
  if (data.sessionID && type !== "session.created" && type !== "session.deleted") {
    await v2Ancestry(ctx, data.sessionID);
  }
  if (type === "session.status") {
    if (data.status?.type === "idle") v2Busy.delete(data.sessionID);
    else if (data.sessionID) rememberBounded(v2Busy, data.sessionID);
  } else if (type === "session.idle" || type === "session.deleted") {
    v2Busy.delete(data.sessionID);
  } else if (type === "session.step.started") {
    v2RecordStepStarted(data);
  } else if (type === "session.step.ended") {
    v2RecordStepEnded(data, false);
  } else if (type === "session.step.failed") {
    v2RecordStepEnded(data, true);
  } else if (type === "session.text.ended") {
    v2RecordText(data);
    return;
  } else if (type === "form.created" || type === "form.replied" || type === "form.cancelled") {
    v2FormEvent(event, directory);
    return;
  } else if (type === "location.shutdown") {
    flushSessions();
    return;
  }
  for (const translated of v2Translate(event, directory)) {
    const status = translated.type === "session.status" ? translated.properties.status?.type : undefined;
    if (status === "busy") rememberBounded(v2Busy, translated.properties.sessionID);
    else if (status === "idle") v2Busy.delete(translated.properties.sessionID);
    await handleEvent(translated, directory, replier);
  }
};

// The default export's `setup`: OpenCode 2.x's plugin entry point. Returns
// the cleanup 2.x runs when it unloads the plugin (measured on 2.2.5: on
// server shutdown).
const setupV2 = async (ctx) => {
  if (!v2ServesPaneSession()) {
    return () => {};
  }
  // OpenCode 1.19.5 calls `setup` too, after `server`, with a context of its
  // own (`options`, `agent`, `catalog`, …: no `event`, no `session`; measured).
  // The 1.x implementation already serves this process: run here as well and
  // the 2.x inbox would read `--mini` as the full TUI, announce, and replace
  // mini's receiver with a doorbell-only one.
  if (globalThis[V1_SERVER_CALLED] || typeof ctx?.event?.subscribe !== "function") {
    return () => {};
  }
  // 2.x may load the plugin again in this module after unloading it (its
  // cleanup ran handleShutdown): the new instance reports again.
  shuttingDown = false;
  const directory = ctx?.location?.directory ?? process.cwd();
  const replier = v2Replier(ctx);
  // The ancestry lookup's client (lookUpAncestry), in 1.x's call shape.
  sessionClient = {
    session: {
      get: ({ path }) => ctx.session.get({ sessionID: path?.id }),
    },
  };
  const registrations = [];
  const register = async (domain, name, callback) => {
    try {
      const registration = await domain?.hook?.(name, callback);
      if (registration) registrations.push(registration);
    } catch (_) {}
  };
  await register(ctx.session, "prompt", async (input) => {
    try {
      await interceptDoorbellV2(input);
      const sessionID = input?.sessionID;
      if (!sessionID) return;
      await v2Ancestry(ctx, sessionID);
      if (isChildSession(sessionID)) return;
      emitUserPrompt(normalizeSessionId(sessionID, directory), input?.prompt?.text, directory);
    } catch (_) {}
  });
  await register(ctx.session, "http.response", (input) => v2NoteResponse(input));
  await register(ctx.tool, "execute.before", async (input) => {
    await v2Ancestry(ctx, input?.sessionID);
    reportTool("tool.execute.before", input?.sessionID, v2ToolName(input?.tool), input?.input, input?.id, directory);
  });
  // A failed call ends like a completed one (`status: "error"`), so the
  // card's tool ends either way, as it did on 1.x.
  await register(ctx.tool, "execute.after", async (input) => {
    await v2Ancestry(ctx, input?.sessionID);
    reportTool("tool.execute.after", input?.sessionID, v2ToolName(input?.tool), input?.input, input?.id, directory);
  });
  startInboxV2(ctx);
  const subscription = typeof AbortController === "function" ? new AbortController() : undefined;
  let stopped = false;
  (async () => {
    try {
      const stream = ctx.event.subscribe(subscription ? { signal: subscription.signal } : undefined);
      for await (const event of stream) {
        if (stopped) break;
        try {
          await v2HandleEvent(event, ctx, replier);
        } catch (_) {}
      }
    } catch (_) {}
  })();
  return async () => {
    stopped = true;
    try {
      subscription?.abort();
    } catch (_) {}
    stopInboxV2();
    for (const registration of registrations.splice(0)) {
      try {
        await registration.dispose();
      } catch (_) {}
    }
    handleShutdown();
  };
};

// Both generations from one module (PRD #2050): OpenCode 1.x (1.18.29+)
// takes `{ id, server }` and calls `server`; OpenCode 2.x takes `{ id, setup }`
// and calls `setup`. Measured: 1.18.35 called `server` once and never `setup`,
// and 2.2.5 called `setup` and never `server`; 1.19.5 calls `server` and then
// `setup` too, which the V1_SERVER_CALLED guard at the top of `setupV2` keeps
// inert. Versions not measured may call either or both.
export default { id: "dot-agent-deck", server: DotAgentDeckPlugin, setup: setupV2 };
"#;

/// PRD #2050: the server plugin's inbox roles on OpenCode 2.x, after the 1.x
/// ones ([`SERVER_INBOX_JS`]) and sharing their state.
///
/// - Under the full TUI (whose own TUI plugin is handed the deck's messages,
///   [`TUI_V2_JS`]) it announces, keeps a doorbell-only receiver, and catches
///   a submitted doorbell line before the model sees it.
/// - Under `opencode mini`, which loads no TUI plugin (measured on 2.2.5), it
///   arms its own stream and starts each message in mini's session with
///   `session.prompt`.
/// - Under `run`, `session` or another subcommand it does nothing: that is
///   not the pane's interactive OpenCode.
///
/// Which client it serves is read from its parent's argv (`v2ClientMode`).
const SERVER_INBOX_V2_JS: &str = r#"// --- OpenCode 2.x inbox (PRD #2050) ---
let v2InboxStarted = false;
const v2InboxInteractive = () => {
  const mode = v2ClientMode();
  return mode === undefined || mode === "tui" || mode === "mini";
};
const v2InboxMini = () => v2ClientMode() === "mini";
// A 2.x client call in the shape inboxHandOver reads: what it returned, or,
// when it threw an error naming an HTTP status, `{ error, response: { status } }`
// so a refusal before acceptance (4xx) is told apart from a lost call.
const v2Call = (call) => async () => {
  try {
    return await call();
  } catch (error) {
    const status = error?.status ?? error?.response?.status ?? error?.cause?.status;
    if (typeof status === "number") return { error, response: { status } };
    throw error;
  }
};
// Record which deck message ids an OpenCode inbox item carries.
const v2NotePending = (inboxID, text) => {
  const receiver = serverReceiver();
  if (!receiver || !inboxID) return;
  const ids = inboxIdsIn(text).filter((id) => receiver.of(id).length > 0);
  if (ids.length) {
    inboxRemember(miniPending, inboxID, [...new Set([...(miniPending.get(inboxID) ?? []), ...ids])]);
  }
};
// The session `opencode mini` shows, when this plugin did not see it created.
// 2.x mini creates (or resumes) its session as it starts, through the server
// API, before this plugin's event stream is subscribed (measured on 2.2.5: the
// session existed 0.2 s before `setup` ran, and no `session.created` reached
// the plugin), and draws only that session. So it is found the way mini finds
// it (2.2.5's mini resolver): `--session <id>` names it; `--continue` takes the
// newest root session of the directory; otherwise mini created a fresh one.
// A `--fork` is a new session too.
//
// A fresh session is bound by WHEN it was created, not by its order:
// sessions live in OpenCode's shared storage, and 2.2.5 records nothing on a
// session that names the server or client that created it (measured: its
// session table has no such column), so another root session in the same
// directory (a second mini, the user's own OpenCode, a scheduled `opencode
// run`) is listed too. The candidates are the root sessions of the directory
// created between this server's start and the moment the mini receiver
// started (`setupAt`, v2ResolveMiniTarget), plus V2_MINI_SLACK_MS. Mini's own
// session falls there because mini creates it before `setup` (the 0.2 s
// above), whatever the box's load. Exactly one candidate is mini's session.
// More than one (another mini started in the same directory while this one
// booted) cannot be told apart: the plugin delivers nothing and reports each
// message not handed, so the deck keeps it and shows it as undelivered rather
// than it reaching another pane's session. None found within
// V2_MINI_RESOLVE_MS: no root is taken by age; a `session.created` this
// plugin observes (observeInboxV2) still names mini's session.
//
// The residual, not measured: a sibling created inside the same window is
// indistinguishable from mini's own, which is why it is refused rather than
// guessed; and a mini whose own session is listed only after
// V2_MINI_RESOLVE_MS, or created later than V2_MINI_SLACK_MS after `setup`,
// gets no deck messages until it restarts.
const V2_MINI_RESOLVE_MS = 5000;
const V2_MINI_SLACK_MS = 500;
const v2ClientFlag = (args, long, short) => {
  for (let n = 0; n < args.length; n++) {
    const arg = args[n];
    if (arg === long || (short && arg === short)) return args[n + 1];
    if (typeof arg === "string" && arg.startsWith(long + "=")) return arg.slice(long.length + 1);
  }
  return undefined;
};
// One look: `{ id }` when mini's session is known, `{ ambiguous: n }` when it
// cannot be told from another one, undefined while it may still appear.
// `started` is this server's start, `setupAt` the mini receiver's.
const v2FindMiniTarget = async (ctx, started, setupAt) => {
  const args = v2ClientArgs() ?? [];
  const fork = args.includes("--fork");
  const named = v2ClientFlag(args, "--session", "-s");
  if (named && !fork) return { id: named };
  const directory = ctx?.location?.directory ?? process.cwd();
  const listed = await ctx.session.list({ directory, parentID: null, limit: 20, order: "desc" });
  const roots = (Array.isArray(listed?.data) ? listed.data : []).filter(
    (s) => typeof s?.id === "string" && !s.parentID && (s.location?.directory ?? directory) === directory,
  );
  if (!fork && (args.includes("--continue") || args.includes("-c"))) {
    return roots[0] ? { id: roots[0].id } : undefined;
  }
  const closes = setupAt + V2_MINI_SLACK_MS;
  const candidates = roots.filter(
    (s) => typeof s.time?.created === "number" && s.time.created >= started && s.time.created <= closes,
  );
  if (candidates.length > 1) return { ambiguous: candidates.length };
  // A session created inside the window may still be listed until it closes.
  if (candidates.length === 1 && Date.now() > closes) return { id: candidates[0].id };
  return undefined;
};
let v2MiniResolution;
const v2MiniNote = (text) => {
  try {
    console.error(`dot-agent-deck: ${text}`);
  } catch (_) {}
};
// Mini's session, looked up once per module, from the moment the mini
// receiver first started in this process (`setupAt`, kept with the session
// across a re-import: miniBinding): a promise of its id, or of undefined when
// it is ambiguous or was not found within V2_MINI_RESOLVE_MS. A
// `session.created` this plugin sees (observeInboxV2) answers it too.
const v2ResolveMiniTarget = (ctx) => {
  if (v2MiniResolution) return v2MiniResolution;
  // The first setup in this process, kept across a re-import (miniBinding).
  const binding = miniBinding();
  if (typeof binding.setupAt !== "number") binding.setupAt = Date.now();
  const setupAt = binding.setupAt;
  v2MiniResolution = (async () => {
    const started = typeof performance === "object" ? performance.timeOrigin : setupAt;
    const deadline = setupAt + V2_MINI_RESOLVE_MS;
    const directory = ctx?.location?.directory ?? process.cwd();
    for (;;) {
      if (miniTarget) return miniTarget;
      if (typeof ctx?.session?.list === "function") {
        let found;
        try {
          found = await v2FindMiniTarget(ctx, started, setupAt);
        } catch (_) {}
        if (miniTarget) return miniTarget;
        if (found?.id) {
          bindMiniTarget(found.id);
          return miniTarget;
        }
        if (found?.ambiguous) {
          v2MiniNote(`${found.ambiguous} OpenCode sessions started in ${directory} with this mini; deck messages are not delivered to it`);
          return undefined;
        }
      }
      if (Date.now() >= deadline) {
        v2MiniNote(`no OpenCode session of this mini found in ${directory}; deck messages are not delivered to it`);
        return undefined;
      }
      await inboxSleep(200);
    }
  })();
  return v2MiniResolution;
};
const v2MiniDeliver = async (ctx, message, generation) => {
  const ref = inboxRef(message);
  const receiver = serverReceiver();
  if (!receiver || generation !== inboxGeneration()) return;
  try {
    const sessionID = miniTarget ?? (await v2ResolveMiniTarget(ctx)) ?? miniTarget;
    if (generation !== inboxGeneration()) return;
    if (!sessionID) {
      receiver.notHanded(ref);
      return;
    }
    const text = inboxText(message);
    await inboxHandOver(
      receiver,
      ref,
      sessionID,
      v2Call(async () => {
        const item = await ctx.session.prompt({ sessionID, text });
        if (item?.id) v2NotePending(item.id, text);
        return item;
      }),
      () => generation === inboxGeneration() && miniTarget === sessionID,
    );
  } catch (_) {
    if (generation === inboxGeneration()) receiver.notHanded(ref);
  }
};
const startInboxV2 = (ctx) => {
  try {
    if (!inboxInPane() || !v2InboxInteractive()) return;
    v2InboxStarted = true;
    const generation = inboxGeneration() + 1;
    globalThis[INBOX_GENERATION] = generation;
    serverReceiver()?.stop();
    globalThis[INBOX_RECEIVER] = undefined;
    const mini = v2InboxMini() && typeof ctx?.session?.prompt === "function";
    if (!mini && !globalThis[INBOX_ANNOUNCED]) {
      globalThis[INBOX_ANNOUNCED] = true;
      try {
        execFileSync(hookBinary(), ["inbox", "announce", "--agent", "opencode"], {
          timeout: 5000,
          stdio: ["ignore", "ignore", "ignore"],
        });
      } catch (_) {}
    }
    if (mini) v2ResolveMiniTarget(ctx);
    const receiver = inboxReceiver({
      agent: "opencode-mini",
      doorbellOnly: !mini,
      active: () => generation === inboxGeneration(),
      deliver: mini
        ? (message) => {
            miniQueue = miniQueue.then(() => v2MiniDeliver(ctx, message, generation));
          }
        : undefined,
    });
    globalThis[INBOX_RECEIVER] = receiver;
    receiver.start();
  } catch (_) {}
};
const stopInboxV2 = () => {
  if (!v2InboxStarted) return;
  try {
    globalThis[INBOX_GENERATION] = inboxGeneration() + 1;
    serverReceiver()?.stop();
    globalThis[INBOX_RECEIVER] = undefined;
  } catch (_) {}
};
const observeInboxV2 = (event) => {
  try {
    const receiver = serverReceiver();
    const type = event?.type;
    const data = event?.data ?? {};
    if (type === "session.created") {
      if (data.sessionID && !data.parentID) bindMiniTarget(data.sessionID);
      return;
    }
    if (!receiver) return;
    if (type === "session.inbox.enqueued") {
      if (data.item?.type === "user") v2NotePending(data.inboxID, data.item.payload?.text);
    } else if (type === "session.inbox.delivered") {
      // OpenCode took the item into a turn: every attempt it carries is
      // committed.
      const refs = intercepted.get(data.inboxID) ?? [];
      intercepted.delete(data.inboxID);
      if (refs.length) receiver.committed(refs);
      const mini = [];
      for (const id of miniPending.get(data.inboxID) ?? []) mini.push(...receiver.of(id));
      miniPending.delete(data.inboxID);
      if (mini.length) {
        if (data.sessionID) miniTurns.add(data.sessionID);
        receiver.committed(mini);
        receiver.send({ repaint: true });
      }
    } else if (type === "session.inbox.cancelled") {
      // A withdrawn prompt: its attempts are lost, not released — the deck
      // may repeat them, marked as possible repeats.
      for (const id of miniPending.get(data.inboxID) ?? []) {
        for (const ref of receiver.of(id)) receiver.lost(ref);
      }
      for (const ref of intercepted.get(data.inboxID) ?? []) receiver.lost(ref);
      miniPending.delete(data.inboxID);
      intercepted.delete(data.inboxID);
    } else if (type === "session.idle" || (type === "session.status" && data.status?.type === "idle")) {
      if (!data.sessionID) return;
      for (const ref of receiver.inSession(data.sessionID)) receiver.lost(ref);
      if (miniTurns.delete(data.sessionID)) receiver.send({ repaint: true });
    }
  } catch (_) {}
};
// The doorbell on 2.x: `session.hook("prompt")` hands over the prompt before
// it is queued, and its text is replaced in place, as 1.x's `chat.message`
// parts were. Only under the pane's interactive OpenCode.
const interceptDoorbellV2 = async (input) => {
  if (!inboxInPane() || !v2InboxInteractive() || typeof input?.prompt?.text !== "string") return;
  const parts = [{ type: "text", text: input.prompt.text }];
  await interceptDoorbell(
    { sessionID: input.sessionID, messageID: input.messageID },
    { parts },
    true,
  );
  input.prompt.text = parts[0].text;
};
"#;

/// PRD #1858: the plugin's half of the deck's usage file. Each update of an
/// assistant message, and each step of one that finishes, appends one line of
/// named fields — its id, model, token counts, OpenCode's cost, `finish` and
/// error type — to the file the deck names in `DOT_AGENT_DECK_USAGE_FILE`,
/// skipping an update that changes none of them. The deck reads the last
/// line for an id as the message's usage
/// (`telemetry::accounting::usage_file`). Nothing else from the message, its
/// text and error message included, is written. Between updates this helper
/// holds a message's latest update and its steps only as a copy of the
/// fields its line is made from (`usageInfo`, `usageTokens`).
///
/// OpenCode gives every assistant message all five counts (`input`,
/// `output`, `reasoning`, `cache.read`, `cache.write`), starting at 0 until
/// the provider reports them. One that is missing or not a count is written
/// as `null`, which the deck reads as a format it does not know, never as 0;
/// a message whose counts are all still 0 while it has failed or not
/// completed is written with `"reported": false`.
///
/// A message's counts are its latest step's while its cost adds up every
/// step's (OpenCode's `SessionProcessor`, `step-finish`), so a message that
/// took more than one step — one retried after a step finished — would be
/// undercounted. Each finished step's `step-finish` part carries that step's
/// counts under an id of its own, so once one is seen the line's counts are
/// the sum of the message's steps, each counted once however often its part
/// is updated. A message forgotten with steps recorded (past the
/// `MAX_USAGE_MESSAGES` most recent) that updates again has its counts
/// written as `null`, since the steps it lost are not in the sum; its id is
/// remembered for that among the `MAX_USAGE_FORGOTTEN` most recently
/// forgotten.
///
/// A message of a subagent's child session is marked `"subagent": true`: its
/// usage is the run's, but how it ended says nothing about how the run's own
/// turn did. One of a session whose ancestry is not known yet is marked
/// `"subagent": null`, which the deck reads as a turn whose ending is not
/// known, since it may be either.
///
/// The file is appended to only when it is safe: already there (the deck
/// makes it), opened without following a symlink or waiting on a FIFO, and a
/// regular file of this user's that no other user can read or write.
/// Otherwise the line is skipped, and OpenCode carries on; a line that was not
/// written is not remembered as written, so the message's next update, even
/// one identical to it, tries it again.
const USAGE_FILE_JS: &str = r#"// PRD #1858: each assistant message's usage, for the deck's run record.
const USAGE_FILE = process.env.DOT_AGENT_DECK_USAGE_FILE;
const USAGE_FILE_ON = typeof USAGE_FILE === "string" && USAGE_FILE.startsWith("/");
const MAX_USAGE_MESSAGES = 256;
const MAX_USAGE_STEPS = 256;
const MAX_USAGE_FORGOTTEN = 4096;
// Message id -> { info, steps, written }: its latest update (usageInfo), its
// finished steps' counts by part id (usageTokens), and the line last written
// for it.
const usageMessages = new Map();
// Ids of messages forgotten with steps recorded, the most recent
// MAX_USAGE_FORGOTTEN: a later step of one cannot be added to those it lost.
const usageForgotten = new Set();
const USAGE_COUNTS = [["input"], ["output"], ["reasoning"], ["cache", "read"], ["cache", "write"]];
const usageCount = (value) =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
const usageName = (value) =>
  typeof value === "string" && /^[A-Za-z0-9._:/@+-]{1,128}$/.test(value) ? value : undefined;
const usageRecord = (value) =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
// A copy of a message's or a step's `tokens`: its counts, each a count or
// null, and nothing else.
const usageTokens = (value) => {
  const tokens = usageRecord(value);
  if (tokens === undefined) return undefined;
  const cache = usageRecord(tokens.cache);
  return {
    input: usageCount(tokens.input),
    output: usageCount(tokens.output),
    reasoning: usageCount(tokens.reasoning),
    cache: cache === undefined ? undefined : { read: usageCount(cache.read), write: usageCount(cache.write) },
  };
};
// The fields of an assistant message's update that its line is made from,
// copied, so nothing else the update holds — its paths, a structured reply,
// an error's message or response body — is kept.
const usageInfo = (info) => {
  const status = info.error?.data?.statusCode;
  return {
    id: usageName(info.id),
    model: usageName(info.modelID),
    session: sessionKey(info.sessionID),
    tokens: usageTokens(info.tokens),
    cost: typeof info.cost === "number" && Number.isFinite(info.cost) && info.cost >= 0 ? info.cost : undefined,
    finish: usageName(info.finish),
    failed: !!info.error,
    error: usageName(info.error?.name),
    status: Number.isInteger(status) && status >= 100 && status <= 599 ? status : undefined,
    completed: !!info.time?.completed,
  };
};
const usageMessage = (id) => {
  let message = usageMessages.get(id);
  if (message === undefined) {
    message = { info: undefined, steps: new Map(), written: undefined };
    // Its earlier steps were forgotten: its counts are not known.
    if (usageForgotten.has(id)) message.steps.set(undefined, undefined);
  }
  usageMessages.delete(id);
  usageMessages.set(id, message);
  if (usageMessages.size > MAX_USAGE_MESSAGES) {
    const [oldest, forgotten] = usageMessages.entries().next().value;
    usageMessages.delete(oldest);
    if (forgotten.steps.size > 0) rememberBounded(usageForgotten, oldest, undefined, MAX_USAGE_FORGOTTEN);
  }
  return message;
};
// Whether the whole line was written.
const appendUsageLine = (text) => {
  const flags =
    fsConstants.O_WRONLY | fsConstants.O_APPEND | (fsConstants.O_NOFOLLOW ?? 0) | (fsConstants.O_NONBLOCK ?? 0);
  let fd;
  try {
    fd = openSync(USAGE_FILE, flags);
  } catch {
    return false;
  }
  try {
    const stat = fstatSync(fd);
    const own = typeof process.getuid !== "function" || stat.uid === process.getuid();
    if (!stat.isFile() || !own || (stat.mode & 0o077) !== 0) return false;
    const line = Buffer.from(text + "\n");
    return writeSync(fd, line) === line.length;
  } catch {
    // The run's usage is then not recorded; OpenCode carries on.
    return false;
  } finally {
    closeSync(fd);
  }
};
// The sum of a message's finished steps' counts, in the shape of a
// message's `tokens`; a count some step lacks, or every count when a step
// could not be told apart from the others, is left out.
const usageStepTotal = (steps) => {
  const total = { cache: {} };
  for (const path of USAGE_COUNTS) {
    let sum = 0;
    for (const step of steps.values()) {
      const count = usageCount(path.reduce((value, key) => usageRecord(value)?.[key], step));
      sum = count === null || sum === null ? null : sum + count;
    }
    if (sum !== null && Number.isSafeInteger(sum)) {
      if (path.length === 1) total[path[0]] = sum;
      else total.cache[path[1]] = sum;
    }
  }
  return total;
};
// `info` is a message's usageInfo.
const usageLine = (info, steps) => {
  const line = { v: 1, type: "reply", agent: "opencode" };
  if (info.id !== undefined) line.id = info.id;
  if (info.model !== undefined) line.model = info.model;
  const ancestry = ancestryOfKey(info.session);
  if (ancestry === "child") line.subagent = true;
  else if (ancestry === undefined) line.subagent = null;
  const tokens = steps === undefined || steps.size === 0 ? info.tokens : usageStepTotal(steps);
  const cache = usageRecord(tokens?.cache);
  const counts = {
    input: usageCount(tokens?.input),
    output: usageCount(tokens?.output),
    reasoning: usageCount(tokens?.reasoning),
    cache_read: usageCount(cache?.read),
    cache_write: usageCount(cache?.write),
  };
  const zeros = Object.values(counts).every((count) => count === 0);
  if ((zeros || tokens === undefined) && (info.failed || !info.completed)) {
    line.reported = false;
  } else {
    line.input = counts.input;
    line.output =
      counts.output === null || counts.reasoning === null ? null : counts.output + counts.reasoning;
    line.cache_read = counts.cache_read;
    line.cache_write = counts.cache_write;
    if (info.cost !== undefined) line.cost_usd = info.cost;
  }
  if (info.finish !== undefined) line.stop = info.finish;
  if (info.error !== undefined) line.error = info.error;
  if (info.status !== undefined) line.status = info.status;
  return JSON.stringify(line);
};
const writeUsage = (message) => {
  const text = usageLine(message.info, message.steps);
  if (message.written === text) return;
  if (appendUsageLine(text)) message.written = text;
};
const recordUsage = (info) => {
  if (!USAGE_FILE_ON || info?.role !== "assistant") return;
  const fields = usageInfo(info);
  if (fields.id === undefined) {
    appendUsageLine(usageLine(fields, undefined));
    return;
  }
  const message = usageMessage(fields.id);
  message.info = fields;
  writeUsage(message);
};
// A finished step of an assistant message (a `step-finish` part).
const recordUsageStep = (part) => {
  if (!USAGE_FILE_ON || part?.type !== "step-finish") return;
  const id = usageName(part.messageID);
  if (id === undefined) return;
  const message = usageMessage(id);
  const step = usageName(part.id);
  if (step === undefined || (!message.steps.has(step) && message.steps.size >= MAX_USAGE_STEPS)) {
    // A step that cannot be told apart from the others: the message's
    // counts are not known.
    message.steps.set(undefined, undefined);
  } else {
    message.steps.set(step, usageTokens(part.tokens));
  }
  if (message.info !== undefined) writeUsage(message);
};
"#;

/// PRD #1666 S8: `chunk` with the deck's shared inbox helpers in front of it
/// (the receiver, [`crate::inbox_receiver::RECEIVER_JS`], among them) and
/// their placeholders filled ([`crate::inbox_receiver::fill_placeholders`]).
fn inbox_js(chunk: &str) -> String {
    crate::inbox_receiver::fill_placeholders(&format!(
        "{INBOX_SHARED_JS}{}{INBOX_HANDOVER_JS}{chunk}",
        crate::inbox_receiver::RECEIVER_JS
    ))
}

/// PRD #1666 S8: what both plugins share for the deck's inbox, in front of
/// the receiver.
const INBOX_SHARED_JS: &str = r#"// PRD #1666: dot-agent-deck's inbox. The deck hands its messages to a
// watcher this plugin runs (`dot-agent-deck inbox wait --stream
// --adapter-integration 2`), one JSON line each, as attempts: `{id, a, e, d}`
// and the framed text. The plugin starts each as a prompt with OpenCode's own
// client and tells the deck where each attempt stands — still held, handed to
// OpenCode, started on (its commit), or positively not handed — and the
// watcher records the receipts the deck reads back. It never touches what the
// user is typing.
const INBOX_FRAMING = __FRAMING_JSON__;
const INBOX_ID = "(m-[a-z2-7]{8}-[0-9a-f]{16})";
const INBOX_OPENER = new RegExp("\\[dot-agent-deck message " + INBOX_ID, "g");
const INBOX_MAX_TRACKED = 256;
const inboxSleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const inboxRemember = (map, key, value) => {
  map.delete(key);
  map.set(key, value);
  while (map.size > INBOX_MAX_TRACKED) {
    map.delete(map.keys().next().value);
  }
};
const inboxInPane = () =>
  !!process.env.DOT_AGENT_DECK_PANE_ID && !!process.env.DOT_AGENT_DECK_PANE_CAPABILITY;
// OpenCode answers several queued user messages in ONE assistant turn whose
// `parentID` names only the last of them, so a turn answers every user
// message its session had up to that parent. `users` maps each user message
// to its session and the order this process first saw it (OpenCode's ids are
// time-ordered too), at most INBOX_MAX_TRACKED of them; a message OpenCode
// removes, as a revert does, leaves it and every `pendings` map, so nothing
// commits it any more.
let inboxUserSeq = 0;
const inboxObserveUsers = (users, event, ...pendings) => {
  const properties = event?.properties;
  if (event?.type === "message.removed") {
    if (properties?.messageID) {
      users.delete(properties.messageID);
      for (const pending of pendings) pending.delete(properties.messageID);
    }
    return;
  }
  const info = properties?.info;
  if (event?.type !== "message.updated" || info?.role !== "user") return;
  if (info.id && info.sessionID && !users.has(info.id)) {
    inboxRemember(users, info.id, { sessionID: info.sessionID, seq: ++inboxUserSeq });
  }
};
// The keys of `pending` (user message ids) that `assistant`'s turn answered:
// its parent, and every earlier user message of the same session. Only a
// parent this process saw, still there and in the assistant's session,
// answers anything: one removed or reverted since is no longer answered.
const inboxCovered = (users, pending, assistant) => {
  const parentID = assistant?.parentID;
  if (!parentID) return [];
  const parent = users.get(parentID);
  if (!parent) return [];
  const sessionID = assistant.sessionID ?? parent.sessionID;
  if (parent.sessionID !== sessionID) return [];
  return [...pending.keys()].filter((key) => {
    if (key === parentID) return true;
    const user = users.get(key);
    return !!user && user.sessionID === sessionID && user.seq <= parent.seq;
  });
};
// The deck message ids whose blocks `text` carries at its top level. A block
// runs from its opener to the closer naming the same id, and an opener inside
// it — one a trusted body quotes — names no block of its own; an opener with
// no closer runs to the end of `text`.
const inboxIdsIn = (text) => {
  if (typeof text !== "string") return [];
  const opener = new RegExp(INBOX_OPENER.source, "g");
  const ids = new Set();
  for (let found = opener.exec(text); found !== null; found = opener.exec(text)) {
    ids.add(found[1]);
    const closer = "[end of dot-agent-deck message " + found[1] + "]";
    const end = text.indexOf(closer, opener.lastIndex);
    if (end < 0) break;
    opener.lastIndex = end + closer.length;
  }
  return [...ids];
};
// `--model provider/model` (or `-m`, or `--model=`) from the user's argv.
const inboxArgvModel = () => {
  const argv = process.argv.slice(2);
  for (let i = 0; i < argv.length; i++) {
    let value;
    if (argv[i] === "--model" || argv[i] === "-m") {
      value = argv[i + 1];
    } else if (typeof argv[i] === "string" && argv[i].startsWith("--model=")) {
      value = argv[i].slice("--model=".length);
    } else {
      continue;
    }
    const slash = typeof value === "string" ? value.indexOf("/") : -1;
    return slash > 0 && slash < value.length - 1
      ? { providerID: value.slice(0, slash), modelID: value.slice(slash + 1) }
      : undefined;
  }
  return undefined;
};
// The model and agent a user message ran with, if it names either.
const inboxSelectionOf = (info) => {
  if (!info || info.role !== "user") return undefined;
  const model =
    info.model && typeof info.model.providerID === "string" && typeof info.model.modelID === "string"
      ? { providerID: info.model.providerID, modelID: info.model.modelID }
      : undefined;
  const agent = typeof info.agent === "string" && info.agent ? info.agent : undefined;
  return model || agent ? { model, agent } : undefined;
};
const inboxLastSelection = (messages) => {
  const list = Array.isArray(messages) ? messages : [];
  for (let i = list.length - 1; i >= 0; i--) {
    const selection = inboxSelectionOf(list[i]?.info ?? list[i]);
    if (selection) return selection;
  }
  return undefined;
};
const inboxText = (message) => INBOX_FRAMING + "\n\n" + message.text;
"#;

/// PRD #1666 S8: what both plugins share after the receiver: the call that
/// hands one attempt to OpenCode behind the final hand-over check.
const INBOX_HANDOVER_JS: &str = r#"// PRD #1666 §4: whether OpenCode answered a call by refusing it before
// accepting it. Its prompt route accepts with 204 before it starts the turn,
// and a 4xx answer (a request it would not take: a missing session, an invalid
// body) comes before that. A thrown error (a lost connection, a timeout) or
// any other answer may follow an accepted prompt, so it proves nothing.
const inboxRefusedBeforeAcceptance = (result) => {
  const status = result?.response?.status;
  return typeof status === "number" && status >= 400 && status < 500;
};
// PRD #1666 §3: hand one attempt to OpenCode through `call`, behind the final
// hand-over check: the deck's approval, then — with no await between them and
// the call — this context's own synchronous check and the caller's `valid`,
// which re-reads what the await may have changed (the session on screen, a
// dialog). An approval voided by a watcher restart, or a check still pending
// when the watcher ended (the receiver's `may` waits for the successor's
// registration), is asked for once more under the new incarnation; any other
// refusal drops the callback before the
// call, so the attempt was positively not handed. Once the call began, only
// OpenCode's refusal before acceptance is a not-handed witness; any other
// failure is lost, for a marked repeat. Returns whether it was handed.
const inboxHandOver = async (receiver, ref, sessionID, call, valid = () => true) => {
  let granted = await receiver.may(ref);
  if (granted !== undefined && granted.epoch !== receiver.epoch()) granted = await receiver.may(ref);
  let still = false;
  try {
    still = receiver.ready(ref, granted) && valid();
  } catch (_) {}
  if (!still) {
    receiver.notHanded(ref);
    return false;
  }
  receiver.calling(ref);
  receiver.session(ref, sessionID);
  let result;
  try {
    result = await call();
  } catch (_) {
    receiver.lost(ref);
    return false;
  }
  if (result?.error) {
    if (inboxRefusedBeforeAcceptance(result)) receiver.rejected(ref);
    else receiver.lost(ref);
    return false;
  }
  receiver.handed([ref]);
  return true;
};
"#;

/// PRD #1666 S8: the server plugin's inbox roles (design §7.3, §7.7, and
/// decisions E and F).
///
/// - In the full TUI it loads in a worker thread, and only announces: the TUI
///   plugin arms the stream, possibly after this plugin's first status report,
///   so the announce holds the doorbell for the arm grace.
/// - In `opencode --mini`, where no TUI plugin loads, it arms its own stream
///   and starts each message in mini's session, then asks the deck to nudge
///   the pane into drawing it.
/// - In the full TUI and `--mini` — never in `run`, `serve` or another
///   subcommand (hardening M2) — it catches a submitted doorbell line before
///   the model sees it, swaps in the waiting content, and acknowledges it on
///   decision B's signal, like the TUI plugin.
const SERVER_INBOX_JS: &str = r#"// OpenCode's subcommands (1.18.35 `--help`): a main-thread load under one of
// them is not a TUI in this pane.
const INBOX_SUBCOMMANDS = new Set(["completion", "acp", "mcp", "attach", "run", "debug",
  "providers", "auth", "agent", "upgrade", "uninstall", "serve", "web", "models", "stats",
  "export", "import", "github", "pr", "session", "plugin", "plug", "db"]);
const INBOX_DOORBELL = new RegExp(
  __DOORBELL_PREFIX_JSON__.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + " " + INBOX_ID +
    __DOORBELL_SUFFIX_JSON__.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
const INBOX_HANDLED =
  "[dot-agent-deck: the message this line announced was already handed over. Nothing is waiting.]";
const INBOX_RECEIVER = "__dotAgentDeckReceiver";
const INBOX_GENERATION = "__dotAgentDeckGeneration";
const INBOX_ANNOUNCED = "__dotAgentDeckAnnounced";
const INBOX_MINI_BINDING = "__dotAgentDeckMiniBinding";
// --mini: the last root session this process created, which OpenCode user
// message carries which deck message ids, and the sessions running a deck
// turn.
//
// Mini's session, and when the 2.x mini receiver first started (`setupAt`,
// v2ResolveMiniTarget), are kept per process on globalThis, not in this
// module: OpenCode 2 reloads the plugin when the module file changes (a deck
// upgrade rewrites it), and a re-import in the same process starts with fresh
// module variables. Re-resolving then would take the reload as `setupAt`, so
// the window would cover every root session of the directory since the
// server started, and a mini sharing its directory with any later session
// would go dark as ambiguous. Not measured: whether 2.2.5's reload re-imports
// the module in the same process or starts a new server; a new server keeps
// nothing here, and a mini whose session predates it is then not found.
const miniBinding = () => {
  if (!globalThis[INBOX_MINI_BINDING]) globalThis[INBOX_MINI_BINDING] = {};
  return globalThis[INBOX_MINI_BINDING];
};
let miniTarget = miniBinding().target;
const bindMiniTarget = (id) => {
  miniTarget = id;
  miniBinding().target = id;
};
const miniPending = new Map();
const miniTurns = new Set();
let miniQueue = Promise.resolve();
// Every mode: doorbell prompts whose content this plugin swapped in, by
// OpenCode user message id, with the attempts they carry.
const intercepted = new Map();
// Every mode: the user messages this process saw (`inboxObserveUsers`).
const inboxUsers = new Map();

const inboxGeneration = () => globalThis[INBOX_GENERATION] ?? 0;
const serverReceiver = () => globalThis[INBOX_RECEIVER];
const isMini = () =>
  isMainThread && process.argv.slice(2).includes("--mini") && !INBOX_SUBCOMMANDS.has(process.argv[2]);

const miniDeliver = async (client, message, generation) => {
  const ref = inboxRef(message);
  const receiver = serverReceiver();
  if (!receiver || generation !== inboxGeneration()) return;
  try {
    while (!miniTarget) {
      if (generation !== inboxGeneration()) return;
      await inboxSleep(200);
    }
    const sessionID = miniTarget;
    let selection;
    try {
      const response = await client.session.messages({ path: { id: sessionID } });
      selection = inboxLastSelection(response?.data);
    } catch (_) {}
    if (!selection) {
      const model = inboxArgvModel();
      selection = model ? { model } : {};
    }
    // Hardening M7: re-checked after the lookup. A retired instance does
    // nothing — the deck asks its successor about the attempt — and a target
    // that moved on is positively not handed.
    if (generation !== inboxGeneration()) return;
    if (miniTarget !== sessionID) {
      receiver.notHanded(ref);
      return;
    }
    const body = { parts: [{ type: "text", text: inboxText(message) }] };
    if (selection.model) body.model = selection.model;
    if (selection.agent) body.agent = selection.agent;
    await inboxHandOver(
      receiver,
      ref,
      sessionID,
      () => client.session.promptAsync({ path: { id: sessionID }, body }),
      () => generation === inboxGeneration() && miniTarget === sessionID,
    );
  } catch (_) {
    if (generation === inboxGeneration()) receiver.notHanded(ref);
  }
};

// Called at every instantiation: OpenCode re-runs a plugin in the same module
// when it disposes an instance, and the previous instance hears nothing, so
// its receiver is stopped here and its callbacks retire with its generation.
// In the full TUI (a worker thread) this plugin only announces and keeps a
// doorbell-only receiver for the doorbell substitution below; the TUI plugin
// is the one handed messages. In `--mini`, where no TUI plugin loads, it is
// the receiver handed them.
const startInbox = (client) => {
  try {
    if (!inboxInPane()) return;
    const generation = inboxGeneration() + 1;
    globalThis[INBOX_GENERATION] = generation;
    serverReceiver()?.stop();
    globalThis[INBOX_RECEIVER] = undefined;
    if (isMainThread && !isMini()) return;
    if (!isMainThread && !globalThis[INBOX_ANNOUNCED]) {
      globalThis[INBOX_ANNOUNCED] = true;
      try {
        execFileSync(hookBinary(), ["inbox", "announce", "--agent", "opencode"], {
          timeout: 5000,
          stdio: ["ignore", "ignore", "ignore"],
        });
      } catch (_) {}
    }
    const mini = isMini() && !!client?.session;
    const receiver = inboxReceiver({
      agent: "opencode-mini",
      doorbellOnly: !mini,
      active: () => generation === inboxGeneration(),
      deliver: mini
        ? (message) => {
            miniQueue = miniQueue.then(() => miniDeliver(client, message, generation));
          }
        : undefined,
    });
    globalThis[INBOX_RECEIVER] = receiver;
    receiver.start();
  } catch (_) {}
};

// OpenCode started answering: every attempt the turn covers is committed.
const inboxAck = (assistant) => {
  const receiver = serverReceiver();
  if (!receiver) return;
  for (const key of inboxCovered(inboxUsers, intercepted, assistant)) {
    const refs = intercepted.get(key);
    intercepted.delete(key);
    receiver.committed(refs);
  }
  const refs = [];
  for (const key of inboxCovered(inboxUsers, miniPending, assistant)) {
    for (const id of miniPending.get(key)) refs.push(...receiver.of(id));
    miniPending.delete(key);
  }
  if (refs.length) {
    if (assistant.sessionID) miniTurns.add(assistant.sessionID);
    receiver.committed(refs);
    receiver.send({ repaint: true });
  }
};

const observeInbox = (event) => {
  try {
    const receiver = serverReceiver();
    const type = event?.type;
    const properties = event?.properties;
    // A reverted prompt: its attempts are lost, not released — the deck may
    // repeat them, marked as possible repeats.
    if (type === "message.removed" && properties?.messageID && receiver) {
      for (const id of miniPending.get(properties.messageID) ?? []) {
        for (const ref of receiver.of(id)) receiver.lost(ref);
      }
      for (const ref of intercepted.get(properties.messageID) ?? []) receiver.lost(ref);
    }
    inboxObserveUsers(inboxUsers, event, intercepted, miniPending);
    if (type === "session.created") {
      const info = properties?.info;
      if (info?.id && !info.parentID) bindMiniTarget(info.id);
    } else if (type === "message.part.updated") {
      const part = properties?.part;
      if (part?.type !== "text" || !part.messageID || !receiver) return;
      const ids = inboxIdsIn(part.text).filter((id) => receiver.of(id).length > 0);
      if (ids.length) {
        inboxRemember(miniPending, part.messageID, [...new Set([...(miniPending.get(part.messageID) ?? []), ...ids])]);
      }
    } else if (type === "message.updated") {
      if (properties?.info?.role === "assistant") inboxAck(properties.info);
    } else if (type === "session.idle" || (type === "session.status" && properties?.status?.type === "idle")) {
      const sessionID = properties?.sessionID;
      if (!sessionID) return;
      // The session went idle with an attempt handed and not started on: the
      // turn was aborted. Lost, so the deck may repeat it.
      for (const ref of receiver?.inSession(sessionID) ?? []) receiver.lost(ref);
      if (miniTurns.delete(sessionID)) receiver?.send({ repaint: true });
    }
  } catch (_) {}
};

// The doorbell, submitted because no receiver was handed the message, never
// reaches the model as typed: its text is replaced IN PLACE (OpenCode saves
// the parts array it passed in) with the waiting content, fetched as attempts
// bound to this context's receiver. The fetch is the deck's final hand-over
// check: it issues under its lock, and nothing awaits between it and the
// substitution. A recalled doorbell whose message was already handed over
// fetches nothing. With no receiver yet, or a deck that cannot be asked, the
// line stays as it is, and asks the model to fetch.
// PRD #1666 hardening M2: only in the interactive OpenCode the deck manages —
// the full TUI, whose server plugin runs in a worker thread, or `--mini` — never
// in `run`, `serve` or another subcommand an agent starts inside its pane.
const inboxInteractive = () => !isMainThread || isMini();
// `v2`: called by OpenCode 2.x's prompt hook (interceptDoorbellV2), which has
// already checked that this is the pane's interactive OpenCode.
const interceptDoorbell = async (input, output, v2 = false) => {
  try {
    if (!inboxInPane() || !(v2 || inboxInteractive()) || !Array.isArray(output?.parts)) return;
    const receiver = serverReceiver();
    for (const part of output.parts) {
      if (part?.type !== "text" || typeof part.text !== "string") continue;
      const match = INBOX_DOORBELL.exec(part.text);
      if (!match) continue;
      const rx = receiver?.rx();
      if (rx === undefined) return;
      const approval = receiver.approval();
      let reply;
      try {
        reply = JSON.parse(
          execFileSync(
            hookBinary(),
            ["inbox", "fetch", "--json", "--doorbell", match[1],
              "--adapter-integration", String(INBOX_ADAPTER_INTEGRATION), "--rx", String(rx)],
            { timeout: 3000, stdio: ["ignore", "pipe", "ignore"], encoding: "utf8" }
          )
        );
      } catch (_) {
        return;
      }
      if (!Array.isArray(reply?.messages)) return;
      const fetched = reply.messages.filter(
        (m) => typeof m?.id === "string" && typeof m.text === "string" && typeof m.a === "number" && typeof m.e === "number"
      );
      if (fetched.length === 0) {
        part.text = part.text.replace(match[0], () => INBOX_HANDLED);
        return;
      }
      receiver.take(fetched);
      const ready = fetched.filter((m) => receiver.ready(inboxRef(m), approval));
      for (const m of fetched) {
        if (!ready.includes(m)) receiver.notHanded(inboxRef(m));
      }
      if (ready.length === 0) {
        part.text = part.text.replace(match[0], () => INBOX_HANDLED);
        return;
      }
      const refs = ready.map(inboxRef);
      for (const ref of refs) {
        receiver.calling(ref);
        receiver.session(ref, input?.sessionID ?? output?.message?.sessionID);
      }
      const content = INBOX_FRAMING + ready.map((m) => "\n\n" + m.text).join("");
      part.text = part.text.replace(match[0], () => content);
      const messageID = input?.messageID ?? output?.message?.id;
      if (messageID) inboxRemember(intercepted, messageID, refs);
      receiver.handed(refs);
      return;
    }
  } catch (_) {}
};
"#;

/// PRD #1666 S8 (design §7.1–§7.6): the TUI plugin, `<root>/dot-agent-deck-tui.js`,
/// with `BINARY_PATH` pinned like the server plugin's ([`plugin_template`]).
///
/// OpenCode loads it from the `plugin` list of `tui.json` in the process that
/// draws the TUI (the full TUI and `attach`; not `--mini`, `run` or `serve`).
/// It arms the deck's stream and starts each message in the session on
/// screen: in place in a root session, in the root of a subagent's session
/// (shown first), and in a new session, shown, from the home screen.
fn tui_plugin_template(binary_path: &str) -> String {
    let binary_path_json =
        serde_json::to_string(binary_path).expect("serializing a &str to a String is infallible");
    format!(
        "// dot-agent-deck's OpenCode TUI plugin, generated by `dot-agent-deck hooks install`.\n\
         // Regenerated on every install; edits are overwritten.\n\
         import {{ execFile, spawn }} from \"child_process\";\n\n\
         const BINARY_PATH = {binary_path_json};\n\n{}{}",
        TUI_HOOK_BINARY_JS,
        inbox_js(TUI_INBOX_JS)
    )
}

/// PRD #2050: OpenCode 2.x's TUI plugin, `<root>/plugins/dot-agent-deck-tui/tui.js`
/// ([`tui_v2_file`]), with `BINARY_PATH` pinned like the others.
///
/// OpenCode 2.x loads `tui.js` from a directory under a config root's
/// `plugins/` in the process that draws the full TUI (measured on 2.2.5: the
/// client process, with the pane's environment; `opencode mini` loads none).
/// The directory has no `server.js` or `index.js`, so 2.x's server-plugin
/// scan skips it, and OpenCode 1.x's one-level file glob never sees it.
fn tui_v2_template(binary_path: &str) -> String {
    let binary_path_json =
        serde_json::to_string(binary_path).expect("serializing a &str to a String is infallible");
    format!(
        "// dot-agent-deck's OpenCode 2 TUI plugin, generated by `dot-agent-deck hooks install`.\n\
         // Regenerated on every install; edits are overwritten.\n\
         import {{ execFile, spawn }} from \"child_process\";\n\n\
         const BINARY_PATH = {binary_path_json};\n\n{}{}",
        TUI_HOOK_BINARY_JS,
        inbox_js(TUI_V2_JS)
    )
}

/// PRD #2050: the 1.x TUI plugin's behaviour on OpenCode 2.x's TUI API: arm
/// the deck's stream and start each message in the session on screen — in
/// place in a root session, in the root of a subagent's session (shown
/// first), and in a new session, shown, from the home screen, carrying the
/// home screen's unsent draft into that session's composer — waiting while a
/// dialog is open and re-reading the screen after every wait.
///
/// What moved: the route is `ui.router.current()` (`{ type, sessionID }`),
/// sessions are `data.session.get`, a prompt is `client.session.prompt({
/// sessionID, text })`, which keeps the session's own model and agent, and a
/// prompt's progress is reported on `session.inbox.*` events naming the
/// inbox item that call returned. OpenCode 2.x exposes no "dialog open" flag:
/// one is taken to be open while the keymap's mode is neither the base mode
/// nor the composer (measured: `base` with no dialog on 2.2.5; the modes a
/// dialog pushes are unmeasured).
const TUI_V2_JS: &str = r#"const TUI_GUARD = "__dotAgentDeckTuiLoaded";

const setup = async (api) => {
  try {
    if (globalThis[TUI_GUARD]?.active || !inboxInPane()) return;
    const instance = { active: true };
    globalThis[TUI_GUARD] = instance;
    let queue = Promise.resolve();
    // Which OpenCode inbox item carries which deck message ids.
    const pending = new Map();
    const unsubscribe = [];

    const sessionOf = (id) => {
      try {
        return api.data?.session?.get?.(id);
      } catch (_) {
        return undefined;
      }
    };
    const dialogOpen = () => {
      try {
        const mode = api.keymap?.mode?.current?.();
        return typeof mode === "string" && mode !== "base" && mode !== "composer";
      } catch (_) {
        return false;
      }
    };
    const untilNoDialog = async () => {
      while (instance.active && dialogOpen()) await inboxSleep(100);
    };
    const rootOf = (id) => {
      let current = id;
      for (let n = 0; n < 64; n++) {
        const parent = sessionOf(current)?.parentID;
        if (!parent) return current;
        current = parent;
      }
      return current;
    };
    const shownSession = () => {
      try {
        const route = api.ui?.router?.current?.();
        return route?.type === "session" ? route.sessionID : undefined;
      } catch (_) {
        return undefined;
      }
    };
    const show = (sessionID) => api.ui.router.navigate({ type: "session", sessionID });
    class Retired extends Error {}
    const stillActive = () => {
      if (!instance.active) throw new Retired();
    };
    const target = async () => {
      await untilNoDialog();
      stillActive();
      const shown = shownSession();
      if (shown) {
        const root = rootOf(shown);
        if (root !== shown) show(root);
        return root;
      }
      const created = await api.client.session.create({});
      stillActive();
      const sessionID = created?.id ?? created?.data?.id;
      if (!sessionID) throw new Error("no session was created");
      if (shownSession()) throw new Error("the person opened a session meanwhile");
      const stashed = await stashDraft();
      show(sessionID);
      if (stashed) await restoreDraft(sessionID);
      return sessionID;
    };
    // The person's unsent draft on the home screen. 2.x gives home and each
    // session a composer of its own, so opening the new session would leave
    // the draft behind, where 1.x carried it along. The plugin API reads no
    // composer, but the TUI's own `prompt.stash` / `prompt.stash.pop` commands
    // move a whole prompt (text, files, pasted parts) and are dispatchable:
    // stash on home, then pop once the session's composer is mounted
    // (measured on 2.2.5: `prompt.stash.pop` is reachable again, with
    // `session.compact`, about 0.1 s after navigating, and the popped prompt
    // leaves OpenCode's stash empty). A pop that never becomes reachable
    // leaves the draft in OpenCode's stash list, not lost.
    const command = (id) => {
      try {
        const found = api.keymap?.commands?.().find((c) => c?.id === id);
        if (!found) return undefined;
        const enabled = typeof found.enabled === "function" ? found.enabled() : found.enabled;
        return enabled === false ? undefined : found;
      } catch (_) {
        return undefined;
      }
    };
    // Whether the draft went to the stash: `prompt.stash` is reachable only
    // while the composer holds text, and the stash empties it.
    const stashDraft = async () => {
      if (!command("prompt.stash")) return false;
      try {
        api.keymap.dispatch("prompt.stash");
      } catch (_) {
        return false;
      }
      for (let n = 0; n < 10; n++) {
        if (!command("prompt.stash")) return true;
        await inboxSleep(10);
      }
      return false;
    };
    const restoreDraft = async (sessionID) => {
      for (let n = 0; n < 100 && instance.active; n++) {
        if (shownSession() !== sessionID) return;
        if (command("session.compact") && command("prompt.stash.pop")) {
          try {
            api.keymap.dispatch("prompt.stash.pop");
          } catch (_) {}
          return;
        }
        await inboxSleep(20);
      }
    };
    const notePending = (inboxID, text) => {
      if (!inboxID) return;
      const ids = inboxIdsIn(text).filter((id) => receiver.of(id).length > 0);
      if (ids.length) inboxRemember(pending, inboxID, [...new Set([...(pending.get(inboxID) ?? []), ...ids])]);
    };
    let receiver;
    const deliver = async (message) => {
      const ref = inboxRef(message);
      try {
        stillActive();
        const sessionID = await target();
        const text = inboxText(message);
        const onTarget = () => {
          const now = shownSession();
          return now === undefined || rootOf(now) === sessionID;
        };
        stillActive();
        if (!onTarget()) throw new Error("the session changed");
        await inboxHandOver(
          receiver,
          ref,
          sessionID,
          async () => {
            try {
              const item = await api.client.session.prompt({ sessionID, text });
              notePending(item?.id ?? item?.data?.id, text);
              return item;
            } catch (error) {
              const status = error?.status ?? error?.response?.status ?? error?.cause?.status;
              if (typeof status === "number") return { error, response: { status } };
              throw error;
            }
          },
          () => instance.active && !dialogOpen() && onTarget(),
        );
      } catch (error) {
        if (!(error instanceof Retired)) receiver.notHanded(ref);
      }
    };
    receiver = inboxReceiver({
      agent: "opencode",
      active: () => instance.active,
      deliver: (message) => {
        queue = queue.then(() => deliver(message));
      },
    });

    const on = (type, handler) => {
      try {
        const off = api.data?.on?.(type, (event) => {
          try {
            handler(event?.data ?? {});
          } catch (_) {}
        });
        if (typeof off === "function") unsubscribe.push(off);
      } catch (_) {}
    };
    on("session.inbox.enqueued", (data) => {
      if (data.item?.type === "user") notePending(data.inboxID, data.item.payload?.text);
    });
    on("session.inbox.delivered", (data) => {
      const refs = [];
      for (const id of pending.get(data.inboxID) ?? []) refs.push(...receiver.of(id));
      pending.delete(data.inboxID);
      receiver.committed(refs);
    });
    on("session.inbox.cancelled", (data) => {
      // A withdrawn prompt: lost, not released.
      for (const id of pending.get(data.inboxID) ?? []) {
        for (const ref of receiver.of(id)) receiver.lost(ref);
      }
      pending.delete(data.inboxID);
    });
    // PRD #2050: OpenCode 2's question forms. Only this plugin, in the
    // process that draws the full TUI, can answer one (`data.session.form`;
    // the server plugin's context has no form API), so it holds each through
    // `await-answer`, as 1.x's server plugin holds a question, and replies
    // with the deck's answer. The server plugin leaves `form.created` to it
    // here, and still reports a form's reply or cancellation.
    const heldForms = new Map();
    const settledForms = new Set();
    // At most this many forms held at once, each an `await-answer` child
    // (audit I2): past it, a form is reported plainly, so the card says it is
    // answered in OpenCode, rather than starting a child per form.
    const MAX_HELD_FORMS = 16;
    const stopForm = (formID) => {
      const child = heldForms.get(formID);
      heldForms.delete(formID);
      try {
        child?.kill();
      } catch (_) {}
    };
    const reportPlainly = (payload) => {
      try {
        const child = spawn(hookBinary(), ["hook", "--agent", "opencode"], { stdio: ["pipe", "ignore", "ignore"] });
        child.on("error", () => {});
        child.stdin?.on("error", () => {});
        child.stdin?.end(JSON.stringify(payload));
      } catch (_) {}
    };
    const replyToForm = async (answer) => {
      const refused = (detail) => ({ delivered: false, detail: String(detail).slice(0, 200) });
      if (answer?.kind !== "form" || typeof answer.request_id !== "string") return refused("the answer is not a form reply");
      const input = { sessionID: answer.body?.sessionID, formID: answer.request_id, answer: answer.body?.answer ?? {} };
      try {
        const forms = api.data?.session?.form;
        if (typeof forms?.reply === "function") await forms.reply(input);
        else await api.client.session.form.reply(input);
        return { delivered: true };
      } catch (error) {
        return refused(error?.message ?? "the reply could not be sent");
      }
    };
    const holdForm = (form) => {
      const formID = form?.id;
      const sessionID = form?.sessionID;
      if (typeof formID !== "string" || typeof sessionID !== "string" || !instance.active) return;
      let root = sessionID;
      try {
        root = api.data?.session?.root?.(sessionID) || sessionID;
      } catch (_) {}
      const cwd = process.cwd();
      const plain = { session_id: root, event: "question.asked", prompt: form.title ?? "", cwd, request_id: formID };
      const payload = {
        session_id: root,
        event: "question.asked",
        cwd,
        properties: { id: formID, sessionID, title: form.title, form },
      };
      stopForm(formID);
      settledForms.delete(formID);
      if (heldForms.size >= MAX_HELD_FORMS) {
        reportPlainly(plain);
        return;
      }
      let child;
      try {
        child = spawn(hookBinary(), ["await-answer", "--agent", "opencode", "--ack"], { stdio: ["pipe", "pipe", "ignore"] });
      } catch (_) {
        reportPlainly(plain);
        return;
      }
      heldForms.set(formID, child);
      let out = "";
      let answered = false;
      const failedEarly = (code, signal) => {
        if (answered || heldForms.get(formID) !== child) return;
        heldForms.delete(formID);
        if (code !== 0 || signal) reportPlainly(plain);
      };
      child.stdin?.on("error", () => {});
      child.stdout?.on("data", async (chunk) => {
        if (answered) return;
        out += chunk;
        const newline = out.indexOf("\n");
        if (newline < 0) return;
        answered = true;
        if (heldForms.get(formID) === child) heldForms.delete(formID);
        let ack;
        try {
          ack = await replyToForm(JSON.parse(out.slice(0, newline)));
        } catch (_) {
          ack = { delivered: false, detail: "the deck's answer could not be read" };
        }
        try {
          child.stdin?.end(JSON.stringify(ack) + "\n");
        } catch (_) {}
        if (!ack.delivered && instance.active && !settledForms.has(formID)) holdForm(form);
      });
      child.on("error", () => failedEarly(null, "error"));
      child.on("close", (code, signal) => failedEarly(code, signal));
      try {
        child.stdin?.write(JSON.stringify(payload) + "\n");
      } catch (_) {}
    };
    on("form.created", (data) => holdForm(data.form));
    for (const type of ["form.replied", "form.cancelled"]) {
      on(type, (data) => {
        const formID = data.id ?? data.formID ?? data.form?.id;
        if (typeof formID !== "string") return;
        settledForms.add(formID);
        if (settledForms.size > 256) settledForms.delete(settledForms.values().next().value);
        stopForm(formID);
      });
    }
    for (const type of ["session.status", "session.idle"]) {
      on(type, (data) => {
        const idle = type === "session.idle" || data.status?.type === "idle";
        if (!idle || !data.sessionID) return;
        // Idle with an attempt handed and not started on: the turn was
        // aborted. Lost, so the deck may repeat it.
        for (const ref of receiver.inSession(data.sessionID)) receiver.lost(ref);
      });
    }
    receiver.start();
    return () => {
      instance.active = false;
      for (const formID of [...heldForms.keys()]) stopForm(formID);
      for (const off of unsubscribe.splice(0)) {
        try {
          off();
        } catch (_) {}
      }
      receiver.stop();
    };
  } catch (_) {}
};

export default { id: "dot-agent-deck-tui", setup };
"#;

/// The TUI plugin's copy of the server plugin's `hookBinary` (PRD #1497's
/// `DOT_AGENT_DECK_BIN` override, absolute paths only).
const TUI_HOOK_BINARY_JS: &str = r#"const isAbsoluteOverride = (value) =>
  process.platform === "win32" ? /^([A-Za-z]:[\\/]|[\\/]{2})/.test(value) : value.startsWith("/");
const hookBinary = () => {
  const override = process.env.DOT_AGENT_DECK_BIN;
  return typeof override === "string" && isAbsoluteOverride(override) ? override : BINARY_PATH;
};

"#;

const TUI_INBOX_JS: &str = r#"// One live instance per process: a second copy of this file (another
// spelling of its path, another tui.json) stays inert while the first runs.
const TUI_GUARD = "__dotAgentDeckTuiLoaded";

const tui = async (api) => {
  try {
    if (globalThis[TUI_GUARD]?.active || !inboxInPane()) return;
    const instance = { active: true };
    globalThis[TUI_GUARD] = instance;
    let lastSeen;
    let queue = Promise.resolve();
    // Which OpenCode user message carries which deck message ids.
    const pending = new Map();
    const users = new Map();

    const sessionOf = (id) => {
      try {
        return api.state?.session?.get?.(id);
      } catch (_) {
        return undefined;
      }
    };
    const selectionFor = (sessionID) => {
      let messages;
      try {
        messages = api.state?.session?.messages?.(sessionID);
      } catch (_) {}
      const own = inboxLastSelection(messages);
      if (own) return own;
      if (lastSeen) return lastSeen;
      const model = inboxArgvModel();
      return model ? { model } : {};
    };
    const dialogOpen = () => {
      try {
        return !!api.ui?.dialog?.open || api.mode?.current?.() === "modal";
      } catch (_) {
        return false;
      }
    };
    const untilNoDialog = async () => {
      while (instance.active && dialogOpen()) await inboxSleep(100);
    };
    const rootOf = (id) => {
      let current = id;
      for (let n = 0; n < 64; n++) {
        const parent = sessionOf(current)?.parentID;
        if (!parent) return current;
        current = parent;
      }
      return current;
    };
    // The session the TUI shows now, if any.
    const shownSession = () => {
      const route = api.route?.current;
      return route?.name === "session" ? route.params?.sessionID : undefined;
    };
    // Hardening M7: a delivery whose instance was disposed during an await is
    // dropped — the deck asks the next instance about its attempt.
    class Retired extends Error {}
    const stillActive = () => {
      if (!instance.active) throw new Retired();
    };
    // The session a delivery goes to, read when it is made — and read again
    // after every wait (hardening M7), since the person may have switched
    // sessions while a dialog was open.
    const target = async () => {
      await untilNoDialog();
      stillActive();
      const shown = shownSession();
      if (shown) {
        const root = rootOf(shown);
        if (root !== shown) api.route.navigate("session", { sessionID: root });
        return root;
      }
      const created = await api.client.session.create({});
      stillActive();
      const sessionID = created?.data?.id;
      if (!sessionID) throw new Error("no session was created");
      if (shownSession()) throw new Error("the person opened a session meanwhile");
      api.route.navigate("session", { sessionID });
      return sessionID;
    };
    let receiver;
    const deliver = async (message) => {
      const ref = inboxRef(message);
      try {
        stillActive();
        const sessionID = await target();
        const selection = selectionFor(sessionID);
        const request = { sessionID, parts: [{ type: "text", text: inboxText(message) }] };
        if (selection.model) request.model = selection.model;
        if (selection.agent) request.agent = selection.agent;
        // Hardening M7: the last check before the side effect — still this
        // instance, and the screen not moved to another conversation. (A
        // navigation this delivery made may not have reached the route yet,
        // so the route may still show the subagent or the home screen.)
        // Checked again after the deck's approval, which is awaited: the
        // person may have switched sessions or opened a dialog meanwhile.
        const onTarget = () => {
          const now = shownSession();
          return now === undefined || rootOf(now) === sessionID;
        };
        stillActive();
        if (!onTarget()) throw new Error("the session changed");
        await inboxHandOver(
          receiver,
          ref,
          sessionID,
          () => api.client.session.promptAsync(request),
          () => instance.active && !dialogOpen() && onTarget(),
        );
      } catch (error) {
        if (!(error instanceof Retired)) receiver.notHanded(ref);
      }
    };
    receiver = inboxReceiver({
      agent: "opencode",
      active: () => instance.active,
      deliver: (message) => {
        queue = queue.then(() => deliver(message));
      },
    });

    api.event.on("message.updated", (event) => {
      try {
        const info = event?.properties?.info;
        if (info?.role === "user") {
          lastSeen = inboxSelectionOf(info) ?? lastSeen;
          inboxObserveUsers(users, { type: "message.updated", properties: { info } });
          return;
        }
        if (info?.role !== "assistant") return;
        const refs = [];
        for (const key of inboxCovered(users, pending, info)) {
          for (const id of pending.get(key)) refs.push(...receiver.of(id));
          pending.delete(key);
        }
        receiver.committed(refs);
      } catch (_) {}
    });
    api.event.on("message.removed", (event) => {
      try {
        // A reverted prompt: lost, not released.
        for (const id of pending.get(event?.properties?.messageID) ?? []) {
          for (const ref of receiver.of(id)) receiver.lost(ref);
        }
        inboxObserveUsers(users, event, pending);
      } catch (_) {}
    });
    for (const type of ["session.status", "session.idle"]) {
      api.event.on(type, (event) => {
        try {
          const properties = event?.properties;
          const idle = event?.type === "session.idle" || properties?.status?.type === "idle";
          if (!idle || !properties?.sessionID) return;
          // Idle with an attempt handed and not started on: the turn was
          // aborted. Lost, so the deck may repeat it.
          for (const ref of receiver.inSession(properties.sessionID)) receiver.lost(ref);
        } catch (_) {}
      });
    }
    api.event.on("message.part.updated", (event) => {
      try {
        const part = event?.properties?.part;
        if (part?.type !== "text" || !part.messageID) return;
        const ids = inboxIdsIn(part.text).filter((id) => receiver.of(id).length > 0);
        if (ids.length) {
          inboxRemember(pending, part.messageID, [...new Set([...(pending.get(part.messageID) ?? []), ...ids])]);
        }
      } catch (_) {}
    });
    api.lifecycle?.onDispose?.(() => {
      instance.active = false;
      receiver.stop();
    });
    receiver.start();
  } catch (_) {}
};

export default { id: "dot-agent-deck", tui };
"#;

/// Ensure `<root>/plugin/` exists and (over)write the flat `dot-agent-deck.js`
/// pinned to `binary_path`, returning the file path written. Also removes any
/// obsolete nested `dot-agent-deck/` directory (see [`stale_plugin_dir`]) so an
/// upgrade migrates the layout in place. Shared by every install path
/// (auto + explicit + test seam).
#[cfg(test)]
fn write_plugin(root: &Path, binary_path: &str) -> std::io::Result<PathBuf> {
    write_plugin_reporting(root, binary_path).map(|(path, _)| path)
}

/// [`write_plugin`], also reporting whether anything on disk changed. A plugin
/// already holding exactly these bytes, with no stale nested layout beside it,
/// is left untouched — same bytes, inode and mtime (PRD #1487).
fn write_plugin_reporting(root: &Path, binary_path: &str) -> std::io::Result<(PathBuf, bool)> {
    let plugin_dir = root.join("plugin");
    let path = plugin_file(root);
    let stale = stale_plugin_dir(root);
    // PRD #1487: before the directory, the stale-layout removal and the temp
    // file. The stale directory sits beside the plugin, so it is judged too.
    crate::config_write_guard::ensure_config_write_allowed(&path)?;
    crate::config_write_guard::ensure_config_write_allowed(&stale)?;

    let content = plugin_template(binary_path);
    if !stale.is_dir() && std::fs::read(&path).is_ok_and(|existing| existing == content.as_bytes())
    {
        return Ok((path, false));
    }

    std::fs::create_dir_all(&plugin_dir)?;

    // Migrate away from the pre-flat nested layout OpenCode never scanned.
    // Best-effort: a failure to remove the dead dir must not abort the install
    // of the working flat file.
    if stale.is_dir() {
        let _ = std::fs::remove_dir_all(&stale);
    }

    // PRD #381 audit, MEDIUM-2. This was `std::fs::write`, the only one of the
    // four config writers not publishing atomically — and the file it writes is
    // JavaScript OpenCode *executes*. `fs::write` follows a pre-created symlink
    // at the destination and truncates in place, so a crash or a concurrent
    // startup could leave a partial plugin for OpenCode to load. Temp file plus
    // `rename` removes both. (The shared helper's own temp-name predictability
    // is issue #731, which fixes it for all four writers at once; routing this
    // one in is what puts the OpenCode plugin behind that fix too.)
    crate::agent_hook_config::write_atomic(&plugin_dir, &path, content.as_bytes())?;

    Ok((path, true))
}

/// PRD #2050 (review F2): the file, in the deck's state directory, holding
/// the OpenCode generation the deck last saw an agent run: `1` or `2`.
const DETECTED_LAYOUT_FILE: &str = "opencode-layout";

/// [`DETECTED_LAYOUT_FILE`] under the deck's state directory.
fn detected_layout_file() -> PathBuf {
    crate::platform::paths::state_dir().join(DETECTED_LAYOUT_FILE)
}

/// The generation recorded at `path` ([`remember_layout_at`]); `None` when
/// there is none or it cannot be read.
fn detected_major_at(path: &Path) -> Option<crate::opencode_version::Major> {
    let text = crate::bounded_read::read_config_file(path, 16).ok()??;
    match text.trim() {
        "1" => Some(crate::opencode_version::Major::V1),
        "2" => Some(crate::opencode_version::Major::V2),
        _ => None,
    }
}

/// The generation an install with no command to look at goes by: the one the
/// deck last saw an agent run ([`remember_layout`]).
fn remembered_major_now() -> Option<crate::opencode_version::Major> {
    detected_major_at(&detected_layout_file())
}

/// PRD #2050 (review F2): remember `major`, the OpenCode generation an agent
/// was just seen to run, for the next install that has no command to look at
/// (startup, `hooks install`, a wrapper's spawn). Called by a spawn that
/// detected the version of the binary its command names, and by the daemon's
/// OpenCode watch once a wrapper-started OpenCode is seen to run a binary
/// whose version a spawn-time probe recorded, the only way the deck learns
/// the generation a wrapper runs; so the record is normally the latest
/// detection (the spawn's thread and the watch's job can race, and the last
/// write wins). What it changes is only whether 2.x's global root is created
/// on a machine that has none ([`Layout`]).
///
/// It goes to the deck's own state directory, not into OpenCode's config:
/// OpenCode 2 reloads its plugins when the module file changes, and a rewrite
/// while an agent runs ended that agent's session in the middle of a turn
/// (measured on 2.2.5 in lane 2: a `SessionEnd` and a new `SessionStart`, and
/// the turn's idle never reported). Blocking.
pub fn remember_layout(major: crate::opencode_version::Major) {
    let path = detected_layout_file();
    if let Err(e) = remember_layout_at(&path, major) {
        tracing::debug!(
            "could not record the OpenCode generation in {}: {e}",
            path.display()
        );
    }
}

/// [`remember_layout`] into `path`. Whether it wrote.
fn remember_layout_at(path: &Path, major: crate::opencode_version::Major) -> std::io::Result<bool> {
    if detected_major_at(path) == Some(major) {
        return Ok(false);
    }
    crate::config_write_guard::ensure_config_write_allowed(path)?;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the state file has no directory"))?;
    std::fs::create_dir_all(dir)?;
    let text = match major {
        crate::opencode_version::Major::V1 => "1\n",
        crate::opencode_version::Major::V2 => "2\n",
    };
    crate::agent_hook_config::write_atomic(dir, path, text.as_bytes())?;
    Ok(true)
}

/// The `BINARY_PATH` an already-installed plugin under `root` pins, or `None`
/// when there is no plugin file, it cannot be read, or it carries no readable
/// literal (a hand-edited or truncated file).
///
/// The plugin is generated **JavaScript**, not JSON, and PRD #381 blames
/// exactly that shape difference for OpenCode being the last of the three
/// integrations to be noticed — a JSON-shaped fix silently misses it. So this
/// reads the one line [`plugin_template`] emits and decodes the literal with
/// `serde_json`, which is the same escaping the template wrote it with.
///
/// **Line-oriented, and both halves of that are fixes.** It used to
/// `split_once("const BINARY_PATH = ")` and then `split_once(";\n")`:
///
/// - The terminator was the literal `";\n"`, so a plugin a user had opened in a
///   CRLF editor (`;\r\n`) parsed as "no pin found" and the still-valid pin was
///   clobbered on the next auto-install — the "repair only when missing" rule
///   silently bypassed for that corner (reviewer N1). [`str::lines`] splits on
///   `\n` and strips a trailing `\r`, so both line endings decode the same.
/// - The marker matched **anywhere** in the file, including inside a comment in
///   a hand-edited plugin, and whatever followed became the preserved — and
///   republished — value (PRD #381 audit, MEDIUM-1). Anchoring it to the start
///   of a line is what [`plugin_template`] actually emits (`const BINARY_PATH =
///   …;` at column 0), so a commented-out or incidental occurrence cannot
///   become the pin.
///
/// Stripping the trailing `;` rather than splitting on one is deliberate: a `;`
/// inside the path itself survives, because `serde_json` escaping keeps the
/// literal on one line and the `;` the template writes is always the last byte
/// of it. The first matching line wins, as before.
fn existing_binary_path(root: &Path) -> Option<String> {
    existing_pin(&plugin_file(root))
}

/// The `BINARY_PATH` a generated plugin file pins; see
/// [`existing_binary_path`].
fn existing_pin(file: &Path) -> Option<String> {
    let js = std::fs::read_to_string(file).ok()?;
    binary_path_literal(&js)
}

/// The `BINARY_PATH` literal in plugin source `js` — [`existing_binary_path`]'s
/// parse.
fn binary_path_literal(js: &str) -> Option<String> {
    js.lines()
        .filter_map(|line| line.strip_prefix("const BINARY_PATH = "))
        .filter_map(|rest| rest.strip_suffix(';'))
        .find_map(|literal| serde_json::from_str::<String>(literal).ok())
}

/// The text of the plugin module at `path`, read as the pins are
/// ([`crate::bounded_read::read_config_file`]): `None` when it is missing,
/// unreadable, not a regular file (a FIFO, or a symlink to one, included) or
/// longer than `MAX_HOOK_CONFIG_BYTES`.
fn bounded_module_text(path: &Path) -> Option<String> {
    crate::bounded_read::read_config_file(path, crate::agent_hook_config::MAX_HOOK_CONFIG_BYTES)
        .ok()
        .flatten()
}

/// What the OpenCode plugin is pinned to now, read back from every root's
/// plugin file without installing anything (issue #1637's pin refresh,
/// `OPENCODE.configured_pins`), one pin per binary as [`auto_install`] reports
/// them. A root whose directory exists but holds no plugin file names
/// nothing, so with no plugin left anywhere, as after `hooks uninstall
/// --agent opencode`, the list is empty (Qodo on #1656). `None` when no root
/// exists (as at startup, which installs nothing then), or a plugin cannot be
/// read (including one that is not a regular file or is past
/// `MAX_HOOK_CONFIG_BYTES`, audit A9) or carries no readable `BINARY_PATH`,
/// which leaves the pins the daemon knows.
pub fn configured_pins() -> Option<Vec<crate::hook_binary::HookPin>> {
    configured_pins_in(&candidate_roots())
}

/// [`configured_pins`] over explicit roots.
fn configured_pins_in(roots: &[PathBuf]) -> Option<Vec<crate::hook_binary::HookPin>> {
    // The same gate as `auto_install_resolved`.
    if !roots.iter().any(|root| root.exists()) {
        return None;
    }
    let mut out: Vec<crate::hook_binary::HookPin> = Vec::new();
    for root in roots {
        // Bounded and non-blocking, as the JSON configs are read (audit A9).
        let js = match crate::bounded_read::read_config_file(
            &plugin_file(root),
            crate::agent_hook_config::MAX_HOOK_CONFIG_BYTES,
        ) {
            Ok(Some(js)) => js,
            Ok(None) => continue,
            Err(_) => return None,
        };
        let binary = binary_path_literal(&js)?;
        if out.iter().all(|pin| pin.binary != binary) {
            out.push(crate::hook_binary::HookPin {
                agent: crate::event::AgentType::OpenCode,
                config: root.clone(),
                binary,
            });
        }
    }
    Some(out)
}

/// Remove one plugin artifact — a flat file or an obsolete nested dir — and print
/// a line naming what was removed. A missing path is reported, not an error.
fn uninstall_impl(path: &PathBuf) -> std::io::Result<()> {
    // PRD #1487: a removal is a write too, and is refused the same way.
    crate::config_write_guard::ensure_config_write_allowed(path)?;
    if !path.exists() {
        println!("No OpenCode plugin found to remove.");
        return Ok(());
    }

    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }

    println!("Removed OpenCode plugin: {}", path.display());
    Ok(())
}

/// Fan-out core for auto-install: for every candidate root that exists, refresh the
/// flat `plugin/dot-agent-deck.js` (migrating away from any stale nested layout).
/// Roots that don't exist are skipped — no speculative directory creation. Per-target
/// failures are logged via `tracing::warn!` and never abort the remaining targets.
/// Silent on stdout (dashboard startup path).
///
/// Returns the binary each existing root's plugin pins afterwards (issue
/// #1637), so the daemon can name the binary OpenCode's events come from.
#[cfg(test)]
fn auto_install_to(roots: &[PathBuf], binary_path: &str) -> Vec<(PathBuf, String)> {
    auto_install_to_reporting(roots, binary_path, &mut Vec::new())
}

/// The adapter integration a generated plugin file declares, `0` for a file
/// that declares none (one from before the integration existed) and `None`
/// when there is no file to read.
fn template_integration(file: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(file).ok()?;
    let declared = text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("const INBOX_ADAPTER_INTEGRATION = ")
        })
        .and_then(|rest| rest.trim_end_matches(';').trim().parse::<u32>().ok());
    Some(declared.unwrap_or(0))
}

/// PRD #1666 §11: whether the plugin file at `file` is a template below the
/// deck's integration floor, which a component would be refused for.
fn below_floor(file: &Path) -> bool {
    template_integration(file).is_some_and(|n| n < crate::inbox_hub::ADAPTER_INTEGRATION)
}

/// [`auto_install_to`], adding each root it could not write to `failed`.
#[cfg(test)]
fn auto_install_to_reporting(
    roots: &[PathBuf],
    binary_path: &str,
    failed: &mut Vec<String>,
) -> Vec<(PathBuf, String)> {
    auto_install_to_reporting_for(roots, binary_path, failed)
}

/// The template revision the module at `file` declares
/// ([`PLUGIN_TEMPLATE_REVISION`]), `0` for one that declares none (one
/// written before revisions existed) and `None` when there is no module to
/// read. Read bounded and without blocking ([`bounded_module_text`]).
fn template_revision(file: &Path) -> Option<u32> {
    let text = bounded_module_text(file)?;
    let declared = text
        .lines()
        .find_map(|line| line.strip_prefix("const DAD_TEMPLATE_REVISION = "))
        .and_then(|rest| rest.trim_end_matches(';').trim().parse::<u32>().ok());
    Some(declared.unwrap_or(0))
}

/// Audit N4: whether the module at `file` is of a later template revision
/// than this binary writes, which an automatic install leaves alone.
fn newer_template(file: &Path) -> bool {
    template_revision(file).is_some_and(|n| n > PLUGIN_TEMPLATE_REVISION)
}

/// The file identity of the executable this process runs: on Linux the file
/// `/proc/self/exe` resolves to, which is still the running image after an
/// upgrade replaced the file at its path (the link then reads `… (deleted)`,
/// and `stat` follows it to the old file). Elsewhere, the identity the file
/// at `current_exe()` had the first time this was asked, which `daemon serve`
/// does at startup ([`note_own_executable`]); a process that never asks
/// before its file is replaced takes the new file for its own.
fn own_executable_key() -> Option<crate::opencode_version::FileKey> {
    #[cfg(test)]
    if let Some(path) = RUNNING_EXE_FOR_TEST.with(|p| p.borrow().clone()) {
        return crate::opencode_version::FileKey::of(&path);
    }
    #[cfg(target_os = "linux")]
    {
        crate::opencode_version::FileKey::of(Path::new("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        static AT_START: std::sync::OnceLock<Option<crate::opencode_version::FileKey>> =
            std::sync::OnceLock::new();
        AT_START
            .get_or_init(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| crate::opencode_version::FileKey::of(&exe))
            })
            .clone()
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: the executable [`own_executable_key`] takes as this process's.
    static RUNNING_EXE_FOR_TEST: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Record this process's executable now, at startup, for the platforms where
/// [`own_executable_key`] cannot read the running image after its file is
/// replaced. A no-op on Linux.
pub fn note_own_executable() {
    let _ = own_executable_key();
}

/// Auditor M2: whether this process runs an executable other than the file
/// now at `pin`, i.e. it is an older deck still running after the install at
/// `pin` was replaced. `false` when either identity cannot be read.
fn running_other_than(pin: &str) -> bool {
    match (
        own_executable_key(),
        crate::opencode_version::FileKey::of(Path::new(pin)),
    ) {
        (Some(running), Some(at_pin)) => running != at_pin,
        _ => false,
    }
}

/// [`auto_install_to_reporting`]: what every automatic install of the server
/// module runs.
fn auto_install_to_reporting_for(
    roots: &[PathBuf],
    binary_path: &str,
    failed: &mut Vec<String>,
) -> Vec<(PathBuf, String)> {
    // Held across the read-preserve-write span, not just the write. See
    // [`PLUGIN_LOCK`].
    let _guard = lock_plugin();
    let takeover = crate::hook_binary::Takeover::current();
    let mut pins = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        // PRD #381 M5, and its Open Question 3: repair only what is UNUSABLE.
        // This path used to rewrite `BINARY_PATH` unconditionally on every
        // dashboard startup, so a perfectly valid pin was clobbered by whatever
        // binary happened to be launching — the "not merely different" rule
        // broken on the one integration where nobody was looking. The file is
        // still regenerated either way, so a template change still lands; only
        // the pinned path is carried over.
        //
        // Not a bare existence probe: a legacy plugin pinning the BARE
        // `"dot-agent-deck"` would otherwise be preserved whenever the process
        // cwd happened to hold a file of that name, and Node's `execFileSync`
        // then resolves that persisted bare name through the AGENT's `$PATH`
        // (PRD #381 audit, MEDIUM-1 — issue #536's own vector).
        //
        // PRD #1487: a plugin pinning ANOTHER live, durable install is not
        // regenerated, apart from the two exceptions below (a takeover by an
        // eligible newer copy, and a template below the integration floor,
        // regenerated around the same pin). Two installs of the deck each resolve to
        // themselves, and a template that differs between their versions would
        // otherwise be rewritten by whichever started last, every start. The
        // pinned install refreshes its own template when it starts; `hooks
        // install --agent opencode` is how a user picks a different one.
        //
        // PRD #1487 re-check R3: kept only on POSITIVE liveness — the rule the
        // Claude, Codex and Devin automatic installs follow. Anything that is
        // not established to be a live, durable install (bare or relative,
        // missing, a path whose existence cannot be read, not executable, cargo
        // build output) is replaced by the installing binary. This path used to
        // fall back to `pin_is_repairable` here, which leaves an unstatable pin
        // alone, so an inaccessible install stayed pinned for OpenCode alone.
        //
        // Issue #1637: except one an eligible newer copy supersedes — a pin
        // whose `--version` reports an older release than this install's. It
        // is replaced like an unusable pin, and logged as a takeover.
        let mut taking_over = None;
        let (pinned, repairing) = match existing_binary_path(root) {
            Some(existing) if crate::agent_hook_config::auto_install_keeps(&existing) => {
                let this_install =
                    crate::agent_hook_config::executables_match(&existing, binary_path);
                if newer_template(&plugin_file(root))
                    && (!this_install || running_other_than(&existing))
                {
                    // Audit N4: a module a newer deck wrote, around a pin
                    // that is usable, is left as it is when it pins another
                    // install, or this install while this process is the
                    // older deck still running after an upgrade replaced it:
                    // that process would otherwise put its older template
                    // back, and each rewrite reloads every live OpenCode 2
                    // pane's plugin. When the file at the pin is the one this
                    // process runs (auditor M2: a deliberate rollback, or a
                    // config synced from a machine with a newer deck), this
                    // install's template is written as before, since a newer
                    // template may call what this build lacks. `hooks install
                    // --agent opencode` writes this binary's template either
                    // way.
                    pins.push((root.clone(), existing));
                    continue;
                }
                if this_install {
                    // This very install: refresh the template, keep the pin's
                    // spelling.
                    (existing, false)
                } else if takeover.supersedes(&existing) {
                    taking_over = Some(existing);
                    (binary_path.to_string(), true)
                } else if below_floor(&plugin_file(root)) {
                    // PRD #1666 §11: the other install's pin is kept, but a
                    // template below the integration floor would be refused,
                    // so it is regenerated with that same pin. This branch
                    // rewrites only a stale template; a pin that an eligible
                    // newer copy supersedes was already taken over above.
                    (existing, false)
                } else {
                    pins.push((root.clone(), existing));
                    continue;
                }
            }
            Some(_) => (binary_path.to_string(), true),
            None => (binary_path.to_string(), false),
        };
        let written = write_plugin_reporting(root, &pinned);
        if let Err(e) = &written {
            failed.push(format!("{}: {e}", plugin_file(root).display()));
        }
        // Reported only once it is what the plugin holds, as the Claude and
        // Devin installers do: a failed write leaves the old pin in place.
        if written.is_ok() {
            pins.push((root.clone(), pinned.clone()));
        }
        match written {
            // Already current: nothing was written, so nothing is announced.
            Ok((_, false)) => {}
            Ok((path, true)) if taking_over.is_some() => {
                tracing::info!(
                    "the OpenCode plugin at {} pinned an older dot-agent-deck ({}); now pinned \
                     to this newer copy, {pinned}",
                    path.display(),
                    taking_over.as_deref().unwrap_or_default()
                );
                crate::agent_hook_config::log_auto_install_change(
                    "opencode",
                    &path,
                    &pinned,
                    "takeover: newer copy",
                );
            }
            // Repair logs what it changed: silently mutating global config is
            // the same class of thing that caused this bug.
            Ok((path, true)) if repairing => {
                tracing::info!(
                    "repaired the OpenCode plugin at {}: its BINARY_PATH was not a usable \
                     durable path, now pinned to {pinned}",
                    path.display()
                );
                crate::agent_hook_config::log_auto_install_change(
                    "opencode",
                    &path,
                    &pinned,
                    "opencode startup auto-install",
                );
            }
            Ok((path, true)) => crate::agent_hook_config::log_auto_install_change(
                "opencode",
                &path,
                &pinned,
                "opencode startup auto-install",
            ),
            Err(e) => tracing::warn!(
                "auto-install: failed to write OpenCode plugin under {}: {e}",
                root.display()
            ),
        }
    }
    pins
}

/// Fan-out core for explicit install: write the plugin into every candidate root that
/// exists; if none exist, fall back to `fallback_root()` (the XDG-default config root),
/// creating it — the first-time-install behavior. The fallback closure is evaluated
/// lazily, only when no layout exists, so the common path avoids the extra filesystem
/// probe. Each successful write emits one `Installed OpenCode plugin: <path>` line to
/// `out`. Every target is attempted even if an earlier one fails; the first error (if
/// any) is returned so the caller surfaces it.
#[cfg(test)]
fn install_to_roots(
    roots: &[PathBuf],
    fallback_root: impl FnOnce() -> PathBuf,
    binary_path: &str,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    install_to_roots_for(roots, fallback_root, binary_path, out)
}

/// [`install_to_roots`]: what `hooks install --agent opencode` runs.
fn install_to_roots_for(
    roots: &[PathBuf],
    fallback_root: impl FnOnce() -> PathBuf,
    binary_path: &str,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    // The explicit path writes rather than preserves, but it publishes to the
    // same file the auto path reads back — so it takes the same lock. See
    // [`PLUGIN_LOCK`].
    let _guard = lock_plugin();
    let mut targets: Vec<PathBuf> = Vec::new();
    for root in roots {
        if root.exists() {
            targets.push(root.clone());
        }
    }
    if targets.is_empty() {
        targets.push(fallback_root());
    }

    let mut first_err: Option<std::io::Error> = None;
    for root in &targets {
        match write_plugin_reporting(root, binary_path) {
            Ok((path, _)) => {
                let _ = writeln!(out, "Installed OpenCode plugin: {}", path.display());
            }
            Err(e) => {
                tracing::warn!(
                    "install: failed to write OpenCode plugin under {}: {e}",
                    root.display()
                );
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// PRD #1666 S8: the TUI plugin file under an OpenCode config root. Not under
/// `plugin/`, where OpenCode's server-plugin glob would load it as a server
/// plugin, and not the obsolete nested `plugin/dot-agent-deck/` directory.
fn tui_plugin_file(root: &Path) -> PathBuf {
    root.join("dot-agent-deck-tui.js")
}

/// The files OpenCode reads its TUI config from, in the order the installer
/// edits them: `tui.json` if it exists, otherwise `tui.jsonc`.
const TUI_CONFIG_FILES: [&str; 2] = ["tui.json", "tui.jsonc"];

/// The largest `tui.json` the installer edits.
const MAX_TUI_CONFIG_BYTES: u64 = 1024 * 1024;

/// The `plugin` entry naming `file`, as a JSON string.
fn tui_entry_literal(file: &Path) -> String {
    serde_json::to_string(&file.to_string_lossy()).expect("serializing a str is infallible")
}

/// The `tui.json` the installer creates when there is none.
fn fresh_tui_config(file: &Path) -> String {
    format!(
        "{{\"$schema\": \"https://opencode.ai/tui.json\", \"plugin\": [{}]}}\n",
        tui_entry_literal(file)
    )
}

/// Whether `entry` is the deck's own TUI plugin entry: the TUI file's path
/// under a candidate root, as a plain path or a `file://` URL. Anything else,
/// even an entry that mentions `dot-agent-deck`, is the user's.
fn is_deck_tui_entry(entry: &str) -> bool {
    candidate_roots()
        .iter()
        .any(|root| names_tui_file(entry, root))
}

/// Whether `entry` names the deck's TUI file under `root`, as a plain path or
/// a `file://` URL.
fn names_tui_file(entry: &str, root: &Path) -> bool {
    let file = tui_plugin_file(root);
    let path = file.to_string_lossy();
    entry == path || entry.strip_prefix("file://") == Some(&*path)
}

/// [`is_deck_tui_entry`], also accepting the TUI file under any of `roots`
/// (the roots an edit was handed, which are the candidate roots outside a
/// test).
fn is_deck_tui_entry_under(entry: &str, roots: &[&Path]) -> bool {
    roots.iter().any(|root| names_tui_file(entry, root)) || is_deck_tui_entry(entry)
}

/// A TUI config's text, `None` when the file does not exist, or why it may
/// not be edited.
///
/// PRD #1666 hardening M5: the type and size are judged on the descriptor that
/// is then read — opened without following a final symlink, and without
/// blocking on a FIFO — and the read itself is bounded, so a file swapped in
/// between a check and the read is not what gets read.
fn read_tui_config(path: &Path) -> Result<Option<String>, String> {
    let symlink = "it is a symlink, and rewriting it would replace the link";
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Err(symlink.to_string()),
        Ok(metadata) if !metadata.is_file() => {
            return Err("it is not a regular file".to_string());
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("it cannot be read ({e})")),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) => {
            return Err(symlink.to_string());
        }
        Err(e) => return Err(format!("it cannot be read ({e})")),
    };
    let metadata = file
        .metadata()
        .map_err(|e| format!("it cannot be read ({e})"))?;
    if !metadata.is_file() {
        return Err("it is not a regular file".to_string());
    }
    if metadata.len() > MAX_TUI_CONFIG_BYTES {
        return Err("it is larger than 1 MiB".to_string());
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_TUI_CONFIG_BYTES + 1),
        &mut bytes,
    )
    .map_err(|e| format!("it cannot be read ({e})"))?;
    if bytes.len() as u64 > MAX_TUI_CONFIG_BYTES {
        return Err("it is larger than 1 MiB".to_string());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "it is not UTF-8".to_string())
}

/// PRD #1666 hardening M5: the cross-process lock every edit of `root`'s TUI
/// configs holds from the first read to the last publish — the sidecar lock
/// the deck's other agent-config writers take ([`crate::agent_hook_config::lock_config`]),
/// one per root, named for its first TUI config.
fn lock_tui_configs(root: &Path) -> std::io::Result<crate::agent_hook_config::ConfigLock> {
    crate::agent_hook_config::lock_config(&root.join(TUI_CONFIG_FILES[0]))
}

/// Why a TUI config edit stops: the file changed under the edit, which only a
/// writer outside the deck's lock can do (an editor, OpenCode itself).
const CHANGED_DURING_EDIT: &str =
    "it changed while the deck was editing it; run the install again to retry";

/// What became of the deck's entry in a TUI config.
enum TuiEntry {
    Added(PathBuf),
    Present(PathBuf),
    Refused(PathBuf, String),
}

/// Write the TUI plugin under `root` pinned to `pin` (left alone when `None`,
/// or when it already holds those bytes), then make sure a TUI config lists
/// it (design §7.2). An `Err` is a write the config guard or the filesystem
/// refused; a config the installer will not edit is [`TuiEntry::Refused`],
/// with the TUI file still written. Callers hold [`PLUGIN_LOCK`].
fn install_tui_half(root: &Path, pin: Option<&str>) -> std::io::Result<TuiEntry> {
    let file = tui_plugin_file(root);
    crate::config_write_guard::ensure_config_write_allowed(&file)?;
    for name in TUI_CONFIG_FILES {
        crate::config_write_guard::ensure_config_write_allowed(&root.join(name))?;
    }
    let _lock = lock_tui_configs(root)?;
    if let Some(pin) = pin {
        let content = tui_plugin_template(pin);
        if std::fs::read(&file).ok().as_deref() != Some(content.as_bytes()) {
            crate::agent_hook_config::write_atomic(root, &file, content.as_bytes())?;
            crate::agent_hook_config::note_config_change();
        }
    }
    let entry = file.to_string_lossy().into_owned();
    let configs: Vec<(PathBuf, Result<Option<String>, String>)> = TUI_CONFIG_FILES
        .iter()
        .map(|name| {
            let path = root.join(name);
            let text = read_tui_config(&path);
            (path, text)
        })
        .collect();
    for (path, text) in &configs {
        if let Ok(Some(text)) = text
            && crate::jsonc_edit::has_entry(text, |e| is_deck_tui_entry_under(e, &[root]))
                .unwrap_or(false)
        {
            return Ok(TuiEntry::Present(path.clone()));
        }
    }
    let exists = |path: &Path| std::fs::symlink_metadata(path).is_ok();
    let (target, text) = configs
        .iter()
        .find(|(path, _)| exists(path))
        .cloned()
        .unwrap_or_else(|| (root.join(TUI_CONFIG_FILES[0]), Ok(None)));
    let updated = match &text {
        Err(why) => return Ok(TuiEntry::Refused(target, why.clone())),
        Ok(None) => fresh_tui_config(&file),
        Ok(Some(text)) => match crate::jsonc_edit::insert_entry(text, &entry) {
            Ok(updated) => updated,
            Err(why) => return Ok(TuiEntry::Refused(target, why.to_string())),
        },
    };
    // Hardening M5: a writer outside the lock may have saved the file since
    // it was read; publishing the splice would undo that save.
    if read_tui_config(&target) != text {
        return Ok(TuiEntry::Refused(target, CHANGED_DURING_EDIT.to_string()));
    }
    crate::agent_hook_config::write_atomic(root, &target, updated.as_bytes())?;
    crate::agent_hook_config::note_config_change();
    Ok(TuiEntry::Added(target))
}

/// The automatic install's TUI half: the pin follows the server plugin's
/// rules, and a refusal is one `warn!` and nothing on stdout.
fn auto_install_tui(root: &Path, binary_path: &str, failed: &mut Vec<String>) {
    let _guard = lock_plugin();
    // Issue #1637: a pin an eligible newer copy supersedes moves to this
    // install, as the server plugin's does in `auto_install_to`.
    let takeover = crate::hook_binary::Takeover::current();
    let pin = match existing_pin(&tui_plugin_file(root)) {
        Some(existing) if crate::agent_hook_config::auto_install_keeps(&existing) => {
            if crate::agent_hook_config::executables_match(&existing, binary_path) {
                Some(existing)
            } else if takeover.supersedes(&existing) {
                Some(binary_path.to_string())
            } else if below_floor(&tui_plugin_file(root)) {
                // PRD #1666 §11: as `auto_install_to` does — the other
                // install's pin kept, its template below the floor
                // regenerated.
                Some(existing)
            } else {
                None
            }
        }
        _ => Some(binary_path.to_string()),
    };
    let installed = install_tui_half(root, pin.as_deref());
    if let Err(e) = &installed {
        failed.push(format!("{}: {e}", tui_plugin_file(root).display()));
    }
    match installed {
        Ok(TuiEntry::Added(file)) => tracing::info!(
            "auto-install: added the OpenCode TUI plugin to {}",
            file.display()
        ),
        Ok(TuiEntry::Present(_)) => {}
        Ok(TuiEntry::Refused(file, why)) => tracing::warn!(
            "auto-install: left {} unchanged: {why}; deck messages will reach OpenCode by \
             the doorbell instead. To enable them, add {} to its \"plugin\" list",
            file.display(),
            tui_entry_literal(&tui_plugin_file(root))
        ),
        Err(e) => tracing::warn!(
            "auto-install: failed to install the OpenCode TUI plugin under {}: {e}",
            root.display()
        ),
    }
}

/// Remove the deck's entry from `root`'s TUI configs (deleting one the
/// installer created from nothing), then the TUI file, telling `say` what
/// was removed. A config that cannot be edited safely, while the deck's TUI
/// plugin may be listed in it, stops here with nothing deleted, so OpenCode
/// is never left listing a plugin file that is gone.
fn uninstall_tui_half(root: &Path, say: &mut dyn FnMut(String)) -> std::io::Result<()> {
    let file = tui_plugin_file(root);
    let _lock = lock_tui_configs(root)?;
    let file_exists = std::fs::symlink_metadata(&file).is_ok();
    let mut edits = Vec::new();
    for name in TUI_CONFIG_FILES {
        let path = root.join(name);
        let text = match read_tui_config(&path) {
            Ok(None) => continue,
            Ok(Some(text)) => text,
            Err(why) if file_exists => return Err(left_unchanged(&path, &file, &why, say)),
            Err(_) => continue,
        };
        if text == fresh_tui_config(&file) {
            edits.push((path, text, None));
            continue;
        }
        match crate::jsonc_edit::remove_entries(&text, |e| is_deck_tui_entry_under(e, &[root])) {
            Ok(None) => {}
            Ok(Some(updated)) => edits.push((path, text, Some(updated))),
            Err(why) if file_exists || text.contains("dot-agent-deck-tui.js") => {
                return Err(left_unchanged(&path, &file, &why.to_string(), say));
            }
            Err(_) => {}
        }
    }
    for (path, _, _) in &edits {
        crate::config_write_guard::ensure_config_write_allowed(path)?;
    }
    if file_exists {
        crate::config_write_guard::ensure_config_write_allowed(&file)?;
    }
    // Hardening M5: nothing is changed when a config was saved since it was
    // read, by a writer outside the deck's lock.
    for (path, text, _) in &edits {
        if read_tui_config(path).ok().flatten().as_ref() != Some(text) {
            return Err(left_unchanged(path, &file, CHANGED_DURING_EDIT, say));
        }
    }
    for (path, _, updated) in edits {
        match updated {
            Some(updated) => {
                crate::agent_hook_config::write_atomic(root, &path, updated.as_bytes())?;
                crate::agent_hook_config::note_config_change();
                say(format!(
                    "Removed the OpenCode TUI plugin from {}",
                    path.display()
                ));
            }
            None => {
                std::fs::remove_file(&path)?;
                crate::agent_hook_config::note_config_change();
                say(format!("Removed {}", path.display()));
            }
        }
    }
    if file_exists {
        std::fs::remove_file(&file)?;
        crate::agent_hook_config::note_config_change();
        say(format!("Removed OpenCode TUI plugin: {}", file.display()));
    }
    Ok(())
}

fn left_unchanged(
    config: &Path,
    file: &Path,
    why: &str,
    say: &mut dyn FnMut(String),
) -> std::io::Error {
    say(format!(
        "Left {} unchanged: {why}. Kept {} so OpenCode does not list a missing plugin. \
         Remove {} from its \"plugin\" list, then run the uninstall again.",
        config.display(),
        file.display(),
        tui_entry_literal(file)
    ));
    std::io::Error::other(format!("left {} unchanged: {why}", config.display()))
}

/// PRD #2050: OpenCode 2.x's CLI config file, `<root>/cli.json`. 2.x creates
/// it when it starts with none, copying 1.x's `<root>/tui.json` `plugin` list
/// into its `plugins` list (measured on 2.2.5; source: `cli.config.migrate`,
/// which reads `tui.json` only while `cli.json` is absent, so the copy is
/// made once per missing `cli.json`, not on every start). The deck's 1.x TUI
/// entry copied there names a file, which 2.x does not load as a TUI plugin.
const CLI_CONFIG_FILE: &str = "cli.json";

/// The list in [`CLI_CONFIG_FILE`] that holds the copied entries.
const CLI_PLUGIN_LIST: &str = "plugins";

/// The roots whose `cli.json` the deck cleans: OpenCode 2.x's global root,
/// and the XDG root when `$OPENCODE_CONFIG_DIR` moved it elsewhere.
fn cli_roots(xdg: &Path, v2_global: &Path) -> Vec<PathBuf> {
    let mut roots = vec![v2_global.to_path_buf()];
    if xdg != v2_global {
        roots.push(xdg.to_path_buf());
    }
    roots
}

/// PRD #2050: remove the deck's own 1.x TUI entry ([`is_deck_tui_entry`])
/// from `<root>/cli.json`'s `plugins` list, keeping every other entry and
/// byte of the file ([`crate::jsonc_edit`]). A missing `cli.json` is never
/// created. `Ok(Some(path))` when an entry was removed, `Ok(None)` when there
/// was nothing to remove, and `Err` with the reason when the file holds the
/// entry (or may: it mentions the deck's TUI file) but could not be edited.
///
/// OpenCode 1.x never reads `cli.json`, so the entry serves no generation and
/// goes whatever layout is being installed.
fn remove_cli_entry(root: &Path, tui_roots: &[&Path]) -> Result<Option<PathBuf>, String> {
    let path = root.join(CLI_CONFIG_FILE);
    if std::fs::symlink_metadata(&path).is_err() {
        return Ok(None);
    }
    // Asked only of a file the edit refused (a symlink, not a regular file,
    // too large, unparsable), to tell a refusal worth reporting from one that
    // concerns no entry of the deck's. Audit L2: bounded and non-blocking
    // ([`crate::bounded_read::read_config_file`]), so a FIFO or a symlink to
    // `/dev/zero` cannot hang or flood the spawn; a file it cannot read that
    // way may hold the entry, so the refusal is reported.
    let mentions_deck = || {
        crate::bounded_read::read_config_file(&path, MAX_TUI_CONFIG_BYTES).map_or(true, |text| {
            text.is_some_and(|text| text.contains("dot-agent-deck-tui.js"))
        })
    };
    let text = match read_tui_config(&path) {
        Ok(None) => return Ok(None),
        Ok(Some(text)) => text,
        Err(why) if mentions_deck() => return Err(why),
        Err(_) => return Ok(None),
    };
    let updated = match crate::jsonc_edit::remove_list_entries(&text, CLI_PLUGIN_LIST, |e| {
        is_deck_tui_entry_under(e, tui_roots)
    }) {
        Ok(None) => return Ok(None),
        Ok(Some(updated)) => updated,
        Err(why) if mentions_deck() => return Err(why.to_string()),
        Err(_) => return Ok(None),
    };
    crate::config_write_guard::ensure_config_write_allowed(&path).map_err(|e| e.to_string())?;
    let _lock = crate::agent_hook_config::lock_config(&path).map_err(|e| e.to_string())?;
    // Hardening M5, as for `tui.json`: OpenCode or an editor may have saved
    // the file since it was read; publishing the splice would undo that save.
    if read_tui_config(&path) != Ok(Some(text)) {
        return Err(CHANGED_DURING_EDIT.to_string());
    }
    crate::agent_hook_config::write_atomic(root, &path, updated.as_bytes())
        .map_err(|e| e.to_string())?;
    crate::agent_hook_config::note_config_change();
    Ok(Some(path))
}

/// [`remove_cli_entry`] for every root of `roots` (which are also the roots
/// the deck's TUI file may be named under), as the automatic install does
/// it: quiet on stdout, a refusal added to `failed`.
fn auto_remove_cli_entries(roots: &[PathBuf], failed: &mut Vec<String>) {
    let tui_roots: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    for root in roots {
        match remove_cli_entry(root, &tui_roots) {
            Ok(Some(path)) => tracing::info!(
                "auto-install: removed the deck's OpenCode 1.x TUI plugin entry from {}, \
                 where OpenCode 2 copied it from tui.json and cannot load it",
                path.display()
            ),
            Ok(None) => {}
            Err(why) => {
                let path = root.join(CLI_CONFIG_FILE);
                failed.push(format!("{}: {why}", path.display()));
                tracing::warn!(
                    "auto-install: left {} unchanged: {why}; remove the entry naming \
                     dot-agent-deck-tui.js from its \"plugins\" list, which OpenCode 2 cannot \
                     load",
                    path.display()
                );
            }
        }
    }
}

/// [`remove_cli_entry`] for every root of `roots`, as `hooks install` and
/// `hooks uninstall` do it: each outcome printed, the first refusal returned.
fn remove_cli_entries_printing(roots: &[PathBuf]) -> std::io::Result<()> {
    let tui_roots: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    let mut first_err = None;
    for root in roots {
        let path = root.join(CLI_CONFIG_FILE);
        match remove_cli_entry(root, &tui_roots) {
            Ok(Some(path)) => println!(
                "Removed the OpenCode 1.x TUI plugin entry from {}",
                path.display()
            ),
            Ok(None) => {}
            Err(why) => {
                println!(
                    "Left {} unchanged: {why}. Remove the entry naming dot-agent-deck-tui.js \
                     from its \"plugins\" list: OpenCode 2 cannot load it.",
                    path.display()
                );
                first_err.get_or_insert_with(|| {
                    std::io::Error::other(format!("left {} unchanged: {why}", path.display()))
                });
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Silently install OpenCode plugin into every existing layout, and the TUI
/// plugin into the XDG root whenever OpenCode is installed (PRD #1666 S8).
/// Intended for dashboard startup — never prints to stdout, refusal included.
pub fn auto_install() -> Vec<crate::hook_binary::HookPin> {
    auto_install_reporting().0
}

/// [`auto_install`] for `hooks refresh` (PRD #1666 part 3, R-3): whether
/// OpenCode is installed here (a config root exists, as the install asks),
/// and every integration file it could not write, which [`auto_install`]
/// only logs. A root that cannot be inspected is a failure, not absence: only
/// every root confirmed missing leaves OpenCode out.
pub fn refresh_install() -> crate::hooks_refresh::InstallAttempt {
    let mut found = false;
    let mut failed = Vec::new();
    for root in candidate_roots() {
        match root.try_exists() {
            Ok(exists) => found |= exists,
            Err(e) => failed.push(format!("could not inspect {}: {e}", root.display())),
        }
    }
    if !found && failed.is_empty() {
        return crate::hooks_refresh::InstallAttempt::Absent;
    }
    if found {
        failed.extend(auto_install_reporting().1);
    }
    crate::hooks_refresh::InstallAttempt::Ran { failed }
}

/// PRD #1666 §11: [`auto_install`], run as an OpenCode generation starts and
/// before its child does, so the agent loads the integration this deck checks
/// it against. Returns the pins and every integration file it could not
/// write, for the card to say so.
///
/// PRD #2050: `major` is the generation the spawn's command runs, when the
/// deck could tell; with none (a wrapper), the one the deck last saw an agent
/// run ([`remembered_major_now`]), as for an install with no command at all.
pub fn refresh_for_spawn(
    major: Option<crate::opencode_version::Major>,
) -> (Vec<crate::hook_binary::HookPin>, Vec<String>) {
    let major = major.or_else(remembered_major_now);
    auto_install_reporting_for(Layout::for_major(major))
}

fn auto_install_reporting() -> (Vec<crate::hook_binary::HookPin>, Vec<String>) {
    refresh_for_spawn(None)
}

/// PRD #2050: the automatic install for `layout`, over the real config roots,
/// pinned to this deck's durable binary.
fn auto_install_reporting_for(layout: Layout) -> (Vec<crate::hook_binary::HookPin>, Vec<String>) {
    let mut failed = Vec::new();
    let roots = candidate_roots();
    let (_, pins) = auto_install_layout(&roots, &v2_global_root(), layout, &mut failed, || {
        crate::platform::paths::durable_binary_path()
    });
    // One pin per binary: the roots of one machine name the same install.
    let mut out: Vec<crate::hook_binary::HookPin> = Vec::new();
    for (root, binary) in pins {
        if out.iter().all(|pin| pin.binary != binary) {
            out.push(crate::hook_binary::HookPin {
                agent: crate::event::AgentType::OpenCode,
                config: root,
                binary,
            });
        }
    }
    (out, failed)
}

/// PRD #2050: the automatic install of `layout` over `roots` (1.x's global
/// root first, then the legacy one, then `$OPENCODE_CONFIG_DIR` when it names
/// another directory) and OpenCode 2.x's global root `v2_global`, with the
/// binary resolution injected. Returns the binary resolved and the pin each
/// root holds afterwards.
///
/// The same files whatever `layout` says, wherever a global root exists
/// ([`Layout`]): the module in the existing global roots with the deck's
/// legacy copy removed ([`auto_install_global_resolved`]), both TUI halves,
/// and the deck's dead entry removed from OpenCode 2.x's `cli.json`
/// ([`auto_remove_cli_entries`]). So installs for alternating generations
/// write nothing once the first has run, and the module's bytes change only
/// when its template or pin does.
fn auto_install_layout(
    roots: &[PathBuf],
    v2_global: &Path,
    layout: Layout,
    failed: &mut Vec<String>,
    resolve: impl FnOnce() -> Result<String, String>,
) -> (Option<String>, Vec<(PathBuf, String)>) {
    let global = global_roots(roots, v2_global);
    // Auditor L2: the XDG root the TUI half below creates whenever any root
    // exists is created first, so a machine with only `~/.opencode` gets the
    // module in the global root in this install, not after the next one
    // moves it there.
    let any_root = roots.iter().any(|root| root.exists()) || v2_global.exists();
    let mut xdg_refused = false;
    if any_root && let Some(xdg) = roots.first().filter(|xdg| !xdg.exists()) {
        xdg_refused = !ensure_root(xdg, &tui_plugin_file(xdg), failed);
    }
    let (binary_path, pins) = if layout == Layout::V2 || global.iter().any(|root| root.exists()) {
        auto_install_global_resolved(
            roots,
            v2_global,
            &global,
            layout == Layout::V2,
            failed,
            resolve,
        )
    } else {
        auto_install_resolved_for(roots, failed, resolve)
    };
    // PRD #1666 S8: the TUI plugin, into the XDG root, from the same resolved
    // binary the server plugin was installed with. OpenCode reads `tui.json`
    // from the XDG root only, so a legacy-only layout gets its TUI half there
    // too (PR #1735 review): OpenCode creates that root itself on any run
    // (measured on 1.18.35, `opencode --version` included), so creating it for
    // an installed OpenCode is not speculative. With no root at all nothing is
    // created. PRD #2050: OpenCode 2.x reads its TUI plugins from its own
    // global root, as a `plugins/` directory. Both halves, whatever the
    // generation: each is invisible to the other generation.
    let Some(binary_path) = binary_path.as_deref() else {
        return (None, pins);
    };
    if !roots.iter().any(|root| root.exists()) && !v2_global.exists() {
        return (Some(binary_path.to_string()), pins);
    }
    let Some(xdg) = roots.first() else {
        return (Some(binary_path.to_string()), pins);
    };
    if !xdg_refused && ensure_root(xdg, &tui_plugin_file(xdg), failed) {
        auto_install_tui(xdg, binary_path, failed);
    }
    if (!xdg_refused || v2_global != xdg.as_path())
        && ensure_root(v2_global, &tui_v2_file(v2_global), failed)
    {
        auto_install_tui_v2(v2_global, binary_path, failed);
    }
    // A 2.x half left in a root 2.x no longer reads (one written before
    // `$OPENCODE_CONFIG_DIR` moved its root).
    for root in roots.iter().filter(|root| *root != v2_global) {
        remove_tui_v2(root, failed);
    }
    // OpenCode 2.x copies the 1.x half's `tui.json` entry into a `cli.json`
    // it creates when there is none, where that entry is dead.
    auto_remove_cli_entries(&cli_roots(xdg, v2_global), failed);
    (Some(binary_path.to_string()), pins)
}

/// Create the config root `root` for `file` the deck is about to write, when
/// the config guard allows it; whether it may go ahead. A refusal is added to
/// `failed` and logged.
fn ensure_root(root: &Path, file: &Path, failed: &mut Vec<String>) -> bool {
    match crate::config_write_guard::ensure_config_write_allowed(file)
        .and_then(|()| std::fs::create_dir_all(root))
    {
        Ok(()) => true,
        Err(e) => {
            failed.push(format!("{}: {e}", root.display()));
            tracing::warn!(
                "auto-install: could not create the OpenCode config root {} for its TUI \
                 plugin: {e}; deck messages will reach OpenCode by the doorbell instead",
                root.display()
            );
            false
        }
    }
}

/// PRD #2050: when `global` has no server module yet, copy the first pinned
/// module found in `legacy` there, so that a pin to another live install
/// which only a legacy copy held is judged by the usual pin rules
/// ([`auto_install_to_reporting_for`]) before that legacy copy is removed.
fn carry_module_over(global: &Path, legacy: &[PathBuf]) {
    let global_file = plugin_file(global);
    let _guard = lock_plugin();
    // Audit L2: read bounded and without blocking on a FIFO
    // ([`bounded_module_text`]), since this holds `PLUGIN_LOCK`, which every
    // other OpenCode install waits on.
    if std::fs::symlink_metadata(&global_file).is_err()
        && let Some(js) = legacy
            .iter()
            .find_map(|root| bounded_module_text(&plugin_file(root)))
            .filter(|js| binary_path_literal(js).is_some())
    {
        let dir = global.join("plugin");
        if let Err(e) = crate::config_write_guard::ensure_config_write_allowed(&global_file)
            .and_then(|()| std::fs::create_dir_all(&dir))
            .and_then(|()| {
                crate::agent_hook_config::write_atomic(&dir, &global_file, js.as_bytes())
            })
        {
            tracing::warn!(
                "auto-install: could not carry the OpenCode plugin over to {}: {e}",
                global_file.display()
            );
        }
    }
}

/// PRD #2050, review S3: the module in the existing global roots `global`
/// ([`global_roots`]), and the deck's own copy removed from the legacy
/// `~/.opencode` once OpenCode 2.x's global root `v2_global` holds the module.
/// OpenCode 2.x would refuse that copy as a duplicate for a project under
/// `$HOME`, and 1.x loads the XDG global root as well, so neither generation
/// needs it. With `create_v2_root` (a 2.x was detected), `v2_global` is
/// created first when missing. A legacy pin is carried over first
/// ([`carry_module_over`]). Only the deck's file names are removed
/// ([`remove_duplicate_module`]).
fn auto_install_global_resolved(
    roots: &[PathBuf],
    v2_global: &Path,
    global: &[PathBuf],
    create_v2_root: bool,
    failed: &mut Vec<String>,
    resolve: impl FnOnce() -> Result<String, String>,
) -> (Option<String>, Vec<(PathBuf, String)>) {
    let binary_path = match resolve() {
        Ok(binary_path) => binary_path,
        Err(e) => {
            tracing::warn!("auto-install: {e}");
            failed.push(e);
            return (None, Vec::new());
        }
    };
    if create_v2_root
        && let Err(e) =
            crate::config_write_guard::ensure_config_write_allowed(&plugin_file(v2_global))
                .and_then(|()| std::fs::create_dir_all(v2_global))
    {
        failed.push(format!("{}: {e}", plugin_file(v2_global).display()));
        tracing::warn!(
            "auto-install: could not create the OpenCode config root {}: {e}",
            v2_global.display()
        );
        return (Some(binary_path), Vec::new());
    }
    let legacy = roots.get(1).filter(|legacy| !global.contains(legacy));
    if let Some(legacy) = legacy {
        for root in global.iter().filter(|root| root.exists()) {
            carry_module_over(root, std::slice::from_ref(legacy));
        }
    }
    let pins = auto_install_to_reporting_for(global, &binary_path, failed);
    // Removed only once 2.x's global copy is in place, so a failed write never
    // leaves OpenCode with no module at all.
    if let Some(legacy) = legacy
        && pins.iter().any(|(root, _)| root == v2_global)
    {
        remove_duplicate_module(legacy, failed);
    }
    (Some(binary_path), pins)
}

/// PRD #2050: remove the deck's server module (and any obsolete nested
/// layout) from `root`, where OpenCode 2.x would load it as a second copy of
/// the global root's. Only the deck's own file names are touched.
fn remove_duplicate_module(root: &Path, failed: &mut Vec<String>) {
    let _guard = lock_plugin();
    for artifact in [plugin_file(root), stale_plugin_dir(root)] {
        if std::fs::symlink_metadata(&artifact).is_err() {
            continue;
        }
        let removed =
            crate::config_write_guard::ensure_config_write_allowed(&artifact).and_then(|()| {
                if artifact.is_dir() {
                    std::fs::remove_dir_all(&artifact)
                } else {
                    std::fs::remove_file(&artifact)
                }
            });
        match removed {
            Ok(()) => tracing::info!(
                "auto-install: removed {}: OpenCode 2 loads the deck's plugin from the global \
                 config root, and would refuse this copy as a duplicate",
                artifact.display()
            ),
            Err(e) => {
                failed.push(format!("{}: {e}", artifact.display()));
                tracing::warn!(
                    "auto-install: could not remove {}, which OpenCode 2 would load as a \
                     duplicate plugin: {e}",
                    artifact.display()
                );
            }
        }
    }
}

/// PRD #2050: write OpenCode 2.x's TUI plugin under `root` pinned to `pin`,
/// leaving a file that already holds those bytes untouched. Whether it
/// changed anything. Callers hold [`PLUGIN_LOCK`].
fn write_tui_v2(root: &Path, pin: &str) -> std::io::Result<bool> {
    let file = tui_v2_file(root);
    crate::config_write_guard::ensure_config_write_allowed(&file)?;
    let content = tui_v2_template(pin);
    if std::fs::read(&file).ok().as_deref() == Some(content.as_bytes()) {
        return Ok(false);
    }
    let dir = tui_v2_dir(root);
    std::fs::create_dir_all(&dir)?;
    crate::agent_hook_config::write_atomic(&dir, &file, content.as_bytes())?;
    crate::agent_hook_config::note_config_change();
    Ok(true)
}

/// PRD #2050: remove OpenCode 2.x's TUI half from `root`, for a machine
/// running 1.x. Only the deck's own directory is touched.
fn remove_tui_v2(root: &Path, failed: &mut Vec<String>) {
    let _guard = lock_plugin();
    let dir = tui_v2_dir(root);
    if std::fs::symlink_metadata(&dir).is_err() {
        return;
    }
    match crate::config_write_guard::ensure_config_write_allowed(&dir)
        .and_then(|()| std::fs::remove_dir_all(&dir))
    {
        Ok(()) => tracing::info!(
            "auto-install: removed the OpenCode 2 TUI plugin at {}: this OpenCode is 1.x",
            dir.display()
        ),
        Err(e) => {
            failed.push(format!("{}: {e}", dir.display()));
            tracing::warn!("auto-install: could not remove {}: {e}", dir.display());
        }
    }
}

/// PRD #2050: the automatic install's 2.x TUI half, its pin following the
/// same rules as the 1.x half's ([`auto_install_tui`]).
fn auto_install_tui_v2(root: &Path, binary_path: &str, failed: &mut Vec<String>) {
    let _guard = lock_plugin();
    let file = tui_v2_file(root);
    let takeover = crate::hook_binary::Takeover::current();
    let pin = match existing_pin(&file) {
        Some(existing) if crate::agent_hook_config::auto_install_keeps(&existing) => {
            if crate::agent_hook_config::executables_match(&existing, binary_path) {
                Some(existing)
            } else if takeover.supersedes(&existing) {
                Some(binary_path.to_string())
            } else if below_floor(&file) {
                Some(existing)
            } else {
                None
            }
        }
        _ => Some(binary_path.to_string()),
    };
    let Some(pin) = pin else {
        return;
    };
    match write_tui_v2(root, &pin) {
        Ok(true) => tracing::info!(
            "auto-install: installed the OpenCode 2 TUI plugin at {}",
            file.display()
        ),
        Ok(false) => {}
        Err(e) => {
            failed.push(format!("{}: {e}", file.display()));
            tracing::warn!(
                "auto-install: failed to install the OpenCode 2 TUI plugin at {}: {e}; deck \
                 messages will reach OpenCode by the doorbell instead",
                file.display()
            );
        }
    }
}

/// [`auto_install`] with the binary-path resolution injected, so the PRD #381 M6
/// refusal branch is reachable from a test.
///
/// On a refusal nothing is written and no directory is created — the plugin file
/// is never opened, so there is no truncated or abandoned JavaScript left for
/// OpenCode to load — and the complaint goes to `tracing::warn!` and nowhere
/// else, because this is the dashboard-startup path.
///
/// The resolver runs only once some root exists, matching
/// `hooks_manage::auto_install_to`'s directory check. Issue #1157 made this run
/// at every daemon start, including the packaged desktop's, where OpenCode is
/// often absent; resolving first logged `durable_binary_path`'s "pinning … as a
/// last resort" warning for a plugin that was then never written (Qodo on PR
/// #1344).
///
/// Returns the binary path it resolved (`None` when it installed nothing) and
/// the pin each existing root holds afterwards.
#[cfg(test)]
fn auto_install_resolved(
    roots: &[PathBuf],
    failed: &mut Vec<String>,
    resolve: impl FnOnce() -> Result<String, String>,
) -> (Option<String>, Vec<(PathBuf, String)>) {
    auto_install_resolved_for(roots, failed, resolve)
}

/// [`auto_install_resolved`]: the module in every existing root, for a
/// machine with no global root yet ([`Layout`]).
fn auto_install_resolved_for(
    roots: &[PathBuf],
    failed: &mut Vec<String>,
    resolve: impl FnOnce() -> Result<String, String>,
) -> (Option<String>, Vec<(PathBuf, String)>) {
    if !roots.iter().any(|root| root.exists()) {
        return (None, Vec::new());
    }
    match resolve() {
        Ok(binary_path) => {
            let pins = auto_install_to_reporting_for(roots, &binary_path, failed);
            (Some(binary_path), pins)
        }
        Err(e) => {
            tracing::warn!("auto-install: {e}");
            failed.push(e);
            (None, Vec::new())
        }
    }
}

/// `dot-agent-deck hooks install --agent opencode`. Unlike [`auto_install`],
/// this ALWAYS writes the freshly resolved path: the user asked for this
/// install by name, so an existing pin is not preserved (PRD #381 M5), and a
/// module of a later template revision is replaced too ([`newer_template`]).
///
/// PRD #2050: the same files as the automatic install ([`Layout`]): the
/// module in the existing global roots ([`global_roots`]), 2.x's created
/// first when the deck last saw an agent run OpenCode 2
/// ([`remembered_major_now`]), with the deck's own `~/.opencode` copy removed
/// once 2.x's global root holds it; with no global root, every existing root,
/// or the XDG default with no root at all; and both TUI halves. Every install removes the
/// deck's entry from OpenCode 2.x's `cli.json` ([`remove_cli_entry`]).
pub fn install() -> std::io::Result<()> {
    use crate::opencode_version::Major;
    let binary_path =
        crate::platform::paths::durable_binary_path().map_err(std::io::Error::other)?;
    let roots = candidate_roots();
    let root = xdg_default_root();
    let v2_global = v2_global_root();
    // As the automatic install for a detected 2.x does: 2.x's global root is
    // created when the deck last saw an agent run OpenCode 2.
    if remembered_major_now() == Some(Major::V2) {
        crate::config_write_guard::ensure_config_write_allowed(&plugin_file(&v2_global))?;
        std::fs::create_dir_all(&v2_global)?;
    }
    let global = global_roots(&roots, &v2_global);
    let targets = if global.iter().any(|root| root.exists()) {
        global
    } else {
        roots.clone()
    };
    install_to_roots_for(
        &targets,
        xdg_default_root,
        &binary_path,
        &mut std::io::stdout(),
    )?;
    // Review S3: OpenCode 2.x's global root now holds the module, so the
    // deck's own copy in `~/.opencode`, which 2.x would refuse as a
    // duplicate, goes; 1.x loads the XDG root as well.
    if plugin_file(&v2_global).exists()
        && let Some(legacy) = roots.get(1).filter(|legacy| !targets.contains(legacy))
    {
        for artifact in [plugin_file(legacy), stale_plugin_dir(legacy)] {
            if std::fs::symlink_metadata(&artifact).is_ok() {
                uninstall_impl(&artifact)?;
            }
        }
    }
    let _guard = lock_plugin();
    // PRD #2050: OpenCode 2.x's TUI half, a directory 1.x never sees.
    std::fs::create_dir_all(&v2_global)?;
    write_tui_v2(&v2_global, &binary_path)?;
    println!(
        "Installed OpenCode 2 TUI plugin: {}",
        tui_v2_file(&v2_global).display()
    );
    remove_cli_entries_printing(&cli_roots(&root, &v2_global))?;
    std::fs::create_dir_all(&root)?;
    // PRD #1666 S8: the TUI plugin, in the XDG root only (design §7.2).
    let tui_file = tui_plugin_file(&root);
    match install_tui_half(&root, Some(&binary_path))? {
        TuiEntry::Added(file) => {
            println!("Installed OpenCode TUI plugin: {}", tui_file.display());
            println!("Added the OpenCode TUI plugin to {}", file.display());
            Ok(())
        }
        TuiEntry::Present(file) => {
            println!("Installed OpenCode TUI plugin: {}", tui_file.display());
            println!("The OpenCode TUI plugin is listed in {}", file.display());
            Ok(())
        }
        TuiEntry::Refused(file, why) => {
            println!(
                "Left {} unchanged: {why}. Deck messages will reach OpenCode by the doorbell instead. \
                 To enable them, add {} to its \"plugin\" list.",
                file.display(),
                tui_entry_literal(&tui_file)
            );
            Err(std::io::Error::other(format!(
                "left {} unchanged: {why}",
                file.display()
            )))
        }
    }
}

pub fn uninstall() -> std::io::Result<()> {
    // PRD #2050: the deck's entry OpenCode 2.x copied into its `cli.json`.
    let roots = candidate_roots();
    remove_cli_entries_printing(&cli_roots(&xdg_default_root(), &v2_global_root()))?;
    // PRD #1666 S8: the TUI plugin's entry and file first, so a config that
    // cannot be edited safely stops the uninstall before anything is deleted.
    for root in roots {
        uninstall_tui_half(&root, &mut |line| println!("{line}"))?;
        // PRD #2050: OpenCode 2.x's TUI half, a directory of the deck's own.
        let dir = tui_v2_dir(&root);
        if std::fs::symlink_metadata(&dir).is_ok() {
            crate::config_write_guard::ensure_config_write_allowed(&dir)?;
            std::fs::remove_dir_all(&dir)?;
            println!("Removed OpenCode 2 TUI plugin: {}", dir.display());
        }
    }
    let artifacts = existing_plugin_artifacts();
    if artifacts.is_empty() {
        println!("No OpenCode plugin found to remove.");
        return Ok(());
    }
    for artifact in &artifacts {
        uninstall_impl(artifact)?;
    }
    Ok(())
}

// --- Testable versions that accept a custom path ---

pub fn uninstall_from(path: &PathBuf) -> std::io::Result<()> {
    uninstall_impl(path)
}

#[cfg(all(test, unix))]
#[path = "opencode_injected_turn_tests.rs"]
mod injected_turn_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Scenario (issue #1637): the daemon's pin refresh reads back the
    /// `BINARY_PATH` each root's plugin pins, one pin per binary. No root at
    /// all, or a plugin with no readable `BINARY_PATH`, is no evidence; a root
    /// with no plugin names nothing, so roots without any name no pin.
    #[test]
    fn configured_pins_reads_each_plugin_back() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let roots = vec![fixture.path().join("xdg"), fixture.path().join("legacy")];
        assert_eq!(configured_pins_in(&roots), None, "no root, as at startup");
        std::fs::create_dir_all(&roots[0]).unwrap();
        assert_eq!(configured_pins_in(&roots), Some(Vec::new()));
        let old = crate::test_paths::abs("/opt/old/dot-agent-deck");
        let old = old.as_str();
        for root in &roots {
            std::fs::create_dir_all(plugin_file(root).parent().unwrap()).unwrap();
        }
        assert_eq!(configured_pins_in(&roots), Some(Vec::new()));
        std::fs::write(plugin_file(&roots[0]), plugin_template(old)).unwrap();
        let pins = configured_pins_in(&roots).expect("one plugin read");
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].binary, old);
        std::fs::write(plugin_file(&roots[1]), plugin_template(old)).unwrap();
        assert_eq!(
            configured_pins_in(&roots).expect("two plugins read").len(),
            1
        );
        std::fs::write(plugin_file(&roots[1]), b"// hand-edited\n").unwrap();
        assert_eq!(configured_pins_in(&roots), None);
    }

    /// Scenario (Qodo on #1656): the OpenCode plugin pins an older copy,
    /// which raised its notice, and then `hooks uninstall --agent opencode`
    /// removes the plugin. The next refresh reads the roots as naming
    /// nothing, so the notice clears within one refresh.
    #[test]
    fn uninstalling_the_plugin_clears_the_notice_within_one_refresh() {
        use crate::hook_binary::{DeckIdentity, HookBinaryState, PinRefresh};
        let fixture = crate::test_temp::tempdir().unwrap();
        let roots = vec![fixture.path().join("xdg"), fixture.path().join("legacy")];
        std::fs::create_dir_all(plugin_file(&roots[0]).parent().unwrap()).unwrap();
        let old = fixture.path().join("missing-old").join("dot-agent-deck");
        std::fs::write(
            plugin_file(&roots[0]),
            plugin_template(&old.to_string_lossy()),
        )
        .unwrap();
        let deck = DeckIdentity {
            exe: None,
            version: "0.46.0".into(),
            self_install_path: None,
        };
        let pins = configured_pins_in(&roots).expect("one plugin read");
        // `old` does not exist, so its probe fails: an `Unprobeable` notice.
        let mut state = HookBinaryState::from_startup(deck.clone(), &pins, None);
        assert_eq!(state.notices().len(), 1);

        uninstall_from(&plugin_file(&roots[0])).unwrap();
        let read = configured_pins_in(&roots).expect("the roots were read");
        assert!(read.is_empty());
        let refresh = PinRefresh::classify(&deck, vec![(crate::event::AgentType::OpenCode, read)]);
        assert!(state.apply_refresh(refresh));
        assert!(state.notices().is_empty(), "{:?}", state.notices());
    }

    /// Scenario (issue #1637 audit A9): one root's plugin file is replaced by
    /// a FIFO with no writer. The read-back returns at once with no evidence
    /// rather than waiting for a writer, as it does for a plugin past the size
    /// cap.
    #[cfg(unix)]
    #[test]
    fn configured_pins_neither_blocks_on_a_fifo_nor_reads_an_oversized_plugin() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let roots = vec![fixture.path().join("xdg"), fixture.path().join("legacy")];
        for root in &roots {
            std::fs::create_dir_all(plugin_file(root).parent().unwrap()).unwrap();
        }
        std::fs::write(
            plugin_file(&roots[0]),
            plugin_template("/opt/old/dot-agent-deck"),
        )
        .unwrap();
        let fifo =
            std::ffi::CString::new(plugin_file(&roots[1]).into_os_string().into_encoded_bytes())
                .unwrap();
        // SAFETY: a NUL-terminated path; `mkfifo` only creates the node.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let read = {
            let roots = roots.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(configured_pins_in(&roots));
            });
            rx.recv_timeout(std::time::Duration::from_secs(10))
                .expect("the read blocked on the FIFO")
        };
        assert_eq!(read, None);

        std::fs::remove_file(plugin_file(&roots[1])).unwrap();
        let mut huge = plugin_template("/opt/old/dot-agent-deck").into_bytes();
        huge.resize(
            crate::agent_hook_config::MAX_HOOK_CONFIG_BYTES as usize + 1,
            b'\n',
        );
        std::fs::write(plugin_file(&roots[1]), huge).unwrap();
        assert_eq!(configured_pins_in(&roots), None);
    }
    #[cfg(unix)]
    use spec::spec;

    #[cfg(unix)]
    pub(crate) fn config_test_install(root: &Path) -> std::io::Result<()> {
        install_to_roots(
            &[],
            || root.to_path_buf(),
            "/opt/dot-agent-deck",
            &mut Vec::new(),
        )
    }

    // PRD #1487 ruling: an automatic install keeps a plugin pinned to another
    // live, durable install as it is; explicit install still repins it.

    #[cfg(unix)]
    fn seed_deck(path: &Path) -> String {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::test_isolation::write_script(path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            path,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    #[cfg(unix)]
    fn plugin_fingerprint(path: &Path) -> (Vec<u8>, u64, i64, i64) {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    }

    #[cfg(unix)]
    fn age_plugin(path: &Path) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(123456)),
            )
            .unwrap();
    }

    /// Scenario: An OpenCode plugin pins live install A with another version's template. An automatic install from B leaves it byte for byte, inode and mtime included.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_opencode_keeps_a_live_install() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let root = fixture.path().join("fake-operator-home").join("opencode");
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        let path = write_plugin(&root, &a).unwrap();
        // Another version's template: the bytes differ from what B would write.
        let mut older = std::fs::read(&path).unwrap();
        older.extend_from_slice(b"// written by another deck version\n");
        std::fs::write(&path, older).unwrap();
        age_plugin(&path);
        let before = plugin_fingerprint(&path);

        auto_install_to(std::slice::from_ref(&root), &b);

        assert_eq!(plugin_fingerprint(&path), before);
        assert_eq!(existing_binary_path(&root).as_deref(), Some(a.as_str()));
    }

    /// Scenario (PRD #1666 §11): the OpenCode plugin pins live install A with a template from before the integration floor, and copy B's automatic install runs as an OpenCode agent starts. A's pin is kept but the template is regenerated, so the agent loads the current integration; the TUI plugin, likewise pinned to A and stale, is regenerated with A's pin too. With the plugin directory unwritable, the same install reports the file it could not write.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_opencode_refreshes_a_stale_template_keeping_a_live_pin() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = crate::test_temp::tempdir().unwrap();
        let root = fixture.path().join("fake-operator-home").join("opencode");
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        let stale = |file: &Path, current: String| {
            let floor = format!(
                "const INBOX_ADAPTER_INTEGRATION = {};",
                crate::inbox_hub::ADAPTER_INTEGRATION
            );
            assert!(current.contains(&floor), "the template declares the floor");
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(
                file,
                current.replace(&floor, "const INBOX_ADAPTER_INTEGRATION = 1;"),
            )
            .unwrap();
            assert!(below_floor(file));
        };
        stale(&plugin_file(&root), plugin_template(&a));
        stale(&tui_plugin_file(&root), tui_plugin_template(&a));

        let mut failed = Vec::new();
        let pins = auto_install_to_reporting(std::slice::from_ref(&root), &b, &mut failed);
        auto_install_tui(&root, &b, &mut failed);

        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(pins, vec![(root.clone(), a.clone())], "A stays pinned");
        assert_eq!(
            std::fs::read_to_string(plugin_file(&root)).unwrap(),
            plugin_template(&a),
            "the server plugin is A's pin in the current template"
        );
        assert_eq!(
            std::fs::read_to_string(tui_plugin_file(&root)).unwrap(),
            tui_plugin_template(&a),
            "and so is the TUI plugin"
        );

        stale(&plugin_file(&root), plugin_template(&a));
        let dir = plugin_file(&root).parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let mut failed = Vec::new();
        auto_install_to_reporting(std::slice::from_ref(&root), &b, &mut failed);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // SAFETY: geteuid has no preconditions. Permissions do not bind for
        // root, which can write the directory anyway.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert!(
                failed[0].contains("dot-agent-deck.js"),
                "the failure names the file: {failed:?}"
            );
        }
    }

    /// Scenario: an automatic install whose plugin write fails reports no pin, so the daemon never announces a binary the plugin does not hold (issue #1637).
    #[cfg(unix)]
    #[test]
    fn config_auto_install_opencode_reports_no_pin_when_the_write_fails() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let root = fixture.path().join("fake-operator-home").join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        // A file where the plugin directory goes: the write cannot succeed.
        std::fs::write(root.join("plugin"), b"not a directory").unwrap();
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));

        let pins = auto_install_to(std::slice::from_ref(&root), &b);

        assert!(pins.is_empty(), "{pins:?}");
        assert_eq!(existing_binary_path(&root), None);
    }

    /// Scenario: the OpenCode plugin pins live install A. A newer, eligible copy B's automatic install repins it to B (issue #1637) and reports B as the pin; a tie, a newer A, an A whose version cannot be read, and an ineligible B all keep A and report A.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_opencode_takeover_matrix() {
        use crate::hook_binary::{Takeover, with_takeover};
        for (pinned, takeover, replaced) in [
            (Some("0.0.1"), Takeover::eligible("1.0.0"), true),
            (Some("1.0.0"), Takeover::eligible("1.0.0"), false),
            (Some("2.0.0"), Takeover::eligible("1.0.0"), false),
            (None, Takeover::eligible("1.0.0"), false),
            (Some("0.0.1"), Takeover::ineligible(), false),
        ] {
            let fixture = crate::test_temp::tempdir().unwrap();
            let root = fixture.path().join("fake-operator-home").join("opencode");
            let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
            if let Some(version) = pinned {
                crate::test_isolation::write_script(
                    std::path::Path::new(&a),
                    format!("#!/bin/sh\necho 'dot-agent-deck {version}'\n").as_bytes(),
                )
                .unwrap();
            }
            let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
            write_plugin(&root, &a).unwrap();

            let pins = with_takeover(takeover, || {
                auto_install_to(std::slice::from_ref(&root), &b)
            });

            let expected = if replaced { &b } else { &a };
            assert_eq!(
                existing_binary_path(&root).as_deref(),
                Some(expected.as_str()),
                "pinned {pinned:?}"
            );
            assert_eq!(
                pins,
                vec![(root.clone(), expected.clone())],
                "pinned {pinned:?}"
            );
        }
    }

    /// Scenario: An OpenCode plugin pins install A, which is then deleted or is cargo build output. An automatic install from B repins it to B.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_opencode_replaces_a_dead_or_build_install() {
        for unusable in ["dead", "cargo-output"] {
            let fixture = crate::test_temp::tempdir().unwrap();
            let root = fixture.path().join("fake-operator-home").join("opencode");
            let a = if unusable == "dead" {
                seed_deck(&fixture.path().join("pruned").join("dot-agent-deck"))
            } else {
                let profile = fixture.path().join("custom-target").join("debug");
                std::fs::create_dir_all(profile.join(".fingerprint")).unwrap();
                std::fs::create_dir_all(profile.join("deps")).unwrap();
                seed_deck(&profile.join("dot-agent-deck"))
            };
            let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
            write_plugin(&root, &a).unwrap();
            if unusable == "dead" {
                std::fs::remove_file(&a).unwrap();
            }

            auto_install_to(std::slice::from_ref(&root), &b);

            assert_eq!(
                existing_binary_path(&root).as_deref(),
                Some(b.as_str()),
                "a {unusable} pin is repinned to B"
            );
        }
    }

    /// Scenario: An OpenCode plugin pins live install A. The explicit install from B repins it to B.
    #[cfg(unix)]
    #[test]
    fn config_explicit_install_opencode_replaces_a_live_install() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let root = fixture.path().join("fake-operator-home").join("opencode");
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        write_plugin(&root, &a).unwrap();

        install_to_roots(
            std::slice::from_ref(&root),
            || unreachable!(),
            &b,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(existing_binary_path(&root).as_deref(), Some(b.as_str()));
    }

    /// Scenario: Alternate automatic OpenCode installs from A, B, A, B into a root with no plugin. Only the first writes; the plugin then keeps its bytes, inode and mtime and pins A.
    #[cfg(unix)]
    #[test]
    fn config_alternating_auto_installs_opencode_write_once() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let root = fixture.path().join("fake-operator-home").join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));

        auto_install_to(std::slice::from_ref(&root), &a);
        let path = plugin_file(&root);
        age_plugin(&path);
        let first = plugin_fingerprint(&path);
        for (start, binary) in [&b, &a, &b].into_iter().enumerate() {
            auto_install_to(std::slice::from_ref(&root), binary);
            assert_eq!(
                plugin_fingerprint(&path),
                first,
                "automatic start {} (A, B, A, B) rewrote the plugin",
                start + 2
            );
        }
        assert_eq!(existing_binary_path(&root).as_deref(), Some(a.as_str()));
    }

    /// Scenario: Install the same OpenCode plugin explicitly a second time. Its bytes, inode and mtime stay unchanged, and repeated removal leaves the missing file absent.
    #[cfg(unix)]
    #[test]
    fn config_no_op_opencode_explicit_install_preserves_file() {
        check_no_op_opencode_install(false);
    }

    /// Scenario: Automatically install an already current OpenCode plugin. The file keeps its bytes, inode and mtime.
    #[cfg(unix)]
    #[test]
    fn config_no_op_opencode_automatic_install_preserves_file() {
        check_no_op_opencode_install(true);
    }

    #[cfg(unix)]
    fn check_no_op_opencode_install(automatic: bool) {
        use std::os::unix::fs::MetadataExt;
        let fixture = crate::test_temp::tempdir().unwrap();
        let root = fixture.path().join("fake-operator-home/.config/opencode");
        let binary = fixture.path().join("installed/dot-agent-deck");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        crate::test_isolation::write_script(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            &binary,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let binary = binary.to_str().unwrap();
        let path = write_plugin(&root, binary).unwrap();
        {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(123456),
                    ),
                )
                .unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let metadata = std::fs::metadata(&path).unwrap();
            if automatic {
                auto_install_to(std::slice::from_ref(&root), binary);
            } else {
                install_to_roots(
                    std::slice::from_ref(&root),
                    || unreachable!(),
                    binary,
                    &mut Vec::new(),
                )
                .unwrap();
            }
            let after = std::fs::metadata(&path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(
                (after.ino(), after.mtime(), after.mtime_nsec()),
                (metadata.ino(), metadata.mtime(), metadata.mtime_nsec()),
                "OpenCode no-op install must preserve inode and mtime (automatic={automatic})"
            );
        }
        uninstall_from(&path).unwrap();
        let parent_before = std::fs::metadata(path.parent().unwrap()).unwrap();
        uninstall_from(&path).unwrap();
        let parent_after = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(
            (
                parent_after.ino(),
                parent_after.mtime(),
                parent_after.mtime_nsec()
            ),
            (
                parent_before.ino(),
                parent_before.mtime(),
                parent_before.mtime_nsec()
            )
        );
        assert!(!path.exists());
    }

    #[test]
    fn plugin_template_uses_exec_file_sync() {
        let content = plugin_template("/usr/local/bin/dot-agent-deck");
        assert!(
            content.contains("import { execFile, execFileSync, spawn } from \"child_process\";")
        );
        assert!(!content.contains("execSync("));
        assert!(content.contains(r#"BINARY_PATH = "/usr/local/bin/dot-agent-deck""#));
        assert!(content.contains("const knownSessions = new Map();"));
        assert!(content.contains("process.once(\"exit\", handleShutdown);"));
        assert!(content.contains(r#"["hook", "--agent", "opencode"]"#));
        assert!(content.contains("event?.type?.startsWith(\"session.\")"));
        assert!(content.contains("\"tool.execute.before\""));
        assert!(content.contains("eventType === \"message.created\""));
        assert!(content.contains("eventType === \"message.updated\""));
        assert!(content.contains("const permissionPayload"));
        assert!(content.contains("\"permission.asked\""));
        // PR #1698 review: both tool hooks, root and child session, forward the
        // call's id, which a `permission.asked` names as `tool.callID`
        // (PRD #2050: through the one report both generations share).
        assert_eq!(content.matches("call_id: callID,").count(), 2);
        assert_eq!(content.matches("input?.callID, directory)").count(), 2);
    }

    /// The installer intentionally fans out to every existing OpenCode config
    /// root, so when both `$XDG_CONFIG_HOME/opencode` and `~/.opencode` exist
    /// OpenCode loads two copies of the plugin into one process and every hook
    /// fires twice (observed as doubled `Received event` lines in the daemon
    /// log). The template must therefore carry a process-wide duplicate-load
    /// guard whose second copy returns inert hooks.
    #[test]
    fn plugin_template_suppresses_a_duplicate_load() {
        let content = plugin_template("/usr/local/bin/dot-agent-deck");
        // The flag is read BEFORE it is set, so the first copy loaded wins.
        let read_at = content
            .find("const DAD_ALREADY_LOADED = globalThis[DAD_GUARD] === true;")
            .expect("guard must snapshot the flag");
        let set_at = content
            .find("globalThis[DAD_GUARD] = true;")
            .expect("guard must claim the flag");
        assert!(
            read_at < set_at,
            "the flag must be READ before being SET, or the first copy would \
             see its own claim and disable itself"
        );
        // A duplicate returns hooks that do nothing.
        assert!(
            content.contains("if (DAD_ALREADY_LOADED) {"),
            "the plugin factory must short-circuit on a duplicate load"
        );
        // The guard must sit on globalThis: two copies are separate ES modules,
        // so module-local state would not be shared between them.
        assert!(
            content.contains(r#"const DAD_GUARD = "__dotAgentDeckPluginLoaded";"#),
            "the guard must be keyed on globalThis, not module scope"
        );
    }

    /// The plugin must be a FLAT `.js` file directly under `plugin/` — that is the
    /// only layout OpenCode's one-level `{plugin,plugins}/*.{ts,js}` glob discovers.
    /// The obsolete nested dir sits a level deeper and is never scanned.
    #[test]
    fn plugin_file_is_flat_under_plugin_dir() {
        let root = PathBuf::from("/some/opencode");
        assert_eq!(
            plugin_file(&root),
            PathBuf::from("/some/opencode/plugin/dot-agent-deck.js")
        );
        assert_eq!(
            stale_plugin_dir(&root),
            PathBuf::from("/some/opencode/plugin/dot-agent-deck")
        );
    }

    /// Read the installed plugin under `root` and assert its `BINARY_PATH` matches.
    fn assert_plugin_binary(root: &Path, binary_path: &str) {
        let file = plugin_file(root);
        let content = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("expected plugin at {}: {e}", file.display()));
        assert!(
            content.contains(&format!(r#"BINARY_PATH = "{binary_path}""#)),
            "plugin at {} should pin BINARY_PATH = {binary_path:?}",
            file.display()
        );
    }

    #[test]
    fn auto_install_writes_to_both_existing_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-both");

        assert_plugin_binary(&xdg_root, "/bin/deck-both");
        assert_plugin_binary(&legacy_root, "/bin/deck-both");
    }

    #[test]
    fn auto_install_only_legacy_present_skips_xdg() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode"); // NOT created
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&legacy_root).unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-legacy");

        assert_plugin_binary(&legacy_root, "/bin/deck-legacy");
        assert!(!xdg_root.exists(), "absent XDG root must not be created");
        assert!(!plugin_file(&xdg_root).exists());
    }

    #[test]
    fn auto_install_only_xdg_present_skips_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode"); // NOT created
        std::fs::create_dir_all(&xdg_root).unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-xdg");

        assert_plugin_binary(&xdg_root, "/bin/deck-xdg");
        assert!(
            !legacy_root.exists(),
            "absent legacy root must not be created"
        );
        assert!(!plugin_file(&legacy_root).exists());
    }

    #[test]
    fn auto_install_neither_present_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        // Neither root created.

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-none");

        assert!(!xdg_root.exists());
        assert!(!legacy_root.exists());
        assert!(!plugin_file(&xdg_root).exists());
        assert!(!plugin_file(&legacy_root).exists());
    }

    /// Note what makes this pass since PRD #381: `/bin/deck-old` does not
    /// exist, so the second pass REPAIRS a dead pin rather than overwriting a
    /// live one. The auto path deliberately no longer clobbers a `BINARY_PATH`
    /// that still resolves — see
    /// `auto_install_repairs_a_dead_binary_path_and_preserves_a_valid_one`,
    /// which pins both halves.
    #[test]
    fn auto_install_idempotent_overwrites_every_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        let roots = [xdg_root.clone(), legacy_root.clone()];

        auto_install_to(&roots, "/bin/deck-old");
        auto_install_to(&roots, "/bin/deck-new");

        for root in [&xdg_root, &legacy_root] {
            let content = std::fs::read_to_string(plugin_file(root)).unwrap();
            assert!(content.contains(r#"BINARY_PATH = "/bin/deck-new""#));
            assert!(!content.contains(r#"BINARY_PATH = "/bin/deck-old""#));
        }
    }

    #[test]
    fn auto_install_one_layout_failure_still_writes_other() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        // Block the XDG write: a regular file where `plugin/` must be a dir makes
        // `create_dir_all` fail for the XDG target only.
        std::fs::write(xdg_root.join("plugin"), b"not a dir").unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-resil");

        assert_plugin_binary(&legacy_root, "/bin/deck-resil");
        assert!(!plugin_file(&xdg_root).exists());
    }

    #[test]
    fn install_fan_out_writes_every_existing_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        let fallback_root = tmp.path().join("fallback").join("opencode");

        let mut out = Vec::new();
        install_to_roots(
            &[xdg_root.clone(), legacy_root.clone()],
            || fallback_root.clone(),
            "/bin/deck-install",
            &mut out,
        )
        .unwrap();

        assert_plugin_binary(&xdg_root, "/bin/deck-install");
        assert_plugin_binary(&legacy_root, "/bin/deck-install");
        // Fallback NOT used because at least one layout existed.
        assert!(!plugin_file(&fallback_root).exists());

        // Stdout names every written path, one line per layout.
        let stdout = String::from_utf8(out).unwrap();
        let lines = stdout
            .lines()
            .filter(|l| l.starts_with("Installed OpenCode plugin:"))
            .count();
        assert_eq!(lines, 2, "one line per written layout, got: {stdout:?}");
        let xdg_index = plugin_file(&xdg_root).display().to_string();
        let legacy_index = plugin_file(&legacy_root).display().to_string();
        assert!(stdout.contains(xdg_index.as_str()));
        assert!(stdout.contains(legacy_index.as_str()));
    }

    #[test]
    fn install_falls_back_to_xdg_default_when_no_layout_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode"); // absent
        let legacy_root = tmp.path().join(".opencode"); // absent
        let fallback_root = tmp.path().join(".config").join("opencode");

        let mut out = Vec::new();
        install_to_roots(
            &[xdg_root.clone(), legacy_root.clone()],
            || fallback_root.clone(),
            "/bin/deck-fallback",
            &mut out,
        )
        .unwrap();

        let content = std::fs::read_to_string(plugin_file(&fallback_root)).unwrap();
        assert!(content.contains(r#"BINARY_PATH = "/bin/deck-fallback""#));

        let stdout = String::from_utf8(out).unwrap();
        let lines = stdout
            .lines()
            .filter(|l| l.starts_with("Installed OpenCode plugin:"))
            .count();
        assert_eq!(lines, 1);
        let fallback_index = plugin_file(&fallback_root).display().to_string();
        assert!(stdout.contains(fallback_index.as_str()));
    }

    #[test]
    fn install_one_layout_failure_still_writes_other_and_surfaces_error() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        std::fs::write(xdg_root.join("plugin"), b"not a dir").unwrap(); // block XDG

        let fallback_root = tmp.path().join("fallback");
        let mut out = Vec::new();
        let result = install_to_roots(
            &[xdg_root.clone(), legacy_root.clone()],
            || fallback_root.clone(),
            "/bin/deck-resil2",
            &mut out,
        );

        assert!(
            result.is_err(),
            "a failed layout must surface as an io::Result error"
        );
        // The other layout is still written despite the failure.
        assert_plugin_binary(&legacy_root, "/bin/deck-resil2");
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("Installed OpenCode plugin:"));
    }

    /// Regression (the "OpenCode shows No agent" bug): a user upgrading from the
    /// old nested layout has `plugin/dot-agent-deck/index.js` on disk, which
    /// OpenCode's one-level glob never scanned. Installing must (a) write the flat
    /// `plugin/dot-agent-deck.js` OpenCode DOES discover, and (b) delete the dead
    /// nested dir so no stale copy lingers — converging every user with no manual
    /// step.
    #[test]
    fn install_migrates_stale_nested_layout_to_flat_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".config").join("opencode");
        // Seed the obsolete nested layout as an upgrade would leave it.
        let stale_dir = stale_plugin_dir(&root);
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("index.js"), b"// old nested plugin").unwrap();

        auto_install_to(std::slice::from_ref(&root), "/bin/deck-migrated");

        // Flat, discoverable file is present and current.
        assert_plugin_binary(&root, "/bin/deck-migrated");
        assert!(
            plugin_file(&root).is_file(),
            "flat plugin/dot-agent-deck.js must exist after install"
        );
        // The dead nested dir is gone.
        assert!(
            !stale_dir.exists(),
            "stale nested plugin/dot-agent-deck/ dir must be removed on install"
        );
    }

    /// Uninstall must clear BOTH the current flat file and any leftover nested dir,
    /// so a machine that was never re-installed after the migration still ends up
    /// clean.
    #[test]
    fn uninstall_impl_removes_flat_file_and_nested_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".config").join("opencode");

        // A flat file...
        let file = plugin_file(&root);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"// flat plugin").unwrap();
        uninstall_impl(&file).unwrap();
        assert!(!file.exists(), "flat plugin file must be removed");

        // ...and, independently, a stale nested dir.
        let stale_dir = stale_plugin_dir(&root);
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("index.js"), b"// old").unwrap();
        uninstall_impl(&stale_dir).unwrap();
        assert!(
            !stale_dir.exists(),
            "stale nested plugin dir must be removed"
        );
    }

    // -----------------------------------------------------------------------
    // PRD #381 — the OpenCode plugin's `BINARY_PATH`.
    //
    // Its own tests, and its own parse, because the plugin is generated
    // JAVASCRIPT rather than JSON: the PRD blames exactly that shape difference
    // for OpenCode being the last of the three integrations to be noticed, so a
    // fix that only speaks JSON silently misses it.
    // -----------------------------------------------------------------------

    /// A real executable at `path`, parents created — the resolver's "exists and
    /// is executable" gate is a real `stat`, so this cannot be faked.
    fn write_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("path has a parent")).expect("create parent");
        crate::test_isolation::write_script(path, b"#!/bin/sh\nexit 0\n")
            .expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    /// The `…/target/release/dot-agent-deck` the field defect wrote, plus the
    /// durable `<home>/.local/bin/dot-agent-deck` the resolver must prefer.
    fn artifact_and_durable(root: &Path) -> (PathBuf, PathBuf) {
        // The crate name plus the platform's executable suffix, which is the
        // file name the resolver searches `~/.local/bin` for. Seeding the bare
        // name left the durable candidate invisible on Windows, so the resolver
        // refused and both tests below failed on `build-windows` (PR #733).
        let name = format!(
            "{}{}",
            crate::platform::paths::DEFAULT_BINARY_NAME,
            std::env::consts::EXE_SUFFIX
        );
        let artifact = root
            .join("checkout")
            .join("target")
            .join("release")
            .join(&name);
        write_executable(&artifact);
        let durable = root.join("home").join(".local").join("bin").join(&name);
        write_executable(&durable);
        (artifact, durable)
    }

    /// Driving the plugin writer with the REAL resolver and a build-artifact
    /// `current_exe()`: `BINARY_PATH` must be the durable path, and no
    /// `target/` path may appear anywhere in the generated JavaScript.
    #[test]
    fn plugin_binary_path_is_never_the_build_artifact_it_was_installed_from() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let (artifact, durable) = artifact_and_durable(tmp.path());
        let root = tmp.path().join("opencode-root");
        std::fs::create_dir_all(&root).expect("create root");

        let resolved = crate::platform::paths::durable_binary_path_with(
            Ok(artifact.clone()),
            &tmp.path().join("home"),
            None,
        )
        .expect("a seeded ~/.local/bin candidate must resolve");
        auto_install_to(std::slice::from_ref(&root), &resolved);

        let js = std::fs::read_to_string(plugin_file(&root)).expect("read plugin");
        assert_eq!(
            existing_binary_path(&root).as_deref(),
            durable.to_str(),
            "BINARY_PATH must be the durable path, not the artifact"
        );
        // Both separator spellings, and the Windows one as the JS literal
        // spells it: `BINARY_PATH` is written through `serde_json`, so
        // `target\release` is escaped to `target\\release` and a
        // single-separator needle would pass vacuously there.
        for marker in [
            "target/release",
            "target/debug",
            r"target\\release",
            r"target\\debug",
        ] {
            assert!(
                !js.contains(marker),
                "the generated plugin names `{marker}`:\n{js}"
            );
        }
    }

    /// PRD #381 M5: on the AUTO path a `BINARY_PATH` pointing at a deleted file
    /// is repaired, and a still-valid one is left exactly as it was.
    ///
    /// This path used to rewrite `BINARY_PATH` unconditionally on every
    /// dashboard startup, which is the "repair what merely differs" behaviour
    /// Open Question 3 rules out — and on the one integration nobody was
    /// watching.
    #[test]
    fn auto_install_repairs_a_dead_binary_path_and_preserves_a_valid_one() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let (artifact, durable) = artifact_and_durable(tmp.path());
        let home = tmp.path().join("home");
        let resolved =
            crate::platform::paths::durable_binary_path_with(Ok(artifact.clone()), &home, None)
                .expect("resolve durable");

        // A pin that still exists: preserved, byte for byte in the value.
        let valid_root = tmp.path().join("valid-root");
        std::fs::create_dir_all(&valid_root).expect("create root");
        let users_own = tmp.path().join("users-own").join("dot-agent-deck");
        write_executable(&users_own);
        write_plugin(&valid_root, users_own.to_str().expect("UTF-8")).expect("seed valid plugin");

        auto_install_to(std::slice::from_ref(&valid_root), &resolved);
        assert_eq!(
            existing_binary_path(&valid_root).as_deref(),
            users_own.to_str(),
            "a BINARY_PATH that still exists must not be repointed just because it differs"
        );

        // A pin that is positively gone: repaired to the resolved path.
        let dead_root = tmp.path().join("dead-root");
        std::fs::create_dir_all(&dead_root).expect("create root");
        let gone = tmp.path().join("pruned-worktree").join("dot-agent-deck");
        assert!(!gone.exists(), "the dead pin must genuinely not exist");
        write_plugin(&dead_root, gone.to_str().expect("UTF-8")).expect("seed dead plugin");

        auto_install_to(std::slice::from_ref(&dead_root), &resolved);
        assert_eq!(
            existing_binary_path(&dead_root).as_deref(),
            durable.to_str(),
            "a BINARY_PATH whose target is gone must be repaired"
        );

        // Idempotent: a second pass over either root changes nothing.
        let before = std::fs::read(plugin_file(&dead_root)).expect("read repaired plugin");
        auto_install_to(std::slice::from_ref(&dead_root), &resolved);
        assert_eq!(
            before,
            std::fs::read(plugin_file(&dead_root)).expect("read again"),
            "a second auto-install pass changed the repaired plugin"
        );
    }

    /// Scenario: an OpenCode plugin pins an absolute install inside a directory
    /// the deck cannot search, so whether that install exists cannot be read.
    /// The startup auto-install does not keep a pin it cannot establish is
    /// live: it repoints the plugin at the installing binary, as the Claude,
    /// Codex and Devin automatic installs do (PRD #1487 re-check R3). Skipped
    /// where permissions do not bind, e.g. as root.
    #[cfg(unix)]
    #[test]
    fn auto_install_replaces_a_pin_whose_existence_cannot_be_read() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let (artifact, durable) = artifact_and_durable(tmp.path());
        let home = tmp.path().join("home");
        let resolved =
            crate::platform::paths::durable_binary_path_with(Ok(artifact.clone()), &home, None)
                .expect("resolve durable");

        let locked = tmp.path().join("locked");
        let hidden = locked.join("bin").join("dot-agent-deck");
        write_executable(&hidden);
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).expect("create root");
        write_plugin(&root, hidden.to_str().expect("UTF-8")).expect("seed plugin");

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let undeterminable = hidden.try_exists().is_err();
        if undeterminable {
            auto_install_to(std::slice::from_ref(&root), &resolved);
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !undeterminable {
            eprintln!("SKIP: permissions do not hide the pin on this host");
            return;
        }
        assert_eq!(
            existing_binary_path(&root).as_deref(),
            durable.to_str(),
            "a pin whose existence cannot be read must be replaced by the installing binary"
        );
    }

    /// PRD #381 M6: a refusal writes nothing at all — not a truncated plugin,
    /// not even the `plugin/` directory. An abandoned half-written file is
    /// worse than none: OpenCode would load it.
    #[test]
    fn auto_install_refusal_leaves_no_plugin_behind() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("opencode-root");
        std::fs::create_dir_all(&root).expect("create root");

        auto_install_resolved(std::slice::from_ref(&root), &mut Vec::new(), || {
            Err("no durable dot-agent-deck".to_string())
        });

        assert!(
            !plugin_file(&root).exists(),
            "a refused auto-install wrote {}",
            plugin_file(&root).display()
        );
        assert!(
            !root.join("plugin").exists(),
            "a refused auto-install created the plugin directory"
        );
    }

    /// Issue #1157: with no OpenCode root on disk the resolver is never asked,
    /// so a machine without OpenCode logs no "pinning … as a last resort"
    /// warning for a plugin nobody will write — and nothing is created.
    #[test]
    fn auto_install_does_not_resolve_a_binary_when_no_root_exists() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let absent = tmp.path().join("no-opencode-here");
        let asked = std::cell::Cell::new(false);

        auto_install_resolved(std::slice::from_ref(&absent), &mut Vec::new(), || {
            asked.set(true);
            Ok("/opt/dot-agent-deck".to_string())
        });

        assert!(
            !asked.get(),
            "the binary path was resolved although no OpenCode root exists"
        );
        assert!(!absent.exists(), "a skipped auto-install created its root");
    }

    /// The JS parse is the one that has to survive a hand-edited or truncated
    /// file: anything it cannot read confidently is `None`, which
    /// [`auto_install_to`] treats as "no pin to preserve" and overwrites.
    #[test]
    fn existing_binary_path_reads_the_generated_literal_and_nothing_else() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        write_plugin(&root, "/with space/dot-agent-deck").expect("write plugin");
        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some("/with space/dot-agent-deck"),
            "a quoted path must round-trip through the JS literal"
        );

        let truncated = tmp.path().join("truncated");
        let file = plugin_file(&truncated);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("create dir");
        std::fs::write(&file, b"const BINARY_PATH = \"unterminated").expect("write truncated");
        assert_eq!(
            existing_binary_path(&truncated),
            None,
            "a truncated plugin must not be read as a valid pin"
        );

        let absent = tmp.path().join("absent");
        assert_eq!(existing_binary_path(&absent), None);
    }

    /// Reviewer N1. The parse used to split on the literal `";\n"`, so a plugin
    /// a user had opened in a CRLF editor terminated its line with `;\r\n`, read
    /// back as `None`, and [`auto_install_to`] then treated a perfectly valid
    /// pin as "nothing to preserve" and clobbered it with the launching build's
    /// path — the "repair only when unusable" rule silently bypassed for that
    /// corner.
    #[test]
    fn a_crlf_converted_plugin_still_yields_its_pin_and_keeps_it() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        let users_own = tmp.path().join("users-own").join("dot-agent-deck");
        write_executable(&users_own);
        let pin = users_own.to_str().expect("UTF-8");

        write_plugin(&root, pin).expect("seed plugin");
        let file = plugin_file(&root);
        let crlf = std::fs::read_to_string(&file)
            .expect("read plugin")
            .replace('\n', "\r\n");
        std::fs::write(&file, &crlf).expect("rewrite as CRLF");

        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some(pin),
            "a CRLF line ending must not hide the pin"
        );

        auto_install_to(std::slice::from_ref(&root), "/bin/deck-launching");
        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some(pin),
            "a valid pin was clobbered because its line ended CRLF"
        );
    }

    /// PRD #381 audit, MEDIUM-1 (the OpenCode half). `split_once` accepted the
    /// first `const BINARY_PATH = ` marker ANYWHERE in the file, so a mention
    /// inside a comment in a hand-edited plugin became the preserved — and
    /// republished — value. The marker is now anchored to the start of a line,
    /// which is where [`plugin_template`] emits it.
    #[test]
    fn a_commented_out_marker_cannot_become_the_preserved_pin() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        let users_own = tmp.path().join("users-own").join("dot-agent-deck");
        write_executable(&users_own);
        let pin = users_own.to_str().expect("UTF-8");

        write_plugin(&root, pin).expect("seed plugin");
        let file = plugin_file(&root);
        let body = std::fs::read_to_string(&file).expect("read plugin");
        let hand_edited =
            format!("// was: const BINARY_PATH = \"/tmp/attacker/dot-agent-deck\";\n{body}");
        std::fs::write(&file, &hand_edited).expect("rewrite with a comment above");

        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some(pin),
            "a commented-out marker must not be read as the pin"
        );

        // Launched by the pinned install itself: the one automatic install that
        // still regenerates the file. A plugin pinning ANOTHER live install is
        // left as it is (PRD #1487), which would prove nothing here.
        auto_install_to(std::slice::from_ref(&root), pin);
        let after = std::fs::read_to_string(&file).expect("read regenerated plugin");
        assert!(
            !after.contains("/tmp/attacker/dot-agent-deck"),
            "a commented-out path was promoted into the generated constant:\n{after}"
        );
    }

    /// PRD #381 audit, MEDIUM-2: the plugin is JavaScript OpenCode *executes*,
    /// and it is now published temp-file-plus-`rename` like the other three
    /// config writers. `std::fs::write` followed a pre-created symlink at the
    /// destination and truncated it in place; `rename` replaces the name.
    #[cfg(unix)]
    #[test]
    fn write_plugin_publishes_atomically_and_does_not_write_through_a_symlink() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        let file = plugin_file(&root);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("create plugin dir");

        let elsewhere = tmp.path().join("elsewhere.js");
        std::fs::write(&elsewhere, b"// the victim's own file\n").expect("seed the link target");
        std::os::unix::fs::symlink(&elsewhere, &file).expect("pre-create the destination symlink");

        write_plugin(&root, "/bin/deck").expect("publish plugin");

        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("read the link target"),
            "// the victim's own file\n",
            "the publish wrote through a pre-created destination symlink"
        );
        assert!(
            std::fs::symlink_metadata(&file)
                .expect("stat the published plugin")
                .file_type()
                .is_file(),
            "the destination should now be a regular file, not the symlink"
        );
        assert_eq!(existing_binary_path(&root).as_deref(), Some("/bin/deck"));

        // No temp file is left beside the destination.
        let strays: Vec<_> = std::fs::read_dir(file.parent().expect("parent"))
            .expect("list plugin dir")
            .map(|e| e.expect("dir entry").file_name())
            .filter(|n| n != "dot-agent-deck.js")
            .collect();
        assert!(strays.is_empty(), "temp file left behind: {strays:?}");
    }

    /// PRD #381 audit, MEDIUM-1 (OpenCode, the pin side): a legacy plugin
    /// pinning the BARE `"dot-agent-deck"` is repaired even when the process cwd
    /// holds a file of that name. `Path::try_exists` is cwd-relative; Node's
    /// `execFileSync` resolves the persisted string through the AGENT's `$PATH`.
    #[test]
    #[serial_test::serial]
    fn a_bare_pin_is_repaired_even_when_the_cwd_holds_a_file_of_that_name() {
        struct CwdGuard(PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }

        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        write_plugin(&root, crate::platform::paths::DEFAULT_BINARY_NAME).expect("seed bare pin");

        let cwd = tmp.path().join("some-checkout");
        write_executable(&cwd.join(crate::platform::paths::DEFAULT_BINARY_NAME));
        let _restore = CwdGuard(std::env::current_dir().expect("read cwd"));
        std::env::set_current_dir(&cwd).expect("move into the decoy directory");
        assert_eq!(
            Path::new(crate::platform::paths::DEFAULT_BINARY_NAME)
                .try_exists()
                .ok(),
            Some(true),
            "the fixture must reproduce the cwd collision, or it proves nothing"
        );

        let durable = tmp.path().join("opt").join("dot-agent-deck");
        write_executable(&durable);
        auto_install_to(
            std::slice::from_ref(&root),
            durable.to_str().expect("UTF-8"),
        );

        assert_eq!(
            existing_binary_path(&root).as_deref(),
            durable.to_str(),
            "issue #536: a bare BINARY_PATH survived because the cwd held a file \
             of that name"
        );
    }

    /// PRD #1497: the plugin reports through the binary `DOT_AGENT_DECK_BIN`
    /// names when that is an absolute path, and through its pinned
    /// `BINARY_PATH` otherwise — unset, empty, whitespace, a bare name the
    /// agent's `PATH` would resolve, or a path relative to its working
    /// directory (tester H2). Loaded under Node with two recorder scripts, one
    /// pinned and one named by the variable, the latter's directory on `PATH`
    /// and as the working directory so a relative value would reach it; each
    /// run sends one event and exactly one recorder receives it.
    #[cfg(unix)]
    #[test]
    fn plugin_reports_through_dot_agent_deck_bin_when_it_is_set() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let recorder = |name: &str| {
            let script = dir.path().join(name);
            let out = dir.path().join(format!("{name}.jsonl"));
            crate::test_isolation::write_script(
                &script,
                format!(
                    "#!/bin/sh\ncat >> '{}'\necho >> '{}'\n",
                    out.display(),
                    out.display()
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            (script, out)
        };
        let (pinned, pinned_out) = recorder("pinned");
        let (built, built_out) = recorder("built");
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&pinned.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import pluginModule from "{}";
const plugin = pluginModule.server;
const hooks = await plugin({{ directory: "/work" }});
await hooks.event({{ event: {{ type: "session.error", properties: {{ sessionID: "s1", error: "boom" }} }} }});
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let lines = |path: &Path| {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        };
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let run = |bin: Option<&str>| {
            let mut cmd = std::process::Command::new("node");
            cmd.arg(&driver)
                .current_dir(dir.path())
                .env("PATH", &path)
                .env_remove(crate::platform::paths::DOT_AGENT_DECK_BIN);
            if let Some(bin) = bin {
                cmd.env(crate::platform::paths::DOT_AGENT_DECK_BIN, bin);
            }
            assert!(
                cmd.status().expect("run node").success(),
                "the plugin driver failed"
            );
        };

        // One run sends the error and whatever the plugin reports on its own
        // around it (its shutdown handler), so count one run's events first.
        run(None);
        let per_run = lines(&pinned_out);
        assert!(per_run > 0, "the plugin reported nothing");
        assert_eq!(lines(&built_out), 0, "unset: only the pinned binary");
        let mut runs = 1;
        for ignored in ["", "   ", "built", "./built"] {
            run(Some(ignored));
            runs += 1;
            assert_eq!(
                (lines(&pinned_out), lines(&built_out)),
                (runs * per_run, 0),
                "{ignored:?} is not absolute: the pinned binary"
            );
        }
        run(Some(&built.to_string_lossy()));
        assert_eq!(
            (lines(&pinned_out), lines(&built_out)),
            (runs * per_run, per_run),
            "absolute: only the named binary"
        );
    }

    /// PRD #91 (Greptile and Qodo on PR #1925): writing the TUI plugin and
    /// adding it to a TUI config is a change the TUI's startup note counts; a
    /// pass that writes nothing (no pin to write, a config it will not edit)
    /// is not.
    #[test]
    fn opencode_tui_half_writes_count_as_a_change_and_a_refusal_does_not() {
        let root = tempfile::tempdir().unwrap();
        let (first, changed) = crate::agent_hook_config::noting_auto_install_changes(|| {
            install_tui_half(root.path(), Some("/opt/dot-agent-deck")).unwrap()
        });
        assert!(matches!(first, TuiEntry::Added(_)) && changed);

        let refused = tempfile::tempdir().unwrap();
        std::fs::write(refused.path().join(TUI_CONFIG_FILES[0]), "{ not json").unwrap();
        let (again, changed) = crate::agent_hook_config::noting_auto_install_changes(|| {
            install_tui_half(refused.path(), None).unwrap()
        });
        assert!(matches!(again, TuiEntry::Refused(_, _)) && !changed);
    }

    /// Hardening M5: the TUI config is edited under the same cross-process
    /// lock the deck's other agent-config writers take, for the whole
    /// read-splice-publish: while another process holds it, nothing is written.
    #[test]
    fn opencode_tui_config_edit_waits_for_the_config_lock() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(TUI_CONFIG_FILES[0]);
        let held = crate::agent_hook_config::lock_config(&config).unwrap();
        let worker = {
            let root = root.path().to_path_buf();
            std::thread::spawn(move || {
                install_tui_half(&root, Some("/opt/dot-agent-deck")).map(|_| ())
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            !config.exists(),
            "the config was written while another process held its lock"
        );
        drop(held);
        worker.join().unwrap().unwrap();
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("dot-agent-deck-tui.js")
        );
    }

    /// A recorder standing in for the deck binary: `inbox wait` registers the
    /// receiver, hands over one deck message as attempt `(1, 1)`, grants every
    /// final hand-over check it is asked, and records every control line the
    /// plugin writes back; every call is logged.
    #[cfg(unix)]
    fn inbox_recorder(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        inbox_recorder_granting_after(dir, None)
    }

    /// [`inbox_recorder`], answering each final hand-over check after
    /// `delay` seconds (`sleep`'s argument) when given.
    #[cfg(unix)]
    fn inbox_recorder_granting_after(
        dir: &std::path::Path,
        delay: Option<&str>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        inbox_recorder_timed(dir, delay, None)
    }

    /// [`inbox_recorder_granting_after`], also handing over its message only
    /// `message_delay` seconds (`sleep`'s argument) after registering, when
    /// given.
    #[cfg(unix)]
    fn inbox_recorder_timed(
        dir: &std::path::Path,
        delay: Option<&str>,
        message_delay: Option<&str>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let wait = delay.map(|d| format!("sleep {d}; ")).unwrap_or_default();
        let message_wait = message_delay
            .map(|d| format!("    sleep {d}\n"))
            .unwrap_or_default();
        let controls = dir.join("controls.log");
        let recorder = dir.join("recorder.sh");
        let id = "m-k3xzq2ab-0f3c9a71d2e4b850";
        let message = serde_json::json!({"messages": [{
            "id": id,
            "subject": "s",
            "from": "x",
            "text": format!("[dot-agent-deck message {id} from x]\nbody\n[end of dot-agent-deck message {id}]"),
            "a": 1, "e": 1, "d": 1,
        }]});
        let registered = serde_json::json!({"registered": {"rx": 7, "slot": "opencode_tui", "keepalive_ms": 30000}});
        crate::test_isolation::write_script(
            &recorder,
            format!(
                r#"#!/bin/sh
echo "call $*" >> '{c}'
case "$1 $2" in
  'inbox wait')
    printf '%s\n' '{r}'
{message_wait}    printf '%s\n' '{m}'
    while IFS= read -r line; do
      printf '%s\n' "$line" >> '{c}'
      case "$line" in
        *'"may"'*)
          q=$(printf '%s' "$line" | sed -n 's/.*"q":\([0-9]*\).*/\1/p')
          {wait}printf '{{"grant":{{"q":%s,"ok":true}}}}\n' "$q";;
      esac
    done;;
esac
exit 0
"#,
                c = controls.display(),
                r = registered,
                m = message,
                wait = wait,
                message_wait = message_wait,
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (recorder, controls)
    }

    /// Run `driver` under Node in a deck pane's environment; its last stdout
    /// line, parsed.
    #[cfg(unix)]
    fn run_inbox_driver(driver: &std::path::Path) -> serde_json::Value {
        let out = std::process::Command::new("node")
            .arg(driver)
            .env("DOT_AGENT_DECK_PANE_ID", "pane-1")
            .env("DOT_AGENT_DECK_PANE_CAPABILITY", "a".repeat(64))
            .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
            .output()
            .expect("run node");
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(stdout.lines().last().unwrap_or("null")).unwrap()
    }

    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// The fake OpenCode TUI API the TUI plugin is driven with: a route, a
    /// dialog flag and a session client the driver controls.
    const FAKE_TUI_API_JS: &str = r#"const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const prompts = [];
const navigations = [];
let dialog = false;
let route = { name: "home", params: {} };
const sessions = { child: { parentID: "rootA" }, rootA: {}, rootB: {} };
let disposeCb;
let releaseCreate;
const api = {
  state: { session: { get: (id) => sessions[id], messages: () => [] } },
  ui: { dialog: { get open() { return dialog; } } },
  mode: { current: () => "normal" },
  route: {
    get current() { return route; },
    navigate: (name, params) => { navigations.push(params.sessionID); route = { name, params }; },
  },
  client: { session: {
    create: () => new Promise((r) => { releaseCreate = r; }),
    promptAsync: async (req) => { prompts.push(req.sessionID); return {}; },
  } },
  event: { on: () => {} },
  lifecycle: { onDispose: (cb) => { disposeCb = cb; } },
};
"#;

    /// Hardening M7: a delivery waiting for a dialog to close re-reads the
    /// route when it does — the person switched to another session meanwhile —
    /// and starts the message where they now are, never navigating back to the
    /// session it saw before the wait.
    #[cfg(unix)]
    #[test]
    fn opencode_tui_delivery_rereads_the_route_after_a_dialog_wait() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, _controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck-tui.mjs");
        std::fs::write(&plugin, tui_plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"{FAKE_TUI_API_JS}
dialog = true;
route = {{ name: "session", params: {{ sessionID: "child" }} }};
const {{ default: mod }} = await import("{}");
await mod.tui(api);
await sleep(700);
route = {{ name: "session", params: {{ sessionID: "rootB" }} }};
dialog = false;
await sleep(700);
disposeCb?.();
console.log(JSON.stringify({{ prompts, navigations }}));
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        assert_eq!(result["navigations"], serde_json::json!([]), "{result}");
        assert_eq!(result["prompts"], serde_json::json!(["rootB"]), "{result}");
    }

    /// Hardening M7: disposing the TUI plugin while it waits on OpenCode to
    /// create a session leaves the old instance with nothing to do: no
    /// navigation, no prompt.
    #[cfg(unix)]
    #[test]
    fn opencode_tui_delivery_stops_when_disposed_during_an_api_call() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, _controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck-tui.mjs");
        std::fs::write(&plugin, tui_plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"{FAKE_TUI_API_JS}
const {{ default: mod }} = await import("{}");
await mod.tui(api);
for (let n = 0; n < 100 && !releaseCreate; n++) await sleep(20);
const held = !!releaseCreate;
disposeCb();
releaseCreate?.({{ data: {{ id: "fresh" }} }});
await sleep(300);
console.log(JSON.stringify({{ held, prompts, navigations }}));
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        assert_eq!(
            result["held"], true,
            "the delivery reached session.create: {result}"
        );
        assert_eq!(result["navigations"], serde_json::json!([]), "{result}");
        assert_eq!(result["prompts"], serde_json::json!([]), "{result}");
    }

    /// Hardening M7: in `--mini`, a delivery whose target session changes
    /// while it looks up that session's model is reported positively not
    /// handed (PRD #1666 §2), not started in the session that is no longer the
    /// target.
    #[cfg(unix)]
    #[test]
    fn opencode_mini_delivery_is_handed_back_when_its_session_changes_mid_lookup() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"process.argv = [process.argv[0], "opencode", "--mini"];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const prompts = [];
let releaseMessages;
const client = {{ session: {{
  messages: () => new Promise((r) => {{ releaseMessages = r; }}),
  promptAsync: async (req) => {{ prompts.push(req.path.id); return {{}}; }},
}} }};
const {{ default: {{ server: plugin }} }} = await import("{}");
const hooks = await plugin({{ directory: "/work", client }});
await hooks.event({{ event: {{ type: "session.created", properties: {{ info: {{ id: "s1" }} }} }} }});
for (let n = 0; n < 100 && !releaseMessages; n++) await sleep(20);
const held = !!releaseMessages;
await hooks.event({{ event: {{ type: "session.created", properties: {{ info: {{ id: "s2" }} }} }} }});
releaseMessages?.({{ data: [] }});
await sleep(400);
console.log(JSON.stringify({{ held, prompts }}));
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        assert_eq!(
            result["held"], true,
            "the delivery reached the lookup: {result}"
        );
        assert_eq!(result["prompts"], serde_json::json!([]), "{result}");
        let written = std::fs::read_to_string(&controls).unwrap_or_default();
        assert!(
            written.contains("\"not_handed\""),
            "positively not handed: {written}"
        );
    }

    /// Scenario (PRD #2050, inbox/opencode/020 on 1.19.5): OpenCode 1.x runs
    /// `opencode --mini`, calls the module's `server`, creates mini's first
    /// session, and then — as 1.19.5 does — calls the 2.x `setup` too, once
    /// with its own context (no `event`, no `session`) and once with a
    /// 2.x-shaped one. `setup` stays inert both times: nothing is announced,
    /// no doorbell-only receiver replaces mini's, nothing is subscribed, and
    /// the deck's message still starts in mini's session as 1.x's mini
    /// receiver delivers it.
    #[cfg(unix)]
    #[test]
    fn opencode_1x_mini_stays_the_receiver_when_1x_also_calls_setup() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"process.argv = [process.argv[0], "opencode", "--mini"];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const prompts = [];
let subscribed = false;
const client = {{ session: {{
  messages: async () => ({{ data: [] }}),
  promptAsync: async (req) => {{ prompts.push(req.path.id); return {{}}; }},
}} }};
const {{ default: mod }} = await import("{}");
const hooks = await mod.server({{ directory: "/work", client }});
await hooks.event({{ event: {{ type: "session.created", properties: {{ info: {{ id: "s1" }} }} }} }});
const own = await mod.setup({{ options: {{}}, agent: {{}}, aisdk: {{}}, catalog: {{}}, command: {{}}, integration: {{}}, plugin: {{}}, reference: {{}}, skill: {{}} }});
const shaped = await mod.setup({{
  location: {{ directory: "/work" }},
  event: {{ subscribe: () => {{ subscribed = true; return {{ [Symbol.asyncIterator]: () => ({{ next: () => new Promise(() => {{}}) }}) }}; }} }},
  session: {{ hook: async () => undefined, prompt: async () => ({{}}) }},
  tool: {{ hook: async () => undefined }},
}});
for (let n = 0; n < 150 && !prompts.length; n++) await sleep(20);
await sleep(300);
console.log(JSON.stringify({{ prompts, subscribed, cleanups: [typeof own, typeof shaped] }}));
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        assert_eq!(result["prompts"], serde_json::json!(["s1"]), "{result}");
        assert_eq!(result["subscribed"], false, "{result}");
        assert_eq!(
            result["cleanups"],
            serde_json::json!(["function", "function"]),
            "{result}"
        );
        let written = std::fs::read_to_string(&controls).unwrap_or_default();
        assert!(
            !written.contains("call inbox announce"),
            "mini is the receiver itself, it does not announce: {written}"
        );
        assert!(
            !written.contains("--doorbell-only"),
            "no doorbell-only receiver replaces mini's: {written}"
        );
        assert_eq!(
            written.matches("call inbox wait").count(),
            1,
            "one receiver, mini's: {written}"
        );
        assert!(written.contains("\"handed\""), "{written}");
    }

    /// PRD #1858: each update of an assistant message appends its usage to
    /// the file the deck names, one line of named fields, an unchanged update
    /// appends nothing, and neither the message's text nor its error message
    /// reaches the file. Counts OpenCode gave the plugin that are missing or
    /// not counts are written as `null`, and the deck's reader takes each
    /// line as the plugin means it: an empty, partial, renamed, string or
    /// negative count unrecognized, never a reported zero.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_appends_assistant_usage_to_the_deck_usage_file() {
        use crate::telemetry::accounting::usage_file::{UsageAgent, UsageFileReader};
        use std::os::unix::fs::OpenOptionsExt as _;
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, _controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let usage = dir.path().join("run.usage.jsonl");
        let private = |path: &std::path::Path| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap();
        };
        private(&usage);
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"const {{ default: {{ server: plugin }} }} = await import("{}");
const hooks = await plugin({{ directory: "/work", client: {{}} }});
const updated = (info) => hooks.event({{ event: {{ type: "message.updated", properties: {{ info }} }} }});
await hooks.event({{ event: {{ type: "session.created", properties: {{ info: {{ id: "s1", directory: "/work" }} }} }} }});
const done = {{ time: {{ created: 1, completed: 2 }}, finish: "stop", role: "assistant", sessionID: "s1", modelID: "gpt-5.4-mini" }};
const reply = {{ ...done, id: "msg_1", cost: 0, tokens: {{ input: 100, output: 15, reasoning: 5, cache: {{ read: 7, write: 0 }} }} }};
await updated(reply);
await updated(reply);
await updated({{ id: "msg_2", sessionID: "s1", role: "assistant", modelID: "gpt-5.4-mini", cost: 0,
  tokens: {{ input: 0, output: 0, reasoning: 0, cache: {{ read: 0, write: 0 }} }}, time: {{ created: 3 }},
  error: {{ name: "APIError", data: {{ message: "SECRET_ERROR_TEXT", statusCode: 400 }} }} }});
await updated({{ id: "msg_3", sessionID: "s1", role: "user", text: "SECRET_PROMPT" }});
await updated({{ ...done, id: "msg_4", tokens: {{}} }});
await updated({{ ...done, id: "msg_5", tokens: {{ input: 5, output: "7", reasoning: 0, cache: {{ read: 0, write: 0 }} }} }});
await updated({{ ...done, id: "msg_6", tokens: {{ inputTokens: 5, output: 7, reasoning: 0, cache: {{ read: 0, write: 0 }} }} }});
await updated({{ ...done, id: "msg_7", tokens: {{ input: 5, output: 7, reasoning: -1, cache: {{ read: 0, write: 0 }} }} }});
console.log("null");
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let run = |file: &std::path::Path| {
            let out = std::process::Command::new("node")
                .arg(&driver)
                .env("DOT_AGENT_DECK_USAGE_FILE", file)
                .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
                .output()
                .expect("run node");
            assert!(out.status.success(), "{out:?}");
        };
        run(&usage);
        let written = std::fs::read_to_string(&usage).unwrap();
        assert!(!written.contains("SECRET"), "{written}");
        let lines: Vec<serde_json::Value> = written
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let nulls = |id: &str, input: serde_json::Value, output: serde_json::Value| {
            serde_json::json!({"v":1,"type":"reply","agent":"opencode","id":id,
                "model":"gpt-5.4-mini","input":input,"output":output,
                "cache_read":if id == "msg_4" { serde_json::Value::Null } else { serde_json::json!(0) },
                "cache_write":if id == "msg_4" { serde_json::Value::Null } else { serde_json::json!(0) },
                "stop":"stop"})
        };
        assert_eq!(
            lines,
            vec![
                serde_json::json!({"v":1,"type":"reply","agent":"opencode","id":"msg_1",
                    "model":"gpt-5.4-mini","input":100,"output":20,"cache_read":7,"cache_write":0,
                    "cost_usd":0,"stop":"stop"}),
                serde_json::json!({"v":1,"type":"reply","agent":"opencode","id":"msg_2",
                    "model":"gpt-5.4-mini","reported":false,"error":"APIError","status":400}),
                nulls("msg_4", serde_json::Value::Null, serde_json::Value::Null),
                nulls("msg_5", serde_json::json!(5), serde_json::Value::Null),
                nulls("msg_6", serde_json::Value::Null, serde_json::json!(7)),
                nulls("msg_7", serde_json::json!(5), serde_json::Value::Null),
            ],
            "{written}"
        );
        // Each line as the deck's reader takes it.
        let want = [
            "provider_reported",
            "not_recorded",
            "unrecognized_format",
            "unrecognized_format",
            "unrecognized_format",
            "unrecognized_format",
        ];
        for (n, (line, want)) in written.lines().zip(want).enumerate() {
            let one = dir.path().join(format!("line-{n}.jsonl"));
            std::fs::write(&one, format!("{line}\n")).unwrap();
            let mut reader = UsageFileReader::new(&one);
            reader.poll().unwrap();
            let tokens = reader.summary().usage(UsageAgent::OpenCode).models[0].tokens;
            let got = tokens
                .unavailable()
                .map_or(tokens.provenance().as_str(), |r| r.as_str());
            assert_eq!(got, want, "{line}: {tokens:?}");
        }
        // A path that is not a private regular file the deck made is not
        // written: a symlink, and a name that is not there.
        let target = dir.path().join("elsewhere");
        private(&target);
        let link = dir.path().join("link.usage.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        run(&link);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
        let missing = dir.path().join("missing.usage.jsonl");
        run(&missing);
        assert!(!missing.exists());
    }

    /// PRD #1858: what the plugin writes, as the deck's reader takes it. A
    /// subagent's reply that completes while the main session's reply is
    /// still asking for tools adds its usage but leaves the run mid-turn. A
    /// message that took two steps counts both steps' tokens, each once,
    /// though OpenCode's message holds only the latest step's. And an update
    /// whose line could not be written — the file briefly readable by others
    /// — is tried again when the same update comes again, here once the file
    /// is private again.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_usage_counts_subagents_steps_and_retries() {
        use crate::telemetry::accounting::usage_file::{UsageAgent, UsageFileReader};
        use crate::telemetry::accounting::{AgentTurnEnd, Field, Tokens};
        use std::os::unix::fs::OpenOptionsExt as _;
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, _controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let usage = dir.path().join("run.usage.jsonl");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&usage)
            .unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import {{ chmodSync }} from "fs";
const {{ default: {{ server: plugin }} }} = await import("{}");
const hooks = await plugin({{ directory: "/work", client: {{}} }});
const send = (type, properties) => hooks.event({{ event: {{ type, properties }} }});
const updated = (info) => send("message.updated", {{ info }});
const step = (id, messageID, tokens) =>
  send("message.part.updated", {{ part: {{ id, messageID, sessionID: "root", type: "step-finish", reason: "stop", cost: 0.01, tokens }} }});
const tokens = (input, output, reasoning, read, write) => ({{ input, output, reasoning, cache: {{ read, write }} }});
const reply = (id, sessionID, finish, cost, counts) => ({{ id, sessionID, role: "assistant", modelID: "gpt-5.4-mini",
  time: {{ created: 1, completed: 2 }}, finish, cost, tokens: counts }});
await send("session.created", {{ sessionID: "root", info: {{ id: "root", directory: "/work" }} }});
await send("session.created", {{ sessionID: "child", info: {{ id: "child", parentID: "root", directory: "/work" }} }});
// A reply of the main session asking for tools, then a subagent's that completes.
await updated(reply("msg_p", "root", "tool-calls", 0.1, tokens(10, 2, 0, 0, 0)));
await updated(reply("msg_c", "child", "stop", 0.1, tokens(5, 2, 0, 0, 0)));
// Two steps of one message, the first step's part updated twice.
const first = tokens(100, 10, 0, 5, 1);
const second = tokens(50, 4, 2, 0, 0);
await step("prt_1", "msg_s", first);
await updated(reply("msg_s", "root", "tool-calls", 0.01, first));
await step("prt_1", "msg_s", first);
await step("prt_2", "msg_s", second);
await updated(reply("msg_s", "root", "tool-calls", 0.02, second));
// A write that fails, then the same update again.
const retried = reply("msg_r", "root", "tool-calls", 0.1, tokens(7, 1, 0, 0, 0));
chmodSync(process.env.DOT_AGENT_DECK_USAGE_FILE, 0o644);
await updated(retried);
chmodSync(process.env.DOT_AGENT_DECK_USAGE_FILE, 0o600);
await updated(retried);
await updated(retried);
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let out = std::process::Command::new("node")
            .arg(&driver)
            .env("DOT_AGENT_DECK_USAGE_FILE", &usage)
            .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
            .output()
            .expect("run node");
        assert!(out.status.success(), "{out:?}");
        let written = std::fs::read_to_string(&usage).unwrap();
        let lines: Vec<serde_json::Value> = written
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let of = |id: &str| -> Vec<&serde_json::Value> {
            lines.iter().filter(|l| l["id"] == id).collect()
        };
        assert_eq!(of("msg_c")[0]["subagent"], true, "{written}");
        assert!(of("msg_p")[0].get("subagent").is_none(), "{written}");
        let last = of("msg_s").last().copied().unwrap();
        assert_eq!(
            (
                &last["input"],
                &last["output"],
                &last["cache_read"],
                &last["cache_write"]
            ),
            (
                &serde_json::json!(150),
                &serde_json::json!(16),
                &serde_json::json!(5),
                &serde_json::json!(1)
            ),
            "{written}"
        );
        assert_eq!(last["cost_usd"], 0.02, "{written}");
        assert_eq!(of("msg_r").len(), 1, "{written}");

        let mut reader = UsageFileReader::new(&usage);
        reader.poll().unwrap();
        let read = reader.summary().usage(UsageAgent::OpenCode);
        assert_eq!(read.last_turn, Some(AgentTurnEnd::MidTurn));
        assert_eq!(
            read.models[0].tokens,
            Field::ProviderReported(Tokens {
                // The deck's input count includes the cache.
                input: 10 + 5 + (150 + 5 + 1) + 7,
                output: 2 + 2 + 16 + 1,
                cache_read: 5,
                cache_write: 1,
            })
        );
    }

    /// PRD #1858: what the usage helper keeps of an assistant message between
    /// its updates is a copy of the fields its line is made from, and of a
    /// step only its counts: no path, structured reply, error message or
    /// response body the update carried, and none of the update's own
    /// objects, stays reachable from it.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_usage_keeps_only_the_fields_its_line_needs() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let source = format!(
            r#"const fsConstants = {{ O_WRONLY: 1, O_APPEND: 1024 }};
const written = [];
const openSync = () => 3;
const fstatSync = () => ({{ isFile: () => true, uid: process.getuid(), mode: 0o100600 }});
const writeSync = (_fd, buffer) => {{ written.push(buffer.toString()); return buffer.length; }};
const closeSync = () => {{}};
const sessionKey = (id) => (typeof id === "string" && id ? id : undefined);
const ancestryOfKey = (key) => (key === "root" ? "root" : undefined);
const rememberBounded = (set, key) => {{ set.delete(key); set.add(key); }};
{USAGE_FILE_JS}
const SENTINEL = "SENTINEL_4c19e7";
const info = {{ id: "msg_1", sessionID: "root", role: "assistant", modelID: "gpt-5.4-mini", providerID: "openai",
  time: {{ created: 1, completed: 2 }}, finish: "stop", cost: 0.01,
  path: {{ cwd: "/" + SENTINEL + "/cwd", root: "/" + SENTINEL + "/root" }},
  structured: {{ answer: SENTINEL + " structured reply" }},
  tokens: {{ input: 10, output: 2, reasoning: 0, cache: {{ read: 1, write: 0, note: SENTINEL }}, note: SENTINEL }},
  error: {{ name: "APIError", data: {{ message: SENTINEL + " message", responseBody: SENTINEL + " body", statusCode: 500 }} }} }};
const part = {{ id: "prt_1", messageID: "msg_1", sessionID: "root", type: "step-finish", reason: SENTINEL,
  snapshot: SENTINEL, tokens: {{ input: 10, output: 2, reasoning: 0, cache: {{ read: 1, write: 0 }}, note: SENTINEL }} }};
recordUsageStep(part);
recordUsage(info);
const originals = new Set([info, info.time, info.path, info.structured, info.tokens, info.tokens.cache, info.error,
  info.error.data, part, part.tokens, part.tokens.cache]);
const found = [];
const seen = new Set();
const walk = (value, at) => {{
  if (originals.has(value)) found.push(at + " is the update's own object");
  if (typeof value === "string" && value.includes(SENTINEL)) found.push(at + " holds the sentinel");
  if (typeof value !== "object" || value === null || seen.has(value)) return;
  seen.add(value);
  const entries = value instanceof Map ? Array.from(value.entries())
    : value instanceof Set ? Array.from(value, (key) => [key, key])
    : Object.entries(value);
  for (const [key, inner] of entries) {{
    walk(key, at + "[key]");
    walk(inner, at + "." + String(key));
  }}
}};
walk(usageMessages, "usageMessages");
walk(usageForgotten, "usageForgotten");
const message = usageMessages.get("msg_1");
console.log(JSON.stringify({{
  found,
  info: Object.keys(message.info).sort(),
  step: Object.keys(message.steps.get("prt_1")).sort(),
  cache: Object.keys(message.steps.get("prt_1").cache).sort(),
  written,
}}));
"#
        );
        let output = std::process::Command::new("node")
            .arg("--input-type=module")
            .arg("-e")
            .arg(&source)
            .env("DOT_AGENT_DECK_USAGE_FILE", "/usage.jsonl")
            .output()
            .expect("run node");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["found"], serde_json::json!([]), "{result}");
        assert_eq!(
            result["info"],
            serde_json::json!([
                "completed",
                "cost",
                "error",
                "failed",
                "finish",
                "id",
                "model",
                "session",
                "status",
                "tokens"
            ]),
            "{result}"
        );
        assert_eq!(
            result["step"],
            serde_json::json!(["cache", "input", "output", "reasoning"]),
            "{result}"
        );
        assert_eq!(
            result["cache"],
            serde_json::json!(["read", "write"]),
            "{result}"
        );
        // What the line is made from is still all there.
        let line: serde_json::Value =
            serde_json::from_str(result["written"][0].as_str().unwrap().trim()).unwrap();
        assert_eq!(
            line,
            serde_json::json!({"v":1,"type":"reply","agent":"opencode","id":"msg_1",
                "model":"gpt-5.4-mini","input":10,"output":2,"cache_read":1,"cache_write":0,
                "cost_usd":0.01,"stop":"stop","error":"APIError","status":500}),
            "{result}"
        );
    }

    /// PRD #1858: a message whose recorded steps were forgotten — 256 other
    /// messages updated after its first step — and that then finishes a
    /// second step has its counts written as unknown, which the deck reads
    /// as unavailable, not as the second step's 50 input tokens alone.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_usage_of_a_message_whose_steps_were_forgotten_is_unavailable() {
        use crate::telemetry::accounting::usage_file::{UsageAgent, UsageFileReader};
        use crate::telemetry::accounting::{Field, Ident};
        use std::os::unix::fs::OpenOptionsExt as _;
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, _controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let usage = dir.path().join("run.usage.jsonl");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&usage)
            .unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"const {{ default: {{ server: plugin }} }} = await import("{}");
const hooks = await plugin({{ directory: "/work", client: {{}} }});
const send = (type, properties) => hooks.event({{ event: {{ type, properties }} }});
const updated = (info) => send("message.updated", {{ info }});
const step = (id, messageID, tokens) =>
  send("message.part.updated", {{ part: {{ id, messageID, sessionID: "root", type: "step-finish", reason: "stop", cost: 0.01, tokens }} }});
const tokens = (input, output) => ({{ input, output, reasoning: 0, cache: {{ read: 0, write: 0 }} }});
const reply = (id, modelID, finish, cost, counts) => ({{ id, sessionID: "root", role: "assistant", modelID,
  time: {{ created: 1, completed: 2 }}, finish, cost, tokens: counts }});
await send("session.created", {{ info: {{ id: "root", directory: "/work" }} }});
await step("prt_1", "long", tokens(100, 10));
await updated(reply("long", "gpt-5.4", "tool-calls", 0.01, tokens(100, 10)));
for (let n = 0; n < 256; n++) {{
  await updated(reply("other_" + n, "gpt-5.4-mini", "stop", 0, tokens(1, 1)));
}}
await step("prt_2", "long", tokens(50, 4));
await updated(reply("long", "gpt-5.4", "stop", 0.02, tokens(50, 4)));
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let out = std::process::Command::new("node")
            .arg(&driver)
            .env("DOT_AGENT_DECK_USAGE_FILE", &usage)
            .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
            .output()
            .expect("run node");
        assert!(out.status.success(), "{out:?}");
        let written = std::fs::read_to_string(&usage).unwrap();
        let last: serde_json::Value = written
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .rfind(|l| l["id"] == "long")
            .unwrap();
        assert_eq!(
            (&last["input"], &last["output"], &last["stop"]),
            (
                &serde_json::Value::Null,
                &serde_json::Value::Null,
                &serde_json::json!("stop")
            ),
            "{last}"
        );

        let mut reader = UsageFileReader::new(&usage);
        reader.poll().unwrap();
        let read = reader.summary().usage(UsageAgent::OpenCode);
        let long = read
            .models
            .iter()
            .find(|m| m.model == Ident::new("gpt-5.4"))
            .unwrap_or_else(|| panic!("{read:?}"));
        assert!(
            !matches!(long.tokens, Field::ProviderReported(_)),
            "a partial sum read as the message's usage: {read:?}"
        );
        assert_eq!(
            long.tokens.unavailable().map(|r| r.as_str()),
            Some("unrecognized_format"),
            "{read:?}"
        );
    }

    /// PRD #1858: a reply of a session whose ancestry the plugin does not
    /// know yet — OpenCode's client has not answered whether it is a child —
    /// ends no turn. Here the main session is asking for tools when a reply
    /// of that session completes: the run's ending reads as unknown, not
    /// completed, and stays so once the answer says the session is a
    /// subagent's and the reply is written again as one; the main session's
    /// own later reply says how the run ended.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_usage_of_unknown_ancestry_never_completes_the_run() {
        use crate::telemetry::accounting::usage_file::{UsageAgent, UsageFileReader};
        use crate::telemetry::accounting::{AgentTurnEnd, Field, Tokens};
        use std::os::unix::fs::OpenOptionsExt as _;
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, _controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let usage = dir.path().join("run.usage.jsonl");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&usage)
            .unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import {{ copyFileSync }} from "fs";
const {{ default: {{ server: plugin }} }} = await import("{}");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const asked = [];
let answer;
const client = {{ session: {{ get: (request) => {{
  asked.push(request?.path?.id);
  return new Promise((resolve) => {{ answer = (info) => resolve({{ data: info }}); }});
}} }} }};
const hooks = await plugin({{ directory: "/work", client }});
const send = (type, properties) => hooks.event({{ event: {{ type, properties }} }});
const updated = (info) => send("message.updated", {{ info }});
const reply = (id, sessionID, finish, input, output) => ({{ id, sessionID, role: "assistant", modelID: "gpt-5.4-mini",
  time: {{ created: 1, completed: 2 }}, finish, cost: 0.1,
  tokens: {{ input, output, reasoning: 0, cache: {{ read: 0, write: 0 }} }} }});
const file = process.env.DOT_AGENT_DECK_USAGE_FILE;
await send("session.created", {{ info: {{ id: "root", directory: "/work" }} }});
await updated(reply("msg_p", "root", "tool-calls", 10, 2));
await updated(reply("msg_c", "late", "stop", 5, 2));
copyFileSync(file, file + ".unknown");
answer?.({{ id: "late", parentID: "root" }});
await sleep(50);
await updated(reply("msg_c", "late", "stop", 5, 2));
copyFileSync(file, file + ".learned");
await updated(reply("msg_q", "root", "stop", 1, 1));
console.log(JSON.stringify({{ asked }}));
process.exit(0);
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let out = std::process::Command::new("node")
            .arg(&driver)
            .env("DOT_AGENT_DECK_USAGE_FILE", &usage)
            .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
            .output()
            .expect("run node");
        assert!(out.status.success(), "{out:?}");
        let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["asked"], serde_json::json!(["late"]), "{result}");
        let lines = |path: &std::path::Path| -> Vec<serde_json::Value> {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        let read = |path: &std::path::Path| {
            let mut reader = UsageFileReader::new(path);
            reader.poll().unwrap();
            reader.summary().usage(UsageAgent::OpenCode)
        };
        let unknown = dir.path().join("run.usage.jsonl.unknown");
        let learned = dir.path().join("run.usage.jsonl.learned");

        let written = lines(&unknown);
        assert!(written[0].get("subagent").is_none(), "{written:?}");
        assert_eq!(written[1]["id"], "msg_c", "{written:?}");
        assert_eq!(
            written[1]["subagent"],
            serde_json::Value::Null,
            "{written:?}"
        );
        assert!(written[1].get("subagent").is_some(), "{written:?}");
        let at_unknown = read(&unknown);
        assert_eq!(at_unknown.last_turn, Some(AgentTurnEnd::Unknown));
        assert_eq!(
            at_unknown.models[0].tokens,
            Field::ProviderReported(Tokens {
                input: 15,
                output: 4,
                cache_read: 0,
                cache_write: 0,
            })
        );

        let written = lines(&learned);
        assert_eq!(written.last().unwrap()["subagent"], true, "{written:?}");
        assert_eq!(read(&learned).last_turn, Some(AgentTurnEnd::Unknown));

        assert_eq!(read(&usage).last_turn, Some(AgentTurnEnd::Completed));
    }

    /// PRD #1666 §2–§4: drive the shared receipt receiver against a fake
    /// watcher child: `script` runs after the receiver and helpers are
    /// defined, with `feed(reply)` writing a daemon line to the receiver,
    /// `sent()` the controls it wrote back (parsed), `restart()` its watcher
    /// respawning, and `check(actual, expected, what)` failing the run.
    #[cfg(unix)]
    fn run_receiver_script(script: &str) {
        let prelude = r#"
import { EventEmitter } from "events";
const children = [];
const spawn = (_bin, args) => {
  const child = new EventEmitter();
  child.args = args;
  child.stdout = new EventEmitter();
  child.stdout.setEncoding = () => {};
  child.written = [];
  child.stdin = new EventEmitter();
  child.stdin.writableEnded = false;
  child.stdin.destroyed = false;
  child.stdin.write = (line) => { child.written.push(JSON.parse(line)); return true; };
  child.stdin.end = () => { child.stdin.writableEnded = true; };
  child.kill = () => {};
  children.push(child);
  return child;
};
const hookBinary = () => "/fake/dot-agent-deck";
"#;
        let helpers = r#"
const check = (actual, expected, what) => {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    console.error(what + ": " + JSON.stringify(actual) + " != " + JSON.stringify(expected));
    process.exit(1);
  }
};
const current = () => children[children.length - 1];
const feed = (reply) => current().stdout.emit("data", JSON.stringify(reply) + "\n");
const sent = () => current().written;
const tick = () => new Promise((r) => setImmediate(r));
"#;
        let source = format!(
            "{prelude}\n{}\n{helpers}\n{script}\nprocess.exit(0);\n",
            inbox_js("")
        );
        let output = std::process::Command::new("node")
            .arg("--input-type=module")
            .arg("-e")
            .arg(&source)
            .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
            .output()
            .expect("run node");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Scenario: A receiver's watcher dies while it holds an attempt, and the agent starts on that attempt before a replacement runs, then the replacement starts and the commit is reported again before it registers. Nothing goes to the dead or the unregistered watcher; once the replacement registers and asks for the attempt back, it is answered `retained` and the commit follows the answers. A commit a watcher took and then died with is sent again only to a successor that asks for its attempt back.
    #[cfg(unix)]
    #[test]
    fn opencode_receiver_keeps_a_stage_report_until_a_watcher_registers() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const ref = (e) => ({ id: "m-k3xzq2ab-0f3c9a71d2e4b850", a: 1, e });
feed({ messages: [{ ...ref(1), subject: "s", from: "x", text: "t", d: 1 }] });
const first = current();
first.emit("exit");
receiver.committed([ref(1)]);
check(first.written.filter((c) => c.committed).length, 0, "nothing to the dead watcher");
receiver.start();
receiver.committed([ref(1)]);
check(sent(), [], "nothing before the replacement registers");
feed({ registered: { rx: 5, slot: "opencode_tui", keepalive_ms: 30000 }, resume: [ref(1)] });
check(sent()[0], { resume_answers: [{ ...ref(1), answer: "retained" }] }, "the committed attempt is retained");
check(sent()[1], { committed: [ref(1)] }, "and its commit follows the answers");
check(sent().filter((c) => c.committed).length, 1, "once");

// Taken by a registered watcher that then died: resent only when asked back.
current().emit("exit");
receiver.start();
feed({ registered: { rx: 6, slot: "opencode_tui", keepalive_ms: 30000 } });
check(sent().filter((c) => c.committed || c.resume_answers).length, 0, "not asked back: settled");
receiver.committed([ref(2)]);
current().emit("exit");
receiver.start();
feed({ registered: { rx: 7, slot: "opencode_tui", keepalive_ms: 30000 }, resume: [ref(2)] });
check(sent()[0], { resume_answers: [{ ...ref(2), answer: "retained" }] }, "asked back");
check(sent()[1], { committed: [ref(2)] }, "and sent again");
"#,
        );
    }

    /// Scenario: A receiver is handed two attempts and reports both handed and committed to its watcher, which dies before publishing. The replacement's registration lists the first attempt as correctable and asks for neither back: only the first attempt's commit is sent again, its handed report and everything about the second are dropped, and nothing is answered. A later registration that lists nothing gets nothing, and one listing the first attempt again finds the report already gone.
    #[spec("inbox/opencode/052")]
    #[cfg(unix)]
    #[test]
    fn opencode_052_a_dead_watchers_commit_is_sent_again_only_for_a_correctable_attempt() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const ref = (e) => ({ id: "m-k3xzq2ab-0f3c9a71d2e4b850", a: 1, e });
feed({ messages: [{ ...ref(1), subject: "s", from: "x", text: "t", d: 1 }, { ...ref(2), subject: "s", from: "x", text: "t", d: 2 }] });
receiver.handed([ref(1), ref(2)]);
receiver.committed([ref(1), ref(2)]);
check(sent().filter((c) => c.committed).length, 1, "the first watcher took both commits");
current().emit("exit");
receiver.start();
feed({ registered: { rx: 5, slot: "opencode_tui", keepalive_ms: 30000 }, correctable: [ref(1)] });
check(sent().filter((c) => c.committed || c.handed), [{ committed: [ref(1)] }], "only the correctable commit is sent again");
check(sent().filter((c) => c.resume_answers).length, 0, "a correctable attempt is not resumed");

current().emit("exit");
receiver.start();
feed({ registered: { rx: 6, slot: "opencode_tui", keepalive_ms: 30000 } });
check(sent().filter((c) => c.committed || c.handed).length, 0, "no authority listed: dropped");
current().emit("exit");
receiver.start();
feed({ registered: { rx: 7, slot: "opencode_tui", keepalive_ms: 30000 }, correctable: [ref(1)] });
check(sent().filter((c) => c.committed || c.handed).length, 0, "and gone for good");
"#,
        );
    }

    /// Scenario: A receiver registers, is handed attempts with delivery numbers 1 and 3, and hears a keepalive. Every snapshot it sends rises in `seq`, names every attempt it holds, and reports only the contiguous observed prefix as `seen`, so the unobserved number 2 never reads as omitted.
    #[spec("inbox/opencode/028")]
    #[cfg(unix)]
    #[test]
    fn opencode_028_snapshots_rise_and_report_a_contiguous_watermark() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
check(current().args.slice(0, 7), ["inbox", "wait", "--agent", "opencode", "--stream",
  "--adapter-integration", String(INBOX_ADAPTER_INTEGRATION)], "the watcher declares the integration");
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const msg = (e, d) => ({ id: "m-k3xzq2ab-0f3c9a71d2e4b850", subject: "s", from: "x", text: "t", a: 1, e, d });
feed({ messages: [msg(1, 1), msg(3, 3)] });
feed({ keepalive: true });
const snaps = sent().filter((c) => c.possess).map((c) => c.possess);
check(snaps.map((p) => p.seq), [1, 2, 3], "seq rises strictly");
check(snaps[2].seen, 1, "seen is the contiguous prefix");
check(snaps[2].held.map((h) => h.e), [1, 3], "both attempts are held");
"#,
        );
    }

    /// Scenario: A revoke reaches an attempt still queued behind a dialog, then one already handed to OpenCode. The queued one is dropped and answered `revoked_ack`; the handed one is answered `already_handed`; a later hand-over check for the revoked one is refused and reported positively not handed.
    #[spec("inbox/opencode/029")]
    #[cfg(unix)]
    #[test]
    fn opencode_029_revokes_are_answered_by_where_the_callback_stands() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
feed({ messages: [{ id, subject: "s", from: "x", text: "t", a: 1, e: 1, d: 1 },
  { id, subject: "s", from: "x", text: "t", a: 1, e: 2, d: 2 }] });
receiver.calling({ id, a: 1, e: 2 });
feed({ revoke: [{ id, a: 1, e: 1 }] });
feed({ revoke: [{ id, a: 1, e: 2 }] });
check(sent().filter((c) => c.revoked_ack).map((c) => c.revoked_ack), [[{ id, a: 1, e: 1 }]], "queued: revoked_ack");
check(sent().filter((c) => c.already_handed).map((c) => c.already_handed), [[{ id, a: 1, e: 2 }]], "handed: already_handed");
let called = false;
const pending = inboxHandOver(receiver, { id, a: 1, e: 1 }, "s1", async () => { called = true; return {}; });
await tick();
const may = sent().find((c) => c.may);
feed({ grant: { q: may.may.q, ok: true } });
await pending;
check(called, false, "a revoked attempt never reaches OpenCode");
check(sent().filter((c) => c.not_handed).length, 1, "it is reported not handed");
"#,
        );
    }

    /// Scenario: A watcher respawns while its context still holds one attempt; the new registration asks it to resume that one and another it never knew. It answers `retained` for the held attempt and `unknown` for the other, and observes the rebound delivery number the deck answers with.
    #[spec("inbox/opencode/030")]
    #[cfg(unix)]
    #[test]
    fn opencode_030_resume_answers_retained_only_what_the_context_holds() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
let live = true;
const receiver = inboxReceiver({ agent: "opencode", active: () => live, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
feed({ messages: [{ id, subject: "s", from: "x", text: "t", a: 1, e: 1, d: 1 }] });
current().emit("exit");
await new Promise((r) => setTimeout(r, 10));
// The fixed backoff is an hour: respawn by hand, as the timer would.
receiver.start();
check(current().args.includes("--resumed"), true, "a respawned watcher says its callbacks survived");
feed({ registered: { rx: 5, slot: "opencode_tui", keepalive_ms: 30000 },
  resume: [{ id, a: 1, e: 1 }, { id: "m-k3xzq2ab-0f3c9a71d2e4b851", a: 1, e: 1 }] });
const answers = sent().find((c) => c.resume_answers).resume_answers;
check(answers.map((a) => a.answer), ["retained", "unknown"], "the answers");
feed({ rebound: [{ id, a: 1, e: 1, d: 1 }] });
const last = sent().filter((c) => c.possess).pop().possess;
check(last.seen, 1, "the rebound delivery number is observed");
check(last.held.length, 1, "the retained attempt is still held");
"#,
        );
    }

    /// Scenario: A callback is approved by the deck's final hand-over check, then its watcher restarts (a rebinding) or its context is disposed (a retirement) before it resumes. The old approval is void: a rebound callback asks again under the new incarnation, and a retired one never reaches OpenCode and reports the attempt not handed.
    #[spec("inbox/opencode/031")]
    #[cfg(unix)]
    #[test]
    fn opencode_031_an_approval_dies_with_its_epoch() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
let live = true;
const receiver = inboxReceiver({ agent: "opencode", active: () => live, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = { id, a: 1, e: 1 };
feed({ messages: [{ ...ref, subject: "s", from: "x", text: "t", d: 1 }] });
const granted = receiver.may(ref);
await tick();
feed({ grant: { q: sent().find((c) => c.may).may.q, ok: true } });
const epoch = await granted;
check(receiver.ready(ref, epoch), true, "approved under the live incarnation");
receiver.start();
check(receiver.ready(ref, epoch), false, "a rebinding voids the approval");
feed({ registered: { rx: 5, slot: "opencode_tui", keepalive_ms: 30000 }, resume: [ref] });
let called = 0;
const second = inboxHandOver(receiver, ref, "s1", async () => { called += 1; return {}; });
await tick();
feed({ grant: { q: sent().filter((c) => c.may).pop().may.q, ok: true } });
await second;
check(called, 1, "asked again under the new incarnation, then handed once");
check(sent().filter((c) => c.handed).length, 1, "and reported handed");
feed({ messages: [{ id, subject: "s", from: "x", text: "t", a: 1, e: 2, d: 2 }] });
const later = { id, a: 1, e: 2 };
const third = inboxHandOver(receiver, later, "s1", async () => { called += 1; return {}; });
await tick();
live = false;
feed({ grant: { q: sent().filter((c) => c.may).pop().may.q, ok: true } });
await third;
check(called, 1, "a retired context's approved callback never reaches OpenCode");
check(sent().filter((c) => c.not_handed).map((c) => c.not_handed[0].e), [2], "it reports not handed");
"#,
        );
    }

    /// Scenario: A callback asks the deck's final hand-over check, and its watcher exits before the deck answers. The exit is not read as a refusal: nothing is reported not handed, and once the respawned watcher registers and the attempt is answered `retained`, the callback asks again under the new incarnation, reaches OpenCode once and reports the attempt handed.
    #[spec("inbox/opencode/055")]
    #[cfg(unix)]
    #[test]
    fn opencode_055_a_check_pending_across_a_watcher_restart_is_asked_again() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = { id, a: 1, e: 1 };
feed({ messages: [{ ...ref, subject: "s", from: "x", text: "t", d: 1 }] });
let called = 0;
const pending = inboxHandOver(receiver, ref, "s1", async () => { called += 1; return {}; });
await tick();
const first = current();
check(first.written.filter((c) => c.may).length, 1, "the check is asked");
first.emit("exit");
await new Promise((r) => setTimeout(r, 10));
check(first.written.filter((c) => c.not_handed).length, 0, "the watcher's exit is not a refusal");
// The fixed backoff is an hour: respawn by hand, as the timer would.
receiver.start();
feed({ registered: { rx: 5, slot: "opencode_tui", keepalive_ms: 30000 }, resume: [ref] });
check(sent().find((c) => c.resume_answers).resume_answers[0].answer, "retained", "the attempt is retained");
await tick();
const asked = sent().filter((c) => c.may);
check(asked.length, 1, "asked again under the new incarnation");
feed({ grant: { q: asked[0].may.q, ok: true } });
check(await pending, true, "handed over");
check(called, 1, "OpenCode is called once");
check(sent().filter((c) => c.not_handed).length, 0, "never reported not handed");
check(sent().filter((c) => c.handed).map((c) => c.handed), [[ref]], "reported handed");
"#,
        );
    }

    /// Scenario: An approved callback is inside its OpenCode call when the deck revokes the attempt, which is answered `already_handed`; OpenCode then refuses the call before accepting it. The attempt is not reported not handed, and a revoke sent again is answered `already_handed` once more. The same holds when the session goes idle while the call is in flight, whether before or after the revoke.
    #[spec("inbox/opencode/056")]
    #[cfg(unix)]
    #[test]
    fn opencode_056_an_answered_already_handed_is_never_contradicted() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = { id, a: 1, e: 1 };
feed({ messages: [{ ...ref, subject: "s", from: "x", text: "t", d: 1 }] });
let answer;
const inCall = new Promise((resolve) => { answer = resolve; });
const pending = inboxHandOver(receiver, ref, "s1", () => inCall);
await tick();
feed({ grant: { q: sent().find((c) => c.may).may.q, ok: true } });
await tick();
feed({ revoke: [ref] });
check(sent().filter((c) => c.already_handed).map((c) => c.already_handed), [[ref]], "answered already_handed");
answer({ error: { status: 400 }, response: { status: 400 } });
check(await pending, false, "the call was refused");
check(sent().filter((c) => c.not_handed).length, 0, "never reported not handed after already_handed");
feed({ revoke: [ref] });
check(sent().filter((c) => c.already_handed).length, 2, "a revoke sent again is answered the same");
check(sent().filter((c) => c.revoked_ack).length, 0, "and never revoked_ack");

// The session goes idle while the call is in flight, before or after the
// revoke: the idle handler's loop drops the held entry (`lost`), and the
// answer still stands (PRD #1666 r9v, reviewer R9U-02).
const idle = () => { for (const r of receiver.inSession("s1")) receiver.lost(r); };
for (const [e, order] of [[2, "revoke, idle"], [3, "idle, revoke"]]) {
  const attempt = { id, a: 1, e };
  const answered = () => sent().filter((c) => c.already_handed).flatMap((c) => c.already_handed).filter((r) => r.e === e);
  const of = (kind) => sent().filter((c) => c[kind]).flatMap((c) => c[kind]).filter((r) => r.e === e);
  feed({ messages: [{ ...attempt, subject: "s", from: "x", text: "t", d: e }] });
  let refuse;
  const inFlight = inboxHandOver(receiver, attempt, "s1", () => new Promise((r) => { refuse = r; }));
  await tick();
  feed({ grant: { q: sent().filter((c) => c.may).pop().may.q, ok: true } });
  await tick();
  if (order === "revoke, idle") {
    feed({ revoke: [attempt] });
    idle();
  } else {
    idle();
    feed({ revoke: [attempt] });
  }
  check(answered().length, 1, order + ": answered already_handed");
  refuse({ error: { status: 400 }, response: { status: 400 } });
  check(await inFlight, false, order + ": the call was refused");
  check(of("not_handed").length, 0, order + ": never reported not handed");
  feed({ revoke: [attempt] });
  check(answered().length, 2, order + ": a later revoke is answered already_handed");
  check(of("revoked_ack").length, 0, order + ": and never revoked_ack");
}
"#,
        );
    }

    /// Scenario: The deck refuses a hand-over check, or never answers it. Neither reaches OpenCode, and both are reported positively not handed, so the deck may hand the message out again, unmarked.
    #[spec("inbox/opencode/032")]
    #[cfg(unix)]
    #[test]
    fn opencode_032_a_refused_or_silent_check_hands_nothing_over() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
feed({ messages: [{ id, subject: "s", from: "x", text: "t", a: 1, e: 1, d: 1 }] });
let called = 0;
const refused = inboxHandOver(receiver, { id, a: 1, e: 1 }, "s1", async () => { called += 1; return {}; });
await tick();
feed({ grant: { q: sent().find((c) => c.may).may.q, ok: false, refusal: "committed" } });
await refused;
check(called, 0, "a refused check reaches nothing");
check(sent().filter((c) => c.not_handed).length, 1, "and is reported not handed");
"#,
        );
    }

    /// Scenario: Two hand-over checks of one attempt race. First an approved callback is already inside its OpenCode call when the other check is refused: the refusal reaches nothing and claims nothing, and the call reports handed. Then a refusal arrives before the other check's approval: the refusal revokes the attempt locally before reporting it not handed, so the late approval never reaches OpenCode. Last, two checks of one attempt are both approved: only one reaches OpenCode, the other claims nothing while that call is in flight, the call's refusal before acceptance is reported not handed, and an approval delayed past it never reaches OpenCode even when the attempt is delivered again.
    #[spec("inbox/opencode/037")]
    #[cfg(unix)]
    #[test]
    fn opencode_037_a_refusal_never_races_an_approval_into_a_false_witness() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = { id, a: 1, e: 1 };
feed({ messages: [{ ...ref, subject: "s", from: "x", text: "t", d: 1 }] });
const lastMay = () => sent().filter((c) => c.may).pop().may.q;
let called = 0;
let release;
const first = inboxHandOver(receiver, ref, "s1", () => {
  called += 1;
  return new Promise((r) => { release = r; });
});
await tick();
feed({ grant: { q: lastMay(), ok: true } });
await tick();
check(called, 1, "the approved callback is inside its call");
const second = inboxHandOver(receiver, ref, "s1", async () => { called += 1; return {}; });
await tick();
feed({ grant: { q: lastMay(), ok: false, refusal: "revoked" } });
await second;
check(called, 1, "the refused check reaches nothing");
check(sent().filter((c) => c.not_handed).length, 0, "and claims nothing while the approved call is in flight");
release({});
await first;
check(sent().filter((c) => c.handed).length, 1, "the approved call reports handed");

const later = { id, a: 1, e: 2 };
feed({ messages: [{ ...later, subject: "s", from: "x", text: "t", d: 2 }] });
const approved = inboxHandOver(receiver, later, "s1", async () => { called += 1; return {}; });
const refused = inboxHandOver(receiver, later, "s1", async () => { called += 1; return {}; });
await tick();
const [approvedQ, refusedQ] = sent().filter((c) => c.may).slice(-2).map((c) => c.may.q);
feed({ grant: { q: refusedQ, ok: false, refusal: "revoked" } });
await refused;
check(sent().filter((c) => c.not_handed).map((c) => c.not_handed[0].e), [2], "the refusal reports not handed");
feed({ grant: { q: approvedQ, ok: true } });
await approved;
check(called, 1, "an approval after the refusal's local revoke never reaches OpenCode");
check(sent().filter((c) => c.handed).length, 1, "and nothing more is reported handed");

const third = { id, a: 1, e: 3 };
const copy = { ...third, subject: "s", from: "x", text: "t", d: 3 };
const thirdNotHanded = () => sent().filter((c) => c.not_handed).flatMap((c) => c.not_handed).filter((r) => r.e === 3);
feed({ messages: [copy] });
let fail;
const one = inboxHandOver(receiver, third, "s1", () => {
  called += 1;
  return new Promise((r) => { fail = r; });
});
const other = inboxHandOver(receiver, third, "s1", async () => { called += 1; return {}; });
await tick();
for (const c of sent().filter((c) => c.may).slice(-2)) feed({ grant: { q: c.may.q, ok: true } });
await tick();
check(called, 2, "two approvals of one attempt make one native call");
check(await other, false, "the other approved check reaches nothing");
check(thirdNotHanded().length, 0, "and claims nothing while the call is in flight");
const delayed = inboxHandOver(receiver, third, "s1", async () => { called += 1; return {}; });
await tick();
fail({ error: { message: "no such session" }, response: { status: 400 } });
check(await one, false, "the one call is refused before acceptance");
check(thirdNotHanded().length, 1, "and reported not handed");
feed({ messages: [copy] });
feed({ grant: { q: sent().filter((c) => c.may).pop().may.q, ok: true } });
check(await delayed, false, "an approval delayed past the failure");
check(called, 2, "never reaches OpenCode, even with the attempt delivered again");
check(sent().filter((c) => c.handed).length, 1, "and nothing more is reported handed");
"#,
        );
    }

    /// Scenario: Three attempts are approved and their calls reach OpenCode's API: the first call throws after it began (a lost connection), the second returns an error with no HTTP status, the third returns a 400 refusal. Only the third is reported not handed; the other two are dropped from the snapshots, so the deck repeats them marked, and a later revoke of either is answered `already_handed`.
    #[spec("inbox/opencode/038")]
    #[cfg(unix)]
    #[test]
    fn opencode_038_a_call_that_fails_after_it_began_is_lost_not_refused() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = (e) => ({ id, a: 1, e });
feed({ messages: [1, 2, 3].map((e) => ({ ...ref(e), subject: "s", from: "x", text: "t", d: e })) });
const prompts = [];
const handOver = async (e, call) => {
  const handing = inboxHandOver(receiver, ref(e), "s1", async () => { prompts.push(e); return call(); });
  await tick();
  feed({ grant: { q: sent().filter((c) => c.may).pop().may.q, ok: true } });
  return handing;
};
check(await handOver(1, () => { throw new Error("socket hang up"); }), false, "a call that threw");
check(await handOver(2, () => ({ error: { message: "fetch failed" } })), false, "an error with no status");
check(await handOver(3, () => ({ error: { message: "no such session" }, response: { status: 400 } })), false, "a 400");
check(prompts, [1, 2, 3], "every call reached the API");
check(sent().filter((c) => c.not_handed).flatMap((c) => c.not_handed.map((r) => r.e)), [3],
  "only the refusal before acceptance is a not-handed witness");
check(sent().filter((c) => c.possess).pop().possess.held, [], "none is held, so the deck may repeat 1 and 2");
feed({ revoke: [ref(1), ref(2), ref(3)] });
check(sent().filter((c) => c.already_handed).flatMap((c) => c.already_handed.map((r) => r.e)), [1, 2],
  "possibly handed");
check(sent().filter((c) => c.revoked_ack).flatMap((c) => c.revoked_ack.map((r) => r.e)), [3], "never handed");
"#,
        );
    }

    /// Scenario: Two attempts reach OpenCode's API and are then lost: the first's turn is aborted (its session goes idle), the second's prompt is reverted. A late refusal path for either reports nothing; a Cancel's revoke of both is answered `already_handed`, never `revoked_ack`; and after a watcher restart the resume asks about the first and is answered `unknown`, never `not_handed`.
    #[spec("inbox/opencode/039")]
    #[cfg(unix)]
    #[test]
    fn opencode_039_a_lost_handed_attempt_stays_possibly_handed() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = (e) => ({ id, a: 1, e });
feed({ messages: [1, 2].map((e) => ({ ...ref(e), subject: "s", from: "x", text: "t", d: e })) });
receiver.calling(ref(1));
receiver.session(ref(1), "aborted");
receiver.calling(ref(2));
receiver.session(ref(2), "reverted");
// The aborted turn: its session went idle with the attempt handed.
for (const lost of receiver.inSession("aborted")) receiver.lost(lost);
// The reverted prompt: OpenCode removed its user message.
for (const lost of receiver.of(id).filter((r) => r.e === 2)) receiver.lost(lost);
check(receiver.of(id), [], "neither is held");
receiver.notHanded(ref(1));
receiver.notHanded(ref(2));
check(sent().filter((c) => c.not_handed).length, 0, "a lost handed attempt is never reported not handed");
feed({ revoke: [ref(1), ref(2)] });
check(sent().filter((c) => c.already_handed).flatMap((c) => c.already_handed.map((r) => r.e)), [1, 2],
  "answered already handed");
check(sent().filter((c) => c.revoked_ack).length, 0, "never revoked_ack");
receiver.stop();
receiver.start();
feed({ registered: { rx: 5, slot: "opencode_tui", keepalive_ms: 30000 }, resume: [ref(1)] });
check(sent().filter((c) => c.resume_answers).pop().resume_answers.map((r) => r.answer), ["unknown"],
  "a restart answers unknown");
"#,
        );
    }

    /// Scenario: The TUI plugin is handed a message for the session on screen, and the deck's approval of the hand-over takes most of a second. While it waits, the person switches to another session; in a second run, the person opens a dialog instead. In neither is the message prompted into the session it was meant for, and the plugin reports it positively not handed. Under `opencode --mini`, the root session changing during the wait does the same.
    #[spec("inbox/opencode/040")]
    #[cfg(unix)]
    #[test]
    fn opencode_040_a_slow_approval_rechecks_the_target_and_the_dialog() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        for (what, change) in [
            (
                "route",
                r#"route = { name: "session", params: { sessionID: "rootB" } };"#,
            ),
            ("dialog", "dialog = true;"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (recorder, controls) = inbox_recorder_granting_after(dir.path(), Some("0.8"));
            let plugin = dir.path().join("dot-agent-deck-tui.mjs");
            std::fs::write(&plugin, tui_plugin_template(&recorder.to_string_lossy())).unwrap();
            let driver = dir.path().join("driver.mjs");
            std::fs::write(
                &driver,
                format!(
                    r#"import fs from "fs";
{FAKE_TUI_API_JS}
const asked = () => {{ try {{ return fs.readFileSync("{controls}", "utf8").includes('"may"'); }} catch (_) {{ return false; }} }};
route = {{ name: "session", params: {{ sessionID: "rootA" }} }};
const {{ default: mod }} = await import("{plugin}");
await mod.tui(api);
for (let n = 0; n < 150 && !asked(); n++) await sleep(20);
const waited = asked();
{change}
await sleep(1500);
disposeCb?.();
console.log(JSON.stringify({{ waited, prompts }}));
process.exit(0);
"#,
                    controls = controls.display(),
                    plugin = plugin.display(),
                ),
            )
            .unwrap();
            let result = run_inbox_driver(&driver);
            assert_eq!(
                result["waited"], true,
                "{what}: the approval was asked for: {result}"
            );
            assert_eq!(result["prompts"], serde_json::json!([]), "{what}: {result}");
            let written = std::fs::read_to_string(&controls).unwrap_or_default();
            assert!(written.contains("\"not_handed\""), "{what}: {written}");
        }

        let dir = tempfile::tempdir().unwrap();
        let (recorder, controls) = inbox_recorder_granting_after(dir.path(), Some("0.8"));
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import fs from "fs";
process.argv = [process.argv[0], "opencode", "--mini"];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const asked = () => {{ try {{ return fs.readFileSync("{controls}", "utf8").includes('"may"'); }} catch (_) {{ return false; }} }};
const prompts = [];
const client = {{ session: {{
  messages: async () => ({{ data: [] }}),
  promptAsync: async (req) => {{ prompts.push(req.path.id); return {{}}; }},
}} }};
const {{ default: {{ server: plugin }} }} = await import("{plugin}");
const hooks = await plugin({{ directory: "/work", client }});
await hooks.event({{ event: {{ type: "session.created", properties: {{ info: {{ id: "s1" }} }} }} }});
for (let n = 0; n < 150 && !asked(); n++) await sleep(20);
const waited = asked();
await hooks.event({{ event: {{ type: "session.created", properties: {{ info: {{ id: "s2" }} }} }} }});
await sleep(1500);
console.log(JSON.stringify({{ waited, prompts }}));
process.exit(0);
"#,
                controls = controls.display(),
                plugin = plugin.display(),
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        assert_eq!(result["waited"], true, "mini: {result}");
        assert_eq!(result["prompts"], serde_json::json!([]), "mini: {result}");
        let written = std::fs::read_to_string(&controls).unwrap_or_default();
        assert!(written.contains("\"not_handed\""), "mini: {written}");
    }

    /// Scenario: An attempt reaches OpenCode's API and the agent starts on it, so the plugin reports it committed while the watcher's receipt publish has not finished; a Cancel's revoke then arrives. It is answered `already_handed`, never `revoked_ack`, and a late refusal path reports nothing. Then a lost attempt is followed by thousands of later attempts that are called and committed, more than the receiver remembers: a revoke of the lost one, or of one the receiver never saw, is still answered `already_handed`, and neither is reported not handed.
    #[spec("inbox/opencode/045")]
    #[cfg(unix)]
    #[test]
    fn opencode_045_a_committed_or_forgotten_attempt_stays_possibly_handed() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 30000 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = (e) => ({ id, a: 1, e });
const copy = (e) => ({ ...ref(e), subject: "s", from: "x", text: "t", d: e });
const answers = (e) => sent().flatMap((c) => [
  ...(c.already_handed ?? []).filter((r) => r.e === e).map(() => "already_handed"),
  ...(c.revoked_ack ?? []).filter((r) => r.e === e).map(() => "revoked_ack"),
]);
const notHanded = (e) => sent().flatMap((c) => c.not_handed ?? []).filter((r) => r.e === e).length;
feed({ messages: [copy(1)] });
receiver.calling(ref(1));
receiver.handed([ref(1)]);
receiver.committed([ref(1)]);
receiver.notHanded(ref(1));
check(notHanded(1), 0, "a committed attempt is never reported not handed");
feed({ revoke: [ref(1)] });
check(answers(1), ["already_handed"], "a revoke racing the commit's publication");

feed({ messages: [copy(2)] });
receiver.calling(ref(2));
receiver.lost(ref(2));
for (let e = 3; e < 3 + 5000; e++) {
  feed({ messages: [copy(e)] });
  receiver.calling(ref(e));
  receiver.committed([ref(e)]);
}
feed({ revoke: [ref(2)] });
check(answers(2), ["already_handed"], "a lost attempt the receiver no longer remembers");
receiver.notHanded(ref(2));
check(notHanded(2), 0, "and it is never reported not handed");
feed({ revoke: [ref(9000)] });
check(answers(9000), ["already_handed"], "once anything was forgotten, absence proves nothing");
receiver.notHanded(ref(9000));
check(notHanded(9000), 0, "nor does a refusal of an attempt it does not hold");
"#,
        );
    }

    /// Scenario: A receiver registers with a ten-millisecond keepalive. One hand-over check is approved and used at once, and reaches OpenCode. The next approval is buffered while the plugin's event loop stalls for longer than the deck waits before retiring a stream that closed: when the loop resumes and reads the approval, it never reaches OpenCode.
    #[spec("inbox/opencode/046")]
    #[cfg(unix)]
    #[test]
    fn opencode_046_an_approval_outlived_by_the_streams_retirement_is_void() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        run_receiver_script(
            r#"
const receiver = inboxReceiver({ agent: "opencode", active: () => true, deliver: () => {} });
receiver.start();
feed({ registered: { rx: 4, slot: "opencode_tui", keepalive_ms: 10 } });
const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
const ref = (e) => ({ id, a: 1, e });
feed({ messages: [1, 2].map((e) => ({ ...ref(e), subject: "s", from: "x", text: "t", d: e })) });
const lastMay = () => sent().filter((c) => c.may).pop().may.q;
const called = [];
const handOver = (e) => inboxHandOver(receiver, ref(e), "s1", async () => { called.push(e); return {}; });
const prompt = handOver(1);
await tick();
feed({ grant: { q: lastMay(), ok: true } });
check(await prompt, true, "an approval used at once");
const stale = handOver(2);
await tick();
const q = lastMay();
const resumes = performance.now() + 200;
while (performance.now() < resumes) {}
feed({ grant: { q, ok: true } });
check(await stale, false, "an approval read after the stall");
check(called, [1], "never reaches OpenCode");
"#,
        );
    }

    /// Hardening M2: the server plugin swaps a submitted doorbell line for the
    /// waiting content only in the managed interactive OpenCode — the full
    /// TUI's worker or `--mini`. A nested `opencode run`, `serve` or other
    /// subcommand an agent starts in its pane leaves the line alone and never
    /// fetches the pane's inbox.
    #[cfg(unix)]
    #[test]
    fn opencode_doorbell_interception_runs_only_in_the_interactive_tui() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls.log");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1 $2\" in 'inbox fetch') echo '{{\"messages\":[]}}';; 'inbox wait') echo '{{\"registered\":{{\"rx\":3,\"slot\":\"opencode_server\",\"keepalive_ms\":30000}}}}'; cat >/dev/null;; esac\nexit 0\n",
                calls.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let line = crate::doorbell::doorbell_line("m-k3xzq2ab-0f3c9a71d2e4b850");
        let line_json = serde_json::to_string(&line).unwrap();
        for (argv, intercepts) in [
            (r#"["run", "fix it"]"#, false),
            (r#"["serve"]"#, false),
            (r#"["acp"]"#, false),
            (r#"["--mini"]"#, true),
        ] {
            let _ = std::fs::remove_file(&calls);
            let driver = dir.path().join("driver.mjs");
            std::fs::write(
                &driver,
                format!(
                    r#"process.argv = [process.argv[0], "opencode", ...{argv}];
const {{ default: {{ server: plugin }} }} = await import("{}");
const hooks = await plugin({{ directory: "/work" }});
// PRD #1666 §4: the substitution needs this context's receiver registered.
await new Promise((r) => setTimeout(r, 500));
const output = {{ parts: [{{ type: "text", text: {line_json} }}] }};
await hooks["chat.message"]({{ messageID: "u1" }}, output);
console.log(JSON.stringify(output.parts[0].text));
process.exit(0);
"#,
                    plugin.display()
                ),
            )
            .unwrap();
            let out = std::process::Command::new("node")
                .arg(&driver)
                .env("DOT_AGENT_DECK_PANE_ID", "pane-1")
                .env("DOT_AGENT_DECK_PANE_CAPABILITY", "a".repeat(64))
                .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
                .output()
                .expect("run node");
            assert!(out.status.success(), "{argv}: {out:?}");
            let text: String =
                serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
            let calls = std::fs::read_to_string(&calls).unwrap_or_default();
            let fetched = calls.lines().any(|call| call.starts_with("inbox fetch"));
            assert_eq!(fetched, intercepts, "{argv}: fetch calls");
            if intercepts {
                assert!(
                    calls.contains("--adapter-integration 2 --rx 3"),
                    "{argv}: the fetch binds its attempts to the receiver: {calls}"
                );
            }
            assert_eq!(text == line, !intercepts, "{argv}: {text:?}");
        }
    }

    /// Scenario: Load the generated plugin under Node with its binary pinned to
    /// a recorder script, and send it a `session.error` whose error carries a
    /// status, a JSON body over 8 KiB, and a mix of allowed and other response
    /// headers. The recorded payload carries the typed fields, only the body's
    /// marker keys, and only the allow-listed headers, and the deck classifies
    /// it as Blocked; an error of an unexpected shape forwards no fields.
    #[cfg(unix)]
    #[spec("status/blocked/016")]
    #[test]
    fn status_blocked_016_opencode_plugin_forwards_structured_error_fields() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payloads.jsonl");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                "#!/bin/sh\ncat >> '{}'\necho >> '{}'\n",
                out.display(),
                out.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import pluginModule from "{}";
const plugin = pluginModule.server;
const hooks = await plugin({{ directory: "/work" }});
await hooks.event({{ event: {{ type: "session.error", properties: {{ sessionID: "s1", error: {{
  name: "APIError",
  data: {{ message: "The usage limit has been reached", statusCode: 429, isRetryable: true,
    responseBody: JSON.stringify({{ type: "error", error: {{ message: "x".repeat(9000),
      type: "usage_limit_reached", resets_at: 1790001000, code: 7, param: "model" }}, request_id: "r1" }}),
    responseHeaders: {{ "Retry-After": "60", "x-codex-rate-limit-reached-type": "workspace_member_usage_limit_reached",
      "anthropic-ratelimit-unified-status": "rejected", "set-cookie": "secret", "authorization": "Bearer nope",
      "x-request-id": "r1" }},
    metadata: {{ url: "https://example.invalid" }} }} }} }} }} }});
await hooks.event({{ event: {{ type: "session.error", properties: {{ sessionID: "s1", error: "just a string" }} }} }});
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let status = std::process::Command::new("node")
            .arg(&driver)
            .status()
            .expect("run node");
        assert!(status.success(), "the plugin driver failed");
        let payloads: Vec<serde_json::Value> = std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let errors: Vec<&serde_json::Value> = payloads
            .iter()
            .filter(|p| p["event"] == "session.error")
            .collect();
        assert_eq!(errors.len(), 2, "{payloads:?}");
        let structured = errors[0];
        assert_eq!(structured["error_name"], "APIError");
        for unread in ["status_code", "is_retryable"] {
            assert!(
                structured.get(unread).is_none(),
                "{unread} is forwarded but nothing reads it: {structured}"
            );
        }
        assert_eq!(
            structured["error_message"],
            "The usage limit has been reached"
        );
        assert!(
            structured.get("response_body").is_none(),
            "the raw body is not forwarded: {structured}"
        );
        // Qodo on PR #1346: the body is over 8 KiB and carries its marker after
        // the long message, so a truncated copy would have been invalid JSON.
        // Only the classifier's keys survive, each with its type checked.
        assert_eq!(
            structured["response_markers"],
            serde_json::json!({
                "type": "error",
                "error": {"type": "usage_limit_reached", "resets_at": 1790001000}
            })
        );
        let hook_input: crate::hook::OpenCodeHookInput =
            serde_json::from_value(structured.clone()).unwrap();
        assert_eq!(
            crate::hook::build_opencode_event(hook_input)
                .unwrap()
                .event_type,
            crate::event::EventType::QuotaBlocked,
            "the forwarded payload must classify as Blocked"
        );
        let headers = structured["response_headers"].as_object().unwrap();
        let mut names: Vec<&str> = headers.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "anthropic-ratelimit-unified-status",
                "retry-after",
                "x-codex-rate-limit-reached-type"
            ],
            "only allow-listed headers, lowercased"
        );
        assert!(structured.get("metadata").is_none());
        let bare = errors[1];
        for key in [
            "error_name",
            "error_message",
            "response_markers",
            "response_headers",
        ] {
            assert!(
                bare.get(key).is_none(),
                "{key} from an unexpected shape: {bare}"
            );
        }
    }

    /// PRD #1497: load the generated plugin under Node with its binary pinned to
    /// a recorder, and play it a turn the way OpenCode 1.18 publishes one — the
    /// user's message, a reasoning part, an assistant message streamed as an
    /// empty text part plus deltas and then its full text, a second assistant
    /// message after a tool step, then `session.status` idle and the deprecated
    /// `session.idle`, for a session whose `session.created` named it a root.
    /// The first idle report carries the LAST assistant
    /// message's text as `reply`, the second none; a turn that hit a
    /// `session.error` carries `reply_failed`; an interrupted turn carries none;
    /// a turn whose last assistant message has no text carries an empty
    /// `reply` on its first idle report only.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_attaches_the_last_assistant_reply_to_the_idle_report() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payloads.jsonl");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                "#!/bin/sh\ncat >> '{}'\necho >> '{}'\n",
                out.display(),
                out.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import pluginModule from "{}";
const plugin = pluginModule.server;
const hooks = await plugin({{ directory: "/work" }});
const send = (type, properties) => hooks.event({{ event: {{ type, properties }} }});
const part = (sessionID, messageID, id, type, text) =>
  send("message.part.updated", {{ sessionID, time: 1, part: {{ id, sessionID, messageID, type, text, time: {{ start: 1 }} }} }});
await send("session.created", {{ sessionID: "s1", info: {{ id: "s1", directory: "/work" }} }});
const turn = async (s, prefix, finalText) => {{
  await send("session.status", {{ sessionID: s, status: {{ type: "busy" }} }});
  await send("message.updated", {{ sessionID: s, info: {{ id: prefix + "u", sessionID: s, role: "user" }} }});
  await part(s, prefix + "u", prefix + "pu", "text", "list the files");
  await send("message.updated", {{ sessionID: s, info: {{ id: prefix + "a1", sessionID: s, role: "assistant" }} }});
  await part(s, prefix + "a1", prefix + "r", "reasoning", "thinking about it");
  await part(s, prefix + "a1", prefix + "t1", "text", "");
  await send("message.part.delta", {{ sessionID: s, messageID: prefix + "a1", partID: prefix + "t1", field: "text", delta: "Let me " }});
  await send("message.part.delta", {{ sessionID: s, messageID: prefix + "a1", partID: prefix + "t1", field: "text", delta: "look." }});
  await part(s, prefix + "a1", prefix + "t1", "text", "Let me look.");
  await send("message.updated", {{ sessionID: s, info: {{ id: prefix + "a2", sessionID: s, role: "assistant" }} }});
  await part(s, prefix + "a2", prefix + "t2", "text", "");
  await send("message.part.delta", {{ sessionID: s, messageID: prefix + "a2", partID: prefix + "t2", field: "text", delta: finalText }});
}};
await turn("s1", "m1", "Final: two files changed. 🦀");
await send("session.status", {{ sessionID: "s1", status: {{ type: "idle" }} }});
await send("session.idle", {{ sessionID: "s1" }});
await turn("s1", "m2", "I could not finish.");
await send("session.error", {{ sessionID: "s1", error: {{ name: "APIError", data: {{ message: "boom" }} }} }});
await send("session.status", {{ sessionID: "s1", status: {{ type: "idle" }} }});
await turn("s1", "m3", "half a sentence");
await send("session.error", {{ sessionID: "s1", error: {{ name: "MessageAbortedError", data: {{}} }} }});
await send("session.status", {{ sessionID: "s1", status: {{ type: "idle" }} }});
await turn("s1", "m4", "");
await send("session.status", {{ sessionID: "s1", status: {{ type: "idle" }} }});
await send("session.idle", {{ sessionID: "s1" }});
"#,
                plugin.display()
            ),
        )
        .unwrap();
        let status = std::process::Command::new("node")
            .arg(&driver)
            .status()
            .expect("run node");
        assert!(status.success(), "the plugin driver failed");
        let payloads: Vec<serde_json::Value> = std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let replies: Vec<Option<crate::daemon_protocol::FinalReply>> = payloads
            .iter()
            .filter(|p| {
                p["event"] == "session.idle"
                    || (p["event"] == "session.status" && p["status"] == "idle")
            })
            .map(crate::hook::extract_opencode_turn_reply)
            .collect();
        assert_eq!(
            replies,
            vec![
                Some(crate::daemon_protocol::FinalReply {
                    turn_id: None,
                    text: "Final: two files changed. 🦀".into(),
                    failed: false,
                }),
                None,
                Some(crate::daemon_protocol::FinalReply {
                    turn_id: None,
                    text: "I could not finish.".into(),
                    failed: true,
                }),
                None,
                // Audit A2: a turn whose last assistant message says nothing
                // is reported once, as ended with nothing to read.
                Some(crate::daemon_protocol::FinalReply {
                    turn_id: None,
                    text: String::new(),
                    failed: false,
                }),
                None,
            ],
            "{payloads:#?}"
        );
        assert!(
            payloads
                .iter()
                .filter(|p| p["event"] != "session.idle" && p["event"] != "session.status")
                .all(|p| p.get("reply").is_none()),
            "only an idle report carries a reply: {payloads:#?}"
        );
        // Every payload still decodes and builds its event.
        for payload in &payloads {
            let input: crate::hook::OpenCodeHookInput =
                serde_json::from_value(payload.clone()).unwrap();
            assert!(
                crate::hook::build_opencode_event(input).is_some(),
                "{payload}"
            );
        }
    }

    /// PRD #1666: `DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS` sets both OpenCode
    /// watchers' respawn delay, as it sets the Pi extension's
    /// (`pi_inbox_008_watcher_backoff_env_fixes_every_respawn_delay`); unset or
    /// unreadable, the delay starts at 1 s and doubles to 30 s.
    #[cfg(unix)]
    #[test]
    fn opencode_watcher_backoff_env_fixes_every_respawn_delay() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let script = format!(
            "{}\n{}",
            inbox_js(""),
            r#"
const check = (actual, expected, what) => {
  if (actual !== expected) { console.error(what + ": " + actual + " != " + expected); process.exit(1); }
};
const ENV = "DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS";
delete process.env[ENV];
check(inboxFirstBackoff(), 1000, "default first");
check(inboxNextBackoff(1000, 10), 2000, "default doubles");
check(inboxNextBackoff(20000, 10), 30000, "default caps");
check(inboxNextBackoff(8000, 31000), 1000, "default resets after a long run");
process.env[ENV] = "45000";
check(inboxFirstBackoff(), 45000, "fixed first");
check(inboxNextBackoff(45000, 10), 45000, "fixed next");
check(inboxNextBackoff(45000, 31000), 45000, "fixed after a long run");
for (const bad of ["", "0", "1.5", "soon", "3600001"]) {
  process.env[ENV] = bad;
  check(inboxFirstBackoff(), 1000, "unreadable " + bad);
}
"#
        );
        let output = std::process::Command::new("node")
            .arg("-e")
            .arg(&script)
            .output()
            .expect("run node");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// PRD #1666 final audit (OpenCode note) and review N-3: a deck prompt
    /// OpenCode removes — a revert — is no longer waiting for an answer, so
    /// its acknowledgement is dropped, and a late assistant message naming it
    /// as parent acknowledges nothing; nor does one whose parent is in another
    /// session. The record of user messages stays bounded however long the
    /// session runs.
    #[cfg(unix)]
    #[test]
    fn opencode_removed_or_foreign_parent_acknowledges_nothing() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let script = format!(
            "{}\n{}",
            inbox_js(""),
            r#"
const check = (actual, expected, what) => {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    console.error(what + ": " + JSON.stringify(actual) + " != " + JSON.stringify(expected));
    process.exit(1);
  }
};
const user = (id, sessionID) =>
  ({ type: "message.updated", properties: { info: { role: "user", id, sessionID } } });
const removed = (messageID, sessionID) =>
  ({ type: "message.removed", properties: { sessionID, messageID } });
const users = new Map();
const pending = new Map();

inboxObserveUsers(users, user("u1", "s1"));
pending.set("u1", ["m-1"]);
inboxObserveUsers(users, removed("u1", "s1"), pending);
check(pending.has("u1"), false, "a removed prompt still waits to be acknowledged");
pending.set("u1", ["m-1"]);
check(inboxCovered(users, pending, { parentID: "u1", sessionID: "s1" }), [],
  "a late answer to a removed prompt acknowledges it");

inboxObserveUsers(users, user("u2", "s2"));
pending.set("u2", ["m-2"]);
check(inboxCovered(users, pending, { parentID: "u2", sessionID: "s1" }), [],
  "an answer in another session acknowledges the prompt");
check(inboxCovered(users, pending, { parentID: "u2", sessionID: "s2" }), ["u2"],
  "the answer to a prompt still acknowledges it");

for (let i = 0; i < 4 * INBOX_MAX_TRACKED; i++) inboxObserveUsers(users, user("x" + i, "s3"));
check(users.size <= INBOX_MAX_TRACKED, true, "the user-message record grows without bound");
"#
        );
        let output = std::process::Command::new("node")
            .arg("-e")
            .arg(&script)
            .output()
            .expect("run node");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// PR #1735 review (4309348163): a prompt commits the deck messages whose
    /// blocks it carries, not one whose opener a trusted body quotes inside
    /// its own block. An unclosed block still names its id.
    #[cfg(unix)]
    #[test]
    fn opencode_an_opener_quoted_inside_a_block_names_no_message() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let script = format!(
            "{}\n{}",
            inbox_js(""),
            r#"
const check = (actual, expected, what) => {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    console.error(what + ": " + JSON.stringify(actual) + " != " + JSON.stringify(expected));
    process.exit(1);
  }
};
const A = "m-k3xzq2ab-0000000000000001";
const B = "m-k3xzq2ab-0000000000000002";
const C = "m-k3xzq2ab-0000000000000003";
const block = (id, body) =>
  "[dot-agent-deck message " + id + " from x]\n" + body + "\n[end of dot-agent-deck message " + id + "]";
check(inboxIdsIn(INBOX_FRAMING + "\n\n" + block(A, "Forward this:\n" + block(B, "work")) + "\n\n" + block(C, "more")),
  [A, C], "a quoted block names a message");
check(inboxIdsIn("[dot-agent-deck message " + A + " from x]\nhead"), [A], "an unclosed block still names its id");
"#
        );
        let output = std::process::Command::new("node")
            .arg("-e")
            .arg(&script)
            .output()
            .expect("run node");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Run `body` under Node against the generated plugin, its binary pinned to
    /// a recorder, and answer the payloads the plugin sent — or `None` (after a
    /// `SKIP:`) when Node is not available. `node_args` go before the driver.
    #[cfg(unix)]
    pub(super) fn run_plugin_driver(
        body: &str,
        node_args: &[&str],
    ) -> Option<Vec<serde_json::Value>> {
        run_plugin_driver_with_awaits(body, node_args).map(|(payloads, _)| payloads)
    }

    /// [`run_plugin_driver`], also answering what the plugin handed
    /// `await-answer` (PRD #1631): its recorder reads that first stdin line,
    /// keeps it apart from the plain reports and exits without an answer.
    /// The driver gets `awaited(n)`, which waits until `n` have been handed
    /// over, so a later event cannot stop a child before it has read.
    #[cfg(unix)]
    fn run_plugin_driver_with_awaits(
        body: &str,
        node_args: &[&str],
    ) -> Option<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
        run_plugin_driver_under(body, node_args, None)
    }

    /// A copy of `/bin/sh` named `opencode` in `dir`, so a process it runs
    /// has an executable (and a first argument) named like an OpenCode binary,
    /// as the 2.x plugin's parent check reads it (audit L5). Copied by a `cp`
    /// child, so this process never holds the file open for writing when a
    /// test execs it ([`crate::test_isolation::write_script`] says why).
    #[cfg(unix)]
    fn opencode_named_shell(dir: &Path) -> PathBuf {
        let shell = dir.join("opencode");
        if !shell.exists() {
            let status = std::process::Command::new("cp")
                .arg("/bin/sh")
                .arg(&shell)
                .status()
                .expect("run cp");
            assert!(status.success(), "copy /bin/sh to {}", shell.display());
        }
        shell
    }

    /// A command running `node <driver>` as the child of a shell whose
    /// executable is named `opencode` (the full TUI's client): the parent a
    /// 2.x `serve` process must have for its `setup` to report (audit L5).
    #[cfg(unix)]
    fn node_under_opencode(driver: &Path) -> std::process::Command {
        let dir = driver.parent().expect("the driver is in a directory");
        std::fs::write(dir.join("client"), format!("node '{}'\n", driver.display())).unwrap();
        let mut command = std::process::Command::new(opencode_named_shell(dir));
        command
            .arg("client")
            .current_dir(dir)
            .envs(crate::test_isolation::unreachable_endpoints());
        command
    }

    /// [`run_plugin_driver_with_awaits`], with node started as the child of a
    /// shell whose arguments read `opencode <client_args>` when `client_args`
    /// is given: the parent a 2.x `serve` process reads its client from.
    #[cfg(unix)]
    fn run_plugin_driver_under(
        body: &str,
        node_args: &[&str],
        client_args: Option<&str>,
    ) -> Option<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payloads.jsonl");
        let awaits = dir.path().join("awaits.jsonl");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"await-answer\" ]; then\n  IFS= read -r input\n  printf '%s\\n' \"$input\" >> '{awaits}'\n  exit 0\nfi\ncat >> '{out}'\necho >> '{out}'\n",
                awaits = awaits.display(),
                out = out.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                "import pluginModule from {plugin};\nconst plugin = pluginModule.server;\nimport {{ existsSync as awaitsExist, readFileSync as readAwaits }} from \"fs\";\nconst awaited = async (n) => {{\n  for (let i = 0; i < 1600; i += 1) {{\n    if (awaitsExist({awaits}) && readAwaits({awaits}, \"utf8\").split(\"\\n\").filter(Boolean).length >= n) return;\n    await new Promise((r) => setTimeout(r, 25));\n  }}\n  throw new Error(\"await-answer was not handed \" + n);\n}};\n{body}",
                plugin = serde_json::to_string(&plugin.display().to_string()).unwrap(),
                awaits = serde_json::to_string(&awaits.display().to_string()).unwrap(),
            ),
        )
        .unwrap();
        let output = match client_args {
            None => std::process::Command::new("node")
                .args(node_args)
                .arg(&driver)
                .output(),
            Some(client_args) => {
                // The shell reads its script from a file named after the
                // client's first word, so its arguments read `opencode
                // <client_args>` (`client` stands in for none: the full TUI).
                let (first, rest) = client_args.split_once(' ').unwrap_or((client_args, ""));
                let first = if first.is_empty() { "client" } else { first };
                let mut line = String::from("node");
                for arg in node_args.iter().copied().chain([driver.to_str().unwrap()]) {
                    line.push_str(&format!(" '{arg}'"));
                }
                std::fs::write(dir.path().join(first), format!("{line}\n")).unwrap();
                std::process::Command::new(opencode_named_shell(dir.path()))
                    .arg(first)
                    .args(rest.split_whitespace())
                    .current_dir(dir.path())
                    .envs(crate::test_isolation::unreachable_endpoints())
                    .output()
            }
        }
        .expect("run node");
        assert!(
            output.status.success(),
            "the plugin driver failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let read = |path: &Path| -> Vec<serde_json::Value> {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).expect("every payload is JSON the deck decodes"))
                .collect()
        };
        Some((read(&out), read(&awaits)))
    }

    /// The final replies the plugin attached to its idle reports, in order,
    /// paired with the session the report names.
    #[cfg(unix)]
    fn idle_replies(
        payloads: &[serde_json::Value],
    ) -> Vec<(String, Option<crate::daemon_protocol::FinalReply>)> {
        payloads
            .iter()
            .filter(|p| {
                p["event"] == "session.idle"
                    || (p["event"] == "session.status" && p["status"] == "idle")
            })
            .map(|p| {
                (
                    p["session_id"].as_str().unwrap_or_default().to_owned(),
                    crate::hook::extract_opencode_turn_reply(p),
                )
            })
            .collect()
    }

    /// PRD #1497 audit AU-B1: a subagent's child session (`parentID` naming
    /// the root, same directory) streams its own assistant text interleaved
    /// with the root's turn and goes idle first. Its text never reaches any
    /// report — its going idle is a `subagent.stop`, not an idle report of the
    /// root's (issue #1678) — and the root's own final reply still arrives. A session
    /// whose ancestry no session event named is asked of OpenCode's client:
    /// a root's reply is sent, a child's or an unanswerable one's is not.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_never_attaches_a_subagent_reply_to_the_root() {
        let Some(payloads) = run_plugin_driver(
            r#"
const client = { session: { get: async ({ path }) => {
  if (path.id === "lookup-fails") throw new Error("offline");
  return { data: path.id === "resumed" ? { id: "resumed" } : { id: path.id, parentID: "root" } };
} } };
const hooks = await plugin({ directory: "/work", client });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const part = (sessionID, messageID, id, text) =>
  send("message.part.updated", { sessionID, part: { id, sessionID, messageID, type: "text", text } });
const delta = (sessionID, messageID, partID, d) =>
  send("message.part.delta", { sessionID, messageID, partID, field: "text", delta: d });
const assistant = (sessionID, id) =>
  send("message.updated", { sessionID, info: { id, sessionID, role: "assistant" } });
await send("session.created", { sessionID: "root", info: { id: "root", directory: "/work" } });
await send("session.status", { sessionID: "root", status: { type: "busy" } });
await send("message.updated", { sessionID: "root", info: { id: "ru", sessionID: "root", role: "user" } });
await part("root", "ru", "rpu", "run the subagent");
await assistant("root", "ra1");
await part("root", "ra1", "rt1", "Delegating.");
await send("session.created", { sessionID: "child", info: { id: "child", parentID: "root", directory: "/work" } });
await send("session.status", { sessionID: "child", status: { type: "busy" } });
await assistant("child", "ca1");
await part("child", "ca1", "ct1", "");
await delta("child", "ca1", "ct1", "SUBAGENT_PRIVATE_SENTINEL ");
await assistant("root", "ra2");
await part("root", "ra2", "rt2", "");
await delta("child", "ca1", "ct1", "more SUBAGENT_PRIVATE_SENTINEL");
await part("child", "ca1", "ct1", "SUBAGENT_PRIVATE_SENTINEL final");
await send("session.status", { sessionID: "child", status: { type: "idle" } });
await send("session.idle", { sessionID: "child" });
await delta("root", "ra2", "rt2", "ROOT_FINAL_SENTINEL: done.");
await send("session.status", { sessionID: "root", status: { type: "idle" } });
for (const id of ["resumed", "lookup-child", "lookup-fails"]) {
  await assistant(id, id + "-a");
  await part(id, id + "-a", id + "-t", "LOOKUP_REPLY " + id);
  await send("session.status", { sessionID: id, status: { type: "idle" } });
}
"#,
            &[],
        ) else {
            return;
        };
        let all = serde_json::to_string(&payloads).unwrap();
        assert!(
            !all.contains("SUBAGENT_PRIVATE_SENTINEL"),
            "a subagent's reply leaked into a report: {payloads:#?}"
        );
        let replies: Vec<(String, Option<String>)> = idle_replies(&payloads)
            .into_iter()
            .map(|(session, reply)| (session, reply.map(|r| r.text)))
            .collect();
        assert_eq!(
            replies,
            vec![
                (
                    "root".to_owned(),
                    Some("ROOT_FINAL_SENTINEL: done.".to_owned())
                ),
                ("root".to_owned(), Some("LOOKUP_REPLY resumed".to_owned())),
                ("root".to_owned(), None),
            ],
            "a child's going idle — the known child's, and the one the client confirmed a child \
             before it went idle — is no idle report of the root's (issue #1678), the root's \
             carries its own reply, and only the client-confirmed root is read: {payloads:#?}"
        );
    }

    /// Scenario: Drive the generated OpenCode plugin under node with a root session that starts a child session (and a grandchild), as OpenCode's `task` tool does. The child's lifecycle reaches the deck as one `subagent.start` when it goes busy and one `subagent.stop` when it goes idle, fails or is deleted, filed under the root with the child's id; its creation, its prompt and its other session events are not sent as the root's, and its tool calls carry the child's id.
    #[cfg(unix)]
    #[spec("status/background/011")]
    #[test]
    fn background_011_opencode_plugin_pairs_a_child_sessions_lifecycle() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const status = (sessionID, type) => send("session.status", { sessionID, status: { type } });
await send("session.created", { sessionID: "root", info: { id: "root", directory: "/work" } });
await status("root", "busy");
await send("session.created", { sessionID: "child", info: { id: "child", parentID: "root", directory: "/work" } });
await send("session.updated", { sessionID: "child", info: { id: "child", parentID: "root", directory: "/work" } });
await send("message.updated", { sessionID: "child", info: { id: "cu", sessionID: "child", role: "user" } });
await send("message.part.updated", { sessionID: "child", part: { id: "cp", sessionID: "child", messageID: "cu", type: "text", text: "CHILD_PROMPT_SENTINEL" } });
await status("child", "busy");
await status("child", "retry");
await status("child", "busy");
await hooks["tool.execute.before"]({ sessionID: "child", tool: "bash" }, { args: { command: "sleep 60" } });
await hooks["tool.execute.after"]({ sessionID: "child", tool: "bash", args: { command: "sleep 60" } });
await send("session.created", { sessionID: "grandchild", info: { id: "grandchild", parentID: "child", directory: "/work" } });
await status("grandchild", "busy");
await status("root", "idle");
await status("child", "idle");
await send("session.idle", { sessionID: "child" });
await status("child", "busy");
await send("session.error", { sessionID: "child", error: { name: "APIError", data: { message: "boom" } } });
await status("child", "idle");
await send("session.deleted", { sessionID: "child", info: { id: "child", parentID: "root", directory: "/work" } });
await send("session.deleted", { sessionID: "grandchild", info: { id: "grandchild", parentID: "child", directory: "/work" } });
await status("root", "busy");
"#,
            &[],
        ) else {
            return;
        };
        let all = serde_json::to_string(&payloads).unwrap();
        assert!(
            !all.contains("CHILD_PROMPT_SENTINEL"),
            "a child session's prompt reached the deck as the user's: {payloads:#?}"
        );
        let seen: Vec<(String, String, String)> = payloads
            .iter()
            .map(|p| {
                let field = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
                (field("session_id"), field("event"), field("subagent_id"))
            })
            .collect();
        let row = |session: &str, event: &str, subagent: &str| {
            (session.to_owned(), event.to_owned(), subagent.to_owned())
        };
        assert_eq!(
            seen,
            vec![
                row("root", "session.created", ""),
                row("root", "session.status", ""),
                row("root", "subagent.start", "child"),
                row("root", "tool.execute.before", "child"),
                row("root", "tool.execute.after", "child"),
                row("root", "subagent.start", "grandchild"),
                row("root", "session.status", ""),
                row("root", "subagent.stop", "child"),
                row("root", "subagent.start", "child"),
                row("root", "subagent.stop", "child"),
                row("root", "subagent.stop", "grandchild"),
                row("root", "session.status", ""),
                // The plugin's own flush as node exits.
                row("root", "session.deleted", ""),
            ],
            "{payloads:#?}"
        );
        for payload in &payloads {
            let event = crate::hook::build_opencode_event(
                serde_json::from_value(payload.clone()).expect("the deck decodes every payload"),
            )
            .expect("every payload maps to an event");
            assert_eq!(
                event.is_from_subagent(),
                payload.get("subagent_id").is_some(),
                "{payload}"
            );
        }
    }

    /// `(session_id, event, subagent_id)` of each payload, for comparing a
    /// driver's whole output at once.
    #[cfg(unix)]
    fn payload_rows(payloads: &[serde_json::Value]) -> Vec<(String, String, String)> {
        payloads
            .iter()
            .map(|p| {
                let field = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
                (field("session_id"), field("event"), field("subagent_id"))
            })
            .collect()
    }

    #[cfg(unix)]
    fn row(session: &str, event: &str, subagent: &str) -> (String, String, String) {
        (session.to_owned(), event.to_owned(), subagent.to_owned())
    }

    /// Issue #1678 audit AU-1: a child that is running while 256 other child
    /// sessions are created (pushing its ancestry out of the plugin's bounded
    /// record) still reports as the subagent it is. Its tool calls keep its
    /// id, its going idle is the stop paired with its start (once, though it
    /// goes idle twice), and nothing is left active for the exit flush to stop.
    /// A grandchild it starts afterwards is filed under the root too.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_keeps_an_active_childs_route_when_its_ancestry_is_forgotten() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const status = (sessionID, type) => send("session.status", { sessionID, status: { type } });
const created = (id, parentID) => send("session.created", { sessionID: id, info: { id, parentID, directory: "/work" } });
await created("root");
await status("root", "busy");
await created("child", "root");
await status("child", "busy");
for (let i = 0; i < 256; i += 1) {
  await created("churn-" + i, "root");
}
await hooks["tool.execute.before"]({ sessionID: "child", tool: "bash" }, { args: { command: "ls" } });
await hooks["tool.execute.after"]({ sessionID: "child", tool: "bash", args: { command: "ls" } });
await created("grandchild", "child");
await status("grandchild", "busy");
await status("child", "idle");
await send("session.idle", { sessionID: "child" });
await status("grandchild", "idle");
"#,
            &[],
        ) else {
            return;
        };
        assert_eq!(
            payload_rows(&payloads),
            vec![
                row("root", "session.created", ""),
                row("root", "session.status", ""),
                row("root", "subagent.start", "child"),
                row("root", "tool.execute.before", "child"),
                row("root", "tool.execute.after", "child"),
                row("root", "subagent.start", "grandchild"),
                row("root", "subagent.stop", "child"),
                row("root", "subagent.stop", "grandchild"),
                // The plugin's own flush as node exits: no child is left to stop.
                row("root", "session.deleted", ""),
            ],
            "{payloads:#?}"
        );
    }

    /// Issue #1678 audit AU-2: a known child's permission prompt and its answer
    /// are filed under the root with the child's id — decoded as that
    /// subagent's `PermissionRequest` and its answered prompt — while the
    /// root's own prompt carries no subagent attribution. PRD #1631: both asks
    /// go to `await-answer`, so the deck can answer either; the child's names
    /// its subagent and becomes that subagent's question, and the child's
    /// reply names the request it answers. A child's ask with no request id,
    /// which the deck could not answer, is still reported, plainly.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_attributes_a_childs_permission_prompt_to_its_subagent() {
        let Some((payloads, awaits)) = run_plugin_driver_with_awaits(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
await send("session.created", { sessionID: "root", info: { id: "root", directory: "/work" } });
await send("session.created", { sessionID: "child", info: { id: "child", parentID: "root", directory: "/work" } });
await send("session.status", { sessionID: "child", status: { type: "busy" } });
await send("permission.asked", { sessionID: "child", id: "per_child", permission: "bash", patterns: ["touch c"], title: "CHILD_ASK" });
await awaited(1);
await send("permission.replied", { sessionID: "child", requestID: "per_child", reply: "once" });
await send("permission.asked", { sessionID: "root", id: "per_root", permission: "bash", patterns: ["touch r"], title: "ROOT_ASK" });
await awaited(2);
await send("permission.asked", { sessionID: "child", title: "UNHELD_ASK" });
"#,
            &[],
        ) else {
            return;
        };
        assert_eq!(
            payload_rows(&payloads),
            vec![
                row("root", "session.created", ""),
                row("root", "subagent.start", "child"),
                row("root", "permission.replied", "child"),
                row("root", "permission.asked", "child"),
                row("root", "subagent.stop", "child"),
                row("root", "session.deleted", ""),
            ],
            "{payloads:#?}"
        );
        let decoded: Vec<crate::event::AgentEvent> = payloads
            .iter()
            .map(|p| {
                crate::hook::build_opencode_event(serde_json::from_value(p.clone()).unwrap())
                    .expect("every payload maps to an event")
            })
            .collect();
        assert!(decoded[2].is_subagent_permission_reply());
        assert_eq!(decoded[2].resolved_question_id(), Some("per_child"));
        assert_eq!(payloads[3]["prompt"], "UNHELD_ASK");
        assert!(payloads[3].get("properties").is_none(), "{:?}", payloads[3]);
        assert_eq!(
            decoded[3].event_type,
            crate::event::EventType::PermissionRequest
        );
        assert!(decoded[3].is_from_subagent());
        assert_eq!(decoded[3].question(), None);

        // What each `await-answer` was handed, in whichever order the two
        // children read it.
        let asked = |id: &str| {
            let payload = awaits
                .iter()
                .find(|p| p["properties"]["id"] == id)
                .unwrap_or_else(|| panic!("{id} was not held: {awaits:#?}"));
            let (event, question) = crate::hook::build_opencode_event_with_question(
                serde_json::from_value(payload.clone()).unwrap(),
            )
            .expect("an event");
            (payload.clone(), event, question.expect("a question"))
        };
        assert_eq!(awaits.len(), 2, "{awaits:#?}");
        let (child, child_event, child_question) = asked("per_child");
        assert_eq!(
            (
                &child["session_id"],
                &child["subagent_id"],
                &child["prompt"]
            ),
            (
                &serde_json::Value::from("root"),
                &serde_json::Value::from("child"),
                &serde_json::Value::from("CHILD_ASK")
            )
        );
        assert_eq!(
            child_event.event_type,
            crate::event::EventType::PermissionRequest
        );
        assert!(child_event.is_from_subagent());
        assert_eq!(child_question.id, "per_child");
        assert_eq!(child_question.subagent_id.as_deref(), Some("child"));
        let (root, root_event, root_question) = asked("per_root");
        assert_eq!(root["session_id"], "root");
        assert!(root.get("subagent_id").is_none(), "{root}");
        assert!(!root_event.is_from_subagent());
        assert_eq!(root_question.subagent_id, None);
    }

    /// Issue #1678 audit AU-3: a child's user message recorded before its
    /// `session.created` named its parent, whose text arrives afterwards, is
    /// the prompt its subagent was given and is not reported as the user's.
    /// The root's own prompt still is.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_drops_a_childs_prompt_recorded_before_its_ancestry() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const user = (sessionID, id) => send("message.updated", { sessionID, info: { id, sessionID, role: "user", directory: "/work" } });
const text = (sessionID, messageID, t) => send("message.part.updated", { sessionID, part: { id: messageID + "-p", sessionID, messageID, type: "text", text: t } });
await send("session.created", { sessionID: "root", info: { id: "root", directory: "/work" } });
await user("child", "cu");
await send("session.created", { sessionID: "child", info: { id: "child", parentID: "root", directory: "/work" } });
await text("child", "cu", "CHILD_INTERNAL_PROMPT_SENTINEL");
await user("late-child", "lu");
await send("session.updated", { sessionID: "late-child", info: { id: "late-child", parentID: "root", directory: "/work" } });
await text("late-child", "lu", "LATE_CHILD_PROMPT_SENTINEL");
await user("root", "ru");
await text("root", "ru", "ROOT_PROMPT");
"#,
            &[],
        ) else {
            return;
        };
        let all = serde_json::to_string(&payloads).unwrap();
        assert!(
            !all.contains("PROMPT_SENTINEL"),
            "a child's prompt was reported as the user's: {payloads:#?}"
        );
        let prompts: Vec<&str> = payloads
            .iter()
            .filter(|p| p["event"] == "session.prompt")
            .map(|p| p["prompt"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(prompts, vec!["ROOT_PROMPT"], "{payloads:#?}");
    }

    /// Issue #1678 audit AU-4: a child session id over 256 UTF-8 bytes (by
    /// length, and by multi-byte characters under 256 characters) is never
    /// reported active and never sent, yet its tool call and permission prompt
    /// still carry subagent attribution — each such child its own stand-in.
    /// Another long id sharing all but its last character is not taken for
    /// that child. A 256-byte id is a child like any other. At most 64 user
    /// messages wait for their text: the oldest one's prompt is then not
    /// reported, the newest one's is, and one sent from a session id over the
    /// bound is not reported (audit AU-8).
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_bounds_the_ids_and_messages_it_holds() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const status = (sessionID, type) => send("session.status", { sessionID, status: { type } });
const created = (id, parentID) => send("session.created", { sessionID: id, info: { id, parentID, directory: "/work" } });
const tool = (sessionID) => hooks["tool.execute.before"]({ sessionID, tool: "bash" }, { args: { command: "ls" } });
await created("root");
const long = "x".repeat(299) + "a";
const wide = "é".repeat(129);
const fits = "y".repeat(256);
for (const id of [long, wide, fits]) {
  await created(id, "root");
  await status(id, "busy");
  await tool(id);
  await send("permission.asked", { sessionID: id, title: "ask" });
  await status(id, "idle");
}
await tool("x".repeat(299) + "b");
for (let i = 0; i < 65; i += 1) {
  await send("message.updated", { sessionID: "root", info: { id: "m" + i, sessionID: "root", role: "user" } });
}
await send("message.updated", { sessionID: "root", info: { id: "mz", sessionID: "z".repeat(300), role: "user" } });
for (const i of [0, 64]) {
  await send("message.part.updated", { sessionID: "root", part: { id: "p" + i, sessionID: "root", messageID: "m" + i, type: "text", text: "PROMPT_" + i } });
}
await send("message.part.updated", { sessionID: "root", part: { id: "pz", sessionID: "root", messageID: "mz", type: "text", text: "OVERSIZED_SESSION_PROMPT" } });
"#,
            &[],
        ) else {
            return;
        };
        let fits = "y".repeat(256);
        // Each oversized child is attributed to its own stand-in.
        let stand_ins: Vec<String> = payloads
            .iter()
            .filter_map(|p| p["subagent_id"].as_str())
            .filter(|id| id.starts_with("oversized-id:"))
            .map(str::to_owned)
            .collect();
        assert_eq!(stand_ins.len(), 4, "{payloads:#?}");
        assert_eq!(stand_ins[0], stand_ins[1], "one child, one stand-in");
        assert_eq!(stand_ins[2], stand_ins[3], "one child, one stand-in");
        assert_ne!(stand_ins[0], stand_ins[2], "two children stay apart");
        let expected = {
            let mut rows = vec![row("root", "session.created", "")];
            for oversized in [&stand_ins[0], &stand_ins[2]] {
                rows.push(row("root", "tool.execute.before", oversized));
                rows.push(row("root", "permission.asked", oversized));
            }
            rows.extend([
                row("root", "subagent.start", &fits),
                row("root", "tool.execute.before", &fits),
                row("root", "permission.asked", &fits),
                row("root", "subagent.stop", &fits),
                row("root", "tool.execute.before", ""),
                row("root", "session.prompt", ""),
                row("root", "session.deleted", ""),
            ]);
            rows
        };
        assert_eq!(payload_rows(&payloads), expected, "{payloads:#?}");
        let longest = payloads
            .iter()
            .flat_map(|p| p.as_object().unwrap().values())
            .filter_map(|v| v.as_str())
            .map(str::len)
            .max();
        assert_eq!(longest, Some(256), "no field carries an id over the bound");
        let prompts: Vec<_> = payloads
            .iter()
            .filter(|p| p["event"] == "session.prompt")
            .map(|p| &p["prompt"])
            .collect();
        assert_eq!(
            prompts,
            vec![&serde_json::Value::from("PROMPT_64")],
            "neither the oldest pending prompt nor one from an oversized session is reported"
        );
        for payload in &payloads {
            let event = crate::hook::build_opencode_event(
                serde_json::from_value(payload.clone()).expect("the deck decodes every payload"),
            )
            .expect("every payload maps to an event");
            assert_eq!(
                event.is_from_subagent(),
                payload.get("subagent_id").is_some(),
                "{payload}"
            );
        }
    }

    /// Issue #1678 audit AU-5: two child sessions whose ids are over the bound
    /// both ask for permission after the root's turn ended, and only the first
    /// is answered. Each prompt is attributed to its own child, so the card,
    /// fed those reports through the deck's hook decoder, stays on Needs Input
    /// until the second is answered too, then returns to Idle.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_keeps_two_oversized_childrens_prompts_apart() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const created = (id, parentID) => send("session.created", { sessionID: id, info: { id, parentID, directory: "/work" } });
await created("root");
await send("session.status", { sessionID: "root", status: { type: "busy" } });
await send("session.status", { sessionID: "root", status: { type: "idle" } });
const first = "x".repeat(299) + "a";
const second = "x".repeat(299) + "b";
for (const id of [first, second]) {
  await created(id, "root");
  await send("session.status", { sessionID: id, status: { type: "busy" } });
  await send("permission.asked", { sessionID: id, title: "ask" });
}
await send("permission.replied", { sessionID: first });
await send("permission.replied", { sessionID: second });
"#,
            &[],
        ) else {
            return;
        };
        let rows = payload_rows(&payloads);
        let asks: Vec<&(String, String, String)> = rows
            .iter()
            .filter(|(_, event, _)| event.starts_with("permission."))
            .collect();
        assert_eq!(asks.len(), 4, "{payloads:#?}");
        let (first, second) = (&asks[0].2, &asks[1].2);
        assert!(first.starts_with("oversized-id:"), "{first}");
        assert!(second.starts_with("oversized-id:"), "{second}");
        assert_ne!(first, second, "two oversized children stay apart");
        assert_eq!(
            asks.iter()
                .map(|(session, event, id)| (session.as_str(), event.as_str(), id))
                .collect::<Vec<_>>(),
            vec![
                ("root", "permission.asked", first),
                ("root", "permission.asked", second),
                ("root", "permission.replied", first),
                ("root", "permission.replied", second),
            ]
        );
        assert!(
            !rows
                .iter()
                .any(|(_, event, _)| event.starts_with("subagent.")),
            "an oversized child is never reported active: {payloads:#?}"
        );

        let mut state = crate::state::AppState::default();
        let mut statuses = Vec::new();
        for payload in &payloads {
            let event = crate::hook::build_opencode_event(
                serde_json::from_value(payload.clone()).expect("the deck decodes every payload"),
            )
            .expect("every payload maps to an event");
            let is_permission = payload["event"]
                .as_str()
                .is_some_and(|event| event.starts_with("permission."));
            state.apply_event(event);
            if is_permission {
                statuses.push(state.sessions["root"].status.clone());
            }
        }
        use crate::state::SessionStatus;
        assert_eq!(
            statuses,
            vec![
                SessionStatus::WaitingForInput,
                SessionStatus::WaitingForInput,
                SessionStatus::WaitingForInput,
                SessionStatus::Idle,
            ],
            "answering one oversized child's prompt leaves the other's open"
        );
    }

    /// Issue #1678 review #2: OpenCode disposing its instance while a child is
    /// still running stops that child, under the session its start was filed
    /// under, before the root's session closes; the exit flush that follows
    /// stops nothing twice.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_stops_its_active_children_when_disposed() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
await send("session.created", { sessionID: "root", info: { id: "root", directory: "/work" } });
await send("session.status", { sessionID: "root", status: { type: "busy" } });
await send("session.created", { sessionID: "child", info: { id: "child", parentID: "root", directory: "/work" } });
await send("session.status", { sessionID: "child", status: { type: "busy" } });
await send("session.status", { sessionID: "root", status: { type: "idle" } });
await send("server.instance.disposed", {});
"#,
            &[],
        ) else {
            return;
        };
        assert_eq!(
            payload_rows(&payloads),
            vec![
                row("root", "session.created", ""),
                row("root", "session.status", ""),
                row("root", "subagent.start", "child"),
                row("root", "session.status", ""),
                row("root", "subagent.stop", "child"),
                row("root", "session.deleted", ""),
            ],
            "{payloads:#?}"
        );
    }

    /// PRD #1497 review: OpenCode's client never answers the ancestry lookup.
    /// Each idle report of a session no event named is sent at once, without
    /// a reply, instead of waiting on the lookup; the session is asked once
    /// however many turns it ends, at most `MAX_ANCESTRY_LOOKUPS` (8) lookups
    /// are outstanding across twenty such sessions, every one is aborted
    /// through its `signal` after `ANCESTRY_LOOKUP_MS`, and once they have
    /// settled the session can be asked again.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_ancestry_lookup_never_delays_the_idle_report_and_is_bounded() {
        let Some(payloads) = run_plugin_driver(
            r#"
const calls = [];
let aborted = 0;
const client = { session: { get: ({ path, signal }) => {
  calls.push(path.id);
  return new Promise((_, reject) => {
    signal?.addEventListener("abort", () => { aborted += 1; reject(new Error("aborted")); });
  });
} } };
const hooks = await plugin({ directory: "/work", client });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const assistant = (sessionID, id) =>
  send("message.updated", { sessionID, info: { id, sessionID, role: "assistant" } });
const part = (sessionID, messageID, id, text) =>
  send("message.part.updated", { sessionID, part: { id, sessionID, messageID, type: "text", text } });
for (let turn = 0; turn < 3; turn++) {
  await assistant("hang", "hang-a" + turn);
  await part("hang", "hang-a" + turn, "hang-t" + turn, "HANG_REPLY");
  const started = Date.now();
  await send("session.status", { sessionID: "hang", status: { type: "idle" } });
  const took = Date.now() - started;
  if (took > 1000) throw new Error("the idle report waited " + took + " ms for the lookup");
}
const asked = (id) => calls.filter((c) => c === id).length;
if (asked("hang") !== 1) throw new Error("hang asked " + asked("hang") + " times: " + calls);
for (let i = 0; i < 20; i++) {
  await assistant("many" + i, "many-a" + i);
}
if (calls.length !== 8) throw new Error(calls.length + " lookups outstanding: " + calls);
await new Promise((resolve) => setTimeout(resolve, 2600));
if (aborted !== 8) throw new Error(aborted + " of 8 lookups aborted");
await assistant("hang", "hang-again");
if (asked("hang") !== 2) throw new Error("hang not asked again: " + calls);
"#,
            &[],
        ) else {
            return;
        };
        let replies = idle_replies(&payloads);
        assert_eq!(replies.len(), 3, "{payloads:#?}");
        assert!(
            replies.iter().all(|(_, reply)| reply.is_none()),
            "{payloads:#?}"
        );
    }

    /// PRD #1497 audit: OpenCode's client IGNORES the abort signal, so eight
    /// lookups stay unsettled past `ANCESTRY_LOOKUP_MS` although every signal
    /// fired. More assistant messages, idle reports, a delete and a recreated
    /// session under the same id start no ninth request until one original
    /// promise settles, and every idle report is still sent at once. The
    /// deleted session's stale "root" answer, settling while the recreated
    /// session's own lookup is in flight, does not make the recreated session
    /// read; that lookup's answer does. After a dispose, the remaining stale
    /// answers mark nothing.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_ancestry_lookup_stays_bounded_when_the_client_ignores_abort() {
        let Some(payloads) = run_plugin_driver(
            r#"
const calls = [];
const client = { session: { get: ({ path, signal }) => {
  const call = { id: path.id, signal };
  calls.push(call);
  return new Promise((resolve) => { call.settle = resolve; });
} } };
const hooks = await plugin({ directory: "/work", client });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const assistant = (sessionID, id) =>
  send("message.updated", { sessionID, info: { id, sessionID, role: "assistant" } });
const part = (sessionID, messageID, id, text) =>
  send("message.part.updated", { sessionID, part: { id, sessionID, messageID, type: "text", text } });
const settle = () => new Promise((resolve) => setTimeout(resolve, 20));
const expectCalls = (n, why) => {
  if (calls.length !== n) throw new Error(why + ": " + calls.length + " requests: " + calls.map((c) => c.id));
};
const idle = async (s) => {
  const started = Date.now();
  await send("session.status", { sessionID: s, status: { type: "idle" } });
  const took = Date.now() - started;
  if (took > 1000) throw new Error("the idle report for " + s + " waited " + took + " ms");
};
const turn = async (s, m, text) => {
  await assistant(s, m);
  await part(s, m, m + "t", text);
  await idle(s);
};

for (let i = 0; i < 8; i++) await assistant("s" + i, "s" + i + "-a0");
expectCalls(8, "eight sessions, eight lookups");
await new Promise((resolve) => setTimeout(resolve, 2300));
if (!calls.every((c) => c.signal?.aborted)) throw new Error("a lookup's signal never fired");

for (let i = 0; i < 4; i++) await turn("n" + i, "n" + i + "-a", "UNKNOWN_REPLY n" + i);
for (let i = 0; i < 8; i++) await turn("s" + i, "s" + i + "-a1", "UNKNOWN_REPLY s" + i);
await send("session.deleted", { sessionID: "s0", info: { id: "s0", directory: "/work/s0" } });
await turn("s0", "s0-b1", "RECREATED_UNKNOWN_REPLY 1");
expectCalls(8, "no ninth request while eight ignore their abort");

calls[1].settle({ data: { id: "s1" } });
await settle();
await assistant("s0", "s0-b2");
expectCalls(9, "a settled original frees one slot");
if (calls[8].id !== "s0") throw new Error("the ninth request is " + calls[8].id);
calls[0].settle({ data: { id: "s0" } });
await settle();
await part("s0", "s0-b2", "s0-b2t", "RECREATED_UNKNOWN_REPLY 2");
await idle("s0");
await turn("s1", "s1-a2", "S1_ROOT_REPLY");
calls[8].settle({ data: { id: "s0" } });
await settle();
await turn("s0", "s0-b3", "RECREATED_ROOT_REPLY");

await send("server.instance.disposed", {});
for (let i = 2; i < 8; i++) calls[i].settle({ data: { id: "s" + i } });
await settle();
await turn("s2", "s2-a2", "AFTER_DISPOSE_UNKNOWN_REPLY");
"#,
            &[],
        ) else {
            return;
        };
        let all = serde_json::to_string(&payloads).unwrap();
        assert!(
            !all.contains("UNKNOWN_REPLY"),
            "a session whose ancestry was never confirmed was read: {payloads:#?}"
        );
        let replies: Vec<Option<String>> = idle_replies(&payloads)
            .into_iter()
            .map(|(_, reply)| reply.map(|r| r.text))
            .collect();
        let mut expected = vec![None; 14];
        expected.push(Some("S1_ROOT_REPLY".to_owned()));
        expected.push(Some("RECREATED_ROOT_REPLY".to_owned()));
        expected.push(None);
        assert_eq!(
            replies, expected,
            "twelve turns of unknown sessions and two of the recreated one are sent replyless, \
             the settled root and the recreated session's own confirmed lookup are read, and \
             nothing is read after the dispose: {payloads:#?}"
        );
    }

    /// PRD #1497 review: an ancestry answer that arrives after its session was
    /// deleted, or after OpenCode disposed, is discarded — so a session later
    /// reported under the same id is not read on the strength of it — while
    /// the same answer for a live session still makes its next idle report
    /// carry the reply.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_discards_an_ancestry_answer_after_delete_or_dispose() {
        let Some(payloads) = run_plugin_driver(
            r#"
const pending = new Map();
const client = { session: { get: ({ path }) =>
  new Promise((resolve) => { pending.set(path.id, resolve); }) } };
const hooks = await plugin({ directory: "/work", client });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const assistant = (sessionID, id) =>
  send("message.updated", { sessionID, info: { id, sessionID, role: "assistant" } });
const part = (sessionID, messageID, id, text) =>
  send("message.part.updated", { sessionID, part: { id, sessionID, messageID, type: "text", text } });
const settle = () => new Promise((resolve) => setTimeout(resolve, 20));
const answer = async (id) => { pending.get(id)({ data: { id } }); pending.delete(id); await settle(); };
const turn = async (s, m, text) => {
  await assistant(s, m);
  await part(s, m, m + "t", text);
  await send("session.status", { sessionID: s, status: { type: "idle" } });
};

await assistant("live", "live-a1");
await answer("live");
await turn("live", "live-a2", "LIVE_REPLY");

await assistant("gone", "gone-a1");
await send("session.deleted", { sessionID: "gone", info: { id: "gone", directory: "/work/gone" } });
const staleGone = pending.get("gone");
pending.delete("gone");
staleGone({ data: { id: "gone" } });
await settle();
await turn("gone", "gone-a2", "STALE_DELETE_REPLY");

await assistant("old", "old-a1");
const staleOld = pending.get("old");
pending.delete("old");
await send("server.instance.disposed", {});
staleOld({ data: { id: "old" } });
await settle();
await turn("old", "old-a2", "STALE_DISPOSE_REPLY");
"#,
            &[],
        ) else {
            return;
        };
        let replies: Vec<Option<String>> = idle_replies(&payloads)
            .into_iter()
            .map(|(_, reply)| reply.map(|r| r.text))
            .collect();
        assert_eq!(
            replies,
            vec![Some("LIVE_REPLY".to_owned()), None, None],
            "{payloads:#?}"
        );
    }

    /// PRD #1497 audit AU-B2 and review RV-S3: one assistant message streamed
    /// as 10,000 distinct 8 KiB parts and 10,000 8 KiB deltas, under a 64 MiB
    /// heap, retains at most the daemon's byte bound in total: the driver
    /// completes (retaining every part, as the unbounded tracker did, exhausts
    /// that heap), and the reply the plugin sends is the message's head, at most
    /// `MAX_TURN_REPLY_BYTES` of UTF-8 bytes, with a lone surrogate replaced
    /// rather than sent as an escape the deck cannot decode.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_bounds_a_huge_reply_by_bytes_and_parts() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
await send("session.created", { sessionID: "big", info: { id: "big", directory: "/work" } });
await send("message.updated", { sessionID: "big", info: { id: "ba", sessionID: "big", role: "assistant" } });
await send("message.part.updated", { sessionID: "big", part: { id: "p-first", sessionID: "big",
  messageID: "ba", type: "text", text: "\ud800HEAD " } });
for (let i = 0; i < 10000; i++) {
  await send("message.part.updated", { sessionID: "big", part: { id: "p" + i, sessionID: "big",
    messageID: "ba", type: "text", text: Buffer.alloc(8192, i + "é🦀").toString() } });
}
for (let i = 0; i < 10000; i++) {
  await send("message.part.delta", { sessionID: "big", messageID: "ba", partID: "p" + (i % 300),
    field: "text", delta: Buffer.alloc(8192, "x" + i).toString() });
}
await send("session.status", { sessionID: "big", status: { type: "idle" } });
"#,
            &["--max-old-space-size=64"],
        ) else {
            return;
        };
        let raw: Vec<&str> = payloads
            .iter()
            .filter_map(|p| p.get("reply").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(raw.len(), 1);
        assert!(
            raw[0].len() <= crate::daemon_protocol::MAX_TURN_REPLY_BYTES,
            "the plugin sends at most the daemon's bound, in bytes: {}",
            raw[0].len()
        );
        let replies = idle_replies(&payloads);
        assert_eq!(replies.len(), 1);
        let reply = replies[0].1.clone().expect("the head of the reply is kept");
        assert!(
            reply.text.len() <= crate::daemon_protocol::MAX_TURN_REPLY_BYTES,
            "{} bytes",
            reply.text.len()
        );
        assert!(
            reply.text.len() > crate::daemon_protocol::MAX_TURN_REPLY_BYTES - 8,
            "the byte budget is used, not a code-point one: {} bytes",
            reply.text.len()
        );
        assert!(
            reply.text.starts_with("\u{fffd}HEAD 0é🦀"),
            "{}",
            &reply.text[..32]
        );
        // The deck bounds it identically, so nothing the plugin kept is cut.
        assert_eq!(
            crate::daemon_protocol::clamp_turn_reply(&reply.text),
            reply.text
        );
    }

    /// PRD #1497 audit AU-B2: a session deleted without going idle leaves no
    /// reply state behind. A failed turn and an interrupted turn are each
    /// followed by `session.deleted`; a session later reported under the same
    /// id is read fresh — its reply is neither marked failed nor withheld as
    /// interrupted, and the deleted turn's text is never sent.
    #[cfg(unix)]
    #[test]
    fn opencode_plugin_forgets_a_deleted_sessions_reply_state() {
        let Some(payloads) = run_plugin_driver(
            r#"
const hooks = await plugin({ directory: "/work" });
const send = (type, properties) => hooks.event({ event: { type, properties } });
const reply = async (s, m, text) => {
  await send("message.updated", { sessionID: s, info: { id: m, sessionID: s, role: "assistant" } });
  await send("message.part.updated", { sessionID: s, part: { id: m + "t", sessionID: s, messageID: m, type: "text", text } });
};
for (const [s, error] of [["d1", "APIError"], ["d2", "MessageAbortedError"]]) {
  await send("session.created", { sessionID: s, info: { id: s, directory: "/work" + s } });
  await reply(s, s + "-old", "DELETED_TURN_SENTINEL");
  await send("session.error", { sessionID: s, error: { name: error, data: { message: "boom" } } });
  await send("session.deleted", { sessionID: s, info: { id: s, directory: "/work" + s } });
  await send("session.created", { sessionID: s, info: { id: s, directory: "/work" + s } });
  await reply(s, s + "-new", "FRESH " + s);
  await send("session.status", { sessionID: s, status: { type: "idle" } });
}
"#,
            &[],
        ) else {
            return;
        };
        let all = serde_json::to_string(&payloads).unwrap();
        assert!(!all.contains("DELETED_TURN_SENTINEL"), "{payloads:#?}");
        let replies: Vec<Option<crate::daemon_protocol::FinalReply>> = idle_replies(&payloads)
            .into_iter()
            .map(|(_, reply)| reply)
            .collect();
        assert_eq!(
            replies,
            ["d1", "d2"]
                .iter()
                .map(|s| Some(crate::daemon_protocol::FinalReply {
                    turn_id: None,
                    text: format!("FRESH {s}"),
                    failed: false,
                }))
                .collect::<Vec<_>>(),
            "{payloads:#?}"
        );
    }

    /// Scenario: Load the generated plugin under Node with its binary pinned
    /// to a recorder and a stand-in OpenCode client, and send it OpenCode's
    /// captured two-question `question.asked`. The plugin hands the question to
    /// `await-answer` and posts the answer it prints to
    /// `/question/{requestID}/reply`. A `permission.asked` whose answer is
    /// still pending is then answered in OpenCode (`permission.replied`): the
    /// plugin stops the waiting child, posts nothing for it, and forwards the
    /// reply with its request id, as it does a `question.rejected`. Each child
    /// is told what OpenCode made of the reply it printed; a reply OpenCode
    /// refuses is reported as not taken and the question is raised again, and
    /// the second reply, which OpenCode takes, is reported as delivered.
    #[cfg(unix)]
    #[spec("question/detect/007")]
    #[test]
    fn question_detect_007_opencode_plugin_holds_and_replies_through_opencode() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let fixture = |name: &str| {
            std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/agent-questions")
                    .join(name),
            )
            .unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let payloads = dir.path().join("payloads.jsonl");
        let awaits = dir.path().join("await.jsonl");
        let posts = dir.path().join("posts.json");
        let acks = dir.path().join("acks.jsonl");
        let release = dir.path().join("release");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                r#"#!/bin/sh
if [ "$1" = "await-answer" ]; then
  IFS= read -r input
  printf '%s\n' "$*|$input" >> '{awaits}'
  case "$input" in
    *per_hold*) while [ ! -e '{release}' ]; do sleep 0.05; done; echo '{{"kind":"permission","request_id":"per_hold","body":{{"reply":"once"}}}}' ;;
    *per_fail*) echo '{{"kind":"permission","request_id":"per_fail","body":{{"reply":"once"}}}}' ;;
    *) echo '{{"kind":"question","request_id":"que_1043ff915001MjL9vryfDO6HZX","body":{{"answers":[["Blue"],["Small","Large"]]}}}}' ;;
  esac
  IFS= read -r ack && printf '%s\n' "$ack" >> '{acks}'
else
  cat >> '{payloads}'
  echo >> '{payloads}'
fi
"#,
                awaits = awaits.display(),
                acks = acks.display(),
                payloads = payloads.display(),
                release = release.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        // Each step waits for the state it depends on rather than for a fixed
        // delay, which a loaded machine outran (issue #1846): the held child
        // must have read its question before OpenCode's own reply stops it.
        // It answers only once `release` exists, after that reply, and the one
        // fixed wait left, at the end, gives a child the plugin failed to stop
        // time to post an answer the posts assertion would catch.
        std::fs::write(
            &driver,
            format!(
                r#"import pluginModule from "{plugin}";
const plugin = pluginModule.server;
import {{ existsSync, readFileSync, writeFileSync }} from "fs";
const posts = [];
let refuseNext = false;
const client = {{ _client: {{ post: async (req) => {{
  posts.push(req);
  if (refuseNext) {{
    refuseNext = false;
    // The SDK reports an error response rather than throwing.
    return {{ error: {{ name: "NotFoundError" }}, response: {{ ok: false, status: 500 }} }};
  }}
  return {{ data: true, response: {{ ok: true, status: 200 }} }};
}} }} }};
const hooks = await plugin({{ directory: "/work", client }});
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const lines = (path) =>
  existsSync(path) ? readFileSync(path, "utf8").split("\n").filter(Boolean).length : 0;
const until = async (what, done) => {{
  for (let i = 0; i < 1600; i += 1) {{
    if (done()) return;
    await wait(25);
  }}
  throw new Error("timed out waiting for " + what);
}};
await hooks.event({{ event: {{ type: "question.asked", properties: {question} }} }});
await until("the question's reply", () => posts.length > 0);
const held = {{ ...{permission}, id: "per_hold" }};
await hooks.event({{ event: {{ type: "permission.asked", properties: held }} }});
await until("the held permission to be handed over", () => lines("{awaits}") >= 2);
await hooks.event({{ event: {{ type: "permission.replied", properties: {{ sessionID: held.sessionID, requestID: "per_hold", reply: "once" }} }} }});
await hooks.event({{ event: {{ type: "question.rejected", properties: {{ sessionID: held.sessionID, requestID: "que_rejected" }} }} }});
writeFileSync("{release}", "");
refuseNext = true;
await hooks.event({{ event: {{ type: "permission.asked", properties: {{ ...held, id: "per_fail" }} }} }});
await until("the refused reply to be posted again", () => posts.length >= 3);
await until("every child's acknowledgement", () => lines("{acks}") >= 3);
await wait(500);
writeFileSync("{posts}", JSON.stringify(posts));
"#,
                plugin = plugin.display(),
                question = fixture("opencode-question-asked.json"),
                permission = fixture("opencode-permission-asked.json"),
                posts = posts.display(),
                awaits = awaits.display(),
                acks = acks.display(),
                release = release.display(),
            ),
        )
        .unwrap();
        let status = std::process::Command::new("node")
            .arg(&driver)
            .status()
            .expect("run node");
        assert!(status.success(), "the plugin driver failed");

        let posted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&posts).unwrap()).unwrap();
        let refused_then_taken = serde_json::json!({
            "url": "/permission/{requestID}/reply",
            "path": {"requestID": "per_fail"},
            "body": {"reply": "once"}
        });
        assert_eq!(
            posted,
            serde_json::json!([
                {
                    "url": "/question/{requestID}/reply",
                    "path": {"requestID": "que_1043ff915001MjL9vryfDO6HZX"},
                    "body": {"answers": [["Blue"], ["Small", "Large"]]}
                },
                refused_then_taken,
                refused_then_taken,
            ]),
            "the question's answer is posted, nothing for the permission OpenCode \
             answered itself, and the refused reply is posted again once re-asked"
        );

        let acked: Vec<serde_json::Value> = std::fs::read_to_string(&acks)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            acked,
            vec![
                serde_json::json!({"delivered": true}),
                serde_json::json!({"delivered": false, "detail": "OpenCode answered 500"}),
                serde_json::json!({"delivered": true}),
            ],
            "each child hears what OpenCode made of its reply"
        );

        let awaited: Vec<String> = std::fs::read_to_string(&awaits)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(awaited.len(), 4, "{awaited:?}");
        assert!(awaited[0].starts_with("await-answer --agent opencode --ack|"));
        assert!(awaited[2].contains("per_fail") && awaited[3].contains("per_fail"));
        let asked: serde_json::Value =
            serde_json::from_str(awaited[0].split_once('|').unwrap().1).unwrap();
        assert_eq!(asked["event"], "question.asked");
        assert_eq!(asked["properties"]["questions"][1]["multiple"], true);
        assert_eq!(asked["session_id"], "ses_efbc02f39ffeP3cw5BFH7xv6VM");

        let forwarded: Vec<serde_json::Value> = std::fs::read_to_string(&payloads)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let resolved = |event: &str, id: &str| {
            let payload = forwarded
                .iter()
                .find(|p| p["event"] == event)
                .unwrap_or_else(|| panic!("no {event} forwarded: {forwarded:?}"));
            assert_eq!(payload["request_id"], id);
            let input: crate::hook::OpenCodeHookInput =
                serde_json::from_value(payload.clone()).unwrap();
            let event = crate::hook::build_opencode_event(input).unwrap();
            assert_eq!(event.resolved_question_id(), Some(id));
        };
        resolved("permission.replied", "per_hold");
        resolved("question.rejected", "que_rejected");
    }

    /// Scenario: Load the generated plugin under Node and have a subagent's
    /// child session ask a question form, which OpenCode then reports answered,
    /// and ask a second one, which it reports dismissed. The plugin files each
    /// under the main session with the child's id, as it does the child's
    /// permission prompts: the card shows the subagent's question and its wait,
    /// and the answer ends that subagent's wait and clears its question.
    #[cfg(unix)]
    #[spec("question/detect/017")]
    #[test]
    fn question_detect_017_an_opencode_subagents_question_is_filed_as_the_subagents() {
        use crate::event::EventType;
        use crate::state::{AppState, SessionStatus};
        let mut asked: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/agent-questions/opencode-question-asked.json"),
            )
            .unwrap(),
        )
        .unwrap();
        asked["sessionID"] = "child".into();
        let question_id = asked["id"].as_str().unwrap().to_string();
        let mut dismissed = asked.clone();
        dismissed["id"] = "que_dismissed".into();
        let Some((payloads, awaits)) = run_plugin_driver_with_awaits(
            &format!(
                r#"
const hooks = await plugin({{ directory: "/work" }});
const send = (type, properties) => hooks.event({{ event: {{ type, properties }} }});
await send("session.created", {{ sessionID: "root", info: {{ id: "root", directory: "/work" }} }});
await send("session.created", {{ sessionID: "child", info: {{ id: "child", parentID: "root", directory: "/work" }} }});
await send("session.status", {{ sessionID: "child", status: {{ type: "busy" }} }});
await send("question.asked", {asked});
await awaited(1);
await send("question.replied", {{ sessionID: "child", requestID: "{question_id}", answers: [["Blue"]] }});
await send("question.asked", {dismissed});
await awaited(2);
await send("question.rejected", {{ sessionID: "child", requestID: "que_dismissed" }});
"#
            ),
            &[],
        ) else {
            return;
        };
        assert_eq!(
            payload_rows(&payloads),
            vec![
                row("root", "session.created", ""),
                row("root", "subagent.start", "child"),
                row("root", "question.replied", "child"),
                row("root", "question.rejected", "child"),
                row("root", "subagent.stop", "child"),
                row("root", "session.deleted", ""),
            ],
            "{payloads:#?}"
        );
        assert_eq!(awaits.len(), 2, "{awaits:#?}");
        let held = awaits
            .iter()
            .find(|p| p["properties"]["id"] == question_id.as_str())
            .unwrap_or_else(|| panic!("the child's question was not held: {awaits:#?}"));
        assert_eq!(held["session_id"], "root");
        assert_eq!(held["subagent_id"], "child", "{held}");
        let (asked_event, question) = crate::hook::build_opencode_event_with_question(
            serde_json::from_value(held.clone()).unwrap(),
        )
        .expect("an event");
        let question = question.expect("the child's form is a question");
        assert_eq!(asked_event.event_type, EventType::WaitingForInput);
        assert!(asked_event.is_from_subagent(), "the ask is the subagent's");
        assert_eq!(question.subagent_id.as_deref(), Some("child"));

        let decode = |payload: &serde_json::Value| {
            crate::hook::build_opencode_event(serde_json::from_value(payload.clone()).unwrap())
                .expect("every payload maps to an event")
        };
        let replied = decode(&payloads[2]);
        assert!(replied.is_subagent_permission_reply(), "{replied:?}");
        assert_eq!(replied.resolved_question_id(), Some(question_id.as_str()));
        let rejected = decode(&payloads[3]);
        assert!(rejected.is_subagent_permission_reply(), "{rejected:?}");
        assert_eq!(rejected.resolved_question_id(), Some("que_dismissed"));

        // What the card makes of it: the subagent's question and wait, which
        // its answer ends, returning the card to the main thread's status.
        let mut state = AppState::default();
        state.register_pane("pane-oc".into());
        let on_pane = |mut event: crate::event::AgentEvent| {
            event.pane_id = Some("pane-oc".into());
            event.agent_id = Some("7".into());
            event
        };
        state.apply_event(on_pane(decode(&payloads[0])));
        state.apply_event(on_pane(asked_event));
        {
            let card = &state.sessions["root"];
            assert_eq!(card.status, SessionStatus::WaitingForInput);
            assert_eq!(
                card.pending_question
                    .as_ref()
                    .and_then(|q| q.subagent_id.as_deref()),
                Some("child")
            );
            assert_eq!(
                card.subagent_wait.as_ref().map(|w| w.subagent_ids.clone()),
                Some(vec!["child".to_string()]),
                "the wait is the subagent's"
            );
        }
        state.apply_event(on_pane(replied));
        let card = &state.sessions["root"];
        assert_eq!(card.pending_question, None);
        assert_ne!(card.status, SessionStatus::WaitingForInput);
    }

    /// Scenario: Load the generated plugin under Node with its binary pinned
    /// to a recorder whose `await-answer` dies before it hands the question to
    /// the deck — exiting with an error, or killed — and send it a permission
    /// prompt and a question form. Each is then reported as the plain event,
    /// so the card still shows Needs Input; a child that ends normally, having
    /// handled its question, adds no second report.
    #[cfg(unix)]
    #[spec("question/detect/016")]
    #[test]
    fn question_detect_016_an_await_answer_child_that_fails_early_falls_back_to_the_plain_event() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: node is not available");
            return;
        }
        let permission: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/agent-questions/opencode-permission-asked.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let payloads = dir.path().join("payloads.jsonl");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                r#"#!/bin/sh
if [ "$1" = "await-answer" ]; then
  IFS= read -r input
  case "$input" in
    *per_crash*|*que_crash*) exit 3 ;;
    *per_killed*) kill -9 $$ ;;
    *) exit 0 ;;
  esac
else
  cat >> '{payloads}'
  echo >> '{payloads}'
fi
"#,
                payloads = payloads.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import pluginModule from "{plugin}";
const plugin = pluginModule.server;
const hooks = await plugin({{ directory: "/work", client: {{}} }});
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const asked = {permission};
for (const id of ["per_crash", "per_killed", "per_quiet"]) {{
  await hooks.event({{ event: {{ type: "permission.asked", properties: {{ ...asked, id }} }} }});
}}
await hooks.event({{ event: {{ type: "question.asked", properties: {{ id: "que_crash", sessionID: asked.sessionID, questions: [{{ question: "Pick?", header: "Pick", options: [{{ label: "A" }}] }}] }} }} }});
await wait(1500);
"#,
                plugin = plugin.display(),
                permission = permission,
            ),
        )
        .unwrap();
        let status = std::process::Command::new("node")
            .arg(&driver)
            .status()
            .expect("run node");
        assert!(status.success(), "the plugin driver failed");

        let forwarded: Vec<serde_json::Value> = std::fs::read_to_string(&payloads)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let reported = |event: &str, id: &str| {
            forwarded
                .iter()
                .filter(|p| p["event"] == event && p["request_id"] == id)
                .count()
        };
        assert_eq!(
            reported("permission.asked", "per_crash"),
            1,
            "{forwarded:#?}"
        );
        assert_eq!(
            reported("permission.asked", "per_killed"),
            1,
            "{forwarded:#?}"
        );
        assert_eq!(reported("question.asked", "que_crash"), 1, "{forwarded:#?}");
        assert_eq!(
            reported("permission.asked", "per_quiet"),
            0,
            "a child that handled its question adds no report: {forwarded:#?}"
        );
        let plain: crate::hook::OpenCodeHookInput = serde_json::from_value(
            forwarded
                .iter()
                .find(|p| p["request_id"] == "que_crash")
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(
            crate::hook::build_opencode_event(plain).map(|e| e.event_type),
            Some(crate::event::EventType::WaitingForInput),
            "the plain question reads as Needs Input"
        );
    }

    // --- PRD #2050: OpenCode 2.x ---

    /// The paths a layout test reads, under `fixture`: the global (XDG) root
    /// and the legacy one, both present, as on a machine where OpenCode has run.
    #[cfg(unix)]
    struct LayoutFixture {
        _dir: tempfile::TempDir,
        roots: Vec<PathBuf>,
        bin: String,
    }

    #[cfg(unix)]
    impl LayoutFixture {
        fn new() -> Self {
            let dir = crate::test_temp::tempdir().unwrap();
            let roots = vec![
                dir.path().join("config").join("opencode"),
                dir.path().join("home").join(".opencode"),
            ];
            for root in &roots {
                std::fs::create_dir_all(root).unwrap();
            }
            let bin = seed_deck(&dir.path().join("local").join("dot-agent-deck"));
            LayoutFixture {
                _dir: dir,
                roots,
                bin,
            }
        }
        fn install(&self, layout: Layout) -> Vec<String> {
            let mut failed = Vec::new();
            auto_install_layout(&self.roots, self.v2_global(), layout, &mut failed, || {
                Ok(self.bin.clone())
            });
            failed
        }
        /// OpenCode 2.x's global root: the XDG root, or the third root when
        /// the fixture has one (`$OPENCODE_CONFIG_DIR`).
        fn v2_global(&self) -> &Path {
            self.roots.get(2).unwrap_or(&self.roots[0])
        }
        fn global(&self) -> &Path {
            &self.roots[0]
        }
        fn legacy(&self) -> &Path {
            &self.roots[1]
        }
        /// What is installed: (module in global, module in legacy, 1.x TUI
        /// half, 2.x TUI half).
        fn state(&self) -> (bool, bool, bool, bool) {
            (
                plugin_file(self.global()).exists(),
                plugin_file(self.legacy()).exists(),
                tui_plugin_file(self.global()).exists(),
                tui_v2_file(self.global()).exists(),
            )
        }
    }

    /// (inode, mtime in ns, bytes) of `path`, to tell an untouched file from
    /// one rewritten with the same bytes.
    #[cfg(unix)]
    fn file_identity(path: &Path) -> (u64, i64, i64, Vec<u8>) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        (
            meta.ino(),
            meta.mtime(),
            meta.mtime_nsec(),
            std::fs::read(path).unwrap(),
        )
    }

    /// Scenario (PRD #2050, review R1, audit N3): every generation gets the
    /// same files: one unmarked dual-shape module in the global root, none in
    /// `~/.opencode` (which OpenCode 2 would refuse as a duplicate), and both
    /// TUI halves, which do not see each other.
    #[cfg(unix)]
    #[test]
    fn opencode_every_layout_installs_the_same_files() {
        for layout in [Layout::V1, Layout::V2, Layout::Unknown] {
            let f = LayoutFixture::new();
            write_plugin(f.legacy(), &f.bin).unwrap();
            assert_eq!(f.install(layout), Vec::<String>::new(), "{layout:?}");
            assert_eq!(f.state(), (true, false, true, true), "{layout:?}");
            assert_eq!(
                std::fs::read_to_string(plugin_file(f.global())).unwrap(),
                plugin_template(&f.bin),
                "{layout:?}: no layout marker in the module"
            );
            assert_eq!(
                existing_pin(&tui_v2_file(f.global())).as_deref(),
                Some(f.bin.as_str()),
                "{layout:?}"
            );
            assert!(
                std::fs::read_to_string(f.global().join("tui.json"))
                    .unwrap()
                    .contains("dot-agent-deck-tui.js"),
                "{layout:?}"
            );
            let module = std::fs::read_to_string(plugin_file(f.global())).unwrap();
            assert!(module.contains(
                "export default { id: \"dot-agent-deck\", server: DotAgentDeckPlugin, setup: setupV2 };"
            ));
        }
    }

    /// Scenario (PRD #2050, review R1, audit N3): a machine running OpenCode
    /// 1.x and 2.x side by side spawns one generation, then the other, then a
    /// wrapper, in any order, and the deck starts with each generation's
    /// startup install (the layout it last recorded, or none). Once the first
    /// install has run, none of them touches the server module (same bytes,
    /// inode and mtime, so a live OpenCode 2 is not made to reload it), the
    /// TUI halves or `tui.json`. A module carrying an older build's layout
    /// marker is rewritten once, without it.
    #[cfg(unix)]
    #[test]
    fn opencode_alternating_generations_rewrite_nothing() {
        let f = LayoutFixture::new();
        std::fs::create_dir_all(plugin_file(f.global()).parent().unwrap()).unwrap();
        std::fs::write(
            plugin_file(f.global()),
            format!(
                "// dot-agent-deck OpenCode layout: 1\n{}",
                plugin_template(&f.bin)
            ),
        )
        .unwrap();
        f.install(Layout::V2);
        assert_eq!(
            std::fs::read_to_string(plugin_file(f.global())).unwrap(),
            plugin_template(&f.bin),
            "the old marker goes with the first install"
        );
        let watched = [
            plugin_file(f.global()),
            tui_plugin_file(f.global()),
            tui_v2_file(f.global()),
            f.global().join("tui.json"),
        ];
        let before: Vec<_> = watched.iter().map(|p| file_identity(p)).collect();
        for layout in [
            Layout::V1,
            Layout::V2,
            Layout::Unknown,
            Layout::V1,
            Layout::Unknown,
            Layout::V2,
        ] {
            assert_eq!(f.install(layout), Vec::<String>::new(), "{layout:?}");
            let after: Vec<_> = watched.iter().map(|p| file_identity(p)).collect();
            for ((path, b), a) in watched.iter().zip(&before).zip(&after) {
                assert!(b == a, "{layout:?} rewrote {}", path.display());
            }
            assert!(!plugin_file(f.legacy()).exists(), "{layout:?}");
        }
    }

    /// Scenario (PRD #2050, auditor L2): with only `~/.opencode` present, an
    /// install of any layout creates the global root, which the TUI half
    /// needs anyway, before placing the module, so the module lands there in
    /// one step and the deck's legacy copy is removed: OpenCode 2 would
    /// refuse that copy for a project under `$HOME` and not load it at all
    /// for one outside it. A second install writes nothing.
    #[cfg(unix)]
    #[test]
    fn opencode_a_legacy_only_machine_converges_on_the_global_root() {
        for layout in [Layout::V2, Layout::V1, Layout::Unknown] {
            let f = LayoutFixture::new();
            std::fs::remove_dir_all(f.global()).unwrap();
            write_plugin(f.legacy(), &f.bin).unwrap();
            assert_eq!(f.install(layout), Vec::<String>::new(), "{layout:?}");
            assert_eq!(f.state(), (true, false, true, true), "{layout:?}: one step");
            let before = file_identity(&plugin_file(f.global()));
            assert_eq!(f.install(layout), Vec::<String>::new(), "{layout:?}");
            assert!(
                file_identity(&plugin_file(f.global())) == before,
                "{layout:?}: the second install rewrote the module"
            );
        }
    }

    /// With nothing remembered (no module yet, or one from before PRD #2050),
    /// an install that cannot tell the version writes the module into the
    /// global root only — not `~/.opencode`, where OpenCode 2 would meet a
    /// second copy (review S3) — and both TUI halves, which work under either
    /// generation.
    #[cfg(unix)]
    #[test]
    fn opencode_unknown_layout_installs_both_tui_halves() {
        let f = LayoutFixture::new();
        assert_eq!(f.install(Layout::Unknown), Vec::<String>::new());
        assert_eq!(f.state(), (true, false, true, true));
        assert_eq!(
            std::fs::read_to_string(plugin_file(f.global())).unwrap(),
            plugin_template(&f.bin),
            "unmarked: the same bytes as before PRD #2050's markers"
        );
    }

    /// Review S3 follow-up: a wrapper's spawn (nothing remembered) on a
    /// machine that still has the deck's copy in `~/.opencode` from an older
    /// deck removes that copy, and its obsolete nested layout, once the global
    /// root holds the module, so OpenCode 2 meets no `Duplicate plugin ID`
    /// after an upgrade with no manual step. Other files in that directory
    /// are left alone.
    #[cfg(unix)]
    #[test]
    fn opencode_unknown_layout_removes_the_decks_legacy_copy() {
        let f = LayoutFixture::new();
        write_plugin(f.legacy(), &f.bin).unwrap();
        std::fs::create_dir_all(stale_plugin_dir(f.legacy())).unwrap();
        std::fs::write(stale_plugin_dir(f.legacy()).join("index.js"), "x").unwrap();
        let other = f.legacy().join("plugin").join("someone-else.js");
        std::fs::write(&other, "export default {}").unwrap();

        assert_eq!(f.install(Layout::Unknown), Vec::<String>::new());
        let (global, legacy, tui, tui_v2) = f.state();
        assert_eq!((global, legacy, tui, tui_v2), (true, false, true, true));
        assert!(!stale_plugin_dir(f.legacy()).exists());
        assert_eq!(
            std::fs::read_to_string(&other).unwrap(),
            "export default {}",
            "only the deck's own files are removed"
        );
        assert_eq!(
            existing_binary_path(f.global()).as_deref(),
            Some(f.bin.as_str())
        );
    }

    /// A pin to another live install that only the legacy copy held is
    /// carried to the global root before the legacy copy goes, as the 2.x
    /// layout does.
    #[cfg(unix)]
    #[test]
    fn opencode_unknown_layout_carries_a_legacy_pin_over() {
        let f = LayoutFixture::new();
        let other = seed_deck(&f._dir.path().join("homebrew").join("dot-agent-deck"));
        write_plugin(f.legacy(), &other).unwrap();
        assert_eq!(f.install(Layout::Unknown), Vec::<String>::new());
        assert!(!plugin_file(f.legacy()).exists());
        assert_eq!(
            existing_binary_path(f.global()).as_deref(),
            Some(other.as_str())
        );
    }

    /// With `$OPENCODE_CONFIG_DIR` moving OpenCode 2.x's global root, the
    /// legacy copy stays until that root holds the module: a module only in
    /// the XDG root is not what 2.x loads.
    #[cfg(unix)]
    #[test]
    fn opencode_unknown_layout_keeps_the_legacy_copy_until_2s_global_root_has_the_module() {
        let mut f = LayoutFixture::new();
        f.roots.push(f._dir.path().join("ocd"));
        write_plugin(f.legacy(), &f.bin).unwrap();
        assert_eq!(f.install(Layout::Unknown), Vec::<String>::new());
        assert!(plugin_file(f.global()).exists());
        assert!(
            plugin_file(f.legacy()).exists(),
            "2.x's global root does not exist, so the legacy copy is kept"
        );
        std::fs::create_dir_all(&f.roots[2]).unwrap();
        assert_eq!(f.install(Layout::Unknown), Vec::<String>::new());
        assert!(plugin_file(&f.roots[2]).exists());
        assert!(!plugin_file(f.legacy()).exists());
    }

    /// Review F6: with `$OPENCODE_CONFIG_DIR` naming an existing directory, an
    /// install that knows no version writes the module to the XDG root (1.x's
    /// global root) and to that directory (2.x's, the only one 2.2.5 loads
    /// then, measured), and removes the deck's legacy `~/.opencode` copy, so
    /// a 2.x started through a wrapper meets one copy.
    #[cfg(unix)]
    #[test]
    fn opencode_unknown_layout_with_opencode_config_dir_leaves_2x_one_copy() {
        let mut f = LayoutFixture::new();
        f.roots.push(f._dir.path().join("ocd"));
        std::fs::create_dir_all(&f.roots[2]).unwrap();
        write_plugin(f.legacy(), &f.bin).unwrap();
        assert_eq!(f.install(Layout::Unknown), Vec::<String>::new());
        assert!(plugin_file(f.global()).exists(), "1.x's global root");
        assert!(plugin_file(&f.roots[2]).exists(), "2.x's global root");
        assert!(!plugin_file(f.legacy()).exists(), "no copy 2.x would scan");
    }

    /// Review F2: the generation an agent was seen to run is recorded in the
    /// deck's own state file, written only when it changes, for an install
    /// with no command to look at; nothing in OpenCode's config is touched by
    /// recording it.
    #[cfg(unix)]
    #[test]
    fn a_detected_generation_is_remembered_outside_opencodes_config() {
        use crate::opencode_version::Major;
        let f = LayoutFixture::new();
        assert_eq!(f.install(Layout::V1), Vec::<String>::new());
        let module = std::fs::read(plugin_file(f.global())).unwrap();
        let state = f._dir.path().join("state").join(DETECTED_LAYOUT_FILE);
        assert_eq!(detected_major_at(&state), None);
        assert!(remember_layout_at(&state, Major::V2).unwrap());
        assert!(!remember_layout_at(&state, Major::V2).unwrap(), "unchanged");
        assert_eq!(detected_major_at(&state), Some(Major::V2));
        assert_eq!(
            std::fs::read(plugin_file(f.global())).unwrap(),
            module,
            "OpenCode's module is untouched"
        );
        assert!(remember_layout_at(&state, Major::V1).unwrap());
        assert_eq!(detected_major_at(&state), Some(Major::V1));
    }

    /// The upgrade to 2.x keeps a pin to another live install that only the
    /// legacy root's copy held: the copy is carried to the global root under
    /// the usual pin rules before the legacy one is removed.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_layout_carries_a_legacy_pin_over() {
        let f = LayoutFixture::new();
        let other = seed_deck(&f._dir.path().join("homebrew").join("dot-agent-deck"));
        write_plugin(f.legacy(), &other).unwrap();
        assert_eq!(f.install(Layout::V2), Vec::<String>::new());
        assert!(!plugin_file(f.legacy()).exists());
        assert_eq!(
            existing_binary_path(f.global()).as_deref(),
            Some(other.as_str())
        );
    }

    /// Review N6, R1: a module pinned to another live install is not
    /// regenerated by any layout, and not marked either: its bytes are left
    /// exactly as they were.
    #[cfg(unix)]
    #[test]
    fn a_module_pinned_elsewhere_is_left_alone_by_every_layout() {
        let f = LayoutFixture::new();
        let other = seed_deck(&f._dir.path().join("homebrew").join("dot-agent-deck"));
        write_plugin(f.global(), &other).unwrap();
        let before = file_identity(&plugin_file(f.global()));
        for layout in [Layout::V1, Layout::Unknown, Layout::V2] {
            f.install(layout);
            assert!(
                file_identity(&plugin_file(f.global())) == before,
                "{layout:?} touched another install's module"
            );
        }
        assert_eq!(
            existing_binary_path(f.global()).as_deref(),
            Some(other.as_str())
        );
    }

    /// Scenario (PRD #2050, audit N4): a deck upgrade replaced the install
    /// and wrote a module of a later template revision, and the older daemon,
    /// still running the replaced file, starts an OpenCode agent. Its
    /// automatic install, of any layout, leaves the newer module exactly as
    /// it is, though it pins this very install; a module of this revision or
    /// an earlier one is still refreshed, and `hooks install --agent
    /// opencode` writes this binary's template when asked to.
    #[cfg(unix)]
    #[test]
    fn an_older_deck_never_downgrades_a_newer_template() {
        let f = LayoutFixture::new();
        // The image this daemon runs: the install's file before the upgrade
        // replaced it, kept here as a separate file.
        let replaced = f._dir.path().join("replaced-dot-agent-deck");
        std::fs::copy(&f.bin, &replaced).unwrap();
        RUNNING_EXE_FOR_TEST.with(|p| *p.borrow_mut() = Some(replaced.clone()));
        assert!(running_other_than(&f.bin));
        let ours = plugin_template(&f.bin);
        let marker = format!("const DAD_TEMPLATE_REVISION = {PLUGIN_TEMPLATE_REVISION};");
        assert!(ours.contains(&marker), "the template declares its revision");
        let newer = ours.replace(
            &marker,
            &format!(
                "const DAD_TEMPLATE_REVISION = {};\n// a later deck's template",
                PLUGIN_TEMPLATE_REVISION + 1
            ),
        );
        std::fs::create_dir_all(plugin_file(f.global()).parent().unwrap()).unwrap();
        std::fs::write(plugin_file(f.global()), &newer).unwrap();
        assert_eq!(
            template_revision(&plugin_file(f.global())),
            Some(PLUGIN_TEMPLATE_REVISION + 1)
        );
        let before = file_identity(&plugin_file(f.global()));
        for layout in [Layout::V1, Layout::V2, Layout::Unknown] {
            assert_eq!(f.install(layout), Vec::<String>::new(), "{layout:?}");
            assert!(
                file_identity(&plugin_file(f.global())) == before,
                "{layout:?} downgraded the module"
            );
        }

        let older = ours.replace(&marker, "");
        assert_eq!(template_revision_of(&older), 0, "no revision: before them");
        std::fs::write(plugin_file(f.global()), &older).unwrap();
        f.install(Layout::Unknown);
        assert_eq!(
            std::fs::read_to_string(plugin_file(f.global())).unwrap(),
            ours,
            "an older template is refreshed"
        );

        std::fs::write(plugin_file(f.global()), &newer).unwrap();
        install_to_roots(
            &[f.global().to_path_buf()],
            || unreachable!(),
            &f.bin,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(plugin_file(f.global())).unwrap(),
            ours,
            "an explicit install writes this template"
        );
        RUNNING_EXE_FOR_TEST.with(|p| *p.borrow_mut() = None);
    }

    /// Scenario (PRD #2050, auditor M2): the user rolled the deck back to an
    /// older release at the same path (or synced an OpenCode config from a
    /// machine with a newer deck), so the module is of a later template
    /// revision and pins this install, and the file at the pin is the one
    /// this process runs. Its automatic install, of any layout, writes its
    /// own template, which this build can run.
    #[cfg(unix)]
    #[test]
    fn a_rolled_back_deck_writes_its_own_template_over_a_newer_one() {
        let f = LayoutFixture::new();
        RUNNING_EXE_FOR_TEST.with(|p| *p.borrow_mut() = Some(PathBuf::from(&f.bin)));
        assert!(!running_other_than(&f.bin));
        let ours = plugin_template(&f.bin);
        let marker = format!("const DAD_TEMPLATE_REVISION = {PLUGIN_TEMPLATE_REVISION};");
        let newer = ours.replace(
            &marker,
            &format!(
                "const DAD_TEMPLATE_REVISION = {};\n// a later deck's template",
                PLUGIN_TEMPLATE_REVISION + 1
            ),
        );
        std::fs::create_dir_all(plugin_file(f.global()).parent().unwrap()).unwrap();
        for layout in [Layout::V1, Layout::V2, Layout::Unknown] {
            std::fs::write(plugin_file(f.global()), &newer).unwrap();
            assert_eq!(f.install(layout), Vec::<String>::new(), "{layout:?}");
            assert_eq!(
                std::fs::read_to_string(plugin_file(f.global())).unwrap(),
                ours,
                "{layout:?} left a newer template over the install this process runs"
            );
        }
        RUNNING_EXE_FOR_TEST.with(|p| *p.borrow_mut() = None);
    }

    /// [`template_revision`] of module text `js`.
    #[cfg(unix)]
    fn template_revision_of(js: &str) -> u32 {
        let dir = crate::test_temp::tempdir().unwrap();
        let file = dir.path().join("m.js");
        std::fs::write(&file, js).unwrap();
        template_revision(&file).unwrap()
    }

    /// Audit N4: [`PLUGIN_TEMPLATE_REVISION`] moves with the template. The
    /// digest is of the module this build writes; when the template changes,
    /// this fails until the revision is bumped and the new digest recorded
    /// here, so a newer deck's module is never taken for an older one's.
    #[test]
    fn the_template_revision_is_bumped_with_the_template() {
        use sha2::Digest;
        const RECORDED: (u32, &str) = (
            1,
            "035ed0de20f3b21daa63df10c95b00bd56829aac6abe152121a976d31d126a33",
        );
        let digest = sha2::Sha256::digest(plugin_template("/opt/dot-agent-deck").as_bytes());
        let digest: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            (PLUGIN_TEMPLATE_REVISION, digest.as_str()),
            RECORDED,
            "the OpenCode plugin template changed: bump PLUGIN_TEMPLATE_REVISION, \
             then record the new revision and digest here"
        );
    }

    /// Scenario: the config roots are resolved from `HOME`,
    /// `XDG_CONFIG_HOME` and `OPENCODE_CONFIG_DIR`. An empty or relative
    /// `XDG_CONFIG_HOME` is ignored (XDG spec), so nothing lands in the
    /// current directory; `OPENCODE_CONFIG_DIR` becomes OpenCode 2.x's global
    /// root and one more 1.x root, and is likewise ignored when empty or
    /// relative.
    #[cfg(unix)]
    #[test]
    fn opencode_roots_follow_xdg_config_home_and_opencode_config_dir() {
        use std::ffi::OsStr;
        let home = Path::new("/h");
        let default = (
            vec![
                PathBuf::from("/h/.config/opencode"),
                PathBuf::from("/h/.opencode"),
            ],
            PathBuf::from("/h/.config/opencode"),
        );
        assert_eq!(roots_from(home, None, None), default);
        for ignored in ["", "opencode-cfg", "./cfg"] {
            assert_eq!(
                roots_from(home, Some(OsStr::new(ignored)), None),
                default,
                "XDG_CONFIG_HOME={ignored:?}"
            );
            assert_eq!(
                roots_from(home, None, Some(OsStr::new(ignored))),
                default,
                "OPENCODE_CONFIG_DIR={ignored:?}"
            );
        }
        assert_eq!(
            roots_from(home, Some(OsStr::new("/x")), None),
            (
                vec![PathBuf::from("/x/opencode"), PathBuf::from("/h/.opencode")],
                PathBuf::from("/x/opencode"),
            )
        );
        assert_eq!(
            roots_from(home, None, Some(OsStr::new("/ocd"))),
            (
                vec![
                    PathBuf::from("/h/.config/opencode"),
                    PathBuf::from("/h/.opencode"),
                    PathBuf::from("/ocd"),
                ],
                PathBuf::from("/ocd"),
            )
        );
        assert_eq!(
            roots_from(home, None, Some(OsStr::new("/h/.config/opencode"))),
            default,
            "a config dir that is the XDG root is not a second root"
        );
    }

    /// A [`LayoutFixture`] with `$OPENCODE_CONFIG_DIR` set, as its third root.
    #[cfg(unix)]
    fn config_dir_fixture() -> LayoutFixture {
        let mut f = LayoutFixture::new();
        let dir = f._dir.path().join("ocd");
        std::fs::create_dir_all(&dir).unwrap();
        f.roots.push(dir);
        f
    }

    /// Scenario: with `$OPENCODE_CONFIG_DIR` set, every layout puts the
    /// server module in the XDG root (1.x's global root) and in that
    /// directory (2.x's, the only one 2.2.5 loads then), the 2.x TUI half in
    /// that directory only, and the 1.x half in the XDG root; the deck's
    /// legacy copy goes. Alternating generations then rewrite nothing.
    #[cfg(unix)]
    #[test]
    fn opencode_layout_follows_opencode_config_dir() {
        let f = config_dir_fixture();
        let ocd = f.roots[2].clone();
        write_plugin(f.legacy(), &f.bin).unwrap();
        write_tui_v2(f.global(), &f.bin).unwrap();
        assert_eq!(f.install(Layout::V1), Vec::<String>::new());
        assert!(plugin_file(&ocd).exists());
        assert!(plugin_file(f.global()).exists());
        assert!(!plugin_file(f.legacy()).exists(), "legacy copy removed");
        assert!(tui_v2_file(&ocd).exists());
        assert!(
            !tui_v2_file(f.global()).exists(),
            "a 2.x half where 2.x no longer reads goes"
        );
        assert!(tui_plugin_file(f.global()).exists());
        assert_eq!(existing_binary_path(&ocd).as_deref(), Some(f.bin.as_str()));
        let before: Vec<_> = [
            plugin_file(&ocd),
            plugin_file(f.global()),
            tui_v2_file(&ocd),
        ]
        .iter()
        .map(|p| file_identity(p))
        .collect();
        for layout in [Layout::V2, Layout::Unknown, Layout::V1] {
            assert_eq!(f.install(layout), Vec::<String>::new());
            let after: Vec<_> = [
                plugin_file(&ocd),
                plugin_file(f.global()),
                tui_v2_file(&ocd),
            ]
            .iter()
            .map(|p| file_identity(p))
            .collect();
            assert!(before == after, "{layout:?} rewrote a file");
        }
    }

    /// `cli.json` as OpenCode 2.2.5 writes it after migrating `tui.json`
    /// (`JSON.stringify(…, null, 2)`), holding `entries` in `plugins`.
    #[cfg(unix)]
    fn migrated_cli_json(entries: &[&str]) -> String {
        let value = serde_json::json!({
            "$schema": "https://opencode.ai/cli.json",
            "plugins": entries,
            "theme": {"name": "opencode"},
        });
        serde_json::to_string_pretty(&value).unwrap() + "\n"
    }

    /// Scenario: a machine upgraded from OpenCode 1.x, where 2.x copied the
    /// deck's 1.x TUI entry from `tui.json` into `cli.json`'s `plugins`
    /// (where 2.x cannot load it). The next install removes only the deck's
    /// entry from `cli.json`, keeping the user's entries, an object entry,
    /// every other member and OpenCode's file mode; the 1.x TUI half and its
    /// `tui.json` entry stay, for the 1.x side of the machine.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_layout_removes_the_migrated_cli_entry() {
        let f = LayoutFixture::new();
        f.install(Layout::V1);
        let entry = tui_plugin_file(f.global()).to_string_lossy().into_owned();
        let tui_json = f.global().join("tui.json");
        assert!(std::fs::read_to_string(&tui_json).unwrap().contains(&entry));
        // What 2.2.5's migration writes: the user's plugin, the deck's, and
        // an object entry (a `[name, options]` tuple in `tui.json`).
        let cli = f.global().join("cli.json");
        let before = migrated_cli_json(&["user-tui-plugin", &entry]).replace(
            "\"plugins\": [",
            "\"plugins\": [\n    {\n      \"package\": \"pkg\",\n      \"options\": {\n        \"a\": 1\n      }\n    },",
        );
        std::fs::write(&cli, &before).unwrap();
        std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();

        assert_eq!(f.install(Layout::V2), Vec::<String>::new());
        let after = std::fs::read_to_string(&cli).unwrap();
        assert_eq!(
            after,
            before.replace(
                &format!(",\n    {}", serde_json::to_string(&entry).unwrap()),
                ""
            ),
            "only the deck's entry goes"
        );
        let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(parsed["plugins"][1], "user-tui-plugin");
        assert_eq!(parsed["plugins"][0]["package"], "pkg");
        assert_eq!(parsed["theme"]["name"], "opencode");
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&cli).unwrap().permissions()
            ) & 0o777,
            0o600,
            "OpenCode's owner-only mode is kept"
        );
        assert!(std::fs::read_to_string(&tui_json).unwrap().contains(&entry));
        assert!(tui_plugin_file(f.global()).exists());

        // A second install changes nothing.
        assert_eq!(f.install(Layout::V2), Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(&cli).unwrap(), after);
    }

    /// Scenario (audit L2): a FIFO with no writer where OpenCode 2's
    /// `cli.json` would be, and another where the legacy root's module would
    /// be carried over from under the plugin lock. An OpenCode 2 install
    /// completes without blocking on either: the `cli.json` refusal is
    /// reported, and nothing is carried over from the FIFO.
    #[cfg(unix)]
    #[test]
    fn opencode_install_never_blocks_on_a_fifo_config() {
        let f = LayoutFixture::new();
        let fifo = |path: &Path| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a NUL-terminated path and a mode.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "{path:?}");
        };
        let cli = f.global().join("cli.json");
        fifo(&cli);
        fifo(&plugin_file(f.legacy()));
        let (tx, rx) = std::sync::mpsc::channel();
        let roots = f.roots.clone();
        let v2_global = f.v2_global().to_path_buf();
        let bin = f.bin.clone();
        std::thread::spawn(move || {
            let mut failed = Vec::new();
            auto_install_layout(&roots, &v2_global, Layout::V2, &mut failed, || Ok(bin));
            let _ = tx.send(failed);
        });
        let failed = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the install blocked on a FIFO");
        assert!(
            failed.iter().any(|f| f.contains("cli.json")),
            "the cli.json refusal is reported: {failed:?}"
        );
        assert_eq!(
            std::fs::read_to_string(plugin_file(f.global()))
                .ok()
                .map(|js| js.contains(&f.bin)),
            Some(true),
            "the global module is the deck's own, not the FIFO's"
        );
    }

    /// Scenario: `cli.json` is OpenCode 2.x's own file. No install creates
    /// it; every layout — 1.x (which never reads it) and unknown included —
    /// removes the deck's dead entry from it, so a wrapper's spawn on a 2.x
    /// machine cannot leave it behind; and with `$OPENCODE_CONFIG_DIR` set the
    /// entry is removed from the `cli.json` there.
    #[cfg(unix)]
    #[test]
    fn opencode_cli_json_is_never_created_and_every_layout_cleans_it() {
        for layout in [Layout::V1, Layout::V2, Layout::Unknown] {
            let f = LayoutFixture::new();
            assert_eq!(f.install(layout), Vec::<String>::new());
            assert!(
                !f.global().join("cli.json").exists(),
                "{layout:?} must not create cli.json"
            );

            let entry = tui_plugin_file(f.global()).to_string_lossy().into_owned();
            let cli = f.global().join("cli.json");
            std::fs::write(&cli, migrated_cli_json(&[&entry, "user-tui-plugin"])).unwrap();
            assert_eq!(f.install(layout), Vec::<String>::new());
            assert_eq!(
                std::fs::read_to_string(&cli).unwrap(),
                migrated_cli_json(&["user-tui-plugin"]),
                "{layout:?}"
            );
        }

        let f = config_dir_fixture();
        let ocd = f.roots[2].clone();
        let entry = tui_plugin_file(f.global()).to_string_lossy().into_owned();
        let cli = ocd.join("cli.json");
        std::fs::write(&cli, migrated_cli_json(&[&format!("file://{entry}")])).unwrap();
        assert_eq!(f.install(Layout::V2), Vec::<String>::new());
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cli).unwrap()).unwrap();
        assert_eq!(after["plugins"], serde_json::json!([]));
        assert_eq!(after["theme"]["name"], "opencode");
        assert!(!f.global().join("cli.json").exists());
    }

    /// Scenario: a `cli.json` the deck cannot edit safely. One that names the
    /// deck's TUI file but is not valid JSONC is left as it is and reported
    /// as a failure (so the card can say so); one that does not mention the
    /// deck at all is nobody's business and reports nothing.
    #[cfg(unix)]
    #[test]
    fn opencode_an_uneditable_cli_json_is_reported_only_when_it_names_the_deck() {
        let f = LayoutFixture::new();
        let cli = f.global().join("cli.json");
        std::fs::write(&cli, "{\"plugins\": [\"x\"").unwrap();
        assert_eq!(f.install(Layout::V2), Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(&cli).unwrap(),
            "{\"plugins\": [\"x\""
        );

        let broken = format!(
            "{{\"plugins\": [{}",
            serde_json::to_string(&tui_plugin_file(f.global()).to_string_lossy()).unwrap()
        );
        std::fs::write(&cli, &broken).unwrap();
        let failed = f.install(Layout::V2);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("cli.json"), "{failed:?}");
        assert_eq!(std::fs::read_to_string(&cli).unwrap(), broken);
    }

    /// A drive of the generated module's OpenCode 2.x `setup` under Node with
    /// a fake 2.x context: an event stream of `{ type, data, location }`,
    /// `tool.hook`, `session.hook`, `session.get`/`prompt` and
    /// `permission.reply`. `body` runs after `setup` with `emit(type, data)`,
    /// `hooks` (the registered callbacks), `prompts`, `replies` and `cleanup`.
    const FAKE_V2_CTX_JS: &str = r#"const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const queue = [];
let wake;
let subscribed = false;
const stream = { [Symbol.asyncIterator]() { return { next: async () => {
  while (!queue.length) await new Promise((r) => { wake = r; });
  return { value: queue.shift(), done: false };
} }; } };
const emit = async (type, data, directory = "/work") => {
  queue.push({ id: "evt", created: Date.now(), type, data, location: { directory } });
  wake?.();
  await sleep(40);
};
const hooks = { tool: {}, session: {} };
const prompts = [];
const replies = [];
const parents = {};
const registration = (domain, name) => ({ dispose: async () => { delete hooks[domain][name]; } });
const ctx = {
  location: { directory: "/work" },
  event: { subscribe: () => { subscribed = true; return stream; } },
  tool: { hook: async (name, cb) => { hooks.tool[name] = cb; return registration("tool", name); } },
  session: {
    hook: async (name, cb) => { hooks.session[name] = cb; return registration("session", name); },
    get: async ({ sessionID }) => ({ id: sessionID, parentID: parents[sessionID] }),
    prompt: async (input) => { prompts.push(input); return { id: "inbox-" + prompts.length }; },
  },
  permission: { reply: async (input) => { replies.push(input); } },
};
"#;

    /// Run `body` against the module's 2.x `setup` (see [`FAKE_V2_CTX_JS`]),
    /// as a standalone server's plugin, outside a deck pane.
    #[cfg(unix)]
    fn run_v2_driver(body: &str) -> Option<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
        run_v2_driver_as(r#""serve", "--stdio""#, body)
    }

    /// [`run_v2_driver`], with the plugin process's own arguments `args` (a
    /// JavaScript argument list): `"mini"` makes it read as `opencode mini`,
    /// nothing as the full TUI, since the client mode is read from a process's
    /// own arguments unless it is a `serve`.
    #[cfg(unix)]
    fn run_v2_driver_as(
        args: &str,
        body: &str,
    ) -> Option<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
        // A `serve` runs under the full TUI's client, `opencode`.
        let client = args.contains("\"serve\"").then_some("");
        run_plugin_driver_under(
            &format!(
                "{FAKE_V2_CTX_JS}\ndelete process.env.DOT_AGENT_DECK_PANE_ID;\ndelete process.env.DOT_AGENT_DECK_USAGE_FILE;\nprocess.argv.push({args});\nconst cleanup = await pluginModule.setup(ctx);\n{body}\nprocess.exit(0);\n"
            ),
            &[],
            client,
        )
    }

    /// Scenario (audit L5): the 2.x `setup` reports only from a `serve` whose
    /// parent is an OpenCode client in a session mode. A `serve --stdio`
    /// under a parent that is not OpenCode (a shell sharing a server), or
    /// under an OpenCode running a non-session subcommand, subscribes to
    /// nothing; under `opencode mini`, `opencode run` and the full TUI it
    /// subscribes.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_setup_reports_only_under_an_opencode_session_client() {
        let body = |args: &str| {
            format!(
                "{FAKE_V2_CTX_JS}\nprocess.argv.push({args});\nconst cleanup = await pluginModule.setup(ctx);\nawait sleep(50);\nconsole.log(JSON.stringify({{ subscribed }}));\nprocess.exit(subscribed ? 10 : 0);\n"
            )
        };
        for client in [None, Some("session list"), Some("plugin list")] {
            assert!(
                run_plugin_driver_under(&body(r#""serve", "--stdio""#), &[], client).is_some(),
                "{client:?}: inert, so it exits 0"
            );
        }
        for client in ["mini", "run hi", ""] {
            let driver = format!(
                "{FAKE_V2_CTX_JS}\nprocess.argv.push(\"serve\", \"--stdio\");\nconst cleanup = await pluginModule.setup(ctx);\nawait sleep(50);\nprocess.exit(subscribed ? 0 : 11);\n"
            );
            assert!(
                run_plugin_driver_under(&driver, &[], Some(client)).is_some(),
                "{client:?}: subscribes"
            );
        }
    }

    /// Scenario (PRD #2050, audit N2): the OpenCode binary the client runs
    /// is replaced in place while it runs (`opencode upgrade`), so Linux
    /// reads its executable as `…/opencode (deleted)`. A `serve` under it
    /// still recognises its client as OpenCode, and its `setup` reports as
    /// before the replacement rather than going inert.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_setup_reports_under_a_replaced_opencode_binary() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template("/opt/dot-agent-deck")).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                "import pluginModule from {plugin};\nimport {{ readlinkSync }} from \"fs\";\n{FAKE_V2_CTX_JS}\ndelete process.env.DOT_AGENT_DECK_PANE_ID;\nconst parent = readlinkSync(`/proc/${{process.ppid}}/exe`);\nif (!parent.endsWith(\"/opencode (deleted)\")) {{ console.log(parent); process.exit(12); }}\nprocess.argv.push(\"serve\", \"--stdio\");\nawait pluginModule.setup(ctx);\nawait sleep(50);\nprocess.exit(subscribed ? 0 : 11);\n",
                plugin = serde_json::to_string(&plugin.display().to_string()).unwrap(),
            ),
        )
        .unwrap();
        let shell = opencode_named_shell(dir.path());
        // The client removes its own binary before starting the server, as
        // an in-place upgrade leaves it.
        std::fs::write(
            dir.path().join("client"),
            format!("rm -f '{}'\nnode '{}'\n", shell.display(), driver.display()),
        )
        .unwrap();
        let out = std::process::Command::new(&shell)
            .arg("client")
            .current_dir(dir.path())
            .envs(crate::test_isolation::unreachable_endpoints())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }

    /// The module both generations load: `{ id, server, setup }`, the 1.x
    /// factory as `server` (also still a named export), and a `setup` that
    /// stays inert — subscribes to nothing, registers nothing — outside a
    /// standalone server, where `process.env` is not the pane's.
    #[cfg(unix)]
    #[test]
    fn opencode_module_exports_both_shapes_and_v2_stays_inert_in_a_shared_service() {
        let Some((payloads, _)) = run_plugin_driver_with_awaits(
            &format!(
                "{FAKE_V2_CTX_JS}\nimport * as named from {module};\nconst shape = {{ id: pluginModule.id, server: typeof pluginModule.server, setup: typeof pluginModule.setup, same: named.DotAgentDeckPlugin === pluginModule.server }};\nprocess.argv.push(\"serve\", \"--service\");\nconst cleanup = await pluginModule.setup(ctx);\nawait sleep(50);\nconsole.log(JSON.stringify(shape));\nif (subscribed || Object.keys(hooks.tool).length || Object.keys(hooks.session).length || typeof cleanup !== \"function\") process.exit(3);\nif (shape.id !== \"dot-agent-deck\" || shape.server !== \"function\" || shape.setup !== \"function\" || !shape.same) process.exit(4);\nprocess.exit(0);\n",
                module = "\"./dot-agent-deck.mjs\""
            ),
            &[],
        ) else {
            return;
        };
        assert!(payloads.is_empty(), "{payloads:?}");
    }

    /// Scenario-level drive of 2.x `setup`: a session starts, the user's
    /// prompt arrives through `session.hook("prompt")`, the turn starts, a
    /// `shell` call runs and FAILS, a `read` call completes, a subagent's
    /// child session works and stops, the assistant replies, and the turn goes
    /// idle; a later turn fails on a provider quota and another is
    /// interrupted; unloading flushes the session. The deck receives the same
    /// `hook --agent opencode` payloads it gets from 1.x, with `shell`
    /// reported as `bash`.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_setup_reports_the_1x_payload_vocabulary() {
        let Some((payloads, _)) = run_v2_driver(
            r#"await sleep(50);
await emit("session.created", { sessionID: "s1", location: { directory: "/work" }, slug: "a", version: "2.2.5" });
await hooks.session.prompt({ sessionID: "s1", messageID: "msg-1", prompt: { text: "  list the files  " }, delivery: "steer" });
await emit("session.execution.started", { sessionID: "s1" });
await hooks.tool["execute.before"]({ tool: "shell", sessionID: "s1", agent: "build", messageID: "a1", id: "call-1", input: { command: "ls -la" } });
await hooks.tool["execute.after"]({ tool: "shell", sessionID: "s1", agent: "build", messageID: "a1", id: "call-1", input: { command: "ls -la" }, status: "error", error: { message: "exit 2" } });
await hooks.tool["execute.before"]({ tool: "read", sessionID: "s1", agent: "build", messageID: "a1", id: "call-2", input: { path: "a.txt" } });
await hooks.tool["execute.after"]({ tool: "read", sessionID: "s1", agent: "build", messageID: "a1", id: "call-2", input: { path: "a.txt" }, status: "completed", result: {} });
await emit("session.created", { sessionID: "c1", parentID: "s1", location: { directory: "/work" }, slug: "b", version: "2.2.5" });
await hooks.session.prompt({ sessionID: "c1", messageID: "msg-2", prompt: { text: "subagent task" }, delivery: "queue" });
await emit("session.status", { sessionID: "c1", status: { type: "busy" } });
await hooks.tool["execute.before"]({ tool: "grep", sessionID: "c1", agent: "explore", messageID: "a2", id: "call-3", input: { pattern: "x" } });
await emit("session.idle", { sessionID: "c1" });
await emit("session.text.ended", { sessionID: "s1", assistantMessageID: "a1", ordinal: 0, text: "Here are the files." });
await emit("session.status", { sessionID: "s1", status: { type: "idle" } });
await emit("session.execution.started", { sessionID: "s1" });
await hooks.session["http.response"]({ sessionID: "s1", response: { ok: false, headers: new Map([["Retry-After", "30"], ["x-other", "no"]]) } });
await emit("session.execution.failed", { sessionID: "s1", error: { type: "APIError", message: "You exceeded your quota", status: 429, response: { body: JSON.stringify({ error: { type: "usage_limit_reached", resets_at: 1790001000 } }) } } });
await emit("session.execution.interrupted", { sessionID: "s1", reason: "user" });
await cleanup();
await sleep(50);"#,
        ) else {
            return;
        };
        let events: Vec<&str> = payloads
            .iter()
            .map(|p| p["event"].as_str().unwrap_or(""))
            .collect();
        let find = |event: &str| -> Vec<&serde_json::Value> {
            payloads.iter().filter(|p| p["event"] == event).collect()
        };
        assert_eq!(
            find("session.created")[0]["session_id"],
            "s1",
            "{payloads:#?}"
        );
        let prompts = find("session.prompt");
        assert_eq!(
            prompts.len(),
            1,
            "only the user's prompt, not the subagent's: {payloads:#?}"
        );
        assert_eq!(prompts[0]["prompt"], "list the files");
        let busy = payloads
            .iter()
            .find(|p| p["event"] == "session.status" && p["status"] == "busy")
            .expect("the turn's start reads as working");
        assert_eq!(busy["session_id"], "s1");
        let starts = find("tool.execute.before");
        assert_eq!(
            starts[0]["tool_name"], "bash",
            "2.x `shell` is reported as `bash`"
        );
        assert_eq!(starts[0]["tool_input"]["command"], "ls -la");
        assert_eq!(starts[0]["call_id"], "call-1");
        assert_eq!(starts[1]["tool_name"], "read");
        let ends = find("tool.execute.after");
        assert_eq!(ends[0]["tool_name"], "bash", "the failed call still ends");
        assert_eq!(ends[0]["call_id"], "call-1");
        assert_eq!(ends[1]["call_id"], "call-2");
        let start = &find("subagent.start")[0];
        assert_eq!(
            (&start["session_id"], &start["subagent_id"]),
            (&"s1".into(), &"c1".into())
        );
        let child_tool = starts.iter().find(|p| p["call_id"] == "call-3").unwrap();
        assert_eq!(child_tool["subagent_id"], "c1");
        assert_eq!(find("subagent.stop")[0]["subagent_id"], "c1");
        let idle = payloads
            .iter()
            .find(|p| p["event"] == "session.status" && p["status"] == "idle")
            .unwrap();
        assert_eq!(idle["reply"], "Here are the files.", "{payloads:#?}");
        let errors = find("session.error");
        assert_eq!(errors[0]["error_name"], "APIError");
        assert_eq!(errors[0]["error_message"], "You exceeded your quota");
        assert_eq!(
            errors[0]["response_markers"]["error"]["type"],
            "usage_limit_reached"
        );
        assert_eq!(
            errors[0]["response_headers"],
            serde_json::json!({"retry-after": "30"}),
            "allow-listed headers from session.hook(\"http.response\")"
        );
        assert_eq!(
            errors[1]["error_name"], "MessageAbortedError",
            "an interrupted turn ends as 1.x's abort"
        );
        assert_eq!(
            events.last(),
            Some(&"session.deleted"),
            "unloading flushes the session: {events:?}"
        );
    }

    /// Scenario (review S2): the client's subcommand is found past leading
    /// flags. `opencode --log-level info mini` is mini, so the server plugin
    /// reports a new form itself; `opencode --session mini` names a session
    /// called `mini` in the full TUI, which leaves the form to the TUI half.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_reads_the_subcommand_past_leading_flags() {
        let body = r#"await sleep(50);
await emit("session.created", { sessionID: "s1", location: { directory: "/work" }, slug: "a", version: "2.2.5" });
await emit("form.created", { form: { id: "form-1", sessionID: "s1", title: "Which branch?", fields: [] } });
await sleep(100);"#;
        for (args, reported) in [
            (r#""--log-level", "info", "mini""#, true),
            (r#""--auto", "mini""#, true),
            (r#""--session", "mini""#, false),
        ] {
            let Some((payloads, _)) = run_v2_driver_as(args, body) else {
                return;
            };
            let asked = payloads
                .iter()
                .filter(|p| p["event"] == "question.asked")
                .count();
            assert_eq!(asked, usize::from(reported), "{args}: {payloads:#?}");
        }
    }

    /// 2.x's `permission.asked` is held through `await-answer` like 1.x's,
    /// with its properties given back their 1.x names so the deck's question
    /// builder reads them; its `permission.replied` names the request. A
    /// question form is reported plainly (2.x's server context has no form
    /// reply) and cleared by its reply or cancellation.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_setup_reports_blocked_prompts_and_forms() {
        // `opencode mini`: no TUI plugin, so the server plugin reports forms.
        let Some((payloads, awaits)) = run_v2_driver_as(
            r#""mini""#,
            r#"await sleep(50);
await emit("session.created", { sessionID: "s1", location: { directory: "/work" }, slug: "a", version: "2.2.5" });
await emit("permission.asked", { id: "per-1", sessionID: "s1", action: "bash", resources: ["rm -rf build"], save: ["rm *"], metadata: { command: "rm -rf build" }, source: { type: "tool", messageID: "a1", id: "call-9" } });
await awaited(1);
await emit("permission.replied", { sessionID: "s1", requestID: "per-1", reply: "once" });
await emit("form.created", { form: { id: "form-1", sessionID: "s1", title: "Which branch?", fields: [] } });
await emit("form.replied", { id: "form-1", sessionID: "s1", answer: {} });
await emit("form.created", { form: { id: "form-2", sessionID: "s1", title: "Proceed?", fields: [] } });
await emit("form.cancelled", { id: "form-2", sessionID: "s1" });"#,
        ) else {
            return;
        };
        assert_eq!(awaits.len(), 1, "{awaits:?}");
        let props = &awaits[0]["properties"];
        assert_eq!(awaits[0]["event"], "permission.asked");
        assert_eq!(awaits[0]["session_id"], "s1");
        assert_eq!(props["id"], "per-1");
        assert_eq!(props["permission"], "bash");
        assert_eq!(props["patterns"], serde_json::json!(["rm -rf build"]));
        assert_eq!(props["always"], serde_json::json!(["rm *"]));
        assert_eq!(props["metadata"]["command"], "rm -rf build");
        assert_eq!(props["tool"]["callID"], "call-9");
        let question = crate::question::opencode_permission_asked(props, 0)
            .expect("the deck builds its question from the renamed properties");
        assert_eq!(question.id, "per-1");
        let replied: Vec<_> = payloads
            .iter()
            .filter(|p| p["event"] == "permission.replied")
            .collect();
        assert_eq!(replied[0]["request_id"], "per-1");
        let asked: Vec<_> = payloads
            .iter()
            .filter(|p| p["event"] == "question.asked")
            .collect();
        assert_eq!(asked.len(), 2, "{payloads:#?}");
        assert_eq!(
            asked[0]["prompt"],
            "answer this form in OpenCode: Which branch?"
        );
        assert!(
            asked[0].get("properties").is_none(),
            "a form is shown, not held"
        );
        let cleared: Vec<_> = payloads
            .iter()
            .filter(|p| p["event"] == "question.replied" || p["event"] == "question.rejected")
            .map(|p| {
                (
                    p["event"].as_str().unwrap(),
                    p["request_id"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            cleared,
            vec![
                ("question.replied", "form-1"),
                ("question.rejected", "form-2")
            ]
        );
    }

    /// The deck's answer to a 2.x permission prompt goes back through 2.x's
    /// own `permission.reply`, with the session the prompt came from, and its
    /// outcome is acknowledged to `await-answer`.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_permission_answer_replies_through_the_permission_api() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let acks = dir.path().join("acks.jsonl");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"await-answer\" ]; then\n  IFS= read -r input\n  printf '%s\\n' '{{\"kind\":\"permission\",\"request_id\":\"per-1\",\"body\":{{\"reply\":\"always\"}}}}'\n  IFS= read -r ack\n  printf '%s\\n' \"$ack\" >> '{acks}'\nfi\ncat > /dev/null\nexit 0\n",
                acks = acks.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                "import pluginModule from {plugin};\n{FAKE_V2_CTX_JS}\ndelete process.env.DOT_AGENT_DECK_PANE_ID;\nprocess.argv.push(\"serve\", \"--stdio\");\nawait pluginModule.setup(ctx);\nawait sleep(50);\nawait emit(\"permission.asked\", {{ id: \"per-1\", sessionID: \"s1\", action: \"bash\", resources: [\"ls\"], save: [\"ls\"] }});\nfor (let n = 0; n < 200 && !replies.length; n++) await sleep(20);\nawait sleep(200);\nconsole.log(JSON.stringify(replies));\nprocess.exit(0);\n",
                plugin = serde_json::to_string(&plugin.display().to_string()).unwrap(),
            ),
        )
        .unwrap();
        let out = node_under_opencode(&driver).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let replies: serde_json::Value =
            serde_json::from_str(stdout.lines().last().unwrap_or("null")).unwrap();
        assert_eq!(
            replies,
            serde_json::json!([{"sessionID": "s1", "requestID": "per-1", "decision": "always"}])
        );
        let ack = std::fs::read_to_string(&acks).unwrap_or_default();
        assert!(ack.contains("\"delivered\":true"), "{ack}");
    }

    /// PRD #1858 on 2.x: each step's counts (`session.step.ended`) and model
    /// (`session.step.started`) are written to the deck's usage file as one
    /// assistant message's usage line, the message's steps summed and its
    /// cost added up across them.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_setup_writes_step_usage() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = crate::test_temp::tempdir().unwrap();
        let usage = dir.path().join("usage.jsonl");
        std::fs::write(&usage, b"").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&usage, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(&recorder, b"#!/bin/sh\ncat > /dev/null\nexit 0\n")
            .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                "import pluginModule from {plugin};\n{FAKE_V2_CTX_JS}\ndelete process.env.DOT_AGENT_DECK_PANE_ID;\nprocess.argv.push(\"serve\", \"--stdio\");\nawait pluginModule.setup(ctx);\nawait sleep(50);\nconst tokens = (i, o) => ({{ input: i, output: o, reasoning: 1, cache: {{ read: 2, write: 0 }} }});\nawait emit(\"session.created\", {{ sessionID: \"s1\", location: {{ directory: \"/work\" }}, slug: \"a\", version: \"2.2.5\" }});\nawait emit(\"session.step.started\", {{ sessionID: \"s1\", assistantMessageID: \"a1\", agent: \"build\", model: {{ id: \"claude-haiku-4-5\", providerID: \"anthropic\" }}, started: 1 }});\nawait emit(\"session.step.ended\", {{ sessionID: \"s1\", assistantMessageID: \"a1\", finish: \"tool-calls\", cost: 0.25, tokens: tokens(10, 5) }});\nawait emit(\"session.step.ended\", {{ sessionID: \"s1\", assistantMessageID: \"a1\", finish: \"stop\", cost: 0.5, tokens: tokens(20, 7) }});\nprocess.exit(0);\n",
                plugin = serde_json::to_string(&plugin.display().to_string()).unwrap(),
            ),
        )
        .unwrap();
        let out = node_under_opencode(&driver)
            .env("DOT_AGENT_DECK_USAGE_FILE", &usage)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = std::fs::read_to_string(&usage).unwrap();
        let last: serde_json::Value =
            serde_json::from_str(text.lines().last().expect("a usage line")).unwrap();
        assert_eq!(last["id"], "a1", "{text}");
        assert_eq!(last["model"], "claude-haiku-4-5");
        assert_eq!(last["input"], 30, "the steps' counts are summed: {text}");
        assert_eq!(last["output"], 14, "output includes reasoning: {text}");
        assert_eq!(last["cache_read"], 4);
        assert_eq!(last["cost_usd"], 0.75);
        assert_eq!(last["stop"], "stop");
    }

    /// Under `opencode mini` (2.x loads no TUI plugin there, measured on
    /// 2.2.5), the server plugin is the deck's receiver: a message the deck
    /// hands over starts in mini's session through `session.prompt`, and the
    /// inbox item that call returns is committed when OpenCode delivers it.
    /// Which client the server serves is read from its parent's argv, here a
    /// shell named `opencode mini`.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_mini_delivers_through_session_prompt() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                "import pluginModule from {plugin};\n{FAKE_V2_CTX_JS}\nprocess.argv.push(\"serve\", \"--stdio\");\nconst cleanup = await pluginModule.setup(ctx);\nawait sleep(50);\nawait emit(\"session.created\", {{ sessionID: \"s1\", location: {{ directory: \"/work\" }}, slug: \"a\", version: \"2.2.5\" }});\nfor (let n = 0; n < 150 && !prompts.length; n++) await sleep(20);\nawait sleep(100);\nawait emit(\"session.inbox.delivered\", {{ sessionID: \"s1\", inboxID: \"inbox-1\" }});\nawait sleep(300);\nawait cleanup();\nconsole.log(JSON.stringify({{ prompts }}));\nprocess.exit(0);\n",
                plugin = serde_json::to_string(&plugin.display().to_string()).unwrap(),
            ),
        )
        .unwrap();
        // The server's parent: a shell whose argv is `opencode mini`.
        std::fs::write(
            dir.path().join("mini"),
            format!("node '{}'\n", driver.display()),
        )
        .unwrap();
        let out = std::process::Command::new(opencode_named_shell(dir.path()))
            .arg("mini")
            .current_dir(dir.path())
            .envs(crate::test_isolation::unreachable_endpoints())
            .env("DOT_AGENT_DECK_PANE_ID", "pane-1")
            .env("DOT_AGENT_DECK_PANE_CAPABILITY", "a".repeat(64))
            .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
            .output()
            .expect("run node");
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let result: serde_json::Value =
            serde_json::from_str(stdout.lines().last().unwrap_or("null")).unwrap();
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(prompts.len(), 1, "{result}");
        assert_eq!(prompts[0]["sessionID"], "s1");
        let text = prompts[0]["text"].as_str().unwrap();
        assert!(
            text.contains("[dot-agent-deck message m-k3xzq2ab-0f3c9a71d2e4b850"),
            "the deck's framed message: {text}"
        );
        let written = std::fs::read_to_string(&controls).unwrap_or_default();
        assert!(written.contains("\"handed\""), "{written}");
        assert!(
            written.contains("\"committed\""),
            "delivered, so committed: {written}"
        );
        assert!(
            !written.contains("call inbox announce"),
            "mini is the receiver itself, it does not announce: {written}"
        );
    }

    /// Run a 2.x mini driver: `body` after the module's `setup` with
    /// [`FAKE_V2_CTX_JS`] plus `ctx.session.list` answering `sessions` (newest
    /// first, as `order: "desc"` does) and recording each input in `lists`,
    /// under a server whose parent's argv is `opencode mini <client_args>`.
    /// The driver's last stdout line, parsed, and the recorder's controls.
    #[cfg(target_os = "linux")]
    fn run_v2_mini_driver(client_args: &str, body: &str) -> (serde_json::Value, String) {
        run_v2_mini_driver_timed(client_args, body, None)
    }

    /// [`run_v2_mini_driver`], with the deck's message handed over
    /// `message_delay` seconds after the receiver registers, when given.
    #[cfg(target_os = "linux")]
    fn run_v2_mini_driver_timed(
        client_args: &str,
        body: &str,
        message_delay: Option<&str>,
    ) -> (serde_json::Value, String) {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, controls) = inbox_recorder_timed(dir.path(), None, message_delay);
        let plugin = dir.path().join("dot-agent-deck.mjs");
        std::fs::write(&plugin, plugin_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                "import pluginModule from {plugin};\n{FAKE_V2_CTX_JS}\nconst lists = [];\nlet sessions = [];\nctx.session.list = async (input) => {{ lists.push(input); return {{ data: sessions, cursor: {{}} }}; }};\nconst now = Date.now();\nconst pluginURL = (await import(\"url\")).pathToFileURL({plugin}).href;\nprocess.argv.push(\"serve\", \"--stdio\");\n{body}\nprocess.exit(0);\n",
                plugin = serde_json::to_string(&plugin.display().to_string()).unwrap(),
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("mini"),
            format!("node '{}'\n", driver.display()),
        )
        .unwrap();
        let out = std::process::Command::new(opencode_named_shell(dir.path()))
            .arg("mini")
            .args(client_args.split_whitespace())
            .current_dir(dir.path())
            .envs(crate::test_isolation::unreachable_endpoints())
            .env("DOT_AGENT_DECK_PANE_ID", "pane-1")
            .env("DOT_AGENT_DECK_PANE_CAPABILITY", "a".repeat(64))
            .env("DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", "3600000")
            .output()
            .expect("run node");
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let result = serde_json::from_str(stdout.lines().last().unwrap_or("null")).unwrap();
        (
            result,
            std::fs::read_to_string(&controls).unwrap_or_default(),
        )
    }

    /// Scenario (PRD #2050, inbox/opencode/020 on 2.2.5): a fresh `opencode
    /// mini` created its session before the deck's server plugin subscribed,
    /// so no `session.created` reaches the plugin. A message the deck hands
    /// over still starts in mini's session: the plugin finds it the way mini
    /// does — the newest root session of the directory created since this
    /// server started — and passes over an older root and a newer subagent
    /// session. Mini's own session appears in the listing only after the
    /// message arrives, and is found on a later look.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_mini_delivers_into_the_session_it_started_with() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let (result, written) = run_v2_mini_driver(
            "",
            r#"sessions = [
  { id: "ses_child", parentID: "ses_mini", time: { created: now + 5 }, location: { directory: "/work" } },
  { id: "ses_old", time: { created: now - 3600000 }, location: { directory: "/work" } },
];
const cleanup = await pluginModule.setup(ctx);
await sleep(700);
const early = prompts.length;
sessions = [sessions[0], { id: "ses_mini", time: { created: now }, location: { directory: "/work" } }, sessions[1]];
for (let n = 0; n < 150 && !prompts.length; n++) await sleep(20);
await sleep(100);
await cleanup();
console.log(JSON.stringify({ early, prompts, lists }));"#,
        );
        assert_eq!(result["early"], 0, "no older root is mini's: {result}");
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(prompts.len(), 1, "{result}");
        assert_eq!(prompts[0]["sessionID"], "ses_mini", "{result}");
        assert_eq!(
            result["lists"][0],
            serde_json::json!({"directory": "/work", "parentID": null, "limit": 20, "order": "desc"}),
            "{result}"
        );
        assert!(written.contains("\"handed\""), "{written}");
    }

    /// Scenario (PRD #2050): `opencode mini --session <id>` shows that session
    /// and `--continue` the newest root session of the directory, which is
    /// where the deck's message starts when the plugin saw neither created.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_mini_delivers_into_a_named_or_continued_session() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let body = r#"sessions = [
  { id: "ses_newest", time: { created: now - 60000 }, location: { directory: "/work" } },
  { id: "ses_older", time: { created: now - 3600000 }, location: { directory: "/work" } },
];
const cleanup = await pluginModule.setup(ctx);
for (let n = 0; n < 150 && !prompts.length; n++) await sleep(20);
await sleep(100);
await cleanup();
console.log(JSON.stringify({ prompts, lists: lists.length }));"#;
        for (args, expected) in [
            ("--session ses_named", "ses_named"),
            ("-s ses_named", "ses_named"),
            ("--continue", "ses_newest"),
        ] {
            let (result, _) = run_v2_mini_driver(args, body);
            let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
            assert_eq!(prompts.len(), 1, "{args}: {result}");
            assert_eq!(prompts[0]["sessionID"], expected, "{args}: {result}");
        }
    }

    /// Scenario (PRD #2050, review B2): two OpenCode 2 minis start in one
    /// directory, and both create their sessions, 1.4 s apart, before this
    /// mini's receiver starts (a slow boot). Both are candidates, so the
    /// plugin cannot tell which is its own: it prompts neither and reports
    /// the deck's message not handed, which leaves it with the deck rather
    /// than looping on it.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_two_minis_started_together_report_the_message_not_handed() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let (result, written) = run_v2_mini_driver(
            "",
            r#"const started = performance.timeOrigin;
sessions = [
  { id: "ses_other_mini", time: { created: started + 1500 }, location: { directory: "/work" } },
  { id: "ses_this_mini", time: { created: started + 100 }, location: { directory: "/work" } },
];
await sleep(Math.max(0, started + 1600 - Date.now()));
const cleanup = await pluginModule.setup(ctx);
await sleep(1500);
await cleanup();
console.log(JSON.stringify({ prompts, lists: lists.length }));"#,
        );
        assert_eq!(
            result["prompts"],
            serde_json::json!([]),
            "two roots created before setup are ambiguous: {result}"
        );
        assert!(!written.contains("\"handed\""), "{written}");
        assert!(
            written.contains("\"not_handed\""),
            "the message is reported not handed, not held in a loop: {written}"
        );
    }

    /// Scenario (PRD #2050, review R2 and audit N1): this mini's server is
    /// slow to boot, so its own session is created 2.5 s after the server
    /// started, while a sibling mini started a second later in the same
    /// directory created its session at 1.3 s. This mini's receiver starts
    /// after both. The sibling's session is older, but the plugin never takes
    /// a session by age: it cannot tell the two apart, so it prompts neither,
    /// and the deck's message is reported not handed instead of reaching the
    /// sibling's session.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_a_slow_mini_never_takes_a_siblings_earlier_session() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let (result, written) = run_v2_mini_driver(
            "",
            r#"const started = performance.timeOrigin;
sessions = [
  { id: "ses_this_mini", time: { created: started + 2500 }, location: { directory: "/work" } },
  { id: "ses_sibling", time: { created: started + 1300 }, location: { directory: "/work" } },
];
await sleep(Math.max(0, started + 2700 - Date.now()));
const cleanup = await pluginModule.setup(ctx);
await sleep(1500);
await cleanup();
console.log(JSON.stringify({ prompts }));"#,
        );
        assert_eq!(
            result["prompts"],
            serde_json::json!([]),
            "nothing reaches the sibling's session: {result}"
        );
        assert!(!written.contains("\"handed\""), "{written}");
        assert!(written.contains("\"not_handed\""), "{written}");
    }

    /// Scenario (PRD #2050, review R2): two OpenCode 2 minis start in one
    /// directory 1.4 s apart, and this one's receiver started before the
    /// other created its session. Only this mini's session falls in its
    /// window, so the deck's message starts there.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_a_sibling_created_after_setup_is_not_a_candidate() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let (result, written) = run_v2_mini_driver(
            "",
            r#"const started = performance.timeOrigin;
sessions = [
  { id: "ses_this_mini", time: { created: started }, location: { directory: "/work" } },
];
const cleanup = await pluginModule.setup(ctx);
sessions = [{ id: "ses_other_mini", time: { created: Date.now() + 1400 }, location: { directory: "/work" } }, ...sessions];
for (let n = 0; n < 200 && !prompts.length; n++) await sleep(20);
await sleep(100);
await cleanup();
console.log(JSON.stringify({ prompts }));"#,
        );
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(prompts.len(), 1, "{result}");
        assert_eq!(prompts[0]["sessionID"], "ses_this_mini", "{result}");
        assert!(written.contains("\"handed\""), "{written}");
    }

    /// Scenario (PRD #2050, audit M1): an OpenCode 2 mini found its session
    /// and took a deck message there; later another root session is created
    /// in the same directory, and the deck's module is re-imported in the same
    /// server process (OpenCode 2 reloads the plugin when the module file
    /// changes). The next deck message still starts in the mini's own session
    /// rather than being refused as ambiguous.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_mini_keeps_its_session_across_a_plugin_reload() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let (result, written) = run_v2_mini_driver(
            "",
            r#"const started = performance.timeOrigin;
sessions = [
  { id: "ses_this_mini", time: { created: started + 100 }, location: { directory: "/work" } },
];
const cleanup = await pluginModule.setup(ctx);
for (let n = 0; n < 200 && !prompts.length; n++) await sleep(20);
const first = prompts.length;
await sleep(700);
sessions = [{ id: "ses_later", time: { created: Date.now() }, location: { directory: "/work" } }, ...sessions];
await cleanup();
const reloaded = (await import(pluginURL + "?reload=1")).default;
const cleanupReloaded = await reloaded.setup(ctx);
for (let n = 0; n < 200 && prompts.length < 2; n++) await sleep(20);
await sleep(100);
await cleanupReloaded();
console.log(JSON.stringify({ first, prompts }));"#,
        );
        assert_eq!(
            result["first"], 1,
            "the first message reached mini: {result}"
        );
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            prompts.len(),
            2,
            "the reloaded plugin delivers too: {result}"
        );
        assert_eq!(prompts[0]["sessionID"], "ses_this_mini", "{result}");
        assert_eq!(prompts[1]["sessionID"], "ses_this_mini", "{result}");
        assert!(!written.contains("\"not_handed\""), "{written}");
    }

    /// Scenario (PRD #2050, round-3 re-review F1): two OpenCode 2 minis start
    /// in one directory three seconds apart, the usual staggered orchestration
    /// launch. The earlier mini's server lists both fresh roots, and only its
    /// own was created before its receiver started; the later mini's server
    /// lists the earlier one's root as created before it started. Each
    /// delivers into its own session.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_two_minis_started_apart_each_take_their_own_session() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        // The earlier mini: its own root just after its start, the later
        // mini's three seconds after.
        let (result, written) = run_v2_mini_driver(
            "",
            r#"const started = performance.timeOrigin;
sessions = [
  { id: "ses_later_mini", time: { created: started + 3100 }, location: { directory: "/work" } },
  { id: "ses_earlier_mini", time: { created: started + 100 }, location: { directory: "/work" } },
];
const cleanup = await pluginModule.setup(ctx);
for (let n = 0; n < 200 && !prompts.length; n++) await sleep(20);
await sleep(100);
await cleanup();
console.log(JSON.stringify({ prompts }));"#,
        );
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(prompts.len(), 1, "{result}");
        assert_eq!(prompts[0]["sessionID"], "ses_earlier_mini", "{result}");
        assert!(written.contains("\"handed\""), "{written}");

        // The later mini: the earlier one's root was created 2.9 s before its
        // own server started.
        let (result, written) = run_v2_mini_driver(
            "",
            r#"const started = performance.timeOrigin;
sessions = [
  { id: "ses_later_mini", time: { created: started + 100 }, location: { directory: "/work" } },
  { id: "ses_earlier_mini", time: { created: started - 2900 }, location: { directory: "/work" } },
];
const cleanup = await pluginModule.setup(ctx);
for (let n = 0; n < 200 && !prompts.length; n++) await sleep(20);
await sleep(100);
await cleanup();
console.log(JSON.stringify({ prompts }));"#,
        );
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(prompts.len(), 1, "{result}");
        assert_eq!(prompts[0]["sessionID"], "ses_later_mini", "{result}");
        assert!(written.contains("\"handed\""), "{written}");
    }

    /// Scenario (PRD #2050, round-3 re-review F1): a later root session in the
    /// directory appears only after this mini's lookup settled (another pane's
    /// OpenCode, started a minute later). The lookup ran as mini started and
    /// kept its answer, so the deck's message still starts in mini's own
    /// session, and the listing is not asked again for the message.
    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_v2_mini_keeps_the_session_it_found_at_start() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let (result, written) = run_v2_mini_driver_timed(
            "",
            r#"const started = performance.timeOrigin;
sessions = [
  { id: "ses_this_mini", time: { created: started + 100 }, location: { directory: "/work" } },
];
const cleanup = await pluginModule.setup(ctx);
await sleep(2600);
sessions = [{ id: "ses_newcomer", time: { created: Date.now() }, location: { directory: "/work" } }, ...sessions];
const looks = lists.length;
for (let n = 0; n < 200 && !prompts.length; n++) await sleep(20);
await sleep(100);
await cleanup();
console.log(JSON.stringify({ prompts, looks, lists: lists.length }));"#,
            Some("3.5"),
        );
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(prompts.len(), 1, "{result}");
        assert_eq!(prompts[0]["sessionID"], "ses_this_mini", "{result}");
        assert_eq!(
            result["looks"], result["lists"],
            "the answer found at start is kept: {result}"
        );
        assert!(written.contains("\"handed\""), "{written}");
    }

    /// Scenario (PRD #2050, inbox/opencode/016 on 2.2.5): the person has an
    /// unsent draft on OpenCode 2's home screen when the deck hands over a
    /// message. The TUI half stashes the draft with OpenCode's own
    /// `prompt.stash`, opens a new session for the message, and once that
    /// session's composer is reachable pops the draft into it, so the draft
    /// survives as on 1.x. With no draft, nothing is stashed or popped.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_tui_carries_the_home_draft_into_the_new_session() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        for draft in ["DRAFT-KEEP-71b3", ""] {
            let dir = tempfile::tempdir().unwrap();
            let (recorder, controls) = inbox_recorder(dir.path());
            let plugin = dir.path().join("tui.mjs");
            std::fs::write(&plugin, tui_v2_template(&recorder.to_string_lossy())).unwrap();
            let driver = dir.path().join("driver.mjs");
            std::fs::write(
                &driver,
                format!(
                    r#"const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const prompts = [];
const navigations = [];
const dispatched = [];
const handlers = new Map();
let route = {{ type: "home" }};
// The composers: home's, then the new session's, which mounts a few looks
// after navigating (measured: about 0.1 s), and OpenCode's stash.
const composer = {{ home: {draft}, session: "" }};
const stash = [];
let looksSinceNavigate = 0;
const mounted = () => route.type === "home" ? "home" : (++looksSinceNavigate > 3 ? "session" : undefined);
const api = {{
  data: {{
    session: {{ get: (id) => ({{ id }}) }},
    on: (type, fn) => {{ handlers.set(type, fn); return () => handlers.delete(type); }},
  }},
  keymap: {{
    mode: {{ current: () => "base" }},
    commands: () => {{
      const where = mounted();
      const list = [{{ id: "session.new" }}];
      if (route.type === "session") list.push({{ id: "session.compact" }});
      if (where) {{
        list.push({{ id: "prompt.stash", enabled: !!composer[where] }});
        list.push({{ id: "prompt.stash.pop", enabled: stash.length > 0 }});
      }}
      return list;
    }},
    dispatch: (id) => {{
      dispatched.push(id);
      const where = mounted();
      if (!where) return;
      if (id === "prompt.stash" && composer[where]) {{ stash.push(composer[where]); composer[where] = ""; }}
      if (id === "prompt.stash.pop" && stash.length) composer[where] = stash.pop();
    }},
  }},
  ui: {{ router: {{
    current: () => route,
    navigate: (to) => {{ navigations.push(to.sessionID); route = to; looksSinceNavigate = 0; }},
  }} }},
  client: {{ session: {{
    create: async () => ({{ id: "fresh" }}),
    prompt: async (input) => {{ prompts.push(input); return {{ id: "inbox-1", sessionID: input.sessionID, type: "user" }}; }},
  }} }},
}};
const {{ default: mod }} = await import("{plugin}");
const cleanup = await mod.setup(api);
for (let n = 0; n < 150 && !prompts.length; n++) await sleep(20);
await sleep(200);
cleanup?.();
console.log(JSON.stringify({{ prompts, navigations, dispatched, composer, stash }}));
process.exit(0);
"#,
                    draft = serde_json::to_string(draft).unwrap(),
                    plugin = plugin.display()
                ),
            )
            .unwrap();
            let result = run_inbox_driver(&driver);
            assert_eq!(
                result["navigations"],
                serde_json::json!(["fresh"]),
                "{result}"
            );
            let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
            assert_eq!(prompts.len(), 1, "{result}");
            assert_eq!(prompts[0]["sessionID"], "fresh", "{result}");
            assert_eq!(result["stash"], serde_json::json!([]), "{result}");
            if draft.is_empty() {
                assert_eq!(result["dispatched"], serde_json::json!([]), "{result}");
            } else {
                assert_eq!(
                    result["dispatched"],
                    serde_json::json!(["prompt.stash", "prompt.stash.pop"]),
                    "{result}"
                );
                assert_eq!(result["composer"]["session"], draft, "{result}");
            }
            let written = std::fs::read_to_string(&controls).unwrap_or_default();
            assert!(written.contains("\"handed\""), "{written}");
        }
    }

    /// OpenCode 2.x's TUI half, driven with a fake 2.x TUI API: a message the
    /// deck hands over while a subagent's session is on screen waits for the
    /// open dialog to close, then starts in that session's root (shown
    /// first) through `client.session.prompt`, and is committed when OpenCode
    /// delivers the inbox item that call returned.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_tui_delivers_to_the_root_and_commits_on_delivery() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (recorder, controls) = inbox_recorder(dir.path());
        let plugin = dir.path().join("tui.mjs");
        std::fs::write(&plugin, tui_v2_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const prompts = [];
const navigations = [];
const handlers = new Map();
let mode = "dialog";
let route = {{ type: "session", sessionID: "child" }};
const sessions = {{ child: {{ id: "child", parentID: "rootA" }}, rootA: {{ id: "rootA" }} }};
const api = {{
  data: {{
    session: {{ get: (id) => sessions[id] }},
    on: (type, fn) => {{ handlers.set(type, fn); return () => handlers.delete(type); }},
  }},
  keymap: {{ mode: {{ current: () => mode }} }},
  ui: {{ router: {{
    current: () => route,
    navigate: (to) => {{ navigations.push(to.sessionID); route = to; }},
  }} }},
  client: {{ session: {{
    create: async () => ({{ id: "fresh" }}),
    prompt: async (input) => {{ prompts.push(input); return {{ id: "inbox-1", sessionID: input.sessionID, type: "user" }}; }},
  }} }},
}};
const {{ default: mod }} = await import("{plugin}");
if (mod.id !== "dot-agent-deck-tui" || typeof mod.setup !== "function") process.exit(5);
const cleanup = await mod.setup(api);
await sleep(500);
const heldByDialog = prompts.length === 0;
mode = "base";
for (let n = 0; n < 150 && !prompts.length; n++) await sleep(20);
handlers.get("session.inbox.delivered")?.({{ type: "session.inbox.delivered", data: {{ sessionID: "rootA", inboxID: "inbox-1" }} }});
await sleep(300);
cleanup?.();
console.log(JSON.stringify({{ heldByDialog, prompts, navigations }}));
process.exit(0);
"#,
                plugin = plugin.display()
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        assert_eq!(result["heldByDialog"], true, "{result}");
        assert_eq!(
            result["navigations"],
            serde_json::json!(["rootA"]),
            "{result}"
        );
        let prompts = result["prompts"].as_array().cloned().unwrap_or_default();
        assert_eq!(prompts.len(), 1, "{result}");
        assert_eq!(prompts[0]["sessionID"], "rootA");
        assert!(
            prompts[0]["text"]
                .as_str()
                .unwrap()
                .contains("m-k3xzq2ab-0f3c9a71d2e4b850")
        );
        let written = std::fs::read_to_string(&controls).unwrap_or_default();
        assert!(written.contains("\"handed\""), "{written}");
        assert!(written.contains("\"committed\""), "{written}");
    }

    /// Scenario: OpenCode 2's TUI half, driven with a fake 2.x TUI context,
    /// sees a question form created in a subagent's session. It hands the form
    /// to `await-answer --ack` under the root session, replies with the deck's
    /// answer through `data.session.form.reply`, and acknowledges OpenCode's
    /// outcome to the child; a second form that OpenCode reports answered
    /// before the deck does stops its child (PRD #2050, rule 20).
    #[cfg(unix)]
    #[test]
    fn opencode_v2_tui_holds_a_form_and_replies_with_the_decks_answer() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let awaits = dir.path().join("awaits.log");
        let acks = dir.path().join("acks.log");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                r#"#!/bin/sh
echo "call $*" >> '{calls}'
if [ "$1" = "await-answer" ]; then
  IFS= read -r input
  printf '%s\n' "$input" >> '{awaits}'
  case "$input" in
    *frm_1*)
      printf '%s\n' '{{"kind":"form","request_id":"frm_1","body":{{"sessionID":"child","answer":{{"ok":true}}}}}}'
      IFS= read -r ack
      printf '%s\n' "$ack" >> '{acks}';;
    *) exec sleep 37;;
  esac
  exit 0
fi
cat > /dev/null
exit 0
"#,
                awaits = awaits.display(),
                acks = acks.display(),
                calls = dir.path().join("calls.log").display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("tui.mjs");
        std::fs::write(&plugin, tui_v2_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import {{ execSync }} from "child_process";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const replies = [];
const handlers = new Map();
const api = {{
  data: {{
    session: {{
      get: (id) => ({{ id }}),
      root: (id) => (id === "child" ? "rootA" : id),
      form: {{ reply: async (input) => {{ replies.push(input); }} }},
    }},
    on: (type, fn) => {{ handlers.set(type, fn); return () => handlers.delete(type); }},
  }},
  keymap: {{ mode: {{ current: () => "base" }} }},
  ui: {{ router: {{ current: () => ({{ type: "home" }}), navigate: () => {{}} }} }},
  client: {{ session: {{ create: async () => ({{ id: "x" }}), prompt: async () => ({{}}) }} }},
}};
const {{ default: mod }} = await import("{plugin}");
const cleanup = await mod.setup(api);
const emit = (type, data) => handlers.get(type)?.({{ type, data }});
emit("form.created", {{ form: {{ id: "frm_1", sessionID: "child", title: "Go?", fields: [{{ key: "ok", type: "boolean" }}] }} }});
for (let n = 0; n < 200 && !replies.length; n++) await sleep(20);
emit("form.created", {{ form: {{ id: "frm_2", sessionID: "rootA", title: "Later", fields: [{{ key: "x", type: "string" }}] }} }});
await sleep(400);
emit("form.replied", {{ id: "frm_2", sessionID: "rootA", answer: {{ x: "typed" }} }});
await sleep(400);
const sleepers = execSync("ps -eo args").toString().split(String.fromCharCode(10)).filter((l) => l.includes("sleep 37")).length;
cleanup?.();
console.log(JSON.stringify({{ replies, sleepers }}));
process.exit(0);
"#,
                plugin = plugin.display()
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        assert_eq!(
            result["replies"],
            serde_json::json!([{"sessionID": "child", "formID": "frm_1", "answer": {"ok": true}}]),
            "{result}; recorder calls: {:?}",
            std::fs::read_to_string(dir.path().join("calls.log")),
        );
        let awaited: Vec<serde_json::Value> = std::fs::read_to_string(&awaits)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(awaited.len(), 2, "{awaited:?}");
        assert_eq!(awaited[0]["event"], "question.asked");
        assert_eq!(awaited[0]["session_id"], "rootA", "filed under the root");
        assert_eq!(awaited[0]["properties"]["id"], "frm_1");
        assert_eq!(awaited[0]["properties"]["sessionID"], "child");
        assert_eq!(
            awaited[0]["properties"]["form"]["fields"][0]["type"],
            "boolean"
        );
        let acked = std::fs::read_to_string(&acks).unwrap_or_default();
        assert!(acked.contains("\"delivered\":true"), "{acked}");
        assert_eq!(
            result["sleepers"], 0,
            "the answered form's child was stopped: {result}"
        );
    }

    /// Scenario (audit I2): a model creates twenty question forms at once in
    /// OpenCode 2's full TUI. The TUI half holds at most sixteen through
    /// `await-answer` children and reports the rest plainly, so the card still
    /// lists them as answered in OpenCode; unloading stops every child.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_tui_caps_the_forms_it_holds_at_once() {
        if !node_available() {
            eprintln!("SKIP: node is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls.log");
        let recorder = dir.path().join("recorder.sh");
        crate::test_isolation::write_script(
            &recorder,
            format!(
                "#!/bin/sh\necho \"$1\" >> '{calls}'\nif [ \"$1\" = \"await-answer\" ]; then\n  IFS= read -r input\n  exec sleep 39\nfi\ncat > /dev/null\nexit 0\n",
                calls = calls.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugin = dir.path().join("tui.mjs");
        std::fs::write(&plugin, tui_v2_template(&recorder.to_string_lossy())).unwrap();
        let driver = dir.path().join("driver.mjs");
        std::fs::write(
            &driver,
            format!(
                r#"import {{ execSync }} from "child_process";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const handlers = new Map();
const api = {{
  data: {{
    session: {{ get: (id) => ({{ id }}), root: (id) => id, form: {{ reply: async () => {{}} }} }},
    on: (type, fn) => {{ handlers.set(type, fn); return () => handlers.delete(type); }},
  }},
  keymap: {{ mode: {{ current: () => "base" }} }},
  ui: {{ router: {{ current: () => ({{ type: "home" }}), navigate: () => {{}} }} }},
  client: {{ session: {{ create: async () => ({{ id: "x" }}), prompt: async () => ({{}}) }} }},
}};
const {{ default: mod }} = await import("{plugin}");
const cleanup = await mod.setup(api);
for (let n = 1; n <= 20; n++) {{
  handlers.get("form.created")?.({{ type: "form.created", data: {{ form: {{ id: "frm_" + n, sessionID: "s1", title: "Q" + n, fields: [{{ key: "ok", type: "boolean" }}] }} }} }});
}}
await sleep(1500);
const count = () => execSync("ps -eo args").toString().split(String.fromCharCode(10)).filter((l) => l.includes("sleep 39")).length;
const held = count();
cleanup?.();
await sleep(500);
console.log(JSON.stringify({{ held, after: count() }}));
process.exit(0);
"#,
                plugin = plugin.display()
            ),
        )
        .unwrap();
        let result = run_inbox_driver(&driver);
        let logged = std::fs::read_to_string(&calls).unwrap_or_default();
        let awaits = logged.lines().filter(|l| *l == "await-answer").count();
        let plain = logged.lines().filter(|l| *l == "hook").count();
        assert_eq!(awaits, 16, "{logged}");
        assert_eq!(plain, 4, "{logged}");
        assert_eq!(result["held"], 16, "{result}");
        assert_eq!(result["after"], 0, "{result}");
    }

    /// Scenario: in the full TUI the server plugin leaves a new form to the
    /// TUI half (which holds it), and still reports the form's reply and
    /// cancellation so the card clears (PRD #2050). Under `opencode mini` it
    /// reports it itself (`opencode_v2_setup_reports_blocked_prompts_and_forms`).
    #[cfg(unix)]
    #[test]
    fn opencode_v2_server_leaves_a_new_form_to_the_tui_half_in_the_full_tui() {
        let Some((payloads, awaits)) = run_v2_driver_as(
            "",
            r#"await sleep(50);
await emit("session.created", { sessionID: "s1", location: { directory: "/work" }, slug: "a", version: "2.2.5" });
await emit("form.created", { form: { id: "form-1", sessionID: "s1", title: "Which branch?", fields: [] } });
await emit("form.replied", { id: "form-1", sessionID: "s1", answer: {} });
await sleep(200);"#,
        ) else {
            return;
        };
        assert!(awaits.is_empty(), "{awaits:?}");
        let events: Vec<_> = payloads
            .iter()
            .map(|p| p["event"].as_str().unwrap_or(""))
            .collect();
        assert!(!events.contains(&"question.asked"), "{payloads:#?}");
        assert!(events.contains(&"question.replied"), "{payloads:#?}");
    }

    /// The event stream real OpenCode 2.2.5 sends (measured with `opencode
    /// run --standalone`): no `session.created`, no `session.status`, no
    /// `session.idle`; a turn is `session.execution.started` … `succeeded`.
    /// The deck still gets the session, the prompt, working, and the turn's
    /// end with its reply; and a subagent's session, known only by asking
    /// `session.get`, is reported as the subagent's, its prompt not as the
    /// user's.
    #[cfg(unix)]
    #[test]
    fn opencode_v2_setup_reads_turns_from_execution_events() {
        let Some((payloads, _)) = run_v2_driver(
            r#"parents.c9 = "s1";
await sleep(50);
await hooks.session.prompt({ sessionID: "s1", messageID: "msg-1", prompt: { text: "Reply with exactly PONG." }, delivery: "steer" });
await emit("session.inbox.enqueued", { sessionID: "s1", inboxID: "msg-1", item: { type: "user", payload: { text: "Reply with exactly PONG." }, delivery: "steer" } });
await emit("session.execution.started", { sessionID: "s1" });
await emit("session.inbox.delivered", { sessionID: "s1", inboxID: "msg-1" });
await hooks.session.prompt({ sessionID: "c9", messageID: "msg-2", prompt: { text: "explore the repo" }, delivery: "steer" });
await emit("session.execution.started", { sessionID: "c9" });
await emit("session.execution.succeeded", { sessionID: "c9" });
await emit("session.step.started", { sessionID: "s1", assistantMessageID: "a1", agent: "build", model: { id: "big-pickle", providerID: "opencode" }, started: 1 });
await emit("session.text.ended", { sessionID: "s1", assistantMessageID: "a1", ordinal: 0, text: "PONG" });
await emit("session.step.ended", { sessionID: "s1", assistantMessageID: "a1", finish: "stop", cost: 0, tokens: { input: 82, output: 3, reasoning: 0, cache: { read: 5914, write: 0 } } });
await emit("session.execution.succeeded", { sessionID: "s1" });
await emit("session.idle", { sessionID: "s1" });
await emit("location.shutdown", {});"#,
        ) else {
            return;
        };
        let events: Vec<(String, String)> = payloads
            .iter()
            .map(|p| {
                (
                    p["event"].as_str().unwrap_or("").to_string(),
                    p["status"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert_eq!(payloads[0]["event"], "session.created", "{events:?}");
        let prompts: Vec<_> = payloads
            .iter()
            .filter(|p| p["event"] == "session.prompt")
            .collect();
        assert_eq!(
            prompts.len(),
            1,
            "the subagent's prompt is not the user's: {events:?}"
        );
        assert_eq!(prompts[0]["prompt"], "Reply with exactly PONG.");
        let start = payloads
            .iter()
            .find(|p| p["event"] == "subagent.start")
            .expect("subagent start");
        assert_eq!(
            (&start["session_id"], &start["subagent_id"]),
            (&"s1".into(), &"c9".into())
        );
        assert!(
            payloads
                .iter()
                .any(|p| p["event"] == "subagent.stop" && p["subagent_id"] == "c9")
        );
        let idles: Vec<_> = payloads
            .iter()
            .filter(|p| {
                p["session_id"] == "s1" && (p["status"] == "idle" || p["event"] == "session.idle")
            })
            .collect();
        assert_eq!(
            idles[0]["reply"], "PONG",
            "the turn's end carries its reply: {events:?}"
        );
        assert!(
            idles.iter().skip(1).all(|p| p.get("reply").is_none()),
            "the reply is sent once: {events:?}"
        );
        assert!(
            payloads.iter().any(|p| p["event"] == "session.status"
                && p["status"] == "busy"
                && p["session_id"] == "s1"),
            "{events:?}"
        );
        assert_eq!(
            events.last().map(|e| e.0.as_str()),
            Some("session.deleted"),
            "{events:?}"
        );
    }
}
