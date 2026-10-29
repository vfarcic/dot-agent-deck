//! `inbox/doorbell/053`: the activation table, rule by rule
//! (`src/doorbell_activation.rs`).
use std::time::Duration;

use dot_agent_deck::doorbell_activation::{
    Activation, DialogKeys, EditorProbe, EditorSelection, ExportShape, FirstModal, KeyEffect,
    KeymapSource, LatchedRelease, Measurement, NoGo, PasteToggles, RELEASE_RECORDS, ReleaseRecord,
    SETTLE_CAP, activation, exact_verdict, first_stage_bytes, parse_unrecorded_claude_releases,
    with_assumed_codex_head_echo, with_assumed_editor_probes, without_unrecorded_claude,
};
use dot_agent_deck::doorbell_generation::generation_activation;
use dot_agent_deck::event::AgentType;
use semver::Version;
use spec::spec;

const CODEX: AgentType = AgentType::Codex;
const CLAUDE: AgentType = AgentType::ClaudeCode;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// The keys a measured dialog classifies, all inert.
const ALL_INERT: &[(u8, KeyEffect)] = &[
    (b'(', KeyEffect::Inert),
    (b')', KeyEffect::Inert),
    (b' ', KeyEffect::Inert),
    (b'd', KeyEffect::Inert),
    (b'1', KeyEffect::Inert),
    (b'\r', KeyEffect::Commits),
];
const INERT_DIALOGS: &[DialogKeys<'static>] = &[DialogKeys {
    dialog: "update",
    keys: ALL_INERT,
}];

fn complete(agent: AgentType, release: &str, first_modal_secs: u64) -> ReleaseRecord<'static> {
    ReleaseRecord {
        agent,
        release: v(release),
        profile: "test-profile",
        ready_composer: Measurement::Verified(()),
        echo: Measurement::Verified(()),
        submit_route: Measurement::Verified(()),
        layout: Measurement::Verified(()),
        key_effects: Measurement::Verified(INERT_DIALOGS),
        first_modal: Measurement::Verified(FirstModal::Slowest {
            launches: 10,
            max: Duration::from_secs(first_modal_secs),
        }),
        input_mode: Measurement::Verified(()),
        release_recogniser: Measurement::Verified(()),
        parser_cost: Measurement::Verified(()),
        head_echo: Measurement::Verified(()),
        placeholder_empty: Measurement::Verified(()),
        delete_key: Measurement::Verified(()),
        transcript_echo: Measurement::Absent,
        editor_probe: Measurement::Absent,
    }
}

/// A record whose echo measurement is unverified, with a verified timing.
fn incomplete(agent: AgentType, release: &str, first_modal_secs: u64) -> ReleaseRecord<'static> {
    ReleaseRecord {
        echo: Measurement::Unverified,
        ..complete(agent, release, first_modal_secs)
    }
}

fn known(release: &str) -> LatchedRelease {
    LatchedRelease::Known(v(release))
}

fn verdict<'a>(records: &[ReleaseRecord<'a>], agent: &AgentType, release: &str) -> Activation<'a> {
    activation(records, agent, &known(release))
}

fn no_go(a: &Activation<'_>) -> Option<NoGo> {
    match a {
        Activation::NoGo(n) => Some(n.clone()),
        _ => None,
    }
}

const REQUIRED: [&str; 9] = [
    "ready_composer",
    "echo",
    "submit_route",
    "layout",
    "key_effects",
    "first_modal",
    "input_mode",
    "release_recogniser",
    "parser_cost",
];

#[derive(Clone, Copy, Debug)]
enum State {
    Verified,
    Absent,
    Unverified,
    Unraisable,
}

fn set(record: &mut ReleaseRecord<'static>, field: usize, state: State) {
    fn m<T: Copy>(state: State, value: T) -> Measurement<T> {
        match state {
            State::Verified => Measurement::Verified(value),
            State::Absent => Measurement::Absent,
            State::Unverified => Measurement::Unverified,
            State::Unraisable => Measurement::Unraisable,
        }
    }
    match field {
        0 => record.ready_composer = m(state, ()),
        1 => record.echo = m(state, ()),
        2 => record.submit_route = m(state, ()),
        3 => record.layout = m(state, ()),
        4 => record.key_effects = m(state, INERT_DIALOGS),
        5 => {
            record.first_modal = m(
                state,
                FirstModal::Slowest {
                    launches: 10,
                    max: Duration::from_secs(2),
                },
            )
        }
        6 => record.input_mode = m(state, ()),
        7 => record.release_recogniser = m(state, ()),
        8 => record.parser_cost = m(state, ()),
        _ => unreachable!(),
    }
}

