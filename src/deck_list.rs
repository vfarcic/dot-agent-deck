//! Issue #1350 — the one deck list both clients share.
//!
//! `~/.config/dot-agent-deck/remotes.toml` has been the CLI's registry since
//! PRD #76 (`remote add`, `connect`, `remote upgrade`), and since #1350 it is
//! the desktop app's deck list as well. Both clients read and write it through
//! this module, which is why it lives in the root crate (CLAUDE.md rule 18):
//! the desktop depends on this package by path, so a second copy of the file
//! format would be a second thing to keep in step.
//!
//! # Three properties every writer here keeps
//!
//! 1. **Every edit is made against a fresh read.** [`add`], [`update`], [`rename`] and
//!    [`remove`] each read the file at call time, apply exactly one change and
//!    write atomically (temp file + rename, [`write_atomic`]). A long-running
//!    writer — the desktop, which stays open for days while the user runs
//!    `remote add` in a terminal — therefore never writes back a list it loaded
//!    earlier, which is the stale-copy clobber #828 fixed inside `desktop.toml`.
//!    And the read, the change and the rename are held under one exclusive
//!    lock, across processes as well as threads ([`edit`]), so the CLI and the
//!    desktop saving at the same moment cannot both read the old file and have
//!    the later rename discard the other's change.
//! 2. **Keys this build does not know survive a re-save.** The file is edited
//!    as a [`toml_edit::DocumentMut`], and an entry is updated key by key, so a
//!    field a newer build added — and every comment — is left where it was. An
//!    **older** CLI does not do this: its `RemotesFile::save` re-serialises the
//!    whole file, so `remote add` from a build before #1350 drops `id`, `user`,
//!    `jump_host` and `socket` from every row. That loss is accepted and
//!    documented (`docs/remote-environments.md`); it is why a row without an
//!    `id` still has one, derived by [`deck_id`].
//! 3. **What is written still parses under the old, strict shape.** A build
//!    before #1350 requires `type`, `version` and `added_at` on every row and
//!    fails to parse the *whole* file without them, so every row written here
//!    carries all three — a deck the desktop added carries
//!    [`UNMANAGED_VERSION`] as its version.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::remote::{RemoteConfigError, RemoteEntry, RemotesFile};
use crate::remote_tunnel::{
    HostAlias, Hostname, RemoteSocketPath, SshArgumentError, SshPort, SshUser,
};

/// The `version` a deck carries when no `dot-agent-deck` binary was installed
/// on it by `remote add` — today, every deck added from the desktop app.
///
/// A row must have *some* `version` (property 3 above), and what an older CLI
/// does with the value decides which one is safe. Measured against the code
/// rather than assumed: `remote list` prints it verbatim in its VERSION column,
/// `remote upgrade` overwrites it without reading it, and `connect` and
/// `remote doctor` never read it at all — they ask the remote binary for its
/// version over ssh. So any string parses and nothing parses it as a version.
///
/// It is deliberately **not** version-shaped. `0.0.0` would read in `remote
/// list` as a real, ancient install, and an empty string as a broken row;
/// `unmanaged` says what is true — the CLI did not install this deck's binary —
/// and `remote upgrade <name>` replaces it with a real version the first time
/// the CLI does.
pub const UNMANAGED_VERSION: &str = "unmanaged";

/// The largest `remotes.toml` this module (and `RemotesFile::load`) will read.
///
/// A row is a few hundred bytes, so 1 MiB is thousands of decks — far past any
/// real list — while turning a path that points at `/dev/zero` or a multi-GB
/// file into a named read error instead of an out-of-memory kill. The desktop's
/// own `desktop.toml` is capped the same way (`MAX_SETTINGS_BYTES`, 256 KiB);
/// this one is larger because it is a list the user grows.
pub const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;

/// Read the registry at `path`, bounded: `Ok(None)` when it does not exist, and
/// an ordinary [`RemoteConfigError::Io`] — the "cannot read `remotes.toml`"
/// every caller already reports — when it is not a regular file (a FIFO, a
/// device, a directory; a symlink is followed and its target judged) or is
/// larger than [`MAX_REGISTRY_BYTES`]. See
/// [`crate::bounded_read::read_config_file`].
pub fn read_registry(path: &Path) -> Result<Option<String>, RemoteConfigError> {
    crate::bounded_read::read_config_file(path, MAX_REGISTRY_BYTES).map_err(|source| {
        RemoteConfigError::Io {
            path: path.display().to_string(),
            source,
        }
    })
}

/// The longest deck name [`validate_deck_name`] accepts.
pub const MAX_DECK_NAME_BYTES: usize = 64;

/// The longest id [`deck_id`] will use as written — the desktop's
/// `MAX_ENDPOINT_ID_BYTES`.
pub const MAX_DECK_ID_BYTES: usize = 64;

/// The two words a deck id may not be, because the desktop's deck selection
/// uses them to mean the local deck and every deck at once.
const RESERVED_IDS: [&str; 2] = ["local", "all"];

/// The `added_at` / `upgraded_at` spelling: RFC 3339, UTC — what `remote add`
/// has always written. A function so a client of this module (the desktop,
/// which has no `chrono` of its own) writes the same spelling.
pub fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// Why a name is not a valid deck name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeckNameError {
    #[error("a deck name cannot be empty")]
    Empty,
    #[error("a deck name is at most {MAX_DECK_NAME_BYTES} bytes; got {len}")]
    TooLong { len: usize },
    #[error("a deck name must start with an ASCII letter or digit")]
    BadStart,
    #[error(
        "a deck name is ASCII letters, digits, '.', '-' and '_'; found byte 0x{byte:02x} at offset {offset}"
    )]
    ForbiddenByte { offset: usize, byte: u8 },
}

/// Whether `name` is a valid name for a **new** deck.
///
/// One function for both clients (issue #1350): `remote add` checks the name
/// the user typed, and the desktop's derived names pass through it too. The
/// rule is a slug — ASCII letters, digits, `.`, `-` and `_`, starting with a
/// letter or digit, at most [`MAX_DECK_NAME_BYTES`] — because the name is typed
/// as `connect <name>` in a shell and rendered in the desktop, and a slug is
/// safe in both without quoting or bidi handling.
///
/// **Applied on write only.** A registry written before this rule may hold a
/// name it refuses — `remote add` accepted any string — and such a file must
/// keep loading; nothing on the read path calls this.
pub fn validate_deck_name(name: &str) -> Result<(), DeckNameError> {
    if name.is_empty() {
        return Err(DeckNameError::Empty);
    }
    if name.len() > MAX_DECK_NAME_BYTES {
        return Err(DeckNameError::TooLong { len: name.len() });
    }
    if let Some((offset, byte)) = name
        .bytes()
        .enumerate()
        .find(|(_, byte)| !is_name_byte(*byte))
    {
        return Err(DeckNameError::ForbiddenByte { offset, byte });
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(DeckNameError::BadStart);
    }
    Ok(())
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
}

/// A valid, unused name for a deck at `host` (and `user`), for a client that
/// does not ask the user for one — the desktop, which deliberately takes no
/// free-form label.
///
/// The host, lowercased, with every byte [`validate_deck_name`] refuses turned
/// into `-`. If that is taken and there is a user, `user-host`. If that is
/// taken too, a numeric suffix: `host-2`, `host-3`, … The result always passes
/// [`validate_deck_name`] and is never one `taken` reports.
pub fn derive_deck_name(host: &str, user: Option<&str>, taken: impl Fn(&str) -> bool) -> String {
    let base = slug(host).unwrap_or_else(|| "deck".to_string());
    let mut candidates = vec![base.clone()];
    if let Some(with_user) = user.and_then(|user| slug(&format!("{user}-{host}"))) {
        candidates.push(with_user);
    }
    if let Some(free) = candidates.into_iter().find(|name| !taken(name)) {
        return free;
    }
    (2u64..)
        .map(|n| {
            let suffix = format!("-{n}");
            let keep = MAX_DECK_NAME_BYTES - suffix.len();
            let stem = base[..base.len().min(keep)].trim_end_matches(['-', '.', '_']);
            format!("{stem}{suffix}")
        })
        .find(|name| !taken(name))
        .expect("an unbounded suffix search always finds a free name")
}

