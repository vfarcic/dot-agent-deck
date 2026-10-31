# Inbox kernel premises: E-PIN, E-LD and E-MF

PRD #1666 part 3's drain frontier (Q35-20a, `src/agent_pty_frontier.rs`) rests on P-DRN: after a zero-timeout poll of a PTY master reports nothing, every byte written to the slave before the poll and not discarded has been read from the master. P-DRN holds only under S-M, that no process other than the daemon holds the master. Amendment 5 §3.1 of the Q35-core design makes three evidence items acceptance gates for Q35-20a, and asks that the results be kept in the repository rather than only under the gitignored `.dot-agent-deck/`. This page is that record. Under `EcPolicy::Hold`, which is production's policy, a taken frontier authorises nothing. The premises gate Q35-20a's acceptance as premise-backed, M-FR-X's reading of its frontiers as sound, the `Authority` flip, and any later design that reads a frontier. Until E-PIN and E-MF are in, Q35-20a lands as observation code (decision Q86).

| Item | Status (2026-10-30) | Where |
| --- | --- | --- |
| E-PIN: re-pin the `upstream` (Linux `v7.0`) source rows against the running kernel, Ubuntu 7.0.0-31.31 @ `68d75f08` | **UNVERIFIED for every `.c` row.** Launchpad's plain-file endpoint still answers `504`. The header rows are pinned (below) | this page |
| E-LD: the writers of a master's `ldisc_sem` | **answered against `v7.0`**: none is reachable from the slave; P-DRN stands, pending E-PIN | this page; full excerpts in `.dot-agent-deck/sources/e-ld-ldisc-sem.md` (local) |
| E-MF: portable-pty's `openpty` → `FD_CLOEXEC` window | **UNVERIFIED.** A `/proc` census before each publication makes the frontier `Missing` when it finds another holder or can't complete, but a sampled census neither establishes nor enforces S-M | `src/composer_probe/master_holders.rs` |

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

The comparison, so it can be checked again: SHA-256 (first 16 hex digits) of each file in the `v7.0` copy and in `/usr/src/linux-headers-7.0.0-31` (package version 7.0.0-31.31), on 2026-10-30.

| Header | `v7.0` | 7.0.0-31 |
| --- | --- | --- |
| `include/linux/termios_internal.h` | `253508873d174df2` | `253508873d174df2` |
| `include/uapi/asm-generic/termbits.h` | `6769d652b4178463` | `6769d652b4178463` |
| `include/uapi/asm-generic/termbits-common.h` | `b022fb7a77f70ad6` | `b022fb7a77f70ad6` |
| `include/linux/tty.h` | `132e1af4beac811b` | `132e1af4beac811b` |
| `include/linux/tty_buffer.h` | `c8ba3e2968700181` | `c8ba3e2968700181` |
| `include/linux/ptrace.h` | `1150f9a6c9e4dea3` | `1150f9a6c9e4dea3` |
| `include/linux/sched/signal.h` | `8ec050eb7733d3ce` | `bc0d296b1ca63968` |
| `include/linux/sched/signal.h`, lines 731–738 only | `c36b2cac68493c86` | `c36b2cac68493c86` |

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

**Status: UNVERIFIED** (decision Q86), alongside E-PIN. portable-pty 0.8.1 calls `libc::openpty` and sets `FD_CLOEXEC` on both descriptors only afterwards (`src/unix.rs:56–58`), so a fork and exec by another daemon thread in between inherits the master. Q35-20a doesn't close that window, and what it adds doesn't establish S-M.

**What the reader does.** Before publishing each frontier, after the zero poll, it takes a census of the master's other holders (`src/composer_probe/master_holders.rs`). The spawn reads the boot clock, in ticks, just before `openpty`, and the census reads every task's descriptor table (`/proc/<pid>/task/<tid>/fd`) of each process started at or after that tick, except the daemon and kernel threads. A descriptor holds the master when it is the same `ptmx` inode on the same device with the same `tty-index`. The frontier is `Missing` when the census finds a holder, or when any read fails other than by its process, task or descriptor having gone (`ENOENT`, `ESRCH`). A task whose table can't be listed is skipped only when a re-read shows it exiting or a zombie (it runs no more user code; a table it shares is read through its live siblings), or when its real uid isn't the daemon's.