/// Scenario: Build a Codex record for every combination of its nine required measurements being verified, absent, unverified or unraisable, and ask the table about that exact release. It is Go exactly when all nine are verified; otherwise the first unmet one is named, and an unraisable one makes the release unmeasured.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_go_iff_every_required_measurement_is_verified() {
    const STATES: [State; 4] = [
        State::Verified,
        State::Absent,
        State::Unverified,
        State::Unraisable,
    ];
    let mut checked = 0;
    for combo in 0..4usize.pow(9) {
        let mut record = complete(CODEX, "0.164.0", 2);
        let mut states = [State::Verified; 9];
        let mut n = combo;
        for (field, slot) in states.iter_mut().enumerate() {
            *slot = STATES[n % 4];
            n /= 4;
            set(&mut record, field, *slot);
        }
        let got = verdict(std::slice::from_ref(&record), &CODEX, "0.164.0");
        let first_unmet = states.iter().position(|s| !matches!(s, State::Verified));
        match first_unmet {
            None => assert!(
                matches!(got, Activation::Go { .. }),
                "all verified must be Go: {got:?}"
            ),
            Some(field) => {
                let want = match states[field] {
                    State::Absent => NoGo::MissingMeasurement(REQUIRED[field]),
                    State::Unverified => NoGo::UnverifiedMeasurement(REQUIRED[field]),
                    State::Unraisable => NoGo::UnmeasuredRelease,
                    State::Verified => unreachable!(),
                };
                assert_eq!(no_go(&got), Some(want), "{states:?}");
            }
        }
        checked += 1;
    }
    assert_eq!(checked, 4usize.pow(9));
    // A failed measurement is unverified, never Go.
    let failed = ReleaseRecord {
        echo: Measurement::Failed,
        ..complete(CODEX, "0.164.0", 2)
    };
    assert_eq!(
        no_go(&verdict(&[failed], &CODEX, "0.164.0")),
        Some(NoGo::UnverifiedMeasurement("echo"))
    );
}

/// Scenario: Ask the table about generations whose header latched nothing, latched two releases, ran a scanner over its cell budget, or had its scanner dropped at its work budget, and about records whose capability-only fields are absent. The first four are held with their own clause; missing capabilities leave a complete record Go.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_unlatched_releases_and_capabilities() {
    let records = [complete(CODEX, "0.164.0", 2)];
    for (latch, want) in [
        (LatchedRelease::Unseen, NoGo::UnknownRelease),
        (LatchedRelease::Conflict, NoGo::ReleaseConflict),
        (LatchedRelease::ScannerGeometry, NoGo::ScannerGeometry),
        (LatchedRelease::ScannerBudget, NoGo::ScannerBudget),
    ] {
        assert_eq!(
            no_go(&activation(&records, &CODEX, &latch)),
            Some(want.clone())
        );
        assert_eq!(NoGo::UnknownRelease.clause(), "unknown_release");
    }
    assert_eq!(NoGo::ScannerBudget.clause(), "scanner_budget");
    let bare = ReleaseRecord {
        placeholder_empty: Measurement::Absent,
        delete_key: Measurement::Absent,
        transcript_echo: Measurement::Absent,
        ..complete(CODEX, "0.164.0", 2)
    };
    assert!(matches!(
        verdict(&[bare], &CODEX, "0.164.0"),
        Activation::Go { .. }
    ));
}

