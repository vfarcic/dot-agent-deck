//! `inbox/doorbell/053`: the composer probe's permission to arm, from the
//! agent's `editor_probe` facts, and the e2e assumption that stands in for
//! them on a named release (`src/composer_probe/mod.rs`,
//! `src/doorbell_activation.rs`).
#[cfg(all(not(feature = "e2e"), target_os = "linux"))]
mod common;
#[cfg(all(not(feature = "e2e"), target_os = "linux"))]
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[cfg(all(not(feature = "e2e"), target_os = "linux"))]
#[path = "support/editor_installation.rs"]
mod installation;

use dot_agent_deck::composer_probe::{Binding, InertCause, Permission, permission};
use dot_agent_deck::doorbell_activation::{
    AGENT_FACTS, AgentFacts, CLAUDE_FACTS, CODEX_FACTS, EditorProbe, EditorSelection, ExportShape,
    KeymapSource, LatchedRelease, Measurement, PasteToggles, RECORDED_RELEASES,
    assumed_editor_probes, parse_assumed_editor_probes,
};
use dot_agent_deck::event::AgentType;
use semver::Version;
use spec::spec;

const CODEX: AgentType = AgentType::Codex;
const CLAUDE: AgentType = AgentType::ClaudeCode;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

const PROBE: EditorProbe<'static> = EditorProbe {
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
};

/// The shipped facts with Claude Code's editor probe set to `PROBE`.
fn claude_verified() -> Vec<AgentFacts<'static>> {
    vec![
        AgentFacts {
            editor_probe: Measurement::Verified(PROBE),
            ..CLAUDE_FACTS
        },
        CODEX_FACTS,
    ]
}

/// Scenario: Ask for the permission to arm on Claude Code generations whose header has not arrived, latched nothing, latched two releases or had its scanner dropped, or latched a release (one recorded, a patch past it, an older one, a number from another agent's range), with Claude Code's editor-probe fact verified and in every other state, each while the agent root is still being looked for, bound, or never to be bound; and on agents with no facts or no verified editor probe. Any latched release with the fact verified and the root bound is armable; a header that latched nothing, a fact not verified, an agent with no facts, or a root never bound settles the generation as inert at once, and anything else waits.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_permission_needs_the_header_a_verified_record_and_the_root() {
    use Binding::{Bound, Never, Waiting};
    use InertCause::{NoEditorRecord, Unbound, UnknownRelease, UnsupportedAgent};
    use Permission::{Armable, Inert, Pending};
    let records = claude_verified();
    let known = |r: &str| Some(LatchedRelease::Known(v(r)));
    let cases: Vec<(&str, Option<LatchedRelease>, [Permission; 3])> = vec![
        ("no header yet", None, [Pending, Pending, Inert(Unbound)]),
        (
            "unseen",
            Some(LatchedRelease::Unseen),
            [Inert(UnknownRelease); 3],
        ),
        (
            "conflict",
            Some(LatchedRelease::Conflict),
            [Inert(UnknownRelease); 3],
        ),
        (
            "geometry",
            Some(LatchedRelease::ScannerGeometry),
            [Inert(UnknownRelease); 3],
        ),
        (
            "budget",
            Some(LatchedRelease::ScannerBudget),
            [Inert(UnknownRelease); 3],
        ),
        (
            "a recorded release",
            known("2.1.310"),
            [Pending, Armable, Inert(Unbound)],
        ),
        (
            "a patch past it",
            known("2.1.311"),
            [Pending, Armable, Inert(Unbound)],
        ),
        (
            "older",
            known("2.1.300"),
            [Pending, Armable, Inert(Unbound)],
        ),
        (
            "Codex's release number",
            known("0.164.0"),
            [Pending, Armable, Inert(Unbound)],
        ),
    ];
    for (name, header, want) in &cases {
        for (binding, want) in [Waiting, Bound, Never].into_iter().zip(want) {
            assert_eq!(
                permission(&records, &[], &CLAUDE, header.as_ref(), binding),
                *want,
                "{name}, {binding:?}"
            );
        }
    }
    // Facts without a verified editor probe, in every state but verified.
    for editor in [
        Measurement::Absent,
        Measurement::Unverified,
        Measurement::Failed,
    ] {
        let records: Vec<_> = records
            .iter()
            .map(|r| AgentFacts {
                editor_probe: editor,
                ..r.clone()
            })
            .collect();
        for binding in [Waiting, Bound, Never] {
            for release in ["2.1.310", "2.1.311"] {
                assert_eq!(
                    permission(&records, &[], &CLAUDE, known(release).as_ref(), binding),
                    Inert(NoEditorRecord),
                    "{editor:?} {binding:?} {release}"
                );
            }
        }
    }
    // The probe must be the generation's own agent's: Codex's facts have none.
    assert_eq!(
        permission(&records, &[], &CODEX, known("2.1.310").as_ref(), Bound),
        Inert(NoEditorRecord)
    );
    // An agent with no facts.
    for agent in [AgentType::OpenCode, AgentType::Pi, AgentType::Devin] {
        assert_eq!(
            permission(&records, &[], &agent, known("2.1.310").as_ref(), Bound),
            Inert(UnsupportedAgent),
            "{agent:?}"
        );
    }
    // Incomplete ring facts decide nothing here: the editor probe does.
    let incomplete: Vec<_> = records
        .iter()
        .map(|r| AgentFacts {
            echo: Measurement::Unverified,
            head_echo: Measurement::Absent,
            ..r.clone()
        })
        .collect();
    assert_eq!(
        permission(&incomplete, &[], &CLAUDE, known("2.1.310").as_ref(), Bound),
        Armable
    );
}

