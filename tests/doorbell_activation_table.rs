//! `inbox/doorbell/053`: the activation table, rule by rule, over one set of
//! facts per agent, and the recorded releases as conformance tests
//! (`src/doorbell_activation.rs`).
use std::time::Duration;

use dot_agent_deck::doorbell_activation::{
    AGENT_FACTS, Activation, AgentFacts, CLAUDE_FACTS, CODEX_FACTS, DialogKeys, EditorProbe,
    EditorSelection, ExportShape, FirstModal, KeyEffect, KeymapSource, LatchedRelease, Measurement,
    NoGo, PasteToggles, RECORDED_RELEASES, RecordedRelease, SETTLE_CAP, activation, conformance,
    editor_probe_for, facts_verdict, first_stage_bytes,
};
use dot_agent_deck::doorbell_generation::generation_activation;
use dot_agent_deck::doorbell_switch::DoorbellSwitch;
use dot_agent_deck::event::AgentType;
use semver::Version;
use spec::spec;

const CODEX: AgentType = AgentType::Codex;
const CLAUDE: AgentType = AgentType::ClaudeCode;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// The keys a known dialog classifies, all inert.
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

fn complete(agent: AgentType, first_modal_secs: u64) -> AgentFacts<'static> {
    AgentFacts {
        agent,
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

/// Facts whose echo is unverified, with a verified timing.
fn incomplete(agent: AgentType, first_modal_secs: u64) -> AgentFacts<'static> {
    AgentFacts {
        echo: Measurement::Unverified,
        ..complete(agent, first_modal_secs)
    }
}

fn known(release: &str) -> LatchedRelease {
    LatchedRelease::Known(v(release))
}

fn verdict<'a>(facts: &[AgentFacts<'a>], agent: &AgentType, release: &str) -> Activation<'a> {
    activation(facts, agent, &known(release))
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
    Failed,
}

fn set(facts: &mut AgentFacts<'static>, field: usize, state: State) {
    fn m<T: Copy>(state: State, value: T) -> Measurement<T> {
        match state {
            State::Verified => Measurement::Verified(value),
            State::Absent => Measurement::Absent,
            State::Unverified => Measurement::Unverified,
            State::Failed => Measurement::Failed,
        }
    }
    match field {
        0 => facts.ready_composer = m(state, ()),
        1 => facts.echo = m(state, ()),
        2 => facts.submit_route = m(state, ()),
        3 => facts.layout = m(state, ()),
        4 => facts.key_effects = m(state, INERT_DIALOGS),
        5 => {
            facts.first_modal = m(
                state,
                FirstModal::Slowest {
                    launches: 10,
                    max: Duration::from_secs(2),
                },
            )
        }
        6 => facts.input_mode = m(state, ()),
        7 => facts.release_recogniser = m(state, ()),
        8 => facts.parser_cost = m(state, ()),
        _ => unreachable!(),
    }
}

/// Scenario: Build Codex facts for every combination of their nine required facts being verified, absent, unverified or failed, and ask the table about a latched release. It is Go exactly when all nine are verified; otherwise the first unmet one is named, as missing_measurement when absent and unverified_measurement when unverified or failed.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_go_iff_every_required_measurement_is_verified() {
    const STATES: [State; 4] = [
        State::Verified,
        State::Absent,
        State::Unverified,
        State::Failed,
    ];
    let mut checked = 0;
    for combo in 0..4usize.pow(9) {
        let mut facts = complete(CODEX, 2);
        let mut states = [State::Verified; 9];
        let mut n = combo;
        for (field, slot) in states.iter_mut().enumerate() {
            *slot = STATES[n % 4];
            n /= 4;
            set(&mut facts, field, *slot);
        }
        let got = verdict(std::slice::from_ref(&facts), &CODEX, "0.164.0");
        let first_unmet = states.iter().position(|s| !matches!(s, State::Verified));
        match first_unmet {
            None => assert!(
                matches!(got, Activation::Go { .. }),
                "all verified must be Go: {got:?}"
            ),
            Some(field) => {
                let want = match states[field] {
                    State::Absent => NoGo::MissingMeasurement(REQUIRED[field]),
                    State::Unverified | State::Failed => {
                        NoGo::UnverifiedMeasurement(REQUIRED[field])
                    }
                    State::Verified => unreachable!(),
                };
                assert_eq!(no_go(&got), Some(want), "{states:?}");
            }
        }
        checked += 1;
    }
    assert_eq!(checked, 4usize.pow(9));
}

