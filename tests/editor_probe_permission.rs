//! `inbox/doorbell/053`: the composer probe's permission to arm, from a
//! release's `editor_probe` record, and the e2e assumption that stands in for
//! one (`src/composer_probe/mod.rs`, `src/doorbell_activation.rs`).
use dot_agent_deck::composer_probe::{Binding, InertCause, Permission, permission};
use dot_agent_deck::doorbell_activation::{
    EditorProbe, EditorSelection, ExportShape, KeymapSource, LatchedRelease, Measurement,
    PasteToggles, RELEASE_RECORDS, ReleaseRecord, assumed_editor_probes,
    parse_assumed_editor_probes, with_assumed_editor_probes,
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

/// The shipped records with Claude Code 2.1.310's editor record verified.
fn claude_verified() -> Vec<ReleaseRecord<'static>> {
    with_assumed_editor_probes(RELEASE_RECORDS.to_vec(), &[(CLAUDE, v("2.1.310"), PROBE)])
}

/// Scenario: Ask for the permission to arm on generations whose header has not arrived, latched nothing, latched two releases or had its scanner dropped, latched a release with a verified editor record, one whose record has none, and one with no record at all, each while the agent root is still being looked for, bound, or never to be bound. Only a verified exact record with the root bound is armable; a release that cannot permit, or a root never bound, settles the generation as inert at once, and anything else waits.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_permission_needs_the_header_a_verified_record_and_the_root() {
    use Binding::{Bound, Never, Waiting};
    use InertCause::{NoEditorRecord, Unbound, UnknownRelease, UnmeasuredRelease};
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
            "verified record",
            known("2.1.310"),
            [Pending, Armable, Inert(Unbound)],
        ),
        (
            "unmeasured, a patch past it",
            known("2.1.311"),
            [Inert(UnmeasuredRelease); 3],
        ),
        (
            "unmeasured, older",
            known("2.1.300"),
            [Inert(UnmeasuredRelease); 3],
        ),
        (
            "unmeasured, Codex's release number",
            known("0.164.0"),
            [Inert(UnmeasuredRelease); 3],
        ),
    ];
    for (name, header, want) in &cases {
        for (binding, want) in [Waiting, Bound, Never].into_iter().zip(want) {
            assert_eq!(
                permission(&records, &CLAUDE, header.as_ref(), binding),
                *want,
                "{name}, {binding:?}"
            );
        }
    }
    // A record without a verified editor record, in every state but verified.
    for editor in [
        Measurement::Absent,
        Measurement::Unverified,
        Measurement::Failed,
        Measurement::Unraisable,
    ] {
        let records: Vec<_> = records
            .iter()
            .map(|r| ReleaseRecord {
                editor_probe: editor,
                ..r.clone()
            })
            .collect();
        for binding in [Waiting, Bound, Never] {
            assert_eq!(
                permission(&records, &CLAUDE, known("2.1.310").as_ref(), binding),
                Inert(NoEditorRecord),
                "{editor:?} {binding:?}"
            );
        }
    }
    // The record must be the generation's own agent's.
    assert_eq!(
        permission(&records, &CODEX, known("2.1.310").as_ref(), Bound),
        Inert(UnmeasuredRelease)
    );
    // An incomplete ring record decides nothing here: the editor record does.
    let incomplete: Vec<_> = records
        .iter()
        .map(|r| ReleaseRecord {
            echo: Measurement::Unverified,
            head_echo: Measurement::Absent,
            ..r.clone()
        })
        .collect();
    assert_eq!(
        permission(&incomplete, &CLAUDE, known("2.1.310").as_ref(), Bound),
        Armable
    );
}

/// Scenario: Ask for the permission to arm on every release the deck ships a record for, with the records as shipped and with no e2e assumption read. None of them is armable: each is inert for its missing editor record, however its root is bound.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_no_shipped_release_is_armable() {
    assert!(assumed_editor_probes().is_empty());
    for record in &RELEASE_RECORDS {
        let header = LatchedRelease::Known(record.release.clone());
        for binding in [Binding::Waiting, Binding::Bound, Binding::Never] {
            assert_eq!(
                permission(&RELEASE_RECORDS, &record.agent, Some(&header), binding),
                Permission::Inert(InertCause::NoEditorRecord),
                "{:?} {}",
                record.agent,
                record.release
            );
        }
    }
}

/// Scenario: Parse e2e assumption lists naming recorded `agent@release` entries, and lists with an entry that is unknown: another agent, the agent's serialized name, a release with no record, another agent's release, a pre-release or build, a missing agent or release, an empty entry, an entry named twice. A list of known entries gives exactly those pairs; any unknown entry refuses the whole list, also next to known ones.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_editor_probe_assumption_refuses_unknown_entries() {
    let parse = |list: &str| parse_assumed_editor_probes(list, &RELEASE_RECORDS);
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
    for unknown in [
        "opencode@2.1.310",
        "claude_code@2.1.310",
        "Claude@2.1.310",
        "claude@2.1.311",
        "claude@2.1.300",
        "codex@0.165.0",
        "codex@2.1.310",
        "claude@0.164.0",
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

/// Scenario: Give Claude Code 2.1.310 the e2e assumption's verified editor record, then ask for the permission to arm on Claude Code generations whose header has not arrived, latched nothing, latched another release, or latched 2.1.310. The assumption supplies a record, never a release: only the generation that latched 2.1.310 itself, with its root bound, is armable.
#[spec("inbox/doorbell/053")]
#[test]
fn doorbell_053_the_editor_probe_assumption_never_supplies_a_release() {
    let named = parse_assumed_editor_probes("claude@2.1.310", &RELEASE_RECORDS).unwrap();
    let assumed: Vec<_> = named.into_iter().map(|(a, r)| (a, r, PROBE)).collect();
    let records = with_assumed_editor_probes(RELEASE_RECORDS.to_vec(), &assumed);
    assert_eq!(
        permission(&records, &CLAUDE, None, Binding::Bound),
        Permission::Pending
    );
    for latch in [
        LatchedRelease::Unseen,
        LatchedRelease::Conflict,
        LatchedRelease::ScannerGeometry,
        LatchedRelease::ScannerBudget,
    ] {
        assert_eq!(
            permission(&records, &CLAUDE, Some(&latch), Binding::Bound),
            Permission::Inert(InertCause::UnknownRelease),
            "{latch:?}"
        );
    }
    for other in ["2.1.311", "2.1.309", "3.0.0"] {
        assert_eq!(
            permission(
                &records,
                &CLAUDE,
                Some(&LatchedRelease::Known(v(other))),
                Binding::Bound
            ),
            Permission::Inert(InertCause::UnmeasuredRelease),
            "{other}"
        );
    }
    // The releases the assumption did not name keep their own record.
    for codex in ["0.164.0", "0.160.0"] {
        assert_eq!(
            permission(
                &records,
                &CODEX,
                Some(&LatchedRelease::Known(v(codex))),
                Binding::Bound
            ),
            Permission::Inert(InertCause::NoEditorRecord)
        );
    }
    let own = LatchedRelease::Known(v("2.1.310"));
    assert_eq!(
        permission(&records, &CLAUDE, Some(&own), Binding::Waiting),
        Permission::Pending
    );
    assert_eq!(
        permission(&records, &CLAUDE, Some(&own), Binding::Bound),
        Permission::Armable
    );
}