/// Scenario: Ask for the permission to arm on every recorded release and on releases nobody measured, with the facts as shipped and with no e2e assumption read. Every Claude Code release, whose editor probe the Claude activation verified, can arm: each waits while its root is looked for, is armable once the root is bound, and is inert when it is never bound. Every Codex release is inert for its missing editor probe, however its root is bound.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_only_the_activated_release_is_armable() {
    assert!(assumed_editor_probes().is_empty());
    let releases: Vec<(AgentType, Version)> = RECORDED_RELEASES
        .iter()
        .map(|r| (r.measured.agent.clone(), r.release.clone()))
        .chain([(CLAUDE, v("2.1.320")), (CODEX, v("0.170.0"))])
        .collect();
    for (agent, release) in &releases {
        let header = LatchedRelease::Known(release.clone());
        let activated = *agent == CLAUDE;
        for binding in [Binding::Waiting, Binding::Bound, Binding::Never] {
            let expected = match (activated, &binding) {
                (false, _) => Permission::Inert(InertCause::NoEditorRecord),
                (true, Binding::Waiting) => Permission::Pending,
                (true, Binding::Bound) => Permission::Armable,
                (true, Binding::Never) => Permission::Inert(InertCause::Unbound),
            };
            assert_eq!(
                permission(&AGENT_FACTS, &[], agent, Some(&header), binding),
                expected,
                "{agent:?} {release}"
            );
        }
    }
}