/// Scenario: Give complete records dialogs in which a token letter, parenthesis, later fixed-text byte or message-id digit commits, and a first-dialog time over the cap. The first two are held as first_stage_commits, later committing bytes require staging, and excessive timing is held as settle_over_cap; in a mixed table, each exact release retains its own settle window.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_key_effects_and_own_settle() {
    let records = [
        complete(CODEX, "0.160.0", 16),
        complete(CODEX, "0.164.0", 2),
    ];
    for (release, seconds) in [("0.160.0", 24), ("0.164.0", 3)] {
        assert_eq!(
            verdict(&records, &CODEX, release),
            Activation::Go {
                profile: "test-profile",
                staged: false,
                settle: Duration::from_secs(seconds),
            },
            "{release} retains its own measured settle in a mixed table"
        );
    }
    for key in *b"d()z" {
        let keys = [(key, KeyEffect::Commits)];
        let dialogs = [DialogKeys {
            dialog: "update",
            keys: &keys,
        }];
        let record = ReleaseRecord {
            key_effects: Measurement::Verified(&dialogs),
            ..complete(CODEX, "0.164.0", 2)
        };
        assert_eq!(
            no_go(&verdict(&[record], &CODEX, "0.164.0")),
            Some(NoGo::FirstStageCommits {
                dialog: "update".into(),
                key
            }),
            "{}",
            key as char
        );
    }
    // `n` is in the line's fixed text (" dot-agent-deck: message "), which is
    // typed after the token head: it stages the line.
    const PREFIX_COMMITS: &[(u8, KeyEffect)] = &[(b'n', KeyEffect::Commits)];
    const PREFIX: &[DialogKeys<'static>] = &[DialogKeys {
        dialog: "trust",
        keys: PREFIX_COMMITS,
    }];
    let record = ReleaseRecord {
        key_effects: Measurement::Verified(PREFIX),
        ..complete(CODEX, "0.164.0", 2)
    };
    assert_eq!(
        verdict(&[record], &CODEX, "0.164.0"),
        Activation::Go {
            profile: "test-profile",
            staged: true,
            settle: Duration::from_secs(3),
        }
    );
    const DIGIT_COMMITS: &[(u8, KeyEffect)] =
        &[(b'1', KeyEffect::Commits), (b'd', KeyEffect::Moves)];
    const DIGIT: &[DialogKeys<'static>] = &[DialogKeys {
        dialog: "update",
        keys: DIGIT_COMMITS,
    }];
    let record = ReleaseRecord {
        key_effects: Measurement::Verified(DIGIT),
        ..complete(CODEX, "0.164.0", 2)
    };
    assert_eq!(
        verdict(&[record], &CODEX, "0.164.0"),
        Activation::Go {
            profile: "test-profile",
            staged: true,
            settle: Duration::from_secs(3),
        }
    );
    assert_eq!(
        verdict(&[complete(CODEX, "0.164.0", 2)], &CODEX, "0.164.0"),
        Activation::Go {
            profile: "test-profile",
            staged: false,
            settle: Duration::from_secs(3),
        }
    );
    // 21 s + 50 % = 31.5 s, over the 30 s cap; 20 s + 50 % = 30 s is not.
    assert_eq!(
        no_go(&verdict(
            &[complete(CODEX, "0.164.0", 21)],
            &CODEX,
            "0.164.0"
        )),
        Some(NoGo::SettleOverCap)
    );
    assert_eq!(
        verdict(&[complete(CODEX, "0.164.0", 20)], &CODEX, "0.164.0"),
        Activation::Go {
            profile: "test-profile",
            staged: false,
            settle: SETTLE_CAP,
        }
    );
    let none = ReleaseRecord {
        first_modal: Measurement::Verified(FirstModal::NoneObserved { launches: 10 }),
        ..complete(CLAUDE, "2.1.310", 0)
    };
    assert_eq!(
        verdict(&[none], &CLAUDE, "2.1.310"),
        Activation::Go {
            profile: "test-profile",
            staged: false,
            settle: Duration::ZERO,
        }
    );
}

/// Scenario: Put a measured Go Codex 0.164.0 beside newer minor and patch releases, a pre-release, another major and an older release. Every release without its own record is held as unmeasured_release, while an exact incomplete record keeps its own missing fact instead of borrowing an older Go.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_only_an_exact_record_admits() {
    // An exact record decides, even incomplete, past an older complete one.
    let records = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.164.0", 2),
    ];
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.164.0")),
        Some(NoGo::UnverifiedMeasurement("echo"))
    );
    let records = [complete(CODEX, "0.164.0", 2)];
    assert!(matches!(
        verdict(&records, &CODEX, "0.164.0"),
        Activation::Go { .. }
    ));
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.165.0-alpha.1")),
        Some(NoGo::UnmeasuredRelease),
        "a nearby newer pre-release"
    );
    for release in ["0.165.0", "0.168.9", "0.169.0", "0.164.1", "0.164.9"] {
        let got = verdict(&records, &CODEX, release);
        assert_eq!(
            no_go(&got),
            Some(NoGo::UnmeasuredRelease),
            "{release} has no measurements of its own: {got:?}"
        );
    }
    // A nearby incomplete record supplies no authority either.
    let incomplete_source = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.163.0", 2),
    ];
    assert_eq!(
        no_go(&verdict(&incomplete_source, &CODEX, "0.164.0")),
        Some(NoGo::UnmeasuredRelease)
    );
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "1.0.0")),
        Some(NoGo::UnmeasuredRelease),
        "another major"
    );
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.159.9")),
        Some(NoGo::UnmeasuredRelease),
        "older than every record"
    );
    assert_eq!(
        no_go(&verdict(&[], &CODEX, "0.164.0")),
        Some(NoGo::UnmeasuredRelease),
        "no records"
    );
}

