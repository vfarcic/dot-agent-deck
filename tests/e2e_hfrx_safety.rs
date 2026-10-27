#![cfg(all(feature = "e2e", target_os = "linux"))]

use spec::spec;

/// Scenario: Run the H-FRX offline safety checks with fixture files, synthetic traces and a detached Python stand-in process tree. Refuse fragmented-secret exports, exercise cancellation and pidfd cleanup, and check reducer contracts without starting an agent, mock provider, namespace, container or deck daemon.
#[spec("harness/hfrx/001")]
#[test]
fn hfrx_001_offline_safety_controls() {
    let output = std::process::Command::new("python3")
        .args([
            "-m",
            "unittest",
            "discover",
            "-s",
            "scripts/hfrx",
            "-p",
            "test_*.py",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("H-FRX stand-in safety checks require python3 and Linux pidfd/subreaper support");
    assert!(
        output.status.success(),
        "H-FRX safety checks failed (unsupported prerequisites are failures): {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
