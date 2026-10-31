# Claude Code menu palette fixture candidates

Recorded locally on 2026-10-31 with the lane-2 `doorbell_036_live_release_menu_frames` harness, based on part-3 head `25be5ffb`. This set extends the original [menu candidates](claude-menu-candidates.md) with five palettes for each pinned release, at 50 rows × 200 columns without resizing. These are fixture candidates for review, not a claim about unrecorded themes, geometries or transient redraws. The separate narrow/light set on `b73294dd` is not a dependency of this set.

Each `claude-<release>-menus-<set>-50x200.bin` is the unchanged raw PTY output. Its `.json` records the read lengths, named offsets, input events, geometry, exact theme/environment, executable/recorder/trace SHA-256, source capture and blocked-submission count. Its `.frames.txt` contains Rust/vt100-rendered screens, cursor positions and prompt/rule foreground/background annotations, with trailing whitespace removed from each readable line. No imported credentials or hook payload file is copied into these fixtures. Rendered account tier, quota information, uid and disposable harness paths remain visible.

The executable pins are unchanged: Claude Code 2.1.310 is the private official npm extraction at `/var/tmp/dad-p3-claude-menus/2.1.310/package/claude`, SHA-256 `b73676430af12f97316a798b8371f81d640f90a58bdcb8f3ce56a835bb47df56`; 2.1.312 is read from `/home/vfarcic/.local/share/claude/versions/2.1.312`, SHA-256 `4f4178ce41d83e23341ebfbaecdbcf8ea6bf7baea0690025abe49eeec3a5d01d`. The recorder verifies the executable version/hash before launch and hash after reaping its captured child. The Python recorder used for these captures has SHA-256 `c459982c15659050c549e9eeb845ac0a128b4943b8480ecc48cef4a93eb13416`.

## Settings and visible palettes

The exact `theme` values below are written into both `.claude/settings.json` and the legacy `.claude.json` inside each disposable harness HOME; user settings override the legacy preference. `TERM=xterm-256color` is the harness default in every set. `COLORTERM=truecolor` is present only in the first two sets and removed for the other three. The outer deck TUI's truecolor environment is distinct from the recorded inner Claude PTY. The capture test checks these three foreground samples against the measured values, independently of readiness.

| Fixture set | `theme` | `COLORTERM` | Main upper rule | Empty shell `!` | Overview `❯` |
| --- | --- | --- | --- | --- | --- |
| `truecolor-dark` | `dark` | `truecolor` | RGB `(136,136,136)` | RGB `(253,93,177)` | RGB `(153,153,153)` |
| `truecolor-light` | `light` | `truecolor` | RGB `(138,138,138)` | RGB `(255,0,135)` | RGB `(102,102,102)` |
| `dark-ansi` | `dark-ansi` | absent | index 7 | index 13 | index 7 |
| `light-ansi` | `light-ansi` | absent | index 7 | index 5 | index 8 |
| `dark-daltonized` | `dark-daltonized` | absent | index 244 | index 75 | index 246 |

These samples agree across the two releases. They demonstrate actual palette changes: RGB versus indexed output, the two ANSI shell colors and overview colors, and the daltonized shell's index 75 versus the original indexed-dark shell's index 211. The main prompt foreground is Default and the overview rules are Default in these samples; the overview prompt is colored. ANSI index 7 is a basic terminal color, not grey index 244/245. All annotated samples have Default background. The annotations are measurements of these cells, not an assertion that the complete stream uses only those colors.

## Acquisition and expected screens

The recorder uses the existing lane-2 TUI harness with its own HOME, trust configuration and private endpoints. It starts interactive `--model haiku --setting-sources user` locally; both releases display `Haiku 5.5`. The harness's private UserPromptSubmit hook blocks the one quotation submission before a model turn; each trace contains the visible `UserPromptSubmit operation blocked by hook` result. The overview advertises another model for new sessions, but no overview task is submitted and no new session is started there. No assistant reply is requested, and empty shell mode is opened without executing a shell command.

The seven indexed screens are the empty main composer, slash-command menu, file menu, empty shell mode, first-Ctrl+C exit warning, normal empty composer beneath warning phrases quoted in the blocked user transcript, and the actual background-session overview. The recorder clears the restored quotation with paced Backspaces, waits for an empty bounded composer, then presses Left twice to open the overview. The acquisition test inspects every cell between the quotation composer's rules and its cursor, so a blank first line with a retained continuation cannot satisfy the empty premise. The overview signature is required in its own `❯ describe a task for a new session` row, not merely in the transcript quotation.

| Screen | Expected readiness and empty-and-ready |
| --- | --- |
| `empty` | true |
| `transcript-quotation` | true |
| `overview` | false |
| `slash` | false |
| `at` | false |
| `bang` | false |
| `exit-warning` | false |

