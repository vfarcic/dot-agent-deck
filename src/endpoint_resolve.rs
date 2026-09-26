//! The pieces of endpoint **I/O** that issue #1121 needed, kept out of
//! [`crate::platform::paths`] so that module's resolvers stay pure.
//!
//! - [`ensure_endpoint_dir`] — create the owner-only fallback directory. Called
//!   by the side that is about to **bind**, and by nothing else.
//! - [`client_socket_path`] / [`client_attach_socket_path`] — the **connect**
//!   side's resolution, which additionally consults the pre-#1121 endpoint
//!   spelling so a newer build still finds an older build's running daemon.
//! - [`legacy_hook_alias`] / [`legacy_attach_alias`], [`prepare_legacy_alias`]
//!   and [`LegacyAlias`] — the **bind** side's mirror image of that read
//!   (issue #1211): `daemon serve` also binds the pre-#1121 spelling,
//!   best-effort, so an older build's client still finds a newer build's
//!   daemon. See [Why the daemon also binds the old spelling](#why-the-daemon-also-binds-the-old-spelling).
//! - [`prepare_bind_endpoints`] — where a daemon about to start binds, which
//!   is the resolved endpoint except when another uid holds the per-uid
//!   directory's name (issue #1173). See [When the per-uid directory is taken](#when-the-per-uid-directory-is-taken).
//!
//! # Why the connect side looks in two places
//!
//! Issue #1121 moved the `XDG_RUNTIME_DIR`-less fallback endpoints from
//! `<temp dir>/dot-agent-deck[-attach]-{uid}.sock` into
//! `<temp dir>/dot-agent-deck-{uid}/{hook,attach}.sock`. Without a compatibility
//! read the upgrade is silent in the worst way: an older daemon binds the old
//! path, a newer TUI looks only at the new one, the two never meet, and the TUI
//! lazy-spawns a *second* daemon with the first one's agents stranded under it.
//! PRD #103/#161's build-version handshake would normally catch exactly this
//! kind of skew and prompt — but it runs over a connection that, in that story,
//! is never made. Reading the legacy path restores the connection, so the
//! handshake fires and the user gets the prompt they get from any other version
//! skew.
//!
//! **The connect side treats the legacy path as read-only: it never creates
//! it, and never unlinks or lazy-spawns at an address it chose by the
//! compatibility read.** That is what stops the squatting problem this issue
//! fixes from simply moving to the compatibility path. A foreign entry planted
//! there can at worst fail
//! [`crate::platform::fsperm::verify_endpoint_trusted`]'s `lstat` (issue
//! #1020), and we fall through to the new path and lazy-spawn there. (Until
//! issue #1211 this paragraph said "never bound" of the whole deck; the daemon
//! now binds the old spelling as a best-effort alias, and the section below
//! says why that does not reopen the wedge.)
//!
//! The unlink clause used to read "never unlinked" and was one quantifier too
//! wide. This module's resolvers never unlinked anything, but the address they
//! returned used to travel onward as a bare [`std::path::PathBuf`], and
//! [`crate::daemon_attach::ensure_daemon_running`]'s stale-inode recovery
//! `remove_file`s whatever address it is handed. A pre-#1121 daemon dying
//! between our probe and that function's own re-probe therefore got **our own**
//! dead legacy socket unlinked — and then a fresh daemon bound the *new*
//! address while the launcher polled the removed legacy one to its full
//! timeout. Resolution now carries [`crate::platform::paths::EndpointSource`]
//! alongside the address, and that function refuses anything that is not a
//! primary endpoint. A **foreign** entry was never at risk either way: the
//! unlink sits behind the same `lstat` the probe uses.
//!
//! The `$XDG_RUNTIME_DIR` and explicit-override spellings are untouched and
//! cost nothing here: the legacy consultation runs only on the arm that
//! resolved to the fallback endpoint. **That is an arm, not a path
//! comparison**, and the difference is the whole of round two's S7: an explicit
//! `DOT_AGENT_DECK_*` override that happens to spell the fallback address
//! satisfies `resolved == fallback` while being an override, and used to send
//! us off to consult the legacy path and hand back a daemon the operator never
//! named.
//!
//! # Why the daemon also binds the old spelling
//!
//! The compatibility read above covers one pairing: a newer client, an older
//! daemon. The other pairing is a release that already shipped and cannot be
//! changed — an older client looks **only** at the flat spelling. Against a
//! daemon that bound only the per-uid directory it found nothing, concluded no
//! deck was running, lazy-spawned a daemon of its own, and PRD #89's
//! auto-restore then rebuilt the saved session under it: a second orchestrator
//! and a second copy of every worker, in the same project, while the originals
//! kept running under the newer daemon. Silently — the build-version handshake
//! runs over a connection, and none was ever made. Measured by the `cargo xver`
//! harness and reported as issue #1211.
//!
//! The daemon is the only side still under our control, so it puts a socket
//! where that client looks: [`legacy_hook_alias`] and [`legacy_attach_alias`]
//! name the flat spelling **only on the fallback arm** (the one arm whose
//! spelling #1121 moved), and `daemon serve` binds each one beside its primary
//! endpoint. The older client then connects, the handshake runs, and the
//! existing mismatch prompt decides — exactly what happened before #1121.
//!
//! **This does not reintroduce the wedge #1121 removed, and the reason is what
//! the bind is allowed to do when it fails.** The wedge was that the deck's
//! *only* endpoint lived at a name a foreign uid could occupy, so an occupied
//! name meant a deck that could not start. The primary endpoint is still the
//! per-uid `0o700` directory and is still the only address this build's own
//! clients prefer. The alias is bound after it, and every way the alias can
//! fail — an entry the deck does not trust at the path, another daemon already
//! answering there, a bind or unlink error, a busy lock — is a warning and
//! nothing else: the daemon carries on serving the primary. So a squatter at
//! the flat spelling can deny discovery to *obsolete* clients, which they could
//! always do; they cannot make the deck unusable, which is the property #1121
//! bought. [`prepare_legacy_alias`] never unlinks an entry it did not verify as
//! this uid's own stale socket, so a planted entry is left exactly as found.
//!
//! **It is a deprecation shim with an end date.** It exists for clients built
//! before #1121, and issue #1213 removes it no earlier than two minor releases
//! after the release that first ships it — deleted then, not extended.
//!
//! # When the per-uid directory is taken
//!
//! Issue #1121 put the fallback endpoints inside `<temp dir>/dot-agent-deck-{uid}`
//! so no foreign uid could create an entry *beside* them — but that directory's
//! own name is as predictable as the sockets' were, in the same world-writable
//! parent. A local user who creates it first (as a directory, a file or a
//! symlink) owns it, [`crate::platform::fsperm::ensure_owner_only_dir`]
//! correctly refuses to put our sockets inside, and before this change the deck
//! simply did not start on its default endpoint.
//!
//! So when — and **only** when — the entry at that name belongs to another uid,
//! [`prepare_bind_endpoints`] binds instead inside a **relocated** directory:
//! `<temp dir>/dot-agent-deck-{uid}.<16 hex digits>`, the digits drawn from the
//! OS at creation time and the directory made by one non-recursive
//! `mkdir(2)` at `0o700`, which fails on any existing entry. Nobody can
//! pre-create a name they cannot predict, and once it exists it is ours and
//! the sticky parent stops anyone else renaming or removing it. A relocated
//! directory that already exists is reused rather than a new one minted per
//! start, under a lock in the daemon's lock root, so every concurrent starter
//! converges on the same one. And the choice is **sticky**: while the per-uid
//! name stays absent, a start keeps choosing the relocated directory even
//! after the squatter's entry is gone, so their removing it cannot make a
//! launcher and the daemon it spawns disagree, or seat a second daemon beside
//! a live one (see `needs_relocation`).
//!
//! Clients find it without any persisted state, which is the property the two
//! alternatives #1173 weighed lacked: a directory under `$HOME` cannot hold a
//! Unix socket on an NFS home, and a randomised name recorded in a state file
//! must first be *found*, from hook clients in a scrubbed environment, before
//! the endpoint can be resolved at all. Discovery here needs exactly the
//! inputs the fallback path already needs — the temp dir and the uid — and
//! works by listing the temp dir for `dot-agent-deck-{uid}.*` entries that
//! `lstat` as a real directory, **owned by this uid**, at an owner-only mode.
//! A foreign uid can create entries with that prefix but cannot create one
//! this uid owns, so every decoy is filtered out before anything connects;
//! the socket inside then gets the same
//! [`crate::platform::fsperm::verify_endpoint_trusted`] check as any other.
//!
//! **Nothing moves on a host where the name was never taken, and that is most
//! of the cross-version argument.** Relocation needs another uid's entry at
//! the per-uid name, or a relocated directory left by one, so a host where
//! nobody ever took the name binds exactly where it did before, and every
//! pairing of builds that interoperates there today still does. On a host
//! where the name *is* taken no build started
//! before this change, so there is no working pairing to break; what remains
//! is how an older client behaves against a daemon that relocated:
//!
//! - it finds it through the pre-#1121 alias above, which a relocated daemon
//!   still binds — the alias sits at a flat `/tmp` spelling, not inside the
//!   taken directory — so the build-version handshake runs and prompts;
//! - failing that, a client from a release that already uses the per-uid
//!   directory (v0.41.2 onwards) cannot start a second daemon **while the
//!   directory is still taken**: its lazy-spawn fails closed on the same
//!   `ensure_owner_only_dir` refusal, loudly, before any daemon exists;
//! - the one pairing that can still duplicate is narrow and stated rather
//!   than hidden: the squatter *removes* their entry while a relocated daemon
//!   runs, the alias is unavailable (squatted too, or gone once #1213 lands),
//!   and a client from before this change then starts. It finds nothing, the
//!   per-uid directory is free again, and it lazy-spawns a second daemon
//!   there — #1211's shape, reached only through a sequence the squatter
//!   drives. A client of this build in the same state still finds the
//!   relocated daemon, because an absent per-uid directory is searched.
//!
//! The listing runs only on the fallback arm, only when nothing answers at
//! the primary endpoint, and only when the per-uid directory is not already
//! ours — so a host with a working deck never pays for it.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use crate::platform::paths::EndpointSource;
use crate::platform::paths::ResolvedEndpoint;

