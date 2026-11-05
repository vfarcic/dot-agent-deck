//! PRD #1666 part 3, Codex Go: Codex 0.164.0's `head_echo` evidence, the
//! committed lane-2 captures under `tests/fixtures/codex-inbox/release-0.164.0/`
//! read back the way the deck reads its pane (`tests/support/codex_head_echo.rs`).
#[path = "support/codex_head_echo.rs"]
mod head_echo;

use dot_agent_deck::doorbell_activation::{
    KeyEffect, Measurement, RELEASE_RECORDS, first_stage_bytes,
};
use dot_agent_deck::doorbell_screen::TOKEN_HEAD_CHARS;
use dot_agent_deck::doorbell_txn::{ECHO_WAIT, TOKEN_ALPHABET};
use dot_agent_deck::event::AgentType;
use dot_agent_deck::release_scanner::{HEADER_RECOGNISERS, ReleaseScanner, ScanLatch};
use head_echo::{Capture, spread};
use semver::Version;
use spec::spec;

const DIR: &str = "codex-inbox/release-0.164.0";
/// The pinned native executable's SHA-256 (`README.md` beside the captures).
const PIN: &str = "b93f6811d3f653f0fb83be4050d857fb9abbb111dfa2ce160563327daff7f306";

/// How a capture's Codex had the deck's hooks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hooks {
    /// Installed and trusted, as a deck pane's wrapper leaves them.
    Trusted,
    /// Installed and trusted, every one switched off in Codex's records.
    Off,
    /// Installed in a Codex home with no trust records, Codex started through
    /// a launcher, and Codex's hooks review answered "Continue without
    /// trusting".
    Untrusted,
}

/// The staged captures: (stem, rows, cols, hooks).
const STAGED: &[(&str, u16, u16, Hooks)] = &[
    ("staged-large-1", 50, 200, Hooks::Trusted),
    ("staged-large-2", 50, 200, Hooks::Trusted),
    ("staged-large-3", 50, 200, Hooks::Trusted),
    ("staged-large-4", 50, 200, Hooks::Trusted),
    ("staged-small-1", 24, 80, Hooks::Trusted),
    ("staged-small-2", 24, 80, Hooks::Trusted),
    ("staged-hooks-off-1", 50, 200, Hooks::Off),
    ("staged-hooks-off-2", 50, 200, Hooks::Off),
    ("staged-untrusted-1", 50, 200, Hooks::Untrusted),
    ("staged-untrusted-2", 50, 200, Hooks::Untrusted),
];

fn capture(stem: &str) -> Capture {
    Capture::committed(&format!("{DIR}/codex-0.164.0-head-echo-{stem}"))
}

/// The release the capture's start-up header latches, read at the PTY
/// reader's recorded split.
fn latched(capture: &Capture) -> Version {
    let mut scanner = ReleaseScanner::new(
        capture.index["rows"].as_u64().unwrap() as u16,
        capture.index["cols"].as_u64().unwrap() as u16,
        HEADER_RECOGNISERS,
    )
    .unwrap();
    let mut pos = 0;
    for len in capture.index["read_lens"].as_array().unwrap() {
        let len = len.as_u64().unwrap() as usize;
        scanner.feed(&capture.bytes[pos..pos + len]);
        pos += len;
    }
    match scanner.latch() {
        ScanLatch::Latched {
            agent: AgentType::Codex,
            release,
            ..
        } => release.clone(),
        other => panic!("{}: {other:?}", capture.name),
    }
}

