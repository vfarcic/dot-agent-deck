# PRD #741: Connect the desktop GUI to any daemon, anywhere, configured in the app

**Status**: Plan approved 2026-09-11. All four decisions taken by the user and recorded below; implementation started at M1. See the Work Log for what the gate conversation changed — it changed a lot.
**Priority**: High — it discharges [#819](https://github.com/vfarcic/dot-agent-deck/issues/819)'s M10, the one milestone that project moved deliberately unticked, and it unblocks [#742](https://github.com/vfarcic/dot-agent-deck/issues/742) and Windows desktop.
**Created**: 2026-09-10
**Issue**: [#741](https://github.com/vfarcic/dot-agent-deck/issues/741)
**Related**: [#819](https://github.com/vfarcic/dot-agent-deck/issues/819) (project resolution behind the daemon — the long pole, closed 2026-09-10), [#803](https://github.com/vfarcic/dot-agent-deck/issues/803) (the settings surface this stores endpoints in, closed 2026-09-04), [#801](https://github.com/vfarcic/dot-agent-deck/issues/801) (compatibility from the contract, open), [#754](https://github.com/vfarcic/dot-agent-deck/issues/754) (the platform guard reads the client's OS, open — out of scope, seam named), [#742](https://github.com/vfarcic/dot-agent-deck/issues/742) (fleet view, out of scope), [#745](https://github.com/vfarcic/dot-agent-deck/issues/745) (the overview's connection model, waiting on this document's answer), [#953](https://github.com/vfarcic/dot-agent-deck/issues/953) (the driver-level desktop tier that does not exist), [#164](https://github.com/vfarcic/dot-agent-deck/issues/164) / PRD #740 (Windows desktop), `prds/done/76-remote-agent-environments.md` (built this architecture once and deleted it).

## Problem Statement

The desktop app can only talk to a daemon on the machine it is running on, and the reason is that it models a daemon as **a filesystem path**. `config::attach_socket_path()` returns a Unix socket path or a Windows named pipe (`src/platform/paths.rs:1269`), `DaemonClient` holds a `PathBuf` (`src/daemon_client.rs:470`), and the desktop's connection DTO puts that path on the wire to the webview as a `String` (`desktop/src-tauri/src/dto.rs:45`).

Remote attach nevertheless **works today**, measured end to end on 2026-08-29 against a real remote daemon — nine agents listed, PTYs streaming, hook events arriving. The issue records the recipe and the evidence; this document does not re-argue it. What matters is *how* it works: `ssh -N -L` projects the remote daemon's socket onto a client-side path and the client never learns it is talking to another machine. **Transport is not the obstacle. The disguise is.**

Three properties of the current client are load-bearing on that disguise, and each is a defect in its own right once the endpoint stops pretending to be local. Two of them are the issue's; the third the reconnaissance found and the issue does not mention.

**The trust check does not say what it appears to say.** `verify_endpoint_trusted` (`src/platform/fsperm/unix.rs:181`) requires a socket owned by our uid at mode exactly `0o600`. Over a forwarded socket both clauses pass trivially and describe the **local `ssh` client process** — not the remote daemon, not the remote host, not the remote user. The premise the check was written against is a same-uid attacker squatting a local inode before the daemon binds; that premise says nothing whatever about who terminates a tunnel. File mode is not peer authentication, and a check that reads like one is worse than no check, because the next reader assumes the endpoint was authenticated. Enumerated at HEAD: **on Unix nothing authenticates an attach peer at all** — `peer_pid` (`src/platform/peercred/unix.rs:17`) is used for termination only, in its two consumers `src/daemon_stop.rs:184` and `src/build_version_handshake.rs:362`/`:399`; the attach server has no peer check; and the wire says of the one thing that looks like one that it is "compatibility metadata, **NOT authentication**" (`src/daemon_protocol.rs:1354`). `ssh_config(5)` adds a second, independent reason the mode clause is not a security story: *"not all operating systems honor the file mode on Unix-domain socket files."*

**Every state refresh reconnects.** One `get_snapshot()` costs a `stat` plus **two** fresh connections on Unix and **three** on Windows, and nothing is pooled — `DaemonClient`'s own doc says "every operation opens its own short-lived `IpcStream`" (`src/daemon_client.rs:465`). The snapshot watcher re-runs it on every daemon event, floored at `SNAPSHOT_COALESCE_INTERVAL = 150ms` (`desktop/src-tauri/src/lib.rs:75`). Over a Unix socket that is free. Over a network hop each is a round trip.

**And "Stop daemon" would kill the tunnel while reporting success.** This is the sharpest finding of the reconnaissance and it is not in the issue. `run_daemon_stop` resolves the daemon's PID from `SO_PEERCRED` on the socket (`src/daemon_stop.rs:184`) and SIGTERMs it. Over an `ssh -L` forwarded socket **that PID is the local `ssh` client's**. Both desktop buttons reach it — `DesktopAction::StopDaemon` (`desktop/src-tauri/src/lib.rs:1176`) and **Replace daemon** (`:1187`). So today, against a forwarded endpoint, "Stop" terminates the tunnel and renders *"Daemon stopped gracefully (pid N)"*, and "Replace" then lazy-spawns a **local** daemon and renders *"Daemon replaced with the desktop's matching bundled build."* Two false-success paths, and per CLAUDE.md rule 15 / [#770](https://github.com/vfarcic/dot-agent-deck/issues/770) a mistaken stop of a *remote* daemon orphans orchestration role maps that live in that daemon's memory and nowhere else — on a machine the operator may not own.

The app also cannot be launched the way a desktop app is launched. The measured recipe passes `DOT_AGENT_DECK_ATTACH_SOCKET` and `DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH` on the command line; a bundle double-clicked from Finder has neither. Connection details have to live in settings, and **the app has to own the connection itself**.

## The governing principle, and what it has already bought

Recorded on #741 on 2026-09-02, adopted by #803 and discharged for project state by #819:

> **The desktop app gets everything from the daemon, wherever that daemon runs. The only thing it owns is its own settings.**

#819 made that true for project state: `desktop/src-tauri/src/lib.rs:124`'s client-side `load_project_config` and `:175`'s local `orchestrator-context.md` write are **gone** — `:175` is now the daemon call `prepare_workflow(...)`. Four new attach verbs (`ListProjects`, `ResolveProject`, `PrepareWorkflow`, `StartPreparedAgent`) ride `PROTOCOL_VERSION` 9, `AttachResponse::capabilities` (`src/daemon_protocol.rs:1360`) advertises them, and a linkage-check tripwire (`xtask/linkage-check/src/desktop_project_boundary.rs`, "check 12") fails the build on a client-side project read.

**This document is the other half of the same sentence: *wherever that daemon runs*.** #819 made the client stop reasoning about a filesystem it is not on; #741 makes the client able to reach a daemon it is not co-located with, and makes the local case keep exactly today's guarantees while it does.

## What the reconnaissance changed

Two read-only agents checked every seam the issue names against this worktree. Six of the issue's claims did not survive; one problem it does not mention is the most consequential thing here. Recorded so a reader does not re-derive them, and so the issue's own text is not quoted as current.

| the issue says | what is true at HEAD |
|---|---|
| "ssh creates the forwarded socket under the ambient umask, so it arrives `0o755` and is rejected" | **FALSE as a mechanism.** `StreamLocalBindMask` defaults to `0177` → `0o600`, exactly what the check wants; the ambient umask is not consulted. Measured: `ssh -G localhost` → `streamlocalbindmask 0177` on OpenSSH_10.2p1 with `umask 0002`. The *conceptual* half — file mode is not peer authentication — survives untouched and is the load-bearing one. |
| "An SSH channel is one more `AsyncRead + AsyncWrite`" | **Half true, and the important half is false.** The five framing helpers genuinely are generic (`daemon_protocol.rs:649`/`:691`, `daemon_client.rs:134`/`:145`/`:194`). Three things are not: `AsRawFd` + `SO_PEERCRED`, the `SHUT_WR`-on-write-half-drop the attach protocol is documented to depend on (`src/platform/ipc/mod.rs:19-25`), and concrete `IpcReadHalf`/`IpcWriteHalf` in the long-lived structs. |
| "decide [the folder picker] before someone clicks Browse" | **STALE — there is no Browse button and no picker.** No `@tauri-apps/plugin-dialog`, `tauri` is `features = []`, capability set is `["core:default"]`. #819 M6 already made project selection daemon-sourced with a paste-a-path fallback — which *is* the UX the issue proposed as the alternative. The decision was made by construction. |
| "the three attach verbs" (#819 comment) | **Four**: `ListProjects`, `ResolveProject`, `PrepareWorkflow`, `StartPreparedAgent`. |
| `daemon_bridge.rs:101` is the build-stamp classification | STALE → `classify_handshake` at `:205`, stamp arm at `:237`. (`:230`, the protocol equality check, is exact.) |
| six further line numbers | STALE, corrected inline throughout: `daemon_client.rs:309`→`:507`, `connect.rs:753`→`:781`, `agent_pty.rs:988`→`:1133-1184`, `dto.rs:493`→`:899`, `lib.rs:821`→`:1107`, `lib.rs:69`→`:75`. Still exact: `fsperm/unix.rs:181`, `fsperm/windows.rs:260`, `paths.rs:1269`, `connect.rs:552`, `release.yml:522`, `daemon_bridge.rs:230`. |
| — *(not in the issue)* | **`peer_pid`-based termination misroutes for a remote endpoint** and reports success. See the Problem Statement. This is the one finding that changes the shape of the design rather than a number in it. |
| — *(not in the issue)* | **A `String` field cannot be added to the desktop settings schema.** `ALLOWED_FIELD_TYPES` (`xtask/linkage-check/src/desktop_settings_secrets.rs:104-136`) holds exactly five entries and `String` is deliberately absent; the guard runs in the **required** `build` job. Endpoint storage therefore needs validating newtypes — which conveniently is also the ssh-argument validation that `SshTarget::parse` (`src/remote.rs:50-61`) performs none of. |
| — *(not in the issue)* | **`verify_endpoint_trusted`'s refusals are pinned by no test anywhere.** Its `mod tests` covers only `ensure_owner_only_dir`; the two tests naming it assert its *signature* and its *audit row*. The accept path is covered indirectly by every e2e deck launch; wrong mode, foreign uid and a regular file at the path are covered by nothing. |

## Solution Overview

**The endpoint stops being a path and becomes an addressable thing with a kind.**

```rust
enum Endpoint {
    Local(PathBuf),         // today's behaviour, today's trust check, byte-identical
    Remote(RemoteEndpoint), // the app opens the connection and speaks the attach protocol over it
}
```

Two properties follow, and they are the whole point:

- **Trust becomes a property of the endpoint kind**, not a `stat` on a path. `Local` keeps uid + mode `0o600`, unchanged and — for the first time — tested. `Remote` gets ssh host-key and user authentication. Neither pretends to be the other, and the operations whose safety rests on locality (`peer_pid` termination, the stale-inode unlink at `src/daemon_attach.rs:173`) become **structurally unreachable** for `Remote` rather than gated by an `if`.
- **The connection is established once** rather than per call — which is what makes a network hop affordable at all, and which is the question [#745](https://github.com/vfarcic/dot-agent-deck/issues/745) is explicitly waiting on.

Endpoints live in the #803 settings document, following that PRD's registration contract rather than a second mechanism invented here.

**What this document does not do**: it does not build the fleet view (#742 — one endpoint at a time), it does not ship Windows desktop (#754 is a genuine prerequisite for that and is open), and it does not close #801.

## Scope

### In Scope

- **The `Endpoint` abstraction**, with `Local` byte-identical to today and the local-only operations unreachable for `Remote` by type rather than by branch.
- **Unit tests pinning `verify_endpoint_trusted`'s refusals**, before anything refactors it. They do not exist today and they are the only thing that would catch a `Remote` split silently loosening the local guarantee.
- **A remote transport** — `ssh -N -L`, app-managed (DECISION 1 below).
- **A connection held open across refreshes**, replacing connect-per-request on the desktop's path — and the answer #745 needs, written down here.
- **Endpoint storage in `desktop.toml`** via #803's contract, using validating newtypes rather than widening `ALLOWED_FIELD_TYPES`.
- **An endpoint settings panel**: add, name, select, remove; local is the default and is present without configuration.
- **A compatibility policy for a remote daemon** that does not assume you can restart it — the #801 slice this needs and no more, with what is left to #801 stated.
- **The `save_to` hardening this PRD is the *named* trigger for.** `desktop/src-tauri/src/settings.rs:745-759` declines parent-directory validation and says in as many words: *"If this document ever holds something security-relevant — a daemon endpoint under #741 … that calculus changes and the anchoring should be revisited."* Answered here, either by taking it or by recording why not.
- **Bidi-safe rendering of the endpoint label.** `dto.rs`'s `safe_message` strips control characters but **not** bidi format characters, and the repo has the right tool unused here (`src/untrusted_text.rs:114`). The connection footer's whole job is telling the user which daemon they are talking to, so a U+202E in a settings-supplied host is the one place a mis-rendered endpoint has a direct security consequence.

### Out of Scope

- **The fleet view (#742).** One endpoint at a time. `DesktopState` becoming a keyed map is #742's job. This document makes it cheap by putting the endpoint in one place; it does not start it.
- **Windows desktop.** The abstraction is what makes it nearly free, and that is a reason to keep the abstraction honest rather than a reason to build Windows now. It also needs **#754**, which is open. The seam is named in Technical Approach; `.github/workflows/release.yml:522`'s exclusion comment is the thing that would change, and it should change in the PRD that can prove a window works.
- **#754 itself** — putting the daemon's OS in the handshake. That is a rule-12 contract change with its own cross-version test, and folding it in here doubles this document's protocol surface for a case it cannot ship anyway.
- **Moving command construction daemon-side.** `desktop/src/lib/profileCommands.ts` still POSIX-quotes a string the daemon runs through its own shell (`src/agent_pty.rs:1133-1184`). Same root cause as project resolution, same fix direction, not this PRD — named in Technical Approach so it is not lost.
- **Closing #801.** This takes the slice a shippable remote build-stamp verdict needs and says what it leaves.
- **A driver-level desktop test tier.** #953. See Testing for what the tiers that *do* exist can and cannot reach.
- **`SecretStore`.** #803 M5 named the seam and deliberately did not build it, and `docs/develop/desktop-gui.md:472` records the property that holds while it does not exist: *"no route in exists at all."* **#741 must not be the PRD that opens the first route** — which is exactly why the storage policy below stores references and never secrets.

## Technical Approach

### DECISION 1 — the transport, which the issue says is this PRD's first decision

The issue states the trade and declines to settle it: shell out to the system `ssh` (cheap, delegates auth to the user's existing ssh config, a child process per endpoint, weaker on Windows) versus an in-process client such as `russh` (a real new dependency with a real audit surface, and `security` / `cargo audit` is a **required** check here).

**The audit half is now measured rather than estimated**, and it is decisive against `russh`:

| variant | packages | new to this repo | `cargo audit` | `cargo check` |
|---|---|---|---|---|
| baseline (this tree) | 615 | — | **exit 0**, zero vulnerabilities, 8 informational | — |
| `russh` default features | 181 | **81** | **exit 1** — RUSTSEC-2023-0071 | not attempted |
| `default-features = false, ["aws-lc-rs","flate2"]` | 178 | 78 | exit 0 | not attempted |
| `default-features = false, ["ring","flate2"]` | 183 | 78 | exit 0 | exit 0 |
| pure Rust (no crypto backend) | 170 | — | — | **exit 101** |

- `rsa` is a **default** feature of russh 0.63.3, pinned `=0.10.0-rc.18`. RUSTSEC-2023-0071 (Marvin timing attack) has `patched = []` — **there is no version to bump to**, and the advisory's own workaround is "avoid using the `rsa` crate where attackers can observe timing", which is a network SSH client exactly.
- Getting past it means either dropping RSA — silently losing `ssh-rsa`/`rsa-sha2-*` host keys and RSA user keys, still common on corporate and older hosts, for precisely the users most likely to have a remote daemon — or writing this repo's **first** `.cargo/audit.toml` ignore. That is a durable weakening of a currently-clean required gate in exchange for a feature.
- **A pure-Rust russh is impossible**, measured: `compile_error!` demands `ring` or `aws-lc-rs`, both native code. So it also adds a C toolchain requirement to `build`, `build-windows`, `build-macos` and to `flake.nix`.
- Of the 81 new crates the overwhelming majority are `0.x` cryptographic primitives, and russh pins **two release candidates** as hard `=` requirements (`ssh-key = "=0.7.0-rc.11"`, `rsa = "=0.10.0-rc.18"`), so the tree cannot take a patch of either without russh cutting a release. This repo's entire crypto exposure today is zero.
- The wider tree is otherwise clean at these versions — `russh`/`russh-cryptovec` RUSTSEC-2026-0153/0154 are patched at ≥ 0.60.3, and `aws-lc-sys`'s five 2026-03 advisories are patched below the resolved 0.45.0.

**And auth is the hard part, which the system `ssh` already solves**: `Host` blocks, `Include`, `Match`, `ProxyJump`, `IdentityAgent`, FIDO/`sk-` keys, PKCS#11, GSSAPI, certificate auth, `known_hosts` with hashed names and `@cert-authority`. Reimplementing the *config* half of that is a project, not a dependency. Delegating it is `src/remote.rs`'s existing bet and it has held.

So `ssh` — but **the honest cost of `ssh` is that OpenSSH has no stdio-to-remote-*Unix-socket* forwarding** (`-W` is `host:port` only). That leaves two shapes, and they are not equivalent:

**Option 1A — `ssh -N -L`, app-managed.** Today's proven recipe, with the app owning the child. **Cost:** it puts a local socket path back on the client, which is precisely what `Endpoint` exists to eliminate for the remote case — so the trust check keeps being asked a question it cannot answer, Windows stays excluded (no Unix socket), and three new failure modes arrive: `StreamLocalBindUnlink` defaults to `no`, so a second tunnel against an existing socket file **fails to forward at all** rather than replacing it; the app must therefore unlink a path it chose, which is a deletion to be careful about; and if the app is SIGKILLed the `ssh` child survives holding that socket, after which `verify_endpoint_trusted` **passes** on the next launch against an orphaned tunnel pointing at a daemon that may be gone — a false-healthy state the current check cannot detect by construction.

**Option 1B — `ssh <host> dot-agent-deck daemon proxy-stdio`, a new deck subcommand. RECOMMENDED.** The remote binary connects to its own local attach socket and pipes it to stdio; the ssh child's stdin/stdout **is** the transport. Verified absent today (`grep -rn "proxy-stdio\|ProxyStdio\|proxy_stdio" src/ desktop/` → 0 hits), so it is new work — but there is direct precedent one line away: `DaemonCmd::Hello` (`src/main.rs:375-380`) exists **specifically** "to detect wire-format skew across an ssh hop without spawning the remote daemon". Same shape, same hop, same reason.

It dissolves four problems at once, which is why it is the recommendation rather than the cheaper one:

- **No local socket anywhere on the client for the remote case**, so the `Endpoint` abstraction actually delivers what it promises instead of hiding a path behind an enum arm. No `StreamLocalBindUnlink`, no chosen-path unlink, no orphaned-tunnel false-healthy state.
- **It is a plain pipe pair, which is what the generic framing helpers want.** `tokio::process::Child`'s `stdin`/`stdout` are `AsyncWrite`/`AsyncRead`, so the five helpers take them today.
- **It is the Windows answer at no extra cost.** A Windows client never needs a named pipe to reach a *remote* daemon, and Windows 10+ ships OpenSSH. Windows desktop still needs #754 and stays out of scope, but this stops being the thing blocking it — which is exactly the claim `release.yml:522` records as currently false.
- **`PROTOCOL_VERSION` does not move.** A subcommand is not a wire change; the bytes over the pipe are the attach protocol unchanged.

Its own costs, stated: a remote daemon whose binary predates the verb fails with `unknown subcommand` — loud, not silent, and the same class as a protocol floor; it needs the remote binary's path resolved, for which `src/connect.rs`'s existing `install_path` machinery is the precedent; and one process per endpoint either way.

> **DECIDED: 1A (`ssh -N -L`, app-managed). 1B is filed as the follow-up that removes the local socket and unlocks Windows.**
>
> **The reversal happened at the gate, and the reason was arithmetic nobody had done.** The desktop holds *concurrent* connections, not one: roughly **one attach socket per shown terminal** (a nine-tile deck is nine, plus a warm set bounded at 3), **plus** one long-lived event subscription, **plus** transient request connections — call it ~11 at once. Under 1B each of those is a separate `ssh` child with its own TCP connection and its own authentication, and each is an `exec` session, so they count against sshd's `MaxSessions` (**default 10**) — which lands squarely inside our range. Under 1A a single `ssh` child carries all of them as **forwarding channels** over one authenticated connection, and forwarding channels are not sessions, so `MaxSessions` does not apply. That is also what the 2026-08-29 measurement actually did, **with nine agents**, successfully.
>
> `UNVERIFIED:` the `MaxSessions`-counts-exec-sessions-but-not-forwarding-channels claim is recalled OpenSSH semantics, not a measurement taken here. It is not load-bearing for 1A (which avoids the question) but it **is** the thing that would have to hold before 1B ships, so whoever picks up 1B measures it first.
>
> **And the local socket stops being the defect it is today, for a reason worth stating precisely.** The disguise was harmful because the client *did not know* it was remote. Under `Endpoint::Remote` it knows: the transport owns that socket, so we do not run the local uid+mode trust check against it, we do not read its presence as health, and we own its lifetime. It becomes an implementation detail of the remote transport rather than a false claim about the endpoint's nature. Today's "it looks local so treat it as local" is the thing being fixed — not the existence of a socket.

**Either way, these are not optional** — each is a measured cost of shelling out, not an argument against it:

- Resolve `ssh` by **absolute path** at startup, never bare `"ssh"` from `PATH`. A GUI launched from Finder or a `.desktop` entry inherits launchd's / the session manager's environment, and a user-writable directory earlier in `PATH` can substitute a binary. (`src/login_shell.rs` exists because this repo already has this problem.)
- Force `BatchMode=yes`. A GUI has no tty: without it ssh tries to read the host-key prompt from a closed stdin and refuses anyway, but the app gets an unclassified failure instead of a classified one — and on a Linux desktop with `SSH_ASKPASS` and `DISPLAY` set, ssh may pop **an unrelated third-party dialog the app did not draw**.
- Reuse `apply_observation_options` (`src/remote.rs:575-592`) minus `ClearAllForwardings`. That function has **ten** options, so **nine** are inherited, each with a written reason, including the sharp one: a `Host *` block carrying `ForwardAgent yes` otherwise exposes the laptop's ssh-agent to the endpoint. (M5's audit corrected an arithmetic slip here and in the code: nine inherited, not ten.)
- **Never** set `StrictHostKeyChecking=no` or `accept-new`. That converts a first-contact decision into silent trust-on-first-use from an app that cannot show a fingerprint. Surface `SshError::HostKeyVerificationFailed` (`src/remote.rs:113`) as a first-class UI state with its existing remedy — run `ssh <target>` once in a terminal. **M5's audit went further than this line asked (finding A2): the tunnel now forces `StrictHostKeyChecking=yes`.** Leaving it unset meant the *user's* config decided, and a `Host * / StrictHostKeyChecking no` line left a long-lived channel the GUI presents as trusted with no host-key check at all. Forcing `yes` costs nothing against the default, because `BatchMode=yes` already makes `ask` fail on an unknown key. The remedy string was also wrong and is fixed: it dropped the port and never carried `-J`, so it named a different endpoint than the one that failed (finding A5).
- Own the child's lifetime: process group, reap, and kill on quit **including** the paths where `Drop` does not run. There is no precedent in the tree — both existing `Command::new("ssh")` sites (`connect.rs:781`, `remote.rs:438`) block on the child — so supervision, restart and teardown are genuinely new work.

### DECISION 2 — does this ship behind the `experimental` flag (CLAUDE.md rule 9)?

**DECIDED: no.** The recommendation was no and the user confirmed it. The reasoning, which is not a fresh judgement: #803 Open Question 1 decided the same for the settings surface, and the mechanical reason it gave applies unchanged: **the flag does not reach the desktop app by any route.** Nothing under `desktop/` mentions it, it is not on the daemon protocol, and the desktop crate never calls `features::init_and_watch` — so `experimental_enabled()` would read its `false` default forever whatever the TOML or the env said. `prds/176-desktop-gui.md` decision 6 records the prior: "a separate GUI binary has no such seam — the act of building/running it is the opt-in", with maturity handled by packaging. Gating this would mean **building the flag's Tauri delivery mechanism as part of this PRD**, and would raise "against which project directory?" for a packaged build — a question the desktop app has no good answer to. The user's call.

### DECISION 3 — the Deck selector, and why #742 stays a separate PRD

**DECIDED by the user at the gate, and it is a better answer than either option this document originally offered.**

The question was whether #741 can ship without [#742](https://github.com/vfarcic/dot-agent-deck/issues/742) (fleet view). The argument for combining them is real and #742's own body makes it: *"this is strictly a superset: multi-daemon without remote is just multi-local"*, *"partial failure becomes normal … the current single `ConnectionStatus` cannot express that"*, and *"build-stamp policy gets harder — see the stamp discussion in #741"*. The app's screens are **fleet** screens, not connection screens; landing on one that shows half your agents because of a connection setting is the misleading half-experience `prds/done/76-remote-agent-environments.md` deleted ~1500 lines to escape.

**The resolution: the agent-activity screens get a Deck selector.** With one endpoint configured it works and is testable, so #741 ships alone; #742 then adds an **"All Decks"** option to *the same control* rather than replacing it.

This is not scaffolding, which is the objection it has to answer. A selector is permanent UI — even with a full fleet view you want to focus on one deck — so #742 adds an option rather than removing a surface. What it buys is that #742's hardest open question, in its own words *"whether agents from different daemons ever appear in one view or stay segregated"*, stops being something #741 has to pre-answer and becomes something that plugs into a defined seam.

**One design constraint makes "All Decks" additive rather than a retrofit**, and it is cheap now and expensive later: the selection is a value that can **grow a variant** — `Selection::One(EndpointId)` with room for `Selection::All` — not a bare `EndpointId` threaded through the state.

**What stays #742's, named so it is not a surprise:** the `DesktopState` singleton (`desktop/src-tauri/src/lib.rs:997`) becoming a keyed map; `ConnectionStatus` going from single-global to per-endpoint (coherent as a global while exactly one deck is selected — partial failure is what breaks it, and that only becomes real with "All Decks"); and whatever "All Decks" renders. M14 records this on #742 so it inherits the seam rather than rediscovering it.

### DECISION 4 — one PR, not two

**DECIDED: one PR.** This document originally recommended splitting at the M4/M5 seam, where M1–M4 change no user-visible behaviour. The user's reasoning is better: **a PR whose behaviour cannot be validated is a PR approved on trust**, which is the opposite of what the review gate is for. The milestones still land as separate commits, which recovers most of what the split was buying — a usable bisect — without asking anyone to sign off on something invisible.

### Naming: the UI says Deck, the code says daemon

**DECIDED by the user: no user-visible surface calls it a "daemon".** It is an **Agent Deck**, or a **Deck**.

The rule, stated so it is applicable without re-deriving it: **rendered text says Deck; code, protocol, CLI, docs, CSS class names and `data-testid`s keep `daemon`.** `dot-agent-deck daemon serve` is unchanged, `DaemonClient` is unchanged, and renaming testids would churn tests for no reader's benefit.

Sized before being agreed to: **~40 user-facing strings across 22 non-test files, with 12 test files asserting on them.** It is a **copy pass, not a find-replace** — several strings need rewriting rather than substituting, because they distinguish the app from the process it manages (`"Agent Deck will start its local daemon process"` does not become `"its local deck process"`). The whole sweep ships in this PR as M15 rather than being limited to new surfaces: a selector labelled **Deck** beside a button labelled **Stop daemon** is worse than either end state, and the 12 test files are the safety net that catches misses.

### The performance question, which the user named as their main concern

**The current design does load the deck's daemon process, the user was right about it, and the first draft of M4 only fixed half of it.**

Measured shape, from reconnaissance: one `get_snapshot()` is 1 `stat` + **2 fresh connections** (`hello`, then `list_agents`), nothing pooled — `DaemonClient`'s own doc says *"every operation opens its own short-lived `IpcStream`"* (`src/daemon_client.rs:465`). `ensure_snapshot_watcher` (`desktop/src-tauri/src/lib.rs:758-802`) re-runs it on **every** daemon event, coalesced to at most one per `SNAPSHOT_COALESCE_INTERVAL` (150ms, `:75`). So the ceiling is ~6.6 refreshes/sec → **~13 connections/sec**, each one accept → task spawn → parse → **serialize the entire agent list** → tear down.

Two halves, and they need different fixes:

- **Connection churn** — removed by holding the connection once.
- **Full-snapshot-per-event** — *not* touched by that. The daemon would still serialize the whole agent list up to 6.6×/sec, which on a fifteen-agent fleet is the expensive half.

**So M4 covers both**: hold the transport, *and* apply the daemon's pushed events incrementally, re-syncing fully only on reconnect or a slow floor.

**Remote does not make the daemon's load worse**, and saying so precisely matters because it is the question that was asked: with `ssh -L` the daemon sees an ordinary local socket connection and cannot tell the client is remote. What remote changes is that the existing inefficiency stops being free — 13 round trips/second over a network link is unusable. **The fix is required for remote and is a straight win for local**, which is the good kind of coupling.

**M4's done-condition is a measurement, not a claim**: daemon-side accepts/sec and bytes/sec against a fleet of 8–15 agents, before and after, by the same method.

#### The baseline, measured 2026-09-11 — and it corrects this document's own framing

Taken from unmodified code by a harness (`examples/perf_baseline_probe.rs`) that binds the **production** `run_attach_server_with_counter` over a Unix socket, spawns N real `cat` PTY agents through the attach protocol, and seeds each with a realistic `ToolStart` event so the `ListAgents` join returns records the size a busy fleet actually produces.

| what | measured |
|---|---|
| connections per refresh (Unix) | **2** — `hello` + `list_agents`. The `verify_endpoint_trusted` `stat` is not a connect. Windows would be 3. |
| the subscribe stream | **one persistent connection, not churned** — the watcher holds it open and re-runs `get_snapshot()` per event |
| refresh ceiling | **≤6.667/s** (the 150ms coalesce floor) ⇒ **≤13.33 desktop-attributable connections/s**, independent of event rate |
| bytes per refresh | **~644 B/agent, linear** — 5.2 KB at 8 agents, 9.7 KB at 15 ⇒ ~34–64 KB/s at the ceiling |
| `list_agents` latency, local socket | median **82 µs**, p95 96 µs ⇒ ~11,600/s single-threaded |
| watcher firing in practice | idle ≈ **0/s**; a busy fleet pins at the 6.667/s ceiling |

**The correction: on a local socket the daemon is not meaningfully loaded, and this document said otherwise.** 13.33 connections/s against ~11,600/s of capacity is **under 0.12% of one core**. The design is genuinely wasteful — it re-serializes the entire agent list up to 6.667 times a second and there is no delta anywhere — but "wasteful" and "loading the daemon" are different claims, and only the first is true locally. Stated plainly because the wrong version was written here first and would otherwise be quoted.

**What the measurement does justify is the remote case, and it justifies it more sharply than the load argument did.** Each of those 13.33 connections is an 82 µs loopback call locally and a **full network round trip** over an ssh hop. That is the number that makes the current design unusable remotely — latency, not daemon CPU. M4 is therefore required for remote and is a waste reduction locally, which is the honest ordering of its two benefits.

#### M4(b) is feasible, and narrows — the event stream is not quite sufficient on its own

Enumerated against the three `BroadcastMsg` variants rather than assumed:

- **Status / active-tool / prompt / cwd transitions: sufficient.** These are exactly the fields the daemon's own `apply_event` folds into `SessionSnapshot`, so the client can replicate that fold. This is the high-frequency case — the one pinning the watcher to its ceiling today — so applying it incrementally removes essentially all steady-state traffic: **2 → 0 connections per refresh**, and bytes from a 5–10 KB full list to the one ~0.3–0.6 KB event frame that changed.
- **Two transitions genuinely need a fetch**, and they are why M4(b) is *hold-open + incremental-apply + a bounded reconciliation floor* rather than pure event application. **(1) A newly-added agent's row cannot be completed from events** — `display_name`, `tab_membership`, `rows`/`cols` and `spawned_at_ms` are registry facts absent from `AgentEvent`. **(2) A silent removal is not signalled at all** — a cooperative agent emits `SessionEnd`, but process death is not guaranteed to, and `agent_pty.rs` notes that a lost `SessionEnd` leaks the session. A crashed agent can therefore disappear from the records with **no removal event**. *(Corrected while building M4(b): the cooperative/crashed distinction does not exist. Records come off `agent_records()`, which filters on the `exited` flag the PTY reader sets at EOF, and no `BroadcastMsg` is sent from that path at all — a `SessionEnd` retires a *session*, never a *record*. So **every** disappearance is silent, which is what sets the reconciliation cadence rather than merely justifying its existence. See the 2026-09-11 M4(b) Work Log entry.)*
- **No protocol change is required.** The events already flow; only the client's application of them changes. `PROTOCOL_VERSION` stays 9.
- **The "after" number must state the reconciliation cadence**, because the incremental model's remaining load is dominated by it. A figure without the cadence is not comparable to the baseline.

### The remote leg still needs a second machine

### The `Endpoint` type, and what `Local` must preserve

The blast radius is counted rather than guessed: **~26 non-test lines across 8 files in 2 crates** — 14 on the desktop's own path (`daemon_bridge.rs`, `dto.rs`, `lib.rs`, `terminal.rs`) and 12 in `src/` (`daemon_client.rs`, `daemon_stop.rs`, `build_version_handshake.rs`, `daemon_attach.rs`). The TUI/CLI's ~9 sites (`src/main.rs` ×8, `src/ui.rs:909`) **do not have to move** if the enum is introduced behind a `Local`-preserving constructor — that is the difference between a ~26-line change and a ~40-line one and it is worth designing for deliberately.

Seven things `Local` must preserve so it keeps exactly today's guarantee, all reachable from the reconnaissance:

1. The four-part predicate at `fsperm/unix.rs:186-202`, unchanged, including `!= 0o600` rather than a mask.
2. Its **position**: out-of-band and before the first connect, at `daemon_attach.rs:160`/`:186` and `daemon_bridge.rs:340`.
3. `ENDPOINT_IS_FILESYSTEM_PATH` (`ipc/mod.rs:67`) gating — the Windows arm must keep taking the connect-probe branch or lazy-spawn times out forever (`daemon_attach.rs:149-157`).
4. `IpcListener::bind`'s umask-before-`bind(2)` dance (`fsperm/unix.rs:26-37`), so the inode never briefly exists world-readable.
5. Windows' client-side owner-SID verification at both entry points.
6. The stale-inode dance (`daemon_attach.rs:158-177`). The `remove_file` at `:173` is only safe *because* the trust check just proved the inode is ours — **a `Remote` arm must not inherit that unlink.**
7. **`peer_pid`-based termination stays `Local`-only, as a type-level impossibility rather than a runtime `if`** — `Endpoint::Local` being the only thing that can produce the value `run_daemon_stop` takes. The desktop's Stop and Replace buttons are disabled for a remote endpoint with an explanation, because "stop the daemon on a machine I don't own" is not obviously something the button should do even once it *can*.

### The transport seam, sized honestly

The framing layer is genuinely generic and an SSH channel or a pipe pair satisfies all five signatures today: `read_frame` (`daemon_protocol.rs:649`), `write_frame` (`:691`), `send_request` (`daemon_client.rs:134`), `read_response` (`:145`), `issue_command` (`:194`). **Zero changes there.** What is concrete is small and structural, ~4 sites:

1. `DaemonClient::connect()` (`daemon_client.rs:507-509`) returns a concrete `IpcStream`.
2. `EventSubscription`'s two half fields (`:1342`, `:1349`).
3. `AttachConnection`'s two half fields and `into_split()` (`:1420-1421`, `:1491`).
4. `desktop/src-tauri/src/terminal.rs:25`'s `Arc<AsyncMutex<IpcWriteHalf>>` — a concrete IPC type in a long-lived per-pane struct.

Boxing (`Pin<Box<dyn AsyncWrite + Send>>`) is the cheaper answer at one vtable dispatch per PTY frame; genericising propagates a type parameter through the session registry and three connection types.

**The half-close is the trap, and it is documented as one.** `ipc/mod.rs:19-25` records that an earlier draft used `tokio::io::split` and its generic write half does **not** `SHUT_WR` on drop, *"silently regressing the attach half-close on Linux/macOS"*; `EventSubscription` holds `_wr` purely so its drop trips the daemon's disconnect detector (`daemon_client.rs:1343-1349`). **Narrowed, because the code narrows it:** `ipc/windows.rs:157-164` shows the protocol already runs without per-half half-close on Windows, so this is "must be designed, not assumed" rather than "impossible" — a custom write half whose `Drop` signals EOF.

Four predicates assume the endpoint has filesystem presence and each needs a third answer rather than a boolean: `ENDPOINT_IS_FILESYSTEM_PATH`, `stale_endpoint_artifact`, `remove_stale_endpoint`, and `DaemonClient::ensure_socket_exists` (`daemon_client.rs:500-505`). **Genuinely out of the radius**, verified: `IpcClient`'s raw `libc::socket`/`connect` path serves the **hook** socket (`src/hook.rs:596`, `:629`, `:785`) and the TUI's blocking request path (`src/ui.rs:916`), not the desktop's attach path.

### The answer #745 is waiting for: how a remote connection is held open

#745's recorded decision says its connection model and interval must not be finalised before this PRD answers this. The answer, with the finding that makes it cheaper than it looks:

**Nothing owns a long-lived client today. `DesktopState` (`desktop/src-tauri/src/terminal.rs:33-38`) holds no `DaemonClient` at all** — its four fields are the session map, the attach gate, a generation counter and a watcher flag. Every `trusted_daemon()` builds a fresh `DaemonClient::new(socket_path)` (`daemon_bridge.rs:352`) and drops it; there is no reconnect, only construct-use-drop.

**The barrier is not mutability.** All 17 request methods take `&self`, not `&mut self`, and the capability cache is already `Arc<Mutex<…>>` shared across clones (`daemon_client.rs:481`) — so a shared `Arc<DaemonClient>` is not blocked. The barrier is exactly two things: `connect()` reconnecting unconditionally, and the two streaming structs each owning a **whole** connection for the life of a stream.

So "established once" is: **a client (or transport) held in `DesktopState` keyed by the chosen endpoint, a `connect()` that reuses it, and a multiplexing decision for `EventSubscription` and `AttachConnection`** — because without muxing those, N sockets still open. That is ~4 structural edits in `daemon_client.rs` plus one field on `DesktopState`, not a 17-call-site rewrite. **#745 can size against that number.** One incidental win: `verify_endpoint_trusted` on Unix runs `std::fs::metadata` synchronously inside the async `trusted_daemon()` (`fsperm/unix.rs:184` from `daemon_bridge.rs:340`) — cheap, but a blocking syscall on a Tauri async task that a hold-once design removes from the hot path.

### Endpoint storage, and the gate that shapes it

**`String` is not available, and that is the mechanism to lean on rather than route around.** `ALLOWED_FIELD_TYPES` (`xtask/linkage-check/src/desktop_settings_secrets.rs:104-136`) holds exactly five entries; `String` is deliberately absent and `docs/develop/desktop-gui.md:432` says why in as many words. The guard runs under `cargo test-fast` and in the **required** `build` job, so `host: String` reddens a required check on the first push. A `FieldKind::Section` does not evade it (`:138-144`).

So: **validating newtypes** — `Hostname`, `SshUser`, `KeyPath`, `HostAlias` — whose `Deserialize` enforces charset and length bounds, added with a written reason in the idiom of the existing five (`ZoomLevel`'s entry at `:120-125` is the model). **One change buys two things**: the credential-shaped-value bound the gate exists for, *and* the ssh-argument validation that nothing performs today — `SshTarget::parse` (`src/remote.rs:50-61`) splits on the first `@` and stores both halves verbatim, with no length bound, no charset check, and no rejection of a leading `-`, whitespace, or NUL. Widening the allowlist to `String` would retire the guard for every future field; this keeps it meaningful.

**The storage policy, stated so #802 inherits it rather than re-litigating it: never store a secret. Reference the user's ssh config and agent by name only** — host, optional user, port, optional key *path*, optional jump-host *name*. The tree already proves this is sufficient: `~/.config/dot-agent-deck/remotes.toml` (`src/remote.rs:931-975`) is the existing remote-endpoint registry and stores a key **path**, never a passphrase and never a key. A passphrase must never be stored; the correct answer is "the key is in an agent, or unencrypted" plus `BatchMode=yes` so a locked key fails fast with a nameable error.

Field naming has one trap: the naming tripwire (`settings.rs:2074`) fails on a key name containing `key`, `token`, `secret`, … with a currently-**empty** path allowlist. **`key` is the natural name for a key path and the one `RemoteEntry` uses, and it trips.** It needs an explicit allowlist entry pinned to a concrete type (#827's rule). `host`, `user`, `port` and `endpoint` do not trip — and `settings.rs:66-69` says exactly why that is not reassurance: *"a field called `endpoint` holding a token passes it."*

**Prefer `desktop.toml` over reusing `remotes.toml`.** `settings.rs`'s read path vets the path (absolute, has a file name, absent or a regular file — symlink/FIFO/socket/device/directory refused) and bounds the read at 256 KiB; `RemotesFile::load` (`remote.rs:997-1008`) is a bare `read_to_string` with neither, so a FIFO at that path blocks and a huge file is read whole. Reusing it means porting the vet over, not inheriting it. `remotes.toml` stays the CLI's.

**Two hardening items this PRD is the named trigger for:**

- `save_to`'s "What this deliberately does not defend against" (`settings.rs:745-759`) declines parent-directory symlink/ownership validation and `openat`/`renameat` anchoring, and names **#741** as the trigger to revisit. Take at minimum the parent-ownership check, which is cheap; the anchoring can be deferred with a written reason.
- **The document's mode is asserted on write and never checked on read.** `read_document` (`:526-557`) has no mode or ownership test, so a `desktop.toml` at `0o644` loads without complaint. Fine for an appearance preference; not fine once it names an endpoint, a user and a key path. **Warn, do not refuse** — `load_from` never failing is a deliberate #803 property and bricking the app over a preferences file would be worse than the exposure.

### The compatibility policy, and what is left to #801

The order in `classify_handshake` **is** the security property and it does not change: `ok:false` → `PROTOCOL_VERSION` **exact equality** (`daemon_bridge.rs:230`, not bypassable, and the reason a new desktop can never reach an old daemon's verbs) → the build stamp → connect. `DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH` and the session allowance touch **only** the stamp arm, and never the trust check, which runs before `hello()` is called at all.

Half of #801 is already delivered and should not be rebuilt: `release_versions_are_compatible` (`:192-203`) compares a `compatibility_key` — `(0, minor)` while major is 0 — so a stamp difference *within* a release now falls through silently, fail-safe (`false` unless **both** stamps parsed).

What is left is exactly two things, and only one of them is this PRD's:

1. **A cross-`0.MINOR` stamp difference is still `Incompatible` by default, and the remedy offered assumes a daemon you own** — `:263-273` renders "use **Replace daemon** to start the matching bundled build", which for a remote daemon means terminating a daemon on someone else's host, and per the Problem Statement would not even do that. #801's framing is right: once the desktop runs on a different machine, "upgrade and restart the daemon" stops being available as a remedy.
2. **#801's item 3 — the `#[serde(other)]` retrofit for daemon→client pushes — is untouched by #819**, and it is the direction that bites. An unknown enum variant in a broadcast `KIND_EVENT` fails the **whole frame** decode; #801 records 4→5 (`AgentType::Pi`) and 6→7 (`ShellBusy`/`ShellIdle`) as exactly this, mid-session, new-daemon-old-client. `capabilities` does nothing for it because the client never asked for the message.

> **RECOMMENDATION: for a `Remote` endpoint, `PROTOCOL_VERSION` stays the hard floor, the build stamp becomes an informational badge and never sets `Incompatible`, and every project-aware action gates on `DaemonCapabilities::supports(...)` and degrades with a named reason** — which is #801's own proposal, on a mechanism that already exists and is already used (`daemon_bridge.rs:354`). `Local` keeps today's verdict unchanged; the desktop bundles its own sidecar there, so lockstep is real and worth enforcing.
>
> **Pair it with #801 item 3's audit, because A alone buys the appearance of compatibility and not the property**: capability gating is a graceful-degradation story, and today an unknown broadcast variant is not graceful — it kills the frame decode and the subscription with it, and the watcher reconnects in a loop. #801 already lists which types carry the catch-all and which do not; applying it to the ones on the desktop's push path is the cheap prerequisite. Anything beyond that stays #801's.
>
> **And the statement no option removes, which the PRD should make out loud rather than let the capability mechanism imply otherwise: a semantic break behind a stable wire is not mechanically detectable.** Nothing in a build stamp can see it either — a development build's `git describe` names the *last* release, so a branch carrying an unreleased semantic break describes as compatible with the release it was cut from (`daemon_bridge.rs:181-191`). Demoting the stamp for `Remote` makes rule 12's cross-version manual test and the `.breaking.md` fragment the **only** thing standing between a remote user and a silently wrong field, where today it is one of two. That is the cost of the recommendation, and it is accepted rather than hidden.

### Cross-version safety (CLAUDE.md rule 12)

**Did this change the TUI↔daemon contract? Under the recommended option 1B, no** — and the answer is stated rather than assumed. `proxy-stdio` adds a **CLI subcommand**, not an `AttachRequest` variant; the bytes over the pipe are the attach protocol unchanged. The `#[serde(other)]` retrofit makes the *client* tolerant of variants it does not know, which is the opposite of a wire change. The daemon OS field that *would* be one belongs to #754 and is out of scope. So: **`PROTOCOL_VERSION` stays 9, and there is no `.breaking.md`.** Patch bump.

**The manual test still runs, because the rule keys on what a PR touches and this one touches the daemon and the transport path.** Read rule 12 in full before running it — it lists traps that each silently turn the run into a meaningless same-version test, and #819's M9 Work Log entry shows what a passing run's evidence looks like. Non-negotiable specifics: the previous-release daemon comes up **with a live agent under it** (start it with the previous-release TUI, then close that TUI with `Ctrl+D` *then* `Ctrl+C` *then* **Detach**, never `Stop`), `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` is exported for **every** process, `DOT_AGENT_DECK_LOG` and `DOT_AGENT_DECK_EXPERIMENTAL` are pinned into the sandbox alongside the sockets and `HOME`, the status-hook check is done **last**, and the evidence recorded is exactly **one** `Attach protocol listening` line for the whole run plus one daemon pid and one build id serving it end to end.

**DECISION 1 landed on 1A (`ssh -L`) rather than 1B, and the answer is unchanged** — a tunnel is not a wire change either, so `PROTOCOL_VERSION` stays 9 under the option actually chosen.

### The seams this PRD names and does not touch

- **#754 — the daemon's OS is not in the handshake.** `ensure_desktop_workflow_platform_supported` (`dto.rs:899`) is parameterised on a `target_os` and its one production caller passes the **client's** (`lib.rs:1107`, `std::env::consts::OS`), mirrored in `desktop/src/lib/platform.ts:9-14` reading `userAgentData.platform`. The fix is an additive `AttachResponse` field, populated daemon-side, threaded through `HandshakeInfo` → `DesktopConnection` to that call site, plus one frontend edit — small in lines, a rule-12 contract change in kind. `capabilities` does not help: it advertises **verbs**, cfg'd on the daemon's platform, not an OS.
- **Command construction is still client-side.** `profileCommands.ts`'s `quoteShellWord` (`:106-108`) POSIX-quotes for a shell resolved on the daemon's host (`agent_pty.rs:1133-1150`). A POSIX client quoting for a `cmd.exe` daemon would not work; a Windows GUI quoting for a Linux daemon happens to. Same root cause as project resolution, not fixed here.
- **No native folder picker, and it stays that way.** The Browse decision is already made by construction — daemon-side `ListProjects` plus a paste-a-path resolved by `ResolveProject`. Re-introducing a picker would reinstate exactly the client-reads-the-wrong-filesystem bug #819 deleted, and the M7 tripwire's `cwd-fallback` finding partially guards it.
- **The M7 tripwire does not obstruct this work, checked deliberately.** `ALLOWED_ROOT_MODULES` already contains `daemon_client`, `daemon_protocol`, `platform` and `config`, so an `Endpoint` living in any of them does not trip. A **new** root module would need a deliberate allowlist entry argued in the PR — which is the rule working, not an obstacle. Transport code must never call `std::env::current_dir()`, which trips `cwd-fallback`; for a remote endpoint that is a desirable guard.

## Verification, and what cannot be verified here

Rule 4's L1/L2 vocabulary is the Rust TUI's, so the mapping is stated rather than assumed — and the honest answer has a hole in the middle.

**What the two desktop tiers can reach.** Both closed on 2026-09-10 and both are **advisory**, not required: `desktop-web` (vitest under jsdom, 15 files) and `desktop-browser` (Playwright over the *built* bundle, 8 specs / 22 tests, chromium **and** webkit). The browser tier reaches its screens through the in-app `FixtureDeckBridge` (`?fixture=1&state=…`), so it needs no daemon and no credential. It **can** drive a settings form end to end — `appearance.spec.ts` plus `AppearancePanel` is the worked precedent — and it **can** assert connection-state screens including the daemon lamp read as a composited colour (`connection-states.spec.ts` does all four states today). It **cannot** exercise a real connection: there is no socket, no `IpcStream` and no handshake anywhere in it.

**Where a real connection is actually testable, and it is a required gate.** `desktop/src-tauri/src/daemon_bridge.rs:1036-1140` already drives a **scripted socket** — a one-shot listener that accepts, reads a frame, answers a scripted `Hello`, and classifies the reply. That is the tier that can exercise endpoint kind, capability negotiation and compatibility classification against a fake daemon, and it runs in the **required** `build` job via `cargo test-fast --workspace`. **Do not rely on the browser tiers for transport correctness — they cannot see it.**

**What nothing reaches.** No real Tauri window: no `tauri-driver`, no WebDriver, no native IPC, no xterm over a real PTY (#953). And Playwright's WebKit is neither the WebKitGTK that Tauri uses on Linux nor WKWebView.

**The remote leg needs a second machine, and a claim without one is worse than a gap.** Locally the client's filesystem **is** the daemon's, so every path assertion passes whichever side resolved it — which is precisely why #819 left M10 unticked rather than reframing a local run as done. A loopback `ssh` to the same box is not remote. If no second machine is available, M11 stays unticked and that surface is reported **UNVERIFIED**.

One cheaper-than-a-second-machine option is on record from #819 and is worth costing rather than assuming: run the daemon in a **mount namespace** so the project path genuinely exists for the daemon and not for the client, and drive the desktop's Rust launch path against it over a real socket. It exercises "a launch against a path absent from the client's filesystem", which is the mechanism — but it does **not** exercise ssh, host-key auth, a network hop, or latency, so it is a complement to M11 and not a substitute for it.

## Success Criteria

- **A daemon on another machine is selected from in-app settings and works end to end** — agents listed, PTYs streaming, hook events arriving — from an app launched the way a desktop app is launched, with no environment variable set by hand. (M13; if the second machine is unavailable, UNVERIFIED and stated as such.)
- **The local case is byte-identical**, and for the first time its refusals are pinned by tests: wrong mode, foreign uid, and a regular file at the path each refuse, and a healthy socket accepts.
- **Trust is a property of the endpoint kind.** `Local` keeps uid + `0o600`; `Remote` rests on ssh host-key and user authentication; neither is described as the other, in code or in docs.
- **Nothing can SIGTERM a process by peer credential on a remote endpoint** — `peer_pid`-based termination is unreachable for `Remote` by type, not by branch, and the Stop / Replace buttons say why they are unavailable rather than silently doing something else.
- **No secret is stored.** The endpoint document holds a host, an optional user, a port, an optional key *path* and an optional jump-host *name*, every one a validating newtype; `ALLOWED_FIELD_TYPES` gains no `String`; and the "no route in exists at all" property of the missing `SecretStore` still holds after this PRD.
- **An endpoint's display text cannot reorder the connection footer** — bidi format characters are stripped on the path that tells the user which daemon they are talking to.
- **A remote daemon at a different release connects and degrades by capability**, naming what it cannot do, rather than refusing with a remedy that does not exist for it.
- **A state refresh against a remote daemon does not re-establish the connection**, and the number of connections per refresh is stated and tested rather than assumed.
- **Rule 12 is discharged**: the contract question answered explicitly (`PROTOCOL_VERSION` unchanged, no `.breaking.md`, with reasons), and the cross-version manual test run with a live agent under a previous-release daemon and exactly one `Attach protocol listening` line recorded.
- **#819's M10 closes**, or is explicitly reported UNVERIFIED with the reason — never quietly reframed.

## Milestones

Ordered by dependency. **M1–M4 change no user-visible behaviour**; that is deliberate, and it is the natural split point if this ships as two PRs (see Risks).

### Iteration 1 — the abstraction, with the local case unchanged

- [x] **M1 — Pin the local guarantee before touching it.** Unit tests in `src/platform/fsperm/unix.rs`'s existing `mod tests` for the four refusals plus the accept, over a `tempfile::tempdir()` `UnixListener`. The foreign-uid arm is not testable without a second account — factor the comparison out as pure data and test *that*, the way `endpoint_owner_is_trusted` is pinned for Windows. First because it is the only thing that would catch a later refactor silently loosening the check, and nothing covers it today.
- [x] **M2 — The `Endpoint` type, and the local-only operations made unreachable.** `Local` byte-identical, preserving all seven properties listed in Technical Approach. `peer_pid` termination and the stale-inode unlink reachable only from `Local`, by type. The TUI/CLI keep their path-shaped API through a `Local`-preserving constructor. A change that does nothing else.
- [x] **M3 — The transport seam.** Generalise `connect()`, `EventSubscription`, `AttachConnection` and `terminal.rs`'s writer over a splittable transport, preserving the Unix `SHUT_WR`-on-drop teardown that `ipc/mod.rs:19-25` records an earlier draft silently regressed. The three filesystem-presence predicates get a third answer rather than a boolean.
- [x] **M4 — The connection is held once, and the daemon stops being re-polled.** Two halves, because the first alone does not deliver the property the user asked for. **(a)** A transport held in `DesktopState` keyed by the endpoint, `connect()` reusing it, and the multiplexing decision for the two streaming structs made and written down — the artifact #745 is waiting on; the blocking `std::fs::metadata` leaves the per-refresh hot path with it. **(b)** The pushed events applied **incrementally** instead of re-fetching the whole agent list on every one, with a full re-sync only on reconnect or a slow floor. **Done-condition is a measurement, not a claim**: daemon-side accepts/sec and bytes/sec against an 8–15 agent fleet, before and after, by the same method. The baseline is taken from unmodified code so the comparison is real rather than reconstructed.

### Iteration 2 — remote

- [x] **M5 — The remote transport** (DECISION 1: `ssh -N -L`, app-managed). Absolute-path `ssh` resolution, `BatchMode=yes`, `apply_observation_options` minus `ClearAllForwardings`, `StrictHostKeyChecking` never weakened, `-o StreamLocalBindUnlink=yes` or an owned unlink, and child lifetime owned including the paths where `Drop` does not run. The forwarded socket is the transport's private implementation detail: no local trust check against it, and its presence is never read as health.
- [ ] **M6 — Endpoint storage.** The validating newtypes with their written reasons, the `key`-path allowlist entry pinned to a concrete type, the `Selection` value shaped so a variant can be added, and the two hardening items `settings.rs` names #741 as the trigger for — parent-directory ownership on write, and a mode **warning** on read.
- [ ] **M7 — The endpoint panel.** One registry row plus a component per #803's contract; add, name, select, remove; local present and default without configuration; the two-column row convention and the "UI text earns its place by being actionable" rule that `docs/develop/desktop-gui.md:426-448` sets. Bidi-safe endpoint display. Stop / Replace disabled for a remote deck with an explanation.
- [ ] **M8 — The compatibility policy.** Capability-gated verdict for `Remote`, stamp demoted to a badge, `Local` unchanged; plus #801 item 3's catch-all applied to the daemon→client push types on the desktop's path. What is left to #801 recorded on that issue.
- [ ] **M9 — The Deck selector.** On the agent-activity screens — the deck and the overview — reading as one control in both, defaulting to local, with the selection a growable value (`Selection::One(EndpointId)`, room for `All`). **This is the milestone that makes every other screen testable with one remote deck**, and it is the seam #742 extends rather than replaces.
- [ ] **M10 — Test connection.** Per endpoint, in settings. Each outcome is a **distinct named state** rather than one "failed": reachable, refused at the handshake (protocol version named), stamp difference, unreachable, host-key unverified with the run-`ssh`-once remedy. Exercises the whole transport stack without any screen having to work first, which is what makes it the early user-testable milestone.
- [ ] **M11 — Docs and changelog.** `docs/develop/desktop-gui.md` gains the endpoint section, the trust-by-kind split, the transport decision **and its rejected alternatives** (1B and russh, with the measurements), the naming rule, and the manual remote walk. The ownership table gains the endpoint rows. Changelog fragment via `dot-ai-changelog-fragment`.
- [ ] **M12 — Rule 12 cross-version manual test**, with the eleven-variable sandbox and the three traps named in Technical Approach, evidence in the Work Log.
- [ ] **M13 — Remote proven, on a second machine.** Discharges #819's M10. The user runs the desktop app on their laptop against this machine's daemon. If it cannot be run, this stays unticked and is reported UNVERIFIED rather than substituted by a local run or a loopback `ssh`.
- [ ] **M14 — Record the seam on #742.** A comment naming the selector, the `Selection` shape, the "All Decks" option it grows, and the two retrofits this PRD deliberately leaves it (`DesktopState` → keyed map, `ConnectionStatus` → per-endpoint), so #742 inherits them rather than rediscovering them.
- [ ] **M15 — The naming sweep: the UI says Deck.** Every user-facing string, per the rule in Technical Approach — rendered text becomes Deck, while code, protocol, CLI, docs, CSS classes and testids keep `daemon`. ~40 strings across 22 non-test files with 12 test files asserting on them; a copy pass rather than a find-replace, since several strings distinguish the app from the process it manages. Last because it touches files every other milestone also touches, and rebasing a rename is miserable.

## Risks

- **This is a large PRD and the honest split is at the M4/M5 seam.** M1–M4 are a refactor with no user-visible change and a required-gate story that is entirely `cargo test-fast`; M5–M11 are the feature. Shipping them as one PR means a diff that is hard to review and a rollback that takes the abstraction with the feature. The counter-argument is that an abstraction with no consumer is proven only by its own tests — the same argument #803 made for shipping its container with one real tenant. **Recommend one PRD, and let the user decide whether it is one PR or two.**
- **The `SHUT_WR`-on-drop half-close is the single most likely silent regression**, and the code says so: an earlier draft already made exactly this mistake and the comment recording it is the reason we know. It will not fail a compile and may not fail a test; it shows up as a daemon that does not notice a disconnected client.
- **`peer_pid` is not the only locality assumption, only the one that was found.** The audit enumerated four filesystem-presence predicates and the stale-inode unlink; there may be others that only a remote run surfaces. This is an argument for M11 being real rather than for more static analysis.
- **A supervised long-lived ssh child has no precedent in this tree.** Both existing call sites block on the child. Supervision, restart, teardown-on-quit and the SIGKILL path are new work, and the failure mode of getting it wrong is an orphaned process holding a socket that the trust check then *passes* on.
- **Demoting the stamp for `Remote` makes rule 12's manual test the only backstop** for a semantic break behind a stable wire. Accepted deliberately and stated in Technical Approach; the mitigation is discipline, which is the weakest kind.
- **The user-visible half is covered only by advisory jobs.** The Rust half is in a required check and holds the properties that matter, but nothing drives a real window. A standing gap for the whole desktop app (#953), not one this PRD creates.
- **The remote leg may go unverified.** If M13 cannot be run, this PRD ships an abstraction whose premise is untested against the one case it exists for — exactly the position #819 chose deliberately and recorded. Repeating that choice knowingly is defensible; repeating it silently is not.

## Open Questions

**All four gate decisions were taken by the user on 2026-09-11 and are recorded in Technical Approach rather than here, because a call made and recorded beats a call deferred.** In summary: transport is **1A** (`ssh -N -L`, app-managed, with 1B `proxy-stdio` filed as the Windows follow-up and russh measured out); **no** `experimental` flag; #742 stays separate and the **Deck selector** is what makes this PRD independently testable; **one PR**. Plus one instruction that was not on the original list: **the UI says Deck, never daemon**.

What remains genuinely open:

1. **Does the endpoint's `user` half get logged?** `tracing` records endpoint paths in several places and `DOT_AGENT_DECK_LOG` is a file people paste into bug reports. Not a vulnerability; a deliberate call worth making rather than discovering.
2. **Can the pushed events actually carry an incremental update?** M4(b) assumes the daemon's event stream says enough to maintain an agent list without re-fetching. If some transition genuinely requires a full re-read, M4(b) narrows to "re-sync on those events only" and the measurement says what that costs. Being answered by the baseline measurement before M4 starts, rather than assumed.
3. **Does `MaxSessions` count forwarding channels?** Not load-bearing for 1A, which avoids the question — but it is the thing that would have to hold before the 1B follow-up ships, and it is currently recalled OpenSSH semantics rather than a measurement.

## Work Log

### 2026-09-10 — Created from the issue plus two read-only reconnaissance passes

Written by the orchestrator from #741's body and all four comments, plus parallel read-only recon by an auditor (trust, transport, build-stamp, secrets) and a reviewer (client architecture, connection lifecycle, settings, test tiers). Neither modified a file. The full reports are at `.dot-agent-deck/prd741-recon-auditor-report.md` and `.dot-agent-deck/prd741-recon-reviewer-report.md`.

Six of the issue's claims did not survive and are tabled in [What the reconnaissance changed](#what-the-reconnaissance-changed); the `0o755` mechanism is **false** and the Browse warning is **stale**. Two things the issue does not mention are now load-bearing on the design: `peer_pid` termination misrouting over a tunnel while reporting success, and `String` being unavailable in the settings schema. The russh trade was **measured** — lockfiles resolved, `cargo audit` run, the RustSec database cross-referenced at `advisory-db` HEAD `b50980aa` — rather than reasoned from memory, which is what turned DECISION 1 from a judgement call into an arithmetic one.

### 2026-09-11 — The step-1 gate, which changed the plan more than it approved it

Four decisions were asked for and four were taken, but two of the answers came back as **better plans than the ones offered**, and both are recorded above in the user's shape rather than this document's.

**The transport recommendation was reversed at the gate, by arithmetic this document had not done.** It recommended 1B (`proxy-stdio`) on the strength of it needing no local socket. The user asked how the PTY streams and the API-like data actually flow, which forced the count: the desktop holds ~11 *concurrent* connections on a nine-tile deck, and under 1B each is a separate `ssh` child with its own authentication and its own `exec` session — against a default `MaxSessions` of 10. Under 1A they are forwarding channels on one authenticated connection, which is what the 2026-08-29 run already demonstrated at nine agents. The recommendation was wrong and the question that exposed it was one sentence long.

**The scope argument was resolved by a third option neither side had proposed.** This document said #742 was out of scope; the user argued the screens are fleet screens and that showing one deck's agents needs a picker that #742 would then throw away. Both were half right. The answer — a **Deck selector** that #742 extends with an "All Decks" option rather than replacing — makes #741 independently testable *and* makes #742 cheaper, and it converts #742's hardest open question into a seam. This document had dismissed a picker as scaffolding, which was the error; a selector is permanent UI, because focusing on one deck stays useful after the fleet view exists.

**One PR, not two.** The proposed M4/M5 split was rejected on the ground that a PR whose behaviour cannot be validated is a PR approved on trust. The milestones still land as separate commits, which keeps the bisect the split was really buying.

**And performance was named as the user's main concern, which widened M4.** As drafted, M4 removed the per-call connection churn and left the full-snapshot-per-event pattern untouched — so the daemon would still have serialised the entire agent list up to 6.6 times a second. M4 now covers both halves, and its done-condition is a before/after measurement rather than a claim. The baseline is being taken from unmodified code for exactly the reason #819's provenance gate exists: a number reconstructed after the change is not evidence.

**Implementation started at M1** — pinning `verify_endpoint_trusted`'s refusals, which reconnaissance found are covered by no test anywhere — with the mutation check included, because a characterisation test that has never failed proves nothing about the regression it exists to catch.

### 2026-09-11 — M1 and M2 landed, and both found something the design did not know

**M1 — the refusals are pinned, and the mutation check earned its place.** Seven tests in `src/platform/fsperm/unix.rs`, plus one pure extraction (`endpoint_uid_is_trusted`) so the foreign-uid arm is testable without a second account, following the Windows `endpoint_owner_is_trusted` precedent. Eight mutations applied one at a time; **every one was caught and every one of the seven tests went red under at least one**, so none is a test that has never failed.

One result is worth keeping because it nearly went the other way. Mutating the mode check from exact equality to a mask (`& 0o077 != 0`) leaves `0o644` refused — so the rows that catch it are the *surprising* ones, `0o700`, `0o400` and `0o000`. A suite covering only the loose direction would have passed that mutation silently. The task predicted this and the measurement confirmed it.

**M1 also found a property nobody had recorded: `verify_endpoint_trusted` stats *through* symlinks.** It uses `std::fs::metadata`, not `symlink_metadata`, so a symlink to a trusted socket is accepted and the link's own mode decides nothing. Pinned as characterization rather than endorsement.

**M2 — `peer_pid` termination is now impossible rather than checked, and the proof is a compiler error.** `Endpoint::Local(LocalEndpoint)` / `Remote(RemoteEndpoint)`, with five functions taking `&LocalEndpoint` where they took `&Path`: `run_daemon_stop`, `run_daemon_restart`, `terminate_daemon_graceful`, `ensure_compatible_daemon_or_die`, and `ensure_daemon_running`/`ensure_external_daemon_or_die`. Verified by compiling a deliberate `run_daemon_stop(&Endpoint::Remote(…))` and reading the `E0308`, then reverting the probe.

**The design decision that actually buys the property is not the enum, and it is worth recording because it is one accessor away from being lost.** `Endpoint::connect_address()` returns a bare `&Path`, deliberately **not** a `LocalEndpoint` — because in M5 the address a *remote* deck is reached at is an `ssh -L` forwarded socket on this filesystem, and had the connect accessor handed that back wearing the `LocalEndpoint` type, `run_daemon_stop(endpoint.connect_address()?)` would compile and kill the tunnel. Keeping "the address I connect to" and "the local daemon I may manage" as different types is what makes the guarantee **survive** M5 rather than expire at it. **M5 must not route its forwarded socket back through `LocalEndpoint::at`** — that is the single greppable call that would undo this milestone.

**The honest statement of the property** (rule 17): `LocalEndpoint::at(path)` accepts any path, so the type guards against *reaching for the wrong value*, not against a caller deliberately asserting a wrong one. No remote endpoint can reach the termination, unlink or lazy-spawn paths **without someone writing `LocalEndpoint::at` explicitly**, and that call is greppable.

**One scope-fence deviation, accepted.** The task said the TUI/CLI's nine sites should not have to move. Three did — `src/main.rs:1581`, `:2023`, `:2064` — because they are the *callers* of exactly the entry points that had to become type-safe, and a function cannot refuse a remote endpoint by type while its callers hand it a bare path. The alternative was wrapping the path in a `LocalEndpoint` *inside* those functions, which is the "checked, not impossible" shape the milestone forbids. Ten mechanical lines, no ripple into the other six sites. The coder stopped and reported rather than proceeding silently, which is what the fence was for.

**Blast radius: 8 non-test files across 2 crates, as the reconnaissance predicted — but a different 8.** `terminal.rs` did **not** move (its concrete `IpcWriteHalf` is the *transport* seam, M3's problem, not "who may be addressed"), and `src/main.rs` did, for the reason above. The recon undercounted by treating `daemon_stop.rs` and `build_version_handshake.rs` as the radius while their callers sat in a file it listed as outside it.

**The symlink question was flagged, not changed — and the residual it exposed is now [#1020](https://github.com/vfarcic/dot-agent-deck/issues/1020).** Keeping `metadata` in M2 was right: switching spellings changes what the local path accepts, and M2's whole claim is that it does nothing else. But the investigation found the case where it bites. `attach_socket_path()` falls back to `/tmp/dot-agent-deck-attach-<uid>.sock` when `XDG_RUNTIME_DIR` is unset — a plain ssh session or a container — and `/tmp` is world-writable, so a **foreign uid** can plant a symlink there pointing at a `0o600` socket of ours, and it is accepted. The payoff is redirection rather than disclosure (the attacker is not the listener), but recovery is poor: `/tmp`'s sticky bit makes our own `remove_file` of their symlink fail with `EPERM`, so lazy-spawn's `bind(2)` fails too — a worse outcome than the clean refusal the same attacker gets with a plain file. **This matters more under this PRD than before it**, because a daemon reached over ssh is exactly where `XDG_RUNTIME_DIR` is commonly unset. Filed rather than fixed inline; the review phase decides whether it lands here.

### 2026-09-11 — M4(b): the daemon stops being re-polled, and the fold is the daemon's own

**The steady-state refresh now costs nothing.** The watcher folds the daemon's pushed events into a local agent list (`desktop/src-tauri/src/agent_view.rs`) and re-fetches only on a `SessionStart`, an `OrchestrationSurface`, a `WorktreeKept`, a re-subscription, or a **5-second reconciliation floor**. Measured, by the same method as M4(a) and pinned as a test (`daemon_bridge::tests::ten_folded_refreshes_cost_one_handshake_and_one_listing`): ten refreshes cost **2 connections**, against 11 at M4(a) and 20 at the baseline. Steady-state connections per second go from 6.867 to **0.4** — one `Hello` per `HANDSHAKE_REVALIDATE_INTERVAL` and one `ListAgents` per reconcile, both 5 s — and request/response bytes per refresh from **9,667 B at fifteen agents to 0**, with the reconcile amortising to ~290 B/refresh at the 6.667/s ceiling. A burst of N events costs **zero** connections where it previously cost N listings drained at 6.667/s.

**The fold is `AppState::apply_event` itself, not a re-implementation**, which is the only reason a client-side status is trustworthy at all. The `ListAgents` live join moved onto `AppState` as `live_session_for` / `attach_live_sessions`, replacing two character-for-character copies in `daemon_protocol.rs`; the desktop calls the same method. The client seeds its fold from each reply with `seed_hydrated_session` — the daemon's own reconnect-side seeder, the one the TUI runs at hydration — because a fold started from zero would restart `tool_count` at 1 for every agent already running. That the tally moves on `ToolEnd` and not on `ToolStart` was a rule the implementation never had to learn: the first draft of the test asserted the wrong one and the daemon's fold corrected it.

**One divergence, named rather than papered over.** The daemon's admission control asks its `AgentPtyRegistry`; a client has no registry and falls back to the historical pane-set rule, so a **paneless** agent's transitions are not admitted while any paned agent is registered. That is the TUI's behaviour too, and it is bounded by the floor. Likewise the daemon broadcasts an event *before* applying it to its own state, so a reply built in that window can swallow one client-side transition — bounded the same way, and closing it properly needs a sequence number on the wire, which is a `PROTOCOL_VERSION` change this milestone is fenced away from.

**Gap 2 is wider than this document said, and it is what sets the cadence.** The PRD read "a cooperative agent emits `SessionEnd`, but process death is not guaranteed to". Enumerating the `BroadcastMsg` senders shows the distinction does not exist: records come off `agent_records()`, which filters on the `exited` flag the PTY reader sets at EOF, and **nothing broadcasts from that path**. A `SessionEnd` retires a *session*, never a *record*. So no **record-removal** signal exists, and only a periodic re-read catches one. *(Narrowed again by the M4 review: read as "no wire signal correlates with an impending removal" that over-reaches. `SessionEnd` **is** broadcast for a cooperative exit and does reach the client's fold — it simply does not remove the record. Marking a fetch due on it, symmetric with the `SessionStart` rule, catches cooperative removals well inside the floor and leaves the floor covering genuine crashes. Queued as a follow-up; the floor is a correct backstop without it.)*

**Five seconds, and it is a two-way trade rather than a pure win.** It replaces an *unbounded* ghost-card window in the quiet case — today the watcher refreshes only on an event, so an agent `SIGKILL`ed while it is the only one running leaves its row up until the user acts — with a 5 s bound. It lengthens the *busy*-case removal latency from ~150 ms to ≤5 s, which is the cost. The mitigation for a tile the user is actually watching is not the floor: that tile has its own PTY stream and sees `KIND_STREAM_END` immediately.

**Freshness was checked rather than assumed, and the cached path is deliberately narrow.** Only the watcher passes a view; `desktop_get_snapshot`, `bootstrap` and the `refresh_and_emit` that tails every `DesktopAction` still fetch in full, so every user-initiated path is as fresh as M4(a) left it (pinned by `a_caller_with_no_view_still_fetches_every_time`). The **incompatible** path — where `running_agent_count` comes from the handshake and gates **Replace daemon** — is never cached, for the same reason M4(a) refused to hold a refused classification. On the connected path the count is derived from the same list the rows are, so the banner and the deck can be stale together but can never disagree.

**The subscription reader is a task of its own, and that is a correctness fix, not tidiness.** `EventSubscription::next_event` is **not cancel-safe** — `read_frame` accumulates a five-byte header across awaits into a local buffer — so it cannot sit in a `select!` arm beside the reconciliation timer. It now drains into a bounded `mpsc` that the refresh loop selects on. A side effect worth noting: before this the loop read at most one event per full `get_snapshot()`, i.e. ~6.667/s, so the desktop drained the daemon's broadcast far more slowly than the daemon filled it.

**Two documentation corrections from the M1–M3 review landed with it.** The boxing rationale in `platform/transport.rs` and `terminal.rs` justified the box with "a single registry cannot hold a local and a remote session at once" and called M5's transport "the second implementor" — both false under DECISION 1A, where a remote deck is an `ssh -L` forwarded Unix socket and therefore the *same* `IpcStream`. **M5 adds no second `AttachTransport` impl at all**, and the docs now say so, with the three reasons that do hold. `HalfCloseOnDrop`'s "what does not qualify" section gained the cross-hop case, which is where it will next be implemented wrongly: under the deferred 1B the write half is a local ssh child's `ChildStdin`, and an impl for it would compile while being wrong. And `LocalEndpoint` gained a signature-stability note — the seven functions taking `&LocalEndpoint` must never revert to `&Path`, because that reversion deletes M2's guarantee with every test still green; there is no `trybuild` harness and DECISION 1's audit reasoning argues against adding one for this alone, so the posture is review-plus-grep and is now written down as such.

### 2026-09-11 — M4 landed in two halves, and the review corrected one of its claims

**The numbers, measured by the same method as the baseline.** Connections per second in steady state: **13.33 → 6.87 → 0.4**, a 97% cut. Bytes at fifteen agents: **~64 KB/s → ~1.9 KB/s**. A burst of 100 events went from 100 connections drained over ~15 s to **zero connections**, drained as fast as the reader reads. The full lane-1 e2e tier ran green on the first attempt — 2993 tests, 81.6 s, at load ~5.

**M4(a) found that the literal plan was unreachable, which corrected this document.** The PRD said the barrier to connection reuse was `connect()` reconnecting unconditionally. It is not: **the daemon answers exactly one request per connection and then closes it** — `handle_connection` (`src/daemon_protocol.rs:2136`) reads one frame, dispatches, replies, returns, with no request loop. Measured with a probe that got `Broken pipe` on a second request over the same connection and `ok` on a fresh one as a control. Reuse is therefore unreachable client-side at any cost; getting it would need the daemon to loop, which is a same-wire/different-meaning change of rule 12's semantic-break class. Pinned by `a_request_connection_answers_once_and_is_then_closed` so it goes red if the daemon ever learns to loop. What M4(a) removed instead was the per-refresh `Hello` — a handshake being re-taken per request batch, which is the one thing a handshake is definitionally not.

**The multiplexing decision, which #745 was blocked on: not multiplexed.** Four reasons, and the fourth is the one that settles it. `EventSubscription` is a population of one (`start_watcher_once`). Muxing attach streams is a **wire change**: the frame header is five bytes, kind plus length, with **no stream id**, so two attaches on one socket could not be told apart. It would need the daemon request loop anyway. And **`ssh -L` already multiplexes** — each connection to a forwarded socket opens a new SSH channel on the existing transport (RFC 4254), so N streams cost N channels over one TCP session and one authentication. Re-implementing multiplexing at the attach layer would buy a channel count and pay a wire break for it. The cost is stated rather than waved at: #742 showing many tiles holds one connection per visible tile with a matching daemon-side task — linear in tiles, an fd-and-task cost, **not** a latency one.

**The fold is the daemon's own code, not a parallel implementation.** The desktop runs `AppState::apply_event` over the same broadcast the daemon applies to itself; the `ListAgents` live join was **extracted** (`AppState::live_session_for` / `attach_live_sessions`) so one implementation now has three callers; seeding uses `AppState::seed_hydrated_session`. That choice paid immediately — a draft test asserted `ToolStart` bumps the tool tally and the daemon's own code corrected it to `ToolEnd`. A parallel status machine would have got that wrong silently.

**A cancel-safety trap avoided.** The obvious `select!` over `next_event()` and an interval is **wrong**: `next_event` is not cancel-safe, because `read_frame` accumulates a five-byte header across awaits, so a dropped arm desynchronises the stream. The subscription is now drained by a task of its own into a bounded channel and the loop selects only over cancel-safe operations. **The same class exists elsewhere and predates this work** — `wait_for_coordinator_readiness` wraps `next_event` in a `timeout` — filed as [#1028](https://github.com/vfarcic/dot-agent-deck/issues/1028).

**The regression, named rather than buried.** Busy-fleet removal latency goes from ~150 ms to ≤5 s. What it buys is a **bound where there was none**: today a `SIGKILL`ed agent that is the only one running emits nothing further, nothing triggers a refresh, and its row stays up indefinitely. A tile the user is actually watching has its own PTY stream, so `KIND_STREAM_END` reaches it immediately and independently of the list — the stale row is a card, not a terminal that lies. The review verified that mitigation is a real independent event rather than an argument.

**The review found nothing blocking and one claim backwards.** The bounded channel — the thing most likely to have made the fold silently diverge — **fails closed**: the sender blocks rather than dropping, and a broadcast lag surfaces as a resubscribe that discards the fold entirely. But M4(b)'s "install race" is **mischaracterised in both the code comment and its report**: `ingest_event` (`src/daemon.rs:1241-1257`) holds one write lock across *both* the broadcast and the apply, and a later `ListAgents` read lock serialises after it, so the described swallow cannot happen. The real residual is the opposite sign — a transient `tool_count` **double-apply** when an event arrives during a fetch and is reflected in both the reply and the next drain, self-healing at the next reconcile. Same severity, inverted mechanism; the comment and this document are being corrected rather than the code.

### 2026-09-11 — M5 shipped the tunnel, and the audit found what `forced_options()` cannot reach

**M5 landed** (`f78da6a2`): `src/remote_tunnel.rs` with five validating newtypes, absolute-path `ssh` resolution, and the `ssh -N -L` tunnel with an owned child lifetime. Full lane-1 tier green, 3036 tests, 63.4 s. 43 unit tests.

**Three real defects, all found by the implementer's own mutation testing rather than by review.** A **classification race** — `try_wait` can see the ssh child exit before the stderr drain has read what it wrote, sending a host-key failure down the `Other` arm and losing its remedy, which is issue #344's defect reintroduced. An **unbounded wait in teardown** — the first fix joined the drain thread, which ends only at EOF, and a `ProxyCommand` grandchild inheriting the write end can hold it indefinitely; that wedged the whole test binary rather than failing a test, and was reachable in production. And **`is_forward_failure_detail` was TCP-only** — M5 creates the first Unix-socket forward, whose bind failure matched none of the three existing markers, so an `ssh -L` collision classified as `HostUnreachable`. A fourth by inspection: the tunnel socket shared a directory with the daemon's lock files, and a sweep that deletes should not read a directory someone else writes.

**Two findings that constrain later milestones.** The tunnel **cannot live in `TrustedDaemon`**, which M4(a) re-establishes every 5 s — that would re-authenticate ssh every five seconds. *Tunnel lifetime is not handshake lifetime*, and where it lives is M7/M9's decision; M5 deliberately left the desktop unwired rather than guess. And `RemoteEndpoint` needs a **remote socket path** field the storage list above did not have: OpenSSH does not expand `~` or environment variables on the remote side of `-L`, and the far host's `XDG_RUNTIME_DIR` and uid are not knowable without a second ssh round trip — which M10's `Test connection` already makes.

#### The audit: nothing blocking for M5, five findings blocking for M6/M7

**Why M5 itself cannot bite anyone, which sets every severity below:** nothing outside `src/remote_tunnel.rs` constructs a `RemoteTunnel`, and `desktop/src-tauri/src/dto.rs:744` still returns `Endpoint::local()` unconditionally. Every finding is **latent** — real and reachable by inspection, not reachable by a user today.

**The unifying defect is that `forced_options()` reaches only the outer `ssh`.** The user's `~/.ssh/config` is still in play, and four of the five findings are that in different clothes. Measured against OpenSSH 10.2p1 with `ssh -G` and `ssh -vvv`, not reasoned from the manual.

- **A1 — `-J` spawns a second `ssh` that inherits none of the forced options.** OpenSSH implements `ProxyJump` as an implicit `ProxyCommand` running a fresh `ssh` carrying only `-l`, `-p`, `-J`, `-F` and `-v`. **Not one `-o` survives.** So a user with the common `Host * / ForwardAgent yes` line gets their **ssh-agent forwarded to the bastion for the entire life of the tunnel** — hours, not the seconds a `remote doctor` probe lasts — while the target correctly gets none, and the GUI shows nothing, which is the exact reason `ForwardAgent=no` is forced. `batchmode no` on that hop also re-opens the third-party-dialog path, and a config `ControlMaster auto` lets it spawn a master that **outlives our child**. **Blocking before `jump` becomes a settings field.**
- **A2 — a config `StrictHostKeyChecking no` wins**, so `remote_tunnel.rs:21`'s claim that "a remote deck's trust story is the ssh host key and ssh user authentication, and that is the whole of it" is **false** in that configuration. An on-path attacker then answers instead, the GUI shows a healthy deck, and every frame — agent names, cwd paths, prompts, hook payloads — goes to them. The app cannot notice **by construction**, because M5 correctly refuses to run the local inode check against a forwarded socket.
- **A3 — the user's `RemoteForward`/`DynamicForward`/`LocalForward` are inherited for the tunnel's whole life.** A `Host * / RemoteForward 9999 localhost:22` line exposes the laptop's sshd from the remote host for hours. `remote doctor` refuses to create that forward at all and names it a criterion violation (`remote.rs:498-510`); the tunnel makes the same exposure for orders of magnitude longer and says nothing.
- **A4 — the forwarded socket's mode is whatever the user's config says.** The `0700` directory is the only control and **it is never asserted**. `StreamLocalBindMask=0177` costs nothing and restores the second leg.
- **A5 — the host-key remedy names a different endpoint than the tunnel uses.** Told to run `ssh deploy@build-box` after a failure on port 2222 through a bastion, the user reaches port 22 with no bastion; since `known_hosts` keys non-default ports as `[host]:port`, accepting there does **not** satisfy the tunnel. The app would have induced the user to trust a host key for a host it never asked them to evaluate.

**The fix shape for A1 also reaches A2**, and it is the one lever OpenSSH leaves: `-F` **is** propagated into the implicit `ProxyCommand`. A generated config whose first block is `Host *` carrying the forced options, followed by `Include ~/.ssh/config`, reaches both hops — OpenSSH's first-value-wins rule makes the leading block authoritative. The cheap interim is to refuse `-J` and say why.

**Four over-wide claims in M5's own code and report were narrowed** — rule 17's defect class, in the milestone that had just applied it to someone else. The trust-story absolute (A2); "a fixed list of root-owned system locations has neither failure mode", false for `/opt/homebrew/bin` and Intel macOS `/usr/local/bin`; "the nonce is what makes the name unique", since a wall-clock nanosecond reading is not a uniqueness guarantee; and an arithmetic slip in both code and report — `apply_observation_options` has ten options, minus `ClearAllForwardings` leaves **nine**, not ten, and six are new rather than three, which is the 15 `tunnel_args` actually emits.

**A2, A3 and B5 are documented residuals rather than code fixes** — each is a real exposure the argv cannot close, and each belongs in `forced_options()`'s "what is not here" block and in whatever M10's `Test connection` surfaces.