/// How long the connect-side liveness probe waits.
///
/// Both candidates are Unix-domain sockets on this machine, where a connect
/// either completes in microseconds or fails outright; the bound exists for the
/// one case that neither does — `connect(2)` on `AF_UNIX` parks indefinitely
/// when the listener's accept queue is full (see
/// [`crate::platform::ipc::IpcClient::connect`]). A wedged daemon must not turn
/// endpoint resolution into a hang, and a quarter second is the same budget
/// [`crate::ui`]'s interactive daemon hint gives itself.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Make sure the directory `endpoint` lives in exists and is ours alone, when
/// that directory is [`crate::platform::paths::fallback_endpoint_dir`].
///
/// Call this immediately before a `bind(2)`, and **only** there. It is the
/// fallible half of the split DECISION 3 of issue #1121 draws: path resolution
/// stays pure and infallible because hook clients in a scrubbed environment,
/// `daemon status`, the desktop bridge and read-only diagnostics all call it,
/// while directory creation belongs to the one caller that is about to put a
/// socket inside.
///
/// A **no-op for every other endpoint**, which is what keeps it safe to call
/// unconditionally at a bind site: an explicit `DOT_AGENT_DECK_SOCKET` /
/// `DOT_AGENT_DECK_ATTACH_SOCKET` override, an `$XDG_RUNTIME_DIR` endpoint, a
/// test's `tempfile::tempdir()` socket and a Windows named pipe all resolve to
/// a parent this function does not own, and it creates nothing for any of them.
/// Deciding by comparing the parent — rather than by re-reading the
/// environment — is deliberate: there is then no second copy of the resolution
/// rules to drift out of step with [`crate::platform::paths::socket_path`].
///
/// Shaped after [`crate::remote_tunnel::tunnel_socket_dir_in`], which already
/// does this for ssh-forwarded sockets: resolve the directory, hand it to
/// [`crate::platform::fsperm::ensure_owner_only_dir`], and map the failure into
/// something the operator can act on.
pub fn ensure_endpoint_dir(endpoint: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        ensure_endpoint_dir_in(endpoint, &crate::platform::paths::fallback_endpoint_dir())
    }
    #[cfg(windows)]
    {
        // Windows endpoints are named pipes (`\\.\pipe\dot-agent-deck-{user}-…`)
        // with no filesystem presence and therefore no directory to own. There
        // is no `fallback_endpoint_dir` on this platform and nothing for this
        // function to create.
        let _ = endpoint;
        Ok(())
    }
}

/// [`ensure_endpoint_dir`] against an explicit fallback directory.
///
/// Split out so the test can drive both arms under a `tempfile::tempdir()`
/// instead of creating and chmodding the operator's **real**
/// `<temp dir>/dot-agent-deck-{uid}` — shared filesystem state with any live
/// fallback daemon on the host, which the first cut of this module's test did
/// (round two, S10). Same reason `fallback_endpoint_dir_in` exists one layer
/// down in [`crate::platform::paths`].
///
/// The decision stays a **parent comparison** rather than an
/// [`EndpointSource`] test, and that is deliberate rather than an oversight of
/// the provenance work elsewhere in this module. This runs at a `bind(2)` site,
/// and what a bind needs is for its parent directory to exist: an operator who
/// points `DOT_AGENT_DECK_SOCKET` deliberately *into* the fallback directory
/// still needs it created, and reading the source here would refuse to and turn
/// a working configuration into a bind failure. The directory it then creates
/// is the same one, at the same mode, that the same host would get with no
/// override at all — so there is no surprise to avoid, which is the only thing
/// provenance would buy here.
#[cfg(unix)]
fn ensure_endpoint_dir_in(endpoint: &Path, dir: &Path) -> std::io::Result<()> {
    if endpoint.parent() != Some(dir) {
        return Ok(());
    }
    ensure_fallback_dir(dir)
}

/// Create the fallback endpoint directory owner-only, with the refusal worded
/// for this directory — [`ensure_endpoint_dir_in`]'s body, shared with
/// [`choose_fallback_dir`].
#[cfg(unix)]
fn ensure_fallback_dir(dir: &Path) -> std::io::Result<()> {
    crate::platform::fsperm::ensure_owner_only_dir(dir).map_err(|source| {
        std::io::Error::new(
            source.kind(),
            format!(
                "{source}\nThis is the endpoint directory the deck falls back to when \
                 XDG_RUNTIME_DIR is unset. To use a different one, set \
                 DOT_AGENT_DECK_SOCKET and DOT_AGENT_DECK_ATTACH_SOCKET to socket paths \
                 under a directory only you can write."
            ),
        )
    })
}

/// The hook-ingestion endpoint a **client** in this process should connect to.
///
/// [`crate::platform::paths::socket_path`], except that when that resolved to
/// the fallback endpoint and nothing is answering there, a daemon from a
/// pre-#1121 build still listening at
/// [`crate::platform::paths::legacy_socket_path`] is used instead. See the
/// module docs for why, and for what "read-only" means here.
///
/// **Never use this to decide where to bind** — `daemon serve` resolves
/// [`crate::platform::paths::socket_path`] directly for its primary endpoint,
/// and reaches the old spelling only through [`legacy_hook_alias`], as a
/// best-effort alias beside that primary rather than instead of it.
pub fn client_socket_path() -> PathBuf {
    client_socket_endpoint().into_path()
}

/// [`client_socket_path`] with its [`EndpointSource`] — see
/// [`client_attach_endpoint`] for who needs the provenance and why.
pub fn client_socket_endpoint() -> ResolvedEndpoint {
    let resolved = crate::platform::paths::resolve_socket_path();
    #[cfg(unix)]
    let resolved = with_legacy_fallback(resolved, crate::platform::paths::legacy_socket_path());
    resolved
}

/// The streaming-attach endpoint a **client** in this process should connect
/// to. [`client_socket_path`]'s sibling, over
/// [`crate::platform::paths::attach_socket_path`] and
/// [`crate::platform::paths::legacy_attach_socket_path`].
pub fn client_attach_socket_path() -> PathBuf {
    client_attach_endpoint().into_path()
}

/// [`client_attach_socket_path`] with the [`EndpointSource`] that produced it.
///
/// The launcher needs the provenance and a bare path cannot carry it. A
/// [`EndpointSource::LegacyCompat`] address names a daemon from a pre-#1121
/// build: it is one this process may talk to and **not** one it may unlink,
/// lazy-spawn at, or poll for a daemon that will never bind it. See
/// [`ResolvedEndpoint::is_primary`], which is what
/// [`crate::daemon_attach::ensure_daemon_running`] refuses on.
pub fn client_attach_endpoint() -> ResolvedEndpoint {
    let resolved = crate::platform::paths::resolve_attach_socket_path();
    #[cfg(unix)]
    let resolved = with_legacy_fallback(
        resolved,
        crate::platform::paths::legacy_attach_socket_path(),
    );
    resolved
}

