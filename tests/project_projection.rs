//! PRD #819 M2: shape tests for the project projection the attach socket's
//! three project verbs carry.
//!
//! These are contract tests, not behaviour tests. They exist because the
//! projection is the thing a future change is most likely to break silently:
//! widening [`ProjectRole`] leaks daemon-side command strings and prompt
//! templates onto the wire, and dropping a `#[serde(default)]` turns an
//! additive response field into a break that only shows up against a peer
//! nobody has locally.
//!
//! Not `#![cfg(unix)]`, unlike `tests/daemon_protocol.rs`: nothing here binds a
//! socket, so the assertions hold and run on every platform.

use std::marker::PhantomData;

use dot_agent_deck::daemon_protocol::AttachResponse;
use dot_agent_deck::event::{
    KnownProject, PreparedWorkflow, ProjectListing, ProjectOrchestration, ProjectRole,
    ResolvedProject,
};
use serde::Serialize;

fn sample_role() -> ProjectRole {
    ProjectRole {
        name: "coder".into(),
        start: true,
    }
}

fn sample_resolved() -> ResolvedProject {
    ResolvedProject {
        path: "/home/dev/project".into(),
        // PRD #819 M4: the revision the client echoes back on `PrepareWorkflow`.
        config_revision: Some("fnv1a128-0123456789abcdef0123456789abcdef".into()),
        orchestrations: vec![ProjectOrchestration {
            name: "dispatch".into(),
            default: true,
            roles: vec![
                sample_role(),
                ProjectRole {
                    name: "tester".into(),
                    start: false,
                },
            ],
        }],
    }
}

/// The complete set of keys [`ProjectRole`] may put on the wire.
const PROJECT_ROLE_KEYS: [&str; 2] = ["name", "start"];

/// Fields of `OrchestrationRoleConfig` that must NOT reach a client. Each is
/// consumed only inside `orchestrator_context::prepare_orchestrator_prompt`,
/// which moves daemon-side in M4 — so a client that needs one of these is a
/// client doing work the daemon should be doing.
const PROJECT_ROLE_FORBIDDEN_KEYS: [&str; 5] = [
    "command",
    "description",
    "prompt_template",
    "agent",
    "clear",
];

#[test]
fn project_role_carries_name_and_start_and_nothing_else() {
    let value = serde_json::to_value(sample_role()).expect("ProjectRole must serialize");
    let object = value.as_object().expect("ProjectRole is a JSON object");

    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = PROJECT_ROLE_KEYS;
    expected.sort_unstable();
    assert_eq!(
        keys,
        expected.to_vec(),
        "ProjectRole's wire shape changed; widening it puts daemon-side command \
         strings or prompt templates on the wire (PRD #819)"
    );

    for forbidden in PROJECT_ROLE_FORBIDDEN_KEYS {
        assert!(
            !object.contains_key(forbidden),
            "`{forbidden}` must never cross the wire: it is consumed only inside \
             prepare_orchestrator_prompt, which is daemon-side"
        );
    }
}

#[test]
fn projection_types_round_trip_through_serde() {
    let listing = ProjectListing {
        projects: vec![
            KnownProject {
                path: "/home/dev/project".into(),
                name: "project".into(),
            },
            KnownProject {
                path: "/srv/other".into(),
                name: "other".into(),
            },
        ],
        primary: Some("/home/dev/project".into()),
    };
    let encoded = serde_json::to_string(&listing).expect("serialize ProjectListing");
    let decoded: ProjectListing = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, listing);

    let resolved = sample_resolved();
    let encoded = serde_json::to_string(&resolved).expect("serialize ResolvedProject");
    let decoded: ResolvedProject = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, resolved);

    let prepared = PreparedWorkflow {
        context_path: "/home/dev/project/.dot-agent-deck/orchestrator-context.md".into(),
        token: "prep-abc123".into(),
        roles: vec![sample_role()],
    };
    let encoded = serde_json::to_string(&prepared).expect("serialize PreparedWorkflow");
    let decoded: PreparedWorkflow = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, prepared);
}

/// The empty-collection and absent-`primary` cases have to survive the trip
/// too: `ListProjects` against a daemon with nothing live answers with an empty
/// list and no primary, and that is a normal answer rather than an error.
#[test]
fn empty_listing_round_trips_and_omits_absent_primary() {
    let listing = ProjectListing {
        projects: vec![],
        primary: None,
    };
    let encoded = serde_json::to_string(&listing).expect("serialize");
    assert!(
        !encoded.contains("primary"),
        "an absent primary must be omitted from the wire, not sent as null: {encoded}"
    );
    let decoded: ProjectListing = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, listing);
}

/// The property that makes the four new [`AttachResponse`] fields additive: a
/// reply from a peer that has never heard of them still deserializes, with each
/// one absent rather than the whole frame rejected as malformed.
///
/// This is the check that would have caught a missing `#[serde(default)]`. It
/// uses a hand-written older-shaped payload rather than a round-trip, because a
/// round-trip of a value this build constructed cannot express "the key was
/// never there".
#[test]
fn older_peer_reply_without_the_new_fields_still_deserializes() {
    let older = serde_json::json!({
        "ok": true,
        "server_version": 8,
        "build_version": "0.39.2-gdeadbee",
        "daemon_version": "0.39.2",
        "guarded_send": true,
    });
    let decoded: AttachResponse =
        serde_json::from_value(older).expect("an older daemon's reply must still parse");

    assert!(decoded.ok);
    assert_eq!(decoded.server_version, Some(8));
    assert_eq!(decoded.guarded_send, Some(true));
    assert!(decoded.projects.is_none());
    assert!(decoded.project.is_none());
    assert!(decoded.workflow_prepared.is_none());
    assert!(
        decoded.capabilities.is_none(),
        "absence must read as `None` — the client rule is withhold, and an empty \
         Vec would read as an explicit \"I support nothing\""
    );
}

