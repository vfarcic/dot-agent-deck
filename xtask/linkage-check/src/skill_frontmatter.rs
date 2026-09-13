//! Issue #1061: every `.claude/skills/*/SKILL.md` frontmatter block must be
//! valid YAML, and every skill name must match the spec's `^[a-z0-9-]+$`.
//!
//! Pi refused to load six of this repository's skills while Claude Code loaded
//! all six without complaint. Both read the same files; Pi is the stricter one.
//! Two of the six carried a `description:` written as a plain scalar containing
//! `": "`, which YAML does not permit there — the value reads as a nested
//! mapping — and four carried names with an underscore or camelCase.
//!
//! **The failure mode is silence, which is what puts the guard here.** Nothing
//! compiles a `SKILL.md`, no CI job read one before this module, and the
//! permissive reader is the one contributors run all day: a malformed
//! description round-trips through Claude Code looking fine and only breaks in
//! a different agent harness, possibly weeks later. That is the same argument
//! CLAUDE.md rule 5 records for `clean_tmp.rs`, `junit_strip.rs` and
//! `pin_lockstep.rs` — a property that exists only at run time, in repository
//! files, guarded by a test because no compile step can see it.
//!
//! Tests only; there is no runtime rule and nothing is added to
//! `cargo xtask linkage-check`'s rule count. Two of them run against synthetic
//! fixtures and the rest against the real checkout, where the real-checkout
//! ones are the guard itself — the `pin_lockstep.rs` shape.
//!
//! **On the parser.** The defect this guards is precisely that a lenient reader
//! accepted what a spec-correct one rejected, so the check is worth only as
//! much as its parser is strict. It uses `yaml-rust2`, a fully YAML 1.2
//! compliant implementation, rather than a hand-rolled scan for `": "` — a
//! bespoke matcher would re-introduce the bug's own cause in the gate meant to
//! catch it, and would miss every sibling defect (an unquoted leading `[`, a
//! stray tab, a duplicated key) that the same class of edit can produce.
//!
//! **What this does NOT claim.** It checks the frontmatter block, not the
//! Markdown body, and it does not validate the skill *schema* beyond requiring
//! a `name` and a `description` that are strings. It is a syntax and naming
//! gate, not a statement that a skill is well-written or that any particular
//! agent harness will load it.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use yaml_rust2::{Yaml, YamlLoader};

    /// Names that fail `^[a-z0-9-]+$` and are deliberately retained anyway.
    ///
    /// Issue #1061 kept both for their subject matter, accepting that a
    /// spec-correct validator keeps reporting them. They are mirrors synced
    /// from the `dot-ai` project, so renaming them is not available: CLAUDE.md
    /// rule 13 records that a local edit to a mirror is reverted by the next
    /// sync (`04a3641` narrowed one, `e94388d` reverted it byte for byte five
    /// days later), and a rename is an edit. Deleting them is the only way off
    /// this list, which is what [`the_allowlist_does_not_rot`] enforces.
    const NAME_ALLOWLIST: &[&str] = &["dot-ai-manageKnowledge", "dot-ai-manageOrgData"];

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask/linkage-check sits two levels below the workspace root")
            .to_path_buf()
    }

    /// Every `SKILL.md` in the checkout, as (skill directory name, path).
    fn skill_files() -> Vec<(String, PathBuf)> {
        let dir = repo_root().join(".claude/skills");
        let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| {
                let f = e.path().join("SKILL.md");
                f.is_file()
                    .then(|| (e.file_name().to_string_lossy().into_owned(), f))
            })
            .collect();
        out.sort();
        assert!(
            out.len() >= 20,
            "expected the skills directory to hold many SKILL.md files, found {} — \
             has the layout moved? This gate silently covers nothing if the walk finds none.",
            out.len()
        );
        out
    }

    /// Read a file with CRLF normalised to LF.
    ///
    /// `.gitattributes` does not pin the working-tree line ending for `.md`, so
    /// a Windows checkout gets `\r\n` and the `---` fence below would not match.
    /// `build-windows` runs `cargo nextest run --workspace`, so this is load
    /// bearing rather than theoretical — it is what `issue_labeler_memory.rs`
    /// and `issue_labeler_policy.rs` already do for the same reason.
    fn read_lf(path: &Path) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
            .replace("\r\n", "\n")
    }

    /// Split the `---`-delimited frontmatter block off the top of a `SKILL.md`.
    ///
    /// Returns `Err` with a human-readable reason rather than panicking, so the
    /// caller can attribute the failure to a named file.
    fn frontmatter(src: &str) -> Result<&str, String> {
        let rest = src
            .strip_prefix("---\n")
            .ok_or_else(|| "does not open with a `---` frontmatter fence".to_string())?;
        let end = rest
            .find("\n---\n")
            .ok_or_else(|| "frontmatter fence is never closed with `---`".to_string())?;
        Ok(&rest[..end])
    }

    fn parse(block: &str) -> Result<Yaml, String> {
        let mut docs = YamlLoader::load_from_str(block).map_err(|e| e.to_string())?;
        match docs.len() {
            1 => Ok(docs.remove(0)),
            n => Err(format!(
                "frontmatter holds {n} YAML documents, expected exactly 1"
            )),
        }
    }

    /// THE GUARD. Every frontmatter block parses as YAML under a spec-correct
    /// parser, and carries a string `name` and a string `description`.
    ///
    /// This is the half that would have caught issue #1061: `prd-queue` and
    /// `reproduce-first` each failed here, on a `description:` plain scalar
    /// containing `": "`.
    #[test]
    fn every_skill_frontmatter_is_valid_yaml() {
        let mut bad: Vec<String> = Vec::new();
        for (dir, path) in skill_files() {
            let src = read_lf(&path);
            let doc = match frontmatter(&src).and_then(parse) {
                Ok(d) => d,
                Err(why) => {
                    bad.push(format!("{dir}: {why}"));
                    continue;
                }
            };
            let Some(map) = doc.as_hash() else {
                bad.push(format!("{dir}: frontmatter is not a YAML mapping"));
                continue;
            };
            for key in ["name", "description"] {
                match map.get(&Yaml::String(key.to_string())) {
                    Some(Yaml::String(s)) if !s.trim().is_empty() => {}
                    Some(_) => bad.push(format!("{dir}: `{key}` is present but is not a string")),
                    None => bad.push(format!("{dir}: `{key}` is missing")),
                }
            }
        }
        assert!(
            bad.is_empty(),
            "SKILL.md frontmatter must be valid YAML (issue #1061). Offenders:\n  {}\n\n\
             A `description:` holding `\": \"`, a `#`, a leading `[`/`{{`/`*`/`&`, or a trailing \
             `:` needs quoting — wrap the value in single quotes (doubling any apostrophe) and \
             do NOT reword it: a description is what the model matches a request against, so \
             rewording changes behaviour to fix a syntax error.",
            bad.join("\n  ")
        );
    }

    /// Every skill name matches the spec's `^[a-z0-9-]+$`, bar the allowlist.
    #[test]
    fn every_skill_name_is_spec_legal() {
        let offenders: Vec<String> = skill_files()
            .into_iter()
            .map(|(dir, _)| dir)
            .filter(|n| {
                !n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            })
            .filter(|n| !NAME_ALLOWLIST.contains(&n.as_str()))
            .collect();
        assert!(
            offenders.is_empty(),
            "skill names must match ^[a-z0-9-]+$ (issue #1061): {offenders:?}\n\n\
             For a project-local skill, rename the directory and its `name:` together. For a \
             `dot-ai-*` mirror, renaming is NOT available — CLAUDE.md rule 13 records that the \
             next sync reverts a local edit — so either delete it or add it to NAME_ALLOWLIST \
             with the reason it is being kept."
        );
    }

    /// The allowlist may not outlive what it excuses.
    ///
    /// Without this, deleting or renaming an allowlisted skill leaves a stale
    /// entry that silently excuses a *future* skill that happens to take the
    /// same name — the allowlist would quietly grow rights it was never
    /// granted.
    #[test]
    fn the_allowlist_does_not_rot() {
        let present: Vec<String> = skill_files().into_iter().map(|(d, _)| d).collect();
        for name in NAME_ALLOWLIST {
            assert!(
                present.iter().any(|p| p == name),
                "NAME_ALLOWLIST excuses `{name}`, which no longer exists — drop the entry"
            );
            assert!(
                !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "NAME_ALLOWLIST excuses `{name}`, which is now spec-legal — drop the entry"
            );
        }
    }

    /// A skill's `name:` matches the directory it lives in.
    ///
    /// Held by all 25 skills when this was written. It is what makes the name
    /// check above meaningful: the directory is what a `/slash` invocation and
    /// every cross-reference in the tree spell, so a `name:` that disagreed
    /// with it would put the two validators on different strings.
    #[test]
    fn skill_name_matches_its_directory() {
        let mut bad: Vec<String> = Vec::new();
        for (dir, path) in skill_files() {
            let src = read_lf(&path);
            let Ok(doc) = frontmatter(&src).and_then(parse) else {
                continue; // attributed by `every_skill_frontmatter_is_valid_yaml`
            };
            if let Some(Yaml::String(name)) = doc
                .as_hash()
                .and_then(|m| m.get(&Yaml::String("name".to_string())))
                && *name != dir
            {
                bad.push(format!("{dir}: declares name `{name}`"));
            }
        }
        assert!(
            bad.is_empty(),
            "a skill's `name:` must match its directory:\n  {}",
            bad.join("\n  ")
        );
    }

    /// The regression fixture: the exact shape issue #1061 fixed is rejected.
    ///
    /// Pins that the parser really is strict enough to catch the original
    /// defect. If this ever passes, the gate above has stopped gating.
    #[test]
    fn a_plain_scalar_description_holding_a_colon_space_is_rejected() {
        // `reproduce-first`'s original line, trimmed to the offending clause.
        let block = "name: reproduce-first\n\
                     description: Reproduce first, then fix. Use it however they phrase it: a \
                     complaint, a neutral observation, or an aside.\n\
                     user-invocable: true";
        let err = parse(block).expect_err("a `\": \"` inside a plain scalar is not valid YAML");
        assert!(
            err.contains("mapping") || err.contains("expected") || !err.is_empty(),
            "unexpected parser error text: {err}"
        );
    }

    /// ...and the single-quoted form issue #1061 replaced it with is accepted,
    /// with the apostrophes, em-dash, `"` and `: ` all surviving verbatim.
    #[test]
    fn the_quoted_form_parses_and_preserves_the_wording() {
        let wording = "Reproduce first, then fix — however they phrase it: a complaint, \
                       an observation, or an aside that something works \"except for\" one detail.";
        let block =
            format!("name: reproduce-first\ndescription: '{wording}'\nuser-invocable: true");
        let doc = parse(&block).expect("a single-quoted scalar holding `: ` is valid YAML");
        let got = doc
            .as_hash()
            .and_then(|m| m.get(&Yaml::String("description".to_string())))
            .and_then(Yaml::as_str)
            .expect("description is a string");
        assert_eq!(got, wording, "quoting must not alter the wording");
    }

    /// A CRLF checkout parses identically to an LF one.
    ///
    /// Pins the `read_lf` normalisation above: without it every skill on a
    /// Windows working tree reports `does not open with a `---` frontmatter
    /// fence`, turning the whole gate into a platform-specific false alarm.
    #[test]
    fn a_crlf_checkout_parses_the_same_as_lf() {
        let lf = "---\nname: demo\ndescription: 'holds a: colon'\n---\n\n# body\n";
        let crlf = lf.replace('\n', "\r\n");

        let from_lf = parse(frontmatter(lf).expect("LF fence is found")).expect("LF parses");
        let normalised = crlf.replace("\r\n", "\n");
        let from_crlf =
            parse(frontmatter(&normalised).expect("CRLF fence is found once normalised"))
                .expect("CRLF parses");

        assert_eq!(from_lf, from_crlf, "line endings must not change the parse");
        assert!(
            frontmatter(&crlf).is_err(),
            "un-normalised CRLF must NOT match the fence — if this ever passes, \
             `read_lf` has stopped being load bearing and this fixture is lying"
        );
    }
}
