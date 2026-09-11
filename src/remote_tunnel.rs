//! PRD #741 M5 — the remote transport: an app-managed `ssh -N -L` tunnel.
//!
//! DECISION 1 landed on **1A**: shell out to the system `ssh` and let the app
//! own a long-lived `-N -L` child, rather than link an in-process SSH client.
//! The measurement behind that is in the PRD and is not re-litigated here; what
//! matters for reading this file is the consequence. A remote deck is reached
//! through a **forwarded Unix socket on this filesystem**, and that socket is
//! the transport's private implementation detail:
//!
//! 1. It never becomes a [`crate::daemon_client::LocalEndpoint`]. That type is
//!    what `run_daemon_stop` takes, and over a forwarded socket the
//!    `SO_PEERCRED` pid is the local `ssh` client's — so a "stop the daemon"
//!    that reached it would tear the tunnel down and report that it had stopped
//!    a daemon. [`RemoteTunnel::connect_address`] returns a bare `&Path` for
//!    exactly the reason [`crate::daemon_client::Endpoint::connect_address`]
//!    does, and `grep -rn 'LocalEndpoint::at' src/` is the check that keeps it
//!    true.
//! 2. The local uid+mode trust check is never run against it. A `0o600` inode
//!    owned by us says only that the local `ssh` client created it; it says
//!    nothing about who answers on the far end. A remote deck's trust story is
//!    the ssh host key and ssh user authentication, and that is the whole of it.
//! 3. Its presence is never read as health. The socket exists because *we*
//!    created the tunnel, so `Path::exists` would answer "yes" for a tunnel
//!    whose far-end daemon died an hour ago. [`RemoteTunnel::health`] asks the
//!    **child process**, which is the only thing here that can go away on its
//!    own; and every socket name is unique per [`RemoteTunnel::open`], so no run
//!    ever adopts a socket some earlier run left behind.
//!
//! # Layout
//!
//! - **Validated ssh arguments** — [`Hostname`], [`SshUser`], [`KeyPath`],
//!   [`HostAlias`], [`RemoteSocketPath`]. Pure, cross-platform, and the shape
//!   [`crate::daemon_client::RemoteEndpoint`] is built from.
//! - **[`SshProgram`]** — the `ssh` binary, resolved by absolute path.
//! - **The tunnel** — Unix only, for the reason DECISION 1A already states:
//!   `-L` with a Unix socket on both ends has no Windows equivalent. The gap is
//!   a **compile-time absence** rather than a runtime refusal: there is no
//!   `RemoteTunnel` on Windows and no error variant standing in for one, because
//!   a variant nothing can construct is a claim the code does not keep. The
//!   deferred `proxy-stdio` option (1B) is the Windows answer and is separate
//!   work; until it lands, a Windows caller cannot reach a remote deck and
//!   cannot be told so by this module.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Validated ssh arguments
// ---------------------------------------------------------------------------

/// Shell metacharacters refused in **every** ssh argument this module accepts.
///
/// # Why this exists when `--` and `Command::arg` already protect the argv
///
/// They protect a different thing. `--` stops a value beginning with `-` being
/// read as an *option*, and `Command::arg` puts the value in `argv` without a
/// shell in between — both existing ssh call sites already do this correctly
/// ([`crate::remote::SystemSshExecutor::build_command`],
/// [`crate::connect::build_connect_command`]). Neither stops the value reaching
/// the user's own `~/.ssh/config`, where `ProxyCommand` and `LocalCommand`
/// interpolate `%h` (host) and `%r` (remote user) into a string that OpenSSH
/// then hands to a **local shell**. CVE-2023-51385 is that class, fixed in
/// OpenSSH 9.6 — and a bundled desktop app runs against whatever OpenSSH the
/// user has, which is not a version we choose. Refusing the bytes at the
/// boundary is independent of their OpenSSH version, which is the only property
/// available to us.
///
/// This list is belt-and-braces rather than the primary defence: every type
/// below validates against a **positive** charset, so a metacharacter is
/// already excluded by not being on the allow list. What the explicit list buys
/// is a *named* refusal — "shell metacharacter `$`" rather than "character not
/// allowed" — which is the difference between a user fixing their input and a
/// user filing a bug.
const SHELL_METACHARACTERS: &[u8] = br#"`$;&|<>()'"\"#;

/// Conservative upper bound on a Unix-domain socket path, in bytes.
///
/// `sun_path` is 108 bytes on Linux and **104** on macOS/BSD, in both cases
/// including the terminating NUL. 104 is the smaller of the two and is applied
/// to both ends of the forward: the local side because that is the address this
/// machine binds, and the remote side because we do not know the far host's OS
/// and a refusal here is worth far more than a `bind` failure buried in ssh's
/// stderr twenty seconds later.
pub const MAX_UNIX_SOCKET_PATH_BYTES: usize = 104;

/// Why a stored ssh argument was refused.
///
/// Every variant names the field, so a settings form can point at the row that
/// is wrong. No variant echoes a non-printable byte back at the reader: a
/// control byte or a non-ASCII byte is reported in hex, and only bytes that are
/// printable ASCII are quoted verbatim. An error message about hostile input
/// that renders the hostile input is not an improvement.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SshArgumentError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} must not start with '-': ssh would read it as an option rather than a value")]
    LeadingDash { field: &'static str },
    #[error("{field} is {actual} bytes, over the {max}-byte limit")]
    TooLong {
        field: &'static str,
        actual: usize,
        max: usize,
    },
    #[error("{field} contains {what} at byte {offset}")]
    ForbiddenByte {
        field: &'static str,
        offset: usize,
        what: String,
    },
    #[error("{field} must be an absolute path starting with '/'")]
    NotAbsolute { field: &'static str },
    #[error("{field} must be an absolute path ('/…') or a home-relative path ('~/…')")]
    NotAbsoluteOrTilde { field: &'static str },
    #[error("{field} opens with '[' so it must be a bracketed IPv6 literal ending in ']'")]
    UnclosedBracket { field: &'static str },
    #[error("{field} may only use ':' or '%' inside a bracketed IPv6 literal")]
    BareIpv6Separator { field: &'static str },
}

/// How one argument type is validated. Implemented per newtype so the rules are
/// readable as data rather than buried in a parser.
trait SshArgumentRules {
    /// The name this field is called in a refusal. Reads as a noun phrase.
    const FIELD: &'static str;
    /// Byte bound, applied before the value is scanned.
    const MAX_BYTES: usize;
    /// The **positive** charset. Everything not named here is refused.
    fn byte_allowed(byte: u8) -> bool;
    /// Structural rules the charset cannot express.
    fn check_shape(_value: &str) -> Result<(), SshArgumentError> {
        Ok(())
    }
}

/// Describe one refused byte without letting it reach the reader's terminal.
fn describe_byte(byte: u8) -> String {
    match byte {
        0 => "a NUL byte".to_string(),
        b if b.is_ascii_whitespace() => format!("ASCII whitespace (0x{b:02x})"),
        b if b.is_ascii_control() || b == 0x7f => format!("a control byte (0x{b:02x})"),
        b if !b.is_ascii() => format!("a non-ASCII byte (0x{b:02x})"),
        b if SHELL_METACHARACTERS.contains(&b) => {
            format!("the shell metacharacter '{}'", b as char)
        }
        b => format!(
            "the character '{}', which this field does not accept",
            b as char
        ),
    }
}

/// The whole validation, in the order the checks must run.
///
/// Length is checked **before** the byte scan so a pathological value costs a
/// comparison rather than a walk; the leading-`-` check is separate from the
/// charset because `-` is legal *inside* a hostname and illegal at the front,
/// and collapsing the two would either reject `build-box` or accept
/// `-oProxyCommand=…`.
fn validate<R: SshArgumentRules>(raw: &str) -> Result<(), SshArgumentError> {
    if raw.is_empty() {
        return Err(SshArgumentError::Empty { field: R::FIELD });
    }
    if raw.len() > R::MAX_BYTES {
        return Err(SshArgumentError::TooLong {
            field: R::FIELD,
            actual: raw.len(),
            max: R::MAX_BYTES,
        });
    }
    if raw.starts_with('-') {
        return Err(SshArgumentError::LeadingDash { field: R::FIELD });
    }
    for (offset, byte) in raw.bytes().enumerate() {
        // The universal refusals come first so they produce the *specific*
        // message even for a type whose charset would have refused the byte
        // anyway.
        //
        // Two of these terms are **redundant and kept for legibility**, which is
        // worth saying rather than implying each pulls its weight: Rust's
        // `is_ascii_control` is `0x00..=0x1F | 0x7F`, so it already covers both
        // `byte == 0` and `byte == 0x7f`. Measured — deleting the `byte == 0`
        // term changes no test's outcome. What NUL does have of its own is the
        // message: `describe_byte` names it, because a NUL is the byte that
        // truncates a C string rather than merely looking odd in a log.
        let universally_forbidden = byte == 0
            || !byte.is_ascii()
            || byte.is_ascii_control()
            || byte == 0x7f
            || byte.is_ascii_whitespace()
            || SHELL_METACHARACTERS.contains(&byte);
        if universally_forbidden || !R::byte_allowed(byte) {
            return Err(SshArgumentError::ForbiddenByte {
                field: R::FIELD,
                offset,
                what: describe_byte(byte),
            });
        }
    }
    R::check_shape(raw)
}

/// Generate one validating newtype over `String`.
///
/// Each expands to the same five surfaces: [`parse`](Hostname::parse), an
/// `as_str`, `Display`, `Serialize` and a `Deserialize` that runs the same
/// validation the constructor does — so a hand-edited `desktop.toml` cannot
/// smuggle past a check that a settings form applies (PRD #741 M6).
macro_rules! ssh_argument {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate `raw` and wrap it. The only constructor: there is no
            /// `From<String>`, deliberately, so no call site can skip the check
            /// by reaching for a cheaper conversion.
            pub fn parse(raw: &str) -> Result<Self, SshArgumentError> {
                validate::<Self>(raw)?;
                Ok(Self(raw.to_string()))
            }

            /// The validated value.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            /// Deserialize through `String` and then validate.
            ///
            /// The intermediate `String` is unbounded at this layer, which is
            /// fine **because of where this is read from**: PRD #803's
            /// `read_document` caps the settings file at 256 KiB before any
            /// deserializer sees a byte. It is not fine anywhere that bound is
            /// absent, so do not reuse these types to parse a stream.
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

ssh_argument! {
    /// The host `ssh` connects to: a DNS name, an IPv4 literal, or a
    /// **bracketed** IPv6 literal (`[2001:db8::1]`).
    ///
    /// Brackets are required for IPv6 because `-L local:remote` and the
    /// `host:port` forms ssh accepts both use `:` as a separator; an unbracketed
    /// `::1` is ambiguous to ssh's own parser, not only to ours.
    Hostname
}

ssh_argument! {
    /// The remote login name.
    ///
    /// `@` is accepted because a UPN-style login (`user@realm`) is ordinary on
    /// Kerberos and cloud-managed hosts. `\` is **not**, so `DOMAIN\user` is
    /// refused: it is a shell metacharacter, and the Windows-domain spelling is
    /// not what OpenSSH wants on the `user@host` destination anyway.
    SshUser
}

ssh_argument! {
    /// Path to a private key file, for `ssh -i`.
    ///
    /// A key **path**, never key material and never a passphrase — the storage
    /// policy PRD #741 inherits from `remotes.toml`. `~` is accepted because
    /// OpenSSH tilde-expands `-i` itself; it is not passed through a shell.
    ///
    /// Whitespace is refused, so a key under a directory with a space in its
    /// name cannot be named here. That is a real limitation and it is the
    /// deliberate side of the trade: a space is the first thing that goes wrong
    /// when a value is interpolated into a `ProxyCommand`.
    KeyPath
}

ssh_argument! {
    /// A `Host` block name from the user's `~/.ssh/config`, used for
    /// `ssh -J <alias>`.
    ///
    /// A *name*, so the jump host's own address, port, key and user stay in the
    /// user's ssh config where they already are. Wildcards are not accepted:
    /// `*` and `?` are not on the charset, because a jump *target* is one host.
    HostAlias
}

ssh_argument! {
    /// The daemon's attach socket path **on the remote host**.
    ///
    /// Required rather than derived, and the reason is OpenSSH's: the remote
    /// side of `-L` is a literal, with no `~` expansion and no environment
    /// substitution, so `$XDG_RUNTIME_DIR/dot-agent-deck-attach.sock` cannot be
    /// written here and cannot be resolved locally either — the far host's
    /// `XDG_RUNTIME_DIR` and uid are not knowable from this side without a
    /// second ssh round trip. Learning it by probing is PRD #741 M10's job
    /// (`Test connection`), which is already making a round trip.
    ///
    /// `:` is not on the charset, and that is load-bearing rather than tidy:
    /// `-L` is parsed by splitting on `:`, so a colon here would silently
    /// re-interpret the forward spec.
    RemoteSocketPath
}

impl SshArgumentRules for Hostname {
    const FIELD: &'static str = "the host";
    /// RFC 1035's 253-byte presentation limit for a fully qualified name, which
    /// also comfortably clears every IP literal form.
    const MAX_BYTES: usize = 253;
    fn byte_allowed(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'.' | b'-' | b'_' | b'[' | b']' | b':' | b'%')
    }
    fn check_shape(value: &str) -> Result<(), SshArgumentError> {
        let bracketed = value.starts_with('[');
        if bracketed {
            if !value.ends_with(']') || value.len() < 3 {
                return Err(SshArgumentError::UnclosedBracket { field: Self::FIELD });
            }
            let inner = &value[1..value.len() - 1];
            if inner.contains('[') || inner.contains(']') {
                return Err(SshArgumentError::UnclosedBracket { field: Self::FIELD });
            }
            return Ok(());
        }
        // Outside brackets `:` would be read as a port separator and `%` as an
        // IPv6 zone id; neither is meaningful for a DNS name or an IPv4
        // literal, and both are the shape that makes a forward spec ambiguous.
        if value.contains(':') || value.contains('%') || value.contains(']') {
            return Err(SshArgumentError::BareIpv6Separator { field: Self::FIELD });
        }
        Ok(())
    }
}

impl SshArgumentRules for SshUser {
    const FIELD: &'static str = "the ssh user";
    /// Comfortably past POSIX's 32-character `LOGIN_NAME_MAX` convention, which
    /// UPN-style logins routinely exceed, and far short of anything that could
    /// hold a credential-shaped blob.
    const MAX_BYTES: usize = 64;
    fn byte_allowed(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@')
    }
}

impl SshArgumentRules for KeyPath {
    const FIELD: &'static str = "the key path";
    /// Linux's `PATH_MAX`. The bound is about refusing a pathological value,
    /// not about pretending a path is short.
    const MAX_BYTES: usize = 4096;
    fn byte_allowed(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'/' | b'~')
    }
    fn check_shape(value: &str) -> Result<(), SshArgumentError> {
        // A relative key path resolves against the process's working
        // directory, and a GUI launched from Finder or a `.desktop` entry has
        // one nobody chose. Refusing it is better than silently reading a
        // different file than the user meant.
        if value.starts_with('/') || value.starts_with("~/") {
            Ok(())
        } else {
            Err(SshArgumentError::NotAbsoluteOrTilde { field: Self::FIELD })
        }
    }
}

impl SshArgumentRules for HostAlias {
    const FIELD: &'static str = "the jump host";
    const MAX_BYTES: usize = 253;
    fn byte_allowed(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
    }
}

impl SshArgumentRules for RemoteSocketPath {
    const FIELD: &'static str = "the remote socket path";
    const MAX_BYTES: usize = MAX_UNIX_SOCKET_PATH_BYTES;
    fn byte_allowed(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'/')
    }
    fn check_shape(value: &str) -> Result<(), SshArgumentError> {
        if value.starts_with('/') {
            Ok(())
        } else {
            Err(SshArgumentError::NotAbsolute { field: Self::FIELD })
        }
    }
}

impl KeyPath {
    /// The value as a path, for `Command::arg`. Safe because the charset is
    /// validated ASCII.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

// ---------------------------------------------------------------------------
// The `ssh` program itself
// ---------------------------------------------------------------------------

/// Absolute paths `ssh` is looked for at, in order.
///
/// # Why not `PATH`
///
/// A desktop app launched from Finder or a `.desktop` entry inherits launchd's
/// or the session manager's environment, not a login shell's — which is the
/// same problem [`crate::login_shell`] exists for, approached from the other
/// side. Worse than a *short* `PATH` is a *hostile* one: any user-writable
/// directory earlier in it substitutes a binary that this app would then run
/// with the user's ssh agent and keys in reach. A fixed list of
/// root-owned system locations has neither failure mode.
///
/// Note what is deliberately absent: `~/.nix-profile/bin`, `~/.local/bin` and
/// every other per-user prefix. They are the *common* place for a
/// user-installed ssh and they are exactly the directories the previous
/// paragraph refuses to trust. A user whose ssh lives only there needs a
/// deliberate, stored choice rather than an ambient one — PRD #741 M6's
/// settings document is where that belongs, because a value in `desktop.toml`
/// is something the user chose once and can see, and an environment variable
/// inherited by a GUI is neither.
#[cfg(unix)]
pub const SSH_PROGRAM_CANDIDATES: &[&str] = &[
    "/usr/bin/ssh",
    "/bin/ssh",
    "/usr/local/bin/ssh",
    // macOS, Homebrew on Apple silicon and on Intel respectively.
    "/opt/homebrew/bin/ssh",
    "/opt/local/bin/ssh",
    // NixOS: the system profile, which is root-owned and not the user's.
    "/run/current-system/sw/bin/ssh",
];

/// Windows locations for the bundled OpenSSH client. Present so the resolution
/// code compiles and is tested on every platform; the tunnel itself is Unix
/// only (see the module docs).
#[cfg(windows)]
pub const SSH_PROGRAM_CANDIDATES: &[&str] = &[
    r"C:\Windows\System32\OpenSSH\ssh.exe",
    r"C:\Program Files\OpenSSH\ssh.exe",
];

/// A resolved, absolute path to the `ssh` program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProgram(std::path::PathBuf);

impl SshProgram {
    /// Find `ssh` among [`SSH_PROGRAM_CANDIDATES`].
    ///
    /// Fails with [`TunnelError::SshNotFound`] rather than letting the absence
    /// surface later as a connection timeout — a slim container image with no
    /// OpenSSH client is the realistic case, and "no ssh on this machine" and
    /// "that host is unreachable" call for completely different actions from
    /// the user.
    pub fn resolve() -> Result<Self, TunnelError> {
        Self::resolve_among(SSH_PROGRAM_CANDIDATES, &is_executable_file)
    }

    /// [`Self::resolve`] over an explicit candidate list and an explicit
    /// predicate. The whole of the resolution policy lives here as pure data so
    /// a test can drive it without a filesystem that happens to have ssh in the
    /// right place.
    pub fn resolve_among(
        candidates: &[&str],
        is_usable: &dyn Fn(&Path) -> bool,
    ) -> Result<Self, TunnelError> {
        for candidate in candidates {
            let path = Path::new(candidate);
            if is_usable(path) {
                return Ok(Self(path.to_path_buf()));
            }
        }
        Err(TunnelError::SshNotFound {
            searched: candidates.join(", "),
        })
    }

    /// An explicitly chosen `ssh`, which must be absolute.
    ///
    /// The escape hatch, and named as one: nothing verifies that `path` is
    /// OpenSSH. It exists for a stored settings value (M6) and for tests that
    /// point the tunnel at a stand-in script. A relative path is refused
    /// because it would resolve against a working directory a GUI never chose.
    pub fn at(path: impl Into<std::path::PathBuf>) -> Result<Self, TunnelError> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(TunnelError::SshPathNotAbsolute {
                path: path.to_string_lossy().into_owned(),
            });
        }
        Ok(Self(path))
    }

    /// The resolved path.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Whether `path` is a file this process could execute.
///
/// `metadata` rather than `symlink_metadata`, deliberately: `/usr/bin/ssh` is a
/// symlink into the Nix store on NixOS and into `Cellar` under Homebrew, and
/// refusing to follow it would refuse the two systems this list exists to
/// cover. What matters is that the *link* sits in a root-owned directory, which
/// is what choosing the directory bought.
fn is_executable_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a remote deck could not be reached.
///
/// Each variant is a *distinct named state* rather than one "failed", which is
/// what PRD #741 M10's `Test connection` renders and what M7's panel needs in
/// order to say something actionable.
#[derive(Debug, Error)]
pub enum TunnelError {
    #[error(
        "no `ssh` program was found at any of the standard locations ({searched}). Install an \
         OpenSSH client, or name its absolute path in the deck's settings."
    )]
    SshNotFound { searched: String },
    #[error("the ssh program path {path} is not absolute")]
    SshPathNotAbsolute { path: String },
    #[error("could not prepare the private directory {dir} for the forwarded socket: {source}")]
    SocketDir {
        dir: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the forwarded socket path {path} is {actual} bytes, over the {max}-byte limit the \
         operating system imposes on a Unix socket address"
    )]
    SocketPathTooLong {
        path: String,
        actual: usize,
        max: usize,
    },
    #[error(
        "the forwarded socket path {path} contains ':', which ssh reads as the separator inside \
         its -L forward specification"
    )]
    SocketPathHasColon { path: String },
    #[error(
        "refusing to use {path} for the forwarded socket: it is a symlink, and replacing it would \
         act on whatever it points at"
    )]
    SocketPathIsSymlink { path: String },
    #[error("could not clear the stale forwarded socket at {path}: {source}")]
    SocketPathNotClearable {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the ssh tunnel to {deck} exited before the forward was ready: {source}")]
    Ssh {
        deck: String,
        #[source]
        source: crate::remote::SshError,
    },
    #[error(
        "the ssh connection to {deck} succeeded but the forward could not be established: \
         {detail}"
    )]
    ForwardFailed { deck: String, detail: String },
    #[error(
        "the ssh tunnel to {deck} did not produce a forwarded socket within {secs}s; the \
         connection is up but nothing is listening on {remote} over there"
    )]
    ForwardTimeout {
        deck: String,
        secs: u64,
        remote: String,
    },
}

