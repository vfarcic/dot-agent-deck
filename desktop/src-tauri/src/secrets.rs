//! The credential seam PRD #803 M5 named and deliberately did not build, built
//! here because PRD #802 M4 is its first consumer.
//!
//! # The rule this exists to serve
//!
//! PRD #803 sets one hard rule: **a secret never goes in `desktop.toml` and
//! never in `localStorage`.** `settings.rs`'s document may hold a non-secret
//! *reference* — which backend a key belongs to — and nothing more, and
//! `xtask/linkage-check/src/desktop_settings_secrets.rs` keeps that true by
//! refusing a `String` field in the settings schema at all. This module is the
//! other half of that: the place a real credential goes instead.
//!
//! # What does NOT cross the bridge
//!
//! **[`SecretStore::load`] has no IPC command and must not acquire one.** The
//! webview never receives a stored secret: it can ask whether one is stored
//! ([`SecretStatus`]), replace one, and forget one. `load` exists for the Rust
//! side — PRD #802 M5's intent backend and M7's transcription backend, both of
//! which make their network call in this process because the CSP leaves them
//! nowhere else to make it. A command that returned a secret to the webview
//! would put it one `JSON.stringify` away from the `localStorage` half of the
//! rule above.
//!
//! [`Secret`] carries that intent in the type: its [`fmt::Debug`] prints no
//! content, and it deliberately implements **neither** `Serialize` nor
//! `Display`, so it cannot be returned from a `#[tauri::command]` or formatted
//! into a log line without someone writing [`Secret::expose`] and being seen to
//! do it. What that does **not** give is memory hygiene — the bytes are an
//! ordinary `String` and are not zeroized on drop, and the value has already
//! been through the webview, the IPC serialiser and serde by the time it
//! arrives here, so zeroizing this one copy would be theatre rather than
//! defence.
//!
//! # Failing visibly is the requirement, not a nicety
//!
//! PRD #802's M4 brief is explicit: *a user who thinks their key is stored and
//! finds voice broken tomorrow is the outcome to design against.* So no failure
//! here is swallowed, and the three situations #803 asked to be handled — **no
//! credential store at all** (a headless Linux session, or a box with no Secret
//! Service provider), **a locked store**, and **anything else the platform
//! reports** — are separate [`SecretErrorKind`]s with different sentences,
//! because the thing to do next differs.
//!
//! The three mutating operations return a [`SecretError`] the caller has to
//! handle. [`SecretStore::status`] is the deliberate exception and takes the
//! same care by a different route: it is a *question*, so it answers one, and a
//! failure arrives as [`SecretStatus::problem`] — which a caller must not render
//! as "nothing is stored", for the reason that field's own docs give.
//!
//! [`SecretStore::status`] is the same answer asked *before* a user types
//! anything — the panel asks it on mount — which is the better moment to learn
//! that this machine has nowhere to put a key.
//!
//! # Blocking
//!
//! Every method here is synchronous and **may block**: on Linux the call is
//! D-Bus round trip to the Secret Service, which can sit for as long as an
//! unlock prompt is on screen, and macOS and Windows can prompt too. Callers on
//! an async runtime must hand it to `tauri::async_runtime::spawn_blocking`;
//! `lib.rs`'s three commands do.

use std::fmt;

use crate::dto::safe_message;

/// The keychain service every entry this app owns is filed under.
///
/// One service name for the whole app rather than one per feature: a user
/// auditing their keychain sees a single application's entries, and
/// [`SecretId`] distinguishes them by account.
pub const KEYCHAIN_SERVICE: &str = "dot-agent-deck-desktop";

/// A credential this app may hold, and the closed set of them.
///
/// An enum rather than a string for the same reason the settings section's
/// choices are enums: the account name is part of a stable on-disk (on-keychain)
/// identity, and a caller that could pass any string could strand a key under a
/// name nothing later reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SecretId {
    /// The key the keyed remote **intent** backend authenticates with
    /// (PRD #802 M5).
    VoiceIntent,
    /// The key the remote **transcription** backend authenticates with
    /// (PRD #802 M7).
    VoiceTranscription,
}

impl SecretId {
    pub const ALL: [SecretId; 2] = [SecretId::VoiceIntent, SecretId::VoiceTranscription];

