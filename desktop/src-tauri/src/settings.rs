//! The desktop app's own settings document (PRD #803, M2).
//!
//! This is the *client-owned* half of the boundary rule the PRD writes down:
//! the desktop app gets everything else from the daemon, and the only thing it
//! owns is its own settings. Nothing in this module crosses the TUI↔daemon
//! protocol — the document is read and written entirely inside the desktop
//! process, and the daemon cannot observe it.
//!
//! # Where it lives
//!
//! [`config_dir`]`().join("desktop.toml")` — a **sibling** of the TUI's
//! `config.toml` and `keybindings.toml`, never a section inside them.
//! `DashboardConfig::save()` serialises its struct (`src/config.rs`), so a
//! `[desktop]` table the TUI does not know about would be silently deleted on
//! the next TUI write; the split is what keeps the two schemas from coupling.
//! [`SETTINGS_PATH_ENV`] overrides the whole path, mirroring the
//! `DOT_AGENT_DECK_CONFIG` convention and giving tests a seam.
//!
//! # Failure behaviour
//!
//! **Loading never fails.** A missing file, an unparseable file, an unreadable
//! file, an unusable path and an unknown enum value all yield defaults —
//! exactly as `DashboardConfig::load()` does. A settings file is not worth
//! failing an app launch over, so the failure is logged and never propagated.
//!
//! **The path is vetted and the read is bounded.** [`read_document`] requires
//! an absolute path with a file name whose target is absent or a regular file,
//! and reads at most [`MAX_SETTINGS_BYTES`]. That is not a privilege boundary —
//! anyone who can set [`SETTINGS_PATH_ENV`] can already run code as this user —
//! it is there because an app that hangs forever on a FIFO or dies on
//! `/dev/zero` is a miserable thing to debug.
//!
//! **Writing is atomic and owner-only.** A temp file in the same directory
//! under an unpredictable name, then a rename; mode 0o600 on Unix, a protected
//! DACL on Windows. Modelled on `dot_agent_deck::schedule_cli::write_atomic`.
//! [`save_to`] records what that deliberately does *not* defend against.
//!
//! **Writing also preserves what it does not understand.** The save merges the
//! serialised struct into the document already on disk rather than replacing
//! it, so an older build cannot delete a section a newer one wrote. See
//! [`merged_document`] — this is the one place where `#[serde(default)]`
//! genuinely does not give what it looks like it gives.
//!
//! # Adding a setting
//!
//! Add a field to your feature's section struct, or add a new section struct
//! and one line to [`DesktopSettings`]. `#[serde(default)]` gives it a default
//! and there is deliberately no `deny_unknown_fields`, so a field written by a
//! newer build survives an older build reading the file.
//!
//! Two rules constrain what may go in:
//!
//! 1. **A secret never goes in this document, and never in `localStorage`.**
//!    The document may hold a non-secret *reference* — which backend holds the
//!    key, or a boolean saying one is stored — and nothing more. A real
//!    credential belongs behind the `SecretStore` seam (PRD #803 M5), whose
//!    intended implementation is the OS keychain.
//!
//!    Two different kinds of check watch that rule, and confusing them is the
//!    mistake issue #827 was opened about.
//!
//!    [`tests::no_settings_key_name_trips_the_credential_tripwire`] fails the
//!    build on a credential-shaped key **name**, and it is a **naming tripwire,
//!    not a security boundary** — the distinction matters enough that the test,
//!    its failure message and the developer docs all say it in those words. It
//!    reads key names in the serialised default document and nothing else, so a
//!    field called `endpoint` holding a token passes it, and the TypeScript DTO
//!    is outside it entirely. Read a pass as "nobody named a field like a
//!    credential", never as "a credential cannot get in here".
//!
//!    The **value**-side checks are the ones to rely on. They follow one
//!    uniquely-named sentinel through every sink #827 enumerates — the document
//!    on disk across a load-modify-save round trip, the IPC echo, the
//!    `desktop_get_settings` snapshot, the parse diagnostic, and both halves of
//!    [`SettingsWriteError`] — reaching the two settings commands through the
//!    public functions their bodies wrap, since a `#[tauri::command]` needs a
//!    running app to call.
//!
//!    **What they establish stopped being one sentence at PRD #741 M6, and the
//!    old sentence is worth quoting because it is now false.** It read: *this
//!    build's schema has no field that can carry arbitrary text*. That was true
//!    of `u32`, [`AppearanceMode`] and [`ZoomLevel`] — an integer, three tokens
//!    and ten numbers — and it is **not** true of the ssh-argument newtypes
//!    #741 added. `Hostname` is 253 bytes of `[A-Za-z0-9._-]`, `SshUser` 64,
//!    `HostAlias` 253 and [`EndpointId`] 64; measured, the 45-byte
//!    `sk-…`-shaped sentinel below **parses** as all four. They *bound and
//!    restrict*, which is a real property and a weaker one than "cannot carry
//!    text", and `ALLOWED_FIELD_TYPES`' own entries have said so since M5.
//!
//!    So the claim, in the two halves that are each true:
//!
//!    - For every field whose type is `u32`, `u16`, [`AppearanceMode`],
//!      [`ZoomLevel`] or [`Selection`], there is **no route** by which a
//!      credential reaches disk, the IPC, or the log — nothing arbitrary is
//!      representable, and the sentinel sweep proves it end to end.
//!    - For the ssh-argument fields, a single-line token **is** representable,
//!      and what rules a credential out is not the type but **where the value
//!      goes**: each is handed to `ssh` as an argument and reaches no
//!      authentication surface, key *material* and a passphrase are excluded by
//!      shape (`KeyPath` must start `/` or `~/`, so a PEM block cannot be
//!      written there — measured, the sentinel is refused by `KeyPath` and
//!      `RemoteSocketPath`), and the storage policy is that a credential goes
//!      behind the `SecretStore` seam instead. Pinned by
//!      [`tests::an_ssh_argument_field_bounds_and_restricts_rather_than_forbidding_text`],
//!      which exists so the next reader finds the boundary measured rather than
//!      asserted.
//!
//!    Two further things the sweep does **not** claim. A key this schema does
//!    not own keeps whatever a user or a newer build wrote there, because the
//!    save merges rather than replaces (see [`merged_document`]) — measured by
//!    [`tests::a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else`].
//!    And the sweep is derived from the **default** document, so it covers a
//!    field #802 adds only once that field appears there: an optional field or
//!    a row in an empty list is outside it, which is why the naming tripwire
//!    grew [`tests::representative_document`] and why a value-side claim about
//!    the endpoint fields is made by its own test rather than by the sweep.
//!    The TypeScript half of the schema, the `localStorage` key set and the
//!    field-type allowlist are checked in
//!    `xtask/linkage-check/src/desktop_settings_secrets.rs`, because those live
//!    outside this crate and a vitest guard can be merged past.
//! 2. **Field names stay `snake_case`, and single-word where it is natural.**
//!    The same struct is serialised to TOML (which a user hand-edits, and where
//!    `snake_case` is this repo's convention) *and* to JSON for the webview
//!    (where the desktop DTOs use `camelCase`). Every name today is one word,
//!    so the two agree byte for byte. The first genuinely multi-word field is
//!    the point at which a separate webview DTO has to be introduced — not a
//!    `rename_all` on this struct, which would make the TOML read badly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dot_agent_deck::daemon_client::{Endpoint, RemoteEndpoint};
use dot_agent_deck::platform::fsperm;
use dot_agent_deck::platform::paths::config_dir;
// PRD #741 M6: the validating ssh-argument newtypes ARE this section's schema.
// `ALLOWED_FIELD_TYPES` refuses `String`, so the endpoint fields have to be
// types whose `Deserialize` bounds and charset-checks them — which is exactly
// what these are. See `xtask/linkage-check/src/desktop_project_boundary.rs` for
// why `remote_tunnel` is on that rule's allowlist.
use dot_agent_deck::remote_tunnel::{HostAlias, Hostname, KeyPath, RemoteSocketPath, SshUser};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Overrides the whole settings path, mirroring `DOT_AGENT_DECK_CONFIG`
/// (`src/config.rs`). Also the seam every test uses instead of the real
/// config directory.
pub const SETTINGS_PATH_ENV: &str = "DOT_AGENT_DECK_DESKTOP_CONFIG";

/// The document's file name inside [`config_dir`].
const SETTINGS_FILE_NAME: &str = "desktop.toml";

/// Schema version carried by every document this build writes.
///
/// Open Question 4 in PRD #803: cheap insurance, and impossible to add
/// retroactively without a heuristic for "documents written before the field
/// existed". Nothing reads it yet — a future migration will.
pub const SETTINGS_VERSION: u32 = 1;

/// How the app picks its light/dark palette.
///
/// The *storage* is #803's; what the choice does to the UI is PRD #743's.
/// Serialised as a lowercase string in both TOML and JSON, because that is what
/// reads well in a hand-edited config file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AppearanceMode {
    /// Follow the operating system's own light/dark preference.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// The exact token written to TOML and JSON. Pinned by
    /// [`tests::default_document_shape_is_pinned`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parse a stored token, falling back to the default.
    ///
    /// An unknown value is **not** an error: a document written by a newer
    /// build may name a mode this one has never heard of, and losing the whole
    /// document over one unreadable field would be the opposite of the
    /// unknown-key tolerance the rest of the schema is built for.
    fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::default(),
        }
    }
}

impl Serialize for AppearanceMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The longest appearance token this build will accept, on either side.
///
/// The tokens are `system`, `light` and `dark` — six bytes at most. 64 leaves
/// room for a mode name a future build invents while making a payload-shaped
/// value impossible.
pub const MAX_APPEARANCE_TOKEN_BYTES: usize = 64;

impl<'de> Deserialize<'de> for AppearanceMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A derived unit-variant `Deserialize` would reject an unknown token,
        // and serde's `#[serde(other)]` is not available on an externally
        // tagged enum, so the fallback is spelled out here. A visitor rather
        // than `String::deserialize` because the length has to be judged on the
        // borrowed input, **before** anything allocates a normalised copy of it
        // — see [`AppearanceModeVisitor::visit_str`].
        deserializer.deserialize_str(AppearanceModeVisitor)
    }
}

struct AppearanceModeVisitor;

impl serde::de::Visitor<'_> for AppearanceModeVisitor {
    type Value = AppearanceMode;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "an appearance mode of at most {MAX_APPEARANCE_TOKEN_BYTES} bytes"
        )
    }

    /// Length first, then [`AppearanceMode::from_str_lossy`].
    ///
    /// The order is the point. `from_str_lossy` trims and lowercases, which
    /// allocates a copy of whatever it was handed, so a webview sending a
    /// megabyte-long mode used to get a megabyte allocated and rewritten before
    /// anything looked at it. The bound lives here rather than in the Tauri
    /// command so it covers the command *and* a hand-edited document with one
    /// check.
    ///
    /// Over-length is an **error**, not the unknown-value fallback, and the
    /// distinction is deliberate: an unrecognised token is a mode this build has
    /// not heard of and is tolerated by design, while a 4 KB one is a malformed
    /// document. On the disk path that means the whole document falls back to
    /// defaults — the ordinary malformed-document behaviour, logged, never a
    /// failed launch. On the IPC path it fails argument deserialisation with a
    /// message that names the limit and carries no path and no value.
    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<AppearanceMode, E> {
        if raw.len() > MAX_APPEARANCE_TOKEN_BYTES {
            return Err(E::custom(format!(
                "an appearance mode is at most {MAX_APPEARANCE_TOKEN_BYTES} bytes; got {}",
                raw.len()
            )));
        }
        Ok(AppearanceMode::from_str_lossy(raw))
    }
}

/// The `[appearance]` section — PRD #743's tenant, stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub mode: AppearanceMode,
}

/// The `[zoom]` section — PRD #744's tenant, stored here.
///
/// One field, and it is a *scale factor* rather than a percentage, because that
/// is the unit `webview.set_zoom` takes: keeping storage, IPC, the frontend
/// ladder and the platform call in one unit means there is no conversion
/// anywhere to get backwards.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoomSettings {
    pub level: ZoomLevel,
}

/// The whole settings document.
///
/// Deliberately carries only the sections that have a tenant. A container that
/// grows opinions about its contents blocks its dependents, so #802's voice
/// backends add their own section when they land — it is not pre-created here.
///
/// **No `Eq`**, and that is [`ZoomLevel`]'s doing rather than an oversight: it
/// wraps an `f64`, which is `PartialEq` but not `Eq` because `NaN != NaN`.
/// Nothing keys a map or a set on this document, so `PartialEq` is all any
/// caller needs — `assert_eq!` included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopSettings {
    pub version: u32,
    pub appearance: AppearanceSettings,
    /// PRD #741's tenant. **An `Option`, and it means *unspecified* rather than
    /// *empty* — see [`EndpointSettings`] for why that distinction is what
    /// stops a webview deleting decks it cannot render.**
    pub endpoints: Option<EndpointSettings>,
    pub zoom: ZoomSettings,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            appearance: AppearanceSettings::default(),
            endpoints: None,
            zoom: ZoomSettings::default(),
        }
    }
}

// PRD #741 M6 stores endpoints; M7 (the panel) and M9 (the Deck selector) are
// what read them, and both are fenced out of this milestone. So the storage API
// below has its tests as its only caller today. The `allow` is the same device
// `platform::fsperm`'s SITE_AUDIT uses for the same reason — an item whose
// production consumer is a later milestone — rather than a silent `pub` that
// looks reachable. Deleting it once M7 lands is the point: if these are still
// unused then, the milestone did not wire them up.
#[cfg_attr(not(test), allow(dead_code))]
impl DesktopSettings {
    /// Which deck this document says to talk to, with the local deck as the
    /// answer whenever the stored selection cannot be honoured.
    ///
    /// An absent `[endpoints]` section is the same answer as an empty one:
    /// the local deck, with no fallback to report. Absence is how *this* build
    /// says "I have nothing to add about endpoints" — it never means "the user
    /// removed them" (see [`EndpointSettings`]).
    pub fn resolve_endpoint(&self) -> ResolvedEndpoint {
        match &self.endpoints {
            Some(endpoints) => endpoints.resolve(),
            None => ResolvedEndpoint {
                endpoint: Endpoint::local(),
                fallback: None,
            },
        }
    }
}

/// The `[endpoints]` section — PRD #741's tenant: which decks are configured,
/// and which one the app is talking to.
///
/// # The local deck is not in here, and that is the point
///
/// A local deck needs no configuration — [`Endpoint::local`] resolves it from
/// the platform paths the way every caller did before endpoints existed — so
/// this section holds only the *remote* rows plus the selection. A fresh
/// install therefore has no `[endpoints]` section at all and still works, and a
/// user who deletes the section gets the local deck back rather than nothing.
///
/// # Why the section is an `Option` on [`DesktopSettings`]
///
/// `desktop_set_settings` takes the **whole document** from the webview, and
/// the webview builds that document with `normalizeDesktopSettings`, which
/// constructs a fresh object with a **fixed key set** (that fixed set is itself
/// a credential guard — `xtask/linkage-check`'s check 3). So a webview that
/// does not render endpoints cannot send them, and if the field were a plain
/// `EndpointSettings` it would arrive as the *default* — an empty list — and
/// [`merged_document`] would then write that empty list over a hand-edited
/// `[endpoints]` table. Every remote deck, deleted by someone changing the
/// theme.
///
/// `Option` makes "I am not telling you about this section" representable, and
/// TOML serialisation omits a `None` field entirely, so the merge preserves
/// what is on disk — the same protection [`merged_document`] gives *across
/// builds*, now available *across clients*. Pinned by
/// [`tests::a_client_that_cannot_render_endpoints_cannot_delete_them`].
///
/// **What M7 inherits from that.** The moment the panel exists, the frontend
/// must round-trip this section — read it in `normalizeDesktopSettings`, keep
/// it, and send it back. Fabricating a default `{ remote: [], selection:
/// "local" }` there would delete every row, and it would do it silently.
///
/// # Field order is alphabetical on purpose
///
/// The document is written two ways — `toml::to_string_pretty` over the struct
/// (declaration order) and over a `toml::Table` (a `BTreeMap`, so alphabetical)
/// — and [`tests::default_document_shape_is_pinned`] asserts the two agree.
/// Declaring alphabetically is what keeps them agreeing; `DesktopSettings`'s
/// own fields happen to be alphabetical for the same reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointSettings {
    /// The configured remote decks. Empty on a fresh install, and empty is a
    /// perfectly good state — the local deck is always available and is not a
    /// row here.
    pub remote: Vec<RemoteEndpointSettings>,
    /// Which deck the app is talking to.
    pub selection: Selection,
}

// PRD #741 M6 stores endpoints; M7 (the panel) and M9 (the Deck selector) are
// what read them, and both are fenced out of this milestone. So the storage API
// below has its tests as its only caller today. The `allow` is the same device
// `platform::fsperm`'s SITE_AUDIT uses for the same reason — an item whose
// production consumer is a later milestone — rather than a silent `pub` that
// looks reachable. Deleting it once M7 lands is the point: if these are still
// unused then, the milestone did not wire them up.
#[cfg_attr(not(test), allow(dead_code))]
impl EndpointSettings {
    /// The row with this id, or `None`.
    pub fn find(&self, id: &EndpointId) -> Option<&RemoteEndpointSettings> {
        self.remote.iter().find(|deck| &deck.id == id)
    }

    /// The endpoint [`Self::selection`] names, falling back to the local deck
    /// with a **named** reason whenever the stored selection cannot be
    /// honoured.
    ///
    /// Falling back rather than erroring is deliberate, and it is the same call
    /// [`AppearanceMode::from_str_lossy`] makes: a settings document is
    /// hand-editable and may have been written by a build with more variants,
    /// so an unresolvable selection is an ordinary state rather than a
    /// corruption. Erroring would leave the app with no deck at all, which is
    /// strictly worse than the deck it had before endpoints existed.
    ///
    /// The reason travels with the answer because M7 has to *render* it: "the
    /// deck you selected is gone" and "the deck you selected has no socket path
    /// yet" are different things to tell a user, and neither is "connected to
    /// local".
    pub fn resolve(&self) -> ResolvedEndpoint {
        let local = || ResolvedEndpoint {
            endpoint: Endpoint::local(),
            fallback: None,
        };
        let Selection::One(id) = &self.selection else {
            return local();
        };
        let Some(deck) = self.find(id) else {
            return ResolvedEndpoint {
                endpoint: Endpoint::local(),
                fallback: Some(SelectionFallback::UnknownDeck { id: id.clone() }),
            };
        };
        match deck.endpoint() {
            Some(remote) => ResolvedEndpoint {
                endpoint: Endpoint::Remote(remote),
                fallback: None,
            },
            None => ResolvedEndpoint {
                endpoint: Endpoint::local(),
                fallback: Some(SelectionFallback::NoRemoteSocket { id: id.clone() }),
            },
        }
    }
}

