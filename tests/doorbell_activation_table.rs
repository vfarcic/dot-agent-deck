//! `inbox/doorbell/053`: the activation table, rule by rule
//! (`src/doorbell_activation.rs`).
use std::time::Duration;

use dot_agent_deck::doorbell_activation::{
    Activation, DialogKeys, FirstModal, KeyEffect, LatchedRelease, Measurement, NoGo,
    ReleaseRecord, SETTLE_CAP, activation, parse_unrecorded_claude_releases,
    without_unrecorded_claude,
};
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
        placeholder_empty: Measurement::Verified(()),
        delete_key: Measurement::Verified(()),
        transcript_echo: Measurement::Absent,
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
    activation(records, agent, &known(release), &[])
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

/// Scenario: Ask the table about generations whose header latched nothing, latched two releases, or ran a scanner over its budget, and about records whose capability-only fields are absent. The first three are held with their own clause; missing capabilities leave a complete record Go.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_unlatched_releases_and_capabilities() {
    let records = [complete(CODEX, "0.164.0", 2)];
    for (latch, want) in [
        (LatchedRelease::Unseen, NoGo::UnknownRelease),
        (LatchedRelease::Conflict, NoGo::ReleaseConflict),
        (LatchedRelease::ScannerGeometry, NoGo::ScannerGeometry),
    ] {
        assert_eq!(
            no_go(&activation(&records, &CODEX, &latch, &[])),
            Some(want.clone())
        );
        assert_eq!(NoGo::UnknownRelease.clause(), "unknown_release");
    }
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

/// Scenario: Give a complete record one dialog in which a byte of the line's fixed head commits, then one in which only a message-id digit commits, then one whose own first-dialog time puts its settle window over the cap. The first is held as first_stage_commits naming the dialog and key, the second is Go and staged, the third is held as settle_over_cap.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_key_effects_and_own_settle() {
    const HEAD_COMMITS: &[(u8, KeyEffect)] = &[(b'd', KeyEffect::Commits)];
    const HEAD: &[DialogKeys<'static>] = &[DialogKeys {
        dialog: "update",
        keys: HEAD_COMMITS,
    }];
    let record = ReleaseRecord {
        key_effects: Measurement::Verified(HEAD),
        ..complete(CODEX, "0.164.0", 2)
    };
    assert_eq!(
        no_go(&verdict(&[record], &CODEX, "0.164.0")),
        Some(NoGo::FirstStageCommits {
            dialog: "update".into(),
            key: b'd'
        })
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

/// Scenario: Ask the table about Codex releases around complete and incomplete records: an exact incomplete record beside an older complete one, an incomplete newest-older record, minor distances 4 and 5, a patch release, another major, a pre-release and a release older than every record. Only the in-range releases above a complete newest source inherit.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_inheritance_rules() {
    // An exact record decides, even incomplete, past an older complete one.
    let records = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.164.0", 2),
    ];
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.164.0")),
        Some(NoGo::UnverifiedMeasurement("echo"))
    );
    // The newest older record is the source, never one past it.
    let records = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.163.0", 2),
    ];
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.164.0")),
        Some(NoGo::InheritSourceIncomplete)
    );
    let records = [complete(CODEX, "0.160.0", 2)];
    let inherited = |release: &str| match verdict(&records, &CODEX, release) {
        Activation::Inherited {
            from,
            profile,
            settle,
            ..
        } => {
            assert_eq!(from, v("0.160.0"));
            assert_eq!(profile, "test-profile");
            assert_eq!(settle, Duration::from_secs(10));
            true
        }
        _ => false,
    };
    assert!(inherited("0.164.0"), "minor distance 4 inherits");
    assert!(inherited("0.160.3"), "a patch release inherits");
    assert!(inherited("0.164.9"));
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.165.0")),
        Some(NoGo::UnmeasuredRelease),
        "minor distance 5"
    );
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "1.0.0")),
        Some(NoGo::UnmeasuredRelease),
        "another major"
    );
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.161.0-alpha.1")),
        Some(NoGo::UnmeasuredRelease),
        "a pre-release"
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

