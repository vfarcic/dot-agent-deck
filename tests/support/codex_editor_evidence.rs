//! PRD #1666 part 3, U4: the readings Codex 0.164.0's `editor_probe` facts,
//! M58r and the E1F premise rest on, checked over a directory of
//! `tests/support/codex_editor_summary.py` output: the committed evidence
//! (`tests/codex_editor_evidence.rs`) or a fresh recording
//! (`tests/e2e_codex_editor_measure.rs`). Every marker reading is the deck's
//! own (`composer_probe::segments::ReturnMarkers` and `ReturnWatch` over the
//! raw PTY bytes), and every key-binding verdict is the deck's own resolver
//! (`composer_probe::keymap`) over the files the recording launched Codex
//! with. Each check panics with what it found when a reading is not the one
//! the proposed facts need.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use dot_agent_deck::composer_probe::keymap::{
    CodexBaseline, Hold, codex_keymap, codex_projects_guard,
};
use dot_agent_deck::composer_probe::segments::{Agent, Marker, ReturnMarkers, ReturnWatch, Seen};
use dot_agent_deck::doorbell_activation::{
    EditorProbe, EditorSelection, ExportShape, KeymapSource, Measurement, PasteToggles,
};
use serde_json::Value;

/// The `editor_probe` Codex 0.164.0's evidence supports (U4's proposal).
/// It is not wired into `CODEX_FACTS` or the 0.164.0 record: the activation
/// unit (U7) does that, and `conformance` then compares the two.
///
/// - `export`: `$CODEX_HOME/editor/.tmpXXXXXX.md`, the editor's last
///   argument, holding the draft; the rewritten file is reimported.
/// - `selection`: a set, non-empty `VISUAL` (split by shell words) before
///   `EDITOR`; a set but empty one is an error, not a fall-through; with
///   neither, an error line and no `PATH` fallback.
/// - `paste_toggles`: two `?2004l` and one `?2004h` per invocation.
/// - `keymap_sources`: `config.toml`, then the profile file, then the
///   trusted project's `.codex/config.toml`, then each `-c`, lowest first.
/// - `keymap_loaded_at_start`: `Absent`, as for every Codex record (no P-KB1
///   premise).
/// - `inert_candidates`: none, the M44e safe default.
/// - `pasted_bel_is_text`: `Absent`: Codex removes a pasted BEL.
pub const PROPOSED_CODEX_0164_EDITOR_PROBE: EditorProbe<'static> = EditorProbe {
    export: ExportShape::CodexEditorDir,
    selection: EditorSelection::Codex,
    paste_toggles: PasteToggles {
        disables: 2,
        enables: 1,
    },
    keymap_sources: &[
        KeymapSource::CodexConfig,
        KeymapSource::CodexProfile,
        KeymapSource::CodexProject,
        KeymapSource::CodexOverride,
    ],
    keymap_loaded_at_start: Measurement::Absent,
    inert_candidates: &[],
    pasted_bel_is_text: Measurement::Absent,
};

pub const RELEASE: &str = "0.164.0";
pub const SHA256: &str = "b93f6811d3f653f0fb83be4050d857fb9abbb111dfa2ce160563327daff7f306";

pub fn summary(dir: &Path, mode: &str) -> Value {
    let path = dir.join(format!("{mode}.summary.json"));
    let value: Value = serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap();
    assert_eq!(value["release"], RELEASE, "{}", path.display());
    assert_eq!(value["sha256"], SHA256, "{}", path.display());
    assert!(
        value["error"].is_null(),
        "{mode}: the recording failed: {}",
        value["error"]
    );
    assert_eq!(value["term"], "xterm-256color");
    assert_eq!(value["colorterm"], "truecolor");
    // Credential-free: nothing but the sandbox's own names reached Codex.
    for name in value["env_names"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        assert!(
            ["COLORTERM", "HOME", "LANG", "PATH", "PWD", "TERM"].contains(&name),
            "{mode}: {name} reached the sandbox"
        );
    }
    value
}

pub fn rows<'a>(summary: &'a Value, field: &str, value: &str) -> Vec<&'a Value> {
    summary["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row[field] == value)
        .collect()
}

