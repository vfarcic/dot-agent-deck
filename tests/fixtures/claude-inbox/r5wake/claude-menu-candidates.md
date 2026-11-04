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


## Claude Code 2.1.317

Recorded on 2026-11-03 through the same local lane-2 harness and recorder, with the executable pinned by SHA-256 `eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9ae57eb0`. That executable is the official npm package `@anthropic-ai/claude-code-linux-x64@2.1.317` (tarball integrity `sha512-u19fYJ4llqi+dAGAj6P1BNu/CRSe0rv4O8Mv/5SQPU5oXCYmUssbJgrBI6rWbIFRAE3YcLCzKlwdHbzlSXBlSA==`), byte-identical to the copy the host's installer put in `~/.local/share/claude/versions/2.1.317`. It was copied to private scratch space outside the checkout and exposed through a private `claude` symlink. Every submission was blocked by the private hook before a model turn.

The capture directories this file names are `dad-claude-2312-measure-*` whichever release they hold, 2.1.317's included: the recorder's directory prefix carried 2.1.312's number until it was renamed `dad-claude-release-measure-*` after these runs, so the name says nothing about the release a capture holds; its `result.json` does.

The menu sets are the same seven named screens as the other releases'. The run that recorded them also re-recorded 2.1.310 and 2.1.312 and passed for all three; the committed 2.1.310 and 2.1.312 fixtures were not replaced. In every set, 2.1.317 draws the main rule, shell glyph and overview glyph in the same foregrounds as 2.1.310 and 2.1.312, with the same structural premise (a coloured main rule over a default-colour `❯`, the reverse in the overview).

| Set | Theme | COLORTERM | Bytes | Trace SHA-256 | Source run under `/var/tmp/` |
| --- | --- | --- | ---: | --- | --- |
| 24x80 | dark | none | 14994 | `b4a0beeea0c8a19fc6b01a16fd39a7e276027d15e25307f163dc07aec5bd6041` | `dad-claude-2312-measure-eC2mb3/capture` |
| 40x120 | dark | none | 16937 | `3f24e53c9e532dc63b39d999b67077dd93ae64be25552aedb07baab25a36a245` | `dad-claude-2312-measure-VNXkh4/capture` |
| 50x200 | dark | none | 17601 | `ae7bb7a8f1f8701fe8b9cb8424c0a93419c88f10438243077105541267e6ae85` | `dad-claude-2312-measure-xNk7Xe/capture` |
| light-50x200 | light | none | 17715 | `e81d04bc51df0486a083623946ecc156f9c93a1c39847ca5fd84bde2c48e547a` | `dad-claude-2312-measure-5zDrPd/capture` |
| truecolor-dark-50x200 | dark | truecolor | 18305 | `35c9434b579329a52859c3e879dad187e84996d226485e73f3e952b836caa565` | `dad-claude-2312-measure-sqidBt/capture` |
| truecolor-light-50x200 | light | truecolor | 18237 | `286defbcdbdb51802f6f5e5f23d285cf4298362a0d097145332a26988b0f6a41` | `dad-claude-2312-measure-RQoe8c/capture` |
| dark-ansi-50x200 | dark-ansi | none | 17658 | `52588f87dea6b9a06ddd211fd27bdc20433cad90c5b7a140d651879cba16dac5` | `dad-claude-2312-measure-ZgouhY/capture` |
| light-ansi-50x200 | light-ansi | none | 17091 | `83224e37dc90b369c46aa0b16c055b62066183270c69d174bf1fd7930a958891` | `dad-claude-2312-measure-IyxcMv/capture` |
| dark-daltonized-50x200 | dark-daltonized | none | 17847 | `2e0e8e0eae10f705901378a8239d7ee9fccaa8b03e2e6a78df9078f01e36dabf` | `dad-claude-2312-measure-9m4KM7/capture` |

