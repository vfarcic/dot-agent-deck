# Hook binary trust: who may change what the hooks run

`platform::paths::durable_binary_path` decides the absolute path the deck writes into `~/.claude/settings.json`, `~/.codex/hooks.json`, the OpenCode plugin and Devin's `config.json`. Those agents **execute** that path at every hook, as the user, with no further check. This page records the trust policy issue #732 settled for those paths, the options it weighed, and what the policy does not cover. The resolution order itself (install location, installed name, `~/.local/bin`, `$PATH`, last resort) is in [Troubleshooting internals](troubleshooting-internals.md).

## The policy

**A path the resolver pins must be one only the current user and root can change.** That is judged by `untrusted_writer`, which resolves the path the way the kernel does at `exec` time, one component at a time, following every symlink (at most 40), and refuses it when any entry it consults fails one of these:

- **Owner.** Every directory, every symlink and the final file is owned by the effective user or by root. An account that plants a file chooses its mode but not its owner, so this is the check that stops a plant.
- **Write permission.** `/`, every directory on the way and the final file are writable by no one else: not by every account, and not by a group unless the group is a trusted writer. A writable directory is what lets another account *replace* an entry the user owns, which the owner check alone cannot see.
- **Sticky directories are the exception.** In a sticky directory (`/tmp`, `/var/tmp`, Nix's `/nix/store`, which is `1775 root:nixbld`) only an entry's own owner can rename or remove it, and that owner is judged in turn.
- **A symlink's own mode is not judged.** It is `0777` on Linux and nothing consults it. Who can retarget a link is decided by its directory, which the walk has already judged.

**Trusted groups** (`TrustedWriters`) are the groups whose every member is already a trusted writer:

- gid 0 (`root` on Linux, `wheel` on macOS), whose membership only root arranges;
- the user's **private group**: their primary group, named after them, with no other account in it (`is_private_group`). This is the Debian/Ubuntu default, where `umask 002` makes `~/.local/bin`, `~/.cargo/bin` and the binaries `cargo install` copies there `0775`. "No other account" is read from the group's member list *and* from a `getpwent` scan for accounts whose primary group it is, because `getgrgid`'s member list omits those;
- on macOS, `admin`, whose members stock macOS lets `sudo` to root. Homebrew makes its prefix and kegs group-writable by it, and `/Applications` is `root:admin 0775`.

Every other group is untrusted, however few members it has. A shared primary group (`staff` on macOS, `users` on some Linux distributions) is exactly the other-account writer the policy exists to refuse. A lookup that fails trusts fewer groups, never more.

**It applies to every step that pins a path**, with one difference in what failing it means:

| Step | On failure |
| --- | --- |
| 1, the running binary at an install location | skipped; resolution continues |
| 1b, an installed name resolving to the running binary | skipped |
| 2a, `~/.local/bin/dot-agent-deck` | skipped |
| 2b, the first match on `$PATH` | skipped; the walk continues to the next entry |
| 3, the running binary as a last resort | **refused**: nothing is written |

Every candidate that existed but was skipped for trust is named, with its reason, in the refusal and in a warning whenever a later candidate is pinned instead, so an install the deck passed over is not a silent mystery. The refusal ends with what to do: `chmod go-w` on what it names, or install to `~/.local/bin`.

## Options considered

1. **Keep the file-mode check alone** (what PRD #381 shipped). It stops a sloppy legitimate install, not a deliberate plant, because a planter chooses the mode. It also did not narrow the time-of-check/time-of-use window at all: any account that could write the candidate's directory could replace it after resolution. And it refused the Debian/Ubuntu default, since `cargo install` under `umask 002` copies a `0775` binary.
2. **Full validation, strict on every group bit** (the auditor's literal ask). It refuses Homebrew on macOS (`/opt/homebrew/bin` is `0775 <user>:admin`) and every `~/.local/bin` on Debian and Ubuntu. Those are supported install layouts, and on them the failure would be a hard refusal or a downgrade to the last resort.
3. **Full validation with trusted writers defined as "already able to act as the user"** (chosen). Owner and ancestor checks on every component of the resolved path, symlinks followed, with the group bit judged by who is in the group.

Option 3 refuses a planted file (wrong owner), a file in or under a directory another account can write, and a link whose target is either. It keeps every supported install layout working: Homebrew on both platforms, Nix (sticky store, root-owned outputs), `cargo install`, the release binary in `~/.local/bin`, the desktop app in `/Applications`, and a scratch copy in `/tmp` that is the only deck on the machine.

**The issue's first argument, that a writable `$PATH` directory is already a full compromise, was not accepted as a reason to skip the check.** It says nothing about a `~/.local/bin` symlink whose target sits in a directory that is on no `$PATH` at all, which is the case the symlink half of the issue is about. And it does not cover time: a pin is executed at every hook from now on, so a path another account can rewrite later is a standing foothold whatever `$PATH` looked like when it was written.

## The decisions inside it

**The symlink spelling is still what gets written; the target is still not refused for being a build artifact.** Durability is a property of the *name*: a `~/.local/bin` link survives a rebuild, and a pin whose target has gone is repaired by the self-heal (`pin_is_repairable`). Trust is a property of *everything the kernel consults*: whoever can rewrite the target runs code at every hook as surely as whoever can rewrite the link. So the walk follows the link for trust and the resolver keeps writing the link. That keeps the e2e harness's seeded `~/.local/bin` link working, and it is what `durable_binary_path_still_accepts_a_symlinked_installed_candidate` pins.

**The running binary is not exempt.** PRD #381 exempted step 1 on the grounds that the user is already running that file. They started it once; a pin runs it at every hook from now on. With trusted groups covering the legitimate prefixes, the exemption no longer bought anything except that foothold.

**The file-mode check is replaced, not kept beside the walk.** `write_mode_is_owner_only` refused a group-writable file even when only the user's private group could write it, while the directory holding that file was just as writable by that group. A file rule stricter than its directory's buys nothing, and that one refused the Debian/Ubuntu `cargo install` layout.

**A pin already in an agent's config is not re-judged.** The policy governs what the deck writes. `pin_is_repairable` decides whether to overwrite a value the user may have put there on purpose, and stays at "a usable absolute executable that is not a build artifact". A pin an older deck wrote to a path this policy now refuses therefore stays until something replaces it.

## The e2e harness answer

The harness seeds `<sandbox-home>/.local/bin/dot-agent-deck` as a symlink to the binary under test, a hard link of cargo's output under the harness temp root. Under `umask 002` cargo writes that binary `0775`. The issue warned that judging the target by the old file rule would reject it, send the resolver to step 2b, and pin the operator's real installed deck from the inherited `$PATH`.

Under this policy it resolves, unchanged, on both a CI runner and a development box: the binary and every directory above it belong to the user, `/var/tmp` is sticky, and the only group-writable entries are owned by the user's private group, which is trusted. A box whose primary group is shared (`users`) *and* whose umask is `002` is different: its group-writable directories are genuinely writable by other accounts, so the resolver passes the seeded link over, and resolver tests that rely on the box's own identity fail there. That is the policy working, and the remedy is `umask 022`.

## What it does not cover

- **ACLs.** Only mode bits are read. An ACL that grants another account write is not seen.
- **Accounts a directory service does not enumerate.** The scan for accounts whose primary group is the user's private group sees what `getpwent` enumerates: local accounts, and directory accounts only where enumeration is on (it is off by default in SSSD). A directory account given the user's private group as its primary one is not seen.
- **The window between check and use is narrowed, not closed.** After resolution, only the user and root can change what the pin names; both could already run code as the user.
- **Windows.** There is no cheap equivalent, so nothing is refused there.

## Tests

The cases live in `src/platform/paths_trust_tests.rs`, which drives `durable_binary_resolution_judged` with an injected `TrustedWriters` so whether a group is trusted is the fixture's choice rather than the box's. They cover an untrusted group on the file and on its directory, a trusted group, a world-writable grandparent, a sticky ancestor, a symlink with an untrusted target, a relative `..` link as Homebrew makes, a symlink loop, another account's file, an untrusted running install, and both outcomes of the last resort.