/// Scenario: Ask the table about generations whose header latched nothing, latched two releases, ran a scanner over its cell budget, or had its scanner dropped at its work budget, and about facts whose capability-only fields are absent. The first four are held with their own clause; missing capabilities leave complete facts Go.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_unlatched_releases_and_capabilities() {
    let facts = [complete(CODEX, 2)];
    for (latch, want) in [
        (LatchedRelease::Unseen, NoGo::UnknownRelease),
        (LatchedRelease::Conflict, NoGo::ReleaseConflict),
        (LatchedRelease::ScannerGeometry, NoGo::ScannerGeometry),
        (LatchedRelease::ScannerBudget, NoGo::ScannerBudget),
    ] {
        assert_eq!(
            no_go(&activation(&facts, &CODEX, &latch)),
            Some(want.clone())
        );
        assert_eq!(NoGo::UnknownRelease.clause(), "unknown_release");
    }
    assert_eq!(NoGo::ScannerBudget.clause(), "scanner_budget");
    let bare = AgentFacts {
        placeholder_empty: Measurement::Absent,
        delete_key: Measurement::Absent,
        transcript_echo: Measurement::Absent,
        ..complete(CODEX, 2)
    };
    assert!(matches!(
        verdict(&[bare], &CODEX, "0.164.0"),
        Activation::Go { .. }
    ));
}

/// Scenario: Give complete facts dialogs in which a token letter, parenthesis, later fixed-text byte or message-id digit commits, and a first-dialog time over the cap. The first two are held as first_stage_commits, later committing bytes require staging, and excessive timing is held as settle_over_cap; in a table with both agents, each agent keeps its own settle window.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_key_effects_and_own_settle() {
    let facts = [complete(CODEX, 16), complete(CLAUDE, 2)];
    for (agent, release, seconds) in [(CODEX, "0.160.0", 24), (CLAUDE, "2.1.310", 3)] {
        assert_eq!(
            verdict(&facts, &agent, release),
            Activation::Go {
                profile: "test-profile",
                staged: false,
                settle: Duration::from_secs(seconds),
            },
            "{agent:?} keeps its own settle in a table with both agents"
        );
    }
    for key in *b"d()z" {
        let keys = [(key, KeyEffect::Commits)];
        let dialogs = [DialogKeys {
            dialog: "update",
            keys: &keys,
        }];
        let facts = AgentFacts {
            key_effects: Measurement::Verified(&dialogs),
            ..complete(CODEX, 2)
        };
        assert_eq!(
            no_go(&verdict(&[facts], &CODEX, "0.164.0")),
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
    let facts = AgentFacts {
        key_effects: Measurement::Verified(PREFIX),
        ..complete(CODEX, 2)
    };
    assert_eq!(
        verdict(&[facts], &CODEX, "0.164.0"),
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
    let facts = AgentFacts {
        key_effects: Measurement::Verified(DIGIT),
        ..complete(CODEX, 2)
    };
    assert_eq!(
        verdict(&[facts], &CODEX, "0.164.0"),
        Activation::Go {
            profile: "test-profile",
            staged: true,
            settle: Duration::from_secs(3),
        }
    );
    assert_eq!(
        verdict(&[complete(CODEX, 2)], &CODEX, "0.164.0"),
        Activation::Go {
            profile: "test-profile",
            staged: false,
            settle: Duration::from_secs(3),
        }
    );
    // 21 s + 50 % = 31.5 s, over the 30 s cap; 20 s + 50 % = 30 s is not.
    assert_eq!(
        no_go(&verdict(&[complete(CODEX, 21)], &CODEX, "0.164.0")),
        Some(NoGo::SettleOverCap)
    );
    assert_eq!(
        verdict(&[complete(CODEX, 20)], &CODEX, "0.164.0"),
        Activation::Go {
            profile: "test-profile",
            staged: false,
            settle: SETTLE_CAP,
        }
    );
    let none = AgentFacts {
        first_modal: Measurement::Verified(FirstModal::NoneObserved { launches: 10 }),
        ..complete(CLAUDE, 0)
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

/// Scenario: Ask about Codex releases nobody measured, near and far from the recorded ones (newer minors at distance 1 to 5, patches, another major, an older release), with complete facts and with incomplete facts. Every latched release gets the agent's facts: Go with complete facts, the facts' own unmet requirement with incomplete ones; with no facts for Codex at all it is agent_not_supported.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_every_release_of_an_agent_gets_its_facts() {
    let facts = [complete(CODEX, 2)];
    for release in [
        "0.164.0", "0.165.0", "0.168.9", "0.169.0", "0.164.1", "0.164.9", "1.0.0", "0.159.9",
        "0.1.0",
    ] {
        assert_eq!(
            verdict(&facts, &CODEX, release),
            Activation::Go {
                profile: "test-profile",
                staged: false,
                settle: Duration::from_secs(3),
            },
            "{release} gets Codex's facts"
        );
    }
    let facts = [incomplete(CODEX, 2)];
    for release in ["0.164.0", "0.165.0", "1.0.0"] {
        assert_eq!(
            no_go(&verdict(&facts, &CODEX, release)),
            Some(NoGo::UnverifiedMeasurement("echo")),
            "{release}"
        );
    }
    assert_eq!(
        no_go(&verdict(&[], &CODEX, "0.164.0")),
        Some(NoGo::AgentNotSupported),
        "no facts"
    );
    assert_eq!(NoGo::AgentNotSupported.clause(), "agent_not_supported");
}

/// Scenario: Ask about Claude Code releases nobody measured (a patch, a minor, a major, an older one) and about agents with no facts, in a table that has Claude Code's facts. Each Claude Code release gets Claude Code's facts and is Go; OpenCode, Pi and Devin are agent_not_supported whatever release they latch.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_claude_releases_get_claudes_facts_and_other_agents_none() {
    let facts = [complete(CLAUDE, 0)];
    for release in ["2.1.310", "2.1.311", "2.2.0", "3.0.0", "2.0.1"] {
        assert!(
            matches!(verdict(&facts, &CLAUDE, release), Activation::Go { .. }),
            "{release}"
        );
    }
    for agent in [AgentType::OpenCode, AgentType::Pi, AgentType::Devin] {
        for latch in [known("2.1.310"), LatchedRelease::Unseen] {
            assert_eq!(
                no_go(&activation(&facts, &agent, &latch)),
                Some(NoGo::AgentNotSupported),
                "{agent:?} has no facts"
            );
        }
    }
}

/// Scenario: Give staged facts (a message-id digit commits in a dialog) each state of the head-echo fact, and unstaged facts no head-echo fact at all. Staged facts are Go only with the head echo verified, held as missing_measurement or unverified_measurement naming head_echo otherwise; unstaged facts are Go without one; and every release of the agent reads the same.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_a_staged_line_needs_its_head_echo_measured() {
    const DIGIT_COMMITS: &[(u8, KeyEffect)] = &[(b'1', KeyEffect::Commits)];
    const DIGIT: &[DialogKeys<'static>] = &[DialogKeys {
        dialog: "update",
        keys: DIGIT_COMMITS,
    }];
    let staged = |head_echo| AgentFacts {
        key_effects: Measurement::Verified(DIGIT),
        head_echo,
        ..complete(CODEX, 2)
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
        (Measurement::Verified(()), None),
    ] {
        for release in ["0.164.0", "0.165.0"] {
            let got = verdict(&[staged(head_echo)], &CODEX, release);
            assert_eq!(no_go(&got), want, "{head_echo:?} {release}");
            if want.is_none() {
                assert!(
                    matches!(got, Activation::Go { staged: true, .. }),
                    "{got:?}"
                );
            }
        }
    }
    // Unstaged: the head is never typed alone, so nothing is needed.
    let unstaged = AgentFacts {
        head_echo: Measurement::Absent,
        ..complete(CODEX, 2)
    };
    assert!(matches!(
        verdict(&[unstaged], &CODEX, "0.164.0"),
        Activation::Go { staged: false, .. }
    ));
}

