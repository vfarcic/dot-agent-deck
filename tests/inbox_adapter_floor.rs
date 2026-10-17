//! PRD #1666 §11: the capability probe against this build's own CLI.
use spec::spec;
use std::time::Duration;

/// Scenario: Run the capability probe against the `dot-agent-deck` binary this build produced. Its `inbox wait --help` names `--adapter-integration`, so the probe proves the CLI supports the deck's integration and sets no notice.
#[spec("inbox/boundary/015")]
#[test]
fn boundary_015_this_builds_cli_passes_the_capability_probe() {
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    assert_eq!(
        dot_agent_deck::inbox_hub::probe_cli(binary, Duration::from_secs(10)),
        dot_agent_deck::inbox_hub::ProbeVerdict::Supported
    );
}