/// The attach endpoint this build **binds** as its primary, with no
/// compatibility read at all.
///
/// What the launcher falls back to when a legacy daemon it selected has gone
/// between resolution and use, and what it re-resolves to after a
/// version-mismatch recovery: in both cases the daemon that is about to exist
/// binds this address and no other.
pub fn primary_attach_endpoint() -> ResolvedEndpoint {
    crate::platform::paths::resolve_attach_socket_path()
}

/// Is a daemon we are willing to talk to listening at `endpoint` right now?
/// The public spelling of the probe [`with_legacy_fallback`] resolves with.
pub fn endpoint_is_answering(endpoint: &Path) -> bool {
    #[cfg(unix)]
    {
        endpoint_answers(endpoint)
    }
    #[cfg(windows)]
    {
        // A named pipe has no inode to `lstat`; `IpcClient::connect` verifies
        // the server's owner SID itself, so the connect *is* the trust check.
        crate::platform::ipc::IpcClient::connect_timeout(endpoint, PROBE_TIMEOUT).is_ok()
    }
}

/// The resolution order both client accessors share.
///
/// Only [`EndpointSource::Fallback`] has a legacy spelling to consult; every
/// other arm returns untouched, without a probe. Then: the new address if a
/// daemon answers there, otherwise the legacy address if one answers there,
/// otherwise the new address — so a host with no daemon at all lazy-spawns at
/// the new spelling, which is what makes this a transition rather than a
/// second home.
///
/// **The arm is read from [`ResolvedEndpoint::source`], not inferred by
/// comparing the address against the fallback spelling.** That comparison was
/// round one's shape and it answered `true` for an explicit
/// `DOT_AGENT_DECK_ATTACH_SOCKET` — or an `$XDG_RUNTIME_DIR` — that happened to
/// name the same address, which is how an operator who named one daemon could
/// be handed another.
///
/// A relocated endpoint (issue #1173) is tried **before** the legacy spelling.
/// A daemon that relocated is this build's, and it also holds the legacy
/// spelling as an alias, so both would reach it; preferring the relocated
/// address keeps this build's clients off a shim that has an end date (#1213),
/// and when both answer with *different* daemons it picks the one of ours.
#[cfg(unix)]
fn with_legacy_fallback(resolved: ResolvedEndpoint, legacy: PathBuf) -> ResolvedEndpoint {
    if resolved.source() != EndpointSource::Fallback || endpoint_answers(resolved.path()) {
        return resolved;
    }
    if let Some(relocated) = answering_relocated_endpoint(resolved.path()) {
        return ResolvedEndpoint::new(relocated, EndpointSource::Relocated);
    }
    if endpoint_answers(&legacy) {
        return ResolvedEndpoint::new(legacy, EndpointSource::LegacyCompat);
    }
    resolved
}

/// Is a daemon we are willing to talk to listening at `endpoint` right now?
///
/// Trust first, then liveness. The trust check is
/// [`crate::platform::fsperm::verify_endpoint_trusted`] — the same `lstat`
/// issue #1020 put in front of every other endpoint connect — so a symlink, a
/// regular file, a foreign-owned socket or a loose mode all answer `false`
/// here without a `connect(2)` being attempted and without the entry being
/// touched. That is the whole of what makes a squatted legacy path harmless:
/// it costs one refused probe and we carry on at the new path.
///
/// **A trusted endpoint that times out answers `true`** (round two, S3), and
/// that is not leniency — it is what the errno means here. Per
/// [`crate::platform::ipc::IpcClient::connect_timeout`]'s own documentation an
/// `AF_UNIX` connect blocks for exactly one reason: the listener's accept
/// queue is full. Blowing the budget is therefore positive proof of a **live**
/// listener, and one we have already trust-checked. Reporting it as absent had
/// a single, destructive consequence — an older daemon alive at the legacy
/// address but saturated for longer than [`PROBE_TIMEOUT`], with nothing at the
/// new one, made both probes answer `false`, so the launcher lazy-spawned a
/// *second* daemon beside a healthy first and the build-version handshake never
/// fired, because it runs over a connection that was never made. That is the
/// silent double-spawn this whole module exists to prevent.
///
/// `NotFound`, `ConnectionRefused` and `PermissionDenied` stay `false`: those
/// are an absent entry, a dead one, and one we may not talk to. The bound is
/// still worth having — a wedged daemon must not turn resolution into a hang —
/// and it is not paid in the ordinary cases: an absent endpoint fails the trust
/// check on `NotFound` before any connect, and a stale inode answers
/// `ECONNREFUSED` at once.
#[cfg(unix)]
fn endpoint_answers(endpoint: &Path) -> bool {
    if crate::platform::fsperm::verify_endpoint_trusted(endpoint).is_err() {
        return false;
    }
    match crate::platform::ipc::IpcClient::connect_timeout(endpoint, PROBE_TIMEOUT) {
        Ok(_) => true,
        Err(source) => connect_failure_still_answers(source.kind()),
    }
}

/// Does a failed probe connect still prove a live listener?
///
/// The pure half of [`endpoint_answers`]'s decision, split out for the reason
/// [`crate::platform::fsperm`]'s `endpoint_uid_is_trusted` is: the input that
/// matters — a listener whose accept queue is full for longer than
/// [`PROBE_TIMEOUT`] — is not something a test process can reliably produce,
/// while the rule over the error kind is exhaustively testable anywhere.
#[cfg(unix)]
fn connect_failure_still_answers(kind: std::io::ErrorKind) -> bool {
    kind == std::io::ErrorKind::TimedOut
}

// ---------------------------------------------------------------------------
// Relocation when the per-uid directory is taken (issue #1173)
// ---------------------------------------------------------------------------

/// Where a daemon about to start should bind each of `resolved`, with the
/// directories they need created — the bind side of
/// [When the per-uid directory is taken](#when-the-per-uid-directory-is-taken).
///
/// Everything but the fallback arm is [`ensure_endpoint_dir`] and the address
/// unchanged: an explicit override names the address the operator chose, and
/// relocating it would hand them a daemon somewhere they did not say — even an
/// override that points *into* a taken per-uid directory, which keeps failing
/// closed with the message naming both uids. Fallback-arm addresses are
/// unchanged too **unless** [`needs_relocation`] says otherwise, in which case
/// each is the same file name inside one relocated directory.
///
/// **One decision for every address passed, which is why this takes a
/// slice.** `daemon serve` passes its hook and attach endpoints together: two
/// separate calls could straddle a squatter removing their entry, putting the
/// hook socket in the relocated directory and the attach socket in the usual
/// one — after which the usual directory is ours, clients stop searching, and
/// hooks go nowhere.
///
/// Called by `daemon serve` before it binds, and by the launcher before it
/// lazy-spawns, so the launcher polls the address the daemon it starts will
/// actually bind. They agree because the decision depends only on filesystem
/// state that the launcher's own call pins: when it relocates it creates the
/// relocated directory, which [`needs_relocation`] then keeps choosing even if
/// the squatter's entry is gone by the time the daemon asks; when it does not,
/// it creates the per-uid directory itself, which no one else can then take.
///
/// The decision and the directory it chooses are made as one operation on the
/// blocking pool, under a lock both callers take ([`choose_fallback_dir`]).
pub async fn prepare_bind_endpoints(
    resolved: &[ResolvedEndpoint],
) -> std::io::Result<Vec<PathBuf>> {
    #[cfg(unix)]
    {
        let primary_dir = crate::platform::paths::fallback_endpoint_dir();
        let uid = crate::platform::paths::current_uid();
        let on_fallback = |endpoint: &ResolvedEndpoint| {
            endpoint.source() == EndpointSource::Fallback
                && endpoint.path().parent() == Some(primary_dir.as_path())
        };
        let relocated = if resolved.iter().any(on_fallback) {
            let dir = primary_dir.clone();
            let lock_root = crate::daemon::lock_root(None);
            on_blocking_pool(move || choose_fallback_dir(&dir, uid, &lock_root)).await?
        } else {
            None
        };
        if let Some((dir, reason)) = &relocated {
            tracing::warn!(
                "this deck's endpoints are in {} instead of {}: {} (issue #1173)",
                dir.display(),
                primary_dir.display(),
                reason.describe()
            );
        }
        let relocated = relocated.map(|(dir, _)| dir);
        let mut paths = Vec::with_capacity(resolved.len());
        for endpoint in resolved {
            match &relocated {
                Some(dir) if on_fallback(endpoint) => {
                    let file_name = endpoint.path().file_name().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("{} has no file name to relocate", endpoint.path().display()),
                        )
                    })?;
                    paths.push(dir.join(file_name));
                }
                _ if on_fallback(endpoint) => {
                    // `choose_fallback_dir` already made the per-uid
                    // directory, under the lock.
                    paths.push(endpoint.path().to_path_buf());
                }
                _ => {
                    let path = endpoint.path().to_path_buf();
                    let for_dir = path.clone();
                    on_blocking_pool(move || ensure_endpoint_dir(&for_dir)).await?;
                    paths.push(path);
                }
            }
        }
        Ok(paths)
    }
    #[cfg(windows)]
    {
        // Named pipes: no directory, so nothing to take and nothing to relocate.
        Ok(resolved
            .iter()
            .map(|endpoint| endpoint.path().to_path_buf())
            .collect())
    }
}