    /// The keychain account name, and the token the webview names it by.
    ///
    /// Stable: changing one of these strands the key a user already stored
    /// under the old name, where nothing reads it and nothing offers to delete
    /// it. Pinned by [`tests::secret_ids_round_trip_their_stable_tokens`].
    pub fn as_str(self) -> &'static str {
        match self {
            SecretId::VoiceIntent => "voice-intent",
            SecretId::VoiceTranscription => "voice-transcription",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.as_str() == value)
    }
}

impl fmt::Display for SecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A credential's value, wrapped so it cannot be printed or serialised by
/// accident.
///
/// See the module docs for what this does and does not buy. In short: the
/// careless routes — a `{:?}`, a derived `Debug` on something holding one, a
/// `#[tauri::command]` return type, a `format!` — are closed by the type, and
/// the deliberate route ([`Secret::expose`]) is open by design because the
/// backends have to send the thing somewhere.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The credential, verbatim. Named `expose` so a call site reads as what it
    /// is in a review diff.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether this is whitespace only, which a store refuses: an empty
    /// credential stored successfully is indistinguishable from a working one
    /// until the first request fails.
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    /// Prints the length and no content, so a derived `{:?}` on a type holding
    /// one prints none either.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(<{} chars, not printed>)", self.0.chars().count())
    }
}

/// The longest credential a store here will accept.
///
/// Every API key this feature could plausibly hold is a few dozen bytes; 8 KiB
/// leaves room for a PEM-shaped credential a later backend might want while
/// keeping a compromised webview from having a megabyte allocated, copied
/// through serde and handed to a platform call before anything looks at it. The
/// same reasoning as [`MAX_APPEARANCE_TOKEN_BYTES`], and the bound is on the
/// store rather than on the command so a Rust-side caller gets it too.
///
/// [`MAX_APPEARANCE_TOKEN_BYTES`]: crate::settings::MAX_APPEARANCE_TOKEN_BYTES
pub const MAX_SECRET_BYTES: usize = 8 * 1024;

/// What every [`SecretStore::store`] refuses before it reaches a backend.
///
/// Shared by both implementations rather than written twice, so the double and
/// the keychain agree about what is storable — a double that accepted a value
/// the real store rejects would make a test prove the wrong thing.
fn vet(secret: &Secret, op: SecretOp) -> Result<(), SecretError> {
    if secret.is_blank() {
        return Err(SecretError::new(
            SecretErrorKind::Backend,
            op,
            "an empty value is not a credential",
        ));
    }
    if secret.expose().len() > MAX_SECRET_BYTES {
        return Err(SecretError::new(
            SecretErrorKind::Backend,
            op,
            format!("a credential is at most {MAX_SECRET_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// Which operation failed, so one sentence can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretOp {
    Store,
    Load,
    Delete,
}

impl SecretOp {
    /// How the sentence refers to what was being attempted.
    fn phrase(self) -> &'static str {
        match self {
            SecretOp::Store => "the key was not saved",
            SecretOp::Load => "the stored key could not be read",
            SecretOp::Delete => "the stored key was not removed",
        }
    }
}

/// Why an operation failed, in the three shapes that call for different
/// remedies.
///
/// PRD #803 named the first two specifically — *a Linux box with no Secret
/// Service needs a documented, non-silent failure path*, and a locked keychain
/// is the everyday version of the same thing. The third is the honest
/// catch-all: `keyring_core::Error` is `#[non_exhaustive]`, so a variant this
/// build has never heard of has to land somewhere, and it lands where a user is
/// told the operation did not happen rather than where it is quietly dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretErrorKind {
    /// There is no credential store on this machine at all, or it could not be
    /// initialised. On Linux that is a session with no Secret Service provider
    /// running — a headless box, or a desktop with neither GNOME Keyring nor
    /// KWallet.
    Unavailable,
    /// The store exists and refused access. Almost always a locked keychain.
    Locked,
    /// Anything else the platform reported.
    Backend,
}

/// A failed credential operation, split the way [`SettingsWriteError`] is.
///
/// [`SettingsWriteError`]: crate::settings::SettingsWriteError
///
/// [`Self::public`] is what the webview is shown and carries **fixed prose
/// only**; [`Self::detail`] adds the platform's own words and belongs in the
/// app's log. That split is `settings.rs`'s and the reasoning is the same:
/// platform error text is written for whoever is debugging and can name paths,
/// bus addresses and identifiers the user did not ask about. It is not that the
/// platform text could contain the secret — it cannot; the secret never reaches
/// the error — it is that an error is a bad channel for incidental detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError {
    kind: SecretErrorKind,
    op: SecretOp,
    /// The platform's own message, already through [`safe_message`].
    platform: String,
}

impl SecretError {
    pub fn new(kind: SecretErrorKind, op: SecretOp, platform: impl AsRef<str>) -> Self {
        Self {
            kind,
            op,
            platform: safe_message(platform),
        }
    }

    /// A complete sentence for the user, with no platform text in it.
    ///
    /// Rendered rather than stored so every caller gets the same wording, which
    /// is the same reason `voice::outcome` renders its sentences in Rust.
    pub fn public(&self) -> String {
        let what = self.op.phrase();
        match self.kind {
            SecretErrorKind::Unavailable => format!(
                "This machine has no OS credential store available, so {what}. \
                 On Linux the desktop app needs a Secret Service provider — GNOME \
                 Keyring or KWallet — running in your session."
            ),
            SecretErrorKind::Locked => format!(
                "Your OS credential store refused access, so {what}. It is \
                 usually locked; unlock it and try again."
            ),
            SecretErrorKind::Backend => {
                format!("Your OS credential store reported an error, so {what}.")
            }
        }
    }

    /// [`Self::public`] plus the platform's own message, for the app's log.
    pub fn detail(&self) -> String {
        if self.platform.is_empty() {
            self.public()
        } else {
            format!("{} ({})", self.public(), self.platform)
        }
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.public())
    }
}