The start-up, composer, layout and compound-key captures are committed as the recorder wrote them (index plus raw bytes), like the other releases'. `compound-keys` carries an added `end` point at the trace's length, as 2.1.312's does.

| Fixture | Bytes | Trace SHA-256 |
| --- | ---: | --- |
| start-50x200 | 1966 | `e894828c10ee025d2836f62803402bbaeff984d13b44d65a6b5ec53342f966f4` |
| composer-24x80 | 88233 | `d24c44d80745fc75f207e837769b91cf9af87a885a68c3461f4f8dc539442263` |
| layout-50x200 | 138500 | `8a24ec2b642094ec39da9e1b3941fe4704d689962097ed5a96b6b3ac102a0d5c` |
| compound-keys-50x200 | 44390 | `b826c11beca83c8fde680283150b75f9529cd25bdc70b34ad3c3850557d3f28e` |

One difference from 2.1.312 was seen while recording. After the hook blocks a submission, Claude Code puts the prompt back in the composer. In one 50×200 composer run on 2.1.317 it came back more than 0.5 s after the hook, after the recorder had already cleared the box, so the "cleared" frame still held the line. The recorder now waits up to 5 s for the prompt to come back as an editable draft before clearing, and records the wait. The first figures that wait gave (302 ms and 34 ms) do not stand: it matched the line's text, which at 24 × 80 never left the box, so it returned at once (`../release-2.1.317/README.md`, "The blocked prompt's restore"). With the corrected wait, on 2026-11-03, the largest restore after the hook was 742 ms at 50×200 and 368, 674 and 513 ms at 24×80 (three runs) on 2.1.317, against 726 ms and 445 ms on 2.1.312. So the late case was seen again, on both releases, and every one of those runs passed inside the 5 s bound. Restore latency is a property of the measurement's blocking hook; nothing in the record or in production reads it.

## Claude Code 2.1.318

