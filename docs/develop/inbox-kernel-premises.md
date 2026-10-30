# Inbox kernel premises: E-PIN, E-LD and E-MF

PRD #1666 part 3's drain frontier (Q35-20a, `src/agent_pty_frontier.rs`) rests on P-DRN: after a zero-timeout poll of a PTY master reports nothing, every byte written to the slave before the poll and not discarded has been read from the master. P-DRN holds only under S-M, that no process other than the daemon holds the master. Amendment 5 §3.1 of the Q35-core design makes three evidence items acceptance gates for Q35-20a, and asks that the results be kept in the repository rather than only under the gitignored `.dot-agent-deck/`. This page is that record. Under `EcPolicy::Hold`, which is production's policy, a taken frontier authorises nothing, so these premises are load-bearing only for the `Authority` flip and for any later design that reads a frontier.

| Item | Status (2026-10-30) | Where |
| --- | --- | --- |
| E-PIN: re-pin the `upstream` (Linux `v7.0`) source rows against the running kernel, Ubuntu 7.0.0-31.31 @ `68d75f08` | **UNVERIFIED for every `.c` row.** Launchpad's plain-file endpoint still answers `504`. The header rows are pinned (below) | this page |
| E-LD: the writers of a master's `ldisc_sem` | **answered against `v7.0`**: none is reachable from the slave; P-DRN stands, pending E-PIN | this page; full excerpts in `.dot-agent-deck/sources/e-ld-ldisc-sem.md` (local) |
| E-MF: portable-pty's `openpty` → `FD_CLOEXEC` window | **checked at each frontier**: the reader scans `/proc` and publishes `Missing` when another process holds the master or could and can't be read | `src/composer_probe/master_holders.rs` |

## E-PIN

The `.c` rows (`tty_ldisc.c`, `tty_io.c`, `tty_port.c`, `tty_jobctrl.c`, `tty_ioctl.c`, `pid_namespace.c`, `user_namespace.c`, `ptrace.c`, `nsproxy.c`) come from `v7.0` @ `028ef9c9`, fetched on 2026-10-27 because Launchpad timed out. They stay evidence about `v7.0` until they are re-pinned from `68d75f08`. `pty.c`, `tty_buffer.c` and `n_tty.c` were already compared and are byte-identical between the two trees.

The header rows can be pinned now, from the running kernel's own headers package, `linux-headers-7.0.0-31` (7.0.0-31.31), installed under `/usr/src/linux-headers-7.0.0-31`:

| Header | Against `v7.0` |
| --- | --- |
| `include/linux/termios_internal.h` (`INIT_C_CC`, `:21–35`) | identical |
| `include/uapi/asm-generic/termbits.h` (`VMIN`/`VTIME` indices, `:47–48`) | identical |
| `include/uapi/asm-generic/termbits-common.h` | identical |
| `include/linux/tty.h` | identical |
| `include/linux/tty_buffer.h` | identical |
| `include/linux/ptrace.h` | identical |
| `include/linux/sched/signal.h` (`delay_group_leader`, `:731–738`) | one line differs, `:742` (a lock annotation, `__acquires` → `__cond_acquires`), outside the quoted range |

## E-LD

The method follows amendment 4's: start at the write side of `ldisc_sem` (`ldsem_down_write*`, in `tty_ldisc.c`), find each caller, and read each one to the entry points a process can reach. The receive path takes the master's `ldisc_sem` for read with a trylock (`tty_ldisc.c:259–269`); when that fails, `tty_port_default_receive_buf` receives nothing and the bytes stay pending (`tty_port.c:33–35`). So the question is which paths write-lock the **master's** semaphore while a reader lives. Line numbers are `v7.0`'s.