/// Each capture was taken on the pinned executable, through the deck's own
/// wrapper, with the hooks in the state its name says.
fn premise(capture: &Capture, hooks: Hooks) {
    let r = &capture.result;
    let who = &capture.name;
    assert_eq!(r["release"], "0.164.0", "{who}");
    assert_eq!(r["sha256"], PIN, "{who}");
    assert!(r["error"].is_null(), "{who}: {}", r["error"]);
    let argv: Vec<&str> = r["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(&argv[1..5], ["wrap", "--agent", "codex", "--"], "{who}");
    assert_eq!(latched(capture), Version::new(0, 164, 0), "{who}");
    let state = r["hooks"].as_object().unwrap();
    if hooks == Hooks::Untrusted {
        assert_eq!(
            &argv[5..9],
            ["sh", "-c", "exec codex \"$@\"", "sh"],
            "{who}"
        );
        assert_eq!(r["codex_home"], ".codex-untrusted", "{who}");
        assert!(state.is_empty(), "{who}: {state:?}");
        assert!(
            capture.index["points"].get("mark:hooks-answer").is_some(),
            "{who}: Codex's hooks review answered"
        );
        return;
    }
    assert_eq!(argv[5], "codex", "{who}");
    assert_eq!(r["codex_home"], ".codex", "{who}");
    for event in ["session_start", "user_prompt_submit", "stop"] {
        let entry = state
            .iter()
            .find(|(key, _)| key.contains(&format!(":{event}:")))
            .unwrap_or_else(|| panic!("{who}: no {event} hook"))
            .1;
        assert_eq!(entry["trusted"], true, "{who} {event}");
        assert_eq!(entry["enabled"], hooks == Hooks::Trusted, "{who} {event}");
    }
}

/// Scenario: Replay every committed staged capture of real Codex 0.164.0 (twenty rings at 50 × 200 and ten at 24 × 80 with the deck's hooks trusted, ten at 50 × 200 with them switched off, ten at 50 × 200 through a launcher with them untrusted) through the deck's evidence state and Codex's screen profile. Each was taken on the pinned executable under the deck's wrapper and latches 0.164.0; on every ring the composer was the empty ready one at admission, showed only the placeholder or a prefix of the head, never a dialog, until it held exactly the head within the echo wait, then only prefixes of the line until it held exactly the whole line, exactly so just before the Enter sent at least 150 ms after the rest, and then showed the line as a transcript user message within the confirm wait, with Codex's own record holding exactly that prompt once. Every ring at 50 × 200 was read on Clean frames of the deck's evidence state, and at 24 × 80 every ring but each session's first: a fresh Codex at that size draws no width witness, so no frame from its start to the first ring's Enter is Clean.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_codex_164_head_echo_staged_captures() {
    let mut head = Vec::new();
    let mut line = Vec::new();
    let mut submit = Vec::new();
    let mut groups: [Vec<head_echo::Trial>; 4] = Default::default();
    let mut counts = [0usize; 4];
    let mut clean = 0;
    for &(stem, rows, cols, hooks) in STAGED {
        let capture = capture(stem);
        assert_eq!(
            (
                capture.index["rows"].as_u64(),
                capture.index["cols"].as_u64()
            ),
            (Some(u64::from(rows)), Some(u64::from(cols))),
            "{stem}"
        );
        premise(&capture, hooks);
        let trials = head_echo::staged_trials(&capture);
        assert_eq!(trials.len(), 5, "{stem}");
        let group = match (cols, hooks) {
            (_, Hooks::Off) => 2,
            (80, Hooks::Trusted) => 1,
            (_, Hooks::Trusted) => 0,
            (_, Hooks::Untrusted) => 3,
        };
        counts[group] += trials.len();
        groups[group].extend(trials.iter().cloned());
        // At 50 × 200 every ring is read on Clean frames; a fresh Codex at
        // 24 × 80 draws no width witness before its first turn.
        if cols == 200 {
            assert!(trials.iter().all(|t| t.clean), "{stem}: {trials:?}");
        } else {
            assert!(
                !trials[0].clean && trials[1..].iter().all(|t| t.clean),
                "{stem}: {trials:?}"
            );
            // Not one Clean frame from Codex's start, through its settle
            // window and the first ring, to that ring's Enter: a doorbell
            // that waits for a Clean frame never admits a fresh Codex at
            // 24 × 80 until something else has prompted it. In these two
            // captures the first Clean frame came as the first turn ended
            // (31.4 s and 6.4 s after its Enter; the turns ran 30.9 s and
            // 6.3 s).
            let reads = head_echo::replay(&capture, |_| Vec::new());
            let first_cr = capture.point("mark:cr-1");
            assert!(
                reads.iter().filter(|r| r.end <= first_cr).all(|r| !r.clean),
                "{stem}: a Clean frame before the first Enter"
            );
            let first_clean = reads.iter().find(|r| r.clean).expect("a Clean frame");
            let cr_sent = capture.result["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["label"] == "cr-1")
                .unwrap()["t"]
                .as_u64()
                .unwrap();
            eprintln!(
                "{stem}: first Clean frame {:.1} s after the first Enter",
                (first_clean.t - cr_sent) as f64 / 1e9
            );
        }
        clean += trials.iter().filter(|t| t.clean).count();
        for t in &trials {
            head.push(t.head_echo_ms);
            line.push(t.line_echo_ms);
            submit.push(t.transcript_ms);
        }
    }
    assert_eq!(counts, [20, 10, 10, 10]);
    for (name, trials) in ["trusted 50x200", "trusted 24x80", "hooks off", "untrusted"]
        .iter()
        .zip(&groups)
    {
        let of =
            |f: fn(&head_echo::Trial) -> f64| spread(&trials.iter().map(f).collect::<Vec<_>>());
        eprintln!(
            "{name}: head {:?} line {:?} floor {:?} transcript {:?}; first submits: transcript {:?}",
            of(|t| t.head_echo_ms),
            of(|t| t.line_echo_ms),
            of(|t| t.floor_ms),
            of(|t| t.transcript_ms),
            spread(
                &trials
                    .iter()
                    .filter(|t| t.first_submit)
                    .map(|t| t.transcript_ms)
                    .collect::<Vec<_>>()
            ),
        );
    }
    assert_eq!(clean, 48);
    let (_, _, max) = spread(&head);
    assert!(max < ECHO_WAIT.as_secs_f64() * 1e3);
    eprintln!(
        "head echo {:?} ms, line echo {:?} ms, transcript {:?} ms over {} rings",
        spread(&head),
        spread(&line),
        spread(&submit),
        head.len()
    );
}

/// Scenario: Replay the committed controls capture of real Codex 0.164.0 at 50 × 200. Six heads typed alone are each held exactly, the strict withdrawal recogniser counts eight deletes for each, and the placeholder returns after them; one to seven characters of a head are held exactly and refused as no whole token; a head and then none, one, ten or forty characters of the rest are held exactly and counted whole; another token's line and an earlier attempt's line are held as typed and never read as empty or as another head's; a head and one byte of the rest (a space) draws as the head alone and is refused; no write opened a dialog or submitted anything; three whole lines with the Enter in the same burst were each kept in the composer, refused for withdrawal and absent from Codex's own record; and six staged lines whose Enter was written once the line had echoed, 81.7 to 120.2 ms after the rest, were each submitted and are in Codex's record once, with nothing else doorbell-shaped there.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_codex_164_head_echo_controls_capture() {
    let capture = capture("controls");
    premise(&capture, Hooks::Trusted);
    let controls = head_echo::checked_controls(&capture);
    // The margin the committed capture supports: the six Enters after the
    // line's echo went 81.7 to 120.2 ms after the rest's write returned (the
    // ones asked for at 50 ms waited for the echo), each submitted.
    let enter: Vec<f64> = controls.iter().filter_map(|c| c.enter_ms).collect();
    assert_eq!(enter.len(), 6);
    let (min, _, max) = spread(&enter);
    assert!((81.0..82.0).contains(&min), "{enter:?}");
    assert!(max < 121.0, "{enter:?}");
    for c in &controls {
        eprintln!("{c:?}");
    }
}

