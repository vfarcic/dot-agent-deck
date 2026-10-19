//! Fast privacy regression for the passive real-Pi observer fixture.

#[test]
fn native_observer_keeps_delivery_evidence_without_private_payloads() {
    let output = std::process::Command::new("node")
        .args(["--test", "tests/fixtures/pi-inbox/observe-native.test.mjs"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run native observer privacy check with Node");
    assert!(
        output.status.success(),
        "observer privacy regression:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