/// Decide where the fallback arm binds and make that directory, as **one**
/// operation under the relocation lock: `Some((relocated, why))`, or `None`
/// once the per-uid directory itself is ready.
///
/// One blocking operation rather than a decision followed by a creation, for
/// two reasons a review of #1349 found. A decision made first can be
/// overtaken: another uid can take the per-uid name between "no need to
/// relocate" and the `mkdir`, and the start would then fail closed instead of
/// relocating — so a failed preparation that finds the entry has become
/// foreign relocates after all. And the lock is a synchronous `flock` taken
/// and released on the same blocking thread, rather than an async guard held
/// across an `.await` on blocking work, which a cancelled caller would drop
/// while the find-or-create was still running.
#[cfg(unix)]
fn choose_fallback_dir(
    primary_dir: &Path,
    uid: u32,
    lock_root: &Path,
) -> std::io::Result<Option<(PathBuf, RelocationReason)>> {
    crate::platform::fsperm::ensure_owner_only_dir(lock_root)?;
    let _lock = lock_exclusive(&lock_root.join(RELOCATION_LOCK_FILE))?;
    if let Some(reason) = needs_relocation(primary_dir, uid) {
        return Ok(Some((relocated_bind_dir(primary_dir, uid)?, reason)));
    }
    match ensure_fallback_dir(primary_dir) {
        Ok(()) => Ok(None),
        Err(_) if entry_is_foreign(primary_dir, uid) => Ok(Some((
            relocated_bind_dir(primary_dir, uid)?,
            RelocationReason::Taken,
        ))),
        Err(source) => Err(source),
    }
}

/// An exclusive `flock(2)` on `path`, released when the returned file is
/// dropped. The synchronous sibling of
/// [`crate::platform::lock::acquire_spawn_lock`], for a caller that is already
/// on a blocking thread and must hold the lock for exactly as long as that
/// thread's work runs.
#[cfg(unix)]
fn lock_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // SAFETY: a valid descriptor and a valid operation; `flock(2)` retains no
    // reference to our memory.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// Run blocking filesystem work off the async worker threads.
#[cfg(unix)]
async fn on_blocking_pool<T: Send + 'static>(
    work: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(std::io::Error::other)?
}

/// Why [`prepare_bind_endpoints`] relocated, for the warning it logs.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelocationReason {
    /// Another uid owns the entry at the per-uid name.
    Taken,
    /// Nothing is at the per-uid name, but an earlier start relocated and its
    /// directory is still here.
    AlreadyRelocated,
}

#[cfg(unix)]
impl RelocationReason {
    fn describe(self) -> &'static str {
        match self {
            Self::Taken => {
                "another user owns the usual directory, and the deck never puts its sockets \
                 inside someone else's directory"
            }
            Self::AlreadyRelocated => {
                "an earlier start relocated them there and the usual directory does not exist, \
                 so they stay put; remove the relocated directory while no deck is running to \
                 return to the usual location"
            }
        }
    }
}

/// Should a daemon starting now bind in a relocated directory rather than in
/// `primary_dir`?
///
/// - **Another uid owns the entry at `primary_dir`** ([`entry_is_foreign`]):
///   yes — the case #1173 is about.
/// - **Nothing is at `primary_dir`, and a relocated directory of ours already
///   exists**: yes, **sticky**. This is what makes the choice stable against
///   the squatter *removing* their entry. Without it the launcher could pick
///   the relocated directory and the daemon it spawns a moment later the
///   usual one, and poll the wrong address to its timeout; and a daemon
///   started while a relocated one is still running would bind the usual
///   directory beside it, which clients then prefer, hiding the first daemon
///   and its agents. With it, the second daemon targets the same relocated
///   socket and meets the start path's refusal to clobber a live one.
/// - Otherwise — the per-uid directory is ours, or absent with nothing
///   relocated — no.
#[cfg(unix)]
fn needs_relocation(primary_dir: &Path, uid: u32) -> Option<RelocationReason> {
    if entry_is_foreign(primary_dir, uid) {
        return Some(RelocationReason::Taken);
    }
    let absent = matches!(
        std::fs::symlink_metadata(primary_dir),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound
    );
    (absent && !relocated_endpoint_dirs(primary_dir, uid).is_empty())
        .then_some(RelocationReason::AlreadyRelocated)
}

/// The lock [`prepare_bind_endpoints`] holds across its find-or-create, in the
/// daemon's lock root — which is never the temp dir, for the reason the
/// daemon's own start lock is not (PRD #93 round four).
#[cfg(unix)]
const RELOCATION_LOCK_FILE: &str = "endpoint-relocation.lock";

/// The attach endpoint `daemon endpoint` should report: the pure resolution,
/// or — on the fallback arm, when nothing answers there — a relocated one that
/// does. No legacy read: the remote probe has its own rung for the pre-#1121
/// spelling, and this answers for this build's daemon only.
pub fn served_attach_endpoint() -> ResolvedEndpoint {
    let resolved = crate::platform::paths::resolve_attach_socket_path();
    #[cfg(unix)]
    if resolved.source() == EndpointSource::Fallback
        && !endpoint_answers(resolved.path())
        && let Some(relocated) = answering_relocated_endpoint(resolved.path())
    {
        return ResolvedEndpoint::new(relocated, EndpointSource::Relocated);
    }
    resolved
}

/// Does an entry exist at `path` whose owner is not `uid`?
///
/// `lstat`, so a symlink is judged by who made the link and not by what it
/// points at. Absent, or unreadable, answers `false`: relocation is for a name
/// someone else **holds**, and every other failure is left to
/// [`crate::platform::fsperm::ensure_owner_only_dir`] to report as it always has.
/// A same-uid entry that is not a usable directory is deliberately not
/// relocated around either — it is the operator's to fix, and the existing
/// refusal tells them how.
#[cfg(unix)]
fn entry_is_foreign(path: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!(std::fs::symlink_metadata(path), Ok(metadata) if metadata.uid() != uid)
}

/// Is `path` a directory — not a symlink to one — owned by `uid` with no group
/// or other bits set? The test every relocated-directory candidate must pass
/// before anything is bound in it or connected to inside it.
#[cfg(unix)]
fn is_owner_only_dir_of(path: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
    })
}

/// The name prefix of `primary_dir`'s relocated siblings: its own file name
/// and a dot, so `dot-agent-deck-1.` never matches uid 10's `dot-agent-deck-10`.
#[cfg(unix)]
fn relocated_dir_prefix(primary_dir: &Path) -> Option<String> {
    Some(format!("{}.", primary_dir.file_name()?.to_str()?))
}

/// The relocated directories of `primary_dir` that already exist and pass
/// [`is_owner_only_dir_of`], sorted by name so every caller sees the same
/// order. Entries another uid made with the same prefix — the only kind a
/// squatter *can* make — are dropped here, before anything connects.
#[cfg(unix)]
fn relocated_endpoint_dirs(primary_dir: &Path, uid: u32) -> Vec<PathBuf> {
    let (Some(parent), Some(prefix)) = (primary_dir.parent(), relocated_dir_prefix(primary_dir))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.len() > prefix.len() && name.starts_with(&prefix))
        })
        .map(|entry| entry.path())
        .filter(|path| is_owner_only_dir_of(path, uid))
        .collect();
    dirs.sort();
    dirs
}