/// `raw` lowercased and reduced to deck-name bytes, or `None` when nothing of
/// it survives.
fn slug(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii() && is_name_byte(lower as u8) {
            out.push(lower);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let bounded = &trimmed[..trimmed.len().min(MAX_DECK_NAME_BYTES)];
    let bounded = bounded.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    (!bounded.is_empty()).then(|| bounded.to_string())
}

/// A deck name [`validate_deck_name`] accepts, as a type.
///
/// For a client that holds a name as a *field* rather than checking one on
/// the way through — the desktop's deck row (issue #1426), whose settings
/// schema refuses a free-text `String` field. Its `Deserialize` runs the same
/// check [`Self::parse`] does, so a value that crossed a serde boundary is a
/// valid name.
///
/// A name the registry already holds is **not** necessarily one of these: the
/// rule applies on write only, and a row written before it may carry a name it
/// refuses. A reader that meets one has no `DeckName` for it and says so.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct DeckName(String);

impl DeckName {
    /// `raw` as a deck name, or why it is not one.
    pub fn parse(raw: &str) -> Result<Self, DeckNameError> {
        validate_deck_name(raw)?;
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeckName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for DeckName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A fingerprint of a stored deck name — 16 lowercase hex digits of the name's
/// FNV-1a hash, the one a derived `h-…` id is made of.
///
/// For a client that must tell two stored names apart **without holding
/// either** (issue #1426's review): the desktop does not carry a name
/// [`DeckName`] refuses, since it renders only validated slugs, yet a rename
/// from a stale window has to notice that such a name changed on disk to
/// another name the rule also refuses. Two different names reading the same
/// here is a 64-bit collision — possible, and the price of carrying no text.
///
/// Its `Deserialize` refuses anything but 16 lowercase hex digits, so a value
/// that crossed a serde boundary carries no text of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct NameDigest(String);

impl NameDigest {
    /// The digest of the stored name `name`.
    pub fn of(name: &str) -> Self {
        Self(format!("{:016x}", fnv1a(name.as_bytes())))
    }

    /// `raw` as a digest, or `None` when it is not 16 lowercase hex digits.
    pub fn parse(raw: &str) -> Option<Self> {
        (raw.len() == 16
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for NameDigest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw)
            .ok_or_else(|| serde::de::Error::custom("a name digest is 16 lowercase hex digits"))
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Whether `entry` carries an `id` of its own that [`deck_id`] answers with,
/// rather than one derived from its name.
fn has_own_id(entry: &RemoteEntry) -> bool {
    entry.id.as_deref().is_some_and(is_usable_deck_id)
}

/// An id no row in `entries` answers to, for a new row whose own [`deck_id`]
/// is already taken: `<that id>-2`, `-3`, … shortened to fit
/// [`MAX_DECK_ID_BYTES`].
///
/// Reachable since issue #1426: renaming a row that had no `id` stores the id
/// its old name derived ([`rename`]), so a later `remote add` under that old
/// name derives the same id. Refusing that add would leave a name nothing
/// uses unusable for a reason the user cannot see; the new row takes a fresh
/// id instead.
fn free_id(taken_id: &str, entries: &[RemoteEntry]) -> String {
    (2u64..)
        .map(|n| {
            let suffix = format!("-{n}");
            let keep = taken_id.len().min(MAX_DECK_ID_BYTES - suffix.len());
            // A usable id is ASCII, so any byte offset is a character boundary.
            format!("{}{suffix}", &taken_id[..keep])
        })
        .find(|id| is_usable_deck_id(id) && !entries.iter().any(|row| deck_id(row) == *id))
        // Each suffix gives a different usable id, so the search fails only
        // if `entries` holds a row for every one of the `u64` suffixes — far
        // more rows than a file that fits in memory can.
        .expect("a deck list holds fewer rows than there are suffixes")
}

/// Whether `raw` can be used as a deck id as written: the charset and bound of
/// the desktop's `EndpointId::parse` — ASCII letters, digits, `-` and `_`, at
/// most [`MAX_DECK_ID_BYTES`], and neither reserved word in any case. The
/// desktop crate pins that the two agree.
pub fn is_usable_deck_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_DECK_ID_BYTES
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && !RESERVED_IDS
            .iter()
            .any(|reserved| raw.eq_ignore_ascii_case(reserved))
}

/// The stable identity of a row: its `id` when it has a usable one, otherwise
/// one derived from its `name`.
///
/// A row without an `id` is ordinary rather than broken: every row `remote
/// add` wrote before #1350 has none, and an older CLI that re-saves the file
/// strips the ones the desktop wrote. The derived id is a pure function of the
/// name, which the file keeps unique, so it is the same on every read and
/// needs writing back only when the name changes — [`rename`] stores it then,
/// so the row keeps answering to the id it was known by (issue #1426). `n-<name>` when the name fits the id charset, and
/// `h-<16 hex digits>` of a stable hash of it otherwise; the prefixes keep both
/// forms apart from each other, from the desktop's minted 16-hex ids and from
/// the reserved words.
pub fn deck_id(entry: &RemoteEntry) -> String {
    match entry.id.as_deref() {
        Some(id) if is_usable_deck_id(id) => id.to_string(),
        _ => derived_id(&entry.name),
    }
}

fn derived_id(name: &str) -> String {
    let readable = format!("n-{name}");
    if is_usable_deck_id(&readable) {
        readable
    } else {
        format!("h-{:016x}", fnv1a(name.as_bytes()))
    }
}

/// FNV-1a, 64-bit. Chosen over `DefaultHasher` because a derived id is stored
/// (as the desktop's selection) and must not change between Rust releases.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Split an ssh destination `[user@]host` into its login and its host.
///
/// On the **last** `@`, which is where OpenSSH splits a destination, so a UPN
/// login (`dev@REALM@host`) is the login `dev@REALM` at the host `host`. The
/// one splitter every reader of a `host` field uses — [`login_and_host`] (and
/// through it the desktop's row conversion), [`validate_host_field`] and
/// `SshTarget::parse` — so what one of them accepts, the others read the same
/// way. Before issue #1350's review the readers split at the first `@` and the
/// validator at the last, so a deck `remote add` accepted was one the desktop
/// read as the host `REALM@host` and skipped.
pub fn split_login(destination: &str) -> (Option<&str>, &str) {
    match destination.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, destination),
    }
}

/// The login name and bare host a row reaches.
///
/// `host` carries `[user@]host`, the way `remote add` stored what the user
/// typed, and is split by [`split_login`]; an explicit `user` field, when
/// present, wins over the one in `host`.
pub fn login_and_host(entry: &RemoteEntry) -> (Option<&str>, &str) {
    let (from_host, host) = split_login(&entry.host);
    (entry.user.as_deref().or(from_host), host)
}

/// The `host` and `user` fields to store for `host` and `user`.
///
/// The user is folded into `host` as `user@host` whenever an older CLI reads
/// it back the same way, because that is the one spelling an older CLI's
/// `connect` understands — it has never heard of a `user` field and would log
/// in as the local user. Only a login containing `@` itself (a Kerberos-style
/// `user@realm`) goes in the separate `user` field: this build reads a folded
/// `user@realm@host` correctly ([`split_login`]), but a build before #1350
/// split its `SshTarget` at the first `@` and would take `realm@host` as the
/// host.
pub fn host_fields(host: &str, user: Option<&str>) -> (String, Option<String>) {
    match user {
        Some(user) if user.contains('@') => (host.to_string(), Some(user.to_string())),
        Some(user) => (format!("{user}@{host}"), None),
        None => (host.to_string(), None),
    }
}

/// What two rows are compared on when deciding they are the same deck: the
/// bare host (lowercased, since DNS names are case-insensitive), the login and
/// the port. Used by the desktop's one-time migration (#1350) so a deck present
/// in both lists is not added twice.
pub fn address_key(entry: &RemoteEntry) -> (String, Option<String>, u16) {
    let (user, host) = login_and_host(entry);
    (
        host.to_ascii_lowercase(),
        user.map(str::to_string),
        entry.port,
    )
}

/// Clear `after`'s recorded install (`install` and `binary`, issue #1372) when
/// the edit moves the row to another endpoint — its [`address_key`] differs
/// from `before`'s — and does not record a new install itself.
///
/// What `remote upgrade` records is a fact about the machine it probed: a
/// Homebrew prefix is per host, and per login too (a Linuxbrew prefix can sit
/// under one user's home). Kept across a move, it has `connect` and `remote
/// doctor` run the old host's path on the new one and report the binary
/// missing (PR #1373's review). Cleared, the row falls back to
/// [`crate::remote::REMOTE_INSTALL_PATH`], the first path tried for a row with
/// no recorded install; `connect` can rediscover Homebrew if it is missing.
///
/// The port counts, because [`address_key`] is this module's one definition
/// of "the same deck" and a different port routinely *is* a different machine
/// (a forwarded port, a container's sshd). When it is the same machine, the
/// cost is one `remote upgrade` to re-detect; keeping a stale path costs a
/// deck that cannot connect with nothing saying why. The jump host and key do
/// not count: they change the route or the credential, not the destination.
///
/// Applied in [`DeckDocument::replace`], which every edit of an existing row
/// goes through — the CLI's [`update`] and the desktop's saves and migration
/// alike — so no writer can move a row and keep its install.
pub fn forget_install_if_moved(before: &RemoteEntry, after: &mut RemoteEntry) {
    let records_new_install = after.install != before.install || after.binary != before.binary;
    if address_key(before) != address_key(after) && !records_new_install {
        after.install = None;
        after.binary = None;
    }
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Whether the ssh-facing fields of `entry` are safe to write: the host (with
/// any `user@` it carries), the port, the key path, the `user` field, the jump
/// host and the remote socket path.
///
/// One definition for both clients, and it is the desktop's: every field but
/// the key goes through the validating newtype in [`crate::remote_tunnel`] that
/// the desktop's settings rows are made of ([`Hostname`], [`SshUser`],
/// [`SshPort`], [`HostAlias`], [`RemoteSocketPath`]) — a leading `-`,
/// whitespace, control bytes, non-ASCII and shell metacharacters are refused,
/// because OpenSSH interpolates `%h` and `%r` into a user's `ProxyCommand`,
/// which a local shell then runs. Checked in [`DeckDocument::push`] and
/// [`DeckDocument::replace`], so no write through this module can put an
/// unsafe value in the file whatever the caller checked first.
///
/// Two places it is wider than the newtypes, both because `remote add` has
/// always accepted the form and hands it to `ssh` as a destination, where it
/// works — see [`validate_host_field`] and [`validate_key_field`].
///
/// **Applied on write only**, like [`validate_deck_name`]: a row written before
/// this rule keeps loading, and [`update`] checks only the fields an edit
/// changes, so `connect` recording `last_connected` on such a row still works.
pub fn validate_deck_address(entry: &RemoteEntry) -> Result<(), SshArgumentError> {
    validate_changed_address(None, entry)
}

/// The fields `remote add` knows before it has reached the host — the
/// `[user@]host` target, the port and the key path — checked with the rules
/// [`validate_deck_address`] applies, so a refusal comes before any ssh.
pub fn validate_ssh_target(
    host: &str,
    port: u16,
    key: Option<&str>,
) -> Result<(), SshArgumentError> {
    validate_host_field(host)?;
    SshPort::parse(port)?;
    key.map(validate_key_field).transpose()?;
    Ok(())
}

/// [`validate_deck_address`] for the fields of `after` that differ from
/// `before` — every field when there is no `before`.
fn validate_changed_address(
    before: Option<&RemoteEntry>,
    after: &RemoteEntry,
) -> Result<(), SshArgumentError> {
    // `after`'s value of one optional field, when it has one `before` did not.
    fn changed<'a>(
        before: Option<&RemoteEntry>,
        after: &'a RemoteEntry,
        field: fn(&RemoteEntry) -> Option<&str>,
    ) -> Option<&'a str> {
        let value = field(after)?;
        (before.and_then(field) != Some(value)).then_some(value)
    }
    if before.map(|row| row.host.as_str()) != Some(after.host.as_str()) {
        validate_host_field(&after.host)?;
    }
    if before.map(|row| row.port) != Some(after.port) {
        SshPort::parse(after.port)?;
    }
    if let Some(key) = changed(before, after, |row| row.key.as_deref()) {
        validate_key_field(key)?;
    }
    if let Some(user) = changed(before, after, |row| row.user.as_deref()) {
        SshUser::parse(user)?;
    }
    if let Some(jump) = changed(before, after, |row| row.jump_host.as_deref()) {
        HostAlias::parse(jump)?;
    }
    if let Some(socket) = changed(before, after, |row| row.socket.as_deref()) {
        RemoteSocketPath::parse(socket)?;
    }
    Ok(())
}

/// The `host` field: `[user@]host`, the spelling `remote add` stores.
///
/// Split by [`split_login`], on the **last** `@`, so a UPN login
/// (`user@realm@host`) is checked as the [`SshUser`] it is. The host
/// is a [`Hostname`] — or, wider than the desktop, a bare IPv6 literal such as
/// `::1` or `fe80::1%eth0`: `remote add` has always passed one straight to
/// `ssh` as a destination, where it works. The desktop requires the bracketed
/// form (a bare `:` is ambiguous inside its `-L` forward spec) and skips such a
/// row on load rather than refusing the file. Accepted only when the address
/// actually parses as IPv6, so every byte is still a hex digit, `:` or a
/// zone id the [`Hostname`] charset already allowed.
pub fn validate_host_field(host: &str) -> Result<(), SshArgumentError> {
    let (user, bare) = split_login(host);
    if let Some(user) = user {
        SshUser::parse(user)?;
    }
    match Hostname::parse(bare) {
        Err(SshArgumentError::BareIpv6Separator { .. }) if is_bare_ipv6(bare) => Ok(()),
        other => other.map(|_| ()),
    }
}

/// `raw` is an unbracketed IPv6 literal, with an optional `%zone` of
/// hostname-charset bytes.
fn is_bare_ipv6(raw: &str) -> bool {
    let (address, zone) = match raw.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (raw, None),
    };
    address.parse::<std::net::Ipv6Addr>().is_ok()
        && zone.is_none_or(|zone| {
            !zone.is_empty()
                && zone
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        })
}

/// The `key` field: a path to a private key file, for `ssh -i`.
///
/// Wider than the desktop's [`KeyPath`](crate::remote_tunnel::KeyPath), which also requires an absolute or
/// `~/` path of a narrow ASCII charset: `remote add --key` has always taken any
/// path the shell hands it — `./id_ed25519`, a directory with a space in its
/// name — and passes it as the argument to `-i`, never through a shell or a
/// `ProxyCommand`, so refusing those would break working decks without making
/// one safer. What is refused is what is unsafe anywhere: an empty value, a
/// leading `-`, and a NUL or control byte. The desktop skips a row whose key
/// its `KeyPath` refuses, as it does any row it cannot represent.
pub fn validate_key_field(key: &str) -> Result<(), SshArgumentError> {
    const FIELD: &str = "the key path";
    if key.is_empty() {
        return Err(SshArgumentError::Empty { field: FIELD });
    }
    if key.starts_with('-') {
        return Err(SshArgumentError::LeadingDash { field: FIELD });
    }
    if let Some((offset, byte)) = key
        .bytes()
        .enumerate()
        .find(|(_, byte)| byte.is_ascii_control())
    {
        return Err(SshArgumentError::ForbiddenByte {
            field: FIELD,
            offset,
            what: crate::remote_tunnel::describe_byte(byte),
        });
    }
    Ok(())
}

/// Which row an edit is aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckRef<'a> {
    /// The row with this `name` — the CLI's handle (`connect <name>`).
    Name(&'a str),
    /// The row whose [`deck_id`] is this — the desktop's handle.
    Id(&'a str),
}

impl DeckRef<'_> {
    pub fn matches(&self, entry: &RemoteEntry) -> bool {
        match self {
            Self::Name(name) => entry.name == *name,
            Self::Id(id) => deck_id(entry) == *id,
        }
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// Why [`add`] refused to add a row.
#[derive(Debug, Error)]
pub enum AddDeckError {
    #[error("A remote named '{name}' already exists.")]
    DuplicateName { name: String },
    #[error("A deck with id '{id}' already exists.")]
    DuplicateId { id: String },
    #[error("Invalid remote name: {0}.")]
    InvalidName(#[from] DeckNameError),
    #[error("Invalid remote address: {0}.")]
    InvalidAddress(#[from] SshArgumentError),
    #[error(transparent)]
    Config(#[from] RemoteConfigError),
}

/// How `remote add` reports a refusal from [`add`]. `DuplicateId` cannot reach
/// it — `remote add` writes no `id` — but is mapped rather than assumed away.
impl From<AddDeckError> for crate::remote::RemoteAddError {
    fn from(error: AddDeckError) -> Self {
        use crate::remote::RemoteAddError;
        match error {
            AddDeckError::DuplicateName { name } => RemoteAddError::DuplicateName { name },
            AddDeckError::InvalidName(reason) => RemoteAddError::InvalidName(reason),
            AddDeckError::InvalidAddress(reason) => RemoteAddError::InvalidAddress(reason),
            AddDeckError::Config(error) => RemoteAddError::Registry(error),
            AddDeckError::DuplicateId { id } => {
                RemoteAddError::Registry(RemoteConfigError::Unwritable {
                    path: "remotes.toml".to_string(),
                    reason: format!("a deck with id '{id}' already exists"),
                })
            }
        }
    }
}

/// Serialises edits within one process, so two desktop windows saving at the
/// same moment cannot both read before either writes. The file lock
/// ([`acquire_edit_lock`]) serialises processes; this keeps threads of one
/// process from contending for it in a poll loop.
static EDIT_LOCK: Mutex<()> = Mutex::new(());

/// How long an edit waits for another process's edit of the same registry to
/// finish. An edit is a read, a small render and a rename — milliseconds; a
/// writer still holding the lock after this long is stuck, and failing the
/// edit visibly beats blocking `remote add` or a desktop save forever. The
/// desktop's `desktop.toml` save lock waits the same.
pub const EDIT_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// How often a waiting edit retries the lock.
const EDIT_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// The sidecar [`edit`] locks: `.remotes.toml.lock` beside the registry.
fn edit_lock_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "remotes.toml".to_string());
    registry_dir(path).join(format!(".{name}.lock"))
}

/// The directory the registry, its temp files and its lock live in.
fn registry_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Take the exclusive lock that serialises edits of the registry at `path`
/// across processes (issue #1350's review) — the CLI's `remote add` /
/// `remove` / `upgrade` / `connect`, and the desktop's saves and its one-time
/// migration. Released when the returned file is dropped.
///
/// The same mechanism as the desktop's `acquire_save_lock` for `desktop.toml`,
/// for the same reasons: the registry itself cannot carry the lock, because
/// every edit **replaces** it by rename and a lock on the old inode would not
/// exclude a process that opened the new one; so a sidecar that is never
/// replaced and never deleted holds it (deleting a lock file others may be
/// waiting on is how two processes end up locking different inodes). An empty,
/// owner-only `.remotes.toml.lock` therefore stays beside the registry; it
/// holds no data. `File::try_lock` is `flock(2)` on Unix and `LockFileEx` on
/// Windows, so it is one call on every platform this crate builds for.
///
/// Every failure to take it is an error — [`RemoteConfigError::Locked`] — and
/// the edit neither reads nor writes: the sidecar's name is taken by something
/// that is not a regular file, it cannot be opened, or another process still
/// holds it after [`EDIT_LOCK_WAIT`]. Going ahead unlocked is exactly the lost
/// update the lock exists to stop.
///
/// The one exception, again the desktop's: a filesystem that **cannot lock at
/// all** (the call reports `Unsupported`). Refusing there would make the deck
/// list uneditable for as long as the config directory lives on it, to close a
/// window one edit wide; so the edit goes ahead unlocked and says so on stderr.
/// It is still made against a fresh read, so only an edit racing inside those
/// milliseconds can be lost.
fn acquire_edit_lock(path: &Path) -> Result<Option<std::fs::File>, RemoteConfigError> {
    let lock_path = edit_lock_path(path);
    let locked = |reason: String| RemoteConfigError::Locked {
        path: path.display().to_string(),
        reason,
    };

    match std::fs::symlink_metadata(&lock_path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(locked(
                "the lock file's name is taken by something that is not a regular file. \
                 Remove it and try again"
                    .to_string(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(locked(format!(
                "the lock file cannot be inspected: {error}"
            )));
        }
    }

    let mut options = std::fs::OpenOptions::new();
    // Nothing is ever written to it; `write` is what `create` requires, and on
    // Windows `LockFileEx` needs a handle opened for reading or writing.
    options.read(true).write(true).create(true).truncate(false);
    // Owner-only on Unix for tidiness. Not on Windows, where the helper pins
    // the handle's access mask for the DACL it applies — and an empty file has
    // nothing a DACL would protect.
    #[cfg(unix)]
    crate::platform::fsperm::set_create_mode_owner_only(&mut options);
    let file = options
        .open(&lock_path)
        .map_err(|error| locked(format!("the lock file cannot be opened: {error}")))?;

    let deadline = std::time::Instant::now() + EDIT_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(EDIT_LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(locked(
                    "another program has been editing the deck list for too long. Try again"
                        .to_string(),
                ));
            }
            Err(std::fs::TryLockError::Error(error))
                if error.kind() == std::io::ErrorKind::Unsupported =>
            {
                eprintln!(
                    "Editing {} without the cross-process lock, which this filesystem does not \
                     support: {error}",
                    path.display()
                );
                return Ok(None);
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(locked(format!("the lock cannot be taken: {error}")));
            }
        }
    }
}

/// The registry as a format-preserving document, handed to an [`edit`]
/// closure. Rows are addressed by index into [`Self::entries`].
pub struct DeckDocument {
    doc: toml_edit::DocumentMut,
    path: String,
}

impl DeckDocument {
    fn open(path: &Path, contents: Option<&str>) -> Result<Self, RemoteConfigError> {
        let display = path.display().to_string();
        let mut doc = match contents {
            Some(contents) => {
                // The strict read first: an edit never lands on a file this
                // build cannot load, since the load would then have refused it.
                toml::from_str::<RemotesFile>(contents).map_err(|source| {
                    RemoteConfigError::Parse {
                        path: display.clone(),
                        source,
                    }
                })?;
                contents
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|error| unwritable(&display, &error.to_string()))?
            }
            None => toml_edit::DocumentMut::new(),
        };
        // `remotes = []` is what `RemotesFile::save` writes for an empty list,
        // and `remotes = [{ … }]` is a legal hand spelling: both become
        // `[[remotes]]` so rows can be edited one at a time.
        if let Some(item) = doc.get_mut("remotes")
            && !item.is_array_of_tables()
        {
            let taken = std::mem::take(item);
            let empty = taken.as_array().is_some_and(toml_edit::Array::is_empty);
            *item = match taken.into_array_of_tables() {
                Ok(rows) => toml_edit::Item::ArrayOfTables(rows),
                // `into_array_of_tables` refuses an empty array, which is
                // exactly the spelling `RemotesFile::save` uses for no rows.
                Err(_) if empty => toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
                Err(_) => {
                    return Err(unwritable(&display, "`remotes` is not a list of tables"));
                }
            };
        }
        Ok(Self { doc, path: display })
    }

    fn rows(&self) -> Option<&toml_edit::ArrayOfTables> {
        self.doc.get("remotes")?.as_array_of_tables()
    }

    fn rows_mut(&mut self) -> &mut toml_edit::ArrayOfTables {
        let item = self
            .doc
            .entry("remotes")
            .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
        item.as_array_of_tables_mut()
            .expect("`open` leaves `remotes` absent or a list of tables")
    }

    /// Every row, as this build reads it, in file order.
    pub fn entries(&self) -> Result<Vec<RemoteEntry>, RemoteConfigError> {
        toml::from_str::<RemotesFile>(&self.doc.to_string())
            .map(|file| file.remotes)
            .map_err(|source| RemoteConfigError::Parse {
                path: self.path.clone(),
                source,
            })
    }

    /// Append `entry` as a new row, refusing one whose address
    /// [`validate_deck_address`] refuses.
    pub fn push(&mut self, entry: &RemoteEntry) -> Result<(), RemoteConfigError> {
        validate_deck_address(entry).map_err(|source| self.invalid_address(source))?;
        let table = entry_table(entry, &self.path)?;
        self.rows_mut().push(table);
        Ok(())
    }

    /// Make row `index` say what `entry` says, key by key.
    ///
    /// A key whose value is unchanged keeps its own bytes, a key `entry` no
    /// longer carries (an optional field set to `None`) is removed, and a key
    /// this build does not know is not touched at all — which is property 2 in
    /// the module docs. Which keys this build knows is read off its own
    /// serialisation of the row rather than listed by hand, so a field added
    /// to [`RemoteEntry`] later is covered without an edit here.
    ///
    /// The address fields `entry` changes are checked with
    /// [`validate_deck_address`]; one it leaves as it was is not, so a row
    /// written before that rule can still be edited in every other field.
    ///
    /// An edit that moves the row to another endpoint drops its recorded
    /// install ([`forget_install_if_moved`]).
    pub fn replace(&mut self, index: usize, entry: &RemoteEntry) -> Result<(), RemoteConfigError> {
        let path = self.path.clone();
        let before = self
            .entries()?
            .into_iter()
            .nth(index)
            .ok_or_else(|| unwritable(&path, "no such row"))?;
        validate_changed_address(Some(&before), entry)
            .map_err(|source| self.invalid_address(source))?;
        let mut entry = entry.clone();
        forget_install_if_moved(&before, &mut entry);
        let old = entry_table(&before, &path)?;
        let new = entry_table(&entry, &path)?;
        let row = self
            .rows_mut()
            .get_mut(index)
            .ok_or_else(|| unwritable(&path, "no such row"))?;
        for (key, value) in new.iter() {
            let unchanged = old.get(key).map(ToString::to_string) == Some(value.to_string());
            match (row.get_mut(key), value.as_value()) {
                (Some(_), _) if unchanged => {}
                // A changed value keeps the comments and spacing around it
                // (issue #1426: a rename's comment above `name = …` used to go
                // with the old name).
                (Some(toml_edit::Item::Value(existing)), Some(value)) => {
                    let decor = existing.decor().clone();
                    *existing = value.clone();
                    *existing.decor_mut() = decor;
                }
                _ => {
                    row.insert(key, value.clone());
                }
            }
        }
        for (key, _) in old.iter() {
            if !new.contains_key(key) {
                row.remove(key);
            }
        }
        Ok(())
    }

    fn invalid_address(&self, source: SshArgumentError) -> RemoteConfigError {
        RemoteConfigError::InvalidAddress {
            path: self.path.clone(),
            source,
        }
    }

    /// Remove row `index`.
    pub fn remove(&mut self, index: usize) {
        let rows = self.rows_mut();
        if index < rows.len() {
            rows.remove(index);
        }
    }
}

/// `entry` rendered by this build, as a table.
fn entry_table(entry: &RemoteEntry, path: &str) -> Result<toml_edit::Table, RemoteConfigError> {
    let text = toml::to_string(entry)?;
    let doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| unwritable(path, &error.to_string()))?;
    Ok(doc.as_table().clone())
}

fn unwritable(path: &str, reason: &str) -> RemoteConfigError {
    RemoteConfigError::Unwritable {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

/// Read `path` fresh, let `f` change the document, and write the result
/// atomically — or, if `f` changed nothing, write nothing.
///
/// The building block [`add`], [`update`], [`rename`] and [`remove`] are made of, and what
/// a caller with a batch uses (the desktop's saves and its one-time
/// migration): everything `f` does is published as one rename or not at all.
/// The result is checked to parse as a registry before it is published, so an
/// edit can never leave a file the next load would refuse. A missing file is
/// an empty registry, and is created only if `f` adds something.
///
/// **Held under an exclusive lock from the read to the rename**, across
/// processes ([`acquire_edit_lock`]) as well as threads, so two writers — the
/// CLI in a terminal and the desktop — can never both read the same original
/// and have the later rename silently discard the other's change. The lock's
/// directory is created (owner-only) if missing, since the sidecar lives in it.
///
/// **A symlinked registry is edited at its target** ([`publish_path`]): the
/// lock, the read, the temp file and the rename all use the file the link
/// resolves to, so the link survives and a writer that came in through the
/// link and one that came in through the target take the same lock. The path
/// is resolved **once**, before the lock, and published by [`write_resolved`],
/// which does not resolve it again — so the rename lands on the file that was
/// locked and read even if the registry is swapped for a symlink mid-edit.
pub fn edit<T, E>(path: &Path, f: impl FnOnce(&mut DeckDocument) -> Result<T, E>) -> Result<T, E>
where
    E: From<RemoteConfigError>,
{
    let _guard = EDIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = &publish_path(path)?;
    let dir = registry_dir(path);
    crate::platform::fsperm::create_owner_only_dir(dir).map_err(|source| {
        RemoteConfigError::Io {
            path: dir.display().to_string(),
            source,
        }
    })?;
    // Held until this function returns, which is after the rename.
    let _lock = acquire_edit_lock(path)?;
    let original = read_registry(path)?;
    let mut document = DeckDocument::open(path, original.as_deref())?;
    let value = f(&mut document)?;
    let rendered = document.doc.to_string();
    let unchanged = match &original {
        Some(original) => *original == rendered,
        None => document.rows().is_none_or(|rows| rows.is_empty()),
    };
    if unchanged {
        return Ok(value);
    }
    toml::from_str::<RemotesFile>(&rendered).map_err(|_| {
        unwritable(
            &path.display().to_string(),
            "the edit would leave the file unreadable, so nothing was written",
        )
    })?;
    write_resolved(path, &rendered)?;
    Ok(value)
}

/// Append `entry`, refusing a name or [`deck_id`] already in the file.
///
/// The name is checked with [`validate_deck_name`] and the address with
/// [`validate_deck_address`] — this is the write path, so this is where those
/// rules apply — and the duplicate checks run against the file as it is
/// **now**, not as the caller last saw it.
///
/// A row with no `id` of its own whose derived id another row already answers
/// to — a row [`rename`]d away from this name keeps the id the name derives —
/// is given a fresh stored id ([`free_id`]) rather than refused. An `id` the
/// caller set is never replaced: a clash with that is [`AddDeckError::DuplicateId`].
pub fn add(path: &Path, mut entry: RemoteEntry) -> Result<RemoteEntry, AddDeckError> {
    validate_deck_name(&entry.name)?;
    validate_deck_address(&entry)?;
    edit(path, |document| {
        let existing = document.entries()?;
        if existing.iter().any(|row| row.name == entry.name) {
            return Err(AddDeckError::DuplicateName {
                name: entry.name.clone(),
            });
        }
        let id = deck_id(&entry);
        if existing.iter().any(|row| deck_id(row) == id) {
            if has_own_id(&entry) {
                return Err(AddDeckError::DuplicateId { id });
            }
            entry.id = Some(free_id(&id, &existing));
        }
        document.push(&entry)?;
        Ok(entry)
    })
}

/// Apply `f` to the row `which` names, as the file holds it now, and return the
/// updated row — or `None`, writing nothing, when no row matches.
pub fn update(
    path: &Path,
    which: DeckRef<'_>,
    f: impl FnOnce(&mut RemoteEntry),
) -> Result<Option<RemoteEntry>, RemoteConfigError> {
    update_if(path, which, |_| Ok(()), f)
}

/// [`update`], applied only when `check` accepts the row as the file holds it
/// now — read under the same lock the write takes, so nothing can change the
/// row between the check and the write. A refusal writes nothing and is
/// returned as it is.
pub fn update_if<E>(
    path: &Path,
    which: DeckRef<'_>,
    check: impl FnOnce(&RemoteEntry) -> Result<(), E>,
    f: impl FnOnce(&mut RemoteEntry),
) -> Result<Option<RemoteEntry>, E>
where
    E: From<RemoteConfigError>,
{
    edit(path, |document| {
        let entries = document.entries()?;
        let Some(index) = entries.iter().position(|row| which.matches(row)) else {
            return Ok(None);
        };
        check(&entries[index])?;
        let mut entry = entries[index].clone();
        f(&mut entry);
        // What `replace` will write, so the row returned is the row on disk.
        forget_install_if_moved(&entries[index], &mut entry);
        if entry != entries[index] {
            document.replace(index, &entry)?;
        }
        Ok(Some(entry))
    })
}

/// Remove the row `which` names and return it — or `None`, writing nothing,
/// when no row matches.
pub fn remove(path: &Path, which: DeckRef<'_>) -> Result<Option<RemoteEntry>, RemoteConfigError> {
    edit(path, |document| {
        let entries = document.entries()?;
        let Some(index) = entries.iter().position(|row| which.matches(row)) else {
            return Ok(None);
        };
        document.remove(index);
        Ok(Some(entries[index].clone()))
    })
}

/// Why [`rename`] refused to rename a row.
///
/// The messages are shown as they are by the desktop's rename form (issue
/// #1426), so each says what to change — except [`Self::Config`], whose text
/// can name the registry's path and is for a log.
#[derive(Debug, Error)]
pub enum RenameDeckError {
    #[error("That deck is no longer in the deck list.")]
    NotFound,
    /// The row found is not the deck the caller was looking at — see
    /// [`rename`]'s `is_expected`.
    #[error(
        "That deck changed since this window loaded it, so it was not renamed. It is shown as it \
         is now; try again."
    )]
    Changed,
    #[error("A deck named '{name}' already exists.")]
    DuplicateName { name: String },
    #[error("Invalid deck name: {0}.")]
    InvalidName(#[from] DeckNameError),
    #[error(transparent)]
    Config(#[from] RemoteConfigError),
}

/// Give the row `which` names the name `new_name`, and return the row as
/// written.
///
/// One [`edit`], so the same fresh read, lock and atomic rename as every other
/// change: the duplicate check runs against the file as it is **now**, and a
/// `remote add` in a terminal cannot land between the check and the write.
/// `new_name` must pass [`validate_deck_name`] and must not be another row's
/// name. Renaming a row to the name it already has writes nothing and
/// succeeds.
///
/// # The row keeps its id
///
/// The desktop keys a deck on its [`deck_id`] — the stored selection in
/// `desktop.toml` and every row it holds — and a row with no `id` of its own
/// answers to one derived from its **name**, which a rename would move. So a
/// rename of such a row also stores the id it answered to before, and the
/// rename is invisible to everything keyed on it. A row with an `id` of its
/// own keeps it untouched. Every other key of the row, and every comment in
/// the file, stays as it was ([`DeckDocument::replace`]).
///
/// # A `which` is not proof the row is still that deck
///
/// A row with no `id` answers to one derived from its name, so a `remote
/// remove prod` then `remote add prod <other host>` hands the old deck's id to
/// an unrelated one — and a rename by that id, made against a list read
/// before, would rename the new deck. `is_expected` is shown the row `which`
/// found, as it is on disk under the lock; `false` refuses the rename with
/// [`RenameDeckError::Changed`] and writes nothing. A caller that read the row
/// earlier checks here that it still is the deck it read (the desktop compares
/// the address and the name it showed); one that just resolved `which` passes
/// `|_| true`.
pub fn rename(
    path: &Path,
    which: DeckRef<'_>,
    new_name: &str,
    is_expected: impl FnOnce(&RemoteEntry) -> bool,
) -> Result<RemoteEntry, RenameDeckError> {
    validate_deck_name(new_name)?;
    edit(path, |document| {
        let entries = document.entries()?;
        let index = entries
            .iter()
            .position(|row| which.matches(row))
            .ok_or(RenameDeckError::NotFound)?;
        let current = &entries[index];
        if !is_expected(current) {
            return Err(RenameDeckError::Changed);
        }
        if current.name == new_name {
            return Ok(current.clone());
        }
        if entries.iter().any(|row| row.name == new_name) {
            return Err(RenameDeckError::DuplicateName {
                name: new_name.to_string(),
            });
        }
        let mut renamed = current.clone();
        if !has_own_id(current) {
            renamed.id = Some(deck_id(current));
        }
        renamed.name = new_name.to_string();
        document.replace(index, &renamed)?;
        Ok(renamed)
    })
}

/// The file an edit of the registry at `path` reads, locks beside and
/// renames onto: `path` itself, unless `path` is a symlink — then the file the
/// link finally resolves to.
///
/// Reads have always followed a symlink at the registry path, deliberately:
/// dotfile managers (stow, chezmoi's symlink mode, a hand-made link into a
/// dotfiles repo) keep `remotes.toml` as a link. A rename onto the link's own
/// path replaces the **link** with a plain file, which silently detaches the
/// registry from the managed copy — every later edit lands in the new file and
/// the dotfiles repo keeps the stale one (issue #1350's review). Publishing at
/// the target keeps the link and updates what it points at. The lock sidecar
/// is derived from the same resolved path, so it lives beside the target, and
/// every writer — through the link or straight at the target — locks the one
/// `.remotes.toml.lock` there.
///
/// An absent path stays as given, so a first `remote add` still creates the
/// file where the configuration says. A **dangling** symlink is refused rather
/// than followed to create its target: that target can be anywhere, and the
/// directories on the way would be created owner-only by [`write_resolved`] — a
/// link left behind by a half-applied dotfile checkout, or planted, should not
/// decide where the deck list and its parent directories appear. Reads already
/// see such a link as an empty registry, so the refusal is on write only and
/// names the link.
fn publish_path(path: &Path) -> Result<std::path::PathBuf, RemoteConfigError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::canonicalize(path).map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    unwritable(
                        &path.display().to_string(),
                        "it is a symlink to a file that does not exist, so nothing was written. \
                         Create the file it points at, or remove the link",
                    )
                } else {
                    RemoteConfigError::Io {
                        path: path.display().to_string(),
                        source,
                    }
                }
            })
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(source) => Err(RemoteConfigError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Distinguishes temp files written by one process, which may save more than
/// once at a time (the desktop, from several windows).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many temp names [`write_resolved`] draws before giving up. The temp file
/// is created with `create_new`, which never opens whatever already holds a
/// name, so a leftover from a crashed run — or a symlink someone planted at the
/// predictable name — costs one draw instead of every later save.
const TEMP_NAME_ATTEMPTS: usize = 8;

/// Atomically replace the file at `path` with `contents`. Creates the parent
/// directory if missing. Writes via a sibling temp file with mode 0o600, then
/// `rename(2)`s it into place — so a partial write or a crash mid-save can
/// never leave a half-written `remotes.toml` for the next run to choke on, and
/// the final file is owner-only (0o600) regardless of the user's umask.
///
/// A `path` that is a symlink is written at its target ([`publish_path`]), so
/// the link is kept rather than replaced by a plain file. The path is resolved
/// once, here; the write itself is [`write_resolved`], which [`edit`] calls
/// directly with the path it already resolved and locked.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), RemoteConfigError> {
    write_resolved(&publish_path(path)?, contents)
}

/// [`write_atomic`] at a path [`publish_path`] has **already** resolved, which
/// it does not resolve again: the temp file goes beside `path` and the rename
/// lands on `path` itself, whatever `path` has become since.
///
/// [`edit`] resolves once and then locks, reads and publishes at that one
/// path. Resolving a second time here would let a registry swapped for a
/// symlink between the two resolutions redirect the rename onto a file the
/// edit neither locked nor read (issue #1350's review). `rename(2)` does not
/// follow a symlink at its destination, so such a link is replaced rather than
/// written through.
fn write_resolved(path: &Path, contents: &str) -> Result<(), RemoteConfigError> {
    use std::io::Write;

    // PRD #163 auditor: create the parent through the fsperm seam, not plain
    // `create_dir_all`, so the *directory* is owner-only too — the same call
    // `schedules.toml` already makes. The per-file DACL/mode protects the
    // contents; this protects the metadata (which remotes exist, by filename)
    // when `DOT_AGENT_DECK_REMOTES` puts the file somewhere shared. Create-only,
    // so an existing directory is never surprise-tightened (PRD #127 S2).
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    crate::platform::fsperm::create_owner_only_dir(parent).map_err(|source| {
        RemoteConfigError::Io {
            path: parent.display().to_string(),
            source,
        }
    })?;

    // Sibling temp file: same directory as the final path so `rename` is
    // atomic on POSIX filesystems. The pid and a per-process counter keep
    // concurrent saves — from several processes, or several windows of one —
    // off each other's temp file.
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "remotes.toml".to_string());

    // PRD #42 M1: owner-only (0o600) creation mode comes from the platform
    // seam — `.mode(0o600)` on Unix; on Windows (#163) the DACL cannot be
    // supplied at create time, so the seam instead puts `WRITE_DAC` on the
    // handle, which is what lets the `set_file_owner_only` call below apply it.
    //
    // `create_new` (`O_CREAT|O_EXCL`), the way the desktop's `desktop.toml`
    // save creates its temp file (issue #1350): the name is predictable, and
    // `O_EXCL` refuses to follow a symlink planted there — it fails with
    // `AlreadyExists` instead of writing through it to wherever it points. A
    // taken name draws a fresh counter value, a bounded number of times.
    let mut opened = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let tmp_path = parent.join(format!(
            "{file_name}.{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.create_new(true).write(true);
        crate::platform::fsperm::set_create_mode_owner_only(&mut open_opts);
        match open_opts.open(&tmp_path) {
            Ok(file) => {
                opened = Some((tmp_path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(RemoteConfigError::Io {
                    path: tmp_path.display().to_string(),
                    source,
                });
            }
        }
    }
    let Some((tmp_path, mut tmp_file)) = opened else {
        return Err(RemoteConfigError::Io {
            path: parent.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "every temp file name tried for the save was already taken",
            ),
        });
    };

    // Owner-only permissions BEFORE the first content byte (PRD #163 M4).
    //
    // Two reasons this runs here rather than after the write. (1) Defense in
    // depth on Unix: the bits are asserted on the handle rather than trusted
    // to the creation mode alone. (2) On Windows this call is not a re-assert but the *only*
    // place the protected current-user-only DACL is applied —
    // `std::fs::OpenOptions` has no `SECURITY_ATTRIBUTES` hook, so
    // `set_create_mode_owner_only` can only pre-authorize this call by putting
    // `WRITE_DAC` on the handle. Doing that after `write_all` would leave the
    // file readable under the parent directory's inherited ACL for the length
    // of the write; doing it now means only an empty file is ever exposed.
    if let Err(source) = crate::platform::fsperm::set_file_owner_only(&tmp_file) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(RemoteConfigError::Io {
            path: tmp_path.display().to_string(),
            source,
        });
    }

    if let Err(source) = tmp_file.write_all(contents.as_bytes()) {
        // Best-effort cleanup; ignore secondary errors.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(RemoteConfigError::Io {
            path: tmp_path.display().to_string(),
            source,
        });
    }
    drop(tmp_file);

    std::fs::rename(&tmp_path, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp_path);
        RemoteConfigError::Io {
            path: path.display().to_string(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, host: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            kind: "ssh".to_string(),
            host: host.to_string(),
            port: 22,
            key: None,
            version: "0.40.0".to_string(),
            added_at: "2026-01-01T00:00:00+00:00".to_string(),
            upgraded_at: None,
            last_connected: None,
            install: None,
            binary: None,
            id: None,
            user: None,
            jump_host: None,
            socket: None,
        }
    }

    fn desktop_entry(name: &str) -> RemoteEntry {
        RemoteEntry {
            key: Some("~/.ssh/id_ed25519".to_string()),
            version: UNMANAGED_VERSION.to_string(),
            id: Some("0123456789abcdef".to_string()),
            user: Some("dev@REALM".to_string()),
            jump_host: Some("bastion".to_string()),
            socket: Some("/run/user/1000/dot-agent-deck-attach.sock".to_string()),
            ..entry(name, "build.example.com")
        }
    }

    /// The registry shape a build before #1350 reads — copied, not imported,
    /// so it stays what those builds actually compiled. Strict in exactly the
    /// way they were: `type`, `version` and `added_at` required, unknown keys
    /// ignored.
    mod pre_1350 {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        pub struct RemoteEntry {
            pub name: String,
            #[serde(rename = "type")]
            pub kind: String,
            pub host: String,
            pub port: u16,
            #[serde(default)]
            pub key: Option<String>,
            pub version: String,
            pub added_at: String,
            #[serde(default)]
            pub upgraded_at: Option<String>,
            #[serde(default)]
            pub last_connected: Option<String>,
        }

        #[derive(Debug, Deserialize)]
        pub struct RemotesFile {
            #[serde(default)]
            pub remotes: Vec<RemoteEntry>,
        }
    }

    fn registry(dir: &tempfile::TempDir, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join("remotes.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_row_this_build_writes_parses_under_the_pre_1350_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("cli-box", "me@cli.example.com")).unwrap();
        add(&path, desktop_entry("desktop-box")).unwrap();

        let old: pre_1350::RemotesFile =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).expect("old CLI parses");
        assert_eq!(old.remotes.len(), 2);
        assert_eq!(old.remotes[1].kind, "ssh");
        assert_eq!(old.remotes[1].version, UNMANAGED_VERSION);
        assert_eq!(old.remotes[1].key.as_deref(), Some("~/.ssh/id_ed25519"));
    }

    #[test]
    fn keys_this_build_does_not_know_survive_an_update_and_a_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "# my decks\n\
             future = \"top-level\"\n\n\
             [[remotes]]\n\
             name = \"a\"\n\
             type = \"ssh\"\n\
             host = \"a.example\"\n\
             port = 22\n\
             version = \"0.40.0\"\n\
             added_at = \"2026-01-01T00:00:00Z\"\n\
             id = \"deck-a\"\n\
             jump_host = \"bastion\"\n\
             colour = \"green\" # a field from a newer build\n\n\
             [[remotes]]\n\
             name = \"b\"\n\
             type = \"ssh\"\n\
             host = \"b.example\"\n\
             port = 22\n\
             version = \"0.40.0\"\n\
             added_at = \"2026-01-01T00:00:00Z\"\n",
        );

        update(&path, DeckRef::Name("a"), |row| {
            row.version = "0.41.0".to_string();
        })
        .unwrap()
        .expect("row a exists");
        remove(&path, DeckRef::Name("b"))
            .unwrap()
            .expect("row b exists");

        let written = std::fs::read_to_string(&path).unwrap();
        for kept in [
            "# my decks",
            "future = \"top-level\"",
            "id = \"deck-a\"",
            "jump_host = \"bastion\"",
            "colour = \"green\" # a field from a newer build",
            "version = \"0.41.0\"",
        ] {
            assert!(written.contains(kept), "{kept:?} missing from:\n{written}");
        }
        assert!(!written.contains("b.example"), "{written}");
    }

    #[test]
    fn an_update_that_clears_an_optional_field_removes_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, desktop_entry("d")).unwrap();
        update(&path, DeckRef::Id("0123456789abcdef"), |row| {
            row.jump_host = None;
        })
        .unwrap()
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("jump_host"), "{written}");
        assert!(written.contains("socket ="), "{written}");
    }

    /// PR #1373's review: `install`/`binary` describe the machine `remote
    /// upgrade` probed, so an edit that moves the row elsewhere drops them —
    /// in the row returned as well as the row written — and one that leaves
    /// the endpoint alone, or records a new install with the move, keeps what
    /// it has.
    #[test]
    fn an_edit_that_moves_the_deck_forgets_the_recorded_install() {
        let homebrew = |row: &mut RemoteEntry| {
            row.install = Some(crate::remote::INSTALL_HOMEBREW.to_string());
            row.binary = Some(
                "/opt/homebrew/bin/dot-agent-deck"
                    .to_string()
                    .try_into()
                    .unwrap(),
            );
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("box", "me@box.example")).unwrap();
        update(&path, DeckRef::Name("box"), homebrew).unwrap();

        let kept = update(&path, DeckRef::Name("box"), |row| {
            row.host = "me@BOX.example".to_string();
            row.key = Some("~/.ssh/other".to_string());
            row.jump_host = Some("bastion".to_string());
        })
        .unwrap()
        .unwrap();
        assert_eq!(kept.install.as_deref(), Some("homebrew"), "same endpoint");

        let moved = update(&path, DeckRef::Name("box"), |row| {
            row.host = "me@elsewhere.example".to_string();
        })
        .unwrap()
        .unwrap();
        assert_eq!((&moved.install, &moved.binary), (&None, &None));
        assert_eq!(RemotesFile::load(&path).unwrap().remotes[0], moved);

        let reinstalled = update(&path, DeckRef::Name("box"), |row| {
            row.port = 2222;
            homebrew(row);
        })
        .unwrap()
        .unwrap();
        assert_eq!(reinstalled.install.as_deref(), Some("homebrew"));
        assert_eq!(RemotesFile::load(&path).unwrap().remotes[0], reinstalled);
    }

    #[test]
    fn an_edit_lands_on_the_file_as_it_is_now_not_as_a_caller_last_saw_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("first", "first.example")).unwrap();
        // A caller that loaded the list here…
        let stale = RemotesFile::load(&path).unwrap();
        // …while another writer adds a row…
        add(&path, entry("second", "second.example")).unwrap();
        // …then edits the row it knows about.
        update(&path, DeckRef::Name(&stale.remotes[0].name), |row| {
            row.port = 2222;
        })
        .unwrap()
        .unwrap();

        let now = RemotesFile::load(&path).unwrap();
        let names: Vec<_> = now.remotes.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["first", "second"]);
        assert_eq!(now.remotes[0].port, 2222);
    }

    #[test]
    fn the_empty_list_the_old_save_writes_can_be_added_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(&dir, "remotes = []\n");
        add(&path, entry("a", "a.example")).unwrap();
        assert_eq!(RemotesFile::load(&path).unwrap().remotes.len(), 1);
    }

    #[test]
    fn an_inline_list_of_rows_is_edited_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "remotes = [{ name = \"a\", type = \"ssh\", host = \"a.example\", port = 22, \
             version = \"0.40.0\", added_at = \"2026-01-01T00:00:00Z\" }]\n",
        );
        add(&path, entry("b", "b.example")).unwrap();
        let names: Vec<_> = RemotesFile::load(&path)
            .unwrap()
            .remotes
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn an_edit_refuses_a_file_this_build_cannot_read_and_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let original = "[[remotes]]\nname = \"a\"\n";
        let path = registry(&dir, original);
        assert!(matches!(
            add(&path, entry("b", "b.example")),
            Err(AddDeckError::Config(RemoteConfigError::Parse { .. }))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn an_edit_that_changes_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let original = "# hand-written\n[[remotes]]\nname = \"a\"\ntype = \"ssh\"\nhost = \"a\"\nport = 22\nversion = \"1.0.0\"\nadded_at = \"x\"\n";
        let path = registry(&dir, original);
        assert!(update(&path, DeckRef::Name("a"), |_| {}).unwrap().is_some());
        assert!(
            update(&path, DeckRef::Name("missing"), |_| {})
                .unwrap()
                .is_none()
        );
        assert!(remove(&path, DeckRef::Name("missing")).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        // And a no-op against a missing file does not create one.
        let absent = dir.path().join("absent.toml");
        assert!(remove(&absent, DeckRef::Name("a")).unwrap().is_none());
        assert!(!absent.exists());
    }

    #[test]
    fn add_refuses_a_duplicate_name_or_id_and_an_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, desktop_entry("d")).unwrap();
        assert!(matches!(
            add(&path, entry("d", "other")),
            Err(AddDeckError::DuplicateName { .. })
        ));
        assert!(matches!(
            add(&path, desktop_entry("e")),
            Err(AddDeckError::DuplicateId { .. })
        ));
        assert!(matches!(
            add(&path, entry("has space", "h")),
            Err(AddDeckError::InvalidName(DeckNameError::ForbiddenByte {
                offset: 3,
                ..
            }))
        ));
    }

    /// Every address form `remote add` has taken and ssh reaches, and every
    /// form the desktop writes.
    #[test]
    fn deck_addresses_accept_every_form_remote_add_and_the_desktop_write() {
        for host in [
            "host",
            "build-box.example.com",
            "ssh_alias",
            "user@host",
            "ops@prod.example.com",
            // A UPN login folded into `host`: OpenSSH splits on the last `@`.
            "dev@REALM@host",
            "10.0.0.1",
            "[2001:db8::1]",
            "[fe80::1%eth0]",
            // Unbracketed IPv6: `remote add` hands it to ssh as a destination.
            "::1",
            "user@2001:db8::1",
            "fe80::1%eth0",
        ] {
            validate_host_field(host).unwrap_or_else(|error| panic!("{host:?}: {error}"));
            validate_deck_address(&entry("d", host))
                .unwrap_or_else(|error| panic!("{host:?}: {error}"));
        }
        for key in [
            "/home/me/.ssh/id_ed25519",
            "~/.ssh/id_ed25519",
            "./id_ed25519",
            "keys/id_rsa",
            "/home/me/my keys/id",
        ] {
            let row = RemoteEntry {
                key: Some(key.to_string()),
                ..entry("d", "host")
            };
            validate_deck_address(&row).unwrap_or_else(|error| panic!("{key:?}: {error}"));
        }
        validate_deck_address(&desktop_entry("d")).unwrap();
        validate_deck_address(&RemoteEntry {
            port: 65535,
            ..entry("d", "host")
        })
        .unwrap();
        validate_ssh_target("user@host", 2222, Some("./id")).unwrap();
    }

    /// What ssh would read as an option, or what a `ProxyCommand` would hand a
    /// shell, is refused in every field — by `add`, and by a batch edit that
    /// pushes or replaces a row directly — and the file is left alone.
    #[test]
    fn deck_addresses_refuse_what_ssh_or_a_shell_would_misread() {
        let unsafe_rows: Vec<(&str, RemoteEntry)> = vec![
            (
                "leading dash host",
                entry("d", "-oProxyCommand=touch /tmp/x"),
            ),
            ("leading dash login", entry("d", "-oProxyCommand=x@host")),
            ("space in host", entry("d", "host name")),
            ("tab in host", entry("d", "host\tname")),
            ("newline in host", entry("d", "host\n-oX")),
            ("NUL in host", entry("d", "host\0")),
            ("semicolon", entry("d", "host;rm")),
            ("command substitution", entry("d", "$(id)")),
            ("backtick", entry("d", "user`id`@host")),
            ("pipe", entry("d", "host|nc")),
            ("non-ASCII", entry("d", "hóst")),
            ("empty host", entry("d", "")),
            ("empty login", entry("d", "@host")),
            ("empty host after login", entry("d", "user@")),
            ("not an IPv6 literal", entry("d", "not:ipv6")),
            ("empty zone", entry("d", "fe80::1%")),
            (
                "port zero",
                RemoteEntry {
                    port: 0,
                    ..entry("d", "host")
                },
            ),
            (
                "leading dash key",
                RemoteEntry {
                    key: Some("-oProxyCommand=x".to_string()),
                    ..entry("d", "host")
                },
            ),
            (
                "control byte in key",
                RemoteEntry {
                    key: Some("/k\u{1b}[2J".to_string()),
                    ..entry("d", "host")
                },
            ),
            (
                "empty key",
                RemoteEntry {
                    key: Some(String::new()),
                    ..entry("d", "host")
                },
            ),
            (
                "shell metacharacter in user",
                RemoteEntry {
                    user: Some("dev$(id)".to_string()),
                    ..entry("d", "host")
                },
            ),
            (
                "leading dash user",
                RemoteEntry {
                    user: Some("-l".to_string()),
                    ..entry("d", "host")
                },
            ),
            (
                "space in jump host",
                RemoteEntry {
                    jump_host: Some("bastion -oX".to_string()),
                    ..entry("d", "host")
                },
            ),
            (
                "leading dash jump host",
                RemoteEntry {
                    jump_host: Some("-J".to_string()),
                    ..entry("d", "host")
                },
            ),
            (
                "relative socket",
                RemoteEntry {
                    socket: Some("run/deck.sock".to_string()),
                    ..entry("d", "host")
                },
            ),
            (
                "colon in socket",
                RemoteEntry {
                    socket: Some("/run/a:b.sock".to_string()),
                    ..entry("d", "host")
                },
            ),
        ];

        let dir = tempfile::tempdir().unwrap();
        let path = registry(&dir, "");
        add(&path, entry("ok", "host")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        for (label, row) in unsafe_rows {
            assert!(validate_deck_address(&row).is_err(), "{label}: accepted");
            assert!(
                matches!(
                    add(&path, row.clone()),
                    Err(AddDeckError::InvalidAddress(_))
                ),
                "{label}: add did not refuse it as an address"
            );
            let pushed = edit(&path, |document| document.push(&row));
            assert!(
                matches!(pushed, Err(RemoteConfigError::InvalidAddress { .. })),
                "{label}: a batch push was not refused: {pushed:?}"
            );
            let replaced = edit(&path, |document| {
                let renamed = RemoteEntry {
                    name: "ok".to_string(),
                    ..row.clone()
                };
                document.replace(0, &renamed)
            });
            assert!(
                matches!(replaced, Err(RemoteConfigError::InvalidAddress { .. })),
                "{label}: a batch replace was not refused: {replaced:?}"
            );
            assert!(
                update(&path, DeckRef::Name("ok"), |entry| *entry = RemoteEntry {
                    name: "ok".to_string(),
                    ..row.clone()
                })
                .is_err(),
                "{label}: update accepted it"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                before,
                "{label}: a refused write touched the file"
            );
        }
    }

    /// The address rule is a write rule. A row written before it — `remote
    /// add` stored whatever target it was given — still loads, and an edit
    /// that leaves its address alone (`connect` recording `last_connected`)
    /// still lands; an edit that writes a new unsafe value does not.
    #[test]
    fn a_row_with_an_unsafe_address_still_loads_and_takes_edits_that_leave_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            r#"
[[remotes]]
name = "legacy"
type = "ssh"
host = "-oProxyCommand=x"
port = 22
key = "-k"
version = "0.30.0"
added_at = "2026-01-01T00:00:00+00:00"
"#,
        );
        let loaded = RemotesFile::load(&path).unwrap();
        assert_eq!(loaded.remotes[0].host, "-oProxyCommand=x");

        let updated = update(&path, DeckRef::Name("legacy"), |entry| {
            entry.last_connected = Some("2026-09-28T00:00:00+00:00".to_string());
        })
        .unwrap()
        .expect("the row exists");
        assert_eq!(updated.host, "-oProxyCommand=x");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("last_connected")
        );

        for (label, change) in [
            (
                "another unsafe host",
                (|entry: &mut RemoteEntry| entry.host = "-oOther".to_string())
                    as fn(&mut RemoteEntry),
            ),
            ("an unsafe jump host", |entry: &mut RemoteEntry| {
                entry.jump_host = Some("a b".to_string())
            }),
            ("another unsafe key", |entry: &mut RemoteEntry| {
                entry.key = Some("-other".to_string())
            }),
        ] {
            assert!(
                matches!(
                    update(&path, DeckRef::Name("legacy"), change),
                    Err(RemoteConfigError::InvalidAddress { .. })
                ),
                "{label}: accepted"
            );
        }

        // Fixing the address is an ordinary edit.
        update(&path, DeckRef::Name("legacy"), |entry| {
            entry.host = "host".to_string();
            entry.key = None;
        })
        .unwrap();
        assert_eq!(RemotesFile::load(&path).unwrap().remotes[0].host, "host");
    }

    #[test]
    fn existing_non_slug_names_still_load_and_can_be_edited_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "[[remotes]]\nname = \"My Box (prod)\"\ntype = \"ssh\"\nhost = \"h\"\nport = 22\n\
             version = \"0.30.0\"\nadded_at = \"2025-01-01T00:00:00Z\"\n",
        );
        let loaded = RemotesFile::load(&path).unwrap();
        assert_eq!(loaded.remotes[0].name, "My Box (prod)");
        let id = deck_id(&loaded.remotes[0]);
        assert!(is_usable_deck_id(&id), "{id}");
        update(&path, DeckRef::Id(&id), |row| row.port = 2200)
            .unwrap()
            .unwrap();
        assert_eq!(RemotesFile::load(&path).unwrap().remotes[0].port, 2200);
        remove(&path, DeckRef::Name("My Box (prod)"))
            .unwrap()
            .unwrap();
        assert!(RemotesFile::load(&path).unwrap().remotes.is_empty());
    }

    #[test]
    fn deck_names_are_slugs() {
        for good in [
            "prod",
            "Prod-1",
            "build.example.com",
            "a_b",
            "9",
            &"x".repeat(64),
        ] {
            assert_eq!(validate_deck_name(good), Ok(()), "{good}");
        }
        assert_eq!(validate_deck_name(""), Err(DeckNameError::Empty));
        assert_eq!(
            validate_deck_name(&"x".repeat(65)),
            Err(DeckNameError::TooLong { len: 65 })
        );
        assert_eq!(validate_deck_name("-x"), Err(DeckNameError::BadStart));
        assert_eq!(validate_deck_name(".x"), Err(DeckNameError::BadStart));
        for bad in ["a b", "a/b", "a@b", "é", "a\u{202e}b", "a;b", "a\nb"] {
            assert!(
                matches!(
                    validate_deck_name(bad),
                    Err(DeckNameError::ForbiddenByte { .. })
                ),
                "{bad:?}"
            );
        }
    }

    /// Issue #1426's review: the digest tells two refused names apart, matches
    /// the hash a derived `h-…` id is made of, and nothing but 16 lowercase
    /// hex digits deserializes as one.
    #[test]
    fn a_name_digest_tells_names_apart_and_carries_no_text() {
        let digest = NameDigest::of("my deck");
        assert_ne!(digest, NameDigest::of("other deck"));
        assert_eq!(format!("h-{}", digest.as_str()), derived_id("my deck"));
        assert_eq!(NameDigest::parse(digest.as_str()), Some(digest.clone()));
        for bad in [
            "",
            "my deck",
            "0123456789ABCDEF",
            "0123456789abcdef0",
            "0123456789abcdeg",
        ] {
            assert_eq!(NameDigest::parse(bad), None, "{bad:?}");
        }
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(serde_json::from_str::<NameDigest>(&json).unwrap(), digest);
        assert!(serde_json::from_str::<NameDigest>("\"my deck\"").is_err());
    }

    #[test]
    fn a_derived_name_is_valid_and_unique() {
        let none = |_: &str| false;
        assert_eq!(
            derive_deck_name("Build.Example.com", None, none),
            "build.example.com"
        );
        assert_eq!(derive_deck_name("[2001:db8::1]", None, none), "2001-db8-1");
        assert_eq!(derive_deck_name("@@@", None, none), "deck");

        let taken = ["box", "dev-box", "box-2"];
        let is_taken = |name: &str| taken.contains(&name);
        assert_eq!(derive_deck_name("box", None, is_taken), "box-3");
        assert_eq!(derive_deck_name("box", Some("ops"), is_taken), "ops-box");
        assert_eq!(derive_deck_name("box", Some("dev"), is_taken), "box-3");

        let long = "h".repeat(80);
        let full = "h".repeat(64);
        let derived = derive_deck_name(&long, None, |name| name == full);
        assert_eq!(validate_deck_name(&derived), Ok(()));
        assert!(derived.ends_with("-2"), "{derived}");
        for host in ["a.b", "UPPER", "x--y", "ünï.côdé", "-lead", "trail-"] {
            let name = derive_deck_name(host, Some("u@realm"), none);
            assert_eq!(validate_deck_name(&name), Ok(()), "{host} → {name}");
        }
    }

    #[test]
    fn a_row_without_an_id_has_a_stable_derived_one() {
        let row = entry("prod", "h");
        assert_eq!(deck_id(&row), "n-prod");
        let odd = entry("My Box", "h");
        let id = deck_id(&odd);
        assert!(id.starts_with("h-") && id.len() == 18, "{id}");
        assert_eq!(deck_id(&odd), id, "deterministic");
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325, "FNV-1a offset basis");

        // A usable id is used as written; a reserved or malformed one is not.
        let with = |id: &str| RemoteEntry {
            id: Some(id.to_string()),
            ..entry("prod", "h")
        };
        assert_eq!(deck_id(&with("abc123")), "abc123");
        assert_eq!(deck_id(&with("LOCAL")), "n-prod");
        assert_eq!(deck_id(&with("all")), "n-prod");
        assert_eq!(deck_id(&with("has space")), "n-prod");
    }

    #[test]
    fn the_login_comes_from_the_user_field_or_the_host() {
        let folded = entry("a", "me@h.example");
        assert_eq!(login_and_host(&folded), (Some("me"), "h.example"));
        let explicit = RemoteEntry {
            user: Some("me@REALM".to_string()),
            ..entry("a", "h.example")
        };
        assert_eq!(login_and_host(&explicit), (Some("me@REALM"), "h.example"));
        assert_eq!(host_fields("h", Some("me")), ("me@h".to_string(), None));
        assert_eq!(
            host_fields("h", Some("me@REALM")),
            ("h".to_string(), Some("me@REALM".to_string()))
        );
        assert_eq!(host_fields("h", None), ("h".to_string(), None));
        assert_eq!(
            address_key(&entry("a", "me@H.Example")),
            ("h.example".to_string(), Some("me".to_string()), 22)
        );
    }

    /// Issue #1350's review: an edit waits for another holder of the lock and
    /// then lands on the file as that holder left it. The test takes the lock
    /// on its own handle — a second, independent acquisition, which is what
    /// another process's is — writes a row while holding it, and only then lets
    /// the blocked `add` through: the row survives.
    #[test]
    fn an_edit_waits_for_the_lock_and_keeps_what_the_holder_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(&dir, "");
        let held = acquire_edit_lock(&path)
            .unwrap()
            .expect("tempdirs can lock");

        let (tx, rx) = std::sync::mpsc::channel();
        let editor_path = path.clone();
        let editor = std::thread::spawn(move || {
            let result = add(&editor_path, entry("from-the-waiter", "w.example"));
            let _ = tx.send(());
            result
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the edit went ahead while another handle held the lock"
        );
        // The holder's own edit, published while the waiter is blocked.
        let holder_row = toml::to_string(&RemotesFile {
            remotes: vec![entry("from-the-holder", "h.example")],
        })
        .unwrap();
        write_atomic(&path, &holder_row).unwrap();
        drop(held);

        editor.join().unwrap().unwrap();
        let names: Vec<_> = RemotesFile::load(&path)
            .unwrap()
            .remotes
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["from-the-holder", "from-the-waiter"]);
    }

    /// A lock sidecar whose name is taken by something that is not a regular
    /// file stops the edit before it reads or writes anything.
    #[test]
    fn an_unusable_lock_file_refuses_the_edit_and_leaves_the_registry_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(&dir, "");
        add(&path, entry("a", "a.example")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let lock = edit_lock_path(&path);
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        assert!(matches!(
            add(&path, entry("b", "b.example")),
            Err(AddDeckError::Config(RemoteConfigError::Locked { .. }))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// Where the concurrent-editors test's child process writes its rows.
    const CHILD_EDITOR_PATH_ENV: &str = "DAD_DECK_LIST_TEST_CHILD_EDITOR_PATH";
    const EDITS_PER_WRITER: usize = 40;

    /// The child half of
    /// [`two_processes_adding_at_once_lose_no_row`]; a no-op unless that test
    /// re-executed this binary with [`CHILD_EDITOR_PATH_ENV`] set.
    #[test]
    fn concurrent_editor_child_process() {
        let Some(path) = std::env::var_os(CHILD_EDITOR_PATH_ENV) else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        for n in 0..EDITS_PER_WRITER {
            add(&path, entry(&format!("child-{n}"), "c.example")).unwrap();
        }
    }

    /// Issue #1350's review, the reported scenario itself: the CLI and the
    /// desktop are separate processes, so the in-process mutex never saw one
    /// another, and two read-modify-writes of the same original let the later
    /// rename discard the earlier's row. This re-executes the test binary as a
    /// second writer and races it: every row from both survives.
    #[test]
    fn two_processes_adding_at_once_lose_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(&dir, "");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "deck_list::tests::concurrent_editor_child_process",
                "--nocapture",
            ])
            .env(CHILD_EDITOR_PATH_ENV, &path)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for n in 0..EDITS_PER_WRITER {
            add(&path, entry(&format!("parent-{n}"), "p.example")).unwrap();
        }
        assert!(child.wait().unwrap().success(), "the child writer failed");

        let names: Vec<String> = RemotesFile::load(&path)
            .unwrap()
            .remotes
            .into_iter()
            .map(|row| row.name)
            .collect();
        let lost: Vec<String> = ["parent", "child"]
            .into_iter()
            .flat_map(|writer| (0..EDITS_PER_WRITER).map(move |n| format!("{writer}-{n}")))
            .filter(|name| !names.contains(name))
            .collect();
        assert!(lost.is_empty(), "rows were lost: {lost:?}");
        assert_eq!(names.len(), 2 * EDITS_PER_WRITER, "{names:?}");
    }

    /// Issue #1350's review: the registry is read on the desktop's startup and
    /// snapshot paths from a path `DOT_AGENT_DECK_REMOTES` can move, so a FIFO
    /// there must be refused rather than block, and the refusal must be the
    /// ordinary read error — from the loader and from an edit alike.
    #[cfg(unix)]
    #[test]
    fn a_registry_path_that_is_a_fifo_is_a_read_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `mkfifo` only reads through it.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let (tx, rx) = std::sync::mpsc::channel();
        let probe = path.clone();
        std::thread::spawn(move || {
            let loaded = RemotesFile::load(&probe).map(|_| ());
            let edited = remove(&probe, DeckRef::Name("a")).map(|_| ());
            let _ = tx.send((loaded, edited));
        });
        let (loaded, edited) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a FIFO must return promptly, not block");
        for (what, result) in [("load", loaded), ("edit", edited)] {
            match result {
                Err(RemoteConfigError::Io { source, .. }) => assert!(
                    source.to_string().contains("a FIFO, not a regular file"),
                    "{what}: {source}"
                ),
                other => panic!("{what}: expected a read error, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_oversized_registry_is_a_read_error_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let at_limit = format!("{}\n", "#".repeat(MAX_REGISTRY_BYTES as usize - 1));
        let path = registry(&dir, &at_limit);
        assert!(
            RemotesFile::load(&path).unwrap().remotes.is_empty(),
            "the limit is a limit"
        );

        let over = format!("{at_limit}\n");
        std::fs::write(&path, &over).unwrap();
        for result in [
            RemotesFile::load(&path).map(|_| ()),
            add(&path, entry("a", "a.example"))
                .map(|_| ())
                .map_err(|error| match error {
                    AddDeckError::Config(error) => error,
                    other => panic!("expected a read error, got {other:?}"),
                }),
        ] {
            match result {
                Err(RemoteConfigError::Io { source, .. }) => assert!(
                    source
                        .to_string()
                        .contains(&format!("larger than the {MAX_REGISTRY_BYTES}-byte limit")),
                    "{source}"
                ),
                other => panic!("expected a read error, got {other:?}"),
            }
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), over);
    }

    /// A symlinked `remotes.toml` (a dotfile manager's) is followed, and its
    /// target judged: a link to a regular file loads.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_registry_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = registry(&dir, "");
        add(&real, entry("a", "a.example")).unwrap();
        let link = dir.path().join("linked.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(RemotesFile::load(&link).unwrap().remotes.len(), 1);
    }

    /// Issue #1350's review: an edit through a symlinked registry lands at the
    /// link's target and leaves the link a link. The rename used to replace
    /// the link itself with a plain file, detaching the registry from the
    /// dotfile manager's copy. The link here sits in another directory than
    /// its target, so the lock and temp file must follow the target there.
    #[cfg(unix)]
    #[test]
    fn an_edit_through_a_symlinked_registry_updates_the_target_and_keeps_the_link() {
        let dotfiles = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let target = registry(&dotfiles, "");
        add(&target, entry("a", "a.example")).unwrap();
        let link = config.path().join("remotes.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        add(&link, entry("b", "b.example")).unwrap();
        update(&link, DeckRef::Name("a"), |row| row.port = 2200).unwrap();
        remove(&link, DeckRef::Name("b")).unwrap();
        RemotesFile::load(&link)
            .unwrap()
            .save(&link)
            .expect("a whole-file save follows the link too");

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by a plain file"
        );
        let rows = RemotesFile::load(&target).unwrap().remotes;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a");
        assert_eq!(rows[0].port, 2200, "the edit did not reach the target");
        let stray: Vec<_> = std::fs::read_dir(config.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "remotes.toml")
            .collect();
        assert!(
            stray.is_empty(),
            "lock or temp file beside the link: {stray:?}"
        );
    }

    /// The lock is one lock whichever path a writer came in by: an edit through
    /// the link waits while the lock taken through the target is held.
    #[cfg(unix)]
    #[test]
    fn an_edit_through_a_link_and_one_at_its_target_share_one_lock() {
        let dotfiles = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let target = registry(&dotfiles, "");
        let link = config.path().join("remotes.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let held = acquire_edit_lock(&target)
            .unwrap()
            .expect("tempdirs can lock");

        let (tx, rx) = std::sync::mpsc::channel();
        let editor = std::thread::spawn(move || {
            let result = add(&link, entry("through-the-link", "l.example"));
            let _ = tx.send(());
            result
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the edit through the link went ahead while the target's lock was held"
        );
        drop(held);
        editor.join().unwrap().unwrap();
        assert_eq!(RemotesFile::load(&target).unwrap().remotes.len(), 1);
    }

    /// A dangling symlink at the registry path is refused on write — its
    /// target, and the directories on the way to it, are not created — and
    /// still reads as an empty registry.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlinked_registry_is_refused_on_write_and_nothing_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-yet").join("remotes.toml");
        let link = dir.path().join("remotes.toml");
        std::os::unix::fs::symlink(&missing, &link).unwrap();

        assert!(RemotesFile::load(&link).unwrap().remotes.is_empty());
        assert!(matches!(
            add(&link, entry("a", "a.example")),
            Err(AddDeckError::Config(RemoteConfigError::Unwritable { .. }))
        ));
        assert!(matches!(
            write_atomic(&link, "remotes = []\n"),
            Err(RemoteConfigError::Unwritable { .. })
        ));
        assert!(!dir.path().join("not-yet").exists());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// Issue #1350's review: parsing split a folded login at the first `@` and
    /// validation at the last, so a `dev@REALM@host` deck `remote add`
    /// accepted was read as the host `REALM@host`. Every reader now splits
    /// where ssh does.
    #[test]
    fn a_folded_upn_login_is_split_at_the_last_at_sign_everywhere() {
        assert_eq!(split_login("dev@REALM@host"), (Some("dev@REALM"), "host"));
        assert_eq!(split_login("me@host"), (Some("me"), "host"));
        assert_eq!(split_login("host"), (None, "host"));
        let folded = entry("upn", "dev@REALM@build.example.com");
        assert_eq!(
            login_and_host(&folded),
            (Some("dev@REALM"), "build.example.com")
        );
        validate_host_field(&folded.host).unwrap();
        assert_eq!(
            address_key(&folded),
            (
                "build.example.com".to_string(),
                Some("dev@REALM".to_string()),
                22
            )
        );
        let target = crate::remote::SshTarget::parse("dev@REALM@build.example.com", 22, None);
        assert_eq!(target.user.as_deref(), Some("dev@REALM"));
        assert_eq!(target.host, "build.example.com");
        assert_eq!(target.user_host(), "dev@REALM@build.example.com");
    }

    /// Issue #1350's review: [`edit`] resolves the registry path once and
    /// publishes through [`write_resolved`], which must not resolve it again.
    /// Here the resolved path's leaf is swapped for a symlink after resolution
    /// — the state a mid-edit swap leaves — and the write lands on the path it
    /// was given, replacing the link, instead of following it to a file the
    /// edit never locked or read.
    #[cfg(unix)]
    #[test]
    fn a_write_at_a_resolved_path_does_not_follow_a_link_swapped_in_after_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(&dir, "remotes = []\n");
        let resolved = publish_path(&path).unwrap();
        assert_eq!(resolved, path, "a plain file resolves to itself");
        let victim = dir.path().join("victim.toml");
        std::fs::write(&victim, "untouched").unwrap();
        std::fs::remove_file(&resolved).unwrap();
        std::os::unix::fs::symlink(&victim, &resolved).unwrap();

        write_resolved(&resolved, "remotes = []\n# edited\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "untouched",
            "the write followed a link swapped in after resolution"
        );
        assert!(
            !std::fs::symlink_metadata(&resolved)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "remotes = []\n# edited\n"
        );
    }

    /// S1 of #1350's review: the temp name is predictable, so a symlink
    /// planted there must never be followed — neither one pointing at a file
    /// (which a plain `O_CREAT|O_TRUNC` would truncate and overwrite) nor a
    /// dangling one (which it would create). The save draws past the taken
    /// names, lands its bytes in `remotes.toml`, and leaves both targets alone.
    ///
    /// The counter is process-global and other tests save concurrently, so the
    /// symlinks cover the next five draws rather than exactly one: however many
    /// of those a sibling consumes, at most five of this save's eight draws hit
    /// a planted name, and any draw that does must not follow it.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_temp_name_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        let dangling = dir.path().join("created-through-a-symlink");
        let next = TEMP_COUNTER.load(Ordering::Relaxed);
        for (offset, target) in [&victim, &dangling, &victim, &dangling, &victim]
            .into_iter()
            .enumerate()
        {
            let name = format!(
                "remotes.toml.{}.{}.tmp",
                std::process::id(),
                next + offset as u64
            );
            std::os::unix::fs::symlink(target, dir.path().join(name)).unwrap();
        }

        write_atomic(&path, "remotes = []\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "remotes = []\n");
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(!dangling.exists(), "a dangling symlink was written through");
    }

    // -- rename (issue #1426) -----------------------------------------------

    #[test]
    fn a_rename_is_visible_on_a_fresh_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, desktop_entry("build")).unwrap();

        let renamed = rename(&path, DeckRef::Id("0123456789abcdef"), "build-box", |_| {
            true
        })
        .unwrap();
        assert_eq!(renamed.name, "build-box");

        let rows = RemotesFile::load(&path).unwrap().remotes;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "build-box");
        assert_eq!(rows[0].id.as_deref(), Some("0123456789abcdef"));
        assert_eq!(rows[0], renamed, "the row returned is the row on disk");
    }

    #[test]
    fn a_rename_to_an_invalid_name_is_refused_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("prod", "prod.example.com")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        for bad in [
            "",
            "-prod",
            "my deck",
            "prod/1",
            &"a".repeat(MAX_DECK_NAME_BYTES + 1),
        ] {
            let error = rename(&path, DeckRef::Name("prod"), bad, |_| true).unwrap_err();
            assert!(
                matches!(error, RenameDeckError::InvalidName(_)),
                "{bad:?}: {error:?}"
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_rename_to_another_rows_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("prod", "prod.example.com")).unwrap();
        add(&path, entry("staging", "staging.example.com")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let error = rename(&path, DeckRef::Name("prod"), "staging", |_| true).unwrap_err();
        assert!(
            matches!(&error, RenameDeckError::DuplicateName { name } if name == "staging"),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "A deck named 'staging' already exists.");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_rename_of_a_row_that_is_not_there_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("prod", "prod.example.com")).unwrap();

        let error = rename(&path, DeckRef::Id("n-gone"), "fresh", |_| true).unwrap_err();
        assert!(matches!(error, RenameDeckError::NotFound), "{error:?}");
    }

    #[test]
    fn a_rename_to_the_current_name_succeeds_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "[[remotes]]\n\
             name = \"prod\"\n\
             type = \"ssh\"\n\
             host = \"prod.example.com\"\n\
             port = 22\n\
             version = \"0.40.0\"\n\
             added_at = \"2026-01-01T00:00:00Z\"\n",
        );
        let before = std::fs::read_to_string(&path).unwrap();

        let same = rename(&path, DeckRef::Name("prod"), "prod", |_| true).unwrap();
        assert_eq!(same.name, "prod");
        assert_eq!(same.id, None, "a no-op stores no id either");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// Scenario: `remote add prod` writes a row with no `id`, so the desktop
    /// knows it as `n-prod`, derived from the name. Renaming it must not move
    /// that: the rename stores the old derived id in the row, and the row
    /// answers to the same id under its new name.
    #[test]
    fn a_rename_of_a_row_with_no_id_keeps_the_id_it_was_known_by() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("prod", "prod.example.com")).unwrap();
        let before = RemotesFile::load(&path).unwrap().remotes;
        assert_eq!(before[0].id, None, "the CLI writes no id");
        let key = deck_id(&before[0]);
        assert_eq!(key, "n-prod");

        let renamed = rename(&path, DeckRef::Id(&key), "production", |_| true).unwrap();

        assert_eq!(renamed.name, "production");
        assert_eq!(renamed.id.as_deref(), Some("n-prod"));
        let after = RemotesFile::load(&path).unwrap().remotes;
        assert_eq!(deck_id(&after[0]), key, "the key survives a fresh read");
        assert!(DeckRef::Id(&key).matches(&after[0]));
        assert!(DeckRef::Name("production").matches(&after[0]));

        // A name that does not fit the id charset derives a hashed id, and
        // that is the one kept.
        add(&path, entry("db.internal", "db.example.com")).unwrap();
        let hashed = deck_id(&RemotesFile::load(&path).unwrap().remotes[1]);
        assert!(hashed.starts_with("h-"), "{hashed}");
        let renamed = rename(&path, DeckRef::Name("db.internal"), "db", |_| true).unwrap();
        assert_eq!(deck_id(&renamed), hashed);
    }

    /// Scenario: `remote add prod` writes a row with no `id` (known as
    /// `n-prod`); `remote remove prod` and `remote add prod` at another host
    /// hand that id to a different deck. A rename by `n-prod` from a caller
    /// that read the first deck is refused, and nothing is written.
    #[test]
    fn a_rename_of_a_row_the_caller_does_not_expect_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("prod", "a.example.com")).unwrap();
        let read = RemotesFile::load(&path).unwrap().remotes[0].clone();
        remove(&path, DeckRef::Name("prod")).unwrap();
        add(&path, entry("prod", "b.example.com")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            deck_id(&RemotesFile::load(&path).unwrap().remotes[0]),
            "n-prod"
        );

        let mut seen = None;
        let error = rename(&path, DeckRef::Id("n-prod"), "production", |row| {
            seen = Some(row.host.clone());
            address_key(row) == address_key(&read)
        })
        .unwrap_err();

        assert!(matches!(error, RenameDeckError::Changed), "{error:?}");
        assert_eq!(
            seen.as_deref(),
            Some("b.example.com"),
            "shown the row on disk"
        );
        assert!(
            !error
                .to_string()
                .contains(&dir.path().display().to_string())
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // The check runs before the no-op: a same-name rename of a row that is
        // not the expected one is refused too, rather than reported as done.
        let error = rename(&path, DeckRef::Id("n-prod"), "prod", |_| false).unwrap_err();
        assert!(matches!(error, RenameDeckError::Changed), "{error:?}");
    }

    /// Scenario: a row written before the naming rule carries a name the rule
    /// refuses (`my deck`, with a space), so it answers to a hashed `h-…` id.
    /// Renaming it by that id to a valid name works, and the row keeps the id.
    #[test]
    fn a_row_whose_stored_name_breaks_the_rule_can_be_renamed_by_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "[[remotes]]\n\
             name = \"my deck\"\n\
             type = \"ssh\"\n\
             host = \"legacy.example.com\"\n\
             port = 22\n\
             version = \"0.40.0\"\n\
             added_at = \"2026-01-01T00:00:00Z\"\n",
        );
        let before = RemotesFile::load(&path).unwrap().remotes[0].clone();
        assert!(validate_deck_name(&before.name).is_err());
        let key = deck_id(&before);
        assert!(key.starts_with("h-"), "{key}");

        let renamed = rename(&path, DeckRef::Id(&key), "legacy", |_| true).unwrap();

        assert_eq!(renamed.name, "legacy");
        let after = RemotesFile::load(&path).unwrap().remotes;
        assert_eq!(after[0].name, "legacy");
        assert_eq!(
            deck_id(&after[0]),
            key,
            "the row keeps the id it was known by"
        );
    }

    #[test]
    fn a_rename_keeps_unknown_keys_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "# my decks\n\
             future = \"top-level\"\n\n\
             [[remotes]]\n\
             # the build machine\n\
             name = \"a\" # its old name\n\
             type = \"ssh\"\n\
             host = \"a.example\"\n\
             port = 22\n\
             version = \"0.40.0\"\n\
             added_at = \"2026-01-01T00:00:00Z\"\n\
             jump_host = \"bastion\"\n\
             colour = \"green\" # a field from a newer build\n",
        );

        rename(&path, DeckRef::Name("a"), "build", |_| true).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        for kept in [
            "# my decks",
            "future = \"top-level\"",
            "# the build machine",
            "jump_host = \"bastion\"",
            "colour = \"green\" # a field from a newer build",
            "added_at = \"2026-01-01T00:00:00Z\"",
            "name = \"build\"",
            "id = \"n-a\"",
        ] {
            assert!(written.contains(kept), "{kept:?} missing from:\n{written}");
        }
        let old: pre_1350::RemotesFile = toml::from_str(&written).expect("old CLI parses");
        assert_eq!(old.remotes[0].name, "build");
    }

    /// Scenario: after `prod` is renamed to `production` the row keeps the id
    /// `n-prod`. A later `remote add prod` derives that same id; it gets a
    /// fresh stored one instead of being refused, and both rows stay distinct.
    #[test]
    fn adding_the_old_name_after_a_rename_takes_a_fresh_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("prod", "prod.example.com")).unwrap();
        rename(&path, DeckRef::Name("prod"), "production", |_| true).unwrap();

        let added = add(&path, entry("prod", "new-prod.example.com")).unwrap();

        assert_eq!(added.id.as_deref(), Some("n-prod-2"));
        let rows = RemotesFile::load(&path).unwrap().remotes;
        let ids: Vec<String> = rows.iter().map(deck_id).collect();
        assert_eq!(ids, ["n-prod", "n-prod-2"]);

        // An id the caller chose is never replaced.
        let mut clash = entry("other", "other.example.com");
        clash.id = Some("n-prod".to_string());
        assert!(matches!(
            add(&path, clash).unwrap_err(),
            AddDeckError::DuplicateId { id } if id == "n-prod"
        ));
    }

    #[test]
    fn a_deck_name_value_accepts_exactly_what_the_rule_accepts() {
        assert_eq!(
            DeckName::parse("build-box.1").unwrap().as_str(),
            "build-box.1"
        );
        assert!(matches!(
            DeckName::parse("-x"),
            Err(DeckNameError::BadStart)
        ));

        #[derive(serde::Deserialize)]
        struct Row {
            name: DeckName,
        }
        assert_eq!(
            toml::from_str::<Row>("name = \"prod\"")
                .unwrap()
                .name
                .as_str(),
            "prod"
        );
        assert!(toml::from_str::<Row>("name = \"my deck\"").is_err());
        assert_eq!(
            serde_json::to_string(&DeckName::parse("prod").unwrap()).unwrap(),
            "\"prod\""
        );
    }
}
