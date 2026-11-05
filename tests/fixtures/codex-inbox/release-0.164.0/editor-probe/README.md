# Codex 0.164.0: editor-probe, E1F, M58r and M59 evidence

Measured on 2026-11-05 for PRD #1666 part 3, unit U4 (Codex probe measurements after the Q35-22 binding), on the native executable with SHA-256 `b93f6811d3f653f0fb83be4050d857fb9abbb111dfa2ce160563327daff7f306` (`@openai/codex-linux-x64@0.164.0`, the package the earlier Codex units unpacked under `/var/tmp/dad-cxhe-codex-0.164.0/`, byte-identical to the host's npm install). These files are what `tests/codex_editor_evidence.rs` (fast tier, `inbox/probe/092`–`094`) replays through the deck's own readers; the lane-2 tests that record them afresh are `tests/e2e_codex_editor_measure.rs` (`092`–`094`) and `tests/e2e_codex_helper_alarm.rs` (`095`).

## Setup: no credential, no network

Every recording here used the nollm setup (`spike/nollm/` on `agent/dispatch-nollm`): a fresh `CODEX_HOME` with a dummy API key (`auth.json`, `auth_mode = "apikey"`), a `model_providers` entry pointing at a canned Responses server on 127.0.0.1, and the work directory trusted. The recorder (`tests/support/codex_editor_measure.py`, and `tests/support/codex_helper_alarm_measure.py` for M59) runs inside `bwrap --unshare-net` with `/home`, `/root`, `/tmp` and `/run/user` replaced by empty tmpfs mounts and an environment of `COLORTERM=truecolor`, `HOME`, `LANG=C.UTF-8`, `PATH`, `PWD` and `TERM=xterm-256color` only (each summary lists the names; the fast test refuses any other). No prompt is submitted, so no measurement here needs a model reply. No request reached the fake server either: it starts its request log (`model-requests.jsonl`) on its first request of any kind, and none of the seven raw captures behind the `.summary.json` files has one. That reading is from the uncommitted raw captures; no committed file records it. Every launch is 50 × 200. Codex is started as the deck's wrapper starts it: `--sandbox workspace-write --ask-for-approval never -c shell_environment_policy.set={}`.

The user's editor is the recorder itself run with `--editor`: it logs its argv, `VISUAL`/`EDITOR`, the exported file's path, mode and contents, then keeps or rewrites the file as the recorder planned and exits with a chosen status.

Every launch waits 4 s after its first ready composer before it is used, because a Ctrl+G sent just after start opens nothing (the `startup` reading below).

## Files