**Why it isn't S-M.** The census is one reading after the poll. Within the threat model (inheritance across `fork`; socket passing, `ptrace` and `pidfd_getfd(2)` are outside it), it still misses:

- a holder that used the master and closed it, or exited, before the reading, including a master-side change that outlives the process that made it;
- a process forked after the census listed `/proc`, whose parent then closed its own copy before being read;
- a thread created after its process's task list was read.

**What would establish it.** A causal mechanism, not a better census: an atomic close-on-exec pair (a portable-pty patch to its Linux pair creation, or a pair-creation broker that receives the descriptors close-on-exec), each still owing an analysis of fork without exec and of reacquisition. Neither is part-3 work unless the user reopens R-XW′ (Q86).

**Fork without exec in the daemon.** In production none was found: `src/wrap.rs`'s lifetime backstop forks only under `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`, and its reaper closes its inherited descriptors (`close_all_descriptors`); the editor helper's fork (`src/composer_probe/helper.rs`) runs in the helper process. The daemon's fork-then-exec children hold the master between the fork and the exec; a census that reads one then makes the frontier `Missing`.

**What it costs.**

- **Yield.** Any same-user process that can't be read and started after an agent's spawn makes that agent's frontiers `Missing` while it lives. On the development box this was measured on, those are `sshd-session` processes, one per SSH login. Neither review found a sound narrower discriminator (name, ancestry, session and uid aren't provenance), so they stay `Unknown` (Q85/Q86). Under `Hold` that changes nothing a user sees; it lowers how often M-FR-X gets a frontier.
- **Reader time.** The census runs on the pane's reader thread, outside the bus lock, once per zero poll that finds the master empty with a request pending, and the pane's output waits while it runs. Measured on the development box on 2026-10-30 (`census_latency`, debug build, 403 processes, 892 tasks, load 2.8), 30 runs each:

| Census | p50 | p90 | max |
| --- | --- | --- | --- |
| a generation just spawned (one process started since) | 13.0 ms | 16.9 ms | 29.4 ms |
| every task of every process, read errors ignored: a whole-walk proxy for a long-lived generation | 101.7 ms | 109.8 ms | 129.5 ms |

  These are census durations from a direct call, not the latency of a requested frontier on the reader thread, and the samples are not a ceiling: the whole walk skips the production scan's per-process stat and its re-read of the stat and status after a failed read, and the process population changed between samples. Before requests go live, the requested-frontier path needs its own release-build measurement and a reader-stall budget (Q35-20a review U3s). No production generation requests a frontier before activation, since installation's rollout is empty (Q35-13), so no user can see this delay today. At activation, a pause of that size after each editor return is something a user could notice, and rule 19's fragment question reopens then.

- **Throughput with no frontier requested.** The poll loop against the blocking loop it replaced (`src/agent_pty_frontier_throughput.rs`, release build, RWX sandbox, 16 cores, load 4.8, seven runs of each, 2026-10-30). These cover the extra `poll` per read only, not the census, since no frontier is requested:

| Reader | 256 MiB burst, median | MiB/s | one-byte round trip p50 | p99 | max |
| --- | --- | --- | --- | --- | --- |
| blocking loop | 1.991 s | 129 | 13.4 µs | 19.6 µs | 301 µs |
| poll loop | 2.005 s | 128 | 14.6 µs | 21.3 µs | 123 µs |

**Coverage.** The census units read the real `/proc` restricted to their own pids (a holder found, none found, a process started before the master, a zombie, and the reviewers' control: an inheritor whose leader left with the raw `exit` syscall while a sibling thread keeps the master, which the census finds through the sibling's task). A scripted `/proc`, one read at a time, covers these branches: a failed read other than going away is `Unknown` at the listing, the task list, the stat, a descriptor's link, file and `fdinfo`, a `ptmx` descriptor of this inode with no `tty-index`, the re-read stat and the real uid. The script replaces the listings and the `fdinfo` parser, so two units cover those below it: the collector every real listing goes through keeps an entry error that follows entries it read, and an absent or malformed `tty-index` parses as none. Hand mutants over those branches and the per-task reading: 10 of 10 caught, and 4 of 5 over the collector and parser; the survivor drops entry errors at the `read_dir` call that feeds the collector, which needs a real directory-entry error no unit produces. `inbox/probe/062` covers the reader's `Missing` when a census finds a holder or can't complete.