/// The deck's own reading of a launch's raw bytes: every return marker.
pub fn seen(dir: &Path, mode: &str, launch: &str) -> Vec<Seen> {
    let bytes = std::fs::read(dir.join(format!("{mode}-{launch}.bin"))).unwrap();
    let mut markers = ReturnMarkers::default();
    let mut out = Vec::new();
    markers.feed(&bytes, &mut out);
    out
}

fn off(row: &Value, key: &str) -> u64 {
    row[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} missing: {row}"))
}

/// The markers in `[from, to)`.
fn window(seen: &[Seen], from: u64, to: u64) -> Vec<Seen> {
    seen.iter()
        .copied()
        .filter(|s| s.start >= from && s.start < to)
        .collect()
}

/// Where the deck's E1F for a trigger written at `from` completes: the
/// `?2026h` whose feeding first makes `ReturnWatch::e1(from)` read Codex as
/// returned, and the `?2004h` that made it enabled.
pub fn e1f(seen: &[Seen], from: u64) -> Option<(u64, u64)> {
    let mut watch = ReturnWatch::e1(from);
    let mut enable = None;
    for s in seen {
        let was = watch.enabled();
        watch.feed(*s);
        if !was && watch.enabled() {
            enable = Some(s.start);
        }
        if watch.returned(Agent::Codex) {
            return Some((enable?, s.start));
        }
    }
    None
}

fn error_line(row: &Value) -> Option<&str> {
    row["error_line"].as_str()
}

/// Export shape, argument rule and selection, with the error lines.
pub fn check_export_and_selection(dir: &Path) {
    let s = summary(dir, "editor");
    let probe = PROPOSED_CODEX_0164_EDITOR_PROBE;
    assert_eq!(probe.export, ExportShape::CodexEditorDir);
    assert_eq!(probe.selection, EditorSelection::Codex);
    let visual = rows(&s, "cell", "visual");
    assert_eq!(visual.len(), 5);
    for row in &visual {
        assert_eq!(row["opened"], true, "{row}");
        let rule = &row["export_rule"];
        assert_eq!(rule["last_argument"], true, "{row}");
        assert_eq!(rule["in_codex_editor_dir"], true, "{row}");
        assert_eq!(rule["name_shape"], true, "{row}");
        // The quoted word stayed one argument: shell words, not spaces.
        assert_eq!(row["editor_ran"], "visual tag", "{row}");
        assert_eq!(row["export_had_draft"], row["draft"], "{row}");
        let edited = row["draft"].as_str().unwrap().replace("DRAFT", "EDITED");
        assert_eq!(row["composer_after"], edited.as_str(), "reimported: {row}");
    }
    let failed = rows(&s, "cell", "visual-rc37");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["opened"], true);
    assert_eq!(
        failed[0]["composer_after"], "CXM_DRAFT_RC",
        "a failing editor keeps the draft"
    );
    assert_eq!(
        error_line(failed[0]),
        Some("■ Failed to open editor: editor exited with status exit status: 37")
    );
    for row in rows(&s, "cell", "editor-only") {
        assert_eq!(row["opened"], true, "{row}");
        assert_eq!(
            row["editor_ran"], "editor",
            "EDITOR when VISUAL is unset: {row}"
        );
    }
    for cell in ["visual-empty-editor-set", "editor-empty"] {
        let cell_rows = rows(&s, "cell", cell);
        assert_eq!(cell_rows.len(), 2, "{cell}");
        for row in cell_rows {
            assert_eq!(
                row["opened"], false,
                "a set but empty one is an error: {row}"
            );
            assert_eq!(
                error_line(row),
                Some("■ Failed to open editor: editor command is empty"),
                "{row}"
            );
        }
    }
    let neither = rows(&s, "cell", "neither-path-stubs");
    assert_eq!(neither.len(), 2);
    for row in neither {
        assert_eq!(row["path_stubs"], true);
        assert_eq!(
            row["opened"], false,
            "no PATH fallback (vi, nano, code stubs present): {row}"
        );
        assert_eq!(
            error_line(row),
            Some("■ Cannot open external editor: set $VISUAL or $EDITOR before starting Codex."),
            "{row}"
        );
    }
    // An Escape on an empty composer changes nothing for the next Ctrl+G.
    for cell in ["esc-empty", "esc-empty-control"] {
        let cell_rows = rows(&s, "cell", cell);
        assert_eq!(cell_rows.len(), 3, "{cell}");
        assert!(cell_rows.iter().all(|r| r["opened"] == true), "{cell}");
    }
}