/// The directory to bind into once the per-uid one is taken: the first
/// existing relocated directory, or a freshly created one.
///
/// Creation is one non-recursive `mkdir(2)` at `0o700` on a name with 64 bits
/// from the OS in it, which fails `EEXIST` on anything already there — so it
/// cannot be steered into a directory someone else prepared — followed by
/// [`crate::platform::fsperm::ensure_owner_only_dir`], which re-verifies it
/// through a descriptor. A collision is retried with fresh digits.
#[cfg(unix)]
fn relocated_bind_dir(primary_dir: &Path, uid: u32) -> std::io::Result<PathBuf> {
    if let Some(existing) = relocated_endpoint_dirs(primary_dir, uid).into_iter().next() {
        return Ok(existing);
    }
    use std::os::unix::fs::DirBuilderExt;
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} cannot have a relocated sibling", primary_dir.display()),
        )
    };
    let parent = primary_dir.parent().ok_or_else(invalid)?;
    let prefix = relocated_dir_prefix(primary_dir).ok_or_else(invalid)?;
    for _ in 0..RELOCATION_ATTEMPTS {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|source| {
            std::io::Error::other(format!("OS randomness is unavailable: {source}"))
        })?;
        let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let dir = parent.join(format!("{prefix}{suffix}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {
                crate::platform::fsperm::ensure_owner_only_dir(&dir)?;
                return Ok(dir);
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(source),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not create a relocated endpoint directory beside {} after {RELOCATION_ATTEMPTS} attempts",
            primary_dir.display()
        ),
    ))
}

/// How many fresh names [`relocated_bind_dir`] tries. A collision on 64 random
/// bits is not a thing that happens by chance; the bound exists so a broken
/// randomness source cannot turn into a loop.
#[cfg(unix)]
const RELOCATION_ATTEMPTS: usize = 8;

/// The connect side of relocation: the first relocated sibling of `primary`'s
/// directory in which a daemon answers at `primary`'s file name, or `None`.
///
/// Skipped outright when the per-uid directory is ours — a daemon of this
/// build would have bound there, so there is nothing to find and the listing
/// is not paid. An **absent** per-uid directory is still searched: a daemon
/// that relocated while the name was taken keeps running after the squatter
/// removes their entry, and a client that stopped looking then would
/// lazy-spawn a second daemon beside it.
#[cfg(unix)]
fn answering_relocated_endpoint(primary: &Path) -> Option<PathBuf> {
    relocated_candidates(primary, crate::platform::paths::current_uid())
        .into_iter()
        .find(|candidate| endpoint_answers(candidate))
}

/// [`answering_relocated_endpoint`] without the liveness probe — the pure-ish
/// half, over an explicit uid so a test can drive it in a temp dir.
#[cfg(unix)]
fn relocated_candidates(primary: &Path, uid: u32) -> Vec<PathBuf> {
    let (Some(primary_dir), Some(file_name)) = (primary.parent(), primary.file_name()) else {
        return Vec::new();
    };
    if is_owner_only_dir_of(primary_dir, uid) {
        return Vec::new();
    }
    relocated_endpoint_dirs(primary_dir, uid)
        .into_iter()
        .map(|dir| dir.join(file_name))
        .collect()
}

// ---------------------------------------------------------------------------
// The daemon's legacy aliases (issue #1211)
// ---------------------------------------------------------------------------

/// The pre-#1121 hook spelling `daemon serve` should **also** bind, as a
/// best-effort alias for clients too old to know the new one — or `None` when
/// there is none to bind.
///
/// Only the fallback arm has one. An explicit `DOT_AGENT_DECK_SOCKET` and an
/// `$XDG_RUNTIME_DIR` endpoint resolve byte-identically in every build, so an
/// older client already finds a newer daemon there; the alias exists for the
/// one arm issue #1121 moved. Decided from [`ResolvedEndpoint::source`] for the
/// reason [`with_legacy_fallback`] is: an override that happens to spell the
/// fallback address is still an override.
///
/// See the module docs for why binding it does not reopen #1121's wedge.
pub fn legacy_hook_alias() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        legacy_alias_for(
            &crate::platform::paths::resolve_socket_path(),
            crate::platform::paths::legacy_socket_path(),
        )
    }
    #[cfg(windows)]
    {
        // Named pipes; no build ever spelled a Windows endpoint any other way.
        None
    }
}

/// [`legacy_hook_alias`]'s sibling for the streaming-attach endpoint — the one
/// an older TUI actually probes before it decides to lazy-spawn, and so the one
/// whose absence produced issue #1211's duplicated orchestration.
pub fn legacy_attach_alias() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        legacy_alias_for(
            &crate::platform::paths::resolve_attach_socket_path(),
            crate::platform::paths::legacy_attach_socket_path(),
        )
    }
    #[cfg(windows)]
    {
        None
    }
}

/// The pure rule behind [`legacy_hook_alias`] / [`legacy_attach_alias`].
#[cfg(unix)]
fn legacy_alias_for(primary: &ResolvedEndpoint, legacy: PathBuf) -> Option<PathBuf> {
    (primary.source() == EndpointSource::Fallback && primary.path() != legacy).then_some(legacy)
}

/// Why a legacy alias was not bound.
///
/// Every variant is a reason to **log and carry on**, never a reason for the
/// daemon to fail: the alias is a courtesy to obsolete clients, and the primary
/// endpoint is already bound by the time it is attempted.
#[derive(Debug)]
pub enum LegacyAliasSkip {
    /// An entry the deck does not trust occupies the path — a symlink, a
    /// regular file, a directory, a socket another uid owns or one at a loose
    /// mode. This is #1121's squatter, and the entry is left exactly as found.
    Untrusted(String),
    /// A daemon already answers at the path — in practice an older build's,
    /// still running. It keeps the address; this daemon does not compete.
    Occupied,
    /// Probing, clearing or binding the path failed — a read-only or missing
    /// `/tmp`, a lost race for the name, and the like.
    Io(std::io::Error),
}

impl std::fmt::Display for LegacyAliasSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Untrusted(reason) => write!(
                f,
                "the entry already there is not one the deck trusts ({reason}); it is left as found"
            ),
            Self::Occupied => write!(
                f,
                "another daemon is already listening there, and it keeps the address"
            ),
            Self::Io(source) => write!(f, "{source}"),
        }
    }
}

/// Get `path` ready for a legacy-alias bind: `Ok` when nothing is there, or
/// when what is there is this uid's own **stale** socket, which this unlinks.
///
/// The order is the point. Trust first —
/// [`crate::platform::fsperm::verify_endpoint_trusted`]'s `lstat`, so a planted
/// symlink, file, directory or foreign-owned socket is refused before anything
/// connects to it and is never touched. Then liveness, with the bounded probe
/// [`endpoint_answers`] uses, and with its reading of a timeout as a live
/// listener. Only an entry that is trusted **and** refuses the connection is
/// unlinked, which is the stale-inode recovery every daemon start already does
/// at its primary address.
///
/// In a sticky `/tmp` that unlink cannot reach a stranger's entry even by a
/// race: the entry verified here is ours, a foreign uid cannot remove it, so
/// it is still ours when `remove_file` runs.
///
/// Blocking (the probe is a bounded synchronous connect), so the daemon runs it
/// on the blocking pool.
#[cfg(unix)]
pub fn prepare_legacy_alias(path: &Path) -> Result<(), LegacyAliasSkip> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(LegacyAliasSkip::Io(source)),
    }
    crate::platform::fsperm::verify_endpoint_trusted(path).map_err(LegacyAliasSkip::Untrusted)?;
    match crate::platform::ipc::IpcClient::connect_timeout(path, PROBE_TIMEOUT) {
        Ok(_) => Err(LegacyAliasSkip::Occupied),
        Err(source) if connect_failure_still_answers(source.kind()) => {
            Err(LegacyAliasSkip::Occupied)
        }
        Err(source) if source.kind() == std::io::ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path).map_err(LegacyAliasSkip::Io)
        }
        Err(source) => Err(LegacyAliasSkip::Io(source)),
    }
}

/// Windows has no legacy spelling ([`legacy_hook_alias`] is always `None`
/// there), so there is nothing to prepare.
#[cfg(windows)]
pub fn prepare_legacy_alias(_path: &Path) -> Result<(), LegacyAliasSkip> {
    Err(LegacyAliasSkip::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Windows endpoints are named pipes and have no pre-#1121 spelling",
    )))
}