| File | Mode | Recorder (`recorder_sha256`) | What it holds |
| --- | --- | --- | --- |
| `editor.summary.json`, `editor-*.bin` | `editor` | `1bb22be7…` | Each Ctrl+G under `VISUAL` (five, then an editor exiting 37), an Escape on an empty composer and its control (three each), `EDITOR` alone, a set but empty `VISUAL` or `EDITOR`, and neither with `vi`/`nano`/`code` stand-ins on `PATH` (two each); the editor's argv, export rule, the composer and the `■` line afterwards, and the byte offsets the deck's marker reader replays over each launch's raw bytes |
| `startup.summary.json` | `startup` | `1bb22be7…` | Ten launches: Ctrl+G at the first ready composer, then again until the editor opens, with the time after ready of each press |
| `config-writes.summary.json` | `config-writes` | `9c1174ae…` | Seven launches on one `CODEX_HOME`, each read until 8 s after its first ready composer: whether Codex changed its own `config.toml` before or after that composer, when (its mtime against the composer), and the diff |
| `forms.summary.json` | `forms` | `1bb22be7…` | Twelve wire forms of Ctrl+G, whole and split, from a draft, the slash popup, reverse search and the `?` shortcuts overlay; ESC then BEL 0, 30 and 60 ms apart; a bracketed `PASTE` BEL `TAIL`, whole and split, twice each, with the next export's bytes |
| `keymap.summary.json` | `keymap` | `1bb22be7…` | M58r: nineteen key-binding source cases, each a fresh `CODEX_HOME` and launch, with every TOML file's contents (paths relative to the sandbox), the arguments, and whether Ctrl+G and F6 opened the editor or Codex refused to start |
| `e1f.summary.json`, `e1f-*.bin` | `e1f` | `2a39164e…` | The first E1F run: three arms of twenty presses (`LATE` at E1's `?2004h`, at the first `?2026h` after it, and while the editor runs), each with its marker offsets, where `LATE` was written and the next export |
| `e1f-rerun/` | `e1f` | `1bb22be7…` | The same, run again with the final recorder |
| `m59-armed-stall.json`, `m59-armed-timeout.json`, `m59-unarmed-long.json` | M59 | `49ccaf03…` (editor recorder `1bb22be7…`) | Per press: the helper's trace, its branch, how long it lived after it, its wait status when the zombie was seen, Codex's own `■` line, the user editor's starts and exits, and the composer afterwards |

The recorder revisions differ only in modes the other files don't use: `2a39164e…` lacks the `keymap` mode's answer to an untrusted folder's dialog (eight lines the `e1f` mode never reaches), and `9c1174ae…` adds the `config-writes` mode to `1bb22be7…`. Each `.summary.json` is written by `tests/support/codex_editor_summary.py` from its capture (the sandbox root written `ROOT`, screens cut to their `■` line); `python3 -I tests/support/codex_editor_summary.py --check <capture> <dir>` regenerates one and compares it byte for byte, and `--scan <dir>` runs the head-echo export's deny scan, which the fast tier runs over both directories. The M59 files are the lane-2 test's `result.json` with each press's screen dropped. The raw captures stay under `/var/tmp/dad-cxm-*` and `/var/tmp/dad-codex-m59-*` on the recording machine.

## Box load

Not recorded in these files or in the raw captures. The recording unit's own notes put CPU pressure (`/proc/pressure/cpu`, `some avg10`) at or under 0.8 % at each run's start and end, apart from `config-writes` (about 46 % at its start, from another unit's build), the 1-minute load at 1.7 to 2.1 on 16 CPUs where it was sampled, and the M59 harness's `QUIET` at every minute; and another unit (`cxl2`) ran real Codex sessions on the box during the E1F rerun, the keymap run and M59. Read those as the recorder's report, not as evidence this directory backs.

## Readings

**Export (`ExportShape::CodexEditorDir`).** 5/5 presses under `VISUAL` ran the editor with `$CODEX_HOME/editor/.tmpXXXXXX.md` (six alphanumerics) as its last argument, holding the draft, and Codex reimported the rewritten file. Every export was 0600 (`file_mode`, all fourteen invocations that ran the editor). Codex created `editor/` 0775 in this sandbox (umask 002), as M56b recorded; the deck's option P creates it 0700 before a direct Codex starts, and the M59 runs pre-created it 0700 for that reason.

**Selection (`EditorSelection::Codex`).** `VISUAL` wins over `EDITOR`, split by shell words (a quoted `'visual tag'` reached the editor as one argument, 5/5). `EDITOR` runs when `VISUAL` is unset (2/2). A set but empty `VISUAL` (with `EDITOR` set) or a set but empty `EDITOR` opens nothing and draws `■ Failed to open editor: editor command is empty` (2/2 each): an empty one is an error, not a fall-through. With neither set, and `vi`, `nano` and `code` stand-ins first on `PATH`, nothing opens and Codex draws `■ Cannot open external editor: set $VISUAL or $EDITOR before starting Codex.` (2/2): there is no `PATH` fallback, unlike Claude Code. An editor exiting 37 keeps the draft and draws `■ Failed to open editor: editor exited with status exit status: 37`.

**Paste toggles (`PasteToggles { disables: 2, enables: 1 }`).** Through the deck's `ReturnMarkers`, each of the fourteen invocations that ran the editor wrote two `?2004l` and then one `?2004h` (M43b's count, on both releases then), and `ReturnWatch::e1` reads Codex's return (E1F) after it; the six presses that opened no editor wrote no `?2004` at all. Claude Code writes one of each.

**Start-up.** In 10/10 launches the first Ctrl+G, 0.04–0.12 s after the first ready composer, opened nothing and drew nothing; the editor opened on a later press 0.73–1.42 s after ready (one launch missed again at 0.73 s; presses were about 0.7 s apart, so this samples the boundary, it does not bound it). An earlier, uncommitted run without the settle saw the same at 0.84–1.08 s after Codex's first output (9 of 10).

**Codex rewrites its own `config.toml` on its first four starts in a home.** On a fresh `CODEX_HOME`, launch 1 added `[tui] screen_reader_detection_done = true` and `[tui.model_availability_nux] "gpt-6.1-sol" = 1`, and launches 2–4 each raised that count by one; launches 5–7 changed nothing. In this recording no write came before the first ready composer: each landed 0.54–1.80 s after it (it ran with CPU pressure at about 46 % from another unit's build, so the times are loose). A later lane-2 recording, not committed, saw launch 1's file already changed by its first ready composer and changed again after it, and launches 2–4 change after it in both. The deck compares a launch's key-binding sources read at spawn with a later read (`codex_keymap`, `before != after` is `Hold::Changed`), so by that code each of those four generations would be held `Keymap(Changed)` for its life; no run here isolated it, because the A-20d-7 attempt was refused `Keymap(Changed)` on every probe for another reason as well (`wrap`'s `#2002` pair is on the bound agent's argv and not on the command the baseline is read from). The notice's key names a model from Codex's own catalog, so a release that adds one would presumably start the count again (not measured).

**Escape on an empty composer.** 3/3 presses after an Escape on an empty composer opened the editor, as did the 3/3 controls without it.

**Wire forms (M44e; `inert_candidates` stays empty).** From a draft, BEL and the CSI-u 103 implicit, press and repeat forms opened the editor, whole and split, and ESC then BEL opened it at 0, 30 and 60 ms; the CSI-u release events, the modifyOtherKeys forms and every codepoint-71 form did not, nor did ESC BEL in one write. In the `?` overlay the same forms opened it as from a draft. Nothing opened from the slash popup or reverse search. One form was unstable across runs: a split ESC BEL in the slash popup opened the editor in an earlier, uncommitted run (1/1) and not in the committed one (0/1).

**Pasted BEL (`pasted_bel_is_text: Absent`).** A bracketed paste of `PASTE` BEL `TAIL` opened no editor, and the next export held `BASEPASTETAIL` (hex `4241534550415354455441494c`), 4/4, whole and split: Codex removes the BEL. Read from the bytes the editor stand-in read, not the screen. M44e recorded the same. Claude Code keeps it as text.

**E1F premise (`inbox/probe/093`).** In both runs, `LATE` typed at the first `?2026h` after E1's `?2004h`, where the deck reads E1F, was in the next export 20/20 (40/40 over the two committed runs), and `LATE` typed while the editor ran was lost 20/20 (M31, the control that shows a loss is visible). No `?2026h` fell between the first `?2004l` and the `?2004h` in any of the 120 presses. Arm one writes `LATE` once the recorder has read E1's `?2004h`, and in two of the first run's twenty trials (t01 and t13) the next `?2026h` had already arrived by then: they wrote `LATE` 152 and 2047 bytes after the `?2004h`, after the deck's E1F frame at +138, so they released at E1F rather than at E1 alone and are not counted. The other 18 wrote `LATE` 8–138 bytes after the `?2004h`, before any byte of that frame, and 2 of them lost it (t07 and t20, both written 8 bytes after). In the rerun all 20 wrote it before the frame, 8–138 bytes after the `?2004h`, and none lost it. That is RV U2-01's E1-before-flush counterexample, on the real binary, at the natural race's rate. No seccomp gate stopped Codex inside its `tcflush` (M-PF1's `tcflush_gate` was not built), so that rate is sampled, not bounded. Across all of arm one's trials the first run wrote `LATE` 8–2047 bytes after the `?2004h`; arm two wrote it 14–1929 bytes after the `?2026h` it acted on.

**M58r (`inbox/probe/094`; `keymap_sources`: config, profile, project, `-c`).** Codex's own behaviour, case by case: a model-only profile file keeps Ctrl+G; a profile file, a trusted project or `config.toml` binding `open_external_editor = "f6"` moves it to F6; `-c tui.keymap.global.open_external_editor="ctrl-g"` restores it; an unparsable profile file and the three legacy `[profiles.measure]` shapes refuse to start (M58's eight, unchanged). Precedence: the profile file over `config.toml`, the trusted project over the profile file and over `config.toml`, and `-c` over each. A symlinked profile file is followed. A project the config marks `trust_level = "untrusted"` draws an `Open restricted` dialog after the first composer frame; opened restricted, its F6 binding is ignored. The deck's resolver, over the same files: it never reads Ctrl+G as the editor where Codex moved it; its merge in the proposed order gives Codex's result in all fourteen cases that started in a trusted folder, and differs in the untrusted one (the merge reads the project file Codex ignores); and `codex_projects_guard` holds `Unpinned` in all seven cases with a project file carrying a keymap, that one included. Where Codex refused to start, the merge reads the legacy table as no keymap, which matters to nothing: no generation exists.

**M59 on Codex (`inbox/probe/095`).** Armed, the helper paused at its first ready poll: 10/10 presses took the armed branch as Codex's own child, ran no editor, and ended 5.002–5.007 s after the branch with the trace at `pause ready`; Codex drew `■ Failed to open editor: editor exited with status signal: 14 (SIGALRM)` after the helper's end 10/10 (the zombie's status was seen 3/10), and the draft stayed. Armed, the exchange unanswered: 10/10 ended 3.515–3.520 s after the branch in an orderly armed exit with the notice on the terminal and status 1, row T of the helper's return table on Codex (Claude's twin exits 0); Codex drew `■ Failed to open editor: editor exited with status exit status: 1` and kept the draft. Unarmed: 11/11 ran the user's editor once and lived 10.11–10.27 s; Codex reimported the sentinel after each 0 and kept the draft after the 37, with its exit-37 line.

## The proposed `editor_probe`

`PROPOSED_CODEX_0164_EDITOR_PROBE` in `tests/support/codex_editor_evidence.rs`, not wired into `CODEX_FACTS` or the 0.164.0 record:

```rust
EditorProbe {
    export: ExportShape::CodexEditorDir,
    selection: EditorSelection::Codex,
    paste_toggles: PasteToggles { disables: 2, enables: 1 },
    keymap_sources: &[
        KeymapSource::CodexConfig,
        KeymapSource::CodexProfile,
        KeymapSource::CodexProject,
        KeymapSource::CodexOverride,
    ],
    keymap_loaded_at_start: Measurement::Absent,
    inert_candidates: &[],
    pasted_bel_is_text: Measurement::Absent,
}
```

## What this evidence does not show

- Any release but 0.164.0, the npm launcher (the native executable ran directly), or the 24 × 80 geometry.
- A stop inside Codex's `tcflush` (M-PF1), so the E1-only loss rate is not a bound.
- A real terminal editor, or a `VISUAL` with shell syntax beyond one quoted word.
- A project root found by markers, a project file in an ancestor directory, or profile selection through `config.toml`'s `profile =`.
- `keymap_loaded_at_start`, which stays `Absent` for Codex by design.
- A Codex generation the deck can check end to end. Neither measured form gives a checkable generation (plain command: bound, held `Keymap(Changed)`; the pair typed into the command: `NotResolved`, `Inert(Unbound)`); the form the deck writes when `VISUAL` or `EDITOR` is set was not measured.