impl std::error::Error for SecretError {}

/// Store, load and delete one credential, keyed by a stable identifier.
///
/// The seam, exactly as PRD #803 M5 described it. Two implementations ship:
/// [`KeychainSecretStore`] over the OS keychain, and an in-memory double the
/// tests drive — because a test that writes to the developer's real keychain is
/// a test that changes their machine and cannot be run twice with confidence.
///
/// Object-safe on purpose: PRD #802 M5's backends take a `&dyn SecretStore` so
/// a test can hand them the double, exactly as they take a `&dyn IntentResolver`.
pub trait SecretStore: Send + Sync {
    /// Replace whatever is stored under `id`.
    ///
    /// A blank secret is refused rather than stored — see [`Secret::is_blank`].
    fn store(&self, id: SecretId, secret: &Secret) -> Result<(), SecretError>;

    /// The stored credential, or `Ok(None)` when none is stored.
    ///
    /// **Absence is not an error.** "No key yet" is the ordinary first-run
    /// state and a settings panel has to be able to ask without handling an
    /// error for the common case; a real failure is still an `Err`.
    fn load(&self, id: SecretId) -> Result<Option<Secret>, SecretError>;

    /// Remove whatever is stored under `id`.
    ///
    /// **Idempotent**: deleting nothing succeeds. A user clicking *Forget* on a
    /// key that is already gone has got what they asked for.
    fn delete(&self, id: SecretId) -> Result<(), SecretError>;

    /// Whether a credential is stored, without reading it.
    ///
    /// The one question the webview may ask, and the reason it is on the trait
    /// rather than composed from [`Self::load`] at the call site: a caller that
    /// wrote `load(...).is_some()` to answer it would have the secret in hand,
    /// one line away from a command's return value.
    fn status(&self, id: SecretId) -> SecretStatus {
        match self.load(id) {
            Ok(found) => SecretStatus {
                stored: found.is_some(),
                problem: None,
            },
            Err(error) => SecretStatus {
                stored: false,
                problem: Some(error.public()),
            },
        }
    }
}

/// What the settings panel is told about one credential.
///
/// `stored` and nothing else about the value — no length, no prefix, no last
/// four characters. A masked preview is a real product pattern and it is
/// deliberately absent: it is an information channel out of the keychain whose
/// only consumer is reassurance, and PRD #803's rule is easier to keep when
/// nothing but a boolean crosses.
///
/// `problem` is `Some` when the answer is "I could not find out", which is not
/// the same as "nothing is stored" and must not render as it. A panel that
/// showed *No key stored* for an unreachable keychain would invite the user to
/// type their key again into a store that cannot hold it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SecretStatus {
    pub stored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The OS keychain: Keychain Services on macOS, the Credential Manager on
