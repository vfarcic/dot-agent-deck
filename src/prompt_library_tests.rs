use super::*;

fn store_in(dir: &tempfile::TempDir) -> PromptLibraryStore {
    PromptLibraryStore::load(dir.path().join(PROMPTS_FILE))
}

fn prompt(id: &str, name: &str, body: &str) -> Prompt {
    Prompt {
        id: id.into(),
        name: name.into(),
        body: body.into(),
        note: None,
    }
}

/// A saved library is what the next daemon start loads, in order and verbatim
/// — including text TOML has to escape.
#[test]
fn a_saved_library_survives_a_reload_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    let awkward = prompt(
        "b",
        "quotes \" and 'apostrophes'",
        "line one\nline two\t\"quoted\" \\ back\u{1b}[31m",
    );
    assert_eq!(
        store.save(prompt("a", "First", "do a")).unwrap(),
        LibraryOutcome::Changed
    );
    assert_eq!(
        store.save(awkward.clone()).unwrap(),
        LibraryOutcome::Changed
    );
    store.persist().unwrap();

    let reloaded = store_in(&dir);
    assert_eq!(reloaded.list(), vec![prompt("a", "First", "do a"), awkward]);
}

/// A save names a prompt by id: a known id is replaced in place, a new one is
/// appended, and saving the stored text again changes nothing once on disk.
#[test]
fn save_replaces_in_place_and_appends_new_ids() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    store.save(prompt("a", "A", "1")).unwrap();
    store.save(prompt("b", "B", "2")).unwrap();
    store.save(prompt("a", "A2", "1b")).unwrap();
    assert_eq!(
        store.list(),
        vec![prompt("a", "A2", "1b"), prompt("b", "B", "2")]
    );
    store.persist().unwrap();
    assert_eq!(
        store.save(prompt("b", "B", "2")).unwrap(),
        LibraryOutcome::Unchanged
    );
}

/// Deleting an id the library does not hold is no error and no change — two
/// clients removing the same prompt both succeed.
#[test]
fn delete_removes_and_tolerates_an_unknown_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    store.save(prompt("a", "A", "1")).unwrap();
    store.persist().unwrap();
    assert_eq!(store.delete("missing"), LibraryOutcome::Unchanged);
    assert_eq!(store.delete("a"), LibraryOutcome::Changed);
    assert!(store.list().is_empty());
    store.persist().unwrap();
    assert!(store_in(&dir).list().is_empty());
}

/// The migration seed adds only ids the daemon lacks and never overwrites one
/// it holds, so handing over an old local copy cannot undo a newer edit.
#[test]
fn a_seed_adds_absent_ids_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    store
        .save(prompt("a", "edited on the daemon", "new"))
        .unwrap();
    let outcome = store
        .seed(vec![
            prompt("a", "stale local copy", "old"),
            prompt("b", "B", "2"),
            prompt("b", "duplicate in the seed", "x"),
        ])
        .unwrap();
    assert_eq!(outcome, LibraryOutcome::Changed);
    assert_eq!(
        store.list(),
        vec![
            prompt("a", "edited on the daemon", "new"),
            prompt("b", "B", "2")
        ]
    );
}

/// A seed is all or nothing: one prompt the library could not store refuses
/// the whole seed, so the client keeps its copy rather than losing part of it.
#[test]
fn a_seed_with_one_bad_prompt_adds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    let too_long = prompt("z", "Z", &"x".repeat(MAX_PROMPT_BODY_BYTES + 1));
    let error = store
        .seed(vec![prompt("a", "A", "1"), too_long])
        .unwrap_err();
    assert!(error.contains("body"), "{error}");
    assert!(store.list().is_empty());
}

