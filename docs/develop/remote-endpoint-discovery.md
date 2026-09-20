# Remote endpoint discovery over ssh

Issue [#1174](https://github.com/vfarcic/dot-agent-deck/issues/1174). This page is the threat model for the step that decides **which socket on a remote host the desktop forwards**: what the checks establish, and — stated before anything else so nobody cites the mechanism as more than it is — what they do **not**. The code is [`remote_tunnel::REMOTE_SOCKET_PROBE`](../../src/remote_tunnel.rs) and `run_daemon_endpoint_cli` in [`src/main.rs`](../../src/main.rs), whose doc comments carry the same argument next to the code.

## What it is not

**It does not authenticate the listener, and no forwarded endpoint is self-describing.** The desktop opens the remote path with `ssh -L`, which terminates the connection locally: the peer credential visible at the near end belongs to the local `ssh` client, never to whatever is listening on the far host. So every local defence — `verify_endpoint_trusted`'s `lstat`, and the peer-uid check on the Unix connect path — passes *by construction* for a forwarded socket, correctly and uninformatively.

What the far side can establish is therefore the whole of the boundary, and it stops short of identity. A completed attach-protocol `Hello` proves something is listening and speaks this wire. It does not prove that something is the deck's daemon. An attacker **already running as the remote login user** can bind a `0o600` socket of their own at the resolved path, answer the handshake, and replace the binary the resolver itself runs from. Against that actor nothing here adds a refusal.

Nothing available on a DAC-only POSIX host closes it either, which is why the obvious fix is not simply pending. That argument is in [What in-band authentication would and would not buy](#what-in-band-authentication-would-and-would-not-buy) below, and the decision is tracked in [#1189](https://github.com/vfarcic/dot-agent-deck/issues/1189).

The same shape is recorded for a sibling boundary in [hook-socket provenance](hook-provenance.md), and for the same structural reason: a secret the legitimate party must be able to read is readable by anyone running as that party.

## Why discovery exists at all

A remote attach socket path cannot be derived from this end. OpenSSH expands neither `~` nor an environment variable on the remote side of `-L`, and the far host's `XDG_RUNTIME_DIR` and uid are not knowable locally. So the path has to be *asked for*, over the ssh round trip the desktop is making anyway, and then stored on the endpoint row so the next connection needs no probe. `RemoteSocketPath` exists because that value is stored rather than computed.

## The problem it does address

Discovery used to be a shell snippet alone — `REMOTE_SOCKET_PROBE`, a restatement of `platform::paths::attach_socket_path`'s rules in `sh`, selecting a candidate with filesystem tests and printing it. Two consequences followed from *what a filesystem test can express*, rather than from any bug in the snippet:

- **Nothing connected.** `-S` answers about an inode, and a socket inode outlives the process that bound it. A stale entry left by a `SIGKILL`ed daemon was selected exactly as a live one was, the forward came up against nothing, and a running remote deck read to the user as unavailable.
- **The mode was never checked, and cannot portably be.** `test` has no mode predicate. The issue's worst case is a listener at `0666`, and every clause the snippet can carry passes it.

Above those sat a maintenance property worth naming separately: the rule had **two implementations**, this program's and a shell restatement of it, under a doc-comment obligation to keep them in step that nothing enforced. The two drifting shows up as a discovered path that never forwards.

Issue [#1121](https://github.com/vfarcic/dot-agent-deck/issues/1121) is a neighbour, not a duplicate. It moves the temp-dir fallback into an owner-only per-uid directory and adds `-S` / `! -h` / `-O` clauses to the snippet's candidates, which closes a **foreign-uid** shadowing route. It was not offered as authentication and does not address either bullet above.

## The mechanism

The snippet's first rung runs the far host's own binary:

```sh
for dad_cmd in "${HOME:-}/.local/bin/dot-agent-deck" "$(command -v dot-agent-deck 2>/dev/null)"; do
  [ -n "$dad_cmd" ] && [ -x "$dad_cmd" ] || continue
  dad_answer=$("$dad_cmd" daemon endpoint 2>/dev/null) || continue
  [ -n "$dad_answer" ] || continue
  printf '%s\n' "$dad_answer"
  exit 0
done
# …the filesystem rungs, unchanged, as the fallback
```

`dot-agent-deck daemon endpoint` prints a path only when three things hold, and prints nothing at all otherwise:

| check | what it establishes | what it does not |
| --- | --- | --- |
| the path comes from `attach_socket_path()` | it is the path this build's daemon calls `bind(2)` on — the same function, not a copy | nothing about what is there |
| `verify_endpoint_trusted` | not a symlink, is a socket, owned by this uid, mode exactly `0o600` | that the owner is the daemon rather than another process of the same user |
| a bounded `Hello` round trip | a live listener exists and speaks the attach protocol | that the listener is the deck's daemon |

The third row is the only clause in the whole snippet that observes a **process**. The second is the only place a mode is checked at all.

The command is read-only in both directions: it never lazy-spawns a daemon (a missing one is the answer, not a reason to start one) and never unlinks the inode it refused (that is the daemon's own recovery).

### Why it costs nothing on the wire

The round trip is an existing `AttachRequest::Hello`, issued through `DaemonClient::capabilities`. So this puts nothing new on the wire and owes no `PROTOCOL_VERSION` bump — the same reasoning `daemon status` records under issue [#459](https://github.com/vfarcic/dot-agent-deck/issues/459). An older daemon answers a newer CLI's `Hello` exactly as it always did.

### Why the shell fallback stays

It is the compatibility path, and it is the common one during any rollout. A remote host whose binary predates the subcommand exits non-zero — clap reports an unrecognised subcommand — and one with no deck installed fails `[ -x ]`. Either way the `continue` drops through to the filesystem rungs, so discovery against those hosts is exactly what it was, and no host has to be upgraded before a desktop can be.

That also bounds the fix honestly: **against a host running an older build, none of the three checks above applies.** The gain is not retroactive.

### Three portability details that were measured, not assumed

The snippet is executed as a program under every `sh` the machine has by `remote_tunnel::tunnel_tests`' `the_probe_*` tests; a string assertion over the constant would pass over a syntax error or an inverted test alike. The cases below were measured under `dash` (this repo's `/bin/sh`), `bash` and `busybox sh`, which agree on all of them.

- **`[ -x "$dad_cmd" ]`, not `command -v "$dad_cmd"`, for the absolute candidate.** `command -v` on an *absolute* path answers about existence rather than executability in `dash` and `busybox sh`: `command -v /etc/hostname` exits 0 in both, and 1 in `bash`. It remains the right tool for the `PATH` candidate, where the lookup does test executability.

  **This is a clarity choice and not a defence**, which matters because the stronger claim is false and was checked: writing it with `command -v` produces the same answer in every case tested, because a non-executable file then fails to exec at rc `126` and `|| continue` catches that. The mutation was run and no test distinguishes the two spellings.

- **Both install locations, in that order.** `~/.local/bin` is where this repo's own installer writes, and is often absent from the `PATH` of a non-interactive `ssh` command; a host whose deck came from a package manager has only the `PATH` one. Trying both means an older build at `~/.local/bin` does not hide a newer one on `PATH`.

- **`2>/dev/null` on the invocation, for one narrow reason.** An older build's clap diagnostic would otherwise land in the probe's captured stderr, and `endpoint_test::discover_socket` reads that stream on its failure path to choose between two *states*: it reports `NoRemoteSocket` — "this run could not learn a path" — only when the captured stderr is empty, and `TransportFailed` otherwise. A diagnostic we expect and have already handled would downgrade an honest answer into a transport error the user cannot act on. Note how narrow that is: on the ordinary older-build path the snippet exits 0 with a path from the rungs below that parses, `discover_socket` returns before it reaches the classification, and the captured stderr is not read.

## What in-band authentication would and would not buy

This is the second fix #1174 offered, and the only one that would make a forwarded endpoint self-describing. It was not taken, for a reason about the threat model rather than about size — recorded here so it does not have to be re-derived, and open for revisiting at [#1189](https://github.com/vfarcic/dot-agent-deck/issues/1189).

**The wire is not the obstacle.** Two rungs of CLAUDE.md rule 18's graded path both avoid a `PROTOCOL_VERSION` bump: additive optional fields on `Hello` (the protocol's own written policy in `src/daemon_protocol.rs`), or a new `AttachRequest` variant gated on a capability every sender withholds until the daemon advertises it, with the check in the client library. Either is cheap.

**The credential is the obstacle.** Whatever secret the desktop verifies against, it has exactly one channel to the far host: the ssh session it is already using, authenticated as the login user. So the secret would be fetched by reading a file on the remote host owned by that user at mode `0600` — and the attacker this exercise is about is running *as* that user, so they read it too and answer correctly. The foreign-uid attacker was already excluded by the filesystem checks. The set of attackers in-band authentication would newly exclude is, on a DAC-only host, approximately empty.

**The one variant that adds something** is trust-on-first-use pinning, on the `known_hosts` model: record the remote daemon's identity on first connect and refuse a changed one. It genuinely detects a *later* substitution. It also (a) pins against a key the same-uid attacker can copy, so it catches an attacker who generates a fresh identity and not one who steals the daemon's, (b) must survive restart, upgrade and re-install or it produces false alarms that train the user to click through, and (c) needs an identity-changed UI this app is on record as unable to present usefully — it is why `UserKnownHostsFile` and `KnownHostsCommand` are deliberately left to the user's own ssh config.

What would actually bind against a same-uid attacker is a secret that uid cannot read: a hardware token, or a kernel keyring with per-process policy. Neither is available here, and agent forwarding is deliberately disabled — `ForwardAgent=no` is forced on both hops.

## What stays open

- **Same-uid impersonation on the remote host**, as above. [#1189](https://github.com/vfarcic/dot-agent-deck/issues/1189).
- **Hosts running an older build**, which fall through to the filesystem rungs and gain none of the three checks.
- **The `DOT_AGENT_DECK_ATTACH_SOCKET` and `XDG_RUNTIME_DIR` fallback rungs are still printed with no test at all** when the resolver is unavailable. That is unchanged behaviour, not a new gap, and it is bounded by the same "older build" case above.
- **The residuals `remote_tunnel::forced_options` already records** are untouched by any of this and are a different boundary: the user's own ssh config decides which host keys count as trusted (`KnownHostsCommand`, `UserKnownHostsFile`), and their `LocalForward` / `RemoteForward` / `DynamicForward` are inherited for the tunnel's whole life because `ClearAllForwardings` would clear our own `-L`.