/// A socket the daemon bound at a pre-#1121 spelling, **unlinked again when
/// this is dropped** — provided the inode at the path is still the one bound.
///
/// The daemon leaves its *primary* sockets on disk at exit, for the next
/// start's stale-inode recovery to clear. The alias is removed instead, because
/// nothing guarantees a next start will bind it: a later build without this
/// shim, or a start whose alias bind is skipped, would leave an older client
/// probing a dead inode.
///
/// The identity check is what keeps the removal from reaching someone else's
/// socket. An older daemon's startup unlinks whatever sits at its attach path
/// before binding (v0.41.0's `bind_attach_listener`), so the name can change
/// hands while this daemon still holds the listener; removing it by name alone
/// would then delete the older daemon's live endpoint.
pub struct LegacyAlias {
    path: PathBuf,
    /// `(st_dev, st_ino)` of the socket as bound; `None` when it could not be
    /// read, in which case nothing is removed.
    #[cfg(unix)]
    identity: Option<(u64, u64)>,
}

impl LegacyAlias {
    /// Record the inode now at `path`, which the caller has **just** bound.
    pub fn adopt(path: PathBuf) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let identity = match std::fs::symlink_metadata(&path) {
                Ok(md) => Some((md.dev(), md.ino())),
                Err(source) => {
                    tracing::warn!(
                        "legacy endpoint alias {} was bound but could not be read back ({source}); \
                         it will be left on disk at exit",
                        path.display()
                    );
                    None
                }
            };
            Self { path, identity }
        }
        #[cfg(windows)]
        {
            Self { path }
        }
    }

    /// The address this alias occupies.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LegacyAlias {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Some(identity) = self.identity else {
                return;
            };
            match std::fs::symlink_metadata(&self.path) {
                Ok(md) if (md.dev(), md.ino()) == identity => {
                    if let Err(source) = std::fs::remove_file(&self.path) {
                        tracing::warn!(
                            "could not remove legacy endpoint alias {}: {source}",
                            self.path.display()
                        );
                    }
                }
                // Gone already, or now another inode — not ours to remove.
                _ => {}
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fallback(path: &Path) -> ResolvedEndpoint {
        ResolvedEndpoint::new(path.to_path_buf(), EndpointSource::Fallback)
    }

    /// A live, trusted socket at the primary path is used and the legacy path
    /// is not consulted — the ordinary case on a host that has already been
    /// through one launch on the new build.
    #[test]
    fn a_live_primary_wins_over_a_live_legacy() {
        let dir = sandbox();
        let primary = dir.path().join("primary.sock");
        let legacy = dir.path().join("legacy.sock");
        let _p = bind_trusted(&primary);
        let _l = bind_trusted(&legacy);

        let resolved = with_legacy_fallback(fallback(&primary), legacy);
        assert_eq!(resolved.path(), primary);
        assert!(resolved.is_primary());
    }

    /// Nothing at the primary and a live daemon at the legacy path: the older
    /// build's daemon is found, which is the entire point of the compatibility
    /// read — and the result is marked [`EndpointSource::LegacyCompat`], which
    /// is what keeps it away from the stale-inode reaper and the lazy spawn.
    #[test]
    fn an_absent_primary_falls_through_to_a_live_legacy() {
        let dir = sandbox();
        let primary = dir.path().join("primary.sock");
        let legacy = dir.path().join("legacy.sock");
        let _l = bind_trusted(&legacy);

        let resolved = with_legacy_fallback(fallback(&primary), legacy.clone());
        assert_eq!(resolved.path(), legacy);
        assert_eq!(resolved.source(), EndpointSource::LegacyCompat);
        assert!(
            !resolved.is_primary(),
            "a legacy address must never be handed to a destructive or lazy-spawn path"
        );
    }

    /// An entry at the legacy path that is not a trusted socket of ours — the
    /// squatter case — is refused by the probe and resolution continues at the
    /// new path. The planted entry is left exactly as it was found.
    #[test]
    fn an_untrusted_legacy_entry_is_refused_and_left_alone() {
        let dir = sandbox();
        let primary = dir.path().join("primary.sock");
        let legacy = dir.path().join("legacy.sock");
        std::fs::write(&legacy, b"squatter").expect("plant a regular file");

        let resolved = with_legacy_fallback(fallback(&primary), legacy.clone());
        assert_eq!(resolved.path(), primary);
        assert_eq!(
            std::fs::read(&legacy).expect("the planted entry survives"),
            b"squatter"
        );
    }

    /// An override or an `$XDG_RUNTIME_DIR` endpoint never consults the legacy
    /// path, however live a daemon there is — the arm short-circuits before any
    /// probe.
    #[test]
    fn a_non_fallback_primary_never_consults_the_legacy_path() {
        let dir = sandbox();
        let overridden = dir.path().join("overridden.sock");
        let legacy = dir.path().join("legacy.sock");
        let _l = bind_trusted(&legacy);

        for source in [EndpointSource::Override, EndpointSource::PlatformDefault] {
            let resolved = with_legacy_fallback(
                ResolvedEndpoint::new(overridden.clone(), source),
                legacy.clone(),
            );
            assert_eq!(
                resolved.path(),
                overridden,
                "{source:?} has no older spelling to look for"
            );
            assert_eq!(resolved.source(), source);
        }
    }

    /// Round two, S7: the arm is read from the provenance, not inferred by
    /// comparing addresses. An explicit override that happens to spell the very
    /// same address the fallback would have produced must still be treated as
    /// an override — under the old equality test this was the one input that
    /// sent us to a daemon the operator never named.
    #[test]
    fn an_override_that_equals_the_fallback_address_is_still_an_override() {
        let dir = sandbox();
        let shared = dir.path().join("shared.sock");
        let legacy = dir.path().join("legacy.sock");
        let _l = bind_trusted(&legacy);

        let resolved = with_legacy_fallback(
            ResolvedEndpoint::new(shared.clone(), EndpointSource::Override),
            legacy,
        );
        assert_eq!(
            resolved.path(),
            shared,
            "an explicit override must win even when it names the fallback address"
        );
    }

    /// Round two, S3: every `connect(2)` failure kind the probe can see, and
    /// whether it still proves a live listener.
    ///
    /// Driven through the pure classifier rather than against a real saturated
    /// listener, which is this repo's own idiom for a rule whose reachable
    /// input needs a condition a test process cannot reliably create (compare
    /// `fsperm::endpoint_uid_is_trusted`). Saturating a Unix accept queue is
    /// exactly such a condition — measured on this box, a `UnixListener` took
    /// 1024 pending connections without parking once — and
    /// `platform::ipc`'s `connect_against_a_saturated_listener_returns_within_the_deadline`
    /// already pins the other half: that a full queue is what produces
    /// `TimedOut` here.
    #[test]
    fn only_a_timeout_proves_a_live_listener_among_the_connect_failures() {
        use std::io::ErrorKind;

        assert!(
            connect_failure_still_answers(ErrorKind::TimedOut),
            "a full accept queue is proof of a live listener, and reporting it \
             absent is what made the launcher spawn a second daemon"
        );
        for absent in [
            ErrorKind::NotFound,
            ErrorKind::ConnectionRefused,
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
        ] {
            assert!(
                !connect_failure_still_answers(absent),
                "{absent:?} is an absent, dead or forbidden endpoint"
            );
        }
    }

    /// [`ensure_endpoint_dir`] creates nothing for an endpoint outside the
    /// fallback directory, which is what makes it safe at a bind site that may
    /// have been handed an override or a test path.
    #[test]
    fn ensure_endpoint_dir_is_inert_outside_the_fallback_directory() {
        let dir = sandbox();
        let elsewhere = dir.path().join("nested").join("attach.sock");
        ensure_endpoint_dir_in(&elsewhere, &dir.path().join("fallback"))
            .expect("a foreign endpoint is a no-op, not an error");
        assert!(
            !dir.path().join("nested").exists(),
            "ensure_endpoint_dir must not create a directory it does not own"
        );
        assert!(
            !dir.path().join("fallback").exists(),
            "and it must not create the fallback directory either"
        );
    }

    /// The arm that does act: an endpoint inside the fallback directory gets
    /// that directory created at mode 0o700.
    ///
    /// Driven through [`ensure_endpoint_dir_in`] under a `tempfile` root. The
    /// first cut of this test called the production `fallback_endpoint_dir()`
    /// and created and chmodded the operator's real
    /// `<temp dir>/dot-agent-deck-{uid}` — shared state with any live fallback
    /// daemon on the host (round two, S10).
    #[test]
    fn ensure_endpoint_dir_creates_the_fallback_directory_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = sandbox();
        let dir = root.path().join("dot-agent-deck-4242");
        ensure_endpoint_dir_in(&dir.join("attach.sock"), &dir)
            .expect("the fallback endpoint directory is ours to create");
        let mode = std::fs::metadata(&dir)
            .expect("the fallback endpoint directory now exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{} must be owner-only", dir.display());
    }

    /// Issue #1211: the daemon aliases the old spelling on the fallback arm and
    /// on no other. An override or an `$XDG_RUNTIME_DIR` endpoint resolves
    /// byte-identically in every build, so there is no older client to serve —
    /// and an override that happens to spell the fallback address is still an
    /// override, for the reason S7 gives above.
    #[test]
    fn only_the_fallback_arm_has_a_legacy_alias() {
        let primary = PathBuf::from("/scratch/dot-agent-deck-4242/attach.sock");
        let legacy = PathBuf::from("/scratch/dot-agent-deck-attach-4242.sock");
        assert_eq!(
            legacy_alias_for(&fallback(&primary), legacy.clone()),
            Some(legacy.clone())
        );
        for source in [EndpointSource::Override, EndpointSource::PlatformDefault] {
            assert_eq!(
                legacy_alias_for(
                    &ResolvedEndpoint::new(primary.clone(), source),
                    legacy.clone()
                ),
                None,
                "{source:?} resolves identically in older builds and needs no alias"
            );
        }
        assert_eq!(
            legacy_alias_for(&fallback(&legacy), legacy.clone()),
            None,
            "an alias that IS the primary would bind one address twice"
        );
    }

    /// Nothing at the path: ready to bind, and nothing is created by the
    /// preparation itself.
    #[test]
    fn an_absent_legacy_path_is_ready_and_left_absent() {
        let dir = sandbox();
        let legacy = dir.path().join("legacy.sock");
        prepare_legacy_alias(&legacy).expect("an absent path is ready");
        assert!(std::fs::symlink_metadata(&legacy).is_err());
    }

    /// Our own stale socket — a daemon that died without unlinking, which is
    /// what every daemon exit leaves at its primary — is cleared, exactly the
    /// recovery a daemon start does at its primary address.
    #[test]
    fn our_own_stale_socket_at_the_legacy_path_is_cleared() {
        let dir = sandbox();
        let legacy = dir.path().join("legacy.sock");
        drop(bind_trusted(&legacy));
        assert!(
            std::fs::symlink_metadata(&legacy).is_ok(),
            "precondition: dropping a std listener leaves its inode"
        );
        prepare_legacy_alias(&legacy).expect("a stale socket of ours is ready");
        assert!(
            std::fs::symlink_metadata(&legacy).is_err(),
            "the stale inode is unlinked so the alias can bind"
        );
    }

    /// A daemon already answering at the old spelling — in practice an older
    /// build's — keeps it. Nothing is unlinked.
    #[test]
    fn an_answering_daemon_at_the_legacy_path_keeps_it() {
        let dir = sandbox();
        let legacy = dir.path().join("legacy.sock");
        let _live = bind_trusted(&legacy);
        assert!(matches!(
            prepare_legacy_alias(&legacy),
            Err(LegacyAliasSkip::Occupied)
        ));
        assert!(
            endpoint_answers(&legacy),
            "the live daemon's socket must survive the alias attempt"
        );
    }

    /// Issue #1211 property 2, at the level of one path: every squatter shape
    /// an unprivileged process can plant is refused by the trust check and left
    /// exactly as found — nothing is connected to, nothing is unlinked.
    ///
    /// The symlink is aimed at a LIVE socket of ours on purpose: following it
    /// would find a daemon that answers, and the refusal has to come from the
    /// `lstat` first. The foreign-uid shape cannot be planted without a second
    /// uid; it reaches the same trust check, whose uid clause is pinned by
    /// `fsperm`'s `endpoint_uid_trust_refuses_a_foreign_uid`, and the daemon
    /// level of it is `daemon::legacy_alias_tests`.
    #[test]
    fn every_squatter_shape_at_the_legacy_path_is_refused_and_left_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = sandbox();
        let live = dir.path().join("live.sock");
        let _live = bind_trusted(&live);

        let file = dir.path().join("file.sock");
        std::fs::write(&file, b"squatter").expect("plant a regular file");
        let link = dir.path().join("link.sock");
        std::os::unix::fs::symlink(&live, &link).expect("plant a symlink");
        let directory = dir.path().join("dir.sock");
        std::fs::create_dir(&directory).expect("plant a directory");
        let loose = dir.path().join("loose.sock");
        drop(std::os::unix::net::UnixListener::bind(&loose).expect("bind a loose socket"));
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o666))
            .expect("loosen its mode");

        for squatted in [&file, &link, &directory, &loose] {
            let before = std::fs::symlink_metadata(squatted).expect("lstat the squatter");
            match prepare_legacy_alias(squatted) {
                Err(LegacyAliasSkip::Untrusted(_)) => {}
                other => panic!(
                    "{} must be refused as untrusted, got {other:?}",
                    squatted.display()
                ),
            }
            let after = std::fs::symlink_metadata(squatted)
                .unwrap_or_else(|e| panic!("{} was removed: {e}", squatted.display()));
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                (before.ino(), before.mode()),
                (after.ino(), after.mode()),
                "{} must be left exactly as found",
                squatted.display()
            );
        }
        assert_eq!(
            std::fs::read(&file).expect("read the planted file"),
            b"squatter"
        );
    }

    /// The alias guard removes the socket it bound when dropped…
    #[test]
    fn a_legacy_alias_unlinks_its_own_socket_on_drop() {
        let dir = sandbox();
        let legacy = dir.path().join("legacy.sock");
        let listener = bind_trusted(&legacy);
        let alias = LegacyAlias::adopt(legacy.clone());
        drop(alias);
        assert!(
            std::fs::symlink_metadata(&legacy).is_err(),
            "the alias must not outlive the daemon that bound it"
        );
        drop(listener);
    }

    /// …and never a socket that has since taken the name. An older daemon's
    /// startup unlinks whatever sits at its attach path before binding, so the
    /// name can change hands while this daemon still holds its listener;
    /// removing by name alone would delete the other daemon's live endpoint.
    #[test]
    fn a_legacy_alias_never_unlinks_a_socket_that_replaced_it() {
        let dir = sandbox();
        let legacy = dir.path().join("legacy.sock");
        let ours = bind_trusted(&legacy);
        let alias = LegacyAlias::adopt(legacy.clone());

        std::fs::remove_file(&legacy).expect("the other daemon unlinks our inode");
        let _theirs = bind_trusted(&legacy);
        drop(alias);

        assert!(
            endpoint_answers(&legacy),
            "the replacement daemon's socket must survive our shutdown"
        );
        drop(ours);
    }

    // -- relocation when the per-uid directory is taken (issue #1173) --------

    /// The uid a test process is *not*, so an entry it made reads as another
    /// user's. Relocation is decided by ownership, and a test cannot create an
    /// entry a second account owns without one — so the rule is driven the
    /// other way round, by asking about our own entries on behalf of a uid
    /// that does not own them.
    fn someone_else() -> u32 {
        crate::platform::paths::current_uid().wrapping_add(1)
    }

    #[test]
    fn only_an_entry_another_uid_owns_is_foreign() {
        let root = sandbox();
        let uid = crate::platform::paths::current_uid();
        let dir = root.path().join("dot-agent-deck-1");

        assert!(
            !entry_is_foreign(&dir, uid),
            "an absent name is free, not taken — the ordinary first launch"
        );

        std::fs::create_dir(&dir).unwrap();
        assert!(
            !entry_is_foreign(&dir, uid),
            "our own directory is not foreign"
        );
        assert!(entry_is_foreign(&dir, someone_else()));

        // `lstat`: a link is judged by who made it, not by what it points at.
        // Ours, pointing at the root-owned `/`, is still ours — and so is left
        // to `ensure_owner_only_dir`'s symlink refusal rather than relocated.
        let link = root.path().join("dot-agent-deck-2");
        std::os::unix::fs::symlink("/", &link).unwrap();
        assert!(!entry_is_foreign(&link, uid));
        assert!(entry_is_foreign(&link, someone_else()));
    }

    #[test]
    fn the_relocated_prefix_never_matches_another_uids_directory() {
        let prefix = relocated_dir_prefix(Path::new("/tmp/dot-agent-deck-1")).unwrap();
        assert_eq!(prefix, "dot-agent-deck-1.");
        assert!(
            !"dot-agent-deck-10".starts_with(&prefix),
            "uid 1's relocated prefix must not claim uid 10's per-uid directory"
        );
        assert!(!"dot-agent-deck-tunnels-1".starts_with(&prefix));
    }

    #[test]
    fn relocation_is_chosen_for_a_foreign_name_and_then_kept_while_the_name_is_free() {
        use std::os::unix::fs::PermissionsExt;
        let root = sandbox();
        let uid = crate::platform::paths::current_uid();
        let primary = root.path().join(format!("dot-agent-deck-{uid}"));

        assert_eq!(
            needs_relocation(&primary, uid),
            None,
            "a free name with nothing relocated is the ordinary first launch"
        );

        std::fs::create_dir(&primary).unwrap();
        std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            needs_relocation(&primary, uid),
            None,
            "our own directory is used"
        );
        assert_eq!(
            needs_relocation(&primary, someone_else()),
            Some(RelocationReason::Taken),
            "another uid's entry is relocated around"
        );

        // The squatter removes their entry while a relocated directory exists:
        // the choice must not flip back, or a launcher and the daemon it spawns
        // could disagree, and a second daemon could bind beside a live one.
        std::fs::remove_dir(&primary).unwrap();
        let relocated = relocated_bind_dir(&primary, uid).unwrap();
        assert_eq!(
            needs_relocation(&primary, uid),
            Some(RelocationReason::AlreadyRelocated)
        );

        // …but once the per-uid directory is ours again, it wins.
        std::fs::create_dir(&primary).unwrap();
        std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(needs_relocation(&primary, uid), None);
        assert!(relocated.exists());
    }

    #[test]
    fn the_fallback_directory_is_chosen_and_made_in_one_locked_step() {
        use std::os::unix::fs::MetadataExt;
        let root = sandbox();
        let locks = root.path().join("locks");
        let uid = crate::platform::paths::current_uid();
        let primary = root.path().join(format!("dot-agent-deck-{uid}"));

        assert_eq!(
            choose_fallback_dir(&primary, uid, &locks).unwrap(),
            None,
            "a free name is used, not relocated around"
        );
        let metadata = std::fs::symlink_metadata(&primary).unwrap();
        assert!(metadata.file_type().is_dir() && metadata.mode() & 0o777 == 0o700);
        assert!(
            locks.join(RELOCATION_LOCK_FILE).exists(),
            "the per-uid directory is made under the relocation lock too"
        );

        // Asked on behalf of a uid that does not own it, the same directory is
        // someone else's: relocate, and say why.
        let (relocated, reason) = choose_fallback_dir(&primary, someone_else(), &locks)
            .unwrap()
            .expect("a foreign per-uid directory is relocated around");
        assert_eq!(reason, RelocationReason::Taken);
        assert!(
            relocated
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("dot-agent-deck-{uid}."))
        );
    }

    #[test]
    fn a_relocated_directory_is_created_owner_only_and_then_reused() {
        use std::os::unix::fs::MetadataExt;
        let root = sandbox();
        let uid = crate::platform::paths::current_uid();
        let primary = root.path().join(format!("dot-agent-deck-{uid}"));

        let first = relocated_bind_dir(&primary, uid).expect("create a relocated directory");
        let name = first.file_name().unwrap().to_str().unwrap().to_string();
        let digits = name
            .strip_prefix(&format!("dot-agent-deck-{uid}."))
            .expect("the name is the per-uid directory's plus a dot");
        assert_eq!(digits.len(), 16, "64 bits, hex-encoded: {name}");
        assert!(digits.bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
        let metadata = std::fs::symlink_metadata(&first).unwrap();
        assert!(metadata.file_type().is_dir());
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert!(
            !primary.exists(),
            "relocating must not create the taken name itself"
        );

        assert_eq!(
            relocated_bind_dir(&primary, uid).unwrap(),
            first,
            "a second start must reuse the directory, not mint one per start — \
             that is what lets the launcher and the daemon it spawns agree"
        );
    }

    #[test]
    fn only_an_owner_only_directory_of_ours_counts_as_relocated() {
        use std::os::unix::fs::PermissionsExt;
        let root = sandbox();
        let uid = crate::platform::paths::current_uid();
        let primary = root.path().join(format!("dot-agent-deck-{uid}"));
        let prefix = format!("dot-agent-deck-{uid}.");

        // Every shape a squatter — or an accident — can leave at the prefix.
        let real_dir_target = root.path().join("elsewhere");
        std::fs::create_dir(&real_dir_target).unwrap();
        std::fs::set_permissions(&real_dir_target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real_dir_target, root.path().join(format!("{prefix}aa")))
            .unwrap();
        std::fs::write(root.path().join(format!("{prefix}bb")), b"").unwrap();
        let loose = root.path().join(format!("{prefix}cc"));
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(root.path().join(&prefix)).unwrap(); // the bare prefix
        let good = root.path().join(format!("{prefix}dd"));
        std::fs::create_dir(&good).unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            relocated_endpoint_dirs(&primary, uid),
            vec![good.clone()],
            "a symlink, a file, a loose directory and the bare prefix must all be skipped"
        );
        assert!(
            relocated_endpoint_dirs(&primary, someone_else()).is_empty(),
            "a directory another uid owns is never a candidate — the only kind a \
             squatter can create"
        );
        assert_eq!(
            relocated_bind_dir(&primary, uid).unwrap(),
            good,
            "the bind side reuses the valid one and ignores the rest"
        );
    }

    #[test]
    fn the_connect_side_searches_only_when_the_per_uid_directory_is_not_ours() {
        use std::os::unix::fs::PermissionsExt;
        let root = sandbox();
        let uid = crate::platform::paths::current_uid();
        let primary_dir = root.path().join(format!("dot-agent-deck-{uid}"));
        let primary = primary_dir.join("attach.sock");
        let relocated = root
            .path()
            .join(format!("dot-agent-deck-{uid}.0123456789abcdef"));
        std::fs::create_dir(&relocated).unwrap();
        std::fs::set_permissions(&relocated, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            relocated_candidates(&primary, uid),
            vec![relocated.join("attach.sock")],
            "an absent per-uid directory is searched: a relocated daemon outlives \
             the squatter removing their entry"
        );

        std::fs::write(&primary_dir, b"").unwrap();
        assert_eq!(
            relocated_candidates(&primary, uid),
            vec![relocated.join("attach.sock")],
            "a per-uid name that is not a directory of ours is searched"
        );

        std::fs::remove_file(&primary_dir).unwrap();
        std::fs::create_dir(&primary_dir).unwrap();
        std::fs::set_permissions(&primary_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            relocated_candidates(&primary, uid).is_empty(),
            "with the per-uid directory ours a daemon would have bound there, so \
             the listing is not paid"
        );
    }

    #[test]
    fn a_daemon_answering_in_a_relocated_directory_is_found_past_a_stale_one() {
        use std::os::unix::fs::PermissionsExt;
        let root = sandbox();
        // A one-letter per-uid name, so the relocated socket path fits macOS's
        // 104-byte `sun_path` under its long per-user `$TMPDIR` — the prefix
        // rule reads the name, so nothing about the lookup changes.
        let primary = root.path().join("d").join("attach.sock");

        // Sorted first, with a dead socket in it — left by an earlier episode.
        let stale_dir = root.path().join("d.0000000000000000");
        let live_dir = root.path().join("d.ffffffffffffffff");
        for dir in [&stale_dir, &live_dir] {
            std::fs::create_dir(dir).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        drop(bind_trusted(&stale_dir.join("attach.sock")));
        let _live = bind_trusted(&live_dir.join("attach.sock"));

        assert_eq!(
            answering_relocated_endpoint(&primary),
            Some(live_dir.join("attach.sock"))
        );
    }

    #[tokio::test]
    async fn an_explicit_address_is_never_relocated() {
        let root = sandbox();
        let chosen = root.path().join("hook.sock");
        for source in [EndpointSource::Override, EndpointSource::PlatformDefault] {
            assert_eq!(
                prepare_bind_endpoints(&[ResolvedEndpoint::new(chosen.clone(), source)])
                    .await
                    .unwrap(),
                vec![chosen.clone()],
                "{source:?}: only the fallback arm's address may move"
            );
        }
    }

    /// A `tempfile::tempdir()` whose mode is restated after creation — see the
    /// twin helper in [`crate::platform::fsperm`]'s tests for why. This module
    /// binds sockets inside its temp roots, so a root that came back without a
    /// search bit fails the bind rather than the assertion.
    fn sandbox() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod the temp root");
        root
    }

    fn bind_trusted(path: &Path) -> std::os::unix::net::UnixListener {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind");
        crate::platform::fsperm::set_endpoint_mode_owner_only(path).expect("0o600");
        listener
    }
}