/// Scenario: Ask about Claude Code releases newer than a complete Claude Code record, including a patch release and a pre-release, and about agents with no record. Each is held as unmeasured_release, while the exact Claude Code release stays Go.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_claude_and_other_agents_need_their_own_record() {
    let records = [complete(CLAUDE, "2.1.310", 0)];
    for release in ["2.1.311", "2.2.0", "2.1.310-beta.1"] {
        assert_eq!(
            no_go(&verdict(&records, &CLAUDE, release)),
            Some(NoGo::UnmeasuredRelease),
            "{release}"
        );
    }
    assert!(matches!(
        verdict(&records, &CLAUDE, "2.1.310"),
        Activation::Go { .. }
    ));
    for agent in [AgentType::OpenCode, AgentType::Pi, AgentType::Devin] {
        assert_eq!(
            no_go(&verdict(&records, &agent, "2.1.310")),
            Some(NoGo::UnmeasuredRelease),
            "{agent:?} has no record"
        );
    }
}

/// Scenario: Exclude Claude Code releases from the records the way the e2e build does: a complete one, an incomplete one, one with no record, and a malformed list. The excluded releases become unmeasured_release, the unrecorded name changes nothing, and the malformed list is refused whole.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_claude_record_exclusion() {
    let records = [
        complete(CLAUDE, "2.1.310", 0),
        incomplete(CLAUDE, "2.1.300", 0),
    ];
    let excluded = parse_unrecorded_claude_releases("2.1.310, 2.1.300").unwrap();
    let left = without_unrecorded_claude(&records, &excluded);
    for release in ["2.1.310", "2.1.300"] {
        assert_eq!(
            no_go(&verdict(&left, &CLAUDE, release)),
            Some(NoGo::UnmeasuredRelease),
            "{release}"
        );
    }
    let left = without_unrecorded_claude(
        &records,
        &parse_unrecorded_claude_releases("2.1.999").unwrap(),
    );
    assert_eq!(left, records.to_vec());
    for malformed in [
        "2.1.310,",
        "2.1.310, claude:2.1.300",
        "2.1",
        "2.1.310-beta.1",
        "2.1.310+build",
        "v2.1.310",
    ] {
        assert!(
            parse_unrecorded_claude_releases(malformed).is_err(),
            "{malformed:?} must refuse the whole list"
        );
    }
    assert_eq!(parse_unrecorded_claude_releases("").unwrap(), vec![]);
}

/// Scenario: Build three Codex tables: an incomplete exact record, an incomplete record beside a newer unrecorded release, and a slow older record beside a newer unrecorded release. Ask each first with no exclusion and then with an exclusion list naming the same version strings. Every Codex verdict is unchanged.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_exclusion_never_reaches_codex() {
    let names = parse_unrecorded_claude_releases("0.160.0,0.163.0,0.164.0").unwrap();
    // (x) an incomplete exact record stays its own NoGo.
    let x = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.164.0", 2),
    ];
    // (y) a newer release beside an incomplete record stays unmeasured.
    let y = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.163.0", 2),
    ];
    // (z) a newer release beside a slow record stays unmeasured.
    let z = [
        complete(CODEX, "0.160.0", 16),
        complete(CODEX, "0.164.0", 2),
    ];
    for (table, release, want) in [
        (&x[..], "0.164.0", Some(NoGo::UnverifiedMeasurement("echo"))),
        (&y[..], "0.164.0", Some(NoGo::UnmeasuredRelease)),
        (&z[..], "0.165.0", Some(NoGo::UnmeasuredRelease)),
    ] {
        let intact = verdict(table, &CODEX, release);
        assert_eq!(no_go(&intact), want);
        let excluded = without_unrecorded_claude(table, &names);
        assert_eq!(verdict(&excluded, &CODEX, release), intact, "{release}");
    }
}