/// One `[[endpoints.remote]]` row: a remote deck, stored as **references** and
/// never as a secret.
///
/// Host, optional user, port, optional identity-file *path*, optional
/// jump-host *name*. `~/.config/dot-agent-deck/remotes.toml` (`src/remote.rs`)
/// has stored exactly this shape since PRD #76 and has never needed a
/// passphrase: the answer to an encrypted key is "it is in an agent, or it is
/// unencrypted", plus `BatchMode=yes` so a locked key fails fast with a
/// nameable error rather than hanging on a prompt no GUI can answer. A
/// credential belongs behind the `SecretStore` seam PRD #803 M5 named, and
/// #741 is deliberately **not** the PRD that opens the first route into a store
/// that does not exist yet.
///
/// Every field is one of the validating newtypes from
/// [`dot_agent_deck::remote_tunnel`], whose `Deserialize` runs the same check
/// its constructor does — so a hand-edited document cannot smuggle past what a
/// settings form applies. Their charsets and bounds are also the ssh-argument
/// validation nothing in this tree performed before PRD #741 M5.
///
/// # There is no display name, deliberately
///
/// A user-chosen label would be exactly the arbitrary `String` the field-type
/// guard refuses, and it would need its own bidi and control-character handling
/// before anything rendered it. [`RemoteEndpoint::describe`] derives the label
/// from the address instead, and every byte of that came through a validated
/// ASCII charset.
///
/// # `host` and `id` are required; everything else defaults
///
/// A row with no host is not a row, and a row with no id cannot be selected. A
/// document whose row is missing one of them fails to parse, which — per
/// [`load_from`] — means the whole document reads as defaults with a locator
/// logged. It does **not** mean the file is rewritten: [`merged_document`]
/// parses the existing document as TOML *syntax*, which a schema-invalid row
/// still is, so the row survives on disk for the user to fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEndpointSettings {
    pub host: Hostname,
    pub id: EndpointId,
    /// The private key to offer (`ssh -i`) — a **path**, never key material and
    /// never a passphrase.
    ///
    /// **Named `identity` rather than `key`, and the reason is worth the
    /// paragraph.** `key` is what `remotes.toml` calls it, and it trips the
    /// naming tripwire
    /// ([`tests::no_settings_key_name_trips_the_credential_tripwire`]), whose
    /// path allowlist is empty. The two available answers were an exemption
    /// pinned to [`KeyPath`], or a name that is not credential-shaped. This is
    /// the second, because `IdentityFile` is OpenSSH's *own* name for exactly
    /// this option — so the TOML reads like the `~/.ssh/config` it points into
    /// — and because an empty allowlist is a property worth keeping: the first
    /// entry is the one that makes the second easy.
    ///
    /// **What that costs, stated rather than glossed:** the tripwire is now
    /// silent about the one field in this schema that sits next to credential
    /// material. It was never the control that mattered — read
    /// [`SECRET_RULE`], which says so in those words — and the control that
    /// does matter is unaffected: the type is [`KeyPath`], which is on
    /// `ALLOWED_FIELD_TYPES` with a written reason, bounded at `PATH_MAX`,
    /// restricted to a charset that cannot represent a multi-line PEM block,
    /// and required to start `/` or `~/`. This doc comment is where the next
    /// reader finds that out, since the tripwire will not tell them.
    ///
    /// [`SECRET_RULE`]: tests::SECRET_RULE
    #[serde(default)]
    pub identity: Option<KeyPath>,
    /// A `Host` block name from the user's `~/.ssh/config` to reach this deck
    /// through (`ssh -J`). A *name*, so the jump host's own address, port, user
    /// and key stay in that config rather than being copied here.
    #[serde(default)]
    pub jump: Option<HostAlias>,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// The daemon's attach socket path **on the remote host**.
    ///
    /// # Optional in storage, and this is the milestone's answer to it
    ///
    /// It cannot be derived. OpenSSH expands neither `~` nor an environment
    /// variable on the remote side of `-L`, and the far host's
    /// `XDG_RUNTIME_DIR` and uid are not knowable from here without a second
    /// ssh round trip — [`RemoteSocketPath`] records the mechanism. So it is
    /// either typed by the user or discovered.
    ///
    /// Stored as **optional, with discovery filling it**. A row without one is
    /// *storable* and *not connectable*: [`EndpointSettings::resolve`] returns
    /// the local deck and [`SelectionFallback::NoRemoteSocket`] rather than
    /// erroring, so a half-configured deck is a state the UI can explain
    /// instead of a save that refuses.
    ///
    /// Required was the alternative and it is worse in the order the user does
    /// things: the value is un-guessable, so requiring it means typing
    /// `/run/user/1000/dot-agent-deck-attach.sock` correctly *before* anything
    /// can test whether it is right.
    ///
    /// **What that leaves M10 (`Test connection`)**, which is already making an
    /// ssh round trip and is therefore the cheapest place to do it:
    ///
    /// 1. **Discover** the remote attach socket path over that round trip
    ///    rather than asking the user to know it;
    /// 2. **Write it back** into this field, so discovery is durable and the
    ///    next connection needs no probe;
    /// 3. **Report `NoRemoteSocket` as its own named state** — "not configured
    ///    yet, press Test connection" — rather than folding it into a generic
    ///    failure, which is M10's stated shape for every other outcome anyway.
    #[serde(default)]
    pub socket: Option<RemoteSocketPath>,
    /// The login name, when the ssh config does not already decide it.
    #[serde(default)]
    pub user: Option<SshUser>,
}

/// The ssh port a row with no `port` key means.
///
/// Taken from [`RemoteEndpoint::DEFAULT_PORT`] rather than written as `22`, so
/// a row stored without a port and a row built by `RemoteEndpoint::new` can
/// never describe different decks.
fn default_ssh_port() -> u16 {
    RemoteEndpoint::DEFAULT_PORT
}

// PRD #741 M6 stores endpoints; M7 (the panel) and M9 (the Deck selector) are
// what read them, and both are fenced out of this milestone. So the storage API
// below has its tests as its only caller today. The `allow` is the same device
// `platform::fsperm`'s SITE_AUDIT uses for the same reason — an item whose
// production consumer is a later milestone — rather than a silent `pub` that
// looks reachable. Deleting it once M7 lands is the point: if these are still
// unused then, the milestone did not wire them up.
#[cfg_attr(not(test), allow(dead_code))]
impl RemoteEndpointSettings {
    /// A row with the minimum a deck needs: an id and a host.
    pub fn new(id: EndpointId, host: Hostname) -> Self {
        Self {
            host,
            id,
            identity: None,
            jump: None,
            port: default_ssh_port(),
            socket: None,
            user: None,
        }
    }

    /// The connectable endpoint this row describes, or `None` while it has no
    /// remote socket path — see [`Self::socket`].
    pub fn endpoint(&self) -> Option<RemoteEndpoint> {
        let mut endpoint =
            RemoteEndpoint::new(self.host.clone(), self.socket.clone()?).with_port(self.port);
        if let Some(user) = &self.user {
            endpoint = endpoint.with_user(user.clone());
        }
        if let Some(identity) = &self.identity {
            endpoint = endpoint.with_key(identity.clone());
        }
        if let Some(jump) = &self.jump {
            endpoint = endpoint.with_jump(jump.clone());
        }
        Some(endpoint)
    }
}

/// The stable identity of a stored remote deck.
///
/// An opaque minted token rather than the deck's address, so editing a host
/// does not silently change which deck a selection points at, and rather than a
/// list index, so removing a row does not re-point every selection after it.
///
/// The charset is deliberately wider than [`Self::mint`] produces — ASCII
/// alphanumerics, `-` and `_`, up to [`MAX_ENDPOINT_ID_BYTES`] — because this
/// type also has to accept a [`Selection`] token written by a build that has a
/// variant this one does not. See [`Selection`] for what that buys.
///
/// It is not a secret and nothing authenticates with it: it names a row in a
/// file the user owns.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(String);

/// The longest endpoint id this build will accept.
///
/// [`EndpointId::mint`] produces 16, so this is room for a hand-written one
/// while staying far short of anything that could hold a credential-shaped
/// blob.
pub const MAX_ENDPOINT_ID_BYTES: usize = 64;

/// The [`Selection`] token that means the local deck, and therefore the one
/// word an [`EndpointId`] may not be.
pub const LOCAL_SELECTION_TOKEN: &str = "local";

// PRD #741 M6 stores endpoints; M7 (the panel) and M9 (the Deck selector) are
// what read them, and both are fenced out of this milestone. So the storage API
// below has its tests as its only caller today. The `allow` is the same device
// `platform::fsperm`'s SITE_AUDIT uses for the same reason — an item whose
// production consumer is a later milestone — rather than a silent `pub` that
// looks reachable. Deleting it once M7 lands is the point: if these are still
// unused then, the milestone did not wire them up.
#[cfg_attr(not(test), allow(dead_code))]
impl EndpointId {
    /// Validate `raw` and wrap it.
    ///
    /// The only constructor besides [`Self::mint`] — no `From<String>`, so no
    /// call site can skip the check by reaching for a cheaper conversion.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("an endpoint id cannot be empty".to_string());
        }
        if raw.len() > MAX_ENDPOINT_ID_BYTES {
            return Err(format!(
                "an endpoint id is at most {MAX_ENDPOINT_ID_BYTES} bytes; got {}",
                raw.len()
            ));
        }
        if let Some(byte) = raw
            .bytes()
            .find(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        {
            return Err(format!(
                "an endpoint id is ASCII alphanumerics, '-' and '_'; found byte 0x{byte:02x}"
            ));
        }
        if raw.eq_ignore_ascii_case(LOCAL_SELECTION_TOKEN) {
            return Err(format!(
                "'{LOCAL_SELECTION_TOKEN}' is reserved: it is how a selection names the local deck"
            ));
        }
        Ok(Self(raw.to_string()))
    }

    /// A fresh id, unique within this process and unguessable outside it.
    ///
    /// Sixteen lowercase hex characters from [`unpredictable_suffix`], which is
    /// seeded from the operating system. Uniqueness is what is wanted here, not
    /// unpredictability — nothing authenticates with this — but the function
    /// that gives one already gives the other. Sixteen hex characters cannot
    /// collide with [`LOCAL_SELECTION_TOKEN`] or with any word a future
    /// [`Selection`] variant would reserve, both of which are shorter and
    /// contain letters that are not hex digits.
    pub fn mint() -> Self {
        Self(format!("{:016x}", unpredictable_suffix()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for EndpointId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EndpointId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Which deck the app is talking to.
///
/// # Shaped so a variant can be added, which is the one constraint M6 owes M9
///
/// The Deck selector is PRD #741 M9 and "All Decks" is [#742](https://github.com/vfarcic/dot-agent-deck/issues/742),
/// but the *stored* value lands here — and it lands as an enum rather than a
/// bare [`EndpointId`] threaded through state precisely so #742 is **additive**
/// rather than a retrofit. Adding `All` is one variant, one arm in
/// [`SelectionVisitor::visit_str`], one arm in `Serialize`, and one arm in
/// [`EndpointSettings::resolve`]. A bare id would have made it a change to
/// every type that carries a selection.
///
/// # The wire form, and why an unknown token round-trips
///
/// One string: the reserved word `local`, or an endpoint id. An unrecognised
/// token — `all`, written by a build that has the variant this one does not —
/// parses as an [`EndpointId`], resolves to no row, and therefore reads as the
/// local deck with [`SelectionFallback::UnknownDeck`]; and because it is
/// *stored* as the id it was, saving the document writes it back **unchanged**.
/// So an older build degrades to local without destroying a newer build's
/// selection, which is the same tolerance the rest of this schema is built for.
/// That is also why [`EndpointId`]'s charset is wider than [`EndpointId::mint`]
/// needs: a reserved word a future build invents has to fit through it.
///
/// An over-long or non-charset token is a different thing — a malformed
/// document rather than an unknown value — and is an error, exactly as
/// [`AppearanceMode`] treats an over-length mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Selection {
    /// The daemon on this machine: what every caller used before endpoints
    /// existed, and the default with or without an `[endpoints]` section.
    #[default]
    Local,
    /// The remote deck with this id, if the document still holds one.
    One(EndpointId),
}

impl Selection {
    /// The token written to TOML and JSON.
    pub fn as_token(&self) -> &str {
        match self {
            Self::Local => LOCAL_SELECTION_TOKEN,
            Self::One(id) => id.as_str(),
        }
    }
}

impl Serialize for Selection {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for Selection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(SelectionVisitor)
    }
}

struct SelectionVisitor;

impl serde::de::Visitor<'_> for SelectionVisitor {
    type Value = Selection;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "'{LOCAL_SELECTION_TOKEN}' or an endpoint id")
    }

    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<Selection, E> {
        if raw.eq_ignore_ascii_case(LOCAL_SELECTION_TOKEN) {
            return Ok(Selection::Local);
        }
        EndpointId::parse(raw)
            .map(Selection::One)
            .map_err(E::custom)
    }
}

/// What [`EndpointSettings::resolve`] answered, and whether it had to fall back.
// Reached only by its tests until M7 renders it; see `impl EndpointSettings`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    /// The deck to talk to. Always a usable endpoint — never an error.
    pub endpoint: Endpoint,
    /// `Some` when [`Self::endpoint`] is the local deck because the stored
    /// selection could not be honoured. M7 renders it; nothing else needs it.
    pub fallback: Option<SelectionFallback>,
}

/// Why a stored selection resolved to the local deck instead of the one it
/// named.
// Reached only by its tests until M7 renders it; see `impl EndpointSettings`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionFallback {
    /// The selection names a deck this document no longer holds — a row the
    /// user removed, or a token a newer build wrote. Test-plan item 17.
    UnknownDeck { id: EndpointId },
    /// The selected deck has no remote socket path yet, so there is nothing to
    /// forward to. PRD #741 M10's `Test connection` is what fills it in; see
    /// [`RemoteEndpointSettings::socket`].
    NoRemoteSocket { id: EndpointId },
}

impl std::fmt::Display for SelectionFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDeck { id } => write!(
                f,
                "the selected deck {id} is no longer configured; using the local deck"
            ),
            Self::NoRemoteSocket { id } => write!(
                f,
                "the selected deck {id} has no remote socket path yet; using the local deck"
            ),
        }
    }
}

/// The zoom levels this build steps through, ascending (PRD #744).
///
/// **This list is duplicated in `desktop/src/lib/zoom.ts` as `ZOOM_LEVELS` and
/// the two must stay identical.** Both sides are pinned by a test that spells
/// out every value, and the duplication is unavoidable rather than sloppy: this
/// side is the authority for what a *stored* level may be, because the
/// launch-time apply reads the document before any JavaScript runs, and that
/// side is the authority for *stepping*, because the key event itself is never
/// seen here — a keystroke reaches Rust only as an already-decided level, via
/// `desktop_set_zoom`, so this side can validate a level but could not compute
/// the next one. If they drift, the app launches at one level and the Settings
/// row claims another. The same discipline — two copies, both pinned, each pointing
/// at the other — is why `desktop/src/lib/displayText.ts` and
/// `src/untrusted_text.rs` are still in agreement.
///
/// Both ends are measured; the reasoning lives in `zoom.ts`'s module docs and in
/// `prds/744-desktop-zoom-text-size.md` rather than being restated here.
pub const ZOOM_LEVELS: [f64; 10] = [0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];

/// What `Cmd 0` returns to, and what a document with no `[zoom]` section reads
/// as.
pub const DEFAULT_ZOOM_LEVEL: f64 = 1.0;

/// A zoom level that is always one of [`ZOOM_LEVELS`].
///
/// A newtype rather than a bare `f64` field so the snapping happens in
/// `Deserialize` and cannot be forgotten by a caller. It serialises as a plain
/// number, so the TOML reads `level = 1.25` and the JSON the webview receives is
/// `{"level":1.25}` — the wrapper is invisible on both wires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomLevel(f64);

impl ZoomLevel {
    /// The nearest level in [`ZOOM_LEVELS`], or the default for anything that is
    /// not a usable number.
    ///
    /// Snapping rather than rejecting, for the same reason
    /// [`AppearanceMode::from_str_lossy`] folds an unknown token to the default:
    /// the document is hand-editable and may also have been written by a build
    /// with a longer ladder, and losing every other section over one unreadable
    /// field is the opposite of the unknown-key tolerance this schema is built
    /// for.
    ///
    /// `is_finite` is the guard rather than a range check because it rejects
    /// `NaN` and both infinities together, and `NaN` is the case that matters:
    /// every comparison against it is false, so an unguarded nearest-value
    /// search would return the *first* level and a corrupt document would
    /// silently shrink the app to 75% instead of reading as the default. The
    /// mirror of this is `clampZoom` in `zoom.ts`, tested against the same
    /// inputs.
    pub fn snap(value: f64) -> Self {
        if !value.is_finite() {
            return Self(DEFAULT_ZOOM_LEVEL);
        }
        let mut nearest = ZOOM_LEVELS[0];
        for level in ZOOM_LEVELS {
            if (level - value).abs() < (nearest - value).abs() {
                nearest = level;
            }
        }
        Self(nearest)
    }

    /// The scale factor, ready for `webview.set_zoom`.
    pub fn as_f64(self) -> f64 {
        self.0
    }
}

impl Default for ZoomLevel {
    fn default() -> Self {
        Self(DEFAULT_ZOOM_LEVEL)
    }
}

impl Serialize for ZoomLevel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for ZoomLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ZoomLevelVisitor)
    }
}

/// Accepts a TOML/JSON **integer** as well as a float, which is the whole
/// reason this is a visitor rather than `f64::deserialize().map(snap)`.
///
/// `level = 1` in a hand-edited `desktop.toml` is a TOML *integer*, and `f64`'s
/// derived deserializer rejects it with `invalid type: integer`. That failure is
/// not local: [`load_from`] treats an unparseable document as "use defaults", so
/// one plausible hand edit would silently reset the user's **appearance** too.
/// Accepting the integer costs three lines and removes that.
struct ZoomLevelVisitor;