/// Windows, the Secret Service on Linux and the other unixes.
///
/// # The `keyring` crate, and which backend each platform gets
///
/// `keyring`'s default `v1` feature selects one store per target, each
/// target-gated in `keyring`'s own manifest: `apple-native-keyring-store` (the
/// Security framework) on macOS, `windows-native-keyring-store`
/// (`windows-sys`) on Windows, and `zbus-secret-service-keyring-store` on Linux
/// and the other unixes. **None of the three needs a system dev package.**
///
/// How much of that was verified, because the three are not equal: the Linux
/// one **built** here with no package added, and the Windows one **type-checked**
/// through `scripts/windows-cross-check.sh`. The macOS one is read from
/// `keyring`'s manifest and from its dependency, `security-framework` — which
/// was already in this workspace's lockfile before any of this, and links a
/// framework every macOS host has. `build-macos` is what actually proves it.
///
/// The two Linux alternatives were each worse in a stated way: the
/// `dbus-secret-service` backend binds `libdbus` and would want
/// `libdbus-1-dev` in `tauri-deps/flake.nix` and both `apt-get` blocks in
/// `ci.yml`, and `linux-keyutils` wants no package but keeps its keys in the
/// kernel, where they do not survive a reboot — which a user experiences as the
/// app forgetting their key overnight.
///
/// # Availability is answered by asking, not by a second method
///
/// `keyring` initialises its store once, on the first [`keyring::Entry::new`],
/// and remembers the result — so a machine with nowhere to put a key reports
/// [`SecretErrorKind::Unavailable`] from the very first call. That is what the
/// settings panel renders when it asks [`SecretStore::status`] on mount, which
/// is how the user learns this machine has no credential store *before* typing
/// one rather than after. A separate `availability()` would have been the same
/// answer down a second path, so there is not one.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeychainSecretStore;

impl KeychainSecretStore {
    pub fn new() -> Self {
        Self
    }

    fn entry(&self, id: SecretId, op: SecretOp) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(KEYCHAIN_SERVICE, id.as_str()).map_err(|error| classify(&error, op))
    }
}

impl SecretStore for KeychainSecretStore {
    fn store(&self, id: SecretId, secret: &Secret) -> Result<(), SecretError> {
        vet(secret, SecretOp::Store)?;
        self.entry(id, SecretOp::Store)?
            .set_password(secret.expose())
            .map_err(|error| classify(&error, SecretOp::Store))
    }

    fn load(&self, id: SecretId) -> Result<Option<Secret>, SecretError> {
        match self.entry(id, SecretOp::Load)?.get_password() {
            Ok(password) => Ok(Some(Secret::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(classify(&error, SecretOp::Load)),
        }
    }

    fn delete(&self, id: SecretId) -> Result<(), SecretError> {
        match self.entry(id, SecretOp::Delete)?.delete_credential() {
            Ok(()) => Ok(()),
            // Deleting nothing is what the caller asked for.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(classify(&error, SecretOp::Delete)),
        }
    }
}

/// Map a `keyring` error onto the three situations a user can act on.
///
/// A free function rather than a `From`, because the operation is half the
/// answer and a `From` has nowhere to put it. Pure, so it is the part of
/// [`KeychainSecretStore`] the tests can exercise without a keychain.
///
/// `keyring_core::Error` is `#[non_exhaustive]`, so the catch-all arm is
/// required by the language as well as by prudence — and it lands on
/// [`SecretErrorKind::Backend`], where the user is told the operation did not
/// happen.
fn classify(error: &keyring::Error, op: SecretOp) -> SecretError {
    let kind = match error {
        // No store could be initialised at all: the headless / no-Secret-Service
        // case PRD #803 asked for by name.
        keyring::Error::NoDefaultStore => SecretErrorKind::Unavailable,
        // The store is there and would not let us in. Usually locked.
        keyring::Error::NoStorageAccess(_) => SecretErrorKind::Locked,
        _ => SecretErrorKind::Backend,
    };
    SecretError::new(kind, op, error.to_string())
}

/// An in-memory [`SecretStore`], for tests and for nothing else.
///
/// It is the store every test below drives, because the alternative is writing
/// to the developer's own keychain — which changes their machine, can raise an
/// unlock prompt in the middle of `cargo test-fast`, and leaves state behind
/// that makes the second run of a test different from the first.
///
/// [`Self::failing`] scripts a failure, which is what makes the non-silent
/// failure path testable at all: the situations that produce one (a headless
/// box, a locked keychain) cannot be arranged from inside a test on a developer
/// machine that has a working keychain.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    entries: std::sync::Mutex<std::collections::BTreeMap<SecretId, Secret>>,
    failure: Option<(SecretErrorKind, String)>,
}

#[cfg(test)]
impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail every operation with `kind`, whatever it was.
    pub fn failing(kind: SecretErrorKind, platform: impl Into<String>) -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            failure: Some((kind, platform.into())),
        }
    }

    fn scripted(&self, op: SecretOp) -> Option<SecretError> {
        self.failure
            .as_ref()
            .map(|(kind, platform)| SecretError::new(*kind, op, platform))
    }
}