/// Scenario: Enumerate small record tables (Codex and Claude Code records on a few releases, each complete, incomplete or absent, fast or slow) against every live release in range and every exclusion list over those version strings. With any exclusion, every verdict equals the intact one or is NoGo, and no Codex verdict ever changes.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_exclusion_only_ever_disables() {
    #[derive(Clone, Copy)]
    enum R {
        Absent,
        Complete(u64),
        Incomplete(u64),
    }
    let codex_releases = ["0.160.0", "0.163.0", "0.164.0"];
    let claude_releases = ["2.1.300", "2.1.310"];
    let codex_states = [
        R::Absent,
        R::Complete(2),
        R::Complete(16),
        R::Incomplete(2),
        R::Incomplete(16),
    ];
    let claude_states = [R::Absent, R::Complete(0), R::Incomplete(0)];
    let live: Vec<(AgentType, &str)> = [
        "0.159.0", "0.160.0", "0.161.0", "0.163.0", "0.164.0", "0.165.0", "0.168.0", "0.169.0",
        "1.0.0",
    ]
    .iter()
    .map(|r| (CODEX, *r))
    .chain(
        ["2.1.299", "2.1.300", "2.1.305", "2.1.310", "2.1.311"]
            .iter()
            .map(|r| (CLAUDE, *r)),
    )
    .collect();
    let names: Vec<&str> = codex_releases
        .iter()
        .chain(&claude_releases)
        .copied()
        .collect();
    let mut tables = 0;
    for codex in 0..codex_states.len().pow(3) {
        for claude in 0..claude_states.len().pow(2) {
            let mut records = Vec::new();
            let mut n = codex;
            for release in codex_releases {
                match codex_states[n % codex_states.len()] {
                    R::Absent => {}
                    R::Complete(t) => records.push(complete(CODEX, release, t)),
                    R::Incomplete(t) => records.push(incomplete(CODEX, release, t)),
                }
                n /= codex_states.len();
            }
            let mut n = claude;
            for release in claude_releases {
                match claude_states[n % claude_states.len()] {
                    R::Absent => {}
                    R::Complete(t) => records.push(complete(CLAUDE, release, t)),
                    R::Incomplete(t) => records.push(incomplete(CLAUDE, release, t)),
                }
                n /= claude_states.len();
            }
            let intact: Vec<Activation<'_>> = live
                .iter()
                .map(|(agent, r)| verdict(&records, agent, r))
                .collect();
            for mask in 0..(1u32 << names.len()) {
                let list: Vec<&str> = names
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, n)| *n)
                    .collect();
                let excluded = parse_unrecorded_claude_releases(&list.join(",")).unwrap();
                let left = without_unrecorded_claude(&records, &excluded);
                for ((agent, r), before) in live.iter().zip(&intact) {
                    let after = verdict(&left, agent, r);
                    if *agent == CODEX {
                        assert_eq!(&after, before, "a Codex verdict changed: {r} {list:?}");
                    } else {
                        assert!(
                            &after == before || matches!(after, Activation::NoGo(_)),
                            "{r} {list:?}: {before:?} became {after:?}"
                        );
                    }
                }
            }
            tables += 1;
        }
    }
    assert_eq!(tables, 125 * 9);
}

/// Scenario: Give a staged record (a message-id digit commits in its dialog) each state of its head-echo measurement, and an unstaged record no head-echo measurement at all; then ask about a newer release beside a staged record, with and without its head echo. The staged record is Go only with its head echo verified, held as missing_measurement or unverified_measurement naming head_echo otherwise and unmeasured_release when unraisable; the unstaged record is Go without one; and the newer release is unmeasured_release either way.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_a_staged_line_needs_its_head_echo_measured() {
    const DIGIT_COMMITS: &[(u8, KeyEffect)] = &[(b'1', KeyEffect::Commits)];
    const DIGIT: &[DialogKeys<'static>] = &[DialogKeys {
        dialog: "update",
        keys: DIGIT_COMMITS,
    }];
    let staged = |head_echo| ReleaseRecord {
        key_effects: Measurement::Verified(DIGIT),
        head_echo,
        ..complete(CODEX, "0.164.0", 2)
    };
    for (head_echo, want) in [
        (
            Measurement::Absent,
            Some(NoGo::MissingMeasurement("head_echo")),
        ),
        (
            Measurement::Unverified,
            Some(NoGo::UnverifiedMeasurement("head_echo")),
        ),
        (
            Measurement::Failed,
            Some(NoGo::UnverifiedMeasurement("head_echo")),
        ),
        (Measurement::Unraisable, Some(NoGo::UnmeasuredRelease)),
        (Measurement::Verified(()), None),
    ] {
        let got = verdict(&[staged(head_echo)], &CODEX, "0.164.0");
        assert_eq!(no_go(&got), want, "{head_echo:?}");
        if want.is_none() {
            assert!(
                matches!(got, Activation::Go { staged: true, .. }),
                "{got:?}"
            );
        }
    }
    // Unstaged: the head is never typed alone, so nothing is needed.
    let unstaged = ReleaseRecord {
        head_echo: Measurement::Absent,
        ..complete(CODEX, "0.164.0", 2)
    };
    assert!(matches!(
        verdict(&[unstaged], &CODEX, "0.164.0"),
        Activation::Go { staged: false, .. }
    ));
    // A newer release borrows nothing from a staged record, complete or not.
    for head_echo in [Measurement::Absent, Measurement::Verified(())] {
        let records = [staged(head_echo)];
        assert_eq!(
            no_go(&verdict(&records, &CODEX, "0.165.0")),
            Some(NoGo::UnmeasuredRelease),
            "{head_echo:?}"
        );
    }
}

