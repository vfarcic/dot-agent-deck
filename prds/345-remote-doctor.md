# PRD #345: `remote doctor` — diagnose remote connectivity and ssh forwards

**Status**: Not Started
**Priority**: Medium
**Created**: 2026-08-02
**GitHub Issue**: [#345](https://github.com/vfarcic/dot-agent-deck/issues/345)
**Related**: [#97](https://github.com/vfarcic/dot-agent-deck/issues/97) (the reverse-tunnel recipe this exists to make debuggable), [#344](https://github.com/vfarcic/dot-agent-deck/issues/344) (`ForwardFailed` classification — the error-path half of the same problem), `src/connect.rs` (`probe_remote_version`, `probe_remote_protocol`, `map_probe_ssh_error`, `RemoteConnectError`), `src/remote.rs` (`SystemSshExecutor::build_command`, `RemoteEntry`, `SshTarget`), [`docs/remote-recipes.md`](../docs/remote-recipes.md#reaching-networks-only-your-laptop-can-see)

## Problem Statement

The recipe added for #97 tells users to configure reverse tunnels in `~/.ssh/config` so a remote can reach networks only the laptop can see. It works — but every way it can fail is currently opaque, and several were discovered only by building the setup and testing it:

1. **`AllowTcpForwarding no` on the remote and a port collision are indistinguishable.** Both produce exactly `Error: remote port forwarding failed for listen port N` on the client. Alpine's `openssh` package ships `AllowTcpForwarding no`, as do most hardening baselines, so this is a live first-run failure and not a corner case.
2. **A failed forward can be silent.** Without `ExitOnForwardFailure yes`, ssh brings the session up anyway and the forward simply does not exist. Agents then fail on network access with errors that point at git, not at the tunnel.
3. **`DynamicForward` looks right and does nothing useful.** It puts the SOCKS listener on the laptop rather than the remote. This exact mistake appeared in the original #97 proposal *and* in the maintainer's reply agreeing with it, which is strong evidence users will make it too. Nothing in the current failure output would tell them.
4. **The ssh config applies to the deck's own probes.** A forward listener that has not been reaped yet makes the version probe fail, which the deck reports as an unreachable host and then burns its reconnect budget against (see #344).

The deck already owns the registry and the ssh transport. What it has never offered is an answer to "is this remote actually set up the way I think it is?" — so today the only diagnostic path is reading `man ssh_config` and guessing.

## Key insight: `ssh -G` makes this cheap and non-invasive

`ssh -G <destination>` prints ssh's *resolved* configuration for that destination, including forwards, in a stable machine-readable form. Verified directly:

```
exitonforwardfailure yes
dynamicforward 9099
remoteforward 1080 [socks]:0
```

Note that ssh labels reverse-dynamic (SOCKS) forwards unambiguously as `[socks]:0`, so the correct and incorrect configurations are trivially distinguishable without parsing anything ourselves.

This matters because it sidesteps the objection that killed the `ssh_args` proposal in #97: the deck does **not** parse `~/.ssh/config`, does **not** store forwarding configuration, and does **not** fork ssh's option grammar. ssh remains the single source of truth; the deck only reads what ssh already decided. Diagnosing the user's infrastructure is a different proposition from owning it.

## Solution

A read-only `dot-agent-deck remote doctor <name>` that runs a fixed list of checks and reports each as PASS / WARN / FAIL with a specific fix. No new state, no new configuration surface, no mutation of anything — it probes and reports.

Checks, roughly in dependency order:

| Check | Source | Catches |
|---|---|---|
| Host reachable, auth works | existing probe path | the ordinary broken-ssh case |
| Binary present, protocol compatible | `probe_remote_version` / `probe_remote_protocol` | drift the deck already classifies |
| Resolved forwards inventory | `ssh -G` | shows the user what ssh actually resolved, which may not be what they wrote |
| `dynamicforward` present | `ssh -G` | the wrong-direction mistake (#3 above) |
| `exitonforwardfailure` unset | `ssh -G` | silent forward failures (#2 above) |
| Remote `AllowTcpForwarding` | `sshd -T` on the remote | the indistinguishable-error case (#1 above) |
| Remote `ClientAliveInterval` | `sshd -T` on the remote | stale listeners orphaned by laptop sleep (#4 above) |
| Forward actually bound | probe the remote's loopback | a forward that failed despite looking configured |
| `forwardagent yes` | `ssh -G` | security advisory — every agent on the remote can use the laptop's ssh-agent |

The last one is an advisory, not a failure: it is a legitimate choice, just one the docs recommend against.

### Relationship to #344

#344 fixes the *error path* — classifying a forward failure as `ForwardFailed` instead of `HostUnreachable` when a connect attempt fails. This PRD fixes the *inspection path* — letting a user ask the question before or after a failure and get a specific answer. They are complementary and independent: #344 is ~15 LOC in an existing enum and should land first, since it is a wrong message today regardless of whether this command is ever built.

Deliberately out of scope for #344 and in scope here: distinguishing *which* cause produced a forward failure. That requires inspecting the remote's sshd config, which belongs in a diagnostic, not in an error constructor.

## Decisions

- **No experimental flag.** Decided during PRD creation, per CLAUDE.md rule 9. A diagnostic's entire value is being reachable when someone is already stuck, and a user debugging a broken tunnel will not have `experimental = true` set — gating it would hide the command from exactly the population it exists for. It is also read-only, so the usual reason to gate (unfinished behavior touching real state) does not apply. Output format can still be iterated after ship.
- **No cross-version contract impact.** This touches neither the daemon, the TUI↔daemon protocol, orchestration, nor hooks — it is a CLI command that shells out over ssh. No `PROTOCOL_VERSION` bump and no `.breaking.md` fragment (CLAUDE.md rule 12). Ships as a patch-level feature.
- **`ssh -G` rather than reading `~/.ssh/config`.** See "Key insight" above.
- **The liveness probe speaks SOCKS5, and that is a *write*.** Decided 2026-08-26, during the `ForwardBound` attribution work, and recorded here because it widens the read-only criterion above. A bare TCP connect only answers "is *something* listening", which cannot separate this recipe's tunnel from an unrelated service holding the port — so a squatter on an otherwise-perfect configuration rendered entirely PASS, exit 0, the flagship scenario reporting healthy while broken. The discriminator is the SOCKS5 no-auth handshake, which is definitive for the reverse-**dynamic** forward the #97 recipe configures: the deck sends the three bytes `05 01 00` (version 5, one method offered, method `00` = no authentication) and a reply of `05 00` proves the listener *is* a SOCKS proxy rather than merely present. A foreign service will not produce it.

  **Those bytes are written only to a `RemoteForward <port>` with no destination** — the reverse-dynamic form, i.e. a port the user declared to be SOCKS. A **concrete** `RemoteForward` carries whatever the user tunnelled, a database or an internal API, so it gets a connect and nothing else: writing three arbitrary bytes into someone's Postgres is not a diagnostic's business, and a line-oriented daemon can log or parse them. The gate lives in `ForwardProbe::for_forward`, which matches the one permitted variant and defaults everything else to connect-only, so a forward kind added later inherits the safe probe.

  **The read-only criterion is hereby scoped to *persistent* state** — ssh config, sshd config, the registry, files, services, and listener state on the remote — **not to bytes on a socket the probe itself opened.** Nothing the handshake does outlives the probe: no file is created, no configuration is touched, and the socket is gone when `bash` exits. This is written down rather than left as an implementation detail because `remote/doctor/005`, the read-only guard, cannot catch it: it inspects the recorded ssh argv for mutating *commands*, not for bytes written to a foreign socket. This decision is the record.
- **An accepting listener on a *concrete* reverse forward is UNKNOWN, not PASS and not FAIL.** The two confident answers are both wrong. PASS and WARN each exit 0 via `Verdict::is_clear`, so either would be false confidence about a listener nobody verified; FAIL would be a lie, because for a concrete forward an accepting listener usually *is* the user's tunnel working, and calling a healthy configuration broken trains people to ignore the tool. UNKNOWN (exit 2) says the true thing: your configuration is right, and the deck could not prove the listener is yours. This is day-one behaviour — `remote doctor` has never shipped a release — so it is an explanation to document, not a compatibility break to note.
- **The handshake's read is bounded independently of the ssh deadline.** `bash`'s `/dev/tcp` has no read timeout, so against a listener that accepts and never speaks — the quietest and most likely real squatter — an unbounded read would block until the ssh wallclock deadline, which `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS` lets a user stretch to an hour. The remote command therefore wraps the read in `timeout 2`, and its exit status is made to reflect the **connect** rather than the read (`|| exit 1` carries a refusal out, a trailing `exit 0` discards everything after it). That separation is what lets an empty reply mean "a listener accepted and said nothing" (FAIL) instead of being confused with tooling that never ran (UNKNOWN, from a non-zero status); absent `timeout` or `od` exits 127 and lands in the same UNKNOWN as an absent `bash`.

## Milestones

- [ ] **M1 — Command skeleton with the checks the deck already knows how to do.** `remote doctor <name>` exists, resolves the registry entry, and reports reachability / binary / protocol using the existing probe functions. Useful on its own for the ordinary "why won't this connect" case.
- [ ] **M2 — Forward inventory from `ssh -G`.** Parses the resolved forwarding directives and reports what ssh actually resolved for that destination, including the `[socks]:0` reverse-dynamic form.
- [ ] **M3 — Remote-side sshd checks.** Reads `AllowTcpForwarding` and `ClientAliveInterval` from `sshd -T` on the remote and reports the two causes that are indistinguishable from the client today.
- [ ] **M4 — Live forward liveness and advisories.** Probes whether a configured forward is actually bound on the remote; emits the `DynamicForward` wrong-direction and `ForwardAgent` advisories.
- [ ] **M5 — Tests.** Fast-tier coverage for `ssh -G` output parsing and check classification against captured fixtures. An e2e test can reuse [`scripts/reverse-tunnel-validation.sh`](../scripts/reverse-tunnel-validation.sh) — the harness written during #97 validation (sshd in a container, a laptop-loopback-only service), which already reproduces every failure mode above deterministically. Note the two false-pass traps it guards against, both of which cost real debugging time: an auth failure exits 255 exactly like a forward collision, and a wholesale forwarding refusal produces the same error text as a port collision, so a collision assertion must first verify the *first* session actually bound.
- [ ] **M6 — Documentation.** A troubleshooting entry wired into the #97 recipe's "Limits worth knowing" section, so the caveats and the command that checks them are cross-referenced.

## Success Criteria

- A user who has followed the #97 recipe and whose tunnel does not work can run one command and be told which of the known causes applies, with the fix.
- Each of the four failure modes in the Problem Statement is distinguishable in the output — in particular `AllowTcpForwarding no` versus a port collision, which no amount of client-side error text can currently separate.
- The command never mutates ssh config, sshd config, the registry, or the remote.

## Risks

- **Remote-side checks need `sshd -T`, which typically requires root.** Run as a normal user it fails or prints a partial config. Mitigation: treat an unavailable `sshd -T` as an explicit UNKNOWN result with a hint, never as a PASS — a diagnostic that silently reports "fine" when it could not look is worse than one that admits it does not know.
- **`ssh -G` output is a stable but not contractual format.** Mitigation: parse leniently (match known keys, ignore everything else), and never fail the whole command because one line was unrecognised.
- **Scope creep toward "fix it for me".** The command reports; it does not edit anyone's ssh config. Writing to `~/.ssh/config` was explicitly ruled out in #97 and stays ruled out here.