#[cfg(test)]
impl SecretStore for MemorySecretStore {
    fn store(&self, id: SecretId, secret: &Secret) -> Result<(), SecretError> {
        if let Some(error) = self.scripted(SecretOp::Store) {
            return Err(error);
        }
        vet(secret, SecretOp::Store)?;
        self.entries
            .lock()
            .expect("secret store poisoned")
            .insert(id, secret.clone());
        Ok(())
    }

    fn load(&self, id: SecretId) -> Result<Option<Secret>, SecretError> {
        if let Some(error) = self.scripted(SecretOp::Load) {
            return Err(error);
        }
        Ok(self
            .entries
            .lock()
            .expect("secret store poisoned")
            .get(&id)
            .cloned())
    }

    fn delete(&self, id: SecretId) -> Result<(), SecretError> {
        if let Some(error) = self.scripted(SecretOp::Delete) {
            return Err(error);
        }
        self.entries
            .lock()
            .expect("secret store poisoned")
            .remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The account names are an on-keychain identity: change one and the key a
    /// user already stored is stranded where nothing reads it and nothing
    /// offers to remove it.
    #[test]
    fn secret_ids_round_trip_their_stable_tokens() {
        assert_eq!(SecretId::VoiceIntent.as_str(), "voice-intent");
        assert_eq!(SecretId::VoiceTranscription.as_str(), "voice-transcription");
        for id in SecretId::ALL {
            assert_eq!(SecretId::parse(id.as_str()), Some(id));
            assert_eq!(id.to_string(), id.as_str());
        }
        assert_eq!(SecretId::parse("voice"), None);
        assert_eq!(SecretId::parse(""), None);
    }

    /// The careless routes out of a [`Secret`] are closed by the type, and the
    /// container case is the one that matters: a `{:?}` on something that
    /// *holds* a secret is how one reaches a log line by accident.
    #[test]
    fn secret_debug_prints_no_content_even_through_a_container() {
        let secret = Secret::new("sk-not-a-real-key-0123456789");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("sk-"), "Debug spilled: {rendered}");
        assert_eq!(rendered, "Secret(<28 chars, not printed>)");

        #[derive(Debug)]
        struct Holder {
            #[allow(dead_code)]
            secret: Secret,
        }
        let rendered = format!(
            "{:?}",
            Holder {
                secret: Secret::new("sk-not-a-real-key-0123456789"),
            }
        );
        assert!(
            !rendered.contains("sk-"),
            "container Debug spilled: {rendered}"
        );

        // And the deliberate route is still there, because the backends have to
        // send the thing somewhere.
        assert_eq!(secret.expose(), "sk-not-a-real-key-0123456789");
    }

    #[test]
    fn secret_is_blank_ignores_whitespace() {
        assert!(Secret::new("   \t\n").is_blank());
        assert!(Secret::new("").is_blank());
        assert!(!Secret::new(" k ").is_blank());
    }

    #[test]
    fn secret_store_round_trips_store_load_delete() {
        let store = MemorySecretStore::new();
        assert_eq!(store.load(SecretId::VoiceIntent).unwrap(), None);

        store
            .store(SecretId::VoiceIntent, &Secret::new("first"))
            .unwrap();
        assert_eq!(
            store.load(SecretId::VoiceIntent).unwrap().unwrap().expose(),
            "first"
        );

        // Storing replaces rather than accumulating.
        store
            .store(SecretId::VoiceIntent, &Secret::new("second"))
            .unwrap();
        assert_eq!(
            store.load(SecretId::VoiceIntent).unwrap().unwrap().expose(),
            "second"
        );

        // The ids are separate keys, not one shared slot.
        assert_eq!(store.load(SecretId::VoiceTranscription).unwrap(), None);

        store.delete(SecretId::VoiceIntent).unwrap();
        assert_eq!(store.load(SecretId::VoiceIntent).unwrap(), None);
        // Idempotent: deleting nothing is what the caller asked for.
        store.delete(SecretId::VoiceIntent).unwrap();
    }