/// The `?2004` writes per invocation, by the deck's marker reader: two
/// disables then one enable around every editor run, and none where no
/// editor ran. Returns the per-invocation counts.
pub fn check_paste_toggles(dir: &Path) -> Vec<(String, usize, usize)> {
    let s = summary(dir, "editor");
    let probe = PROPOSED_CODEX_0164_EDITOR_PROBE;
    let mut counts = Vec::new();
    for row in s["rows"].as_array().unwrap() {
        let launch = row["launch"].as_str().unwrap();
        let all = seen(dir, "editor", launch);
        let w = window(&all, off(row, "sent_off"), off(row, "done_off"));
        let disables = w.iter().filter(|m| m.marker == Marker::PasteOff).count();
        let enables = w.iter().filter(|m| m.marker == Marker::PasteOn).count();
        let label = row["label"].as_str().unwrap().to_string();
        if row["opened"] == true {
            assert_eq!(
                (disables, enables),
                (
                    probe.paste_toggles.disables as usize,
                    probe.paste_toggles.enables as usize
                ),
                "{label}"
            );
            let last_off = w
                .iter()
                .filter(|m| m.marker == Marker::PasteOff)
                .map(|m| m.start)
                .max();
            let on = w
                .iter()
                .find(|m| m.marker == Marker::PasteOn)
                .unwrap()
                .start;
            assert!(
                last_off.unwrap() < on,
                "{label}: the enable follows both disables"
            );
            assert!(
                e1f(&all, off(row, "sent_off"))
                    .is_some_and(|(_, frame)| frame < off(row, "done_off")),
                "{label}: the deck reads E1F after the editor"
            );
        } else {
            assert_eq!(
                (disables, enables),
                (0, 0),
                "{label}: no editor, no toggles"
            );
        }
        counts.push((label, disables, enables));
    }
    counts
}

/// When Ctrl+G starts to open the editor after the first ready composer.
/// Returns (largest delay after ready that opened nothing, smallest that
/// opened).
pub fn check_startup(dir: &Path) -> (f64, f64) {
    let s = summary(dir, "startup");
    let launches = s["rows"].as_array().unwrap();
    assert_eq!(launches.len(), 10);
    let (mut last_miss, mut first_open) = (0f64, f64::MAX);
    for launch in launches {
        let tries = launch["tries"].as_array().unwrap();
        let opened: Vec<_> = tries.iter().filter(|t| t["opened"] == true).collect();
        assert_eq!(
            opened.len(),
            1,
            "every launch's editor opened, once: {launch}"
        );
        assert_eq!(tries.last().unwrap()["opened"], true);
        assert!(
            opened[0]["sent_after_ready_s"].as_f64().unwrap() < 1.5,
            "opened within 1.5 s of ready: {launch}"
        );
        assert_eq!(
            tries[0]["opened"], false,
            "the first Ctrl+G at the first ready composer: {launch}"
        );
        for t in tries {
            let at = t["sent_after_ready_s"].as_f64().unwrap();
            if t["opened"] == true {
                first_open = first_open.min(at);
            } else {
                assert!(
                    t["error_line"].is_null(),
                    "an early Ctrl+G draws nothing: {t}"
                );
                last_miss = last_miss.max(at);
            }
        }
    }
    (last_miss, first_open)
}

