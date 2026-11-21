# Codex 0.164.0: the status line after `/cd` and `/worktree` (issue #2130)

Recorded on 2026-11-19 on the pinned native executable (SHA-256 `b93f6811…7f306`, `summary.json`'s `executable_sha256`) by `tests/support/codex_editor_measure.py`'s `cwd` and `worktree` modes (`recorder_sha256` in `summary.json`), in the nollm sandbox the other files in `../editor-probe/` describe: a fresh `CODEX_HOME` with a dummy key, a canned Responses server on 127.0.0.1, `bwrap --unshare-net` with `/home`, `/root`, `/tmp` and `/run/user` on empty tmpfs mounts, and the environment started from `COLORTERM`, `HOME`, `LANG`, `PATH`, `PWD` and `TERM`. Nothing was submitted. Every folder named here is trusted in the sandbox's `config.toml`.

`tests/codex_session_cwd.rs` (`inbox/probe/103`) parses each `.bin` at its size and reads the status line the deck compares with the folder Codex started in.

## What was measured

`cwd`: four launches, at 50 × 200 and 24 × 80, each started in `ROOT/work` (outside `HOME`) and in `~/plain` (`HOME` is `ROOT/home`). In each, Ctrl+G and then F6 were pressed on the empty box, then `/cd` into `ROOT/other`, `~/proj` and a long path `ROOT/<60 l>/<60 m>/proj`, each holding a `.codex/config.toml` that sets `[tui.keymap.global] open_external_editor = "f6"`, and finally back to where the launch started, with Ctrl+G and F6 pressed after each move. `summary.json` lists every status line and key result.

- In all four launches, after each of the three moves Ctrl+G opened no editor and F6 opened it (12 of 12); after the move back Ctrl+G opened it and F6 didn't (4 of 4), as at the start (4 of 4).
- The status line under the box named the folder each `/cd` moved to, `~/…` under `HOME` and the absolute path elsewhere. At 24 × 80 the long path was cut short with `…`; at 50 × 200 it was whole.
- An earlier run of the same steps, by a scratch copy of the recorder that this one was made from, gave the same results (not committed).

`worktree`: one launch at 50 × 200 in `ROOT/work`, a git repository with one empty commit. `/worktree`, then Enter on its first entry (`Continue current conversation`), moved the status line to `~/.codex/worktrees/<id>/work`; Ctrl+G still opened the editor there, since that checkout has no project file of its own.

`/resume` is not here: the picker listed no recorded session in this sandbox under either of its filters, so it was not measured.

## Files

| File | Size | Cut before | Status line |
| --- | --- | --- | --- |
| `work-24x80-start.bin` | 24 × 80 | the first Ctrl+G | `nollm-model default · ROOT/work` |
| `work-24x80-cd-other.bin` | 24 × 80 | the Ctrl+G after `/cd ROOT/other` | `… · ROOT/other` |
| `work-24x80-cd-home-proj.bin` | 24 × 80 | the Ctrl+G after `/cd ~/proj` | `… · ~/proj` |
| `work-24x80-cd-long.bin` | 24 × 80 | the Ctrl+G after `/cd` into the long path | the path cut with `…` |
| `work-24x80-cd-back.bin` | 24 × 80 | the Ctrl+G after `/cd ROOT/work` | `… · ROOT/work` |
| `plain-50x200-start.bin` | 50 × 200 | the first Ctrl+G | `nollm-model default · ~/plain` |
| `worktree-50x200.bin` | 50 × 200 | the Ctrl+G after `/worktree` | `… · ~/.codex/worktrees/4c8f/work` |

Each `.bin` is the launch's raw PTY output from its start up to the byte offset where the recorder sent that key, after the composer was emptied, so the screen it parses to is Codex's ready box. `summary.json`'s `files` gives each one's launch, cut label, byte count and SHA-256, and its `cwd.root` and `worktree.root` the sandbox roots the absolute paths start with. The full captures stay under `/var/tmp/dad-2130-rec-*` on the recording machine.

## Box load

`/proc/pressure/cpu` `some avg10` was 0.01 % at the start of the `cwd` run and 0.23 % after the `worktree` run, with a 1-minute load average of 1.06 and 1.40 on that host; no other unit was recording Codex then, by the recorder's own report.