Recorded on 2026-11-04 through the same local lane-2 harness, with the executable pinned by SHA-256 `708c77b204db30086ff6f1ed34577bda03e1be74ced80557cbf7b54249ebf0bd`. That executable is the official npm package `@anthropic-ai/claude-code-linux-x64@2.1.318` (tarball integrity `sha512-GX+7CUng2R5f7SsA2w0LaXlH2f62fRCFD/Z2SUexoeH3B0KfulKQt5DtY9NeD2Yp3VEBz6/k/xWSbGPUi6NV2A==`, matching the registry's), byte-identical to the copy the host's installer put in `~/.local/share/claude/versions/2.1.318` on 2026-11-04. It was copied to private scratch space outside the checkout and exposed through a private `claude` symlink. Every submission was blocked by the private hook before a model turn. Every 2.1.318 capture here was made by the recorder `tests/support/claude_release_measure.py` at SHA-256 `79931911…`, the revision committed with these fixtures, and copied into the fixture set by `tests/support/claude_fixture_export.py`, whose docstring gives the mapping (it regenerates the committed 2.1.317 `menus-24x80` fixture byte for byte from that capture).

`doorbell_036_live_release_318_menu_frames` recorded the nine menu sets on their own, through the same per-release body as `doorbell_036_live_release_menu_frames`. In every set, 2.1.318 draws the main rule, shell glyph and overview glyph in the same foregrounds as 2.1.310, 2.1.312 and 2.1.317, with the same structural premise and the same screen signatures; the fast replay reads all sixty-three screens right (no wrong acceptance, no wrong refusal). The first attempt ran inside the default 180 s nextest window and was cut off after eight sets; the recorded sets are the rerun's, under the 480 s window it now has.

| Set | Theme | COLORTERM | Bytes | Trace SHA-256 | Source run under `/var/tmp/` |
| --- | --- | --- | ---: | --- | --- |
| 24x80 | dark | none | 15573 | `ed6218e18deb55812c991b87cf94df3850df82c62f29dcc82a557edce7b8da37` | `dad-claude-release-measure-5GxWsQ/capture` |
| 40x120 | dark | none | 17503 | `a046573b3e242bb5dd5c2ed93680f33ad577cd13823edd64e48498168c81ee52` | `dad-claude-release-measure-ZC47u8/capture` |
| 50x200 | dark | none | 17187 | `a11458467e389a95faffbbffe14d2a7f86bd3ce5f33b9926e948194544ef213c` | `dad-claude-release-measure-l8Pznv/capture` |
| light-50x200 | light | none | 17362 | `aaf697f8eb769d464dc3881708ab6c7f4697954b971f84b5489e2e82d5a62e5a` | `dad-claude-release-measure-bGPe2W/capture` |
| truecolor-dark-50x200 | dark | truecolor | 17878 | `f5cc90e9d0f457f5f1bc341e4a5a059d5b685377bc5cc149e26ebe141d9f05c1` | `dad-claude-release-measure-ip3lnh/capture` |
| truecolor-light-50x200 | light | truecolor | 17402 | `313c9722063822142943bbb507ff2c5809bddde089cc1af46955689eed70d721` | `dad-claude-release-measure-pqU9wh/capture` |
| dark-ansi-50x200 | dark-ansi | none | 17137 | `9a07afc6b50e3d36199c1b25fc57cc3e40896dc12c02f162e016ca47b33ca7d3` | `dad-claude-release-measure-znnHAP/capture` |
| light-ansi-50x200 | light-ansi | none | 17493 | `a6e326229908f5509275164783fd0751a6640760b6f9d5aa5f7e6fdaf16df903` | `dad-claude-release-measure-7wn2px/capture` |
| dark-daltonized-50x200 | dark-daltonized | none | 17936 | `a9931e853af0d1b416ec00b27228b74731a85fd9dfb268ded42c169bc45beaf1` | `dad-claude-release-measure-gpAgXx/capture` |

The start-up, composer, layout and compound-key captures are committed as the recorder wrote them (index plus raw bytes), and `compound-keys` carries an added `end` point at the trace's length, as 2.1.312's and 2.1.317's do. The start-up fixture is the first of the ten 2.1.318 start-ups.

| Fixture | Bytes | Trace SHA-256 |
| --- | ---: | --- |
| start-50x200 | 1979 | `25452df782497f94c17b934cd1745c152b0e0afd672c00391c6a1f8a95de02b5` |
| composer-24x80 | 89277 | `0decdfd1a815fa2b0e9dfcf850c62f32afff4d9d8c7c81ac0db506a9b5002876` |
| layout-50x200 | 147283 | `6f2596764a5d0e926a871ba97e7b8d867c712bdff922269d90dc0244a1c25a9a` |
| compound-keys-50x200 | 44504 | `b145c1be33fefbbbfb57595f86cec4e0ffddedf7d21b8b89987101ca4b1184ce` |

The blocked prompt's restore, with the corrected wait, was at most 348.7 ms after the hook at 24×80 and 361.6 ms at 50×200 on 2.1.318, inside the 5 s bound; as for 2.1.317, that is a property of the measurement's blocking hook, which nothing in the record or in production reads.

## History recall (Claude Code 2.1.317 and 2.1.318)

Recorded on 2026-11-04 by `doorbell_036_live_release_history_recall_frames`, with the same recorder (`79931911…`, its `history` kind), pins and hook. A blocked prompt does not enter Claude Code's prompt history (Up after two blocked submissions recalled nothing, on 2.1.317), so the recorder seeds six prompts from an earlier session in this project into the disposable HOME's `.claude/history.jsonl`, in the shape Claude Code keeps them there (`display`, `pastedContents`, `timestamp`, `project`, `sessionId`); it never reads or writes the host's history. Each capture's `.frames.txt` lists every named point with what the profile reads there (composer, ready, empty-and-ready, dialog).

What the captures show, on both releases and at both sizes:

- Up from the empty composer draws `─── History 6/6 ` and the rest of a rule in place of the composer's upper rule, over the newest recalled prompt with the cursor after it; each further Up steps to an older entry (`5/6`, `4/6`), and Down past the newest returns the plain composer. Escape leaves the recalled prompt and its History rule in place; Ctrl+U clears it and draws the plain composer; the history position is kept (the next Up recalls the next-older entry: `History 5/6` before Ctrl+U, then `4/6` after one Up), so this frame is the same state as the blank History-labelled slot below, drawn with a plain rule; Ctrl+C clears it too; Enter submits it (blocked again, so Claude puts it back as a draft). Up over a typed draft recalls the newest entry, and Down returns the draft with the cursor at its start.
- After Shift+Tab, the mode line under a recalled prompt reads `⏸ manual mode on` (in the harness's settings the permission mode starts as `⏵⏵ auto mode on`). The mode line is not what the profile reads: it refuses these frames because the row above the prompt is not a plain full-width rule.
- **After Ctrl+U clears a recalled prompt, walking Up and Down back to it finds that slot blank.** With nothing else typed in between (`slot-cleared`, then `slot-up-1` … `slot-down-3`), revisiting it (`slot-down-1`, `slot-up-2`, `slot-down-3`) draws `─── History 6/6 ` over a blank box with the cursor at the indent: cell for cell the empty ready composer apart from the rule's label. That is not a lasting edit of the history: earlier in the same captures, prompts cleared the same way were recalled intact after a typed draft and an Enter, so how long a slot stays blank was not measured. (The recorder's own comment in its `history` kind says Ctrl+U "empties that history slot for the session"; that is wider than these captures show, and it is left as written only so the committed recorder stays the file whose SHA-256 the fixtures carry. This paragraph is the reading.)
- **What the deck's line plus Enter does in that state** (the plain-drawn one, and the cleared slot under its History label) is measured in [`../history-recall-submit/README.md`](../history-recall-submit/README.md) (Q143b): the line alone was submitted and the history stayed intact in 80 of 80 trials on 2.1.317 and 2.1.318.
- **Recorder notes, to fix at the next recorder change** (each changes the SHA-256 `79931911…` the fixtures cite, so neither is fixed alone): (U3-1) the `history` kind's restore wait (`self.wait(lambda: restored(line), 5)`, right after the hook count reaches 1) does not first wait for the submitted form the way the composer kind does, so it could match the pre-submit draft; in all four committed captures the order is typed, submitted, then restored, so the `restored-1` snapshot is the genuine restore. The Enter way has no restore wait, only a fixed pump. (U3-2) `recall()`'s comment says "Down and Up are tried in turn", but the loop sends only Up.
- Every recorded point drawn under a History rule (24 per capture) has no composer, no readiness and no empty box to the profile, and admission's frame builder reads no composer there. 2.1.317 and 2.1.318 draw the same rule and prompt rows at every point after the first (whose `Try "…"` placeholder is chosen at random per launch).

| Release | Size | Bytes | Trace SHA-256 | Source run under `/var/tmp/` |
| --- | --- | ---: | --- | --- |
| 2.1.317 | 50x200 | 7396 | `818b106f2268c80258787517fb82d2e987a77657cf6a7d629487188877083139` | `dad-claude-release-measure-F8sAtw/capture` |
| 2.1.317 | 24x80 | 6542 | `c25bdc2a64d7eaabb9d368178cb9f6d66c5ff0ece3e55dae4c181dd1a38a385a` | `dad-claude-release-measure-mfyWSi/capture` |
| 2.1.318 | 50x200 | 7457 | `c87ab96fe573fc616bbfff4ab9832f55c1e9afc475bc03986ab4fb9ceb59f329` | `dad-claude-release-measure-YSBO42/capture` |
| 2.1.318 | 24x80 | 6592 | `2c963a6c617b6b0837d316c7750c5d55ed50691ef829974042a739e0f0c45f30` | `dad-claude-release-measure-3czas6/capture` |