// ---------------------------------------------------------------------------
// The tunnel
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod tunnel {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{MAX_UNIX_SOCKET_PATH_BYTES, SshProgram, TunnelError};
    use crate::daemon_client::{Endpoint, LocalEndpoint, RemoteEndpoint};
    use crate::remote::{SshError, classify_ssh_error};

    /// Seconds ssh may spend on DNS, TCP and the ssh handshake before giving
    /// up. Short, because a GUI has a user watching it.
    const TUNNEL_CONNECT_TIMEOUT_SECS: u64 = 10;

    /// Keepalive cadence. Matched to [`crate::connect`]'s live-session values
    /// rather than the probe path's `ServerAliveCountMax=1`: a tunnel is a
    /// long-lived session, so a 15–30 second network blip must not tear it
    /// down, while a genuinely dead link is still detected in about
    /// `interval × count` — and *detectable* is the whole point. Without it a
    /// dead tunnel is a socket that accepts connections forever and answers
    /// nothing, which is the worst failure shape available.
    const TUNNEL_KEEPALIVE_INTERVAL_SECS: u64 = 15;
    const TUNNEL_KEEPALIVE_COUNT_MAX: u64 = 3;

    /// How long [`RemoteTunnel::open`] waits for the forwarded socket to
    /// appear. Generous relative to the connect timeout because ssh has to
    /// authenticate first, and authentication can involve a hardware key the
    /// user has to touch.
    const FORWARD_READY_TIMEOUT: Duration = Duration::from_secs(30);
    const FORWARD_READY_POLL: Duration = Duration::from_millis(20);

    /// Grace between SIGTERM and SIGKILL when tearing a tunnel down.
    const TEARDOWN_GRACE: Duration = Duration::from_millis(500);
    const TEARDOWN_POLL: Duration = Duration::from_millis(10);

    /// Cap on ssh stderr held in memory for classification.
    ///
    /// The drain thread keeps **reading** past this and simply stops storing,
    /// which is the opposite of what [`crate::remote::run_with_wallclock_kill`]
    /// does and is right for a different reason: that helper kills a bounded
    /// probe at the cap, whereas a tunnel must live, so refusing to drain would
    /// fill the pipe buffer and wedge ssh instead of protecting anything.
    const STDERR_CAPTURE_CAP: usize = 8 * 1024;

    /// Prefix every file this module owns inside the tunnel directory carries.
    const TUNNEL_FILE_PREFIX: &str = "tun-";

    /// How long a read of the capture waits for the drain thread to reach EOF.
    ///
    /// **Bounded rather than a `join`, and the difference is a hang.** The
    /// obvious spelling is to join the drain thread, which ends at EOF — but the
    /// pipe reaches EOF only when *every* write end closes, and the ssh child is
    /// not necessarily the last holder of one: a `ProxyCommand` helper inherits
    /// it, and so does anything that helper spawned. A join would then park the
    /// caller forever, and the callers are `health()` and `close()` — the two
    /// the app calls to find out whether things are all right and to tidy up.
    /// Measured the hard way: a deliberate mutation that stopped `close()`
    /// signalling the child wedged the whole test binary instead of failing a
    /// test, which is the same wedge a real un-killable child would cause.
    const STDERR_SETTLE: Duration = Duration::from_millis(250);
    const STDERR_SETTLE_POLL: Duration = Duration::from_millis(2);

    /// Bounded capture of a child's stderr.
    #[derive(Debug, Default)]
    struct CappedStderr {
        bytes: Vec<u8>,
        truncated: bool,
        /// Set when the drain thread has read to EOF, so a reader can tell
        /// "nothing was written" from "nothing has been read yet".
        at_eof: bool,
    }

    impl CappedStderr {
        fn push(&mut self, chunk: &[u8]) {
            let room = STDERR_CAPTURE_CAP.saturating_sub(self.bytes.len());
            if room == 0 {
                self.truncated = !chunk.is_empty() || self.truncated;
                return;
            }
            if chunk.len() > room {
                self.bytes.extend_from_slice(&chunk[..room]);
                self.truncated = true;
            } else {
                self.bytes.extend_from_slice(chunk);
            }
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes).into_owned()
        }
    }

    /// What the supervising side can say about a tunnel right now.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TunnelHealth {
        /// The `ssh` child is still running.
        Alive,
        /// The child exited. `detail` is ssh's own stderr, scrubbed.
        Exited { code: Option<i32>, detail: String },
    }

    /// An app-managed `ssh -N -L` child and the forwarded socket it created.
    ///
    /// Owning the child is the work this milestone exists for: both pre-existing
    /// `Command::new("ssh")` sites in this tree block on their child
    /// (`connect.rs` via `status()`, `remote.rs` via `output()`), so nothing
    /// here supervised, reaped or tore down a long-lived one before.
    #[derive(Debug)]
    pub struct RemoteTunnel {
        deck: String,
        remote_socket: String,
        socket: PathBuf,
        sidecar: Option<PathBuf>,
        child: Child,
        stderr: Arc<Mutex<CappedStderr>>,
        closed: bool,
    }

    impl RemoteTunnel {
        /// Open a tunnel to `endpoint`, choosing a fresh forwarded-socket path.
        ///
        /// The path is chosen rather than configured, and it is **never reused**
        /// — the name carries this process's pid and a per-open nonce. That is
        /// the structural half of the orphan answer: a tunnel left behind by a
        /// SIGKILLed app holds a socket whose name no later run will ever
        /// construct, so no later run can mistake it for its own and read it as
        /// a healthy deck.
        pub fn open(ssh: &SshProgram, endpoint: &RemoteEndpoint) -> Result<Self, TunnelError> {
            let dir = tunnel_socket_dir()?;
            // Best effort, and deliberately before we add one of our own: a
            // sweep that runs on the way in bounds how many leftovers can
            // accumulate across crashes without needing a shutdown hook that a
            // SIGKILL would skip anyway.
            let reaped = reap_orphaned_tunnels(&dir);
            if reaped > 0 {
                tracing::info!(reaped, dir = %dir.display(), "reaped orphaned ssh tunnels");
            }
            let socket = dir.join(socket_file_name());
            Self::open_at(ssh, endpoint, socket)
        }

        /// [`Self::open`] against an explicitly chosen socket path.
        ///
        /// Public for tests and for a caller that already owns a private
        /// directory. It returns a [`RemoteTunnel`], never a
        /// [`LocalEndpoint`] — see the module docs for why that distinction is
        /// the one thing here that must not erode.
        pub fn open_at(
            ssh: &SshProgram,
            endpoint: &RemoteEndpoint,
            socket: PathBuf,
        ) -> Result<Self, TunnelError> {
            Self::open_at_within(ssh, endpoint, socket, FORWARD_READY_TIMEOUT)
        }

        /// [`Self::open_at`] with an explicit readiness deadline.
        ///
        /// Exposed so the timeout arm is reachable in under a second rather
        /// than in [`FORWARD_READY_TIMEOUT`]: a path that is only exercised by a
        /// test nobody is willing to wait for is a path nobody exercises.
        pub fn open_at_within(
            ssh: &SshProgram,
            endpoint: &RemoteEndpoint,
            socket: PathBuf,
            ready_timeout: Duration,
        ) -> Result<Self, TunnelError> {
            check_socket_path(&socket)?;
            clear_stale_socket(&socket)?;

            let deck = endpoint.describe();
            let remote_socket = endpoint.socket().as_str().to_string();
            let mut command = build_tunnel_command(ssh, endpoint, &socket);
            let mut child = command.spawn().map_err(|source| TunnelError::Spawn {
                program: ssh.path().to_string_lossy().into_owned(),
                source,
            })?;

            let sidecar = write_sidecar(&socket, child.id());

            let stderr = Arc::new(Mutex::new(CappedStderr::default()));
            if let Some(mut pipe) = child.stderr.take() {
                let sink = Arc::clone(&stderr);
                // Detached deliberately — nothing ever joins it. See
                // [`STDERR_SETTLE`]: a join is what turns an un-killable
                // grandchild holding the write end into an app that hangs. The
                // cost is one thread blocked in `read` per such tunnel, which
                // ends when the last write end closes.
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        match pipe.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if let Ok(mut guard) = sink.lock() {
                                    guard.push(&buf[..n]);
                                }
                            }
                        }
                    }
                    if let Ok(mut guard) = sink.lock() {
                        guard.at_eof = true;
                    }
                });
            }

            let mut tunnel = Self {
                deck,
                remote_socket,
                socket,
                sidecar,
                child,
                stderr,
                closed: false,
            };
            tunnel.await_forward(endpoint, ready_timeout)?;
            Ok(tunnel)
        }

        /// Poll for the forwarded socket, failing fast if the child dies first.
        fn await_forward(
            &mut self,
            endpoint: &RemoteEndpoint,
            ready_timeout: Duration,
        ) -> Result<(), TunnelError> {
            let deadline = Instant::now() + ready_timeout;
            loop {
                // The socket's *appearance* is an establishment signal, not a
                // health signal — it means the local ssh client bound it. What
                // is on the far end is the daemon's business and the
                // handshake's.
                if std::fs::symlink_metadata(&self.socket).is_ok() {
                    return Ok(());
                }
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.settle_stderr();
                        let detail = self.stderr_text();
                        return Err(classify_exit(
                            &self.deck,
                            endpoint,
                            &detail,
                            status.code(),
                            &self.remote_socket,
                        ));
                    }
                    Ok(None) => {}
                    Err(source) => {
                        return Err(TunnelError::Spawn {
                            program: "ssh".to_string(),
                            source,
                        });
                    }
                }
                if Instant::now() >= deadline {
                    let remote = self.remote_socket.clone();
                    let deck = self.deck.clone();
                    self.close();
                    return Err(TunnelError::ForwardTimeout {
                        deck,
                        secs: ready_timeout.as_secs(),
                        remote,
                    });
                }
                std::thread::sleep(FORWARD_READY_POLL);
            }
        }

        /// The address a client connects to.
        ///
        /// **A bare `&Path`, never a [`LocalEndpoint`]** — the single property
        /// PRD #741 M2 built and this milestone is the one most likely to undo.
        /// Handing this value to `LocalEndpoint::at` would make
        /// `run_daemon_stop` compile against it, and its `SO_PEERCRED` lookup
        /// would then name the local `ssh` client: the tunnel would be torn
        /// down and the user told a daemon had stopped gracefully.
        pub fn connect_address(&self) -> &Path {
            &self.socket
        }

        /// How this deck is named to a user.
        pub fn deck(&self) -> &str {
            &self.deck
        }

        /// The `ssh` child's pid, for logging and for a supervisor that wants
        /// to say which process it is waiting on.
        pub fn child_pid(&self) -> u32 {
            self.child.id()
        }

        /// Ask the **child process** whether the tunnel is still up.
        ///
        /// Never `self.socket.exists()`: the socket is created by the local ssh
        /// client and outlives a dead far end, so its presence answers a
        /// question nobody asked. `ServerAliveInterval`/`ServerAliveCountMax`
        /// are what turn a dead link into a child exit this can see.
        pub fn health(&mut self) -> TunnelHealth {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code();
                    self.settle_stderr();
                    TunnelHealth::Exited {
                        code,
                        detail: self.stderr_text(),
                    }
                }
                Ok(None) => TunnelHealth::Alive,
                // An errored `try_wait` cannot be reported as alive: the honest
                // answer is that we no longer know, and treating "unknown" as
                // healthy is the false-healthy state this milestone exists to
                // avoid.
                Err(error) => TunnelHealth::Exited {
                    code: None,
                    detail: format!("could not query the ssh child: {error}"),
                },
            }
        }

        /// Give the stderr drain a bounded chance to reach EOF.
        ///
        /// Called before every read of the capture that follows a child exit,
        /// and it is not tidiness: `try_wait` observes the exit as soon as the
        /// kernel reaps it, which can be *before* the drain thread has read what
        /// the child wrote on the way out. Classifying an empty capture sends a
        /// host-key failure down the `Other` arm and loses its remedy, and sends
        /// a forward-bind failure to `HostUnreachable` — the exact
        /// misclassification issue #344 fixed once already. Demonstrated by
        /// delaying the drain thread 50 ms and watching
        /// `a_host_key_failure_keeps_its_name_and_its_remedy` go red.
        ///
        /// Bounded rather than joined, for the reason [`STDERR_SETTLE`] gives.
        /// Returning without EOF costs a truncated classification; joining could
        /// cost the app.
        fn settle_stderr(&self) {
            let deadline = Instant::now() + STDERR_SETTLE;
            loop {
                if self.stderr.lock().map(|guard| guard.at_eof).unwrap_or(true) {
                    return;
                }
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(STDERR_SETTLE_POLL);
            }
        }

        /// ssh's own stderr so far, scrubbed of control and bidi bytes.
        ///
        /// Scrubbed at this boundary for the reason
        /// [`crate::remote::scrub_remote_text`] records: the bytes are written
        /// by a host we have not yet decided to trust, and this string reaches
        /// an error message and a log.
        pub fn stderr_text(&self) -> String {
            let raw = self
                .stderr
                .lock()
                .map(|guard| {
                    let mut text = guard.text();
                    if guard.truncated {
                        text.push_str("\n… (ssh stderr truncated)");
                    }
                    text
                })
                .unwrap_or_default();
            crate::untrusted_text::strip_control_and_bidi(&raw, true)
                .trim()
                .to_string()
        }

        /// Tear the tunnel down: signal the child's process group, reap it, and
        /// remove the socket and its sidecar.
        ///
        /// Idempotent, so [`Drop`] can call it after an explicit close.
        /// `killpg` rather than `kill` because ssh may itself have started a
        /// `ProxyCommand` child, and the reap is not optional — an unreaped
        /// child is a zombie per reconnect.
        pub fn close(&mut self) {
            if self.closed {
                return;
            }
            self.closed = true;
            let pid = self.child.id() as i32;
            if matches!(self.child.try_wait(), Ok(None)) {
                signal_group(pid, libc::SIGTERM);
                let deadline = Instant::now() + TEARDOWN_GRACE;
                while Instant::now() < deadline {
                    if !matches!(self.child.try_wait(), Ok(None)) {
                        break;
                    }
                    std::thread::sleep(TEARDOWN_POLL);
                }
                if matches!(self.child.try_wait(), Ok(None)) {
                    signal_group(pid, libc::SIGKILL);
                }
            }
            // Reap. Without this every reconnect leaves a zombie behind.
            let _ = self.child.wait();
            self.settle_stderr();
            // ssh unlinks the forwarded socket on a clean exit; a SIGKILLed one
            // does not. This is a deletion of a path *this* object chose, made
            // inside a directory this module created 0o700, and it is skipped
            // entirely if anything other than a socket is there.
            remove_if_socket(&self.socket);
            if let Some(sidecar) = &self.sidecar {
                let _ = std::fs::remove_file(sidecar);
            }
        }
    }

    impl Drop for RemoteTunnel {
        /// Covers the ordinary paths. It deliberately does **not** claim to
        /// cover the ones where `Drop` never runs — a SIGKILL of the app, a
        /// force-quit, a window close that bypasses teardown. Those are handled
        /// by the unique socket name (nothing adopts an orphan's socket) and by
        /// [`reap_orphaned_tunnels`] on the next [`RemoteTunnel::open`].
        fn drop(&mut self) {
            self.close();
        }
    }

    /// A live substrate for an [`Endpoint`].
    ///
    /// The seam that replaces M2's `RemoteTransportUnavailable`: a local deck
    /// needs nothing established, a remote one owns a tunnel, and both answer
    /// [`Self::connect_address`] with the address a client opens. There is no
    /// accessor that yields a [`LocalEndpoint`] from the remote arm — the
    /// forwarded socket enters this type as a `PathBuf` and leaves it as a
    /// `&Path`, and never wears the type that would make it terminable.
    #[derive(Debug)]
    pub enum EndpointConnection {
        Local(LocalEndpoint),
        Remote(Box<RemoteTunnel>),
    }

    impl EndpointConnection {
        /// Establish whatever `endpoint` needs.
        pub fn open(endpoint: &Endpoint, ssh: &SshProgram) -> Result<Self, TunnelError> {
            match endpoint {
                Endpoint::Local(local) => Ok(Self::Local(local.clone())),
                Endpoint::Remote(remote) => {
                    Ok(Self::Remote(Box::new(RemoteTunnel::open(ssh, remote)?)))
                }
            }
        }

        /// The address a client connects to. Infallible, because by the time
        /// one of these exists the transport is up.
        pub fn connect_address(&self) -> &Path {
            match self {
                Self::Local(local) => local.path(),
                Self::Remote(tunnel) => tunnel.connect_address(),
            }
        }
    }

    /// Build the `ssh -N -L` command.
    pub fn build_tunnel_command(
        ssh: &SshProgram,
        endpoint: &RemoteEndpoint,
        local_socket: &Path,
    ) -> Command {
        let mut cmd = Command::new(ssh.path());
        for arg in tunnel_args(endpoint, local_socket) {
            cmd.arg(arg);
        }
        // stdin is closed rather than inherited: `BatchMode=yes` already stops
        // ssh prompting, and a GUI has no terminal for a prompt to reach. stdout
        // is discarded because `-N` produces none; stderr is a pipe because it
        // is the only channel that says *why* a tunnel failed.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // SAFETY: `pre_exec` runs in the child between fork and exec, where only
        // async-signal-safe calls are permitted. `setsid(2)` is on POSIX's list
        // and is the only call made. It gives the child its own session and
        // process group, which is what makes the `killpg` in
        // [`RemoteTunnel::close`] reach a `ProxyCommand` helper too, and stops
        // ssh sharing the app's controlling terminal.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd
    }

    /// The argv `ssh` is invoked with, minus the program itself.
    ///
    /// Pure, so the forced options can be asserted as data. The order is the one
    /// property that is not free: every `-o` comes before `--`, and `--`
    /// immediately precedes the destination, so no value a user stored can be
    /// read as an option however it begins.
    pub fn tunnel_args(endpoint: &RemoteEndpoint, local_socket: &Path) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        // No remote command and no session: this child exists to carry a
        // forward and nothing else.
        args.push("-N".to_string());

        for option in forced_options() {
            args.push("-o".to_string());
            args.push(option);
        }

        args.push("-L".to_string());
        args.push(format!(
            "{}:{}",
            local_socket.to_string_lossy(),
            endpoint.socket()
        ));
        args.push("-p".to_string());
        args.push(endpoint.port().to_string());
        if let Some(key) = endpoint.key() {
            args.push("-i".to_string());
            args.push(key.as_str().to_string());
        }
        if let Some(jump) = endpoint.jump() {
            args.push("-J".to_string());
            args.push(jump.as_str().to_string());
        }
        args.push("--".to_string());
        args.push(endpoint.user_host());
        args
    }

    /// The `-o` options forced on every tunnel.
    ///
    /// This is [`crate::remote`]'s `apply_observation_options` **minus
    /// `ClearAllForwardings`** — a forward is the entire point here — plus the
    /// four a long-lived forward needs. Each of the ten inherited ones keeps the
    /// reason written at that function; the sharp one is `ForwardAgent=no`,
    /// because a `Host *` block carrying `ForwardAgent yes` would otherwise
    /// expose the laptop's ssh-agent to the endpoint for the life of the tunnel,
    /// and a GUI cannot show the user what their own ssh config delegated on
    /// their behalf.
    ///
    /// What is **not** here, and must never be:
    ///
    /// - `StrictHostKeyChecking` in any form. Setting it to `no` or
    ///   `accept-new` would convert a first-contact decision into silent
    ///   trust-on-first-use taken by an app that cannot show the user a
    ///   fingerprint. A host key we have not seen must fail, and
    ///   [`crate::remote::SshError::HostKeyVerificationFailed`] already carries
    ///   the remedy (run `ssh <target>` once in a terminal).
    /// - `UserKnownHostsFile`. Pointing it anywhere is the same decision wearing
    ///   a different name.
    fn forced_options() -> Vec<String> {
        let mut options: Vec<String> = [
            // A GUI has no tty. Without BatchMode ssh tries to read the
            // host-key prompt from a closed stdin and fails anyway — but the app
            // gets an *unclassified* failure, and on a Linux desktop with
            // SSH_ASKPASS and DISPLAY set ssh may instead pop a third-party
            // dialog this app did not draw and cannot control.
            "BatchMode=yes",
            // Without this ssh happily stays up having failed to bind the
            // forward, leaving a live child and no socket — a healthy-looking
            // tunnel that carries nothing.
            "ExitOnForwardFailure=yes",
            // StreamLocalBindUnlink defaults to `no`, which means a second
            // tunnel against an existing socket file fails to forward at all
            // rather than replacing it. We also clear the path ourselves before
            // spawning (see `clear_stale_socket`); both are here because the
            // unlink ssh performs is bounded to a path we chose inside a
            // directory we created 0o700, which is what makes it safe to let it
            // happen at all.
            "StreamLocalBindUnlink=yes",
            // Inherited from `apply_observation_options`, minus
            // ClearAllForwardings. Never join or spawn a shared master: a
            // pre-existing master would carry the user's own forwards and
            // outlive this child, taking the tunnel's lifetime out of the app's
            // hands.
            "ControlMaster=no",
            "ControlPath=none",
            // A transport must run no side effects on the laptop, and
            // connecting must not rewrite known_hosts.
            "PermitLocalCommand=no",
            "UpdateHostKeys=no",
            // Delegation. ClearAllForwardings never covered these (verified
            // against OpenSSH 10.2 in `apply_observation_options`), and they are
            // the ones that hand the far end a capability rather than a channel.
            "ForwardAgent=no",
            "ForwardX11=no",
            "ForwardX11Trusted=no",
            "GSSAPIDelegateCredentials=no",
            "AddKeysToAgent=no",
        ]
        .iter()
        .map(|option| (*option).to_string())
        .collect();
        options.push(format!("ConnectTimeout={TUNNEL_CONNECT_TIMEOUT_SECS}"));
        options.push(format!(
            "ServerAliveInterval={TUNNEL_KEEPALIVE_INTERVAL_SECS}"
        ));
        options.push(format!("ServerAliveCountMax={TUNNEL_KEEPALIVE_COUNT_MAX}"));
        options
    }

    /// Turn a dead tunnel's exit into the error that says what to do about it.
    ///
    /// Reuses both existing classifiers rather than inventing a third:
    /// [`crate::connect::is_forward_failure_detail`] first, because ssh reports
    /// a forward that could not bind with exit 255 exactly like a transport
    /// failure and sending the user to debug a network path that was never
    /// broken is the misclassification issue #344 already fixed once; then
    /// [`classify_ssh_error`], which already distinguishes host-key failure and
    /// already carries its remedy.
    fn classify_exit(
        deck: &str,
        endpoint: &RemoteEndpoint,
        detail: &str,
        code: Option<i32>,
        remote: &str,
    ) -> TunnelError {
        if crate::connect::is_forward_failure_detail(detail) {
            return TunnelError::ForwardFailed {
                deck: deck.to_string(),
                detail: detail.to_string(),
            };
        }
        // Exit 255 is ssh's own "transport or auth failed". Any other code is
        // ssh exiting for a reason its stderr explains, and `classify_ssh_error`
        // folds anything it cannot name into `Other` with that stderr attached.
        let target = endpoint.ssh_target();
        let source = classify_ssh_error(&target, detail);
        if code == Some(255) || !detail.is_empty() {
            return TunnelError::Ssh {
                deck: deck.to_string(),
                source,
            };
        }
        TunnelError::Ssh {
            deck: deck.to_string(),
            source: SshError::Other {
                target: target.user_host(),
                detail: format!(
                    "ssh exited with status {} and said nothing; the forward at {remote} was \
                     never established",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                ),
            },
        }
    }

    /// `killpg` the child's group, tolerating a group that is already gone.
    fn signal_group(pid: i32, signal: i32) {
        // SAFETY: `killpg(2)` is async-signal-safe and the pgid is the child's
        // own pid — `pre_exec`'s `setsid` made it a session and group leader, so
        // this cannot reach any other process group.
        let rc = unsafe { libc::killpg(pid, signal) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                tracing::warn!(pid, signal, %error, "killpg on the ssh tunnel failed");
            }
        }
    }

    // -- socket paths -------------------------------------------------------

    /// The private directory forwarded sockets live in.
    ///
    /// `$XDG_RUNTIME_DIR` when it is set — a per-user, mode-0700,
    /// wiped-on-logout directory, which is the correct home for a socket — and
    /// a uid-suffixed directory under the system temp dir otherwise. The `/tmp`
    /// fallback mirrors [`crate::platform::paths::attach_socket_path`]'s, uid
    /// included, and is created through
    /// [`crate::platform::fsperm::ensure_owner_only_dir`], which refuses a
    /// symlink and fails closed when it cannot set 0o700 — which is what it
    /// does when another user got there first.
    pub fn tunnel_socket_dir() -> Result<PathBuf, TunnelError> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|v| !v.is_empty());
        tunnel_socket_dir_in(runtime.as_deref().map(Path::new))
    }

    /// [`tunnel_socket_dir`] against an explicit `$XDG_RUNTIME_DIR`.
    ///
    /// Split out so a test can choose the directory **without setting a
    /// process-global environment variable**. `XDG_RUNTIME_DIR` is read by
    /// `attach_socket_path` and by the daemon's lock-dir resolution, and
    /// `cargo test` runs unit tests as threads in one process, so a test that
    /// mutated it would race every other test in the binary rather than only
    /// its own. (`cargo test-fast` runs nextest, which is process-per-test and
    /// would have hidden that.)
    ///
    /// **The `tunnels` component is load-bearing, not tidiness.**
    /// `$XDG_RUNTIME_DIR/dot-agent-deck` already exists on a developer's machine
    /// and holds the daemon's `*.lock` files. [`reap_orphaned_tunnels`] reads
    /// the whole directory it is given, and while it only ever *acts* on the two
    /// names this module writes, pointing a sweep that deletes at a directory
    /// somebody else also writes is a blast radius with no upside. A dedicated
    /// subdirectory keeps the sweep's reach to files this module created.
    pub fn tunnel_socket_dir_in(runtime_dir: Option<&Path>) -> Result<PathBuf, TunnelError> {
        let dir = match runtime_dir {
            Some(runtime) => runtime.join("dot-agent-deck").join("tunnels"),
            None => std::env::temp_dir().join(format!(
                "dot-agent-deck-tunnels-{}",
                crate::platform::paths::current_uid()
            )),
        };
        crate::platform::fsperm::ensure_owner_only_dir(&dir).map_err(|source| {
            TunnelError::SocketDir {
                dir: dir.to_string_lossy().into_owned(),
                source,
            }
        })?;
        Ok(dir)
    }

    /// A socket file name no other open will ever produce.
    ///
    /// Two parts, each doing a different job: the **pid** is what
    /// [`reap_orphaned_tunnels`] reads to decide whether the owning app is
    /// still alive, and the **nonce** is what makes the name unique within one
    /// process so two concurrent tunnels never collide. Short because the whole
    /// path has to fit in `sun_path`.
    fn socket_file_name() -> String {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!(
            "{TUNNEL_FILE_PREFIX}{}-{:08x}.sock",
            std::process::id(),
            nonce
        )
    }

    /// Refuse a socket path the OS or ssh could not use.
    pub fn check_socket_path(socket: &Path) -> Result<(), TunnelError> {
        let text = socket.to_string_lossy();
        if text.len() > MAX_UNIX_SOCKET_PATH_BYTES {
            return Err(TunnelError::SocketPathTooLong {
                actual: text.len(),
                max: MAX_UNIX_SOCKET_PATH_BYTES,
                path: text.into_owned(),
            });
        }
        // ssh splits `-L` on ':'. A colon anywhere in the local path silently
        // re-interprets the forward specification rather than failing.
        if text.contains(':') {
            return Err(TunnelError::SocketPathHasColon {
                path: text.into_owned(),
            });
        }
        Ok(())
    }

    /// Remove a leftover inode at `socket` so ssh can bind there.
    ///
    /// A deletion, so it is narrow on purpose: a **symlink** is refused outright
    /// rather than followed, and anything that is not a socket is left alone and
    /// reported. Together with `StreamLocalBindUnlink=yes` this is belt and
    /// braces — either would do on its own, and doing both means neither ssh's
    /// version nor ours is the single point of failure.
    fn clear_stale_socket(socket: &Path) -> Result<(), TunnelError> {
        let metadata = match std::fs::symlink_metadata(socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(TunnelError::SocketPathNotClearable {
                    path: socket.to_string_lossy().into_owned(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(TunnelError::SocketPathIsSymlink {
                path: socket.to_string_lossy().into_owned(),
            });
        }
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() {
            return Err(TunnelError::SocketPathNotClearable {
                path: socket.to_string_lossy().into_owned(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "something that is not a socket is already at this path",
                ),
            });
        }
        std::fs::remove_file(socket).map_err(|source| TunnelError::SocketPathNotClearable {
            path: socket.to_string_lossy().into_owned(),
            source,
        })
    }

    /// Remove `path` only if it is still a socket. Used at teardown, where a
    /// failure is not worth reporting but following a symlink would be.
    fn remove_if_socket(path: &Path) {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(metadata) = std::fs::symlink_metadata(path)
            && metadata.file_type().is_socket()
        {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Record the `ssh` child's pid beside its socket so a later run can
    /// terminate an orphan rather than only unlinking its socket.
    fn write_sidecar(socket: &Path, ssh_pid: u32) -> Option<PathBuf> {
        let sidecar = socket.with_extension("ssh");
        match std::fs::write(&sidecar, ssh_pid.to_string()) {
            Ok(()) => Some(sidecar),
            Err(error) => {
                // Not fatal: without it an orphan's socket is still unlinked
                // and still never adopted, and the stray process is still
                // bounded by its own keepalives once the far end goes.
                tracing::warn!(%error, sidecar = %sidecar.display(), "could not record the ssh tunnel pid");
                None
            }
        }
    }

    // -- the orphan case ----------------------------------------------------

    /// Sweep `dir` for tunnels an earlier run left behind, and return how many
    /// were cleared.
    ///
    /// # What the orphan case actually is
    ///
    /// If the app is SIGKILLed or force-quit, `Drop` does not run and the `ssh`
    /// child survives holding the forwarded socket. The danger PRD #741 names is
    /// not the leak but the **false-healthy state**: a local trust check would
    /// *pass* against that socket on the next launch, and a naive `exists()`
    /// would read it as a live deck, while the daemon on the far end may be
    /// long gone.
    ///
    /// # What was chosen
    ///
    /// Two answers, and the first is the one that matters:
    ///
    /// 1. **Structural.** Nothing ever adopts an existing socket. Every
    ///    [`RemoteTunnel::open`] mints a name carrying its own pid and a nonce,
    ///    so an orphan's path is one no later run constructs. The trust check is
    ///    not run against a forwarded socket at all, and health is read off the
    ///    child process rather than off the inode. A false-healthy state is
    ///    therefore unreachable rather than defended against, which is the
    ///    difference this milestone was asked to deliver.
    /// 2. **Hygiene.** This sweep, so leftovers do not accumulate forever. It
    ///    is deliberately conservative, because it deletes and it signals:
    ///    - The owner pid comes from the file *name*. `kill(pid, 0)` returning
    ///      `ESRCH` means dead; `EPERM` means alive under another uid, and is
    ///      treated as **alive** — a pid we may not signal is not ours to reap.
    ///    - A live owner is left entirely alone, at any age.
    ///    - The `ssh` child is signalled only when the sidecar names a pid whose
    ///      `/proc/<pid>/cmdline` still contains this exact socket path. That
    ///      evidence requirement is the same discipline
    ///      `xtask/linkage-check`'s temp-dir reaper uses, and it is what makes a
    ///      recycled pid harmless. Where the evidence cannot be read — any
    ///      non-Linux host — the process is left running and only the socket is
    ///      unlinked, which is the conservative direction: a stray tunnel costs
    ///      a connection, and killing the wrong process costs something else.
    ///    - Only the two names this module writes are touched, and only when
    ///      they are a socket and a regular file respectively. A symlink is
    ///      never followed.
    pub fn reap_orphaned_tunnels(dir: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut reaped = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(owner) = owner_pid_from_socket_name(name) else {
                continue;
            };
            if pid_is_live(owner) {
                continue;
            }
            let sidecar = path.with_extension("ssh");
            if let Some(ssh_pid) = read_sidecar_pid(&sidecar)
                && pid_is_live(ssh_pid)
                && process_cmdline_mentions(ssh_pid, &path.to_string_lossy())
            {
                signal_group(ssh_pid, libc::SIGTERM);
                signal_group(ssh_pid, libc::SIGKILL);
            }
            remove_if_socket(&path);
            let _ = std::fs::remove_file(&sidecar);
            reaped += 1;
        }
        reaped
    }

    /// Pull the owning pid out of `tun-<pid>-<nonce>.sock`. `None` for anything
    /// this module did not write.
    pub fn owner_pid_from_socket_name(name: &str) -> Option<i32> {
        let rest = name.strip_prefix(TUNNEL_FILE_PREFIX)?;
        let rest = rest.strip_suffix(".sock")?;
        let (pid, nonce) = rest.split_once('-')?;
        if nonce.is_empty() || !nonce.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        pid.parse::<i32>().ok().filter(|pid| *pid > 0)
    }

    fn read_sidecar_pid(sidecar: &Path) -> Option<i32> {
        let metadata = std::fs::symlink_metadata(sidecar).ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }
        let text = std::fs::read_to_string(sidecar).ok()?;
        text.trim().parse::<i32>().ok().filter(|pid| *pid > 0)
    }

    /// `kill(pid, 0)`: `ESRCH` is the only answer that means dead. `EPERM` means
    /// the pid is alive under another uid, which is a pid we must not reap.
    fn pid_is_live(pid: i32) -> bool {
        // SAFETY: signal 0 performs the permission and existence checks without
        // delivering anything.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    /// Whether `pid`'s command line still names `needle`.
    ///
    /// Linux only. On every other platform this returns `false`, so the caller
    /// declines to signal rather than signalling without evidence.
    fn process_cmdline_mentions(pid: i32, needle: &str) -> bool {
        #[cfg(target_os = "linux")]
        {
            let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            String::from_utf8_lossy(&raw).contains(needle)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pid, needle);
            false
        }
    }
}

#[cfg(unix)]
pub use tunnel::{
    EndpointConnection, RemoteTunnel, TunnelHealth, build_tunnel_command, check_socket_path,
    owner_pid_from_socket_name, reap_orphaned_tunnels, tunnel_args, tunnel_socket_dir,
    tunnel_socket_dir_in,
};

#[cfg(test)]
mod tests {
    use super::*;

    // -- validated ssh arguments -------------------------------------------

    /// The shapes a user actually types must all survive, or the validation is
    /// a refusal machine rather than a boundary. IPv6 is bracketed because ssh
    /// wants it bracketed wherever a `:` could be read as a separator.
    #[test]
    fn a_hostname_accepts_the_shapes_ssh_actually_takes() {
        for good in [
            "build-box",
            "deck.example.com",
            "deck.example.com.",
            "10.0.0.7",
            "[2001:db8::1]",
            "[::1]",
            "[fe80::1%25eth0]",
            "host_1",
            "a",
        ] {
            assert!(
                Hostname::parse(good).is_ok(),
                "{good} is a host somebody has: {:?}",
                Hostname::parse(good)
            );
        }
    }

    /// The refusals, one row per shape, so a mutation that drops any single
    /// check reddens a named row rather than "some test".
    ///
    /// The rows that matter most are the ones a charset alone would not catch:
    /// a **leading `-`** (ssh reads it as an option even though `-` is legal
    /// inside a host name), and the **shell metacharacters**, which are refused
    /// not because the argv is unsafe — it is not, `--` and `Command::arg`
    /// already handle that — but because the value can reach a `ProxyCommand`
    /// in the user's own ssh config and be interpolated into a shell there.
    #[test]
    fn a_hostname_refuses_every_shape_that_could_reach_a_shell() {
        let long = "a".repeat(254);
        let rows: &[(&str, &str)] = &[
            ("", "Empty"),
            ("-oProxyCommand=touch /tmp/pwned", "LeadingDash"),
            ("-", "LeadingDash"),
            (&long, "TooLong"),
            ("build box", "ForbiddenByte"),
            ("build\tbox", "ForbiddenByte"),
            ("build\nbox", "ForbiddenByte"),
            ("build\0box", "ForbiddenByte"),
            ("build\u{7}box", "ForbiddenByte"),
            ("build\u{7f}box", "ForbiddenByte"),
            ("h`id`", "ForbiddenByte"),
            ("h$(id)", "ForbiddenByte"),
            ("h;id", "ForbiddenByte"),
            ("h&id", "ForbiddenByte"),
            ("h|id", "ForbiddenByte"),
            ("h<id", "ForbiddenByte"),
            ("h>id", "ForbiddenByte"),
            ("h'id", "ForbiddenByte"),
            ("h\"id", "ForbiddenByte"),
            ("h\\id", "ForbiddenByte"),
            ("héllo", "ForbiddenByte"),
            ("h*", "ForbiddenByte"),
            ("h/x", "ForbiddenByte"),
            ("::1", "BareIpv6Separator"),
            ("host:22", "BareIpv6Separator"),
            ("fe80::1%eth0", "BareIpv6Separator"),
            ("[::1", "UnclosedBracket"),
            ("[]", "UnclosedBracket"),
        ];
        for (raw, expected) in rows {
            let err =
                Hostname::parse(raw).expect_err(&format!("{raw:?} must be refused as a host name"));
            assert_eq!(
                variant_name(&err),
                *expected,
                "{raw:?} was refused as {err:?}, expected {expected}"
            );
        }
    }

    /// The universal bytes are refused by **every** type, not only by the one
    /// whose tests happen to list them. Written as a loop over boxed
    /// constructors because the types are distinct and the property is shared.
    #[test]
    fn every_ssh_argument_type_refuses_the_universal_bytes() {
        type Parser = Box<dyn Fn(&str) -> Result<(), SshArgumentError>>;
        let parsers: Vec<(&str, Parser, &str)> = vec![
            (
                "Hostname",
                Box::new(|raw| Hostname::parse(raw).map(|_| ())),
                "host",
            ),
            (
                "SshUser",
                Box::new(|raw| SshUser::parse(raw).map(|_| ())),
                "user",
            ),
            (
                "KeyPath",
                Box::new(|raw| KeyPath::parse(raw).map(|_| ())),
                "/k",
            ),
            (
                "HostAlias",
                Box::new(|raw| HostAlias::parse(raw).map(|_| ())),
                "jump",
            ),
            (
                "RemoteSocketPath",
                Box::new(|raw| RemoteSocketPath::parse(raw).map(|_| ())),
                "/s",
            ),
        ];
        for (name, parse, stem) in &parsers {
            assert!(
                parse(stem).is_ok(),
                "{name} must accept its own ordinary value {stem:?}"
            );
            for evil in [
                "\0", " ", "\t", "\n", "\r", "\u{7}", "\u{7f}", "`", "$", ";", "&", "|", "<", ">",
                "(", ")", "'", "\"", "\\", "é",
            ] {
                let candidate = format!("{stem}{evil}");
                let err = parse(&candidate).expect_err(&format!(
                    "{name} must refuse {candidate:?} — it can reach a ProxyCommand"
                ));
                assert_eq!(
                    variant_name(&err),
                    "ForbiddenByte",
                    "{name} refused {candidate:?} for the wrong reason: {err:?}"
                );
            }
            let dashed = format!("-{stem}");
            assert_eq!(
                variant_name(&parse(&dashed).expect_err("a leading dash must be refused")),
                "LeadingDash",
                "{name} must refuse a value ssh would read as an option"
            );
            assert_eq!(
                variant_name(&parse("").expect_err("an empty value must be refused")),
                "Empty",
                "{name} must refuse an empty value"
            );
        }
    }

    /// `DOMAIN\user` is the shape that would otherwise look harmless. It is
    /// refused because `\` is a shell metacharacter, and because OpenSSH does
    /// not want that spelling on the destination anyway.
    #[test]
    fn an_ssh_user_takes_a_upn_login_and_refuses_a_backslash_domain() {
        assert!(SshUser::parse("deploy").is_ok());
        assert!(
            SshUser::parse("viktor@corp.example").is_ok(),
            "a UPN-style login is ordinary on Kerberos and cloud-managed hosts"
        );
        assert!(SshUser::parse("CORP\\viktor").is_err());
        assert_eq!(
            variant_name(&SshUser::parse(&"u".repeat(65)).expect_err("bounded")),
            "TooLong"
        );
    }

    /// A relative key path resolves against a working directory a GUI never
    /// chose, so it is refused; `~` is accepted because OpenSSH tilde-expands
    /// `-i` itself rather than handing it to a shell.
    #[test]
    fn a_key_path_must_be_absolute_or_home_relative() {
        assert!(KeyPath::parse("/home/v/.ssh/id_ed25519").is_ok());
        assert!(KeyPath::parse("~/.ssh/id_ed25519").is_ok());
        assert_eq!(
            variant_name(&KeyPath::parse("id_ed25519").expect_err("relative is refused")),
            "NotAbsoluteOrTilde"
        );
        assert_eq!(
            variant_name(&KeyPath::parse("~root/.ssh/id").expect_err("only ~/ is accepted")),
            "NotAbsoluteOrTilde"
        );
    }

    /// The remote socket path is the one field whose bound is a **remote**
    /// operating-system limit, and whose charset excludes `:` for a reason that
    /// is not hygiene: `-L` is parsed by splitting on `:`.
    #[test]
    fn a_remote_socket_path_is_absolute_colon_free_and_sun_path_bounded() {
        assert!(RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock").is_ok());
        assert_eq!(
            variant_name(&RemoteSocketPath::parse("run/attach.sock").expect_err("absolute only")),
            "NotAbsolute"
        );
        assert_eq!(
            variant_name(
                &RemoteSocketPath::parse("/run/a:b.sock").expect_err("a colon would split -L")
            ),
            "ForbiddenByte"
        );
        assert_eq!(
            variant_name(&RemoteSocketPath::parse("~/attach.sock").expect_err(
                "ssh does not tilde-expand the remote side of -L, so accepting it would \
                 promise an expansion that never happens"
            )),
            "ForbiddenByte"
        );
        let over = format!("/{}", "a".repeat(MAX_UNIX_SOCKET_PATH_BYTES));
        assert_eq!(
            variant_name(&RemoteSocketPath::parse(&over).expect_err("sun_path is bounded")),
            "TooLong"
        );
    }

    /// A hand-edited `desktop.toml` must not get past a check a settings form
    /// applies — the deserializer runs the same validation the constructor
    /// does, which is the property PRD #741 M6 relies on.
    #[test]
    fn deserializing_runs_the_same_validation_as_the_constructor() {
        assert!(serde_json::from_str::<Hostname>("\"build-box\"").is_ok());
        for hostile in [
            "\"-oProxyCommand=touch /tmp/pwned\"",
            "\"build box\"",
            "\"h$(id)\"",
            "\"\"",
        ] {
            assert!(
                serde_json::from_str::<Hostname>(hostile).is_err(),
                "{hostile} must be refused on the way in, not only in a constructor"
            );
        }
        let round: Hostname = serde_json::from_str("\"deck.example.com\"").expect("valid");
        assert_eq!(
            serde_json::to_string(&round).expect("serializes"),
            "\"deck.example.com\"",
            "the value this crate writes back is the one it read"
        );
    }

    /// The whole endpoint round-trips through the settings document format M6
    /// will store it in, including the optional fields being genuinely
    /// optional.
    #[test]
    fn a_remote_endpoint_round_trips_through_toml() {
        let endpoint = crate::daemon_client::RemoteEndpoint::new(
            Hostname::parse("build-box").unwrap(),
            RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock").unwrap(),
        )
        .with_user(SshUser::parse("deploy").unwrap())
        .with_port(2222)
        .with_key(KeyPath::parse("~/.ssh/id_ed25519").unwrap())
        .with_jump(HostAlias::parse("bastion").unwrap());
        let text = toml::to_string(&endpoint).expect("serializes");
        let back: crate::daemon_client::RemoteEndpoint =
            toml::from_str(&text).expect("deserializes");
        assert_eq!(back, endpoint);

        let minimal: crate::daemon_client::RemoteEndpoint = toml::from_str(
            "host = \"build-box\"\nsocket = \"/run/user/1000/dot-agent-deck-attach.sock\"\n",
        )
        .expect("host and socket alone are enough");
        assert_eq!(minimal.port(), crate::remote::DEFAULT_SSH_PORT);
        assert!(minimal.user().is_none());
        assert!(minimal.key().is_none());
        assert!(minimal.jump().is_none());

        assert!(
            toml::from_str::<crate::daemon_client::RemoteEndpoint>(
                "host = \"h$(id)\"\nsocket = \"/run/attach.sock\"\n"
            )
            .is_err(),
            "a hostile host must not survive the document parse"
        );
    }

    /// Refusals name the field and describe the offending byte **without**
    /// printing it: a message about a control byte that contains the control
    /// byte is a message that can rewrite the terminal it is printed to.
    #[test]
    fn a_refusal_names_the_field_and_never_echoes_a_control_byte() {
        let err = Hostname::parse("build\u{1b}[2Jbox").expect_err("a CSI byte is refused");
        let msg = err.to_string();
        assert!(msg.contains("the host"), "name the field: {msg}");
        assert!(
            !msg.contains('\u{1b}'),
            "the message must not carry the escape byte itself: {msg:?}"
        );
        assert!(msg.contains("0x1b"), "describe it in hex instead: {msg}");

        let nul = Hostname::parse("a\0b").expect_err("NUL is refused");
        assert!(
            nul.to_string().contains("NUL"),
            "NUL is named rather than described as a control byte: {nul}"
        );
    }

    /// A type whose charset admits everything, so the **universal** refusals can
    /// be tested as themselves.
    ///
    /// No shipped newtype has a charset this wide, which is exactly the problem
    /// this solves: with today's five types the universal checks in `validate`
    /// are redundant — a metacharacter, a space, a control byte and a non-ASCII
    /// byte are all refused by the positive charset anyway — so deleting one of
    /// them changes nothing any test can see, and the guard rots silently. The
    /// universal checks are not there for today's types; they are there so the
    /// **next** newtype, with whatever charset it needs, still cannot admit a
    /// byte that reaches a `ProxyCommand`. This is that next newtype, written in
    /// advance.
    struct AnyByte;

    impl SshArgumentRules for AnyByte {
        const FIELD: &'static str = "the permissive test field";
        const MAX_BYTES: usize = 64;
        fn byte_allowed(_byte: u8) -> bool {
            true
        }
    }

    /// The universal refusals hold even for a charset that allows everything.
    #[test]
    fn the_universal_refusals_bite_a_type_whose_charset_allows_everything() {
        assert!(
            validate::<AnyByte>("plain-value").is_ok(),
            "the permissive type must accept an ordinary value, or this proves nothing"
        );
        assert!(
            validate::<AnyByte>("%h-and-%r-and-*-and-?").is_ok(),
            "and it must genuinely be permissive — these are not universal refusals"
        );
        let rows: &[(&str, &str)] = &[
            ("a`b", "shell metacharacter"),
            ("a$b", "shell metacharacter"),
            ("a;b", "shell metacharacter"),
            ("a&b", "shell metacharacter"),
            ("a|b", "shell metacharacter"),
            ("a<b", "shell metacharacter"),
            ("a>b", "shell metacharacter"),
            ("a(b", "shell metacharacter"),
            ("a)b", "shell metacharacter"),
            ("a'b", "shell metacharacter"),
            ("a\"b", "shell metacharacter"),
            ("a\\b", "shell metacharacter"),
            ("a b", "ASCII whitespace"),
            ("a\tb", "ASCII whitespace"),
            ("a\nb", "ASCII whitespace"),
            ("a\rb", "ASCII whitespace"),
            ("a\u{7}b", "control byte"),
            ("a\u{1b}b", "control byte"),
            ("a\u{7f}b", "control byte"),
            ("a\0b", "NUL"),
            ("aéb", "non-ASCII byte"),
        ];
        for (raw, expected) in rows {
            let err = validate::<AnyByte>(raw).expect_err(&format!(
                "{raw:?} must be refused whatever the charset says"
            ));
            assert_eq!(variant_name(&err), "ForbiddenByte", "{raw:?} -> {err:?}");
            assert!(
                err.to_string().contains(expected),
                "{raw:?} should name {expected:?}: {err}"
            );
        }
        assert_eq!(
            variant_name(&validate::<AnyByte>("").expect_err("empty")),
            "Empty"
        );
        assert_eq!(
            variant_name(&validate::<AnyByte>("-x").expect_err("leading dash")),
            "LeadingDash"
        );
        assert_eq!(
            variant_name(&validate::<AnyByte>(&"x".repeat(65)).expect_err("too long")),
            "TooLong"
        );
    }

    /// A refused byte is described by **which rule it broke**, not merely as
    /// "not allowed".
    ///
    /// This is the test that makes the universal-byte checks mutation-visible.
    /// Every one of them is belt-and-braces at the *variant* level — no type's
    /// positive charset admits a metacharacter, a space or a non-ASCII byte
    /// anyway — so deleting one of those terms changes only the message, and a
    /// test that asserted the variant alone would pass. Asserting the wording
    /// is what turns "the check is redundant" into "the check is checked".
    #[test]
    fn a_refusal_says_which_rule_the_byte_broke() {
        let rows: &[(&str, &str)] = &[
            ("h$x", "shell metacharacter"),
            ("h`x", "shell metacharacter"),
            ("h x", "ASCII whitespace"),
            ("héllo", "non-ASCII byte (0xc3)"),
            ("h\u{7}x", "control byte (0x07)"),
            ("h\0x", "NUL"),
            ("h*x", "does not accept"),
        ];
        for (raw, expected) in rows {
            let msg = Hostname::parse(raw)
                .expect_err(&format!("{raw:?} must be refused"))
                .to_string();
            assert!(
                msg.contains(expected),
                "{raw:?} should be refused as {expected:?}, got {msg:?}"
            );
        }
    }

    /// The length bound is checked in **both** directions. A value exactly at
    /// the limit is accepted and one byte past it is not — the pair is what
    /// catches a `>` flipped to `>=`, which a one-sided test would wave
    /// through, and it is the same lesson PRD #741 M1 recorded about its mode
    /// check: the surprising row is the one that does the work.
    #[test]
    fn a_length_bound_accepts_the_limit_and_refuses_one_byte_past_it() {
        let at_limit = "a".repeat(253);
        assert!(
            Hostname::parse(&at_limit).is_ok(),
            "253 bytes is RFC 1035's presentation limit, not one byte over it"
        );
        assert_eq!(
            variant_name(&Hostname::parse(&"a".repeat(254)).expect_err("254 is over")),
            "TooLong"
        );

        let user_at_limit = "u".repeat(64);
        assert!(SshUser::parse(&user_at_limit).is_ok());
        assert_eq!(
            variant_name(&SshUser::parse(&"u".repeat(65)).expect_err("65 is over")),
            "TooLong"
        );

        let socket_at_limit = format!("/{}", "s".repeat(MAX_UNIX_SOCKET_PATH_BYTES - 1));
        assert_eq!(socket_at_limit.len(), MAX_UNIX_SOCKET_PATH_BYTES);
        assert!(
            RemoteSocketPath::parse(&socket_at_limit).is_ok(),
            "a path exactly at sun_path's smaller limit still fits"
        );
    }

    /// The name of an error's variant, so a table-driven refusal test can say
    /// *which* rule fired rather than only that one did.
    fn variant_name(err: &SshArgumentError) -> &'static str {
        match err {
            SshArgumentError::Empty { .. } => "Empty",
            SshArgumentError::LeadingDash { .. } => "LeadingDash",
            SshArgumentError::TooLong { .. } => "TooLong",
            SshArgumentError::ForbiddenByte { .. } => "ForbiddenByte",
            SshArgumentError::NotAbsolute { .. } => "NotAbsolute",
            SshArgumentError::NotAbsoluteOrTilde { .. } => "NotAbsoluteOrTilde",
            SshArgumentError::UnclosedBracket { .. } => "UnclosedBracket",
            SshArgumentError::BareIpv6Separator { .. } => "BareIpv6Separator",
        }
    }

    // -- resolving the ssh program -----------------------------------------

    /// Resolution walks a fixed list of absolute paths in order and takes the
    /// first usable one. `PATH` is never consulted.
    #[test]
    fn ssh_resolves_to_the_first_usable_absolute_candidate() {
        let program =
            SshProgram::resolve_among(&["/no/such/ssh", "/usr/bin/ssh", "/bin/ssh"], &|p| {
                p == Path::new("/usr/bin/ssh") || p == Path::new("/bin/ssh")
            })
            .expect("the second candidate is usable");
        assert_eq!(program.path(), Path::new("/usr/bin/ssh"));
    }

    /// A machine with no OpenSSH client — a slim container image is the
    /// realistic case — gets a **named** error at startup rather than a
    /// connection timeout thirty seconds later. The two call for completely
    /// different actions from the user, so collapsing them is the defect.
    #[test]
    fn a_missing_ssh_is_a_named_error_that_lists_where_it_looked() {
        let err = SshProgram::resolve_among(&["/no/such/ssh", "/also/missing"], &|_| false)
            .expect_err("nothing is usable");
        let msg = err.to_string();
        assert!(msg.contains("/no/such/ssh"), "say where it looked: {msg}");
        assert!(msg.contains("/also/missing"), "all of it: {msg}");
        assert!(
            msg.contains("Install"),
            "say what the user can do about it: {msg}"
        );
        assert!(matches!(err, TunnelError::SshNotFound { .. }));
    }

    /// The candidate list is the security property, so it is asserted rather
    /// than trusted: every entry absolute, and none of them under a per-user
    /// prefix a non-root account could write to.
    #[test]
    fn every_ssh_candidate_is_an_absolute_system_path() {
        for candidate in SSH_PROGRAM_CANDIDATES {
            assert!(
                Path::new(candidate).is_absolute(),
                "{candidate} must be absolute — a relative candidate is a PATH lookup wearing \
                 a disguise"
            );
            assert!(
                !candidate.contains('~')
                    && !candidate.contains("/home/")
                    && !candidate.contains("Users"),
                "{candidate} is under a per-user prefix, which is exactly the directory a \
                 substituted binary would sit in"
            );
        }
    }

    /// An explicitly chosen ssh must still be absolute: a relative one resolves
    /// against a working directory a GUI never chose.
    #[test]
    fn an_explicitly_chosen_ssh_must_be_absolute() {
        assert!(SshProgram::at("/usr/bin/ssh").is_ok());
        let err = SshProgram::at("ssh").expect_err("a bare name is refused");
        assert!(matches!(err, TunnelError::SshPathNotAbsolute { .. }));
    }
}

/// The tunnel's own tests, against a **stand-in** `ssh`.
///
/// What a stand-in proves and what it does not, stated once here rather than
/// re-argued per test (CLAUDE.md rule 4): these exercise the *mechanics* the app
/// owns — the argv it constructs, spawning, readiness, death detection,
/// teardown, reaping, and the stale-socket collision. They prove nothing about a
/// real remote daemon. That is PRD #741 **M13**, it needs a second machine, and
/// a loopback `ssh` to this box would not be evidence either, because locally
/// the client's filesystem *is* the daemon's.
#[cfg(all(test, unix))]
mod tunnel_tests {
    use super::*;
    use crate::daemon_client::{Endpoint, LocalEndpoint, RemoteEndpoint};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn deck() -> RemoteEndpoint {
        RemoteEndpoint::new(
            Hostname::parse("build-box").unwrap(),
            RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock").unwrap(),
        )
    }

    fn full_deck() -> RemoteEndpoint {
        deck()
            .with_user(SshUser::parse("deploy").unwrap())
            .with_port(2222)
            .with_key(KeyPath::parse("~/.ssh/id_ed25519").unwrap())
            .with_jump(HostAlias::parse("bastion").unwrap())
    }

    fn args_for(endpoint: &RemoteEndpoint) -> Vec<String> {
        tunnel_args(
            endpoint,
            Path::new("/run/user/1000/dot-agent-deck/tun-1-2.sock"),
        )
    }

    // -- the constructed argv ----------------------------------------------

    /// The options a GUI cannot recover from if they are absent. Each row is a
    /// failure mode rather than a style preference — see `forced_options` for
    /// the reason attached to each.
    #[test]
    fn the_tunnel_argv_forces_every_option_a_gui_depends_on() {
        let args = args_for(&deck());
        for option in [
            // No tty for a prompt, and on a Linux desktop with SSH_ASKPASS and
            // DISPLAY set, ssh would otherwise pop a dialog this app did not
            // draw.
            "BatchMode=yes",
            // Otherwise ssh stays up having failed to bind the forward: a live
            // child, no socket, and a tunnel that looks healthy and carries
            // nothing.
            "ExitOnForwardFailure=yes",
            // Defaults to `no`, which makes a second tunnel against an existing
            // socket file fail to forward at all rather than replacing it.
            "StreamLocalBindUnlink=yes",
            // A pre-existing master would carry the user's own forwards and
            // outlive this child.
            "ControlMaster=no",
            "ControlPath=none",
            "PermitLocalCommand=no",
            "UpdateHostKeys=no",
            // The sharp one: a `Host *` block carrying `ForwardAgent yes` would
            // expose the laptop's ssh-agent to the endpoint for the life of the
            // tunnel, and a GUI cannot show the user what their config
            // delegated on their behalf.
            "ForwardAgent=no",
            "ForwardX11=no",
            "ForwardX11Trusted=no",
            "GSSAPIDelegateCredentials=no",
            "AddKeysToAgent=no",
        ] {
            assert!(
                args.iter().any(|a| a == option),
                "the tunnel must force {option}: {args:?}"
            );
        }
        assert!(
            args.iter().any(|a| a.starts_with("ServerAliveInterval=")),
            "without ServerAlive* a dead tunnel is a socket that accepts forever and \
             answers nothing: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("ServerAliveCountMax=")),
            "{args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("ConnectTimeout=")),
            "{args:?}"
        );
    }

    /// Host-key checking is never weakened, in any spelling. `no` or
    /// `accept-new` would convert a first-contact decision into silent
    /// trust-on-first-use taken by an app that cannot show a fingerprint;
    /// pointing `UserKnownHostsFile` elsewhere is the same decision renamed.
    #[test]
    fn the_tunnel_argv_never_weakens_host_key_verification() {
        for endpoint in [deck(), full_deck()] {
            let args = args_for(&endpoint);
            for forbidden in [
                "StrictHostKeyChecking",
                "UserKnownHostsFile",
                "GlobalKnownHostsFile",
                "accept-new",
                "CheckHostIP",
            ] {
                assert!(
                    !args.iter().any(|a| a.contains(forbidden)),
                    "the tunnel must never mention {forbidden}: {args:?}"
                );
            }
        }
    }

    /// `ClearAllForwardings` is the one option deliberately *dropped* from
    /// `apply_observation_options`, because a forward is the entire point here.
    /// Inheriting the whole list wholesale would produce a tunnel that forwards
    /// nothing.
    #[test]
    fn the_tunnel_argv_does_not_clear_the_forwarding_it_exists_to_make() {
        let args = args_for(&deck());
        assert!(
            !args.iter().any(|a| a.contains("ClearAllForwardings")),
            "{args:?}"
        );
    }

    /// `--` separates options from the destination and the destination is last,
    /// so no stored value can be read as an option however it begins. The
    /// newtypes already refuse a leading `-`; this is the second lock on the
    /// same door.
    #[test]
    fn every_option_precedes_the_double_dash_and_the_destination_is_last() {
        let args = args_for(&full_deck());
        let dashdash = args
            .iter()
            .position(|a| a == "--")
            .expect("the destination must be separated by --");
        assert_eq!(
            dashdash,
            args.len() - 2,
            "-- must immediately precede the destination: {args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some("deploy@build-box"));
        for (index, arg) in args.iter().enumerate() {
            if arg == "-o" || arg == "-L" || arg == "-p" || arg == "-i" || arg == "-J" {
                assert!(
                    index < dashdash,
                    "{arg} at {index} must come before -- at {dashdash}: {args:?}"
                );
            }
        }
    }

    /// The forward names both ends and nothing else. The `:` between them is
    /// why neither socket path may contain one.
    #[test]
    fn the_forward_spec_names_the_local_socket_then_the_remote_one() {
        let args = args_for(&deck());
        let at = args.iter().position(|a| a == "-L").expect("a forward");
        assert_eq!(
            args[at + 1],
            "/run/user/1000/dot-agent-deck/tun-1-2.sock:/run/user/1000/dot-agent-deck-attach.sock"
        );
        assert!(args.contains(&"-N".to_string()), "no session: {args:?}");
    }

    /// The optional halves reach the argv when set and are absent when not —
    /// an endpoint with no key must not pass `-i` with an empty value.
    #[test]
    fn the_optional_ssh_arguments_appear_only_when_they_are_set() {
        let bare = args_for(&deck());
        assert!(!bare.contains(&"-i".to_string()), "{bare:?}");
        assert!(!bare.contains(&"-J".to_string()), "{bare:?}");
        assert_eq!(bare.last().map(String::as_str), Some("build-box"));
        assert_eq!(
            bare[bare.iter().position(|a| a == "-p").unwrap() + 1],
            "22",
            "the port is always explicit, so the ssh config cannot move it under us"
        );

        let full = args_for(&full_deck());
        assert_eq!(
            full[full.iter().position(|a| a == "-i").unwrap() + 1],
            "~/.ssh/id_ed25519"
        );
        assert_eq!(
            full[full.iter().position(|a| a == "-J").unwrap() + 1],
            "bastion"
        );
        assert_eq!(
            full[full.iter().position(|a| a == "-p").unwrap() + 1],
            "2222"
        );
    }

    /// The command is built against the resolved absolute program, not `"ssh"`.
    #[test]
    fn the_command_runs_the_resolved_absolute_program() {
        let ssh = SshProgram::at("/usr/bin/ssh").unwrap();
        let command = build_tunnel_command(&ssh, &deck(), Path::new("/tmp/t.sock"));
        assert_eq!(command.get_program(), std::ffi::OsStr::new("/usr/bin/ssh"));
    }

    // -- socket paths -------------------------------------------------------

    /// `sun_path` is 104 bytes on macOS and 108 on Linux; a path over the bound
    /// gets a named refusal rather than a `bind` failure buried in ssh's stderr
    /// half a minute later.
    #[test]
    fn an_over_long_or_colon_bearing_socket_path_is_refused_by_name() {
        let long = PathBuf::from(format!("/tmp/{}", "a".repeat(MAX_UNIX_SOCKET_PATH_BYTES)));
        assert!(matches!(
            check_socket_path(&long),
            Err(TunnelError::SocketPathTooLong { .. })
        ));
        assert!(matches!(
            check_socket_path(Path::new("/tmp/a:b.sock")),
            Err(TunnelError::SocketPathHasColon { .. })
        ));
        assert!(check_socket_path(Path::new("/tmp/tun-1-2.sock")).is_ok());
    }

    /// The directory forwarded sockets live in is created owner-only, and it is
    /// a directory only this module writes — not the one the daemon already
    /// keeps its lock files in, which a sweep that deletes has no business
    /// reading.
    #[test]
    fn the_tunnel_directory_is_owner_only_and_is_not_shared_with_the_daemon() {
        let temp = tempfile::tempdir().expect("tempdir");
        let daemon_dir = temp.path().join("dot-agent-deck");
        std::fs::create_dir_all(&daemon_dir).expect("the daemon's own runtime directory");
        let lock = daemon_dir.join("hook.sock-deadbeef.lock");
        std::fs::write(&lock, b"").expect("a lock file the daemon owns");

        let dir = tunnel_socket_dir_in(Some(temp.path())).expect("a private directory");
        assert_ne!(
            dir, daemon_dir,
            "a sweep that deletes must not read the daemon's lock directory"
        );
        assert!(
            dir.starts_with(&daemon_dir),
            "but it stays under it: {dir:?}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir)
            .expect("created")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "a forwarded socket lives in a private directory"
        );

        assert_eq!(
            reap_orphaned_tunnels(&dir),
            0,
            "nothing of ours is in there yet"
        );
        assert!(lock.exists(), "and the daemon's lock file is untouched");

        let fallback = tunnel_socket_dir_in(None).expect("a temp-dir fallback");
        assert!(
            fallback
                .to_string_lossy()
                .contains(&crate::platform::paths::current_uid().to_string()),
            "the fallback lives under a world-writable temp dir, so it is per-uid: {fallback:?}"
        );
    }

    // -- the stand-in -------------------------------------------------------

    /// Whether a Unix-socket-binding stand-in can run here. Following
    /// `xtask/linkage-check`'s `junit_strip.rs` precedent: where `python3` is
    /// absent the test prints `SKIP:` and returns rather than failing, because
    /// a missing interpreter is not a defect in this code.
    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// Install a stand-in `ssh` in `dir` and return its absolute path.
    ///
    /// `body` is shell, appended after the script has parsed `-L` into
    /// `$local_sock`. That parsing is the stand-in's only real job: it proves
    /// the forward spec the tunnel builds is one a program can actually read.
    fn standin_ssh(dir: &Path, body: &str) -> SshProgram {
        let script = dir.join("ssh-standin");
        let text = format!(
            "#!/bin/sh\n\
             spec=\"\"\n\
             prev=\"\"\n\
             for a in \"$@\"; do\n\
             \tif [ \"$prev\" = \"-L\" ]; then spec=\"$a\"; fi\n\
             \tprev=\"$a\"\n\
             done\n\
             local_sock=${{spec%%:*}}\n\
             {body}\n"
        );
        std::fs::write(&script, text).expect("write the stand-in");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("make it executable");
        SshProgram::at(script).expect("an absolute stand-in")
    }

    /// A stand-in that binds the forwarded socket and then sleeps, which is
    /// what a real `ssh -N -L` does from this side.
    fn binding_standin(dir: &Path) -> SshProgram {
        let binder = dir.join("bind.py");
        std::fs::write(
            &binder,
            "import socket, sys, time\n\
             s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n\
             s.bind(sys.argv[1])\n\
             s.listen(1)\n\
             time.sleep(3600)\n",
        )
        .expect("write the binder");
        standin_ssh(
            dir,
            &format!("exec python3 {} \"$local_sock\"", binder.display()),
        )
    }

    /// A short socket path under a directory this test owns — short because
    /// `tempfile`'s own path plus a long name would breach `sun_path`.
    fn socket_in(dir: &Path) -> PathBuf {
        dir.join("t.sock")
    }

    fn wait_until(mut predicate: impl FnMut() -> bool, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        predicate()
    }

    fn pid_alive(pid: u32) -> bool {
        // SAFETY: signal 0 checks existence and permission without delivering.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    // -- lifecycle ----------------------------------------------------------

    /// Spawn, readiness, and the address the client is handed.
    ///
    /// The address comes back as a bare `&Path` and never as a
    /// [`LocalEndpoint`] — the property PRD #741 M2 built and this milestone was
    /// most likely to undo, because `run_daemon_stop` takes that type and its
    /// `SO_PEERCRED` lookup over a forwarded socket names the local `ssh`
    /// client.
    #[test]
    fn a_tunnel_comes_up_and_hands_back_its_forwarded_socket() {
        if !python3_available() {
            println!("SKIP: python3 is not available, so no stand-in can bind a Unix socket");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = binding_standin(temp.path());
        let socket = socket_in(temp.path());
        let mut tunnel =
            RemoteTunnel::open_at(&ssh, &deck(), socket.clone()).expect("the stand-in binds");
        assert_eq!(tunnel.connect_address(), socket.as_path());
        assert!(
            std::os::unix::fs::FileTypeExt::is_socket(
                &std::fs::symlink_metadata(&socket).unwrap().file_type()
            ),
            "the forward is a real socket"
        );
        assert_eq!(tunnel.health(), TunnelHealth::Alive);
        assert_eq!(tunnel.deck(), "build-box");
    }

    /// Teardown: the child's whole process group goes, the child is **reaped**
    /// (an unreaped one is a zombie per reconnect), and the socket this object
    /// chose is removed. Calling close twice must be harmless, because `Drop`
    /// calls it after an explicit close.
    #[test]
    fn closing_a_tunnel_kills_the_child_reaps_it_and_removes_the_socket() {
        if !python3_available() {
            println!("SKIP: python3 is not available, so no stand-in can bind a Unix socket");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = binding_standin(temp.path());
        let socket = socket_in(temp.path());
        let mut tunnel = RemoteTunnel::open_at(&ssh, &deck(), socket.clone()).expect("opens");
        let pid = tunnel.child_pid();
        tunnel.close();
        tunnel.close();
        assert!(
            wait_until(|| !pid_alive(pid), Duration::from_secs(5)),
            "the ssh child must not outlive its tunnel"
        );
        assert!(
            !socket.exists(),
            "the forwarded socket must not be left behind"
        );
    }

    /// The same teardown on the ordinary path, through `Drop`.
    #[test]
    fn dropping_a_tunnel_tears_it_down() {
        if !python3_available() {
            println!("SKIP: python3 is not available, so no stand-in can bind a Unix socket");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = binding_standin(temp.path());
        let socket = socket_in(temp.path());
        let pid = {
            let tunnel = RemoteTunnel::open_at(&ssh, &deck(), socket.clone()).expect("opens");
            tunnel.child_pid()
        };
        assert!(
            wait_until(|| !pid_alive(pid), Duration::from_secs(5)),
            "Drop must tear the child down"
        );
        assert!(!socket.exists());
    }

    /// Teardown is **bounded** even when something that is not the ssh child
    /// still holds the stderr pipe open.
    ///
    /// This is the hang the mutation sweep found rather than a hypothetical: a
    /// deliberate mutation that stopped `close()` signalling the child wedged
    /// the entire test binary, because the code joined the drain thread and the
    /// drain thread ends only at EOF. A `ProxyCommand` helper — or anything it
    /// spawned — inherits that write end and can outlive the ssh child by any
    /// amount, so the same wedge was reachable in production, in `close()` and
    /// in `health()`. The stand-in here reproduces exactly that: a `setsid`
    /// grandchild that escapes the process group `close()` signals and keeps
    /// the pipe open after the tunnel's own child is gone.
    #[test]
    fn teardown_is_bounded_when_a_grandchild_still_holds_the_stderr_pipe() {
        if !python3_available() {
            println!("SKIP: python3 is not available, so no stand-in can bind a Unix socket");
            return;
        }
        if !Path::new("/usr/bin/setsid").exists() {
            println!("SKIP: /usr/bin/setsid is needed to make a grandchild escape the group");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let binder = temp.path().join("bind.py");
        std::fs::write(
            &binder,
            "import socket, sys, time\n\
             s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n\
             s.bind(sys.argv[1])\n\
             s.listen(1)\n\
             time.sleep(3600)\n",
        )
        .expect("write the binder");
        let ssh = standin_ssh(
            temp.path(),
            &format!(
                "/usr/bin/setsid sh -c 'sleep 20' &\nexec python3 {} \"$local_sock\"",
                binder.display()
            ),
        );
        let socket = socket_in(temp.path());
        let mut tunnel = RemoteTunnel::open_at(&ssh, &deck(), socket.clone()).expect("opens");
        let pid = tunnel.child_pid();
        let started = Instant::now();
        tunnel.close();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "close() must be bounded well inside the 20s the escaped grandchild holds the \
             pipe for, and took {elapsed:?}"
        );
        assert!(
            wait_until(|| !pid_alive(pid), Duration::from_secs(5)),
            "and must still have killed the ssh child"
        );
        assert!(!socket.exists());
    }

    /// **Presence is not health.** The child is killed from outside while the
    /// socket file stays on disk — exactly what a SIGKILLed ssh leaves — and
    /// `health()` must report the death anyway, because it asks the process and
    /// not the inode. A `Path::exists` check here would say the deck is fine.
    #[test]
    fn a_dead_tunnel_is_detected_from_the_child_while_its_socket_still_exists() {
        if !python3_available() {
            println!("SKIP: python3 is not available, so no stand-in can bind a Unix socket");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = binding_standin(temp.path());
        let socket = socket_in(temp.path());
        let mut tunnel = RemoteTunnel::open_at(&ssh, &deck(), socket.clone()).expect("opens");
        let pid = tunnel.child_pid();
        // SAFETY: the pid is this test's own child.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        assert!(
            wait_until(
                || matches!(tunnel.health(), TunnelHealth::Exited { .. }),
                Duration::from_secs(5)
            ),
            "a dead child must read as dead"
        );
        assert!(
            socket.exists(),
            "the premise of this test: a SIGKILLed ssh leaves its socket behind, so \
             presence would have reported a healthy deck"
        );
    }

    /// A host key we have not seen must fail, and the failure must arrive as
    /// the variant that already carries the remedy — run `ssh <target>` once in
    /// a terminal. Reusing `classify_ssh_error` rather than writing a second
    /// matcher is what makes that true for free.
    #[test]
    fn a_host_key_failure_keeps_its_name_and_its_remedy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = standin_ssh(
            temp.path(),
            "echo 'Host key verification failed.' >&2\nexit 255",
        );
        let err = RemoteTunnel::open_at(&ssh, &deck(), socket_in(temp.path()))
            .expect_err("an unverified host key must refuse");
        let TunnelError::Ssh { source, .. } = &err else {
            panic!("expected a classified ssh failure, got {err:?}");
        };
        assert!(
            matches!(
                source,
                crate::remote::SshError::HostKeyVerificationFailed { .. }
            ),
            "got {source:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("run `ssh build-box` once"),
            "the remedy must survive into the message a GUI renders: {msg}"
        );
    }

    /// "The forward could not bind" and "the host is unreachable" are reported
    /// by ssh identically — exit 255 — and sending a user to debug a network
    /// path that was never broken is the misclassification issue #344 already
    /// fixed once. `is_forward_failure_detail` is reused rather than
    /// re-derived.
    #[test]
    fn a_forward_that_cannot_bind_is_not_reported_as_an_unreachable_host() {
        let temp = tempfile::tempdir().expect("tempdir");
        // OpenSSH's own wording for a **Unix-socket** local forward that could
        // not bind, which is the shape this milestone makes and which contains
        // none of the three TCP markers `is_forward_failure_detail` carried
        // before it.
        let ssh = standin_ssh(
            temp.path(),
            "echo 'unix_listener: cannot bind to path /tmp/t.sock: Address already in use' >&2\n\
             echo 'Could not request local forwarding.' >&2\n\
             exit 255",
        );
        let err = RemoteTunnel::open_at(&ssh, &deck(), socket_in(temp.path()))
            .expect_err("a forward that cannot bind must refuse");
        assert!(
            matches!(err, TunnelError::ForwardFailed { .. }),
            "got {err:?}"
        );

        let temp2 = tempfile::tempdir().expect("tempdir");
        let unreachable = standin_ssh(
            temp2.path(),
            "echo 'ssh: connect to host build-box port 22: Connection refused' >&2\nexit 255",
        );
        let err = RemoteTunnel::open_at(&unreachable, &deck(), socket_in(temp2.path()))
            .expect_err("an unreachable host must refuse");
        let TunnelError::Ssh { source, .. } = &err else {
            panic!("expected a classified ssh failure, got {err:?}");
        };
        assert!(
            matches!(source, crate::remote::SshError::ConnectionRefused { .. }),
            "the two must not collapse into one message: {source:?}"
        );
    }

    /// ssh's stderr is remote-influenced text that reaches an error message and
    /// a log, so it is bounded and scrubbed at this boundary — the same seam
    /// `remote.rs` puts `scrub_remote_text` at, for the same reason.
    #[test]
    fn ssh_stderr_is_bounded_and_stripped_of_control_bytes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = standin_ssh(
            temp.path(),
            "printf 'boom\\033[2Jcleared\\n' >&2\n\
             i=0\n\
             while [ $i -lt 400 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' >&2; i=$((i+1)); done\n\
             exit 255",
        );
        let err = RemoteTunnel::open_at(&ssh, &deck(), socket_in(temp.path()))
            .expect_err("the stand-in exits");
        let text = err.to_string();
        assert!(
            !text.contains('\u{1b}'),
            "an escape byte must never reach a message printed to a terminal: {text:?}"
        );
        assert!(text.contains("boom"), "the useful part survives: {text}");
        assert!(
            text.len() < 32 * 1024,
            "a hostile remote must not stream the client into memory pressure; \
             got {} bytes",
            text.len()
        );
    }

    /// The `StreamLocalBindUnlink` collision, which is the failure mode
    /// DECISION 1A names: the option defaults to `no`, so a second tunnel
    /// against an existing socket file **fails to forward at all** rather than
    /// replacing it. Two defences, both asserted — the option is passed, and
    /// the path is cleared before the spawn, so neither ssh's version nor ours
    /// is the single point of failure.
    #[test]
    fn a_stale_socket_at_the_chosen_path_does_not_block_a_new_tunnel() {
        if !python3_available() {
            println!("SKIP: python3 is not available, so no stand-in can bind a Unix socket");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = socket_in(temp.path());
        let stale = std::os::unix::net::UnixListener::bind(&socket).expect("a stale socket");
        drop(stale);
        assert!(socket.exists(), "the premise: an inode is already there");

        let ssh = binding_standin(temp.path());
        let mut tunnel =
            RemoteTunnel::open_at(&ssh, &deck(), socket.clone()).expect("a stale inode is cleared");
        assert_eq!(tunnel.health(), TunnelHealth::Alive);
        assert!(
            args_for(&deck())
                .iter()
                .any(|a| a == "StreamLocalBindUnlink=yes"),
            "and ssh is told to do the same, so neither defence stands alone"
        );
    }

    /// Clearing a stale inode is a **deletion**, so it is narrow: a symlink is
    /// refused rather than followed, and anything that is not a socket is left
    /// alone and reported.
    #[test]
    fn the_socket_path_is_never_cleared_through_a_symlink() {
        let temp = tempfile::tempdir().expect("tempdir");
        let victim = temp.path().join("precious");
        std::fs::write(&victim, b"do not delete me").expect("a file worth keeping");
        let socket = socket_in(temp.path());
        std::os::unix::fs::symlink(&victim, &socket).expect("plant a symlink");

        let ssh = standin_ssh(temp.path(), "exit 0");
        let err = RemoteTunnel::open_at(&ssh, &deck(), socket.clone())
            .expect_err("a symlink at the socket path must refuse");
        assert!(
            matches!(err, TunnelError::SocketPathIsSymlink { .. }),
            "got {err:?}"
        );
        assert!(victim.exists(), "the symlink's target must be untouched");

        let plain = temp.path().join("p.sock");
        std::fs::write(&plain, b"not a socket").expect("write");
        let err = RemoteTunnel::open_at(&ssh, &deck(), plain.clone())
            .expect_err("a regular file at the socket path must refuse");
        assert!(
            matches!(err, TunnelError::SocketPathNotClearable { .. }),
            "got {err:?}"
        );
        assert!(plain.exists(), "and must be left where it is");
    }

    /// A connection that comes up but never produces a forward is bounded, and
    /// the refusal names the remote socket rather than blaming the network.
    #[test]
    fn a_forward_that_never_appears_times_out_with_a_named_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = standin_ssh(temp.path(), "sleep 30");
        let started = Instant::now();
        let err = RemoteTunnel::open_at_within(
            &ssh,
            &deck(),
            socket_in(temp.path()),
            Duration::from_millis(300),
        )
        .expect_err("no socket ever appears");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it must be bounded"
        );
        let TunnelError::ForwardTimeout { remote, .. } = &err else {
            panic!("expected a timeout, got {err:?}");
        };
        assert_eq!(remote, "/run/user/1000/dot-agent-deck-attach.sock");
    }

    // -- the orphan case ----------------------------------------------------

    /// Socket names carry the owning pid and a nonce, which is the *structural*
    /// half of the orphan answer: an orphan's path is one no later run
    /// constructs, so no later run can adopt it and read it as a healthy deck.
    #[test]
    fn a_socket_name_names_its_owner_and_nothing_else_is_recognised() {
        assert_eq!(
            owner_pid_from_socket_name("tun-4242-0a1b2c3d.sock"),
            Some(4242)
        );
        for foreign in [
            "attach.sock",
            "tun-.sock",
            "tun-abc-0a1b.sock",
            "tun-4242.sock",
            "tun-4242-zz.sock",
            "tun-0-0a1b.sock",
            "tun-4242-0a1b2c3d.ssh",
            "tun--1-0a1b.sock",
        ] {
            assert_eq!(
                owner_pid_from_socket_name(foreign),
                None,
                "{foreign} was not written by this module and must not be touched"
            );
        }
    }

    /// The hygiene half. A leftover whose owning app is **dead** is cleared,
    /// including the `ssh` child named in its sidecar; a leftover whose owner is
    /// **alive** is left entirely alone at any age, and so is anything this
    /// module did not write.
    #[test]
    fn the_sweep_clears_a_dead_owners_tunnel_and_never_a_live_ones() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();

        // A pid that is certainly dead: spawn and reap.
        let mut corpse = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn");
        let dead_pid = corpse.id();
        corpse.wait().expect("reap");

        let orphan = dir.join(format!("tun-{dead_pid}-0a1b2c3d.sock"));
        let orphan_sidecar = dir.join(format!("tun-{dead_pid}-0a1b2c3d.ssh"));
        drop(std::os::unix::net::UnixListener::bind(&orphan).expect("bind the orphan"));
        std::fs::write(&orphan_sidecar, dead_pid.to_string()).expect("sidecar");

        let live = dir.join(format!("tun-{}-0a1b2c3e.sock", std::process::id()));
        drop(std::os::unix::net::UnixListener::bind(&live).expect("bind the live one"));

        let stranger = dir.join("someone-elses.sock");
        drop(std::os::unix::net::UnixListener::bind(&stranger).expect("bind a stranger"));

        let reaped = reap_orphaned_tunnels(dir);
        assert_eq!(reaped, 1, "exactly the dead owner's tunnel");
        assert!(!orphan.exists(), "the orphan's socket is cleared");
        assert!(!orphan_sidecar.exists(), "and so is its sidecar");
        assert!(
            live.exists(),
            "a live owner's tunnel is never reaped, at any age"
        );
        assert!(
            stranger.exists(),
            "and nothing this module did not write is touched"
        );
    }

    /// The sweep's kill half: an orphan's `ssh` child is terminated only when
    /// its command line still names that exact socket, which is what makes a
    /// recycled pid harmless.
    #[test]
    fn the_sweep_terminates_an_orphans_ssh_child_when_the_evidence_holds() {
        if !cfg!(target_os = "linux") {
            println!("SKIP: the cmdline evidence this sweep requires is /proc-only");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();

        let mut corpse = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn");
        let dead_owner = corpse.id();
        corpse.wait().expect("reap");

        let orphan = dir.join(format!("tun-{dead_owner}-0a1b2c3d.sock"));
        drop(std::os::unix::net::UnixListener::bind(&orphan).expect("bind"));

        // A stand-in for the orphaned ssh child, modelling the two things that
        // make the real one reapable: its command line still names the socket
        // (here as `$0`, because a single-command `sh -c` execs and loses the
        // script text), and it is its own session leader, which is what
        // `pre_exec`'s `setsid` gives every tunnel child and what makes the
        // `killpg` reach a `ProxyCommand` helper alongside it.
        let mut stray = {
            let mut command = std::process::Command::new("/bin/sh");
            command
                .arg("-c")
                .arg("while :; do sleep 1; done")
                .arg(orphan.to_string_lossy().into_owned());
            // SAFETY: `setsid(2)` is async-signal-safe and is the only call made
            // between fork and exec.
            unsafe {
                use std::os::unix::process::CommandExt;
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            command.spawn().expect("spawn the stray")
        };
        let stray_pid = stray.id();
        std::fs::write(
            dir.join(format!("tun-{dead_owner}-0a1b2c3d.ssh")),
            stray_pid.to_string(),
        )
        .expect("sidecar");

        assert_eq!(reap_orphaned_tunnels(dir), 1);
        assert!(
            wait_until(
                || stray.try_wait().map(|s| s.is_some()).unwrap_or(true),
                Duration::from_secs(5)
            ),
            "an orphaned ssh child whose command line still names the socket is terminated"
        );
        let _ = stray.kill();
        let _ = stray.wait();
    }

    /// A sidecar pointing at a process whose command line does **not** name the
    /// socket is a recycled pid, and the conservative answer is to unlink the
    /// socket and leave the process alone. Killing the wrong process costs more
    /// than a stray tunnel does.
    #[test]
    fn the_sweep_leaves_a_recycled_pid_alone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();

        let mut corpse = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn");
        let dead_owner = corpse.id();
        corpse.wait().expect("reap");

        let orphan = dir.join(format!("tun-{dead_owner}-0a1b2c3d.sock"));
        drop(std::os::unix::net::UnixListener::bind(&orphan).expect("bind"));

        // A **session leader**, deliberately: `killpg` is a no-op on a pid that
        // does not lead a process group, so an innocent spawned the ordinary way
        // would survive the sweep whether or not the evidence gate exists — and
        // the test would pass while proving nothing. Measured: without this the
        // mutation that deletes the `process_cmdline_mentions` gate survived.
        // A recycled pid that *does* lead a group is the case that matters,
        // because there `killpg` would take the whole group with it.
        let mut innocent = {
            let mut command = std::process::Command::new("/bin/sh");
            command.arg("-c").arg("sleep 5");
            // SAFETY: `setsid(2)` is async-signal-safe and is the only call made
            // between fork and exec.
            unsafe {
                use std::os::unix::process::CommandExt;
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            command.spawn().expect("spawn an unrelated process")
        };
        std::fs::write(
            dir.join(format!("tun-{dead_owner}-0a1b2c3d.ssh")),
            innocent.id().to_string(),
        )
        .expect("sidecar");

        assert_eq!(reap_orphaned_tunnels(dir), 1);
        assert!(!orphan.exists(), "the socket is still cleared");
        assert!(
            innocent.try_wait().expect("query").is_none(),
            "a process whose command line does not name the socket must survive"
        );
        let _ = innocent.kill();
        let _ = innocent.wait();
    }

    // -- the endpoint seam --------------------------------------------------

    /// A local deck establishes nothing and its address is byte-identical to
    /// the one it always had.
    #[test]
    fn opening_a_local_deck_establishes_nothing() {
        let ssh = SshProgram::at("/usr/bin/ssh").unwrap();
        let endpoint = Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock"));
        let connection = EndpointConnection::open(&endpoint, &ssh).expect("local needs nothing");
        assert!(matches!(connection, EndpointConnection::Local(_)));
        assert_eq!(connection.connect_address(), Path::new("/tmp/attach.sock"));
    }

    /// And a remote deck establishes a tunnel, whose forwarded socket is the
    /// address — handed back as a bare `&Path`, never wearing the type that
    /// would make `run_daemon_stop` compile against it.
    #[test]
    fn opening_a_remote_deck_establishes_a_tunnel_and_yields_its_socket() {
        if !python3_available() {
            println!("SKIP: python3 is not available, so no stand-in can bind a Unix socket");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let ssh = binding_standin(temp.path());
        let socket = socket_in(temp.path());
        let tunnel = RemoteTunnel::open_at(&ssh, &deck(), socket.clone()).expect("opens");
        let connection = EndpointConnection::Remote(Box::new(tunnel));
        let address: &Path = connection.connect_address();
        assert_eq!(address, socket.as_path());
    }
}
