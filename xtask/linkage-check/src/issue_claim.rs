//! Issue #2381: the claim on an issue (the runner as assignee plus the
//! `in-progress` label) is defined once, in `.claude/skills/issue-claim`, and
//! every skill that claims, selects around or releases a claim points there.
//!
//! The queues used to claim with the assignee alone, which cannot tell two
//! machines running as one GitHub account apart, and nothing released a claim.
//! The fix is a procedure, not code, and it lives in prose that several skills
//! share; a copy pasted back into one of them, or a selecting filter that drops
//! the label, would go unnoticed without these tests.
//!
//! **What this does NOT claim.** It reads text. It checks that the defining
//! skill still carries its commands, that the skills which claim, select or
//! release link to it, that the two queues' selection filters still exclude the
//! label, and that no other skill writes an assignee or the label itself. It
//! does not check that an agent follows them, and it cannot close the race the
//! skill describes.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The skill that defines the claim.
    const SKILL: &str = "issue-claim";

    /// How a sibling skill links to it.
    const LINK: &str = "(../issue-claim/SKILL.md)";

    /// The writes only the defining skill may carry.
    const WRITES: &[&str] = &[
        "--add-assignee",
        "--add-label in-progress",
        "--remove-label in-progress",
    ];

    /// The selection filter's exclusion of the label.
    const EXCLUDE: &str = r#"select((.labels|map(.name)|index("in-progress"))==null)"#;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask/linkage-check sits two levels below the workspace root")
            .to_path_buf()
    }

    fn skill(name: &str) -> String {
        let path = repo_root().join(format!(".claude/skills/{name}/SKILL.md"));
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Every `SKILL.md` under `.claude/skills/`, as (skill directory name,
    /// contents).
    fn project_skills() -> Vec<(String, String)> {
        let dir = repo_root().join(".claude/skills");
        let mut skills = Vec::new();
        for entry in fs::read_dir(&dir).expect("read .claude/skills") {
            let entry = entry.expect("read a .claude/skills entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(text) = fs::read_to_string(entry.path().join("SKILL.md")) {
                skills.push((name, text));
            }
        }
        skills.sort();
        skills
    }

    #[test]
    fn issue_claim_001_the_skill_carries_its_commands() {
        let text = skill(SKILL);
        for needle in [
            // claim: read, one write for both halves, read back
            r#"gh issue edit <n> --repo "$OWNER/$REPO" --add-assignee "$ME" --add-label in-progress"#,
            r#"any(.labels[]; .name=="in-progress")"#,
            // select: the exclusion, and the time the label was added
            EXCLUDE,
            r#".event=="labeled" and .label.name=="in-progress""#,
            "claimed, possibly stale",
            // release: both halves, and the moments
            r#"--remove-label in-progress --remove-assignee "$ME""#,
            "without reporting.",
            "--head '<branch>' --state all",
            // the merge's issue, from a dispatch branch only
            "agent/dispatch-(issue|prd)-([0-9]+)",
        ] {
            assert!(
                text.contains(needle),
                "{SKILL}/SKILL.md no longer contains `{needle}`"
            );
        }
    }

    #[test]
    fn issue_claim_002_the_skills_that_claim_select_or_release_link_to_it() {
        for name in ["issue-queue", "prd-queue", "prd-start", "merge-pr"] {
            assert!(
                skill(name).contains(LINK),
                "{name}/SKILL.md no longer links to `{LINK}`"
            );
        }
    }

    #[test]
    fn issue_claim_003_the_queues_select_around_the_label() {
        for name in ["issue-queue", "prd-queue"] {
            assert!(
                skill(name).contains(EXCLUDE),
                "{name}/SKILL.md's selection no longer excludes `in-progress`: `{EXCLUDE}`"
            );
        }
    }

    #[test]
    fn issue_claim_004_no_other_skill_writes_a_claim_itself() {
        let copies: Vec<String> = project_skills()
            .into_iter()
            .filter(|(name, text)| name != SKILL && WRITES.iter().any(|w| text.contains(w)))
            .map(|(name, _)| name)
            .collect();
        assert!(
            copies.is_empty(),
            "these skills write an assignee or the `in-progress` label themselves instead of \
             pointing at .claude/skills/{SKILL}/SKILL.md, so their copy can drift from it: \
             {copies:?}"
        );
    }
}