/// Every limit is enforced on save, and the refusal names the limit, never the
/// text.
#[test]
fn limits_are_refused_without_quoting_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    let secret = "sk-SENTINEL";
    let cases = [
        prompt("", "n", secret),
        prompt(&"i".repeat(MAX_PROMPT_ID_BYTES + 1), "n", secret),
        prompt("bad\nid", "n", secret),
        prompt(
            "a",
            &format!("{secret}{}", "n".repeat(MAX_PROMPT_NAME_BYTES)),
            "b",
        ),
        prompt(
            "a",
            "n",
            &format!("{secret}{}", "b".repeat(MAX_PROMPT_BODY_BYTES)),
        ),
        Prompt {
            note: Some(format!("{secret}{}", "o".repeat(MAX_PROMPT_NOTE_BYTES))),
            ..prompt("a", "n", "b")
        },
    ];
    for case in cases {
        let error = store.save(case).unwrap_err();
        assert!(!error.contains(secret), "{error}");
    }
    assert!(store.list().is_empty());

    for index in 0..MAX_PROMPTS {
        store.save(prompt(&format!("p{index}"), "n", "b")).unwrap();
    }
    let error = store.save(prompt("one-more", "n", "b")).unwrap_err();
    assert!(error.contains(&MAX_PROMPTS.to_string()), "{error}");
    assert_eq!(store.list().len(), MAX_PROMPTS);
}

/// The library's total size is bounded, so the reply that carries it fits one
/// frame.
#[test]
fn the_library_size_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    let body = "b".repeat(MAX_PROMPT_BODY_BYTES);
    let fit = MAX_LIBRARY_BYTES / (MAX_PROMPT_BODY_BYTES + 8);
    for index in 0..fit {
        store.save(prompt(&format!("p{index}"), "", &body)).unwrap();
    }
    let error = store.save(prompt("over", "", &body)).unwrap_err();
    assert!(error.contains("bytes of text"), "{error}");
}

/// The file is owner-only, like every store file the daemon keeps.
#[cfg(unix)]
#[test]
fn the_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    store.save(prompt("a", "A", "1")).unwrap();
    store.persist().unwrap();
    let mode = std::fs::metadata(store.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// A malformed file loads as an empty library and its text is never logged —
/// only its path and line.
#[test]
fn a_malformed_file_loads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(PROMPTS_FILE);
    std::fs::write(&path, "[[prompt]]\nid = \"a\"\nbody = \"sk-SENTINEL\n").unwrap();
    assert!(PromptLibraryStore::load(path).list().is_empty());
}

/// A file with entries a save could not have stored loads what it could have:
/// valid prompts, first of each id.
#[test]
fn a_loaded_file_drops_what_a_save_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(PROMPTS_FILE);
    std::fs::write(
        &path,
        concat!(
            "[[prompt]]\nid = \"a\"\nname = \"A\"\nbody = \"1\"\n",
            "[[prompt]]\nid = \"\"\nname = \"no id\"\nbody = \"2\"\n",
            "[[prompt]]\nid = \"a\"\nname = \"dup\"\nbody = \"3\"\n",
            "[[prompt]]\nid = \"c\"\nname = \"C\"\nbody = \"4\"\nnote = \"n\"\n",
        ),
    )
    .unwrap();
    let store = PromptLibraryStore::load(path);
    assert_eq!(
        store.list(),
        vec![
            prompt("a", "A", "1"),
            Prompt {
                note: Some("n".into()),
                ..prompt("c", "C", "4")
            }
        ]
    );
}

/// A symlink at the file's path is refused rather than followed.
#[cfg(unix)]
#[test]
fn a_symlinked_file_loads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("elsewhere.toml");
    std::fs::write(
        &target,
        "[[prompt]]\nid = \"a\"\nname = \"A\"\nbody = \"planted\"\n",
    )
    .unwrap();
    let path = dir.path().join(PROMPTS_FILE);
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(PromptLibraryStore::load(path).list().is_empty());
}

/// Two persists that run out of order leave the newest library on disk.
#[test]
fn a_stale_persist_never_overwrites_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    store.save(prompt("a", "A", "1")).unwrap();
    store.save(prompt("a", "A", "2")).unwrap();
    store.persist().unwrap();
    // A second persist for the earlier change finds the generation on disk.
    store.persist().unwrap();
    assert_eq!(store_in(&dir).list(), vec![prompt("a", "A", "2")]);
}

/// A copy of a prompt the library already holds is skipped before it is
/// checked, so an oversized stale copy cannot block the rest of a seed.
#[test]
fn a_skipped_copy_is_not_checked() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    store.save(prompt("a", "A", "held")).unwrap();
    let oversized_copy = prompt("a", "A", &"x".repeat(MAX_PROMPT_BODY_BYTES + 1));
    store
        .seed(vec![oversized_copy, prompt("b", "B", "2")])
        .unwrap();
    assert_eq!(
        store.list(),
        vec![prompt("a", "A", "held"), prompt("b", "B", "2")]
    );
}