/// Scenario: For every dialog of the shipped facts and of every recorded release, now and in any later edit, look up each byte the line's first stage can contain: both parentheses and every letter of the token alphabet. Each must be classified, and classified inert: facts or a recorded release in which one of them commits, moves, edits or dismisses, or is left unmeasured, fail this test.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_every_recorded_dialog_leaves_the_token_head_inert() {
    let first_stage: Vec<u8> = first_stage_bytes().collect();
    assert_eq!(first_stage.len(), 18, "two parentheses and sixteen letters");
    let tables = AGENT_FACTS
        .iter()
        .map(|f| (format!("{:?} facts", f.agent), f))
        .chain(
            RECORDED_RELEASES
                .iter()
                .map(|r| (format!("{:?} {}", r.measured.agent, r.release), &r.measured)),
        );
    for (who, facts) in tables {
        let Measurement::Verified(dialogs) = facts.key_effects else {
            continue;
        };
        for dialog in dialogs {
            for key in &first_stage {
                let effect = dialog.effect(*key);
                assert_eq!(
                    effect,
                    Some(KeyEffect::Inert),
                    "{who} `{}`: {:?} in {}",
                    *key as char,
                    effect,
                    dialog.dialog
                );
            }
        }
    }
}

/// Scenario: Ask the table about the facts as they ship, for releases that were recorded and releases nobody measured. Every Claude Code release (2.1.310, 2.1.311, 2.1.317, 2.1.318, 2.1.319, 2.2.0) is Go, unstaged, with no settle window, and the facts' editor probe is the one measured on 2.1.317 and 2.1.318; every Codex release (0.160.0, 0.164.0, 0.166.0, 0.200.1) is Go, staged, with the 6.69 s settle window from the slowest recorded first dialog, through the table and through a generation's verdict alike, and Codex's editor probe is the one measured on 0.164.0; and both agents' facts have their parser cost verified.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_shipped_facts_verdicts() {
    assert_eq!(AGENT_FACTS.len(), 2);
    assert_eq!(AGENT_FACTS[0], CLAUDE_FACTS);
    assert_eq!(AGENT_FACTS[1], CODEX_FACTS);
    for facts in &AGENT_FACTS {
        assert!(matches!(facts.parser_cost, Measurement::Verified(())));
    }
    let claude = Activation::Go {
        profile: "claude-code-2026-10",
        staged: false,
        settle: Duration::ZERO,
    };
    let codex = Activation::Go {
        profile: "codex-2026-10",
        staged: true,
        settle: Duration::from_micros(6_691_500),
    };
    assert_eq!(facts_verdict(&CLAUDE_FACTS), claude);
    assert_eq!(facts_verdict(&CODEX_FACTS), codex);
    let on = DoorbellSwitch::default();
    for (agent, releases, want) in [
        (
            CLAUDE,
            &[
                "2.1.310", "2.1.311", "2.1.317", "2.1.318", "2.1.319", "2.2.0",
            ][..],
            &claude,
        ),
        (
            CODEX,
            &["0.160.0", "0.164.0", "0.166.0", "0.200.1"][..],
            &codex,
        ),
    ] {
        for release in releases {
            assert_eq!(&verdict(&AGENT_FACTS, &agent, release), want, "{release}");
            assert_eq!(
                &generation_activation(&AGENT_FACTS, Some(&agent), &known(release), &on),
                want,
                "{release}"
            );
        }
    }
    use dot_agent_deck::doorbell_activation::{
        CLAUDE_2_1_317_EDITOR_PROBE, CLAUDE_2_1_318_EDITOR_PROBE, CLAUDE_EDITOR_PROBE,
    };
    assert_eq!(CLAUDE_EDITOR_PROBE, CLAUDE_2_1_317_EDITOR_PROBE);
    assert_eq!(CLAUDE_EDITOR_PROBE, CLAUDE_2_1_318_EDITOR_PROBE);
    assert_eq!(
        CLAUDE_FACTS.editor_probe,
        Measurement::Verified(CLAUDE_EDITOR_PROBE)
    );
    use dot_agent_deck::doorbell_activation::{CODEX_0_164_0_EDITOR_PROBE, CODEX_EDITOR_PROBE};
    assert_eq!(CODEX_EDITOR_PROBE, CODEX_0_164_0_EDITOR_PROBE);
    assert_eq!(
        CODEX_FACTS.editor_probe,
        Measurement::Verified(CODEX_EDITOR_PROBE)
    );
}