/// Scenario: For every dialog of every release record the deck carries, now and in any later edit of the table, look up each byte the line's first stage can contain: both parentheses and every letter of the token alphabet. Each must be classified, and classified inert: a record in which one of them commits, moves, edits or dismisses, or is left unmeasured, fails this test.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_every_recorded_dialog_leaves_the_token_head_inert() {
    let first_stage: Vec<u8> = first_stage_bytes().collect();
    assert_eq!(first_stage.len(), 18, "two parentheses and sixteen letters");
    for record in &RELEASE_RECORDS {
        let Measurement::Verified(dialogs) = record.key_effects else {
            continue;
        };
        for dialog in dialogs {
            for key in &first_stage {
                let effect = dialog.keys.iter().find(|(k, _)| k == key).map(|(_, e)| *e);
                assert_eq!(
                    effect,
                    Some(KeyEffect::Inert),
                    "{:?} {} `{}`: {:?} in {}",
                    record.agent,
                    record.release,
                    *key as char,
                    effect,
                    dialog.dialog
                );
            }
        }
    }
}

/// Scenario: Assume `head_echo` for Codex 0.164.0, as an e2e run asks before the Q27 Go change, over the shipped records. 0.164.0 becomes Go exactly as its other measurements say, staged; 0.160.0, named nowhere, and the Claude Code record, named or not, are unchanged; and a Codex record missing anything besides `head_echo` keeps its own NoGo.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_an_assumed_head_echo_completes_only_that_codex_record() {
    let assumed =
        with_assumed_codex_head_echo(RELEASE_RECORDS.to_vec(), &[v("0.164.0"), v("2.1.310")]);
    assert!(matches!(
        verdict(&assumed, &CODEX, "0.164.0"),
        Activation::Go { staged: true, .. }
    ));
    assert_eq!(
        verdict(&assumed, &CODEX, "0.160.0"),
        verdict(&RELEASE_RECORDS, &CODEX, "0.160.0")
    );
    assert_eq!(
        verdict(&assumed, &CLAUDE, "2.1.310"),
        verdict(&RELEASE_RECORDS, &CLAUDE, "2.1.310")
    );
    let missing = [incomplete(CODEX, "0.164.0", 2)];
    let still = with_assumed_codex_head_echo(missing.to_vec(), &[v("0.164.0")]);
    assert_eq!(
        verdict(&still, &CODEX, "0.164.0"),
        verdict(&missing, &CODEX, "0.164.0")
    );
    assert!(no_go(&verdict(&still, &CODEX, "0.164.0")).is_some());
    assert_eq!(
        with_assumed_codex_head_echo(RELEASE_RECORDS.to_vec(), &[]),
        RELEASE_RECORDS.to_vec()
    );
}

/// Scenario: Ask the table about each release the deck carries a record for, as it ships. Claude Code 2.1.310 and 2.1.312 are Go, unstaged, with their editor probes absent, while 2.1.311 remains unmeasured; both Codex releases stage the line and are held as missing_measurement until their head echo is recorded; and every record has its parser cost verified.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_shipped_records_verdicts() {
    for record in &RELEASE_RECORDS {
        assert!(
            matches!(record.parser_cost, Measurement::Verified(())),
            "{:?} {}",
            record.agent,
            record.release
        );
        let verdict = exact_verdict(record);
        match (&record.agent, record.release.to_string().as_str()) {
            (AgentType::ClaudeCode, "2.1.310" | "2.1.312") => assert!(
                matches!(verdict, Activation::Go { staged: false, .. }),
                "{verdict:?}"
            ),
            (AgentType::Codex, "0.164.0" | "0.160.0") => assert_eq!(
                verdict,
                Activation::NoGo(NoGo::MissingMeasurement("head_echo"))
            ),
            other => panic!("a record this test does not know: {other:?}"),
        }
    }
    for release in ["2.1.310", "2.1.312"] {
        let record = RELEASE_RECORDS
            .iter()
            .find(|record| record.agent == CLAUDE && record.release == v(release))
            .expect("measured Claude release record");
        assert!(matches!(record.editor_probe, Measurement::Absent));
        assert_eq!(
            activation(
                &RELEASE_RECORDS,
                &CLAUDE,
                &LatchedRelease::Known(v(release)),
            ),
            exact_verdict(record)
        );
    }
    assert_eq!(
        verdict(&RELEASE_RECORDS, &CLAUDE, "2.1.311"),
        Activation::NoGo(NoGo::UnmeasuredRelease)
    );
}