/// What Codex 0.164.0 writes to its own `config.toml` as it starts, on one
/// `CODEX_HOME`: the first launch adds `[tui] screen_reader_detection_done`
/// and a model-availability notice count, the next three raise the count,
/// each after its first ready composer, and from the fifth nothing changes.
/// Returns each write's delay after the first ready composer, in ms.
pub fn check_config_writes(dir: &Path) -> Vec<f64> {
    let s = summary(dir, "config-writes");
    let launches = s["rows"].as_array().unwrap();
    assert_eq!(launches.len(), 7);
    let mut delays = Vec::new();
    for (i, row) in launches.iter().enumerate() {
        let n = i + 1;
        assert_eq!(row["launch"], n as u64);
        // The first launch's writes landed after its first ready composer in
        // one recording and partly before it in another; the later ones after.
        if n > 1 {
            assert_eq!(
                row["changed_by_ready"], false,
                "launch {n}: written before the first ready composer"
            );
        }
        let diff = row["diff"].as_str().unwrap();
        if n <= 4 {
            assert_eq!(row["changed_after_ready"], true, "launch {n}");
            assert!(
                diff.contains(&format!("+\"gpt-6.1-sol\" = {n}")),
                "launch {n}: {diff}"
            );
            assert_eq!(
                diff.contains("+screen_reader_detection_done = true"),
                n == 1,
                "launch {n}: {diff}"
            );
            delays.push(row["mtime_after_ready_ms"].as_f64().unwrap());
        } else {
            assert_eq!(row["changed_after_ready"], false, "launch {n}: {diff}");
            assert!(diff.is_empty(), "launch {n}: {diff}");
        }
    }
    delays
}

/// The E1F premise: LATE written at the first `?2026h` after E1's `?2004h`
/// (by the deck's reading) is kept in every trial; LATE written while the
/// editor runs is lost in every trial (the control that shows a loss is
/// visible); and between the first `?2004l` and the `?2004h` no `?2026h`.
/// Returns kept counts per arm, and the E1-only arm's genuine trials.
pub fn check_e1f(dir: &Path) -> E1f {
    let s = summary(dir, "e1f");
    let mut out = Vec::new();
    let (mut lost, mut e1_only, mut e1_n) = (0, 0, 0);
    for arm in ["at-2004h", "at-2026h", "during-editor"] {
        let trials = rows(&s, "arm", arm);
        assert_eq!(trials.len(), 20, "{arm}");
        let mut kept = 0;
        for row in &trials {
            assert_eq!(row["opened"], true, "{row}");
            let all = seen(dir, "e1f", row["launch"].as_str().unwrap());
            let from = off(row, "sent_off");
            let check = off(row, "check_off");
            let w = window(&all, from, check);
            let offs: Vec<_> = w.iter().filter(|m| m.marker == Marker::PasteOff).collect();
            assert_eq!(offs.len(), 2, "{row}");
            let on = w
                .iter()
                .find(|m| m.marker == Marker::PasteOn)
                .unwrap()
                .start;
            assert!(
                !w.iter()
                    .any(|m| m.marker == Marker::Frame && m.start > offs[0].start && m.start < on),
                "a ?2026h between the disable and the enable: {row}"
            );
            let (enable, frame) = e1f(&all, from).expect("the deck reads E1F");
            assert_eq!(enable, on);
            let late = row["late_off"]
                .as_u64()
                .unwrap_or_else(|| panic!("LATE not written: {row}"));
            match arm {
                "at-2004h" => {
                    assert_eq!(row["enable_off"].as_u64(), Some(enable), "{row}");
                    assert!(late >= enable + 8, "{row}");
                    // The recorder records `frame_off` only when the `?2026h`
                    // had already arrived when it wrote LATE: that trial
                    // released at E1F, not at E1 alone. Without it, LATE
                    // went out before any byte of the deck's E1F frame.
                    let before_frame = match row["frame_off"].as_u64() {
                        Some(f) => {
                            assert_eq!(f, frame, "{row}");
                            late < f
                        }
                        None => {
                            assert!(late <= frame, "frame unrecorded but read: {row}");
                            true
                        }
                    };
                    e1_n += 1;
                    if before_frame {
                        e1_only += 1;
                        lost += usize::from(row["kept"] == false);
                    }
                }
                "at-2026h" => {
                    assert_eq!(row["frame_off"].as_u64(), Some(frame), "{row}");
                    assert!(late >= frame + 8, "written after the frame was read: {row}");
                    assert_eq!(row["kept"], true, "E1F release kept: {row}");
                }
                _ => {
                    assert!(late < enable, "written while the editor ran: {row}");
                    assert_eq!(
                        row["kept"], false,
                        "typed during the editor is discarded: {row}"
                    );
                }
            }
            let export = row["next_export"].as_str().unwrap();
            assert_eq!(row["kept"] == true, export == "BASELATE", "{row}");
            assert!(export == "BASELATE" || export == "BASE", "{row}");
            kept += usize::from(row["kept"] == true);
        }
        out.push((arm.to_string(), kept, trials.len()));
    }
    E1f {
        arms: out,
        e1_only: (lost, e1_only, e1_n),
    }
}