/// Scenario: Check every recorded release (Claude Code 2.1.310, 2.1.312, 2.1.317, 2.1.318; Codex 0.160.0, 0.164.0) against its agent's shipped facts. Each conforms: nothing measured on it contradicts a fact, its dialogs and keys are all in the facts with the measured effect or a more cautious commit, and its slowest first dialog is no later than the facts'. The recorded set is the one this test knows, so a release added or dropped is noticed.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_every_recorded_release_conforms_to_its_agents_facts() {
    let names: Vec<String> = RECORDED_RELEASES
        .iter()
        .map(|r| format!("{:?}@{}", r.measured.agent, r.release))
        .collect();
    assert_eq!(
        names,
        [
            "Codex@0.164.0",
            "Codex@0.160.0",
            "ClaudeCode@2.1.310",
            "ClaudeCode@2.1.312",
            "ClaudeCode@2.1.317",
            "ClaudeCode@2.1.318",
        ]
    );
    for recorded in &RECORDED_RELEASES {
        assert_eq!(
            conformance(recorded, &AGENT_FACTS),
            Ok(()),
            "{:?} {}",
            recorded.measured.agent,
            recorded.release
        );
    }
}

/// Scenario: Make one of the shipped facts wrong in each way a recorded release can show it: Codex's settle fact faster than 0.160.0's slowest first dialog, a key of Codex's update dialog that commits on the recorded releases called inert, folder trust's `2` taken from 0.164.0 (dismisses) where 0.160.0 commits, a dialog missing from the facts, a key of a recorded dialog missing from the facts, the wrong profile, an editor probe different from the measured one, a recorded release whose echo, key effects or first dialog failed, and an agent with no facts. Each makes conformance fail for the release that contradicts it, with the error naming that contradiction, and the shipped facts pass.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_conformance_fails_when_a_fact_is_wrong() {
    // Fails for the reason the mutation set up, not for another one.
    #[track_caller]
    fn fails_with(result: Result<(), String>, reason: &str) {
        match result {
            Err(e) => assert!(e.contains(reason), "expected {reason:?}, got {e:?}"),
            Ok(()) => panic!("expected {reason:?}, conformance passed"),
        }
    }
    let release = |agent: &AgentType, release: &str| -> &'static RecordedRelease<'static> {
        RECORDED_RELEASES
            .iter()
            .find(|r| r.measured.agent == *agent && r.release == v(release))
            .unwrap()
    };
    let codex_160 = release(&CODEX, "0.160.0");
    let codex_164 = release(&CODEX, "0.164.0");
    let claude_317 = release(&CLAUDE, "2.1.317");
    let with_codex = |codex: AgentFacts<'static>| [CLAUDE_FACTS, codex];
    // Faster than the slowest recorded first dialog (0.164.0's figure alone).
    let fast = with_codex(AgentFacts {
        first_modal: Measurement::Verified(FirstModal::Slowest {
            launches: 36,
            max: Duration::from_millis(4020),
        }),
        ..CODEX_FACTS
    });
    fails_with(conformance(codex_160, &fast), "later than the facts'");
    assert_eq!(conformance(codex_164, &fast), Ok(()));
    // A committing key called inert.
    let update_inert: Vec<(u8, KeyEffect)> = CODEX_FACTS.key_effects.verified().unwrap()[0]
        .keys
        .iter()
        .map(|&(k, e)| {
            if k == b'1' {
                (k, KeyEffect::Inert)
            } else {
                (k, e)
            }
        })
        .collect();
    let update_inert: &'static [(u8, KeyEffect)] = update_inert.leak();
    let mut dialogs = CODEX_FACTS.key_effects.verified().unwrap().to_vec();
    dialogs[0] = DialogKeys {
        dialog: "update",
        keys: update_inert,
    };
    let dialogs: &'static [DialogKeys<'static>] = dialogs.leak();
    let wrong_key = with_codex(AgentFacts {
        key_effects: Measurement::Verified(dialogs),
        ..CODEX_FACTS
    });
    fails_with(
        conformance(codex_160, &wrong_key),
        "key 0x31 in update is Commits on this release, the facts say Inert",
    );
    fails_with(
        conformance(codex_164, &wrong_key),
        "key 0x31 in update is Commits on this release, the facts say Inert",
    );
    // Folder trust from 0.164.0, where 0.160.0's `2` commits.
    let trust_164 = codex_164.measured.key_effects.verified().unwrap()[1];
    assert_eq!(trust_164.dialog, "trust");
    let mut dialogs = CODEX_FACTS.key_effects.verified().unwrap().to_vec();
    dialogs[1] = trust_164;
    let dialogs: &'static [DialogKeys<'static>] = dialogs.leak();
    let less_cautious = with_codex(AgentFacts {
        key_effects: Measurement::Verified(dialogs),
        ..CODEX_FACTS
    });
    fails_with(
        conformance(codex_160, &less_cautious),
        "key 0x32 in trust is Commits on this release, the facts say Dismisses",
    );
    assert_eq!(conformance(codex_164, &less_cautious), Ok(()));
    // A dialog missing from the facts.
    let no_hooks = with_codex(AgentFacts {
        key_effects: Measurement::Verified(&CODEX_FACTS.key_effects.verified().unwrap()[..2]),
        ..CODEX_FACTS
    });
    fails_with(
        conformance(codex_164, &no_hooks),
        "dialog hooks is not in the facts",
    );
    // The wrong profile.
    let profile = with_codex(AgentFacts {
        profile: "codex-2027-01",
        ..CODEX_FACTS
    });
    fails_with(
        conformance(codex_164, &profile),
        "measured with profile codex-2026-10, the facts name codex-2027-01",
    );
    // An editor probe different from the measured one.
    let other_probe = EditorProbe {
        paste_toggles: PasteToggles {
            disables: 0,
            enables: 0,
        },
        ..dot_agent_deck::doorbell_activation::CLAUDE_EDITOR_PROBE
    };
    let claude_wrong = [
        AgentFacts {
            editor_probe: Measurement::Verified(other_probe),
            ..CLAUDE_FACTS
        },
        CODEX_FACTS,
    ];
    fails_with(
        conformance(claude_317, &claude_wrong),
        "editor_probe measured",
    );
    assert_eq!(
        conformance(release(&CLAUDE, "2.1.310"), &claude_wrong),
        Ok(()),
        "2.1.310's editor probe was not measured"
    );
    // A recorded release whose echo failed contradicts the verified fact.
    let failed = RecordedRelease {
        release: v("0.170.0"),
        measured: AgentFacts {
            echo: Measurement::Failed,
            ..codex_164.measured.clone()
        },
    };
    fails_with(
        conformance(&failed, &AGENT_FACTS),
        "echo failed on this release",
    );
    // A release of an agent without facts.
    let pi = RecordedRelease {
        release: v("1.0.0"),
        measured: AgentFacts {
            agent: AgentType::Pi,
            ..codex_164.measured.clone()
        },
    };
    fails_with(conformance(&pi, &AGENT_FACTS), "the agent has no facts");
    // A key a recorded release measured in a dialog, missing from the facts.
    let update_short: Vec<(u8, KeyEffect)> = CODEX_FACTS.key_effects.verified().unwrap()[0]
        .keys
        .iter()
        .copied()
        .filter(|&(k, _)| k != b'1')
        .collect();
    let update_short: &'static [(u8, KeyEffect)] = update_short.leak();
    let mut dialogs = CODEX_FACTS.key_effects.verified().unwrap().to_vec();
    dialogs[0] = DialogKeys {
        dialog: "update",
        keys: update_short,
    };
    let dialogs: &'static [DialogKeys<'static>] = dialogs.leak();
    let missing_key = with_codex(AgentFacts {
        key_effects: Measurement::Verified(dialogs),
        ..CODEX_FACTS
    });
    fails_with(
        conformance(codex_164, &missing_key),
        "key 0x31 in update is not in the facts",
    );
    // A recorded release whose key effects, or whose first dialog, failed.
    let keys_failed = RecordedRelease {
        release: v("0.170.0"),
        measured: AgentFacts {
            key_effects: Measurement::Failed,
            ..codex_164.measured.clone()
        },
    };
    fails_with(
        conformance(&keys_failed, &AGENT_FACTS),
        "key_effects failed on this release",
    );
    let first_modal_failed = RecordedRelease {
        release: v("2.1.399"),
        measured: AgentFacts {
            first_modal: Measurement::Failed,
            ..claude_317.measured.clone()
        },
    };
    fails_with(
        conformance(&first_modal_failed, &AGENT_FACTS),
        "first_modal failed on this release",
    );
}