/// A verified editor record, as a measurement of one release would give it.
const PROBE: EditorProbe<'static> = EditorProbe {
    export: ExportShape::CodexEditorDir,
    selection: EditorSelection::Codex,
    paste_toggles: PasteToggles {
        disables: 1,
        enables: 1,
    },
    keymap_sources: &[KeymapSource::CodexConfig],
    keymap_loaded_at_start: Measurement::Absent,
    inert_candidates: &[b"\x07"],
    pasted_bel_is_text: Measurement::Verified(()),
};

/// Every state an editor record can be in.
const EDITOR_STATES: [Measurement<EditorProbe<'static>>; 5] = [
    Measurement::Absent,
    Measurement::Unverified,
    Measurement::Failed,
    Measurement::Unraisable,
    Measurement::Verified(PROBE),
];

/// Scenario: Give each kind of record the table decides (Go, Go and staged, each required measurement absent, unverified or unraisable, a committing first-stage key, a settle over the cap, a staged one without its head echo) every editor-record state, and ask the table and the production path that applies the e2e assumptions about its release. Each verdict is the one the record has with no editor record: a missing one turns no Go off, and a verified one turns no NoGo on.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_an_editor_record_decides_no_ring() {
    const PAREN_COMMITS: &[DialogKeys<'static>] = &[DialogKeys {
        dialog: "trust",
        keys: &[(b'(', KeyEffect::Commits)],
    }];
    const DIGIT_COMMITS: &[DialogKeys<'static>] = &[DialogKeys {
        dialog: "update",
        keys: &[(b'1', KeyEffect::Commits)],
    }];
    let mut records = vec![
        complete(CODEX, "0.164.0", 2),
        ReleaseRecord {
            key_effects: Measurement::Verified(DIGIT_COMMITS),
            ..complete(CODEX, "0.164.0", 2)
        },
        ReleaseRecord {
            key_effects: Measurement::Verified(DIGIT_COMMITS),
            head_echo: Measurement::Absent,
            ..complete(CODEX, "0.164.0", 2)
        },
        ReleaseRecord {
            key_effects: Measurement::Verified(PAREN_COMMITS),
            ..complete(CODEX, "0.164.0", 2)
        },
        complete(CODEX, "0.164.0", 21),
        complete(CLAUDE, "2.1.310", 0),
    ];
    for field in 0..REQUIRED.len() {
        for state in [State::Absent, State::Unverified, State::Unraisable] {
            let mut record = complete(CODEX, "0.164.0", 2);
            set(&mut record, field, state);
            records.push(record);
        }
    }
    let mut go = 0;
    let mut held = 0;
    for base in &records {
        let release = LatchedRelease::Known(base.release.clone());
        let want = exact_verdict(base);
        match want {
            Activation::NoGo(_) => held += 1,
            _ => go += 1,
        }
        for editor in EDITOR_STATES {
            let record = ReleaseRecord {
                editor_probe: editor,
                ..base.clone()
            };
            assert_eq!(exact_verdict(&record), want, "{editor:?}");
            let one = std::slice::from_ref(&record);
            assert_eq!(activation(one, &record.agent, &release), want);
            assert_eq!(
                generation_activation(one, Some(&record.agent), &release),
                want,
                "{editor:?}"
            );
        }
    }
    assert_eq!((go, held), (3, 3 + 3 * REQUIRED.len()));
}