/// Scenario: Over every Codex record the deck carries, check each measured dialog's key table: both parentheses and every token letter are classified there and classified inert, and every token head typed in the committed captures is made only of those bytes. A future table that makes a token byte commit, move or dismiss, or leaves one unmeasured, fails here.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_codex_token_head_bytes_are_inert_in_every_dialog() {
    let first: Vec<u8> = first_stage_bytes().collect();
    assert_eq!(first.len(), 2 + TOKEN_ALPHABET.len());
    let mut checked = 0;
    for record in RELEASE_RECORDS
        .iter()
        .filter(|r| r.agent == AgentType::Codex)
    {
        let Measurement::Verified(dialogs) = record.key_effects else {
            panic!("{}: key effects not verified", record.release);
        };
        for dialog in dialogs.iter() {
            for &byte in &first {
                let effect = dialog
                    .keys
                    .iter()
                    .find(|(k, _)| *k == byte)
                    .map(|(_, e)| *e);
                assert_eq!(
                    effect,
                    Some(KeyEffect::Inert),
                    "Codex {} {}: {:?}",
                    record.release,
                    dialog.dialog,
                    char::from(byte)
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0);
    for &(stem, ..) in STAGED.iter().chain(&[("controls", 0, 0, Hooks::Trusted)]) {
        for line in capture(stem).lines() {
            let head: Vec<u8> = line.bytes().take(TOKEN_HEAD_CHARS).collect();
            assert!(head.iter().all(|b| first.contains(b)), "{stem}: {line}");
        }
    }
}

/// `python3 -I <script> <args>` from the checkout, with a cleared
/// environment apart from `PATH`; panics with its output unless it exits 0.
#[cfg(unix)]
fn python_ok(script: &str, args: &[String]) {
    let root = env!("CARGO_MANIFEST_DIR");
    let output = std::process::Command::new("python3")
        .arg("-I")
        .arg(format!("{root}/tests/support/{script}"))
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("the export checks require python3 on PATH");
    assert!(
        output.status.success(),
        "{script} {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Scenario: Run the unit checks of the head-echo export's deny scan, with no agent and no network. Text the committed captures draw passes; an owner or group column naming anyone but the allow-listed owner handle, an `id` listing, a `/home/` path, key, token, JWT, `org-`/`user-` id, secret-named field and email address are each refused, also with styling inside the word; with an empty allow-list the committed 24 × 80 capture's drawn `stat` rows are refused; and an export carrying one writes nothing.
#[cfg(unix)]
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_codex_head_echo_export_deny_scan_checks() {
    python_ok("codex_head_echo_export_checks.py", &[]);
}

/// Scenario: Run the head-echo export's deny scan (`--scan`) over every committed Codex 0.164.0 head-echo capture, the eleven `.bin`, `.json` and `.result.json` triples. None may contain a username other than the allow-listed owner handle in an owner, `id` or `USER=` position, a `/home/` path, or a key, token, JWT, `org-`/`user-` id, secret-named field or email address, so a re-recording that would commit one fails here.
#[cfg(unix)]
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_codex_head_echo_committed_captures_pass_the_deny_scan() {
    let stems: Vec<String> = STAGED
        .iter()
        .map(|&(stem, ..)| stem)
        .chain(["controls"])
        .map(|stem| format!("tests/fixtures/{DIR}/codex-0.164.0-head-echo-{stem}"))
        .collect();
    assert_eq!(stems.len(), 11);
    // Every committed capture in the directory is one of these.
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{DIR}"));
    let on_disk = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".bin")
        })
        .count();
    assert_eq!(on_disk, stems.len(), "{}", dir.display());
    let mut args = vec!["--scan".to_string()];
    args.extend(stems);
    python_ok("codex_head_echo_export.py", &args);
}