/// Scenario: Run this test again in separate subprocesses of a build without the e2e feature, with valid and malformed editor-probe assumption variables set. The production reader ignores both values: every Claude Code release can arm on Claude Code's verified editor-probe fact, and every Codex release, those the variable names among them, stays unable to arm. On Linux, the actual production daemon also starts synthetic commands: a direct Claude Code gets the installation the activation rolled out (the deck's helper as its editor and a tool environment file), every other command keeps its original editor environment, whatever the variable names, and every complete argv survives unchanged.
#[spec("inbox/probe/012")]
#[test]
#[cfg(not(feature = "e2e"))]
fn probe_012_production_reader_ignores_set_editor_assumption() {
    const ASSUME: &str = "DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE";
    const CHILD: &str = "DOT_AGENT_DECK_PRODUCTION_READER_CHILD";
    if let Some(expected) = std::env::var_os(CHILD) {
        assert_eq!(std::env::var_os(ASSUME), Some(expected));
        assert!(
            assumed_editor_probes().is_empty(),
            "a set e2e variable must not supply production editor records"
        );
        for recorded in &RECORDED_RELEASES {
            let agent = &recorded.measured.agent;
            let header = LatchedRelease::Known(recorded.release.clone());
            let expected = if *agent == CLAUDE {
                Permission::Armable
            } else {
                Permission::Inert(InertCause::NoEditorRecord)
            };
            assert_eq!(
                permission(
                    &AGENT_FACTS,
                    &assumed_editor_probes(),
                    agent,
                    Some(&header),
                    Binding::Bound
                ),
                expected,
                "{agent:?} {}: the production build's own facts decide, not the variable",
                recorded.release
            );
        }
        #[cfg(target_os = "linux")]
        {
            use serde_json::{Value, json};
            let value = std::env::var(ASSUME).unwrap();
            let daemon = common::spawn_daemon_serve_with_env(None, "0", &[(ASSUME, &value)]);
            for kind in ["claude", "codex", "opencode", "pi", "wrapper"] {
                for (state, original) in [
                    ("unset", None),
                    ("empty", Some("")),
                    ("set", Some("user-editor --literal")),
                ] {
                    let name = format!("production-{kind}-{state}");
                    let release = if kind == "codex" {
                        "0.164.0"
                    } else {
                        "2.1.310"
                    };
                    let spawned =
                        installation::spawn(&daemon, &name, kind, release, original, None, false);
                    if kind == "claude" {
                        // The Claude activation's rollout, not the variable.
                        let helper = installation::installed(&spawned);
                        for variable in ["VISUAL", "EDITOR"] {
                            assert_eq!(
                                spawned.report["env"][variable],
                                json!(helper.display().to_string()),
                                "{name}: {variable}"
                            );
                        }
                        assert!(
                            spawned.report["env"]["CLAUDE_ENV_FILE"].is_string(),
                            "{name}"
                        );
                    } else {
                        assert_eq!(spawned.report["env"]["VISUAL"], json!(original), "{name}");
                        assert_eq!(spawned.report["env"]["EDITOR"], json!(original), "{name}");
                        assert_eq!(
                            spawned.report["env"]["CLAUDE_ENV_FILE"],
                            Value::Null,
                            "{name}"
                        );
                    }
                    assert_eq!(spawned.report["argv"], spawned.original_argv, "{name}");
                }
            }
        }
        return;
    }
    for value in ["claude@2.1.310,codex@0.164.0", "broken"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "probe_012_production_reader_ignores_set_editor_assumption",
                "--nocapture",
            ])
            .env(ASSUME, value)
            .env(CHILD, value)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "production reader with {value:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Scenario: Parse e2e assumption lists naming `agent@release` entries, a release nobody recorded and a number from another agent's range among them, and lists with an entry that is not one: another agent, the agent's serialized name, a pre-release or build, a missing agent or release, an empty entry, an entry named twice. A list of valid entries gives exactly those pairs; any other entry refuses the whole list, also next to valid ones.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_editor_probe_assumption_refuses_unknown_entries() {
    let parse = parse_assumed_editor_probes;
    assert_eq!(parse(""), Ok(Vec::new()));
    assert_eq!(parse("  "), Ok(Vec::new()));
    assert_eq!(
        parse("claude@2.1.310, codex@0.164.0,codex@0.160.0"),
        Ok(vec![
            (CLAUDE, v("2.1.310")),
            (CODEX, v("0.164.0")),
            (CODEX, v("0.160.0")),
        ])
    );
    assert_eq!(
        parse("claude@2.1.399,codex@2.1.310,claude@0.164.0"),
        Ok(vec![
            (CLAUDE, v("2.1.399")),
            (CODEX, v("2.1.310")),
            (CLAUDE, v("0.164.0")),
        ])
    );
    for unknown in [
        "opencode@2.1.310",
        "claude_code@2.1.310",
        "Claude@2.1.310",
        "claude@2.1.310-beta.1",
        "claude@2.1.310+build",
        "claude@2.1",
        "claude",
        "2.1.310",
        "@2.1.310",
        "claude@",
        "claude@2.1.310,",
        "claude@2.1.310,,codex@0.164.0",
        "claude@2.1.310,claude@2.1.310",
    ] {
        assert!(parse(unknown).is_err(), "{unknown:?} was accepted");
        let mixed = format!("codex@0.164.0,{unknown}");
        assert!(parse(&mixed).is_err(), "{mixed:?} was accepted");
    }
}

/// Scenario: Give Codex 0.164.0 the e2e assumption's verified editor probe (Codex's own facts have none), then ask for the permission to arm on Codex generations whose header has not arrived, latched nothing, latched another release, or latched 0.164.0. The assumption supplies a probe, never a release: only the generation that latched 0.164.0 itself, with its root bound, is armable; Claude Code keeps its own facts' probe.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_editor_probe_assumption_never_supplies_a_release() {
    let named = parse_assumed_editor_probes("codex@0.164.0").unwrap();
    let assumed: Vec<_> = named.into_iter().map(|(a, r)| (a, r, PROBE)).collect();
    let facts = &AGENT_FACTS;
    assert_eq!(
        permission(facts, &assumed, &CODEX, None, Binding::Bound),
        Permission::Pending
    );
    for latch in [
        LatchedRelease::Unseen,
        LatchedRelease::Conflict,
        LatchedRelease::ScannerGeometry,
        LatchedRelease::ScannerBudget,
    ] {
        assert_eq!(
            permission(facts, &assumed, &CODEX, Some(&latch), Binding::Bound),
            Permission::Inert(InertCause::UnknownRelease),
            "{latch:?}"
        );
    }
    for other in ["0.165.0", "0.160.0", "1.0.0"] {
        assert_eq!(
            permission(
                facts,
                &assumed,
                &CODEX,
                Some(&LatchedRelease::Known(v(other))),
                Binding::Bound
            ),
            Permission::Inert(InertCause::NoEditorRecord),
            "{other}"
        );
    }
    // Claude Code is armable on its own facts, named or not.
    assert_eq!(
        permission(
            facts,
            &assumed,
            &CLAUDE,
            Some(&LatchedRelease::Known(v("0.164.0"))),
            Binding::Bound
        ),
        Permission::Armable
    );
    let own = LatchedRelease::Known(v("0.164.0"));
    assert_eq!(
        permission(facts, &assumed, &CODEX, Some(&own), Binding::Waiting),
        Permission::Pending
    );
    assert_eq!(
        permission(facts, &assumed, &CODEX, Some(&own), Binding::Bound),
        Permission::Armable
    );
}