/// Scenario: Read the editor record measured for Claude Code 2.1.317. It names Claude's export and editor selection, one paste-mode disable and one enable, the user's key-binding file as the only source, the bindings loaded at start, no inert candidate form, and the pasted BEL left absent; it is the recorded release's measurement and Claude Code's editor-probe fact, so any latched Claude Code release, 2.1.317 or one nobody measured, is armable once its root is bound.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_measured_2_1_317_editor_record() {
    use dot_agent_deck::composer_probe::{Binding, Permission, permission};
    use dot_agent_deck::doorbell_activation::CLAUDE_2_1_317_EDITOR_PROBE;
    assert_eq!(
        CLAUDE_2_1_317_EDITOR_PROBE,
        EditorProbe {
            export: ExportShape::ClaudeTmpdir,
            selection: EditorSelection::ClaudeCode,
            paste_toggles: PasteToggles {
                disables: 1,
                enables: 1,
            },
            keymap_sources: &[KeymapSource::ClaudeKeybindings],
            keymap_loaded_at_start: Measurement::Verified(()),
            inert_candidates: &[],
            pasted_bel_is_text: Measurement::Absent,
        }
    );
    let recorded = RECORDED_RELEASES
        .iter()
        .find(|r| r.measured.agent == CLAUDE && r.release == v("2.1.317"))
        .unwrap();
    assert_eq!(
        recorded.measured.editor_probe,
        Measurement::Verified(CLAUDE_2_1_317_EDITOR_PROBE)
    );
    for release in ["2.1.317", "2.1.320"] {
        let header = known(release);
        assert_eq!(
            permission(&AGENT_FACTS, &[], &CLAUDE, Some(&header), Binding::Bound),
            Permission::Armable,
            "{release}"
        );
    }
}