    /// An empty credential stored successfully is indistinguishable from a
    /// working one until the first request fails, which is the whole failure
    /// shape this module is designed against.
    #[test]
    fn secret_store_refuses_a_blank_credential() {
        let store = MemorySecretStore::new();
        let error = store
            .store(SecretId::VoiceIntent, &Secret::new("   "))
            .expect_err("a blank value must be refused");
        assert!(error.public().contains("the key was not saved"), "{error}");
        assert_eq!(store.load(SecretId::VoiceIntent).unwrap(), None);

        // And the same for one past the bound, which is the other way a store
        // could accept something it cannot honour.
        let error = store
            .store(
                SecretId::VoiceIntent,
                &Secret::new("k".repeat(MAX_SECRET_BYTES + 1)),
            )
            .expect_err("an over-long value must be refused");
        assert!(error.detail().contains("at most"), "{}", error.detail());
        assert_eq!(store.load(SecretId::VoiceIntent).unwrap(), None);
    }

    /// The failure path, which is the half PRD #802's brief is emphatic about:
    /// it must fail visibly rather than appear to have saved a key it dropped.
    #[test]
    fn secret_store_failure_is_reported_and_stores_nothing() {
        let store = MemorySecretStore::failing(SecretErrorKind::Unavailable, "no session bus");
        let error = store
            .store(SecretId::VoiceIntent, &Secret::new("k"))
            .expect_err("a failing store must not report success");
        // The sentence names both halves: what went wrong, and what therefore
        // did not happen.
        assert!(error.public().contains("no OS credential store"), "{error}");
        assert!(error.public().contains("the key was not saved"), "{error}");
        assert!(
            error.detail().contains("no session bus"),
            "{}",
            error.detail()
        );

        // And `status` reports the problem rather than "nothing stored", which
        // would invite the user to type their key again into a store that
        // cannot hold it. Its sentence is the READ one, because that is the
        // operation `status` performed — the same situation, and a different
        // thing that did not happen.
        let status = store.status(SecretId::VoiceIntent);
        assert!(!status.stored);
        let problem = status.problem.expect("a failing read must say so");
        assert!(problem.contains("no OS credential store"), "{problem}");
        assert!(
            problem.contains("the stored key could not be read"),
            "{problem}"
        );
    }

    /// `status` is `stored` and nothing else about the value, and it is what
    /// the webview asks — so its serialised shape is a contract.
    #[test]
    fn secret_status_carries_a_boolean_and_no_value() {
        let store = MemorySecretStore::new();
        assert_eq!(
            serde_json::to_value(store.status(SecretId::VoiceIntent)).unwrap(),
            serde_json::json!({ "stored": false })
        );

        store
            .store(SecretId::VoiceIntent, &Secret::new("sk-not-a-real-key"))
            .unwrap();
        let json = serde_json::to_value(store.status(SecretId::VoiceIntent)).unwrap();
        assert_eq!(json, serde_json::json!({ "stored": true }));
        assert!(
            !json.to_string().contains("sk-"),
            "the status leaked the value: {json}"
        );
    }

    /// Each kind gets its own sentence, each names what did not happen, and
    /// none of them carries the platform's text — which [`SecretError::detail`]
    /// does, for the log.
    #[test]
    fn secret_error_sentences_name_the_situation_and_the_operation() {
        let bus = "org.freedesktop.DBus.Error.ServiceUnknown";
        let unavailable = SecretError::new(SecretErrorKind::Unavailable, SecretOp::Store, bus);
        assert!(unavailable.public().contains("the key was not saved"));
        assert!(unavailable.public().contains("Secret Service"));
        assert!(
            !unavailable.public().contains(bus),
            "the public sentence must carry no platform text: {}",
            unavailable.public()
        );
        assert!(unavailable.detail().contains(bus));

        let locked = SecretError::new(SecretErrorKind::Locked, SecretOp::Load, "locked");
        assert!(locked.public().contains("the stored key could not be read"));
        assert!(locked.public().contains("unlock"));

        let backend = SecretError::new(SecretErrorKind::Backend, SecretOp::Delete, "");
        assert!(backend.public().contains("the stored key was not removed"));
        // With no platform text there is nothing to append, so the two agree.
        assert_eq!(backend.detail(), backend.public());

        // Control characters in the platform's message are scrubbed on the way
        // in, the way every other foreign string in this crate is.
        let noisy = SecretError::new(SecretErrorKind::Backend, SecretOp::Store, "a\u{0}b");
        assert!(!noisy.detail().contains('\u{0}'), "{}", noisy.detail());
    }