| Path | Reachable while the reader lives |
| --- | --- |
| `TIOCSETD` on a master fd (`tty_io.c:2727–2728` → `tty_set_ldisc`, `tty_ldisc.c:529`) | only through a master fd: under S-M |
| `TIOCSETD` on a slave fd | yes, but it locks the slave's semaphore: `tty_ioctl` passes the file's own tty (`tty_io.c:2670`, `:2728`) |
| a hangup of the master (`__tty_hangup` → `tty_ldisc_hangup`, `tty_io.c:621`; `tty_ldisc.c:718`) | only through `TIOCVHANGUP` on a master fd with `CAP_SYS_ADMIN` (`tty_io.c:2729–2732`): under S-M |
| `vhangup(2)`, a session leader's exit, `TIOCVHANGUP` on a slave fd | yes, but they hang up the slave: a master can't become a controlling tty, since `TIOCSCTTY` on a master fd acts on `real_tty`, the slave (`tty_jobctrl.c:584`). Their output flush discards bytes rather than stalling them (amendment 4 §7.1) |
| the master's own close (`pty_close` hangs up the slave, `pty.c:77`) | the reader holds a master fd, so this ends the reader first |
| the slave's close (`pty.c:65–67`) | yes, and it takes no lock of the master's |
| `tty_reopen` (`tty_io.c:1346`) | not for a master: it can't be looked up (`pty.c:669–673`) |
| the pair's setup (`tty_io.c:1421`) | before the reader exists |
| the pair's final release (`tty_ldisc_release`, `tty_ldisc.c:780`) | only once the master's count is zero, after the reader's end |

By amendment 5's decision rule, P-DRN stands: the semaphore's writers are reachable from a master fd, which S-M covers, or not while the reader lives. The daemon issues none of these ioctls: `src/` has no `TIOCSTI`, `TIOCSETD`, `TIOCVHANGUP`, `vhangup` or `TIOCPKT`.

## E-MF

portable-pty 0.8.1 calls `libc::openpty` and sets `FD_CLOEXEC` on both descriptors only afterwards (`src/unix.rs:56–58`). A fork and exec by another daemon thread in between inherits the master.

**Why it's checked rather than enforced.** A spawn lock would have to order every fork in the daemon process against `openpty`, including `std::process::Command` calls across the crate and in dependencies, which no lock in this crate can reach. An atomic close-on-exec open (`posix_openpt(O_CLOEXEC)` and `TIOCGPTPEER`) would need portable-pty's master and slave built from our descriptors, and its constructors are private, so it means replacing portable-pty's spawn path. Amendment 6 adds that atomic close-on-exec wouldn't establish S-M either, since a fork with no exec keeps every descriptor.

**What the check does.** The spawn reads the boot clock, in ticks, just before `openpty`. A process can hold the master only by inheriting it across a fork, so any holder other than the daemon started at or after that tick. Before publishing each frontier, after the zero poll, the reader lists `/proc` and reads the descriptors of every process started at or after that tick, except the daemon itself and kernel threads. A descriptor holds the master when it is the same `ptmx` inode, on the same device, with the same `tty-index` in its `fdinfo`. The frontier is `Missing` when:

- a process holds the master;
- a process whose real uid is the daemon's can't have its descriptors listed, unless it has started exiting or is a zombie. The kernel makes an exiting task's `/proc/<pid>/fd` root's, and such a task runs no more user code.

Another user's process is skipped: it can hold the master only by being sent it over a socket, which is outside the threat model, as `ptrace` and `pidfd_getfd(2)` are.

**What the check doesn't establish.** It is a reading at one instant, so:

- a holder that used the master and closed it, or exited, before the scan isn't seen;
- a process forked while `/proc` is being listed may be missed.

Two cases fail safe:

- **The daemon's own forks.** The daemon's fork-then-exec children hold the master between the fork and the exec, so a scan that lands there makes the frontier `Missing`.
- **Fork without exec.** In production the daemon has no such site. `src/wrap.rs`'s lifetime backstop forks only under `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`, and the reaper it forks closes its inherited descriptors. The editor helper's fork (`src/composer_probe/helper.rs`) runs in the helper process, not in the daemon.

**Coverage.** `src/composer_probe/master_holders.rs`'s units cover a sole holder, an inheritor, a process that started before the master and a zombie. `inbox/probe/062` covers the reader's `Missing` when a holder is found or unreadable. The 1,000-trial e2e (`tests/e2e_pty_drain_frontier.rs`) passes with the scan in place. Its first runs found the exiting-task case: the fixture's short-lived children made the frontier `Missing` until exiting tasks were skipped.