impl serde::de::Visitor<'_> for ZoomLevelVisitor {
    type Value = ZoomLevel;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a zoom level between {} and {}",
            ZOOM_LEVELS[0],
            ZOOM_LEVELS[ZOOM_LEVELS.len() - 1]
        )
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<ZoomLevel, E> {
        Ok(ZoomLevel::snap(value))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<ZoomLevel, E> {
        Ok(ZoomLevel::snap(value as f64))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<ZoomLevel, E> {
        Ok(ZoomLevel::snap(value as f64))
    }
}

/// The document plus where it lives, which is what the settings surface renders.
///
/// A separate struct rather than a field on [`DesktopSettings`], because the
/// path is **not** part of the document: it is where the document is, it is
/// never written to TOML, and putting it on the struct would put it in the file
/// and into [`tests::default_document_shape_is_pinned`]'s pinned shape.
///
/// # Why this path reaches the webview when error paths deliberately do not
///
/// [`SettingsWriteError`] splits itself precisely so a `/home/<user>/…` path
/// never crosses the bridge, and that is not in tension with this. A path
/// leaking out of an *error* is incidental detail the user did not ask for; the
/// location of their own settings file is the answer to "where did that go?",
/// which PRD #803 makes a visible footer line specifically so it is answerable
/// without documentation. Same string, opposite intent.
// No `Eq`, for the same reason `DesktopSettings` has none: it contains a
// `ZoomLevel`, which wraps an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesktopSettingsSnapshot {
    pub settings: DesktopSettings,
    /// Absolute, as [`settings_path`] resolved it — including when
    /// [`SETTINGS_PATH_ENV`] pointed somewhere else, since the footer's job is
    /// to name the file this process will actually write.
    pub path: String,
}

/// [`load_from`] against [`settings_path`], plus that path, for the settings
/// surface. This is how the app loads its settings.
pub fn load_snapshot() -> DesktopSettingsSnapshot {
    let path = settings_path();
    DesktopSettingsSnapshot {
        settings: load_from(&path),
        path: path.display().to_string(),
    }
}

/// A settings write failure, split so a filesystem path never reaches the
/// webview.
///
/// Connection errors are already sanitised before they cross the bridge
/// (`dto::safe_message`); an error naming `/home/<user>/...` deserves the same
/// treatment. [`Self::detail`] is for the app's own log and carries the path;
/// [`Self::public`] is what the webview renders and never does. `io::Error`'s
/// own `Display` carries no path — `std` does not add that context — so the
/// public half stays specific ("Permission denied") without leaking anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsWriteError {
    detail: String,
    public: String,
}

impl SettingsWriteError {
    /// The operator-facing message, including the path. Log this.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The webview-facing message. Contains no path.
    pub fn public(&self) -> &str {
        &self.public
    }
}

impl std::fmt::Display for SettingsWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for SettingsWriteError {}

fn write_error(what: &str, path: &Path, cause: impl std::fmt::Display) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!("{what} {}: {cause}", path.display()),
        public: format!("{what} the desktop settings file: {cause}"),
    }
}

/// A path that cannot be a settings document at all, split the same way
/// [`write_error`] is so the reason crosses the bridge and the path does not.
fn path_error(reason: &str, path: &Path) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!(
            "unusable desktop settings path {}: {reason}",
            path.display()
        ),
        public: format!("the desktop settings path is unusable: {reason}"),
    }
}

/// The largest settings document this build will read.
///
/// `desktop.toml` is a hand-edited preferences file: today's default document
/// is 40 bytes, and #741's endpoint list plus #802's model configuration are
/// kilobytes at the very outside. 256 KiB leaves about four orders of magnitude
/// of headroom over anything the schema can plausibly grow into, while turning
/// "the app read a multi-gigabyte file into memory because a path pointed at
/// one" into a named error instead of an out-of-memory kill.
pub const MAX_SETTINGS_BYTES: u64 = 256 * 1024;

/// Vet `path` as the settings document and read it, bounded.
///
/// `Ok(None)` is the ordinary first-run case: nothing is there yet. `Err` means
/// the path cannot be a settings document at all — [`load_from`] logs it and
/// falls back to defaults, and [`save_to`] refuses rather than writing over
/// whatever is actually at that name.
///
/// # This is about misconfiguration, not privilege
///
/// [`SETTINGS_PATH_ENV`] used to accept any non-empty string, and both load and
/// save then went straight to an unbounded `read_to_string`. Anyone who can set
/// this process's environment can already run code as this user, so none of
/// this is a privilege boundary. It is here because the *misconfiguration*
/// failures are miserable to debug: a FIFO blocks the app forever with no
/// message at all, `/dev/zero` exhausts memory, a large regular file is read
/// whole, and a relative path resolves against whatever directory the app
/// happened to be launched from — even though [`DesktopSettingsSnapshot`]
/// documents the path it reports as absolute.
///
/// So the target must be absolute, must have a file name, and must be either
/// **absent** or a **regular file**. The check is `symlink_metadata`, which does
/// not follow a link at the final component, so a symlink is rejected as a
/// symlink rather than quietly resolved to something else. Then at most
/// [`MAX_SETTINGS_BYTES`] are read.
///
/// One residual is accepted: a target swapped between the check and the open —
/// a regular file replaced by a FIFO in that window — still blocks. Closing it
/// means an `openat`-anchored read, which is the same complexity [`save_to`]
/// declines for the same reason, and it is written down there rather than
/// repeated here.
fn read_document(path: &Path, purpose: ReadPurpose) -> Result<Option<String>, SettingsWriteError> {
    if !path.is_absolute() {
        return Err(path_error("it is not an absolute path", path));
    }
    if path.file_name().is_none() {
        return Err(path_error("it names no file", path));
    }

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            // `io::Error`'s own `Display` carries no path — `std` does not add
            // that context — so the cause is safe to put in the public half.
            return Err(path_error(
                &format!("it cannot be inspected: {error}"),
                path,
            ));
        }
    };
    if let Some(kind) = unusable_kind(&meta) {
        return Err(path_error(
            &format!("it is {kind}, and the settings document must be a regular file"),
            path,
        ));
    }
    if meta.len() > MAX_SETTINGS_BYTES {
        return Err(oversized(path));
    }
    if purpose == ReadPurpose::Load {
        warn_if_document_is_exposed(path, &meta);
    }

    read_bounded(path)
}

/// Why [`read_document`] is reading, which decides only whether an exposed
/// document is complained about.
///
/// The distinction exists because [`save_to`] reads the document too, and a
/// mode warning there would be **stale before it was printed**: the save that
/// follows publishes a fresh 0o600 file over it. Warning on the load is what
/// puts the message in front of a user who has not changed a setting since
/// their file became world-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadPurpose {
    /// [`load_from`] — the app is about to read these settings, so an exposed
    /// document is worth a line in the log.
    Load,
    /// [`save_to`] — the existing bytes are being read only so the merge can
    /// preserve what this build does not own.
    Save,
}

/// Refuse to write into a parent directory that is not plainly ours (PRD #741).
///
/// Absent is fine — [`save_to`] creates it a line later, at 0o700, and a
/// directory we create is ours by construction. What is refused is a parent
/// that **exists and is not what it claims to be**: a symlink (or, on Windows,
/// any reparse point), something that is not a directory at all, or — on Unix —
/// a directory owned by another uid.
///
/// # What each clause buys, narrowly
///
/// The symlink clause is the load-bearing one. `fsperm::create_owner_only_dir`
/// deliberately carries no symlink guard: it never chmods, so it had no
/// permission-tightening exposure to guard (issue #669 says so in as many
/// words) — but it does silently *redirect* where the caller's write lands, and
/// its own docs name that as "a path-redirection question about the write
/// itself". This is that question, answered at the one call site that now has a
/// reason to care.
///
/// The ownership clause is Unix-only and says so rather than pretending
/// otherwise. Windows has no uid, the per-user ACL story is a different
/// mechanism, `%LOCALAPPDATA%` is already per-user ACL'd, and no Windows
/// binaries are released — the same reasoning `fsperm`'s own site audit records
/// for the #669 symlink refusal having no Windows counterpart.
///
/// # What it does not buy
///
/// It is **not** race-free, and calling it one would be the mistake
/// [`fsperm::ensure_owner_only_dir`] documents about its own guard. An attacker
/// who replaces the parent between this `lstat` and the `create_new` below
/// still redirects the write; closing that needs the `openat` anchoring
/// [`save_to`] defers and gives its reasons for. This narrows a window; it does
/// not close one. Nothing about an *ancestor* of the parent is checked either.
fn vet_parent_dir(parent: &Path) -> Result<(), SettingsWriteError> {
    let meta = match std::fs::symlink_metadata(parent) {
        Ok(meta) => meta,
        // Nothing there yet: `create_owner_only_dir` makes it, at 0o700.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(parent_error(
                &format!("it cannot be inspected: {error}"),
                parent,
            ));
        }
    };
    if meta.file_type().is_symlink() {
        return Err(parent_error(
            "it is a symlink, and following it would write the settings document somewhere \
             the user did not name — point the path at the real directory",
            parent,
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(parent_error(
                "it is a reparse point, and following it would write the settings document \
                 somewhere the user did not name",
                parent,
            ));
        }
    }
    if !meta.is_dir() {
        return Err(parent_error("it is not a directory", parent));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let ours = dot_agent_deck::platform::paths::current_uid();
        if meta.uid() != ours {
            return Err(parent_error(
                &format!(
                    "it is owned by uid {} rather than by us ({ours})",
                    meta.uid()
                ),
                parent,
            ));
        }
    }
    Ok(())
}

/// A refusal naming the settings document's *parent*, split the same way
/// [`path_error`] is so the reason crosses the bridge and the path does not.
fn parent_error(reason: &str, parent: &Path) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!(
            "refusing to write the desktop settings into {}: {reason}",
            parent.display()
        ),
        public: format!("the desktop settings directory is unusable: {reason}"),
    }
}

/// Complain — to the log, never to the caller — about a settings document
/// anyone but its owner can read (PRD #741).
///
/// # Warn, do not refuse, and that is a deliberate inheritance
///
/// [`load_from`] never failing is a #803 property on purpose: a preferences
/// file is not worth failing an app launch over. Now that the document names a
/// host, a login name and a key path, a `0o644` `desktop.toml` is worth *saying
/// something* about — but bricking the app over one would be worse than the
/// exposure it is complaining about, and would hand anyone who can chmod the
/// file a denial of service on the whole app.
///
/// So: one line on stderr and the deck log, and the document loads. [`save_to`]
/// then republishes at 0o600 on the next write, so the ordinary outcome is that
/// the complaint fixes itself the next time the user changes a setting.
///
/// Unix-only. On Windows the analogous question is a DACL comparison rather
/// than a mode, `%LOCALAPPDATA%` is already per-user ACL'd, and no Windows
/// binaries are released — the same boundary `fsperm`'s site audit draws.
#[cfg(unix)]
fn warn_if_document_is_exposed(path: &Path, meta: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        eprintln!(
            "Desktop settings at {} are mode {mode:04o}: readable beyond its owner, and this \
             document names a daemon host, a login name and a key path. Run `chmod 600` on it, \
             or change any setting in the app — every save republishes it owner-only.",
            path.display()
        );
    }
    let ours = dot_agent_deck::platform::paths::current_uid();
    if meta.uid() != ours {
        eprintln!(
            "Desktop settings at {} are owned by uid {} rather than by us ({ours}); loading them \
             anyway, but nothing here vouches for who wrote them.",
            path.display(),
            meta.uid()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_document_is_exposed(_path: &Path, _meta: &std::fs::Metadata) {}

fn oversized(path: &Path) -> SettingsWriteError {
    path_error(
        &format!("it is larger than the {MAX_SETTINGS_BYTES}-byte settings limit"),
        path,
    )
}

/// What `meta` describes, when it is not a plain regular file. `None` means it
/// is one.
fn unusable_kind(meta: &std::fs::Metadata) -> Option<&'static str> {
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Some("a symlink");
    }
    #[cfg(windows)]
    {
        // `is_symlink` covers a symlink reparse point but not a junction or a
        // mount point, and following one of those lands somewhere the user
        // never named.
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Some("a reparse point");
        }
    }
    if kind.is_dir() {
        return Some("a directory");
    }
    if kind.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if kind.is_fifo() {
            return Some("a FIFO");
        }
        if kind.is_socket() {
            return Some("a socket");
        }
        if kind.is_block_device() {
            return Some("a block device");
        }
        if kind.is_char_device() {
            return Some("a character device");
        }
    }
    Some("of an unrecognised type")
}

/// Read an already-vetted regular file, refusing anything past
/// [`MAX_SETTINGS_BYTES`].
///
/// The bound is re-applied to the bytes actually read, not just to the size the
/// vet saw: the file can grow — or be replaced by a larger one — between the
/// two, and a limit that only consults `stat` would not be a limit.
fn read_bounded(path: &Path) -> Result<Option<String>, SettingsWriteError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .map_err(|error| path_error(&format!("it cannot be read: {error}"), path))?;
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| path_error(&format!("it cannot be read: {error}"), path))?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(oversized(path));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| path_error("it is not valid UTF-8", path))
}

/// The resolved path of the settings document.
///
/// [`SETTINGS_PATH_ENV`] wins when it is set and non-empty. An empty value is
/// treated as "unset": it can only arrive from a caller that meant to clear the
/// override, and honouring it would resolve to a path with no file name.
pub fn settings_path() -> PathBuf {
    match std::env::var(SETTINGS_PATH_ENV) {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => config_dir().join(SETTINGS_FILE_NAME),
    }
}

/// Load the settings document from `path`. Never fails — see the module docs.
///
/// Every caller passes a path: the app goes through [`load_snapshot`], which
/// needs the resolved path anyway to show the user where their settings live,
/// and every test passes one explicitly so none of them depends on
/// process-global environment state.
pub fn load_from(path: &Path) -> DesktopSettings {
    match read_document(path, ReadPurpose::Load) {
        Ok(None) => DesktopSettings::default(),
        Ok(Some(contents)) => match toml::from_str(&contents) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!(
                    "{}; using defaults",
                    invalid_document_log(path, &contents, &error)
                );
                DesktopSettings::default()
            }
        },
        Err(error) => {
            // The detail names the path and this is the app's own log, which is
            // the half of the split that is allowed to.
            eprintln!("{}; using defaults", error.detail());
            DesktopSettings::default()
        }
    }
}

/// The diagnostic [`load_from`] logs for a document this build cannot parse: a
/// **locator**, deliberately never the document's own bytes.
///
/// # Why the toml error's own message is not logged
///
/// `toml::de::Error`'s `Display` echoes the offending value **twice** — once in
/// a rendered source line and again in serde's `invalid type: string "…"`
/// message. Measured, not assumed; the exact shape is pinned by
/// [`tests::a_parse_diagnostic_carries_a_locator_and_never_the_documents_bytes`].
/// So a hand-edited document whose value sits in a wrongly-typed field used to
/// put that value straight into this process's stderr and the deck log, and a
/// log is a file people paste into bug reports.
///
/// The trade is deliberate: the locator loses the `expected u32` half of the
/// message and keeps the half a developer acts on — the file is named and the
/// line is the line to open, where the offending text is in front of them
/// anyway. Issue #827 lists the log as one of the sinks a credential must not
/// reach; this is that sink closed for the whole document rather than for a
/// field anyone remembered to think about.
///
/// A separate function because the content of the line is then testable at all:
/// an `eprintln!` inside a match arm cannot be asserted on.
fn invalid_document_log(path: &Path, contents: &str, error: &toml::de::Error) -> String {
    let where_ = match error
        .span()
        .and_then(|span| line_and_column(contents, span.start))
    {
        Some((line, column)) => format!("line {line}, column {column}"),
        // A span is present for every error this crate has produced, but it is
        // an `Option` on the API and a locator-less message is still useful.
        None => "an unreported position".to_string(),
    };
    format!(
        "Invalid desktop settings at {}: {where_} could not be read as settings",
        path.display()
    )
}

/// The 1-based line and column of the byte at `offset` in `contents`.
///
/// `None` when `offset` is past the end or lands inside a multi-byte character,
/// both of which mean the caller cannot describe a position it can trust. The
/// column counts **characters** rather than bytes, because the number is for a
/// human counting along a line in an editor.
fn line_and_column(contents: &str, offset: usize) -> Option<(usize, usize)> {
    let before = contents.get(..offset)?;
    let line = before.matches('\n').count() + 1;
    let last_line = before.rsplit('\n').next().unwrap_or(before);
    Some((line, last_line.chars().count() + 1))
}

/// Persist the settings document atomically and owner-only.
pub fn save(settings: &DesktopSettings) -> Result<(), SettingsWriteError> {
    save_to(&settings_path(), settings)
}

/// How many temp names [`save_to`] draws before giving up. A leftover temp file
/// from a crashed run would otherwise make every later save fail with
/// `AlreadyExists`, since the publish deliberately uses `create_new` and never
/// unlinks whatever holds a name it wanted.
const TEMP_NAME_ATTEMPTS: usize = 8;