/// And the same additive property in the other direction: a NEWER peer's reply
/// carrying a field this build does not know is ignored rather than rejected.
/// `AttachResponse` sets no `deny_unknown_fields`, and every doc comment on it
/// relies on that; a future `deny_unknown_fields` would break every deployed
/// client at once, so pin it.
#[test]
fn newer_peer_reply_with_an_unknown_field_still_deserializes() {
    let newer = serde_json::json!({
        "ok": true,
        "capabilities": ["list-projects", "resolve-project", "prepare-workflow"],
        "a_field_from_a_later_build": { "nested": [1, 2, 3] },
    });
    let decoded: AttachResponse =
        serde_json::from_value(newer).expect("an unknown field must be ignored, not rejected");
    assert!(decoded.ok);
    assert_eq!(
        decoded.capabilities.as_deref(),
        Some(
            [
                "list-projects".to_string(),
                "resolve-project".to_string(),
                "prepare-workflow".to_string()
            ]
            .as_slice()
        )
    );
}

/// A response carrying the new fields survives a round trip through the whole
/// [`AttachResponse`], not merely through the projection types on their own.
#[test]
fn attach_response_round_trips_the_new_fields() {
    let mut resp = AttachResponse::ok();
    resp.projects = Some(ProjectListing {
        projects: vec![KnownProject {
            path: "/home/dev/project".into(),
            name: "project".into(),
        }],
        primary: None,
    });
    resp.project = Some(sample_resolved());
    resp.workflow_prepared = Some(PreparedWorkflow {
        context_path: "/home/dev/project/.dot-agent-deck/orchestrator-context.md".into(),
        token: "prep-abc123".into(),
        roles: vec![sample_role()],
    });

    let encoded = serde_json::to_string(&resp).expect("serialize AttachResponse");
    let decoded: AttachResponse = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded.projects, resp.projects);
    assert_eq!(decoded.project, resp.project);
    assert_eq!(decoded.workflow_prepared, resp.workflow_prepared);
}

// ---------------------------------------------------------------------------
// No config type gained `Serialize`
// ---------------------------------------------------------------------------

/// A negative trait bound, which Rust has no direct syntax for.
///
/// The probe works by method resolution: `Probe<T>` gets a blanket
/// `impl<T> Fallback for Probe<T>` supplying `is_serialize() -> false`, and an
/// *inherent* `impl<T: Serialize> Probe<T>` supplying `is_serialize() -> true`.
/// Rust looks for an inherent method before a trait one, so `probe.is_serialize()`
/// answers `true` exactly when `T: Serialize` holds and `false` otherwise —
/// without either arm failing to compile.
struct Probe<T>(PhantomData<T>);

impl<T> Probe<T> {
    const fn new() -> Self {
        Self(PhantomData)
    }
}

trait Fallback {
    fn is_serialize(&self) -> bool {
        false
    }
}

impl<T> Fallback for Probe<T> {}

impl<T: Serialize> Probe<T> {
    fn is_serialize(&self) -> bool {
        true
    }
}

/// A local type that deliberately derives nothing, so the probe's negative arm
/// has a control that cannot drift. Most std types are `Serialize` — serde
/// implements it for `Cell`, `RefCell`, `Mutex` and friends — so reaching for
/// one as the control is how this test passes vacuously.
struct NeverSerialized;

/// The probe has to be shown working in both directions, or a bug in it would
/// make the assertion below vacuously pass.
#[test]
fn serialize_probe_answers_both_ways() {
    assert!(
        Probe::<ProjectRole>::new().is_serialize(),
        "ProjectRole derives Serialize, so the probe must say so"
    );
    assert!(
        !Probe::<NeverSerialized>::new().is_serialize(),
        "NeverSerialized derives nothing, so the probe must say so"
    );
}

/// PRD #819: **do not put `ProjectConfig` on the wire.** It deserializes via
/// `#[serde(try_from = "RawProjectConfig")]`, so a derived `Serialize` would
/// emit a shape that does not round-trip — `extends` flattened, defaults
/// materialised — which looks right and is not. The projection above is the
/// sanctioned way across, and this test is what makes "add `Serialize` to
/// `ProjectConfig`" fail loudly instead of quietly shipping that shape.
#[test]
fn project_config_types_did_not_gain_serialize() {
    use dot_agent_deck::project_config::{
        OrchestrationConfig, OrchestrationRoleConfig, ProjectConfig,
    };
    assert!(
        !Probe::<ProjectConfig>::new().is_serialize(),
        "ProjectConfig gained `Serialize`; use the PRD #819 projection instead — \
         a derive on the resolved struct does not round-trip through its `try_from`"
    );
    assert!(
        !Probe::<OrchestrationConfig>::new().is_serialize(),
        "OrchestrationConfig gained `Serialize`; project ProjectOrchestration instead"
    );
    assert!(
        !Probe::<OrchestrationRoleConfig>::new().is_serialize(),
        "OrchestrationRoleConfig gained `Serialize`; project ProjectRole instead — \
         its command / prompt_template fields must stay daemon-side"
    );
}
