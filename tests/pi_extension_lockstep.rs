//! Issue #1949: pi-extension's `typebox` is pinned to exactly the version
//! `@earendil-works/pi-coding-agent` depends on, so npm keeps one copy and the
//! extension's typecheck and tests run against the `typebox` Pi loads at
//! runtime (#1928). renovate.json does not update it on its own; these tests
//! are what make a pi-coding-agent bump that moves its `typebox` carry ours
//! along in the same PR. They read only `pi-extension/package.json` and
//! `package-lock.json` from the checkout.

use std::path::Path;

/// Checks that every `typebox` in `package_lock` is the version
/// `package_json` pins, and that `@earendil-works/pi-coding-agent` declares
/// exactly that version rather than a range.
fn typebox_lockstep(
    package_json: &serde_json::Value,
    package_lock: &serde_json::Value,
) -> Result<(), String> {
    const PI: &str = "node_modules/@earendil-works/pi-coding-agent";
    let pin = package_json["devDependencies"]["typebox"]
        .as_str()
        .ok_or("pi-extension/package.json has no `typebox` dev dependency")?;
    let packages = package_lock["packages"]
        .as_object()
        .ok_or("pi-extension/package-lock.json has no `packages` map")?;
    let Some(declared) = packages
        .get(PI)
        .and_then(|pi| pi["dependencies"].get("typebox"))
    else {
        return Err(format!(
            "`{PI}` in package-lock.json no longer depends on `typebox`, so there is \
             no Pi version to keep the extension's `typebox` in step with: revisit \
             the pin and renovate.json's typebox rule"
        ));
    };
    let copies: Vec<(&String, &str)> = packages
        .iter()
        .filter(|(path, _)| path.ends_with("node_modules/typebox"))
        .map(|(path, entry)| (path, entry["version"].as_str().unwrap_or("<none>")))
        .collect();
    if copies.is_empty() {
        return Err("package-lock.json resolves no `typebox`".into());
    }
    let drifted: Vec<String> = copies
        .iter()
        .filter(|(_, version)| *version != pin)
        .map(|(path, version)| format!("{path} = {version}"))
        .collect();
    if !drifted.is_empty() {
        return Err(format!(
            "pi-extension/package.json pins `typebox` {pin}, but package-lock.json \
             also resolves {}. Pin `typebox` to exactly the version \
             `@earendil-works/pi-coding-agent` depends on, then run `npm install` in \
             pi-extension/ so one copy remains",
            drifted.join(", ")
        ));
    }
    // A range here would let another install of the same Pi release
    // resolve a `typebox` this lockfile never saw.
    if declared.as_str() != Some(pin) {
        return Err(format!(
            "`{PI}` declares `typebox` {declared}, but pi-extension/package.json \
             pins {pin}: the pin must be exactly the version Pi declares, and a \
             range there means there is no single version to pin against"
        ));
    }
    Ok(())
}

/// Scenario: Reads pi-extension's real package.json and package-lock.json and checks that the pinned `typebox` is the only `typebox` the lockfile resolves, so the extension is typechecked against the copy `@earendil-works/pi-coding-agent` brings.
#[test]
fn pi_extension_typebox_is_the_copy_pi_resolves() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("pi-extension");
    let read = |name: &str| -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(name)).expect(name);
        serde_json::from_str(&text).expect(name)
    };
    if let Err(drift) = typebox_lockstep(&read("package.json"), &read("package-lock.json")) {
        panic!("{drift}");
    }
}

/// Scenario: Feeds the typebox lockstep check synthetic package.json and lockfile pairs: one copy at the pinned version passes, while a pin bumped alone (#1942's shape), a Pi bump that left ours behind, a range instead of an exact pin, a Pi that declares a range, and a Pi that no longer depends on `typebox` each fail.
#[test]
fn typebox_lockstep_refuses_a_copy_pi_does_not_load() {
    use serde_json::json;
    const PI: &str = "node_modules/@earendil-works/pi-coding-agent";
    let package = |pin: &str| json!({ "devDependencies": { "typebox": pin } });
    let lock = |pi_typebox: Option<&str>, copies: &[(&str, &str)]| {
        let mut packages = serde_json::Map::new();
        let pi_deps = pi_typebox.map_or(json!({}), |v| json!({ "typebox": v }));
        packages.insert(
            PI.into(),
            json!({ "version": "1.4.0", "dependencies": pi_deps }),
        );
        for (path, version) in copies {
            packages.insert((*path).into(), json!({ "version": version }));
        }
        json!({ "packages": packages })
    };
    let one_copy = lock(Some("1.3.27"), &[("node_modules/typebox", "1.3.27")]);
    assert_eq!(typebox_lockstep(&package("1.3.27"), &one_copy), Ok(()));

    let bumped_alone = lock(
        Some("1.3.27"),
        &[
            ("node_modules/typebox", "1.3.37"),
            (&format!("{PI}/node_modules/typebox"), "1.3.27"),
        ],
    );
    let err = typebox_lockstep(&package("1.3.37"), &bumped_alone).unwrap_err();
    assert!(
        err.contains("pi-coding-agent/node_modules/typebox = 1.3.27"),
        "{err}"
    );

    let pi_moved = lock(
        Some("1.3.40"),
        &[
            ("node_modules/typebox", "1.3.27"),
            (&format!("{PI}/node_modules/typebox"), "1.3.40"),
        ],
    );
    assert!(typebox_lockstep(&package("1.3.27"), &pi_moved).is_err());

    assert!(typebox_lockstep(&package("^1.3.27"), &one_copy).is_err());

    let pi_ranged = lock(Some("^1.3.27"), &[("node_modules/typebox", "1.3.27")]);
    let err = typebox_lockstep(&package("1.3.27"), &pi_ranged).unwrap_err();
    assert!(err.contains("declares `typebox` \"^1.3.27\""), "{err}");

    let pi_dropped = lock(None, &[("node_modules/typebox", "1.3.27")]);
    let err = typebox_lockstep(&package("1.3.27"), &pi_dropped).unwrap_err();
    assert!(err.contains("no longer depends on `typebox`"), "{err}");
}