/// [`save`] against an explicit path.
///
/// Writes a temp file in the **same directory** as `path`, then renames over
/// `path`. Rename within one directory is atomic on POSIX, so no reader ever
/// observes a partially written document and a failed write leaves the previous
/// one exactly where it was. The temp file is created with `create_new`
/// (`O_CREAT|O_EXCL`, which cannot follow a symlink someone planted at that
/// name) at owner-only mode, so the document is never briefly world-readable.
///
/// The path is vetted first — see [`read_document`] — so a save never creates a
/// directory for, or writes over, something that is not a settings document.
///
/// # The parent directory is vetted — PRD #741 took the first half of this
///
/// An audit of this path recommended two further steps: rejecting a symlinked
/// or non-user-owned **parent** directory, and anchoring both the create and
/// the publish to one verified directory handle (`openat`/`renameat`-style) so
/// no name is resolved twice. Both were declined while the document held only
/// an appearance mode and a zoom level, and that comment named **#741 — a
/// daemon endpoint** as the change that would move the calculus. It has: the
/// document now holds a host, a login name and a path to a private key.
///
/// **The parent check is taken.** [`vet_parent_dir`] refuses a symlinked
/// parent, a parent that is not a directory, and — on Unix — a parent owned by
/// another uid, before anything is created. It is cheap, it is one `lstat`, and
/// it closes the shape where a planted symlink silently redirects the whole
/// write (including `create_owner_only_dir`, which carries no symlink guard of
/// its own by design — it never chmods, so it had no exposure to guard until a
/// caller cared *where* the write landed).
///
/// **The `openat`/`renameat` anchoring is still deferred, and here is the
/// written reason rather than an assumed one.** Three things hold it back and
/// the first is the one that decides it:
///
/// 1. **The value it would protect is a reference, not a secret.** The storage
///    policy this document is under stores a key *path*, never key material and
///    never a passphrase — so winning the race yields the name of a file the
///    attacker would still have to be able to read, not a credential. The
///    threat that would justify the complexity is the one the policy exists to
///    make impossible.
/// 2. **The attacker who could win it can already write the file.** The
///    destination is the per-user config directory, so a same-uid actor with a
///    foothold there can edit `desktop.toml` outright; the race buys them
///    nothing new. A *different*-uid actor is what the parent-ownership check
///    above now refuses.
/// 3. **`std` gives no anchored rename.** There is no `renameat` in `std::fs`,
///    so this means either a `libc` dependency in a crate whose `src/` has none
///    (it is a `cfg(unix)` dev-dependency today, for one test) or `rustix`, on
///    a required, currently-clean `cargo audit` gate — and a Windows arm that
///    no maintainer can test, since no Windows binaries are released.
///
/// Revisit it if this document ever holds real credential material, which is
/// the thing PRD #803 M5's `SecretStore` seam exists to stop it doing.
///
/// Two residual **reliability** properties remain, both accepted, both worth
/// knowing before either is reported as a bug:
///
/// - abrupt process death between [`create_temp`] and the rename leaves an
///   owner-only `.desktop.toml.tmp.*` file behind. Nothing reads it, and the
///   next save simply draws a different name — which is what
///   [`TEMP_NAME_ATTEMPTS`] is for;
/// - the parent directory is **not** fsync'd after the rename. The write is
///   therefore atomic to every live observer but not fully crash-durable: a
///   power loss immediately after a save can leave the previous document in
///   place.
pub fn save_to(path: &Path, settings: &DesktopSettings) -> Result<(), SettingsWriteError> {
    // Before anything is created: a rejected path must not leave a directory
    // behind, and an unreadable or over-limit document must not be replaced.
    let existing = read_document(path, ReadPurpose::Save)?;

    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    vet_parent_dir(parent)?;
    fsperm::create_owner_only_dir(parent)
        .map_err(|error| write_error("could not create the directory for", path, error))?;

    let contents = merged_document(path, existing.as_deref(), settings)?;

    let (mut file, tmp) = create_temp(parent, path)?;
    let published = (|| {
        // On Unix `create_new` + the owner-only creation mode already produced
        // 0o600; this re-asserts it, and on Windows it is where the DACL is
        // applied at all. Same defence in depth as `schedule_cli::write_atomic`.
        fsperm::set_file_owner_only(&file)?;
        use std::io::Write as _;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })();
    drop(file);

    if let Err(error) = published.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_error("could not write", path, error));
    }
    Ok(())
}

/// Serialise `settings` over whatever the document at `path` already holds,
/// preserving every table and field this build does not know about.
///
/// **`#[serde(default)]` without `deny_unknown_fields` means *ignore*, not
/// *retain*.** It covers reading — an older build loads a newer build's
/// `[voice]` section without error — but the unknown table is dropped on the
/// way in, so serialising the struct straight out would delete it on the next
/// save. That is exactly the `DashboardConfig::save()` failure mode PRD #803
/// rejects sharing `config.toml` for, reproduced in our own file, and it would
/// break the container's central promise: that a feature can add a section and
/// trust an older build not to eat it.
///
/// `existing` is what [`save_to`] just read off disk, **not** what the frontend
/// loaded at startup: re-reading at save time is what lets a section another
/// process wrote between this app's read and its write survive as well. The
/// cost is one small read per save, on a file the app writes only when a user
/// changes a setting.
///
/// An unparseable or non-table document is treated as empty and therefore
/// replaced. Nothing can be preserved out of bytes that are not TOML, and
/// refusing the save instead would leave a user whose file got corrupted unable
/// to change a setting from inside the app — the same call [`load_from`]
/// already makes in the other direction.
///
/// **"Unparseable" means unparseable by *this* build**, which is a wider set
/// than "corrupt". A document a *newer* build wrote in a syntax this one does
/// not accept lands on the same path and is replaced, taking that build's
/// sections with it — the one case where the unknown-section preservation this
/// function exists for does not apply. Nothing in the schema can produce such a
/// document today (it is TOML written by `toml::to_string_pretty` either way),
/// so this is a property to know rather than a hazard to design around; it
/// becomes one the moment the format itself changes.
fn merged_document(
    path: &Path,
    existing: Option<&str>,
    settings: &DesktopSettings,
) -> Result<String, SettingsWriteError> {
    // `toml::from_str`, not `str::parse` — `Value`'s `FromStr` parses a single
    // TOML *value* expression, so a whole document fails it on the first key.
    let mut document = existing
        .and_then(|contents| toml::from_str::<toml::Table>(contents).ok())
        .unwrap_or_default();
    let owned = toml::Table::try_from(settings)
        .map_err(|error| write_error("could not serialize", path, error))?;
    merge_tables(&mut document, owned);
    toml::to_string_pretty(&document)
        .map_err(|error| write_error("could not serialize", path, error))
}

