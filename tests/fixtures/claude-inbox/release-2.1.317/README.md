# Claude Code 2.1.317: editor-probe, freshness, M59 and parser-cost evidence

Measured on 2026-11-03 for PRD #1666 part 3's activation, on the executable with SHA-256 `eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9ae57eb0`, the official `@anthropic-ai/claude-code-linux-x64@2.1.317` (byte-identical to the host's installed copy). The screen and ring captures for the same release are under `../r5wake/` (`claude-2.1.317-*`, described in `../r5wake/claude-menu-candidates.md`). Every run here was local (lane 2), in the TUI harness's own HOME, with every submission blocked by a private hook before a model turn. The raw captures, PTY bytes included, stay in the `source_capture` directory each summary names under `/var/tmp` on the recording machine; these files are compact summaries of them.

| File | Test | What it holds |
| --- | --- | --- |
| `editor-probe-088-export-selection-toggles.json` | `probe_088_live_claude_317_editor_export_selection_and_paste_toggles` | Each Ctrl+G invocation's editor argv, export path and contents, and the `?…h`/`?…l` mode writes before, during and after the editor: `VISUAL`, `EDITOR`, PATH `vi`/`nano` and PATH `code`/`vi`/`nano` cells, and an editor exiting 37 |
| `editor-probe-088-keymap-start-m49b.json` | `probe_088_live_claude_317_keymap_loaded_at_start` | M49b at start: five launches with a Ctrl+G-unbinding `keybindings.json`, acted on at the first composer frame |
| `editor-probe-088-keymap-unchanged-m49b.json` | `probe_088_live_claude_317_keymap_unchanged_ten_minutes` | M49b: the same file unchanged for ten minutes, one Ctrl+G a minute |
| `editor-probe-088-keymap-reload-m49b.json` | `probe_088_live_claude_317_keymap_created_and_removed` | M49b: the file created, then removed, during a session, with sampled times |
| `editor-probe-088-keymap-project.json` | `probe_088_live_claude_317_project_keymap` | The same file in the project's `.claude/`, with user settings only and with every settings source |
| `editor-probe-089-forms-m44e.json` | `probe_089_live_claude_317_ctrl_g_forms` | M44e: which of twelve wire forms opened the editor in six states, whole and split; ESC/BEL gaps; bracketed-paste BEL and the next export |
| `m59-090-armed-stall.json`, `m59-090-armed-timeout.json`, `m59-090-unarmed-long.json` | `probe_090_live_claude_317_*` | M59: per press, the helper's trace (start, branch, exit), how long it lived after its branch, its wait status when the zombie was seen, the editor's starts and exits, and the composer afterwards |
| `freshness-a20d7.jsonl` | `probe_083_claude_idle_screen_freshness_refusal_rate` with `DAD_PROBE_FRESHNESS_RELEASE=2.1.317` | A-20d-7: one line per run, three idle and two with sixteen `yes` burners |
| `parser-cost-replay.txt` | `scanner_budget_measurement` (release profile, `--ignored`) | The release scanner's added hold and CPU per generation over every recorded trace, these included |

The freshness runs were taken on a throwaway local build carrying a proposed 2.1.317 release record, because the `e2e` build's editor-probe assumption accepts only a release that has a record of its own. That record was never committed.
