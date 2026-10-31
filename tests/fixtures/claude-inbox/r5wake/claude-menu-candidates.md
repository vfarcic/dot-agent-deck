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

The sampled surfaces have the same layout in both releases: the slash and file menus are above the composer, shell mode uses `!` in place of `❯`, the exit warning occupies the footer, and the overview has its own prompt and sessions list. The version header, disposable paths and randomized empty-composer suggestion differ. These original observations cover 50×200 in the default dark theme; the expansion below records additional sizes and the light theme.

To record again, obtain the pinned executables outside the checkout, make Python `pyte` available, and run locally with the two source paths supplied explicitly:

```bash
DOT_AGENT_DECK_CLAUDE_MENU_2_1_310=/private/path/to/2.1.310/claude \
DOT_AGENT_DECK_CLAUDE_MENU_2_1_312=/private/path/to/2.1.312/claude \
DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 \
cargo test-e2e-live --test e2e_claude_release_measure doorbell_036_live_release_menu_frames
```

The capture roots are printed by the test and retained for inspection. Only the selected fixture candidates are committed; credentials and hook payload files stay outside the checkout. The source recorders signal and reap their captured child PID, and the TUI harness owns teardown of its private deck.

## Size and theme expansion

Recorded on 2026-10-31 through the same local lane-2 harness, using the executable pins above, Haiku and the blocking UserPromptSubmit hook. Each capture contains the same seven named screens. The new stems are `claude-<release>-menus-24x80`, `claude-<release>-menus-40x120` and `claude-<release>-menus-light-50x200`; each has unchanged raw PTY `.bin`, indexed `.json` and Rust/vt100-rendered `.frames.txt`. Rows precede columns, and there are no resizes. The indices include theme, source run, recorder SHA-256, executable SHA-256, trace SHA-256 and input events. No imported credential or hook payload file is exported.

| Release | Size | Theme | Bytes | Trace SHA-256 | Source run under `/var/tmp/` |
| --- | --- | --- | ---: | --- | --- |
| 2.1.310 | 24×80 | dark | 16515 | `fa23db9bff217d051b74dddca2b77637f0185d8d3333507992bbf0556320d6d9` | `dad-claude-2312-measure-NwkwzR/capture` |
| 2.1.310 | 40×120 | dark | 17586 | `5cd628ff39c155d9bc90ac415206e103d2495b5b4b8bae01a9bb987bdce1f961` | `dad-claude-2312-measure-5GJhfh/capture` |
| 2.1.310 | 50×200 | light | 18335 | `547028a9c103cc8052dc8d39a56c900d2560e23bbc06c4889dba4281948dd281` | `dad-claude-2312-measure-a72DHZ/capture` |
| 2.1.312 | 24×80 | dark | 16266 | `3582d1431e4d1095c67bc6cda24e037cad0522d5b48694288a24552495a2b1d3` | `dad-claude-2312-measure-zvCwXp/capture` |
| 2.1.312 | 40×120 | dark | 18317 | `36d350024fa64c189d8100964a8c39e7030277f0b69d7ce6b74f751c84ecb44d` | `dad-claude-2312-measure-8mqyKy/capture` |
| 2.1.312 | 50×200 | light | 18312 | `28349efd9d69a98e568fcd72897c1e32747d207d4fb8c749c2ba9bc53b814bbe` | `dad-claude-2312-measure-1EtB8Z/capture` |

The recorder sets Claude's built-in `dark` or `light` theme in its disposable HOME's `.claude.json` and `.claude/settings.json`. The user setting takes precedence over the legacy preference; setting only the latter left an inherited dark user setting effective during an earlier discarded trial. Acquisition now checks that light and dark actually draw different shell colours. In both releases the light main rules use foreground index 245 and shell rules/glyph use 199, versus 244 and 211 in dark. The rendered files also list the actual foreground/background of the prompt and rule cells at each point; raw bytes retain all attributes. These captures use xterm-256color without COLORTERM and do not establish truecolor or arbitrary-theme coverage.

At narrow widths the restored quotation wraps across several composer rows, so a single Ctrl+U clears only its last visual line. The expanded recorder instead sends paced Backspaces for every character and waits until the draft is visibly empty before taking `transcript-quotation`. It checks the blank prompt independently of the recogniser under test. Each retained capture has one submission blocked by the hook and a visible `UserPromptSubmit operation blocked by hook` transcript. The quotation is a real hook-rendered user echo; there is no requested assistant reply or shell command execution.

The slash menu's description and the overview's footer wrap or truncate at narrower sizes, while the menu lists remain above the composer. The warning remains on the first status row. A right-aligned quota notice appears immediately above the main composer's upper rule after the blocked quotation; at narrow widths it begins left of the midpoint. The two release versions retain the same sampled structures and theme palettes, with differing release headers, temporary paths, placeholder suggestions and byte/read boundaries.

The live acquisition test now records all four size/theme combinations per release, including the original 50×200 dark control. The existing fast replay test covers the original fixtures plus these six candidates and reports every wrong readiness verdict, distinguishing wrong acceptance (safety) from wrong refusal (availability). Fixture identities and expected readiness remain independent of the current recogniser's verdict.

Against recogniser `6de2f810`, all thirty new negative screens are refused and both releases pass all seven light-theme checks. Both narrow empty-start controls also pass. The four narrow `transcript-quotation` positives are wrongly refused (`ready=false`, `empty_and_ready=false`, expected true): two releases × 24×80 and 40×120. These visibly blank main composers have a right-aligned quota notice starting at column 16 of 80 or column 56 of 120 immediately above the upper rule, inside the recogniser's left-half suggestion region. The 50×200 controls put the notice beyond the midpoint and remain ready. The replay is intentionally RED for these availability failures; no wrong acceptance was observed in the captured set, and no claim about uncaptured colours or layouts follows.