/// What `check_e1f` counted.
#[derive(Debug)]
pub struct E1f {
    /// `(arm, kept, n)` for each arm, every trial counted.
    pub arms: Vec<(String, usize, usize)>,
    /// The E1-only arm (`at-2004h`) as `(lost, e1_only, n)`: `e1_only` counts
    /// the trials whose LATE was written before the deck's E1F `?2026h`, the
    /// only ones that release at E1 alone, and `lost` those of them whose
    /// LATE was not in the next export.
    pub e1_only: (usize, usize, usize),
}

/// How Codex and the deck classify Ctrl+G for one launch's sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Ctrl+G opens the external editor.
    Opens,
    /// Ctrl+G opens nothing (the action moved): Codex; `Rebound`: the deck.
    Moved,
    /// Codex refused to start: no generation exists to resolve.
    NoGeneration,
    /// The deck holds for another reason.
    Held(Hold),
}

/// One M58r case: Codex's own outcome, the deck's resolver merge alone, and
/// the deck's full verdict (the project guard, then the merge).
#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    pub codex: Outcome,
    pub merge: Outcome,
    pub deck: Outcome,
}

fn deck_outcome(verdict: Result<(), Hold>) -> Outcome {
    match verdict {
        Ok(()) => Outcome::Opens,
        Err(Hold::Rebound) => Outcome::Moved,
        Err(hold) => Outcome::Held(hold),
    }
}

/// M58r: rebuild each case's files under `scratch`, read them as the deck
/// does at spawn, and compare its verdict with what Codex did.
pub fn keymap_cases(dir: &Path, scratch: &Path) -> Vec<Case> {
    let s = summary(dir, "keymap");
    let mut out = Vec::new();
    for row in s["rows"].as_array().unwrap() {
        let name = row["case"].as_str().unwrap().to_string();
        let root = scratch.join(&name);
        for (rel, file) in row["files"].as_object().unwrap() {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            match file["symlink"].as_str() {
                Some(to) => std::os::unix::fs::symlink(root.join(to), &path).unwrap(),
                None => std::fs::write(&path, file["text"].as_str().unwrap()).unwrap(),
            }
        }
        // A symlink's target is listed as a file of its own too, under the
        // target's path, so it exists once every entry is written.
        let codex_home = root.join("home/.codex");
        let cwd = root.join(row["cwd"].as_str().unwrap());
        std::fs::create_dir_all(&cwd).unwrap();
        let args: Vec<OsString> = row["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| OsString::from(a.as_str().unwrap()))
            .collect();
        let baseline = CodexBaseline::at_spawn(&codex_home, &args, &cwd);
        let precedence = PROPOSED_CODEX_0164_EDITOR_PROBE.keymap_sources;
        let (merge, deck) = match &baseline {
            Ok(b) => (
                deck_outcome(codex_keymap(&b.read, &b.read, precedence)),
                deck_outcome(
                    codex_projects_guard(&b.read)
                        .and_then(|()| codex_keymap(&b.read, &b.read, precedence)),
                ),
            ),
            Err(hold) => (Outcome::Held(*hold), Outcome::Held(*hold)),
        };
        let codex = if row["started"] != true {
            Outcome::NoGeneration
        } else if row["ctrl_g_opened"] == true {
            Outcome::Opens
        } else {
            assert_eq!(
                row["f6_opened"], true,
                "{name}: neither Ctrl+G nor F6 opened: {row}"
            );
            Outcome::Moved
        };
        out.push(Case {
            name,
            codex,
            merge,
            deck,
        });
    }
    out
}

/// M58r's verdicts: the deck never reads Ctrl+G as the editor where Codex
/// moved it (safety); the merge alone equals Codex wherever Codex started in
/// a trusted folder (the precedence), and differs in the untrusted one,
/// whose project file Codex ignores and the merge reads; and the deck holds
/// `Unpinned` in exactly the cases with a project file carrying a keymap
/// (`codex_projects_guard`, by design), which covers that one. Returns the
/// cases.
pub fn check_keymap(dir: &Path, scratch: &Path) -> Vec<Case> {
    let cases = keymap_cases(dir, scratch);
    assert_eq!(cases.len(), 19);
    let s = summary(dir, "keymap");
    for (case, row) in cases.iter().zip(s["rows"].as_array().unwrap()) {
        if case.codex == Outcome::NoGeneration {
            continue;
        }
        assert!(
            !(case.deck == Outcome::Opens && case.codex != Outcome::Opens),
            "{}: the deck would probe a Codex whose Ctrl+G moved: {case:?}",
            case.name
        );
        // An untrusted folder's project file is one Codex ignores and the
        // merge reads (it resolves no trust): only the guard holds there.
        if row["dialog"] == "open-restricted" {
            assert_ne!(
                case.merge, case.codex,
                "{}: the merge read nothing",
                case.name
            );
        } else {
            assert_eq!(
                case.merge, case.codex,
                "{}: the merge disagrees with Codex",
                case.name
            );
        }
        let project_keymap = row["files"].as_object().unwrap().iter().any(|(rel, f)| {
            rel.starts_with("work/") && f["text"].as_str().unwrap().contains("tui.keymap")
        });
        if project_keymap {
            assert_eq!(case.deck, Outcome::Held(Hold::Unpinned), "{case:?}");
        } else {
            assert_eq!(case.deck, case.codex, "{case:?}");
        }
    }
    // Codex refused to start in exactly M58's four cases.
    let refused: Vec<_> = cases
        .iter()
        .filter(|c| c.codex == Outcome::NoGeneration)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        refused,
        [
            "m58-5-unparsable-profile",
            "m58-6-legacy-table-model-only",
            "m58-7-legacy-table-binding",
            "m58-8-profile-file-and-legacy-table"
        ]
    );
    cases
}