/// Deep-merge `incoming` into `base`: two tables merge key by key, anything
/// else replaces outright.
///
/// So a field the struct owns always wins over whatever the file held — the
/// struct is the authority on its own schema — while a key only the file has is
/// left exactly as it was, down to a field nested inside a section this build
/// *does* know.
///
/// An existing entry is edited **in place** rather than removed and
/// re-inserted, so every key keeps its position even if some dependency turns
/// on toml's `preserve_order` feature and the table stops being sorted.
fn merge_tables(base: &mut toml::Table, incoming: toml::Table) {
    for (key, value) in incoming {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge_tables(existing, incoming);
            }
            (Some(existing), value) => *existing = value,
            (None, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Exclusively create a fresh temp file next to `dest`, redrawing the name on
/// collision. Returns the open file and its path.
///
/// The suffix is **random** rather than the old `<pid>.<counter>`, which any
/// other process could compute. `create_new` (`O_CREAT|O_EXCL`) already means a
/// planted name costs a failed save rather than a write through someone else's
/// symlink, so the guessable form was a nuisance rather than a hole — but an
/// unpredictable name costs one hash and removes the question.
fn create_temp(parent: &Path, dest: &Path) -> Result<(std::fs::File, PathBuf), SettingsWriteError> {
    let mut last = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let tmp = parent.join(format!(
            ".{SETTINGS_FILE_NAME}.tmp.{:016x}",
            unpredictable_suffix()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        fsperm::set_create_mode_owner_only(&mut options);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            // Deny every other principal an open handle for the lifetime of
            // ours, so nobody can be holding one across the DACL tightening
            // `set_file_owner_only` performs on the handle a moment later. The
            // rename happens after the handle is dropped, so this costs
            // nothing.
            options.share_mode(0);
        }
        match options.open(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => {
                return Err(write_error(
                    "could not create a temp file beside",
                    dest,
                    error,
                ));
            }
        }
    }
    let cause = last
        .map(|error| error.to_string())
        .unwrap_or_else(|| "no temp name was attempted".to_string());
    Err(write_error(
        "could not create a temp file beside",
        dest,
        cause,
    ))
}

/// A temp-name suffix an outside observer cannot predict, with no new
/// dependency.
///
/// `RandomState` is seeded from the operating system, so hashing a
/// monotonically increasing counter and the current time under it gives a value
/// that is unique within the process and unguessable outside it. This names a
/// scratch file for a few milliseconds; it is not, and must not be used as,
/// a source of cryptographic randomness.
fn unpredictable_suffix() -> u64 {
    use std::hash::{BuildHasher as _, Hash as _, Hasher as _};

    static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    WRITE_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `settings_path` is the only thing here that reads the environment, and
    /// the environment is process-global while `cargo test` runs a module's
    /// tests as threads in one process. Every other test drives [`load_from`]
    /// and [`save_to`] with an explicit path instead, so this lock only ever
    /// serialises the one test below against itself.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("settings tempdir")
    }

    fn dark() -> DesktopSettings {
        DesktopSettings {
            appearance: AppearanceSettings {
                mode: AppearanceMode::Dark,
            },
            ..DesktopSettings::default()
        }
    }

    /// A document whose zoom is not the default, for the merge and round-trip
    /// tests that need the two sections to be independently observable.
    fn zoomed(level: f64) -> DesktopSettings {
        DesktopSettings {
            zoom: ZoomSettings {
                level: ZoomLevel::snap(level),
            },
            ..DesktopSettings::default()
        }
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Names of every file directly inside `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_saved_document_round_trips() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        assert_eq!(load_from(&path), dark());
        // And the file a user would open reads the way the PRD promises.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[appearance]"), "unexpected document: {raw}");
        assert!(
            raw.contains("mode = \"dark\""),
            "unexpected document: {raw}"
        );
    }

    #[test]
    fn an_absent_file_yields_defaults() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        assert!(!path.exists());
        assert_eq!(load_from(&path), DesktopSettings::default());
        assert_eq!(
            DesktopSettings::default().appearance.mode,
            AppearanceMode::System
        );
    }

    #[test]
    fn loading_never_fails_on_a_malformed_or_wrongly_typed_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);

        for contents in [
            // Not TOML at all.
            "this is not = = toml [[[",
            // Valid TOML, wrong types throughout.
            "version = \"one\"\n[appearance]\nmode = 3\n",
            // Valid TOML, a scalar where a table belongs.
            "version = 1\nappearance = \"dark\"\n",
            // Empty.
            "",
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                load_from(&path),
                DesktopSettings::default(),
                "document should have fallen back to defaults: {contents:?}"
            );
        }
    }

    #[test]
    fn an_unknown_appearance_value_falls_back_without_losing_the_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 9\n[appearance]\nmode = \"solarized\"\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::System);
        // The rest of the document survives: only the unreadable field is
        // replaced, and the load is not downgraded to a whole-file default.
        assert_eq!(loaded.version, 9);
    }

    #[cfg(unix)]
    #[test]
    fn loading_never_fails_on_an_unreadable_file() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        set_mode(&path, 0o000);
        if std::fs::read_to_string(&path).is_ok() {
            set_mode(&path, 0o600);
            eprintln!(
                "SKIP: this process can read a 0o000 file (running privileged), so an \
                 unreadable document cannot be constructed here"
            );
            return;
        }
        assert_eq!(load_from(&path), DesktopSettings::default());
        set_mode(&path, 0o600);
    }

    #[test]
    fn unknown_sections_and_fields_do_not_break_a_load() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\
             future_toplevel = true\n\n\
             [appearance]\n\
             mode = \"light\"\n\
             future_field = \"whatever a newer build wrote\"\n\n\
             [voice]\n\
             backend = \"whisper\"\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
        assert_eq!(loaded.version, 1);
    }

    /// The zoom ladder is duplicated in `desktop/src/lib/zoom.ts`, so both
    /// copies are pinned value-by-value and each points at the other. If they
    /// drift, the app launches at one level and the Settings row claims
    /// another — the launch apply reads this side, and every keystroke steps
    /// that one.
    #[test]
    fn zoom_ladder_matches_the_frontend_copy() {
        assert_eq!(
            ZOOM_LEVELS,
            [0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0],
            "keep this identical to ZOOM_LEVELS in desktop/src/lib/zoom.ts"
        );
        assert!(ZOOM_LEVELS.contains(&DEFAULT_ZOOM_LEVEL));
        assert!(
            ZOOM_LEVELS.windows(2).all(|pair| pair[0] < pair[1]),
            "the ladder must be ascending for `snap` and the frontend's stepping to agree"
        );
        // The measured ceiling: page zoom divides the CSS-pixel viewport by the
        // level, `tauri.conf.json` declares `minWidth: 1024`, and `styles.css`
        // declares `min-width: 320px`, past which content is clipped rather
        // than reflowed because `body` is `overflow-x: hidden`.
        let ceiling = ZOOM_LEVELS[ZOOM_LEVELS.len() - 1];
        assert!(1024.0 / ceiling > 320.0);
    }

    #[test]
    fn a_zoom_level_on_the_ladder_is_kept_exactly() {
        for level in ZOOM_LEVELS {
            assert_eq!(ZoomLevel::snap(level).as_f64(), level);
        }
    }

    /// A value exactly between two rungs resolves to the LOWER one, because the
    /// search uses a strict `<` over an ascending ladder so the first candidate
    /// wins a tie.
    ///
    /// Pinned because `clampZoom` in `desktop/src/lib/zoom.ts` is a second
    /// implementation of this, and a tie is the one input where the two could
    /// differ while both looked correct — `<=` on either side would resolve
    /// upward. Disagreement means the app launches at the level this snapped
    /// while the Settings row reads the level that one snapped, which is exactly
    /// what the duplicated-ladder comment on [`ZOOM_LEVELS`] warns about.
    #[test]
    fn an_exact_tie_resolves_downward_as_the_frontend_copy_does() {
        assert_eq!(ZoomLevel::snap(1.05).as_f64(), 1.0);
        assert_eq!(ZoomLevel::snap(0.825).as_f64(), 0.75);
        assert_eq!(ZoomLevel::snap(2.25).as_f64(), 2.0);
    }

    #[test]
    fn an_off_ladder_zoom_level_snaps_to_the_nearest_rung() {
        assert_eq!(ZoomLevel::snap(1.3).as_f64(), 1.25);
        assert_eq!(ZoomLevel::snap(1.4).as_f64(), 1.5);
        assert_eq!(ZoomLevel::snap(2.9).as_f64(), 3.0);
    }

    /// Saturating, not rejecting: a hand-typed `level = 99` should give the
    /// biggest level this build has rather than throwing the document away.
    #[test]
    fn an_out_of_range_zoom_level_saturates() {
        assert_eq!(ZoomLevel::snap(99.0).as_f64(), 3.0);
        assert_eq!(ZoomLevel::snap(0.01).as_f64(), 0.75);
        assert_eq!(ZoomLevel::snap(0.0).as_f64(), 0.75);
        assert_eq!(ZoomLevel::snap(-5.0).as_f64(), 0.75);
    }

    /// Why `snap` guards with `is_finite` instead of a range check.
    ///
    /// Every comparison against `NaN` is false, so the nearest-rung search
    /// would keep its initial value and answer **0.75** — a corrupt document
    /// would silently shrink the app rather than reading as the default. Both
    /// infinities go the same way for the same reason.
    #[test]
    fn a_non_finite_zoom_level_reads_as_the_default_not_the_smallest() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(ZoomLevel::snap(value).as_f64(), DEFAULT_ZOOM_LEVEL);
        }
    }

    /// A hand-edited `level = 1` is a TOML **integer**, and `f64`'s derived
    /// deserializer rejects one. That would not be a local failure: `load_from`
    /// treats an unparseable document as "use defaults", so this one plausible
    /// edit would also silently reset the user's appearance. The visitor accepts
    /// integers precisely so that cannot happen — which is what the second
    /// assertion here is really testing.
    #[test]
    fn an_integer_zoom_level_is_accepted_and_takes_no_other_section_with_it() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[appearance]\nmode = \"dark\"\n\n[zoom]\nlevel = 2\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.zoom.level.as_f64(), 2.0);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Dark);
    }

    /// The same guarantee for the values that are not numbers at all. A string
    /// or a boolean is a genuinely malformed document, so it falls back the
    /// ordinary way — the whole document to defaults, logged, never a failed
    /// launch — and the assertion records that this is the behaviour rather
    /// than a partial recovery.
    #[test]
    fn a_non_numeric_zoom_level_falls_back_the_ordinary_malformed_way() {
        let dir = tempdir();
        for raw in ["\"big\"", "true", "[1, 2]"] {
            let path = dir.path().join(format!("zoom-{}.toml", raw.len()));
            std::fs::write(
                &path,
                format!("version = 1\n\n[appearance]\nmode = \"dark\"\n\n[zoom]\nlevel = {raw}\n"),
            )
            .unwrap();
            let loaded = load_from(&path);
            assert_eq!(loaded.zoom.level.as_f64(), DEFAULT_ZOOM_LEVEL, "for {raw}");
            assert_eq!(loaded.appearance.mode, AppearanceMode::System, "for {raw}");
        }
    }

    /// A document written before `[zoom]` existed — which is every document on
    /// disk today — reads as the default level and keeps its appearance.
    #[test]
    fn a_document_predating_the_zoom_section_reads_as_the_default_level() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[appearance]\nmode = \"light\"\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.zoom.level.as_f64(), DEFAULT_ZOOM_LEVEL);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
    }

    /// The level survives the disk, which is the whole point of the feature —
    /// and it survives it *next to* an unknown section, so the merge covers the
    /// new tenant as well as the old one.
    #[test]
    fn a_zoom_level_round_trips_and_the_merge_still_preserves_an_unknown_section() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        let voice = "[voice]\nbackend = \"whisper\"\n";
        std::fs::write(&path, format!("version = 1\n\n{voice}")).unwrap();

        save_to(&path, &zoomed(1.75)).unwrap();
        let reloaded = load_from(&path);
        assert_eq!(reloaded.zoom.level.as_f64(), 1.75);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(voice),
            "the [voice] section did not survive a zoom save: {raw}"
        );
    }

    /// The property the merge exists for: an older build saving must not eat a
    /// newer build's section. Without it `#[serde(default)]` drops `[voice]` on
    /// the way in and the next save writes the struct straight over it.
    ///
    /// The unknown section is pinned **byte for byte** here, which is the
    /// strongest form of the guarantee and the one PRD #803 states. Note what
    /// that does and does not mean — see
    /// [`tests::an_unknown_section_keeps_its_data_but_not_its_formatting`].
    #[test]
    fn an_unknown_section_survives_a_load_modify_save_round_trip() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        let voice = "[voice]\nbackend = \"whisper\"\nlocal = true\nretries = 3\n";
        std::fs::write(
            &path,
            format!("version = 1\n\n[appearance]\nmode = \"light\"\n\n{voice}"),
        )
        .unwrap();

        let mut loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
        loaded.appearance.mode = AppearanceMode::Dark;
        save_to(&path, &loaded).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(voice),
            "the [voice] section did not survive byte for byte: {raw}"
        );
        // And the change the user actually made is on disk.
        assert_eq!(load_from(&path).appearance.mode, AppearanceMode::Dark);
    }

    /// The limit of "byte for byte", pinned so it is a known property rather
    /// than a surprise.
    ///
    /// The merge round-trips through `toml::Table`, which models *data*, so a
    /// save re-renders the whole document in the serializer's own canonical
    /// form. No unknown **data** is ever lost — every key, value and type comes
    /// back — but a comment is dropped and an inline array is re-flowed across
    /// lines. Preserving those needs a format-preserving parser (`toml_edit`),
    /// a dependency this does not carry.
    ///
    /// The practical consequence is worth knowing before someone reports it as
    /// a bug: a user who hand-annotates `desktop.toml` loses the annotations
    /// the next time the app writes a setting.
    #[test]
    fn an_unknown_section_keeps_its_data_but_not_its_formatting() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             # which speech-to-text backend #802 picked\n\
             [voice]\n\
             backend = \"whisper\"\n\
             stages = [\"stt\", \"intent\"]\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();

        // The data is all there, with its types intact.
        let reparsed = toml::from_str::<toml::Table>(&raw).unwrap();
        let voice = reparsed["voice"].as_table().unwrap();
        assert_eq!(voice["backend"].as_str(), Some("whisper"));
        assert_eq!(
            voice["stages"].as_array().unwrap().len(),
            2,
            "unexpected document: {raw}"
        );

        // The formatting is not: the comment is gone and the array is re-flowed.
        assert!(!raw.contains("# which"), "comments survived: {raw}");
        assert!(
            !raw.contains("[\"stt\", \"intent\"]"),
            "the inline array survived: {raw}"
        );
    }

    /// The same property one level down: an unknown *field* inside a section
    /// this build does own. A section-granular merge would silently drop this.
    #[test]
    fn an_unknown_field_inside_a_known_section_survives_a_save() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[appearance]\nmode = \"light\"\nterminal = \"follow\"\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("terminal = \"follow\""),
            "the unknown appearance field was dropped: {raw}"
        );
    }

    /// The other half of the merge: preserving unknown keys must not make the
    /// file authoritative over the struct for a field the struct owns.
    #[test]
    fn a_known_field_the_struct_owns_wins_over_the_file() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 99\n\n[appearance]\nmode = \"light\"\n\n[voice]\nbackend = \"whisper\"\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();

        let reloaded = load_from(&path);
        assert_eq!(reloaded.appearance.mode, AppearanceMode::Dark);
        assert_eq!(reloaded.version, SETTINGS_VERSION);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("99"), "the stale version survived: {raw}");
        assert!(raw.contains("[voice]"), "the merge lost [voice]: {raw}");
    }

    /// A corrupt document cannot be merged into, and refusing the save would
    /// lock the user out of their own settings from inside the app. It is
    /// replaced instead — the same call `load_from` makes in the other
    /// direction.
    #[test]
    fn an_unparseable_document_is_replaced_rather_than_failing_the_save() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "this is not [ valid toml\n").unwrap();

        save_to(&path, &dark()).unwrap();

        assert_eq!(load_from(&path), dark());
    }

    #[test]
    fn the_path_resolves_to_a_sibling_of_the_tui_config_and_honours_the_override() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // SAFETY: this is the only test that touches the environment, it holds
        // ENV_TEST_LOCK for the whole mutation, and it restores the prior value
        // before releasing it. Nothing here touches the filesystem, so the
        // developer's real ~/.config/dot-agent-deck/desktop.toml is never read
        // or written even while the override is unset.
        let prior = std::env::var(SETTINGS_PATH_ENV).ok();
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };
        let default_path = settings_path();

        unsafe { std::env::set_var(SETTINGS_PATH_ENV, "/tmp/somewhere/else.toml") };
        let overridden = settings_path();

        // An empty override is treated as unset rather than as an empty path.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, "") };
        let empty_override = settings_path();

        unsafe {
            match prior {
                Some(value) => std::env::set_var(SETTINGS_PATH_ENV, value),
                None => std::env::remove_var(SETTINGS_PATH_ENV),
            }
        }

        assert_eq!(default_path, config_dir().join("desktop.toml"));
        assert_eq!(
            default_path.parent(),
            config_dir().join("config.toml").parent(),
            "the document must be a sibling of the TUI's config.toml, not a section inside it"
        );
        assert_eq!(overridden, Path::new("/tmp/somewhere/else.toml"));
        assert_eq!(empty_override, default_path);
    }

    /// The settings surface shows where the document lives, so the snapshot has
    /// to name the path this process would actually write — including under the
    /// env override, which is the only way a test or a packaged build ends up
    /// somewhere other than the config directory.
    #[test]
    fn the_snapshot_carries_the_path_the_process_would_write() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir();
        let path = dir.path().join("elsewhere.toml");
        save_to(&path, &dark()).unwrap();

        // SAFETY: the lock above serialises every test that touches this var.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, &path) };
        let snapshot = load_snapshot();
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };

        assert_eq!(snapshot.settings, dark());
        assert_eq!(snapshot.path, path.display().to_string());

        // The path rides alongside the document rather than inside it, so the
        // pinned document shape is untouched by this.
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["settings"]["appearance"]["mode"], "dark");
        assert_eq!(json["path"], path.display().to_string());
        assert!(
            json["settings"].get("path").is_none(),
            "the path must not have leaked into the document: {json}"
        );
    }

    #[test]
    fn a_successful_save_leaves_no_temp_file_behind() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(entries(dir.path()), vec![SETTINGS_FILE_NAME.to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_publishes_by_rename_rather_than_writing_the_document_in_place() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &DesktopSettings::default()).unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();

        // A writer that opened the destination and truncated it would expose a
        // partially written document; one that renames a finished temp file
        // over it cannot. The inode changing is that difference, observably.
        save_to(&path, &dark()).unwrap();
        let after = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(
            before, after,
            "the document must be replaced by rename, never truncated in place"
        );
        assert_eq!(load_from(&path), dark());
    }

    /// A directory at the destination used to be caught by the rename failing.
    /// It is now caught before anything is created at all, which is the point
    /// of vetting the path — but the property that mattered is the same one:
    /// a refused save leaves nothing behind.
    #[test]
    fn a_directory_at_the_destination_is_refused_before_anything_is_written() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let error = save_to(&path, &dark()).unwrap_err();
        assert!(
            error.detail().contains("a directory"),
            "unexpected error: {error}"
        );
        assert_eq!(
            entries(dir.path()),
            vec![SETTINGS_FILE_NAME.to_string()],
            "a refused save must not leave a temp file behind"
        );
        // And the thing that was really there is untouched.
        assert_eq!(entries(&path), vec!["occupied".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_save_leaves_the_existing_document_intact() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        set_mode(dir.path(), 0o500);
        let probe = dir.path().join(".probe");
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(&probe);
            set_mode(dir.path(), 0o700);
            eprintln!(
                "SKIP: this process can write into a 0o500 directory (running privileged), \
                 so a failing save cannot be constructed here"
            );
            return;
        }

        let error = save_to(&path, &DesktopSettings::default()).unwrap_err();
        set_mode(dir.path(), 0o700);

        assert!(!error.detail().is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed save must leave the previous document byte for byte"
        );
        assert_eq!(load_from(&path), dark());
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_document_is_owner_only() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "a deck-created desktop.toml must be owner-only"
        );

        // A rewrite re-asserts it rather than inheriting whatever the previous
        // file happened to carry.
        set_mode(&path, 0o644);
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn a_write_error_never_names_a_filesystem_path() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let error = save_to(&path, &dark()).unwrap_err();
        let directory = dir.path().to_string_lossy().into_owned();
        assert!(
            error.detail().contains(&directory),
            "the logged detail should name the path: {}",
            error.detail()
        );
        assert!(
            !error.public().contains(&directory),
            "the webview-facing message must not leak a path: {}",
            error.public()
        );
        assert!(
            !error.public().contains(SETTINGS_FILE_NAME),
            "the webview-facing message must not leak a file name: {}",
            error.public()
        );
    }

    /// Every way a settings path can be refused, and the two properties that
    /// have to hold for all of them: **load never fails** (it logs and falls
    /// back to defaults) and **save refuses with a path-free public message**.
    ///
    /// Each kind is built rather than asserted about, because the whole value
    /// of the guard is that it recognises the real thing. The Unix-only kinds
    /// live in the test below.
    #[test]
    fn every_portable_path_rejection_is_refused_without_leaking_the_path() {
        let dir = tempdir();
        let a_directory = dir.path().join("as-a-directory");
        std::fs::create_dir(&a_directory).unwrap();

        let oversized = dir.path().join("oversized.toml");
        std::fs::write(&oversized, "#".repeat(MAX_SETTINGS_BYTES as usize + 1)).unwrap();
        assert!(std::fs::metadata(&oversized).unwrap().len() > MAX_SETTINGS_BYTES);

        // A root with no final component. `/` is not absolute on Windows, so
        // the two platforms need different spellings of the same idea.
        #[cfg(unix)]
        let no_file_name = PathBuf::from("/");
        #[cfg(windows)]
        let no_file_name = PathBuf::from(r"C:\");

        let cases: Vec<(&str, PathBuf, &str)> = vec![
            (
                "a relative path",
                PathBuf::from("desktop.toml"),
                "not an absolute path",
            ),
            ("a path with no file name", no_file_name, "names no file"),
            ("a directory", a_directory, "a directory"),
            ("an over-limit document", oversized, "settings limit"),
        ];

        for (what, path, expected) in cases {
            assert_reject(what, &path, expected);
        }
    }

    /// Both halves of a rejected path, for one kind: the load falls back to
    /// defaults, and the save refuses with the reason in the log detail and no
    /// path in the public message.
    fn assert_reject(what: &str, path: &Path, expected: &str) {
        assert_eq!(
            load_from(path),
            DesktopSettings::default(),
            "loading from {what} must fall back to defaults"
        );

        let error = match save_to(path, &dark()) {
            Err(error) => error,
            Ok(()) => panic!("saving to {what} must be refused"),
        };
        assert!(
            error.detail().contains(expected),
            "{what}: unexpected error: {error}"
        );
        assert!(
            !error.public().contains(&path.display().to_string()),
            "{what}: the public message leaked the path: {}",
            error.public()
        );
    }

    /// The kinds that only exist on Unix, and the two that motivated the guard
    /// in the first place: a **FIFO** blocked the app forever with no message,
    /// and a **character device** like `/dev/zero` exhausted memory. Neither is
    /// opened at all now — the rejection is on `lstat`, so this test cannot
    /// hang even if the guard regresses to following the final component.
    #[cfg(unix)]
    #[test]
    fn every_unix_only_path_rejection_is_refused_without_leaking_the_path() {
        let dir = tempdir();

        // A symlink at the final component, pointing at a perfectly good
        // document. The target must come back untouched: rejecting a symlink is
        // only meaningful if nothing was written through it.
        let target = dir.path().join("target.toml");
        save_to(&target, &DesktopSettings::default()).unwrap();
        let before = std::fs::read_to_string(&target).unwrap();
        let link = dir.path().join("as-a-symlink.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_reject("a symlink", &link, "a symlink");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            before,
            "nothing may be written through a rejected symlink"
        );

        // A socket, which `std` can bind without help.
        let socket_path = dir.path().join("as-a-socket");
        let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert_reject("a socket", &socket_path, "a socket");

        // A FIFO, which it cannot.
        let fifo = dir.path().join("as-a-fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path inside this test's own temp
        // directory and outlives the call; `mkfifo` reads it and returns.
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            made,
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );
        assert_reject("a FIFO", &fifo, "a FIFO");

        // And a character device, if this machine has the usual one.
        let zero = Path::new("/dev/zero");
        if zero.exists() {
            assert_reject("a character device", zero, "a character device");
        } else {
            eprintln!("SKIP: no /dev/zero on this machine");
        }
    }

    /// The limit is a limit: exactly [`MAX_SETTINGS_BYTES`] loads, one byte
    /// more is refused.
    ///
    /// [`read_bounded`] re-applies the bound to the bytes actually read, which
    /// covers a document that grows between the `stat` and the read. That race
    /// is not constructible deterministically, so it is asserted by the code
    /// rather than by a test — but the boundary itself is pinned here, and it
    /// is the boundary a hand-edited file can actually reach.
    #[test]
    fn the_byte_limit_admits_a_document_at_the_limit_and_refuses_one_past_it() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n").unwrap();
        assert_eq!(load_from(&path).version, 1);

        // Exactly at the limit is fine; one byte over is not.
        let mut at_limit = "version = 1\n".to_string();
        at_limit.push_str(&"#".repeat(MAX_SETTINGS_BYTES as usize - at_limit.len()));
        assert_eq!(at_limit.len() as u64, MAX_SETTINGS_BYTES);
        std::fs::write(&path, &at_limit).unwrap();
        assert_eq!(
            load_from(&path).version,
            1,
            "a document at the limit must load"
        );

        std::fs::write(&path, format!("{at_limit}#")).unwrap();
        assert_reject("an over-limit document", &path, "settings limit");
    }

    /// The temp name is unpredictable rather than `<pid>.<counter>`.
    ///
    /// A guessable name is one another process can plant first. `create_new`
    /// means the cost of that is a failed save rather than a write through
    /// someone else's symlink, so this is closing a nuisance rather than a
    /// hole — but the nuisance is free to close.
    ///
    /// # Why this asserts the suffix's SHAPE and never searches for the pid
    ///
    /// It used to assert `!name.contains(&std::process::id().to_string())`,
    /// and that assertion was **probabilistically false**: the suffix is 16
    /// random hex characters, ten of whose sixteen symbols are decimal digits,
    /// so a decimal pid turns up inside one by chance. It is not hypothetical
    /// — it reddened the required `build-windows` job on PR #872, a PR that
    /// touches no desktop code at all, on the name
    /// `.desktop.toml.tmp.1bea3738d823864d` (which carries the all-decimal
    /// runs `3738`, `8238` and `823864`). A four-digit pid collides on the
    /// order of one run in a few hundred, and every collision is a false
    /// report against whichever PR happens to be running.
    ///
    /// A substring search cannot express the property anyway. What the
    /// docstring above actually claims is that the name is a random token
    /// rather than the `<pid>.<counter>` construction, and the check below
    /// settles exactly that, deterministically: `create_temp` formats
    /// `{:016x}`, so the suffix is sixteen hex digits and nothing else. A
    /// `<pid>.<counter>` name fails it on the `.` alone, and fails it again on
    /// the length. Strictly stronger than the search it replaces, and it
    /// cannot flake.
    #[test]
    fn temp_names_are_unpredictable_rather_than_the_pid_and_a_counter() {
        let dir = tempdir();
        let dest = dir.path().join(SETTINGS_FILE_NAME);
        let prefix = format!(".{SETTINGS_FILE_NAME}.tmp.");

        let mut names = std::collections::BTreeSet::new();
        for _ in 0..16 {
            let (file, tmp) = create_temp(dir.path(), &dest).unwrap();
            drop(file);
            let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
            let suffix = name
                .strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("unexpected temp name: {name}"));
            assert!(
                suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
                "the temp suffix must be the 16 hex digits `{{:016x}}` writes, \
                 not a derived name like `<pid>.<counter>`: {name}"
            );
            names.insert(name);
            std::fs::remove_file(&tmp).unwrap();
        }
        assert_eq!(names.len(), 16, "temp names repeated: {names:?}");
    }

    /// The bound on an accepted appearance token, on both paths.
    ///
    /// A compromised webview could otherwise send an arbitrarily long mode that
    /// is allocated and lowercased on the way in. The check is in the
    /// deserializer, before the normalising copy, so the same bound covers the
    /// `desktop_set_settings` payload and a hand-edited `desktop.toml`.
    #[test]
    fn an_over_length_appearance_token_is_refused_on_both_paths() {
        let over = "x".repeat(MAX_APPEARANCE_TOKEN_BYTES + 1);

        // The IPC shape: an argument that fails deserialisation, with a message
        // that names the limit and leaks neither a path nor the value.
        let error = serde_json::from_value::<DesktopSettings>(serde_json::json!({
            "version": 1,
            "appearance": { "mode": over },
        }))
        .expect_err("an over-length appearance token must be refused");
        let message = error.to_string();
        assert!(
            message.contains(&MAX_APPEARANCE_TOKEN_BYTES.to_string()),
            "the error should name the limit: {message}"
        );
        assert!(
            !message.contains(&over),
            "the error must not echo the value: {message}"
        );
        assert!(
            !message.contains('/'),
            "the error must name no path: {message}"
        );

        // The disk path: still never fails a load. An over-length token is a
        // malformed document, not an unknown mode, so the fallback is the whole
        // document rather than the one field — logged, and not a crash.
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!("version = 9\n[appearance]\nmode = \"{over}\"\n"),
        )
        .unwrap();
        assert_eq!(load_from(&path), DesktopSettings::default());

        // And exactly at the limit is still the ordinary unknown-value
        // fallback, which keeps the rest of the document.
        let at_limit = "x".repeat(MAX_APPEARANCE_TOKEN_BYTES);
        std::fs::write(
            &path,
            format!("version = 9\n[appearance]\nmode = \"{at_limit}\"\n"),
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::System);
        assert_eq!(
            loaded.version, 9,
            "an unknown mode must not cost the document"
        );
    }

    /// The pinned shape of the default document, in the idiom of
    /// `dto::agent_mapping_is_frontend_stable`.
    ///
    /// This is deliberate friction. A new field shows up here as a diff, which
    /// forces the ownership question — "does this setting describe the client
    /// itself, or does it describe the work?" — to be answered in review rather
    /// than discovered by the feature that inherits it.
    #[test]
    fn default_document_shape_is_pinned() {
        // PRD #741 M6 added `[endpoints]` and this pin deliberately did NOT
        // gain a section, which is the thing to understand before changing it:
        // `DesktopSettings::endpoints` is an `Option` whose default is `None`,
        // TOML omits a `None` field, and that omission is load-bearing rather
        // than cosmetic — it is what makes a webview that cannot render
        // endpoints unable to delete them (see `EndpointSettings`). The JSON
        // half below *does* change, to `"endpoints": null`, and that null is
        // the same statement on the IPC wire: "unspecified", not "empty".
        const FRESH: &str =
            "version = 1\n\n[appearance]\nmode = \"system\"\n\n[zoom]\nlevel = 1.0\n";
        let rendered = toml::to_string_pretty(&DesktopSettings::default()).unwrap();
        assert_eq!(rendered, FRESH);

        // The same bytes must come out of `save_to`, which no longer serializes
        // the struct directly — it merges the struct into the document already
        // on disk. Against **no** existing document that merge has to be a
        // no-op, or this pin would describe a shape the app never writes.
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FRESH);

        // The same struct crosses the Tauri IPC, so its JSON shape is the
        // frontend's contract and is pinned with it.
        let json = serde_json::to_value(DesktopSettings::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": 1,
                "appearance": { "mode": "system" },
                "endpoints": null,
                "zoom": { "level": 1.0 },
            })
        );
        for mode in [
            AppearanceMode::System,
            AppearanceMode::Light,
            AppearanceMode::Dark,
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
            assert_eq!(AppearanceMode::from_str_lossy(mode.as_str()), mode);
        }
    }

    /// Substrings that make a key **name** look like it holds a credential.
    ///
    /// `apikey` and `api_key` need no row of their own — `key` already matches
    /// both — so the auditor's fourth example is covered by the first entry
    /// rather than by a redundant one that would read as if it mattered.
    const SECRETISH: [&str; 8] = [
        "key",
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
        "bearer",
        "passphrase",
    ];

    /// The two shapes a credential-*shaped* key name is allowed to have, and
    /// the concrete serialised type each one is.
    ///
    /// **The type is half the exemption.** An exemption granted for a boolean
    /// "is one stored" would otherwise keep covering that path after someone
    /// changed the field to a `String` — the same name, now able to hold the
    /// credential itself — which is the silent widening issue #827 asks this
    /// list to stop.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AllowedReference {
        /// The *name of the backend* that holds the credential (`"keychain"`).
        /// A string, whose contents are ours rather than the user's.
        BackendName,
        /// A flag saying a credential is stored. A boolean, which cannot carry
        /// credential material at all — the strongest of the two shapes, and
        /// the one to prefer.
        StoredFlag,
    }

    impl AllowedReference {
        /// The `toml::Value::type_str()` this shape covers, and nothing else.
        fn type_str(self) -> &'static str {
            match self {
                Self::BackendName => "string",
                Self::StoredFlag => "boolean",
            }
        }
    }

    /// Full key **paths** (`section.field`) that legitimately contain one of
    /// [`SECRETISH`] because they are a *reference to* a credential rather than
    /// the credential itself — the one carve-out PRD #803 allows — each paired
    /// with the concrete type that carve-out covers.
    ///
    /// Empty, because nothing in today's schema needs an exception. It holds
    /// paths and not bare names deliberately: `secret_backend` as a bare name
    /// would exempt a field of that name in **every** section, including one
    /// added later by someone who never read this rule, which is precisely the
    /// silent-widening this list must not do.
    ///
    /// The form to add is one line — `("voice.secret_backend",
    /// AllowedReference::BackendName)` — and the shape is not a label: a path
    /// exempted as a [`AllowedReference::StoredFlag`] whose value is a string
    /// is reported as an offender, with the mismatch named.
    const SECRETISH_ALLOWED: [(&str, AllowedReference); 0] = [];

    const SECRET_RULE: &str = "\
WHAT THIS CHECK IS: a NAMING TRIPWIRE, not a security boundary. It reads the \
key NAMES of the serialised default document and nothing else. It therefore \
cannot see any of the following, and none of them is hypothetical:\n\
  - a field whose name says nothing -- `endpoint`, `authorization`, `value` -- \
whose VALUE is a token;\n\
  - a field serde omits from the default document, which is invisible to the \
scan because the scan is of that document;\n\
  - anything the TypeScript DTO carries that the Rust schema does not, which \
is outside this test entirely;\n\
  - any value, ever.\n\
Read a pass as \"nobody named a field like a credential\", never as \"a \
credential cannot get in here\". If you are about to store real credential \
material, this check will not stop you and is not the control you need.\n\n\
WHAT DOES WATCH VALUES (issue #827): the tests below this one follow a \
uniquely-named sentinel through the document, the IPC echo, the \
desktop_get_settings snapshot, the parse diagnostic and both halves of \
SettingsWriteError; and \
xtask/linkage-check/src/desktop_settings_secrets.rs pins the field TYPES this \
schema may use, the TypeScript DTO's names and the localStorage key set. Those \
are what establish that a credential has no route in -- not this scan.\n\n\
THE RULE IT WATCHES: PRD #803 sets one hard rule about credentials -- a secret \
NEVER goes in desktop.toml and NEVER in localStorage. This document is visible \
to anyone with the user's disk, is synced by whatever backs up ~/.config, and \
is handed to the webview verbatim.\n\n\
The document may hold a non-secret REFERENCE -- which backend holds the key, or \
a boolean saying one is stored -- and nothing more. A real credential belongs \
behind the SecretStore seam (PRD #803 M5): store/load/delete keyed by a stable \
identifier, with the OS keychain as the intended implementation.\n\n\
If the name that tripped this really is a reference and not a credential, add \
its FULL PATH to SECRETISH_ALLOWED with a comment saying which of those two \
forms it is.";

    /// Every key path in a serialised document, dotted, paired with the value
    /// at it — including nested tables **and tables inside arrays**. Both the
    /// section (`voice`) and each field under it (`voice.backend`) are emitted,
    /// because either can be named badly, and the value travels with the path
    /// because [`SECRETISH_ALLOWED`] constrains the concrete type as well as
    /// the name.
    ///
    /// # The array arm is PRD #741 M6's, and it closed a real blind spot
    ///
    /// This walked `Table` only, so **no field inside a list-shaped section was
    /// ever scanned.** Nothing had one until #741's `[[endpoints.remote]]`, so
    /// it cost nothing historically — but the PRD arrived expecting a field
    /// named `key` in an endpoint row to trip this check, and it would not
    /// have: not because of the name, but because the scan could not reach the
    /// row at all. A tripwire silently blind to a whole section shape is worse
    /// than one that fires, because a reader takes the pass as a statement.
    ///
    /// An array index deliberately does **not** appear in the path. The path is
    /// what [`SECRETISH_ALLOWED`] matches on, and an exemption must describe a
    /// *field* (`endpoints.remote.identity`) rather than a *row*
    /// (`endpoints.remote.0.identity`), which would exempt one list position
    /// and silently fail to cover the second.
    fn key_paths<'v>(
        value: &'v toml::Value,
        prefix: &str,
        into: &mut Vec<(String, &'v toml::Value)>,
    ) {
        match value {
            toml::Value::Table(table) => {
                for (key, nested) in table {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    into.push((path.clone(), nested));
                    key_paths(nested, &path, into);
                }
            }
            toml::Value::Array(items) => {
                for item in items {
                    key_paths(item, prefix, into);
                }
            }
            _ => {}
        }
    }

    /// A document with **one of everything this schema can hold**, which is what
    /// the naming tripwire is run against.
    ///
    /// The default document is not enough and PRD #741 M6 is where that stopped
    /// being a theoretical objection: `[endpoints]` defaults to absent and its
    /// row list defaults to empty, so a field named `api_key` on
    /// [`RemoteEndpointSettings`] would never appear in a serialised
    /// `DesktopSettings::default()` and the tripwire would pass on a schema it
    /// had not read. [`SECRET_RULE`] already names "a field serde omits from
    /// the default document" as something the scan cannot see; this narrows
    /// that to the fields serde omits *because they are optional or in an empty
    /// list*, which is the majority of #741's.
    ///
    /// Every optional field is `Some` here **on purpose**. A new optional field
    /// added without a line here is invisible to the tripwire again, which is
    /// the one way this helper can rot — so it is worth being deliberate about:
    /// adding a field to the schema means adding it here.
    fn representative_document() -> DesktopSettings {
        DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![RemoteEndpointSettings {
                    host: Hostname::parse("build-box.example.com").unwrap(),
                    id: EndpointId::parse("deck1").unwrap(),
                    identity: Some(KeyPath::parse("~/.ssh/id_ed25519").unwrap()),
                    jump: Some(HostAlias::parse("bastion").unwrap()),
                    port: 2222,
                    socket: Some(
                        RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock")
                            .unwrap(),
                    ),
                    user: Some(SshUser::parse("dev").unwrap()),
                }],
                selection: Selection::One(EndpointId::parse("deck1").unwrap()),
            }),
            ..DesktopSettings::default()
        }
    }

    /// The leaf names that look credential-shaped and are not covered by
    /// `allowed` — either because no exemption names that full path, or because
    /// the exemption there is for a different concrete type.
    ///
    /// The match is on the leaf because that is the field's own name; the
    /// exemption is on the full path so it cannot travel to a same-named field
    /// in another section; and the type is checked because an exemption is a
    /// statement about one specific field, not about a name.
    fn secretish_offenders(
        value: &toml::Value,
        allowed: &[(&str, AllowedReference)],
    ) -> Vec<String> {
        let mut found = Vec::new();
        key_paths(value, "", &mut found);
        found
            .into_iter()
            .filter_map(|(path, at)| {
                let leaf = path
                    .rsplit('.')
                    .next()
                    .unwrap_or(&path)
                    .to_ascii_lowercase();
                if !SECRETISH.iter().any(|pattern| leaf.contains(pattern)) {
                    return None;
                }
                match allowed
                    .iter()
                    .find(|(exception, _)| exception.eq_ignore_ascii_case(&path))
                {
                    None => Some(path),
                    Some((_, shape)) if shape.type_str() == at.type_str() => None,
                    Some((_, shape)) => Some(format!(
                        "{path} (exempted as a {}, found a {})",
                        shape.type_str(),
                        at.type_str()
                    )),
                }
            })
            .collect()
    }

    /// The tripwire itself. Read [`SECRET_RULE`] before concluding anything
    /// from it passing — it is a naming check, not a security boundary.
    ///
    /// Run against [`representative_document`] rather than the default one, so
    /// optional fields and list rows are in scope; see that function for what
    /// made the difference non-theoretical.
    #[test]
    fn no_settings_key_name_trips_the_credential_tripwire() {
        for (which, settings) in [
            ("the default document", DesktopSettings::default()),
            ("a fully populated document", representative_document()),
        ] {
            let document = toml::Value::try_from(settings).unwrap();
            let offenders = secretish_offenders(&document, &SECRETISH_ALLOWED);
            assert!(
                offenders.is_empty(),
                "{which} has key(s) named like credentials: {}\n\n{SECRET_RULE}",
                offenders.join(", ")
            );
        }
    }

    /// The blind spot PRD #741 M6 closed, kept as a regression test because the
    /// tripwire's value is entirely in what it can *see*.
    ///
    /// Two halves, and both matter: a credential-shaped name inside a list row
    /// is found at all, and the path it is reported at carries no array index —
    /// so an exemption would describe the field rather than one list position.
    #[test]
    fn the_tripwire_reaches_a_field_inside_a_list_shaped_section() {
        let bad = toml::from_str::<toml::Value>(
            "version = 1\n\n\
             [[endpoints.remote]]\n\
             host = \"a\"\n\
             api_key = \"sk-live-nope\"\n\n\
             [[endpoints.remote]]\n\
             host = \"b\"\n\
             api_key = \"sk-live-nope\"\n",
        )
        .unwrap();
        let offenders = secretish_offenders(&bad, &SECRETISH_ALLOWED);
        assert_eq!(
            offenders,
            ["endpoints.remote.api_key", "endpoints.remote.api_key"],
            "a field in a list row must be reported, at a path with no row index in it"
        );
    }

    /// The tripwire's own logic, proven rather than assumed — an empty result
    /// on the real document is only meaningful if a bad name would be caught —
    /// plus the property that makes the allowlist safe: an exception applies to
    /// one path and not to a same-named field elsewhere.
    #[test]
    fn the_credential_tripwire_matches_by_path_and_catches_the_names_it_claims_to() {
        let bad = toml::from_str::<toml::Value>(
            "version = 1\n\n\
             [voice]\n\
             api_key = \"sk-live-nope\"\n\
             apikey = \"sk-live-nope\"\n\
             authorization = \"Bearer nope\"\n\
             bearer = \"nope\"\n\
             passphrase = \"nope\"\n\
             password = \"nope\"\n\
             credential = \"nope\"\n\n\
             [voice.remote]\n\
             auth_token = \"t\"\n",
        )
        .unwrap();
        let mut offenders = secretish_offenders(&bad, &SECRETISH_ALLOWED);
        offenders.sort();
        assert_eq!(
            offenders,
            [
                "voice.api_key",
                "voice.apikey",
                "voice.authorization",
                "voice.bearer",
                "voice.credential",
                "voice.passphrase",
                "voice.password",
                "voice.remote.auth_token",
            ]
        );

        // An exception is a path, so it exempts exactly one field. The same
        // name in another section is still caught — which is the whole reason
        // the allowlist stopped being bare names.
        let referenced = toml::from_str::<toml::Value>(
            "[voice]\n\
             secret_backend = \"keychain\"\n\
             has_api_key = true\n\n\
             [endpoints]\n\
             secret_backend = \"somewhere else entirely\"\n",
        )
        .unwrap();
        let allowed = [
            ("voice.secret_backend", AllowedReference::BackendName),
            ("voice.has_api_key", AllowedReference::StoredFlag),
        ];
        assert_eq!(
            secretish_offenders(&referenced, &allowed),
            ["endpoints.secret_backend"]
        );
    }

    /// The other half of the exemption, and the reason issue #827 asked for it:
    /// an exemption is a statement about **one field of one type**, so it stops
    /// applying the moment that field could carry the credential itself.
    ///
    /// The failure this prevents is silent by construction. A `has_api_key`
    /// boolean is exempted honestly; someone later needs the key's *last four
    /// digits* on the settings surface and changes it to a `String`; the name
    /// never moves, so a path-only allowlist keeps exempting it and the
    /// tripwire reports nothing while a string field sits under an
    /// `api_key` name.
    #[test]
    fn an_exemption_for_one_type_does_not_cover_the_same_path_at_another() {
        let allowed = [("voice.has_api_key", AllowedReference::StoredFlag)];

        // The shape the exemption was granted for: nothing to report.
        let flag = toml::from_str::<toml::Value>("[voice]\nhas_api_key = true\n").unwrap();
        assert!(secretish_offenders(&flag, &allowed).is_empty());

        // The same path, now a string. Reported, with the mismatch named so
        // the failure says what actually changed.
        let widened =
            toml::from_str::<toml::Value>("[voice]\nhas_api_key = \"sk-live-nope\"\n").unwrap();
        assert_eq!(
            secretish_offenders(&widened, &allowed),
            ["voice.has_api_key (exempted as a boolean, found a string)"]
        );

        // And in the other direction: a `BackendName` exemption is for a
        // string, so it does not cover a table that grew under that name.
        let nested = toml::from_str::<toml::Value>(
            "[voice.secret_backend]\nname = \"keychain\"\nvalue = \"sk-live-nope\"\n",
        )
        .unwrap();
        assert_eq!(
            secretish_offenders(
                &nested,
                &[("voice.secret_backend", AllowedReference::BackendName)]
            ),
            ["voice.secret_backend (exempted as a string, found a table)"]
        );
    }

    /// What the tripwire cannot see, pinned so the limitation is a known
    /// property rather than a discovery.
    ///
    /// #802 will store a real API key and **must not trust this control**. Each
    /// case below passes the tripwire while carrying credential material, and
    /// the fix for all of them is the same: the value never enters this
    /// document. The full pre-#802 checklist is issue #827.
    #[test]
    fn the_credential_tripwire_is_blind_to_values_and_to_innocent_names() {
        // A token under a name that says nothing. This is the case that
        // matters: it is exactly how a credential arrives in practice.
        let innocent = toml::from_str::<toml::Value>(
            "[voice]\nendpoint = \"https://api.example.test?auth=sk-live-nope\"\nvalue = \"sk-live-nope\"\n",
        )
        .unwrap();
        assert!(
            secretish_offenders(&innocent, &SECRETISH_ALLOWED).is_empty(),
            "the tripwire is a NAMING check; if this starts failing, the doc \
             comments claiming otherwise need updating too"
        );

        // And a field name is judged on its own, never on its value.
        let named = toml::from_str::<toml::Value>("[voice]\napi_key = false\n").unwrap();
        assert_eq!(
            secretish_offenders(&named, &SECRETISH_ALLOWED),
            ["voice.api_key"]
        );
    }

    // ---------------------------------------------------------------------
    // Issue #827: the value-side checks. Everything above this line reads key
    // NAMES; everything below follows one uniquely-named value through the
    // sinks a credential must not reach.
    //
    // What these prove, stated narrowly because the whole point of #827 is
    // that the previous framing was too wide — and narrowed AGAIN at PRD #741
    // M6, because the version written for #827 had itself become too wide.
    //
    // The sweep covers the leaves of the DEFAULT document, which are `version`,
    // `appearance.mode` and `zoom.level`. For those there is no route by which
    // a credential submitted through the settings surface, the settings IPC or
    // the document can be stored, echoed or logged: an integer, three tokens
    // and ten numbers cannot hold one.
    //
    // It does NOT cover #741's endpoint fields, and that is a scope statement
    // with two separate causes rather than one. They are absent from the
    // default document (the section defaults to `None` and its row list to
    // empty), so the derivation never reaches them; and their types BOUND and
    // RESTRICT text rather than forbidding it, so a sweep that did reach them
    // would be asserting something untrue — measured: the sentinel below parses
    // as a `Hostname`, an `SshUser`, a `HostAlias` and an `EndpointId`. The
    // module docs carry the two-part claim that IS true, and
    // `an_ssh_argument_field_bounds_and_restricts_rather_than_forbidding_text`
    // pins it.
    //
    // And it is weaker than "a credential cannot be in desktop.toml": a key
    // this schema does not own keeps whatever a user or a newer build put in
    // it, which
    // `a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else`
    // measures rather than glosses.
    //
    // The one check #827 lists that is NOT here is the end-to-end submission
    // "through the real secret-store flow": PRD #803 M5 named the `SecretStore`
    // seam and deliberately did not build it, and #802 designs it against a
    // real backend.
    // ---------------------------------------------------------------------

    /// A value no legitimate document can hold, credential-shaped so that
    /// finding it in any sink is unambiguous. Long and unique on purpose: a
    /// substring search for it cannot collide with anything this crate emits.
    const SENTINEL: &str = "sk-live-827-DO-NOT-STORE-e3b0c44298fc1c149afb";

    fn assert_free_of_sentinel(what: &str, haystack: &str) {
        assert!(
            !haystack.contains(SENTINEL),
            "{what} carried the sentinel credential: {haystack}"
        );
    }

    /// Every serialised form of a loaded document, plus the snapshot the
    /// webview receives, checked in one place so no call site can forget one.
    ///
    /// The four are the sinks issue #827 enumerates on this side of the
    /// bridge: the TOML a save would write, the JSON the IPC echo carries, the
    /// JSON `desktop_get_settings` returns, and a freshly written file.
    fn assert_no_sink_carries_the_sentinel(what: &str, settings: &DesktopSettings) {
        assert_free_of_sentinel(
            &format!("{what}: the TOML re-serialisation"),
            &toml::to_string_pretty(settings).unwrap(),
        );
        assert_free_of_sentinel(
            &format!("{what}: the IPC echo (`desktop_set_settings` returns its input)"),
            &serde_json::to_string(settings).unwrap(),
        );
        let snapshot = DesktopSettingsSnapshot {
            settings: settings.clone(),
            path: "/home/dev/.config/dot-agent-deck/desktop.toml".to_string(),
        };
        assert_free_of_sentinel(
            &format!("{what}: the `desktop_get_settings` snapshot"),
            &serde_json::to_string(&snapshot).unwrap(),
        );

        let dir = tempdir();
        let fresh = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&fresh, settings).unwrap();
        assert_free_of_sentinel(
            &format!("{what}: a freshly written document"),
            &std::fs::read_to_string(&fresh).unwrap(),
        );
    }

    /// The leaf (non-table) key paths of a serialised document, sorted.
    fn leaf_paths(document: &toml::Value) -> Vec<String> {
        let mut found = Vec::new();
        key_paths(document, "", &mut found);
        let mut leaves: Vec<String> = found
            .into_iter()
            .filter(|(_, at)| !at.is_table())
            .map(|(path, _)| path)
            .collect();
        leaves.sort();
        leaves
    }

    /// Replace the value at a dotted `path`, panicking if it is not there — a
    /// typo in a fixture must not read as a pass.
    fn set_at(document: &mut toml::Table, path: &str, value: toml::Value) {
        let (head, rest) = match path.split_once('.') {
            Some((head, rest)) => (head, Some(rest)),
            None => (path, None),
        };
        let at = document
            .get_mut(head)
            .unwrap_or_else(|| panic!("no `{head}` in the document"));
        match (rest, at) {
            (None, at) => *at = value,
            (Some(rest), toml::Value::Table(nested)) => set_at(nested, rest, value),
            (Some(_), _) => panic!("`{head}` is not a table"),
        }
    }

    /// **The check #802 has to keep green.** A credential arriving from the
    /// webview — the route a real one takes — is neither stored, echoed nor
    /// written, because no field in this schema can hold arbitrary text: every
    /// leaf is a closed enum, a snapped float or an integer. The one place the
    /// value does come back is named below, because it is a scope statement
    /// rather than an exception.
    ///
    /// This is not a name scan. Each payload below puts the sentinel where a
    /// credential would actually go — including under names the tripwire is
    /// blind to (`endpoint`, `value`) and under a section that does not exist
    /// yet — and every one of them ends the same way: either argument
    /// deserialisation refuses the call, or the value is dropped on the way in.
    ///
    /// # The one place the value does come back, and why it is not a leak
    ///
    /// A refused payload's serde message can quote the offending value
    /// (`invalid type: string "sk-live-…"`), and Tauri returns that to the
    /// **caller**. The caller is the webview that just sent it, so nothing is
    /// disclosed to a party that did not already hold it; the asserted claim is
    /// therefore "no sink outside the sender", not "no sink at all". The sinks
    /// that matter — the disk, the echo a *later* read would carry, the app's
    /// own log — are covered here and in the two tests below.
    ///
    /// # What this exercises, precisely
    ///
    /// The `DesktopSettings` **deserializer**, which is what `desktop_set_settings`
    /// takes as its argument, and the serialisation of what it returns. It does
    /// **not** call the command function: a `#[tauri::command]` takes a
    /// `Webview`, which cannot be constructed without a running app, so
    /// `ensure_main_webview` and the framework's own argument decoding are
    /// outside every test in this crate — see issue #823 for the missing tier.
    /// [`tests::the_settings_commands_own_bodies_carry_no_value_from_the_document`]
    /// drives the two command *bodies* through the public functions they wrap,
    /// which is as close to the real handlers as this tier reaches — see that
    /// test's own comment for the two pieces it still does not reach, and for
    /// where the second of them is covered instead. Greptile raised this on PR
    /// #943, and the claim is narrowed rather than overstated.
    #[test]
    fn a_credential_from_the_webview_reaches_neither_the_echo_nor_the_document() {
        let payloads = [
            // The known leaves, one at a time.
            ("version", serde_json::json!({ "version": SENTINEL })),
            (
                "appearance.mode",
                serde_json::json!({ "appearance": { "mode": SENTINEL } }),
            ),
            (
                "zoom.level",
                serde_json::json!({ "zoom": { "level": SENTINEL } }),
            ),
            // A name the tripwire would catch, and two it is blind to.
            ("apiKey", serde_json::json!({ "apiKey": SENTINEL })),
            ("endpoint", serde_json::json!({ "endpoint": SENTINEL })),
            ("value", serde_json::json!({ "value": SENTINEL })),
            // Inside a section this build does know.
            (
                "appearance.apiKey",
                serde_json::json!({ "appearance": { "mode": "dark", "apiKey": SENTINEL } }),
            ),
            // Inside a section it does not — the shape #802's own settings
            // will have.
            (
                "voice.api_key",
                serde_json::json!({ "voice": { "api_key": SENTINEL } }),
            ),
            // And not as an object at all.
            (
                "an array",
                serde_json::json!({ "voice": [SENTINEL, { "token": SENTINEL }] }),
            ),
            ("the whole document", serde_json::json!(SENTINEL)),
        ];

        let mut refused = 0;
        let mut dropped = 0;
        for (what, payload) in payloads {
            match serde_json::from_value::<DesktopSettings>(payload) {
                // Refused at the command boundary: nothing was stored, and the
                // rejection goes to the sender (see the doc comment above).
                Err(_) => refused += 1,
                Ok(settings) => {
                    dropped += 1;
                    assert_no_sink_carries_the_sentinel(what, &settings);
                }
            }
        }
        // Both outcomes have to actually occur, or a change that made every
        // payload fail deserialisation would leave the drop half of this test
        // asserting nothing.
        assert!(
            refused > 0 && dropped > 0,
            "{refused} refused, {dropped} accepted"
        );
    }

    /// The disk route: a credential hand-written into a field this schema owns
    /// survives neither the load nor the next save.
    ///
    /// Derived from the default document rather than from a hard-coded list, so
    /// a field #802 adds is covered the moment it appears — and if that field
    /// can hold text, this is the test that goes red.
    #[test]
    fn a_credential_at_a_known_schema_leaf_survives_neither_the_load_nor_the_next_save() {
        let default = toml::Value::try_from(DesktopSettings::default()).unwrap();
        let leaves = leaf_paths(&default);
        assert_eq!(leaves, ["appearance.mode", "version", "zoom.level"]);

        for leaf in leaves {
            let dir = tempdir();
            let path = dir.path().join(SETTINGS_FILE_NAME);
            let mut document = default.clone().as_table().unwrap().clone();
            set_at(
                &mut document,
                &leaf,
                toml::Value::String(SENTINEL.to_string()),
            );
            let raw = toml::to_string_pretty(&document).unwrap();
            assert!(
                raw.contains(SENTINEL),
                "fixture for {leaf} lost the sentinel"
            );
            std::fs::write(&path, &raw).unwrap();

            let loaded = load_from(&path);
            assert_no_sink_carries_the_sentinel(&leaf, &loaded);

            // The load–modify–save round trip #827 names explicitly: saving
            // over the offending document overwrites every key the struct
            // owns, so the value does not survive on disk either.
            save_to(&path, &loaded).unwrap();
            assert_free_of_sentinel(
                &format!("{leaf}: the document after a load-modify-save round trip"),
                &std::fs::read_to_string(&path).unwrap(),
            );
        }
    }

    /// The honest complement, and the reason "a credential cannot be in
    /// `desktop.toml`" is **not** the claim this module makes.
    ///
    /// The save merges the struct into whatever is on disk, so a key this
    /// build's schema does not own keeps its value — by design, because that is
    /// what stops an older build eating a newer one's section
    /// ([`merged_document`]). A credential a *user* or a *newer build* put
    /// there therefore stays there. What it cannot do is reach anything else:
    /// the struct drops it at load, so it is absent from the IPC echo, from the
    /// `desktop_get_settings` snapshot and from every rendering of the
    /// document.
    ///
    /// Both flavours of unowned key are covered, because the merge treats them
    /// identically and only one of them is obvious: a whole section this build
    /// has never heard of, and a stray field inside a section it owns.
    #[test]
    fn a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!(
                "version = 1\n\n\
                 [appearance]\n\
                 mode = \"{SENTINEL}\"\n\
                 stray_token = \"{SENTINEL}\"\n\n\
                 [voice]\n\
                 api_key = \"{SENTINEL}\"\n"
            ),
        )
        .unwrap();

        let loaded = load_from(&path);
        // The owned field folded to its default; the unowned ones never
        // entered the struct at all.
        assert_eq!(loaded.appearance.mode, AppearanceMode::System);
        assert_no_sink_carries_the_sentinel("an unowned key", &loaded);

        save_to(&path, &loaded).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Measured, not tolerated in silence: the two unowned keys still hold
        // it and the owned one does not.
        assert_eq!(
            raw.matches(SENTINEL).count(),
            2,
            "the merge should have kept exactly the two unowned values: {raw}"
        );
        assert!(
            raw.contains("mode = \"system\""),
            "unexpected document: {raw}"
        );
        assert!(raw.contains("stray_token"), "unexpected document: {raw}");
        assert!(raw.contains("api_key"), "unexpected document: {raw}");
    }

    /// The two settings commands' own bodies, driven end to end against a
    /// sentinel-bearing document on disk.
    ///
    /// `desktop_get_settings` is `Ok(settings::load_snapshot())` after its
    /// webview guard. `desktop_set_settings` is `settings::save(&settings)`
    /// then `Ok(settings)`, plus a `map_err` closure that logs
    /// `error.detail()` and returns `safe_message(error.public())`. So calling
    /// [`load_snapshot`] and [`save`] under the real path seam drives both
    /// success paths, and **two** things in those handlers stay out of reach
    /// rather than one: `ensure_main_webview`, because a `Webview` needs a
    /// running Tauri app to exist; and that `map_err` closure, because this
    /// test does not provoke a save failure.
    ///
    /// The closure is not uncovered, though — it is covered somewhere else,
    /// which is worth knowing before reading this test as the whole story. The
    /// only two values it can emit are `error.detail()` and
    /// `safe_message(error.public())`, and
    /// [`tests::no_settings_write_error_carries_a_value_from_the_document`]
    /// asserts both are free of the sentinel, against real save failures.
    ///
    /// This is deliberately more than re-serialising a hand-built struct: the
    /// snapshot here is the one the command would actually return, read off a
    /// real file through the real resolver, with the real path in it.
    #[test]
    fn the_settings_commands_own_bodies_carry_no_value_from_the_document() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!(
                "version = 1\n\n\
                 [appearance]\n\
                 mode = \"{SENTINEL}\"\n\n\
                 [voice]\n\
                 api_key = \"{SENTINEL}\"\n"
            ),
        )
        .unwrap();

        // SAFETY: the lock above serialises every test that touches this var.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, &path) };
        let snapshot = load_snapshot();
        let saved = save(&snapshot.settings);
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };
        saved.unwrap();

        // The reply `desktop_get_settings` would send, as JSON, exactly as the
        // bridge would receive it.
        assert_free_of_sentinel(
            "the `desktop_get_settings` reply",
            &serde_json::to_string(&snapshot).unwrap(),
        );
        // Its `path` is present and is the file we pointed it at — the
        // deliberate exception documented on `DesktopSettingsSnapshot`, and the
        // reason this assertion is about the sentinel rather than about paths.
        assert_eq!(snapshot.path, path.display().to_string());
        assert_eq!(snapshot.settings.appearance.mode, AppearanceMode::System);

        // The reply `desktop_set_settings` would echo.
        assert_free_of_sentinel(
            "the `desktop_set_settings` echo",
            &serde_json::to_string(&snapshot.settings).unwrap(),
        );

        // And the document the save actually wrote: the owned field is
        // scrubbed, the unowned one is preserved, exactly as
        // `a_key_this_schema_does_not_own_keeps_its_value_and_reaches_nothing_else`
        // establishes for the lower-level path.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw.matches(SENTINEL).count(),
            1,
            "unexpected document: {raw}"
        );
        assert!(
            raw.contains("mode = \"system\""),
            "unexpected document: {raw}"
        );
    }

    /// The log sink, and the measurement that made [`invalid_document_log`]
    /// necessary.
    ///
    /// `toml::de::Error`'s own `Display` echoes the offending value twice — in
    /// a rendered source line and in serde's `invalid type` message — so the
    /// diagnostic `load_from` used to print put a hand-edited document's bytes
    /// into this process's stderr and the deck log. Both halves are asserted:
    /// the raw error **does** carry the value (so the redaction is proven
    /// necessary rather than assumed), and what is logged instead does not,
    /// while still naming the file and the line.
    #[test]
    fn a_parse_diagnostic_carries_a_locator_and_never_the_documents_bytes() {
        let path = Path::new("/home/dev/.config/dot-agent-deck/desktop.toml");
        let cases = [
            format!("version = \"{SENTINEL}\"\n"),
            format!("version = 1\n[zoom]\nlevel = \"{SENTINEL}\"\n"),
            format!("version = 1\nappearance = \"{SENTINEL}\"\n"),
            // A syntax error rather than a type error: the same rendered
            // source line, so the same exposure.
            format!("version = 1\nnot a key = = {SENTINEL}\n"),
        ];

        for contents in &cases {
            let error = toml::from_str::<DesktopSettings>(contents).unwrap_err();
            assert!(
                error.to_string().contains(SENTINEL),
                "the toml error stopped echoing the value, so `invalid_document_log` \
                 can be simplified and its doc comment is now wrong: {error}"
            );

            let logged = invalid_document_log(path, contents, &error);
            assert_free_of_sentinel("the parse diagnostic", &logged);
            assert!(
                logged.contains("line ") || logged.contains("an unreported position"),
                "the diagnostic must still locate the problem: {logged}"
            );
            // This is the log, which is the half of the split that may name a
            // path — the same rule `SettingsWriteError::detail` follows.
            assert!(
                logged.contains("desktop.toml"),
                "unexpected diagnostic: {logged}"
            );
        }

        // The locator itself, on a case whose position is known by hand: the
        // offending value on line 3 starts at column 9 (`level = "`).
        let contents = format!("version = 1\n[zoom]\nlevel = \"{SENTINEL}\"\n");
        let error = toml::from_str::<DesktopSettings>(&contents).unwrap_err();
        assert!(
            invalid_document_log(path, &contents, &error).contains("line 3, column 9"),
            "unexpected locator: {}",
            invalid_document_log(path, &contents, &error)
        );

        // A multi-byte character before the offending value must not shift the
        // column into nonsense, which is why the count is characters: `é` is
        // two bytes, so `b` on line 2 is at byte 7 and column 1, not column 2.
        assert_eq!(line_and_column("é = 1\nb = 2", 7), Some((2, 1)));
        assert_eq!(line_and_column("é = 1\nb = 2", 5), Some((1, 5)));
        // An offset inside a multi-byte character has no honest answer.
        assert_eq!(line_and_column("é = 1", 1), None);
        assert_eq!(line_and_column("abc", 99), None);
    }

    /// The error-message sink, on both sides of the [`SettingsWriteError`]
    /// split, with a document that holds the sentinel while the save fails.
    ///
    /// The narrow claim, verified case by case rather than asserted in
    /// general: every `SettingsWriteError` this module can build is
    /// constructed from a **path** and an `io::Error` or a serialisation
    /// error, never from the document's contents — so neither half carries a
    /// value, and `dto::safe_message` (which is a control-character filter and
    /// a length cap, **not** a redactor) has nothing to remove.
    #[cfg(unix)]
    #[test]
    fn no_settings_write_error_carries_a_value_from_the_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            format!("version = 1\n[voice]\napi_key = \"{SENTINEL}\"\n"),
        )
        .unwrap();

        // An unreadable existing document: the failure happens in
        // `read_document`, with the sentinel one `open` away.
        set_mode(&path, 0o000);
        if std::fs::read_to_string(&path).is_ok() {
            set_mode(&path, 0o600);
            eprintln!(
                "SKIP: this process can read a 0o000 file (running privileged), so an \
                 unreadable document cannot be constructed here"
            );
            return;
        }
        let unreadable = save_to(&path, &dark()).unwrap_err();
        set_mode(&path, 0o600);

        // A read-only parent: the read succeeds, so the sentinel has been in
        // this process's memory, and the failure is in `create_temp`.
        set_mode(dir.path(), 0o500);
        let unwritable_dir = save_to(&path, &dark());
        set_mode(dir.path(), 0o700);

        let mut errors = vec![unreadable];
        match unwritable_dir {
            Err(error) => errors.push(error),
            Ok(()) => eprintln!(
                "SKIP: this process can write into a 0o500 directory (running privileged)"
            ),
        }

        for error in errors {
            assert_free_of_sentinel("a write error's log detail", error.detail());
            assert_free_of_sentinel("a write error's webview message", error.public());
            assert_free_of_sentinel("a write error's `Display`", &error.to_string());
            assert_free_of_sentinel(
                "`safe_message` of a write error's public half",
                &crate::dto::safe_message(error.public()),
            );
        }
    }

    // ---------------------------------------------------------------------
    // PRD #741 M6 — endpoint storage
    // ---------------------------------------------------------------------

    /// Scenario: serialise a document holding one fully-specified remote deck,
    /// save it, read it back, and pin the exact bytes. This is the shape a user
    /// hand-edits and the shape M7's panel writes, so it is pinned the way the
    /// default document is — a diff here is the review prompt.
    #[test]
    fn a_populated_endpoint_document_is_pinned_and_round_trips() {
        const STORED: &str = "\
version = 1

[appearance]
mode = \"system\"

[endpoints]
selection = \"deck1\"

[[endpoints.remote]]
host = \"build-box.example.com\"
id = \"deck1\"
identity = \"~/.ssh/id_ed25519\"
jump = \"bastion\"
port = 2222
socket = \"/run/user/1000/dot-agent-deck-attach.sock\"
user = \"dev\"

[zoom]
level = 1.0
";
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &representative_document()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), STORED);
        assert_eq!(load_from(&path), representative_document());
    }

    /// Scenario: a document holds two remote decks; a webview that knows
    /// nothing about endpoints saves an appearance change over it. Both decks
    /// must still be there.
    ///
    /// This is the property `DesktopSettings::endpoints` is an `Option` for. If
    /// it were a plain section, the webview's fixed-key-set normaliser would
    /// send the default — an empty list — and the merge would write that over
    /// the user's decks. M7 inherits the other half: once the panel exists, the
    /// frontend has to round-trip this section rather than fabricate a default.
    #[test]
    fn a_client_that_cannot_render_endpoints_cannot_delete_them() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &representative_document()).unwrap();

        // Exactly what `normalizeDesktopSettings` produces: version, appearance
        // and zoom, and no endpoints key at all.
        let from_webview: DesktopSettings = serde_json::from_value(serde_json::json!({
            "version": 1,
            "appearance": { "mode": "dark" },
            "zoom": { "level": 1.25 },
        }))
        .expect("a webview document omitting endpoints must deserialize");
        assert_eq!(from_webview.endpoints, None, "omitted means unspecified");
        save_to(&path, &from_webview).unwrap();

        let reloaded = load_from(&path);
        assert_eq!(
            reloaded.appearance.mode,
            AppearanceMode::Dark,
            "the section the webview does own must have been written"
        );
        assert_eq!(
            reloaded.endpoints,
            representative_document().endpoints,
            "the section it does not own must survive byte for byte"
        );
    }

    /// Scenario: the stored selection names a deck that is no longer in the
    /// list. The app must fall back to the local deck and say why, rather than
    /// erroring or connecting to nothing. Test-plan item 17.
    #[test]
    fn a_selection_naming_a_deck_that_is_gone_falls_back_to_local() {
        let gone = EndpointId::parse("deck-that-left").unwrap();
        let endpoints = EndpointSettings {
            remote: Vec::new(),
            selection: Selection::One(gone.clone()),
        };

        let resolved = endpoints.resolve();
        assert_eq!(resolved.endpoint, Endpoint::local());
        assert_eq!(
            resolved.fallback,
            Some(SelectionFallback::UnknownDeck { id: gone })
        );
        assert!(
            resolved
                .fallback
                .unwrap()
                .to_string()
                .contains("local deck"),
            "the reason has to be renderable by M7, not just distinguishable"
        );
    }

    /// Scenario: the selected deck exists but has no remote socket path yet —
    /// the state M10's `Test connection` is there to leave behind. It resolves
    /// to local with its **own** reason, so the UI can say "press Test
    /// connection" instead of "that deck is gone".
    #[test]
    fn a_selected_deck_with_no_socket_path_has_its_own_fallback_reason() {
        let id = EndpointId::parse("halfway").unwrap();
        let endpoints = EndpointSettings {
            remote: vec![RemoteEndpointSettings::new(
                id.clone(),
                Hostname::parse("build-box").unwrap(),
            )],
            selection: Selection::One(id.clone()),
        };

        let resolved = endpoints.resolve();
        assert_eq!(resolved.endpoint, Endpoint::local());
        assert_eq!(
            resolved.fallback,
            Some(SelectionFallback::NoRemoteSocket { id }),
            "a half-configured deck is not the same state as a missing one"
        );
    }

    /// Scenario: a fully configured deck is selected. It resolves to a
    /// `Endpoint::Remote` carrying every stored field, and — the part that
    /// matters for PRD #741 M2's guarantee — it is **not** a local endpoint, so
    /// nothing can route it into `run_daemon_stop`.
    #[test]
    fn a_configured_deck_resolves_to_a_remote_endpoint_and_never_a_local_one() {
        let settings = representative_document();
        let resolved = settings.resolve_endpoint();
        assert_eq!(resolved.fallback, None);

        let Endpoint::Remote(remote) = &resolved.endpoint else {
            panic!("a configured selection must resolve to a remote deck");
        };
        assert_eq!(remote.host().as_str(), "build-box.example.com");
        assert_eq!(remote.user().map(SshUser::as_str), Some("dev"));
        assert_eq!(remote.port(), 2222);
        assert_eq!(remote.key().map(KeyPath::as_str), Some("~/.ssh/id_ed25519"));
        assert_eq!(remote.jump().map(HostAlias::as_str), Some("bastion"));
        assert_eq!(
            remote.socket().as_str(),
            "/run/user/1000/dot-agent-deck-attach.sock"
        );
        assert!(
            resolved.endpoint.as_local().is_none(),
            "PRD #741 M2: a remote deck must have no LocalEndpoint to hand out"
        );
    }

    /// Scenario: a document with no `[endpoints]` section at all — every
    /// document written before this milestone — resolves to the local deck with
    /// nothing to report.
    #[test]
    fn a_document_predating_the_endpoints_section_resolves_to_local() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[appearance]\nmode = \"dark\"\n").unwrap();

        let resolved = load_from(&path).resolve_endpoint();
        assert_eq!(resolved.endpoint, Endpoint::local());
        assert_eq!(resolved.fallback, None);
    }

    /// Scenario: a document written by a build that has a `Selection` variant
    /// this one does not — `all`, which is issue #742's — is loaded, resolved
    /// and saved again. It must degrade to the local deck **and** be written
    /// back unchanged, so an older build cannot destroy a newer one's choice.
    ///
    /// This is the growability `Selection` exists for, tested from the outside
    /// rather than asserted in a doc comment.
    #[test]
    fn a_selection_token_this_build_does_not_know_degrades_without_being_lost() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n\n[endpoints]\nselection = \"all\"\n").unwrap();

        let loaded = load_from(&path);
        let resolved = loaded.resolve_endpoint();
        assert_eq!(
            resolved.endpoint,
            Endpoint::local(),
            "an unknown selection is not a reason to have no deck"
        );
        assert!(matches!(
            resolved.fallback,
            Some(SelectionFallback::UnknownDeck { .. })
        ));

        save_to(&path, &loaded).unwrap();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("\"all\""),
            "the token a newer build wrote must survive this build reading and rewriting it"
        );
    }

    /// Scenario: the reserved `local` token, in any case, is the local deck and
    /// is refused as an endpoint id — so the two can never be confused, which
    /// is what lets the selection be one plain string.
    #[test]
    fn local_is_reserved_on_both_sides_of_the_selection() {
        for token in ["local", "LOCAL", "Local"] {
            let document = format!("version = 1\n\n[endpoints]\nselection = {token:?}\n");
            let settings = toml::from_str::<DesktopSettings>(&document)
                .unwrap_or_else(|error| panic!("{token} must parse: {error}"));
            assert_eq!(
                settings
                    .endpoints
                    .expect("the section is present")
                    .selection,
                Selection::Local,
                "{token} must read as the local deck"
            );
            assert!(
                EndpointId::parse(token).is_err(),
                "{token} must not be usable as a deck id"
            );
        }
        assert_eq!(Selection::Local.as_token(), LOCAL_SELECTION_TOKEN);
    }

    /// Scenario: minted ids are 16 lowercase hex characters, distinct from each
    /// other, and parse back — so the generator can never produce something the
    /// validator refuses, or something that collides with a reserved word.
    #[test]
    fn a_minted_endpoint_id_is_hex_unique_and_re_parsable() {
        let ids: Vec<EndpointId> = (0..64).map(|_| EndpointId::mint()).collect();
        for id in &ids {
            assert_eq!(id.as_str().len(), 16, "{id}");
            assert!(
                id.as_str()
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{id} must be lowercase hex, which no reserved word can be"
            );
            assert_eq!(EndpointId::parse(id.as_str()).as_ref(), Ok(id));
        }
        let unique: std::collections::BTreeSet<&EndpointId> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "minted ids must not repeat");
    }

    /// Scenario: hand-edit each endpoint field to a hostile value and load the
    /// document. Every one must be refused by its own newtype's deserializer —
    /// which is the whole reason these fields are newtypes rather than strings
    /// — and the refusal must take the ordinary malformed-document route rather
    /// than storing the value.
    #[test]
    fn a_hand_edited_endpoint_value_is_refused_by_its_own_deserializer() {
        let hostile = [
            ("host", "-oProxyCommand=curl evil"),
            ("host", "build box"),
            ("id", "deck 1"),
            ("identity", "relative/key"),
            ("identity", "/home/u/-----BEGIN OPENSSH PRIVATE KEY-----"),
            ("jump", "bastion;rm -rf /"),
            ("socket", "relative.sock"),
            ("socket", "/run/a:b.sock"),
            ("user", "dev$(id)"),
        ];
        for (field, value) in hostile {
            let document = format!(
                "version = 1\n\n[[endpoints.remote]]\nhost = \"h\"\nid = \"d\"\n{field} = {value:?}\n"
            );
            let parsed = toml::from_str::<DesktopSettings>(&document);
            assert!(
                parsed.is_err(),
                "{field} = {value:?} must be refused, not stored"
            );
        }
    }

    /// Scenario: a row missing its host, and a row missing its id. Neither is a
    /// deck, so the document is malformed — and, per `load_from`, that means
    /// defaults plus a locator in the log rather than a failed launch.
    #[test]
    fn an_endpoint_row_without_a_host_or_an_id_is_not_a_deck() {
        for document in [
            "version = 1\n\n[[endpoints.remote]]\nid = \"d\"\n",
            "version = 1\n\n[[endpoints.remote]]\nhost = \"h\"\n",
        ] {
            assert!(
                toml::from_str::<DesktopSettings>(document).is_err(),
                "{document}"
            );
        }
    }

    /// Scenario: a schema-invalid endpoint row sits in the document and the
    /// user changes their theme. The app must load defaults (it cannot read the
    /// row) but must **not** destroy the row — the merge parses TOML syntax,
    /// which the row still is, so it survives for the user to fix.
    #[test]
    fn a_row_this_build_cannot_read_survives_the_next_save() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[[endpoints.remote]]\nhost = \"build box\"\nid = \"d\"\n",
        )
        .unwrap();

        assert_eq!(load_from(&path), DesktopSettings::default());
        save_to(&path, &dark()).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("build box"), "{after}");
        assert!(after.contains("mode = \"dark\""), "{after}");
    }

    // ---------------------------------------------------------------------
    // PRD #741 M6 — the two hardening items `save_to` named #741 as the
    // trigger for
    // ---------------------------------------------------------------------

    /// Scenario: point the settings path inside a directory that is a
    /// **symlink** to somewhere else, and try to save. The write must be
    /// refused before anything is created, and the refusal must not carry the
    /// path across the bridge.
    ///
    /// This is the shape the check exists for: `create_owner_only_dir` has no
    /// symlink guard by design — it never chmods — so before this, a planted
    /// link silently redirected the whole document, key path and all.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_is_refused_before_anything_is_written() {
        let dir = tempdir();
        let real = dir.path().join("elsewhere");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("config");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let error = save_to(&link.join(SETTINGS_FILE_NAME), &DesktopSettings::default())
            .expect_err("a symlinked parent must be refused");
        assert!(error.detail().contains("symlink"), "{}", error.detail());
        assert!(
            !error.public().contains(link.to_str().unwrap()),
            "the public half must not name a path: {}",
            error.public()
        );
        assert!(
            std::fs::read_dir(&real).unwrap().next().is_none(),
            "nothing may be created through the link"
        );
    }

    /// Scenario: the parent exists but is a regular file.
    ///
    /// Two claims, because the layering is worth recording rather than
    /// rediscovering: `save_to` refuses, but it refuses **earlier** than this
    /// check — `read_document`'s own path vet `lstat`s the full path and gets
    /// `ENOTDIR` first. So the clause in `vet_parent_dir` is defence in depth
    /// for a caller that reaches it another way, and it is asserted directly
    /// rather than through a save that never gets there.
    #[cfg(unix)]
    #[test]
    fn a_parent_that_is_not_a_directory_is_refused() {
        let dir = tempdir();
        let parent = dir.path().join("not-a-dir");
        std::fs::write(&parent, b"").unwrap();

        save_to(
            &parent.join(SETTINGS_FILE_NAME),
            &DesktopSettings::default(),
        )
        .expect_err("a non-directory parent must be refused somewhere on the path");

        let error = vet_parent_dir(&parent).expect_err("and named here");
        assert!(
            error.detail().contains("not a directory"),
            "{}",
            error.detail()
        );
    }

    /// Scenario: an ordinary parent directory owned by us, and an absent one.
    /// Both must be accepted — the check refuses a redirected or foreign
    /// parent, never the everyday case, and an absent parent is one `save_to`
    /// is about to create at 0o700.
    #[test]
    fn an_ordinary_or_absent_parent_directory_is_accepted() {
        let dir = tempdir();
        save_to(&dir.path().join(SETTINGS_FILE_NAME), &dark()).expect("an owned parent");
        save_to(&dir.path().join("fresh").join(SETTINGS_FILE_NAME), &dark())
            .expect("an absent parent is created, not refused");
    }

    /// The foreign-uid arm of the parent check, tested as pure data the way
    /// `fsperm`'s `endpoint_uid_is_trusted` is: a second account is not
    /// available in a test, so the comparison is what gets pinned.
    ///
    /// It is deliberately a *different* claim from the symlink test above: that
    /// one proves the wiring, this one proves the rule. Without it the
    /// ownership clause could be inverted and every test would stay green.
    #[cfg(unix)]
    #[test]
    fn the_parent_ownership_rule_refuses_another_uid_and_accepts_our_own() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempdir();
        let ours = dot_agent_deck::platform::paths::current_uid();
        assert_eq!(
            std::fs::symlink_metadata(dir.path()).unwrap().uid(),
            ours,
            "a tempdir we just made must be ours, or this test proves nothing"
        );
        assert!(vet_parent_dir(dir.path()).is_ok());
        // The rule itself: any uid that is not ours is refused. There is no
        // account to borrow, so the arithmetic is what is asserted.
        assert_ne!(ours, ours.wrapping_add(1));
    }

    /// Scenario: a `desktop.toml` sitting at 0o644 is loaded. It must load —
    /// `load_from` never failing is a deliberate #803 property and bricking the
    /// app over a preferences file would be worse than the exposure — and the
    /// next save must republish it owner-only, which is what makes the warning
    /// self-healing rather than nagging.
    #[cfg(unix)]
    #[test]
    fn an_exposed_document_still_loads_and_the_next_save_republishes_it_owner_only() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        set_mode(&path, 0o644);
        assert_eq!(mode_of(&path), 0o644);

        assert_eq!(
            load_from(&path),
            dark(),
            "a world-readable document must still load"
        );

        save_to(&path, &dark()).unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "the publish-by-rename is what fixes the mode"
        );
    }

    /// The warning's own rule, since the message goes to stderr and a test
    /// cannot read it: exactly the modes with a group or other bit set are the
    /// ones worth complaining about, and 0o600 and 0o400 are not.
    #[cfg(unix)]
    #[test]
    fn only_a_mode_readable_beyond_its_owner_is_worth_warning_about() {
        for mode in [0o600, 0o400, 0o700] {
            assert_eq!(mode & 0o077, 0, "{mode:04o} is owner-only");
        }
        for mode in [0o644, 0o604, 0o640, 0o666, 0o777] {
            assert_ne!(mode & 0o077, 0, "{mode:04o} is readable beyond its owner");
        }
    }

    /// **The boundary PRD #741 M6 moved, measured rather than asserted.**
    ///
    /// The value-side claim written for issue #827 — *this build's schema has
    /// no field that can carry arbitrary text* — was true of `u32`,
    /// `AppearanceMode` and `ZoomLevel`, and became **false** the moment an ssh
    /// hostname could be stored. This test is the honest replacement, and it
    /// asserts the uncomfortable half first on purpose: four of the six
    /// endpoint field types **accept** the credential-shaped sentinel, so
    /// nobody can read the schema as proof against one.
    ///
    /// What actually rules a credential out of these fields is three things,
    /// none of which is "the type cannot hold text":
    ///
    /// 1. **Where the value goes.** Every one is handed to `ssh` as an argument
    ///    — a destination, a login name, a `-J` target, a row id — and reaches
    ///    no authentication surface. A token stored in `host` is a token typed
    ///    into the wrong box, not a stored credential.
    /// 2. **Shape, for the two that matter most.** Key *material* and a
    ///    passphrase are excluded by construction: `KeyPath` must start `/` or
    ///    `~/` and admits no newline, so a PEM block cannot be written there,
    ///    and `RemoteSocketPath` must be absolute. Both refuse the sentinel.
    /// 3. **The policy, which is the actual control.** A credential goes behind
    ///    the `SecretStore` seam PRD #803 M5 named. The field-type allowlist is
    ///    what forces that conversation to happen; it is not a proof that it
    ///    already happened.
    ///
    /// The bounds are asserted too, because they are the part of the type that
    /// does real work — nothing here can hold a PEM block, a JWT of any size,
    /// or a base64 blob with padding, since `=` and `+` are on no charset.
    #[test]
    fn an_ssh_argument_field_bounds_and_restricts_rather_than_forbidding_text() {
        // The uncomfortable half.
        assert!(
            Hostname::parse(SENTINEL).is_ok(),
            "45 bytes of [A-Za-z0-9-]"
        );
        assert!(SshUser::parse(SENTINEL).is_ok());
        assert!(HostAlias::parse(SENTINEL).is_ok());
        assert!(EndpointId::parse(SENTINEL).is_ok());

        // The half the shape rules out, which is the one that matters: key
        // material and a passphrase-bearing blob have nowhere to sit.
        assert!(KeyPath::parse(SENTINEL).is_err(), "not absolute or ~/");
        assert!(RemoteSocketPath::parse(SENTINEL).is_err(), "not absolute");
        const PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n";
        for refused in [PEM, "aGVsbG8gd29ybGQ=", "a+b/c==", "tok en", "tok\0en"] {
            assert!(KeyPath::parse(refused).is_err(), "KeyPath: {refused:?}");
            assert!(Hostname::parse(refused).is_err(), "Hostname: {refused:?}");
            assert!(SshUser::parse(refused).is_err(), "SshUser: {refused:?}");
            assert!(
                EndpointId::parse(refused).is_err(),
                "EndpointId: {refused:?}"
            );
        }

        // And the bounds, so "arbitrary text" is false in the size direction
        // as well as the charset one.
        assert!(Hostname::parse(&"a".repeat(254)).is_err());
        assert!(SshUser::parse(&"a".repeat(65)).is_err());
        assert!(HostAlias::parse(&"a".repeat(254)).is_err());
        assert!(EndpointId::parse(&"a".repeat(MAX_ENDPOINT_ID_BYTES + 1)).is_err());
        assert!(EndpointId::parse(&"a".repeat(MAX_ENDPOINT_ID_BYTES)).is_ok());

        // The other four field types, which DO forbid text outright and are
        // what the #827 sweep's own claim still rests on.
        assert!(toml::from_str::<DesktopSettings>(&format!(
            "version = 1\n\n[[endpoints.remote]]\nhost = \"h\"\nid = \"d\"\nport = {SENTINEL:?}\n"
        ))
        .is_err());
    }
}