## Recogniser results on part-3 head

`doorbell_036_claude_composer_over_real_frames` passes against `25be5ffb`, using the production recogniser unchanged. Both direct readiness and `empty_and_ready` agree with the table above in every new case. This is a GREEN evidence extension: no requested case required an implementation change or an intentionally failing test.

| Release | Set | Correct main-composer acceptances | Correct other-surface refusals | Wrong acceptance (safety) | Wrong refusal (availability) |
| --- | --- | ---: | ---: | ---: | ---: |
| 2.1.310 | truecolor-dark | 2 | 5 | 0 | 0 |
| 2.1.310 | truecolor-light | 2 | 5 | 0 | 0 |
| 2.1.310 | dark-ansi | 2 | 5 | 0 | 0 |
| 2.1.310 | light-ansi | 2 | 5 | 0 | 0 |
| 2.1.310 | dark-daltonized | 2 | 5 | 0 | 0 |
| 2.1.312 | truecolor-dark | 2 | 5 | 0 | 0 |
| 2.1.312 | truecolor-light | 2 | 5 | 0 | 0 |
| 2.1.312 | dark-ansi | 2 | 5 | 0 | 0 |
| 2.1.312 | light-ansi | 2 | 5 | 0 | 0 |
| 2.1.312 | dark-daltonized | 2 | 5 | 0 | 0 |

The 70 new cases and 14 original indexed-dark 50×200 controls pass together, 84/84. The existing composer, submission, clearing and compound-key controls also remain in this test. Independent pyte replay matches all 70 new rendered frames, cursors and palette annotations; indexed read-length sums, offsets and raw hashes agree. This matrix measures ready predicates on recorded end states, not a live doorbell write, resize or surface transition. It does not resolve the separate narrow-width quotation refusals in the second fixture set or establish unrestricted Claude probe activation.

## Raw traces and retained source captures

Source directories below are retained under `/var/tmp/<source>/capture` on the recording machine; only raw/index/rendered candidates are exported.

| Release | Set | Bytes | Trace SHA-256 | Source |
| --- | --- | ---: | --- | --- |
| 2.1.310 | dark-ansi | 16417 | `915f61a191a9d6316455709fc06e08caf2760263acdca4eadc27f132bc8ae0e2` | `dad-claude-2312-measure-Ff9Hyh` |
| 2.1.310 | dark-daltonized | 17498 | `494e8fc95e0bf643a1ba8711b173770fb3e6b3e4b2cf46a102552e7fd74fab6b` | `dad-claude-2312-measure-1Lmudf` |
| 2.1.310 | light-ansi | 17537 | `40f9684674eed69f1b8a4ee4da279dafce1b6d7b740dbf60afde70f50dbe7ff1` | `dad-claude-2312-measure-cKeJdb` |
| 2.1.310 | truecolor-dark | 18457 | `c5cb53cc27ec6518f7da945f2d47405a8213256748772726df7492c7b996523f` | `dad-claude-2312-measure-ksQMeA` |
| 2.1.310 | truecolor-light | 17126 | `57bd4044e287f9baf224172fe8e55935e18ae61dd43acb7d20ad4d028854e1ad` | `dad-claude-2312-measure-RUsXyr` |
| 2.1.312 | dark-ansi | 18350 | `c4e549732421679baa3a3a5ce3873ecf6535812a31ec6ec39e44c2d50f489bfb` | `dad-claude-2312-measure-XIO5Kw` |
| 2.1.312 | dark-daltonized | 17086 | `2b5a68882e26039d22da0ba1e5059f74e0f1382d397d4927ceebdad9b972134a` | `dad-claude-2312-measure-au8QX0` |
| 2.1.312 | light-ansi | 17193 | `66b17c9ffb773e079952d5962b97e685e18ad77dcf34295b6472b73dabae01ea` | `dad-claude-2312-measure-QIQZ83` |
| 2.1.312 | truecolor-dark | 17979 | `70ffd293b1bcc969e2d5dce9d6bd3aa35925eeed23068f02015cd15ad02bf49c` | `dad-claude-2312-measure-M1oBj7` |
| 2.1.312 | truecolor-light | 18029 | `ab7bdcf4cfe3bc65ea1cce0b72ea0206bbfd2ee929621c28133dcf851db75f11` | `dad-claude-2312-measure-VyD81u` |

To reproduce, use the local lane-2 command in the original README with both pinned executable paths. `doorbell_036_live_release_menu_frames` now records these five palette sets per release and prints the retained capture roots. Real-agent launches stay local, outside RWX; offline fixture replays and fast gates can run in RWX. Teardown signals only the captured child PID and reaps it; the TUI harness tears down its private deck. No production daemon or real configuration is modified.