/// M44e's forms and the pasted BEL. Returns the forms that opened the
/// editor, per state.
pub fn check_forms(dir: &Path) -> Vec<(String, Vec<String>)> {
    let s = summary(dir, "forms");
    let probe = PROPOSED_CODEX_0164_EDITOR_PROBE;
    assert!(probe.inert_candidates.is_empty(), "the M44e safe default");
    assert_eq!(probe.pasted_bel_is_text, Measurement::Absent);
    let forms = rows(&s, "cell", "forms");
    let mut by_state: Vec<(String, Vec<String>)> = Vec::new();
    for row in &forms {
        let state = row["state"].as_str().unwrap().to_string();
        let form = format!(
            "{}{}",
            row["form"].as_str().unwrap(),
            if row["split"] == true { "/split" } else { "" }
        );
        if !by_state.iter().any(|(s, _)| *s == state) {
            by_state.push((state.clone(), Vec::new()));
        }
        if row["opened"] == true {
            by_state
                .iter_mut()
                .find(|(s, _)| *s == state)
                .unwrap()
                .1
                .push(form);
        }
    }
    let composer = &by_state.iter().find(|(s, _)| s == "composer").unwrap().1;
    for form in ["bel", "bel/split", "u-103-implicit", "u-103-event1"] {
        assert!(
            composer.contains(&form.to_string()),
            "{form} opens from a draft: {composer:?}"
        );
    }
    let pasted = rows(&s, "cell", "paste-bel");
    assert_eq!(pasted.len(), 4);
    for row in pasted {
        assert_eq!(row["opened"], false, "a pasted BEL opens no editor: {row}");
        // The bytes the editor read: the BEL is gone, not kept as text.
        assert_eq!(
            row["next_export_hex"].as_str(),
            Some(hex(b"BASEPASTETAIL").as_str()),
            "{row}"
        );
    }
    by_state
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The fixture directory, from the crate root.
pub fn committed_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex-inbox/release-0.164.0/editor-probe")
}