/// Scenario: Read the editor record measured for Claude Code 2.1.318. It holds the same values as 2.1.317's: Claude's export and editor selection, one paste-mode disable and one enable, the user's key-binding file as the only source, the bindings loaded at start, no inert candidate form, and the pasted BEL left absent; it is the recorded release's measurement and equals Claude Code's editor-probe fact, so 2.1.318 is armable once its root is bound.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_measured_2_1_318_editor_record() {
    use dot_agent_deck::composer_probe::{Binding, Permission, permission};
    use dot_agent_deck::doorbell_activation::{
        CLAUDE_2_1_317_EDITOR_PROBE, CLAUDE_2_1_318_EDITOR_PROBE,
    };
    assert_eq!(
        CLAUDE_2_1_318_EDITOR_PROBE,
        EditorProbe {
            export: ExportShape::ClaudeTmpdir,
            selection: EditorSelection::ClaudeCode,
            paste_toggles: PasteToggles {
                disables: 1,
                enables: 1,
            },
            keymap_sources: &[KeymapSource::ClaudeKeybindings],
            keymap_loaded_at_start: Measurement::Verified(()),
            inert_candidates: &[],
            pasted_bel_is_text: Measurement::Absent,
        }
    );
    assert_eq!(CLAUDE_2_1_318_EDITOR_PROBE, CLAUDE_2_1_317_EDITOR_PROBE);
    let recorded = RECORDED_RELEASES
        .iter()
        .find(|r| r.measured.agent == CLAUDE && r.release == v("2.1.318"))
        .unwrap();
    assert_eq!(
        recorded.measured.editor_probe,
        Measurement::Verified(CLAUDE_2_1_318_EDITOR_PROBE)
    );
    let header = known("2.1.318");
    assert_eq!(
        permission(&AGENT_FACTS, &[], &CLAUDE, Some(&header), Binding::Bound),
        Permission::Armable
    );
}

