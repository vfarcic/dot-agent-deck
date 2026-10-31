# Claude Code menu and warning fixture candidates

Captured on 2026-10-31 by `doorbell_036_live_release_menu_frames` in `tests/e2e_claude_release_measure.rs`, using the existing lane-2 TUI harness and `tests/support/claude_release_measure.py`. These are newly recorded candidates for O-20.1, inspected as rendered screens by the tester; they do not replace the missing captures from the previous machine by provenance claim.

| Release | Executable SHA-256 | Raw PTY bytes | Trace SHA-256 |
| --- | --- | ---: | --- |
| 2.1.310 | `b73676430af12f97316a798b8371f81d640f90a58bdcb8f3ce56a835bb47df56` | 11332 | `ef5b3263c60ddb4c3e406bc1162d675b09797082a8188d1f0886664435c912e2` |
| 2.1.312 | `4f4178ce41d83e23341ebfbaecdbcf8ea6bf7baea0690025abe49eeec3a5d01d` | 11370 | `fb1773d22f3b675102ffd559a44bfdb24f526c7fc4bc56dd87305aa5778a95b0` |

Each `claude-<release>-menus-50x200.bin` is the unchanged raw PTY output. Its `.json` indexes the actual read lengths, named byte offsets, input events, initial geometry (50 rows by 200 columns), release and hashes; there are no resizes. Its `.frames.txt` contains the rendered frames and cursor positions replayed by the Rust harness through vt100, with trailing whitespace trimmed for readability. Paths in the frames name disposable harness directories, not the user's checkout or HOME. The original run directories are `dad-claude-2312-measure-IXkO0O` (2.1.310) and `dad-claude-2312-measure-oFKUkA` (2.1.312) under `/var/tmp` on the recording machine.

The 2.1.310 executable was extracted from the official npm package `@anthropic-ai/claude-code-linux-x64@2.1.310` into private scratch space, without installing it globally; that package archive has SHA-256 `8c90b0170d08fdb370db282c61d74897b834fc2569dd14c35eb1e491212dc6de`. The 2.1.312 executable was read from `~/.local/share/claude/versions/2.1.312`. Both executables printed their exact version with `--version`; the recorder checks the executable hash before running it and after reaping its captured child. Each is exposed to its harness through a private `claude` symlink on a private executable search path outside the checkout. No profile or installed executable was changed.

The recorder launches interactive `--model haiku --setting-sources user` with the harness's own HOME, trust settings, imported credentials and private endpoints, locally rather than in RWX. Both releases label this running composer `Haiku 5.5`. The overview advertises `New sessions: Opus 5.5`; no new session is started there. The recorder replaces the disposable HOME's UserPromptSubmit hook with a blocking hook before launching Claude. The one submission in each capture is blocked, and the transcript visibly says `UserPromptSubmit operation blocked by hook`.

The sequence is an empty main composer; `/` and its command list; Escape and Ctrl+U; `@` and its file list; Escape and Ctrl+U; empty `!` shell mode; Escape and Ctrl+U; Ctrl+C and its exit warning; a three-second wait; one blocked submission quoting the warning phrases; Ctrl+U to leave a normal empty composer with the `Original prompt: …` quotation still in the transcript; then two paced Left presses to open the actual background-session overview. The recorder checks that the overview's own `❯ describe a task for a new session` row is visible, rather than accepting that phrase from the quotation. The positive is a hook-rendered transcript quotation, not an assistant model reply. Shell mode is opened without executing a shell command.

Expected readiness is true at `empty` and `transcript-quotation`, and false at `slash`, `at`, `bang`, `exit-warning` and `overview`. `quotation-echo` is an additional indexed moment before the draft is cleared. The existing `inbox/doorbell/036` fast replay test loads these candidates without launching an agent and reports every wrong surface; it is intentionally RED against a recogniser that accepts these menus and warnings. The lane-2 capture test asserts acquisition signatures rather than the recogniser's unfinished negative rules.

The sampled surfaces have the same layout in both releases: the slash and file menus are above the composer, shell mode uses `!` in place of `❯`, the exit warning occupies the footer, and the overview has its own prompt and sessions list. The version header, disposable paths and randomized empty-composer suggestion differ. These observations cover this geometry and sequence.

To record again, obtain the pinned executables outside the checkout, make Python `pyte` available, and run locally with the two source paths supplied explicitly:

```bash
DOT_AGENT_DECK_CLAUDE_MENU_2_1_310=/private/path/to/2.1.310/claude \
DOT_AGENT_DECK_CLAUDE_MENU_2_1_312=/private/path/to/2.1.312/claude \
DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 \
cargo test-e2e-live --test e2e_claude_release_measure doorbell_036_live_release_menu_frames
```

The capture roots are printed by the test and retained for inspection. Only the selected fixture candidates are committed; credentials and hook payload files stay outside the checkout. The source recorders signal and reap their captured child PID, and the TUI harness owns teardown of its private deck.