/// Scenario: Set the tripwire for one inherited Codex release and ask about it and about another inherited release. The tripped one is held as release_tripwire while the other still inherits, and a measured release is unaffected.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_tripwire_is_per_release() {
    let records = [complete(CODEX, "0.160.0", 2), complete(CODEX, "0.162.0", 2)];
    let tripped = [(CODEX, v("0.163.0"))];
    assert_eq!(
        no_go(&activation(&records, &CODEX, &known("0.163.0"), &tripped)),
        Some(NoGo::ReleaseTripwire)
    );
    assert!(matches!(
        activation(&records, &CODEX, &known("0.164.0"), &tripped),
        Activation::Inherited { .. }
    ));
    // A tripwire on another agent type's release of the same number does nothing.
    let other = [(CLAUDE, v("0.164.0"))];
    assert!(matches!(
        activation(&records, &CODEX, &known("0.164.0"), &other),
        Activation::Inherited { .. }
    ));
    assert!(matches!(
        activation(
            &records,
            &CODEX,
            &known("0.162.0"),
            &[(CODEX, v("0.162.0"))]
        ),
        Activation::Go { .. }
    ));
}

/// Scenario: Ask about inherited Codex releases with first-dialog times recorded on several releases: a slow older record beside a fast newer source, a record whose timing is unverified, a slower record of another agent, and only fast records. The window is twice the slowest verified Codex time with a 10 s floor, never the source's alone, and over the cap it is held, never clamped.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_timing_set() {
    let records = [
        complete(CODEX, "0.160.0", 12),
        complete(CODEX, "0.164.0", 2),
    ];
    assert_eq!(
        verdict(&records, &CODEX, "0.165.0"),
        Activation::Inherited {
            from: v("0.164.0"),
            profile: "test-profile",
            staged: false,
            settle: Duration::from_secs(24),
            settle_from: Some(v("0.160.0")),
        }
    );
    let records = [
        complete(CODEX, "0.160.0", 16),
        complete(CODEX, "0.164.0", 2),
    ];
    assert_eq!(
        no_go(&verdict(&records, &CODEX, "0.165.0")),
        Some(NoGo::SettleOverCap),
        "2 × 16 s = 32 s is over the cap"
    );
    assert!(
        matches!(
            verdict(&records, &CODEX, "0.164.0"),
            Activation::Go { settle, .. } if settle == Duration::from_secs(3)
        ),
        "0.164.0 itself stays Go on its own timing"
    );
    assert!(
        matches!(
            verdict(&records, &CODEX, "0.160.0"),
            Activation::Go { settle, .. } if settle == Duration::from_secs(24)
        ),
        "16 s + 50 % is within the cap"
    );
    // An unverified timing adds nothing.
    let unverified = ReleaseRecord {
        first_modal: Measurement::Unverified,
        ..complete(CODEX, "0.158.0", 0)
    };
    let with = [unverified, complete(CODEX, "0.164.0", 2)];
    let without = [complete(CODEX, "0.164.0", 2)];
    assert_eq!(
        verdict(&with, &CODEX, "0.165.0"),
        verdict(&without, &CODEX, "0.165.0")
    );
    // Another agent type's slower record adds nothing.
    let mixed = [
        complete(CLAUDE, "2.1.310", 20),
        complete(CODEX, "0.164.0", 2),
    ];
    assert_eq!(
        verdict(&mixed, &CODEX, "0.165.0"),
        verdict(&without, &CODEX, "0.165.0")
    );
    // Only fast records: the 10 s floor.
    assert!(matches!(
        verdict(&without, &CODEX, "0.165.0"),
        Activation::Inherited { settle, .. } if settle == Duration::from_secs(10)
    ));
}

/// Scenario: Ask about Claude Code releases newer than a complete Claude Code record, including a patch release. Claude Code never inherits: each is held as unmeasured_release.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_inheritance_is_codex_only() {
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

/// Scenario: Build the three Codex tables where removing a Codex record would unblock inheritance or shorten its window: an incomplete exact record, an incomplete newest source, and a slow timing. Ask each first with no exclusion and then with an exclusion list naming the same version strings. Every Codex verdict is unchanged.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_exclusion_never_reaches_codex() {
    let names = parse_unrecorded_claude_releases("0.160.0,0.163.0,0.164.0").unwrap();
    // (x) an incomplete exact record stays its own NoGo.
    let x = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.164.0", 2),
    ];
    // (y) an incomplete newest source stays inherit_source_incomplete.
    let y = [
        complete(CODEX, "0.160.0", 2),
        incomplete(CODEX, "0.163.0", 2),
    ];
    // (z) a slow timing stays over the cap.
    let z = [
        complete(CODEX, "0.160.0", 16),
        complete(CODEX, "0.164.0", 2),
    ];
    for (table, release, want) in [
        (&x[..], "0.164.0", Some(NoGo::UnverifiedMeasurement("echo"))),
        (&y[..], "0.164.0", Some(NoGo::InheritSourceIncomplete)),
        (&z[..], "0.165.0", Some(NoGo::SettleOverCap)),
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