/// Scenario: Read the editor record measured for Codex 0.164.0. It names Codex's `editor/` export and editor selection, two paste-mode disables and one enable, Codex's four key-binding sources in precedence order, no bindings-loaded-at-start premise, no inert candidate form, and the pasted BEL left absent; it is the recorded release's measurement and Codex's editor-probe fact, while 0.160.0's record has none, so 0.160.0, 0.164.0 and a release nobody measured are all armable once their root is bound, and nothing is armable before its header latches a release.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_measured_0_164_0_editor_record() {
    use dot_agent_deck::composer_probe::{Binding, InertCause, Permission, permission};
    use dot_agent_deck::doorbell_activation::CODEX_0_164_0_EDITOR_PROBE;
    assert_eq!(
        CODEX_0_164_0_EDITOR_PROBE,
        EditorProbe {
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
        }
    );
    let record = |release: &str| {
        RECORDED_RELEASES
            .iter()
            .find(|r| r.measured.agent == CODEX && r.release == v(release))
            .unwrap()
            .measured
            .editor_probe
    };
    assert_eq!(
        record("0.164.0"),
        Measurement::Verified(CODEX_0_164_0_EDITOR_PROBE)
    );
    assert_eq!(record("0.160.0"), Measurement::Absent);
    for release in ["0.160.0", "0.164.0", "0.166.0"] {
        let header = known(release);
        assert_eq!(
            permission(&AGENT_FACTS, &[], &CODEX, Some(&header), Binding::Bound),
            Permission::Armable,
            "{release}"
        );
        assert_eq!(
            permission(&AGENT_FACTS, &[], &CODEX, Some(&header), Binding::Waiting),
            Permission::Pending,
            "{release}"
        );
    }
    assert_eq!(
        permission(&AGENT_FACTS, &[], &CODEX, None, Binding::Bound),
        Permission::Pending
    );
    assert_eq!(
        permission(
            &AGENT_FACTS,
            &[],
            &CODEX,
            Some(&LatchedRelease::Unseen),
            Binding::Bound
        ),
        Permission::Inert(InertCause::UnknownRelease)
    );
}

/// A verified editor record, as a stand-in's would be.
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
const EDITOR_STATES: [Measurement<EditorProbe<'static>>; 4] = [
    Measurement::Absent,
    Measurement::Unverified,
    Measurement::Failed,
    Measurement::Verified(PROBE),
];

/// Scenario: Give each kind of facts the table decides (Go, Go and staged, each required fact absent, unverified or failed, a committing first-stage key, a settle over the cap, staged without the head echo) every editor-record state, and ask the table and a generation's verdict about a latched release. Each verdict is the one the facts have with no editor record: a missing one turns no Go off, and a verified one turns no NoGo on.
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
    let mut all = vec![
        complete(CODEX, 2),
        AgentFacts {
            key_effects: Measurement::Verified(DIGIT_COMMITS),
            ..complete(CODEX, 2)
        },
        AgentFacts {
            key_effects: Measurement::Verified(DIGIT_COMMITS),
            head_echo: Measurement::Absent,
            ..complete(CODEX, 2)
        },
        AgentFacts {
            key_effects: Measurement::Verified(PAREN_COMMITS),
            ..complete(CODEX, 2)
        },
        complete(CODEX, 21),
        complete(CLAUDE, 0),
    ];
    for field in 0..REQUIRED.len() {
        for state in [State::Absent, State::Unverified, State::Failed] {
            let mut facts = complete(CODEX, 2);
            set(&mut facts, field, state);
            all.push(facts);
        }
    }
    let on = DoorbellSwitch::default();
    let release = known("0.170.0");
    let mut go = 0;
    let mut held = 0;
    for base in &all {
        let want = facts_verdict(base);
        match want {
            Activation::NoGo(_) => held += 1,
            _ => go += 1,
        }
        for editor in EDITOR_STATES {
            let facts = AgentFacts {
                editor_probe: editor,
                ..base.clone()
            };
            assert_eq!(facts_verdict(&facts), want, "{editor:?}");
            let one = std::slice::from_ref(&facts);
            assert_eq!(activation(one, &facts.agent, &release), want);
            assert_eq!(
                generation_activation(one, Some(&facts.agent), &release, &on),
                want,
                "{editor:?}"
            );
        }
    }
    assert_eq!((go, held), (3, 3 + 3 * REQUIRED.len()));
}