    /// The `keyring` error mapping, which is the part of
    /// [`KeychainSecretStore`] that can be exercised with no keychain: the
    /// three situations a user can act on, plus the catch-all the
    /// `#[non_exhaustive]` enum requires.
    #[test]
    fn keychain_errors_map_onto_the_three_actionable_situations() {
        // Asserted through the sentence rather than through a tag, because the
        // sentence is the contract: this module exists to fail visibly, and
        // what a user reads is the visible part.
        let unavailable = classify(&keyring::Error::NoDefaultStore, SecretOp::Store);
        assert_eq!(
            unavailable.public(),
            SecretError::new(SecretErrorKind::Unavailable, SecretOp::Store, "").public()
        );
        let locked = classify(
            &keyring::Error::NoStorageAccess("locked".into()),
            SecretOp::Load,
        );
        assert_eq!(
            locked.public(),
            SecretError::new(SecretErrorKind::Locked, SecretOp::Load, "").public()
        );
        let backend = classify(
            &keyring::Error::PlatformFailure("boom".into()),
            SecretOp::Delete,
        );
        assert_eq!(
            backend.public(),
            SecretError::new(SecretErrorKind::Backend, SecretOp::Delete, "").public()
        );
        // A variant with no situation of its own lands on Backend rather than
        // being read as "nothing stored". `keyring_core::Error` is
        // `#[non_exhaustive]`, so this arm is what a variant a later version
        // adds will take.
        let unsupported = classify(
            &keyring::Error::NotSupportedByStore("nope".to_string()),
            SecretOp::Store,
        );
        assert_eq!(
            unsupported.public(),
            SecretError::new(SecretErrorKind::Backend, SecretOp::Store, "").public()
        );
        // The operation travels with the error, so the sentence names it, and
        // the platform's own words reach the log half and not the user half.
        assert!(
            classify(&keyring::Error::NoDefaultStore, SecretOp::Delete)
                .public()
                .contains("was not removed")
        );
        assert!(backend.detail().contains("boom"), "{}", backend.detail());
        assert!(!backend.public().contains("boom"), "{}", backend.public());
    }

    /// The real keychain, opt-in and skipped by default.
    ///
    /// It is the only thing that proves [`KeychainSecretStore`] talks to the
    /// platform at all, and it is off unless `DOT_AGENT_DECK_KEYCHAIN_TEST=1`
    /// because an on-by-default version would write to the developer's own
    /// keychain, could raise an unlock prompt in the middle of
    /// `cargo test-fast`, and would leave state behind that makes a second run
    /// different from the first. The `SKIP:` line is `settings.rs`'s idiom for
    /// the same situation.
    ///
    /// It cleans up after itself under a unique account so two runs cannot
    /// collide, and it probes with a `load` of a real id first: on a box with
    /// no Secret Service that is the `Unavailable` error, and skipping on it is
    /// more honest than failing a test about the round trip for a reason that
    /// has nothing to do with one.
    #[test]
    fn keychain_round_trip_when_opted_in() {
        if std::env::var("DOT_AGENT_DECK_KEYCHAIN_TEST").as_deref() != Ok("1") {
            eprintln!("SKIP: set DOT_AGENT_DECK_KEYCHAIN_TEST=1 to exercise the real OS keychain");
            return;
        }
        let store = KeychainSecretStore::new();
        if let Err(error) = store.load(SecretId::VoiceIntent) {
            eprintln!(
                "SKIP: no usable OS credential store here — {}",
                error.detail()
            );
            return;
        }

        // A unique account, so a leftover from an interrupted run cannot make
        // this test depend on the last one.
        let account = format!("selftest-{}-{}", std::process::id(), line!());
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &account).expect("entry");
        let value = "sk-not-a-real-key-0123456789";
        entry.set_password(value).expect("set");
        assert_eq!(entry.get_password().expect("get"), value);
        entry.delete_credential().expect("delete");
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }
}