/// Scenario: Enumerate small record tables (Codex and Claude Code records on a few releases, each complete, incomplete or absent, fast or slow) and, for each, every assignment of a verified or missing editor record to its records, then ask the table about every live release in range, unrecorded ones included. Every verdict equals the one with no editor record anywhere.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_editor_records_move_no_verdict_in_any_table() {
    #[derive(Clone, Copy)]
    enum R {
        Absent,
        Complete(u64),
        Incomplete(u64),
    }
    let codex_releases = ["0.160.0", "0.163.0", "0.164.0"];
    let claude_releases = ["2.1.300", "2.1.310"];
    let states = [R::Absent, R::Complete(2), R::Complete(16), R::Incomplete(2)];
    let live: Vec<(AgentType, &str)> = [
        "0.159.0", "0.160.0", "0.161.0", "0.163.0", "0.164.0", "0.165.0", "0.168.0", "0.169.0",
    ]
    .iter()
    .map(|r| (CODEX, *r))
    .chain(
        ["2.1.300", "2.1.305", "2.1.310"]
            .iter()
            .map(|r| (CLAUDE, *r)),
    )
    .collect();
    let mut tables = 0;
    for table in 0..states.len().pow(5) {
        let mut records = Vec::new();
        let mut n = table;
        for (agent, release) in codex_releases
            .iter()
            .map(|r| (CODEX, *r))
            .chain(claude_releases.iter().map(|r| (CLAUDE, *r)))
        {
            match states[n % states.len()] {
                R::Absent => {}
                R::Complete(t) => records.push(complete(agent, release, t)),
                R::Incomplete(t) => records.push(incomplete(agent, release, t)),
            }
            n /= states.len();
        }
        let intact: Vec<Activation<'_>> = live
            .iter()
            .map(|(agent, r)| verdict(&records, agent, r))
            .collect();
        for mask in 1..(1u32 << records.len()) {
            let with: Vec<ReleaseRecord<'_>> = records
                .iter()
                .enumerate()
                .map(|(i, r)| ReleaseRecord {
                    editor_probe: if mask & (1 << i) != 0 {
                        Measurement::Verified(PROBE)
                    } else {
                        Measurement::Absent
                    },
                    ..r.clone()
                })
                .collect();
            for ((agent, r), before) in live.iter().zip(&intact) {
                assert_eq!(&verdict(&with, agent, r), before, "{r} mask {mask:#b}");
            }
        }
        tables += 1;
    }
    assert_eq!(tables, 4usize.pow(5));
}

/// Scenario: Ask about each record the deck ships, as shipped and with the e2e assumption giving every one of them a verified editor record. Every shipped record's editor record is absent; the assumption changes that field and nothing else, adds no record, and leaves every verdict, Claude Code 2.1.310's Go and both Codex releases' missing_measurement, as it was.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_editor_probe_assumption_moves_no_shipped_verdict() {
    for record in &RELEASE_RECORDS {
        assert_eq!(
            record.editor_probe,
            Measurement::Absent,
            "{}",
            record.release
        );
    }
    let all: Vec<_> = RELEASE_RECORDS
        .iter()
        .map(|r| (r.agent.clone(), r.release.clone(), PROBE))
        .chain(std::iter::once((CLAUDE, v("2.1.311"), PROBE)))
        .collect();
    let assumed = with_assumed_editor_probes(RELEASE_RECORDS.to_vec(), &all);
    assert_eq!(assumed.len(), RELEASE_RECORDS.len(), "no record is added");
    for (after, before) in assumed.iter().zip(&RELEASE_RECORDS) {
        assert_eq!(after.editor_probe, Measurement::Verified(PROBE));
        assert_eq!(
            &ReleaseRecord {
                editor_probe: Measurement::Absent,
                ..after.clone()
            },
            before
        );
        assert_eq!(exact_verdict(after), exact_verdict(before));
        let release = LatchedRelease::Known(before.release.clone());
        assert_eq!(
            generation_activation(&assumed, Some(&before.agent), &release),
            generation_activation(&RELEASE_RECORDS, Some(&before.agent), &release),
        );
    }
    assert_eq!(
        no_go(&generation_activation(
            &assumed,
            Some(&CLAUDE),
            &known("2.1.311"),
        )),
        Some(NoGo::UnmeasuredRelease)
    );
    // Naming one release changes only that record.
    let one = with_assumed_editor_probes(RELEASE_RECORDS.to_vec(), &[(CODEX, v("0.160.0"), PROBE)]);
    let verified: Vec<String> = one
        .iter()
        .filter(|r| r.editor_probe != Measurement::Absent)
        .map(|r| format!("{:?}@{}", r.agent, r.release))
        .collect();
    assert_eq!(verified, ["Codex@0.160.0"]);
}