/// Scenario: Enumerate small facts tables (Codex and Claude Code each complete, incomplete, slow or absent) and, for each, every assignment of a verified or missing editor record to its facts, then ask the table about live releases of both agents. Every verdict equals the one with no editor record anywhere.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_editor_records_move_no_verdict_in_any_table() {
    #[derive(Clone, Copy)]
    enum R {
        Absent,
        Complete(u64),
        Incomplete(u64),
    }
    let states = [R::Absent, R::Complete(2), R::Complete(21), R::Incomplete(2)];
    let live: Vec<(AgentType, &str)> = ["0.159.0", "0.160.0", "0.164.0", "0.169.0"]
        .iter()
        .map(|r| (CODEX, *r))
        .chain(["2.1.300", "2.1.310", "2.2.0"].iter().map(|r| (CLAUDE, *r)))
        .collect();
    let mut tables = 0;
    for table in 0..states.len().pow(2) {
        let mut facts = Vec::new();
        let mut n = table;
        for agent in [CODEX, CLAUDE] {
            match states[n % states.len()] {
                R::Absent => {}
                R::Complete(t) => facts.push(complete(agent.clone(), t)),
                R::Incomplete(t) => facts.push(incomplete(agent.clone(), t)),
            }
            n /= states.len();
        }
        let intact: Vec<Activation<'_>> = live
            .iter()
            .map(|(agent, r)| verdict(&facts, agent, r))
            .collect();
        for mask in 1..(1u32 << facts.len()) {
            let with: Vec<AgentFacts<'_>> = facts
                .iter()
                .enumerate()
                .map(|(i, f)| AgentFacts {
                    editor_probe: if mask & (1 << i) != 0 {
                        Measurement::Verified(PROBE)
                    } else {
                        Measurement::Absent
                    },
                    ..f.clone()
                })
                .collect();
            for ((agent, r), before) in live.iter().zip(&intact) {
                assert_eq!(&verdict(&with, agent, r), before, "{r} mask {mask:#b}");
            }
        }
        tables += 1;
    }
    assert_eq!(tables, 16);
}

/// Scenario: Apply the e2e assumption, which gives the stand-ins' editor probe to exact agent@release pairs (only Claude Code's since the Codex activation). A generation that latched a named release gets the assumed probe; another release of the same agent, and the other agent, keep their own facts' probe (Claude Code's measured one, Codex's measured one); and no verdict of the ring moves, since the ring reads no editor probe.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_editor_probe_assumption_moves_no_shipped_verdict() {
    use dot_agent_deck::doorbell_activation::{CLAUDE_EDITOR_PROBE, CODEX_EDITOR_PROBE};
    let assumed = [(CLAUDE, v("2.1.310"), PROBE)];
    assert_eq!(
        editor_probe_for(&AGENT_FACTS, &assumed, &CLAUDE, &v("2.1.310")),
        Measurement::Verified(PROBE)
    );
    assert_eq!(
        editor_probe_for(&AGENT_FACTS, &assumed, &CLAUDE, &v("2.1.311")),
        Measurement::Verified(CLAUDE_EDITOR_PROBE)
    );
    for release in ["0.160.0", "0.164.0", "0.165.0"] {
        assert_eq!(
            editor_probe_for(&AGENT_FACTS, &assumed, &CODEX, &v(release)),
            Measurement::Verified(CODEX_EDITOR_PROBE),
            "{release}"
        );
    }
    assert_eq!(
        editor_probe_for(&AGENT_FACTS, &assumed, &AgentType::Pi, &v("2.1.310")),
        Measurement::Absent
    );
    assert_eq!(
        editor_probe_for(&AGENT_FACTS, &[], &CLAUDE, &v("2.1.310")),
        Measurement::Verified(CLAUDE_EDITOR_PROBE)
    );
}
