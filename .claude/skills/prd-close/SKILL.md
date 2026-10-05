---
name: prd-close
description: Close a PRD that is already implemented or no longer needed
user-invocable: true
---

# Close PRD

Close a PRD that is already implemented (in previous work or external projects) or is no longer needed. This workflow records why, closes the GitHub issue, and — for an older PRD that has a file — archives the file.

**Where the PRD lives.** This skill is a project-local fork of the `dot-ai` mirror of the same name (CLAUDE.md rule 13), changed only where it assumed the PRD is a file. Since issue #1591 a new PRD is its GitHub issue, and closing it is closing the issue with a closing comment: **no file move and no commit**. An older PRD keeps its `prds/<n>-*.md` file, and archiving that file is a commit — which here goes through a PR, because `main` is protected (CLAUDE.md rule 8) and the mirror's "commit directly to main" step is refused with `GH013` (or, for an admin, silently bypasses the review). `bash .claude/skills/prd-start/prd-source.sh <n>` says which case you are in, and [`../prd-start/issue-prd.md`](../prd-start/issue-prd.md) has the issue conventions.

## When to Use This Command

**Use `/prd-close` when:**
- ✅ PRD functionality is already implemented in a separate project or previous work
- ✅ PRD is no longer relevant (superseded, requirements changed, out of scope)
- ✅ PRD requirements are satisfied by existing functionality
- ✅ No new code implementation is needed in this repository

**DO NOT use `/prd-close` when:**
- ❌ You just finished implementing the PRD (use `/pr-create` instead)
- ❌ PRD has active implementation work in progress
- ❌ There are uncommitted code changes that need to be part of a PR

## Usage

```bash
# Interactive mode - will prompt for PRD number and closure reason
/prd-close

# With PRD number
/prd-close 20

# With PRD number and reason
/prd-close 20 "Already implemented by dot-ai-controller"
```

**Note**: If any `gh` command fails with "command not found", inform the user that GitHub CLI is required and provide the installation link: https://cli.github.com/

## Workflow Steps

### Step 1: Identify PRD and Reason

**If PRD number not provided:**
- Check conversation context for recent PRD discussion
- Check git branch for PRD indicators (e.g., `feature/prd-X`)
- If unclear, prompt user for PRD number

**Closure Reason Categories:**
- **Already Implemented**: Functionality exists in external project or previous work
- **No Longer Needed**: Requirements changed, out of scope, or superseded
- **Duplicate**: Another PRD covers the same functionality
- **Deferred**: Moved to future version or different project

**Required Information:**
- PRD number
- Closure reason (brief description)
- Implementation reference (if already implemented): link to repo, PR, or documentation

### Step 2: Read and Validate PRD

Locate it with `bash .claude/skills/prd-start/prd-source.sh [number]`, then read it: the file it names for `SOURCE=file`, or the issue body and its collaborator comments for `SOURCE=issue`. For `SOURCE=none` the issue carries no PRD; closing it is then an ordinary issue close, which this skill can still do through the issue path below, but say so to the user.

**Validation checks:**
- [ ] PRD exists and is readable (its file, or its issue)
- [ ] Confirm with user that this PRD should be closed
- [ ] Verify closure reason makes sense given PRD content
- [ ] Ask user for implementation evidence (if "already implemented")

**Present PRD summary to user:**
```markdown
## PRD #X: [Title]
**Status**: [Current Status]
**Created**: [Date]

**Summary**: [Brief description of what PRD requested]

**Proposed Action**: Close as [reason]
**Implementation Reference**: [If applicable]

Proceed with closure? (yes/no)
```

### Step 3: Close It — Issue PRD (`SOURCE=issue`, or `none`)

No file, no branch, no commit.

1. **Post the closure comment** — the Step F4 template below, headed `### PRD closed — [YYYY-MM-DD]` instead, and with its `### Files` section replaced by "**PRD**: this issue's body". Write it to `.dot-agent-deck/prd-[number]-comment.md` with your file-writing tool and post it with `gh issue comment [number] --body-file .dot-agent-deck/prd-[number]-comment.md` ([`../prd-start/issue-prd.md`](../prd-start/issue-prd.md), "Comments").
2. **Close the issue with the reason that matches**:
   ```bash
   gh issue close [number] --reason completed          # Already Implemented
   gh issue close [number] --reason "not planned"      # No Longer Needed / Deferred
   gh issue close [number] --duplicate-of [other]      # Duplicate
   ```
3. Leave the body as it is. It is the PRD as it stood when it closed, and the closing comment says why.

### Step 4: Close It — File PRD (`SOURCE=file`)

#### F1: Update PRD File

Update the PRD metadata:

**Metadata Updates:**
```markdown
**Status**: Complete [or] No Longer Needed [or] Duplicate
**Last Updated**: [Current Date]
**Completed**: [Current Date] [or] **Closed**: [Current Date]
```

#### F2: Move PRD to Archive

Work on a branch, never on `main` (CLAUDE.md rule 1 says to ask whether the user wants a worktree or a branch). Move the PRD file to the done directory and update roadmap:

```bash
git mv prds/[number]-[name].md prds/done/
```

**Note**: If the move fails because `prds/done/` doesn't exist, create it with `mkdir -p prds/done` and retry.

**Update ROADMAP.md (if it exists):**
- [ ] Check if `docs/ROADMAP.md` exists
- [ ] Remove the closed PRD from the roadmap (search for "PRD #[number]")
- [ ] Remove the entire line that references this PRD
- [ ] Closed PRDs should not appear in future roadmap as they're no longer being worked on

#### F3: Update GitHub Issue

**Update issue description with new PRD path and status** (the link resolves once the PR merges):
```bash
gh issue edit [number] --body "$(cat <<'EOF'
## PRD: [Title]

**Problem**: [Original problem statement]

**Solution**: [Original solution statement]

**Detailed PRD**: See [prds/done/[number]-[name].md](./prds/done/[number]-[name].md)

**Priority**: [Original Priority]

**Status**: ✅ **[COMPLETE/CLOSED]** - [Brief reason]
EOF
)"
```

#### F4: Record the Closure on the Issue

Post the comprehensive closure comment now, but do **not** close the issue here: the PR's `Closes #[number]` closes it when the archival merges, so the issue and the file cannot disagree about whether the PRD is closed.

```bash
gh issue comment [number] --body "$(cat <<'EOF'
## ✅ PRD #[number] Closed - [Reason Category]

[Detailed explanation of why PRD is being closed]

### [If "Already Implemented"]
**Implementation Details**

This PRD requested [functionality]. **All core requirements are satisfied** by [implementation reference].

| Requirement | Implementation | Status |
|-------------|----------------|--------|
| [Requirement 1] | [Where implemented] | ✅ Complete |
| [Requirement 2] | [Where implemented] | ✅ Complete |

**Implementation Reference**: [Link to project/repo/PR]

[If there are gaps]
**Not Implemented** (deferred or out of scope):
- [Feature X] - [Why not needed or deferred]

### [If "No Longer Needed"]
**Reason for Closure**

[Explain why requirements changed, what superseded this, or why it's out of scope]

**Alternative Approach**: [If applicable]
[What replaced this PRD or how needs are met differently]

### Files

**PRD Location**: `prds/done/[number]-[name].md`
**Status**: [Complete/Closed]
**Closed**: [Date]
EOF
)"
```

#### F5: Commit on a Branch and Open a PR

Run the gates CLAUDE.md rule 2 names before committing, then:

```bash
git add prds/
git status   # only the moved PRD and its in-repo link fixes

git commit -m "docs(prd-[number]): close PRD #[number] - [brief reason]

- Moved PRD to prds/done/ directory
- Updated PRD status to [Complete/Closed]
- [Implementation details or reason]

Closes #[number]"
```

Then run `/pr-create`, with `Closes #[number]` in the PR body. No `[skip ci]`: the required checks have to report for the PR to merge.

## Example Scenarios

### Example 1: Already Implemented in External Project

```bash
/prd-close 20 "Implemented by dot-ai-controller"
```

**Closure Comment:**
```markdown
## ✅ PRD #20 Closed - Already Implemented

This PRD requested proactive Kubernetes cluster monitoring with AI-powered remediation.
**Core functionality (60-80%) is already implemented** by the separate
[dot-ai-controller](https://github.com/vfarcic/dot-ai-controller) project.

| Requirement | Implementation | Status |
|-------------|----------------|--------|
| Continuous health checks | Event-based monitoring via K8s events | ✅ Complete |
| Intelligent alerting | Slack notifications with AI analysis | ✅ Complete |
| Automated remediation | Automatic/manual modes with confidence thresholds | ✅ Complete |
| Anomaly detection | AI-powered event analysis | ✅ Complete |

**Not Implemented** (advanced features, may be future PRD):
- Continuous metrics monitoring (Prometheus-style)
- Predictive analytics with baseline learning
- Multi-channel alerting (email, PagerDuty)
```

### Example 2: Duplicate PRD

```bash
/prd-close 45 "Duplicate of PRD #44"
```

**Closure Comment:**
```markdown
## 🔄 PRD #45 Closed - Duplicate

This PRD covers the same functionality as PRD #44. Consolidating all work
under PRD #44 to avoid fragmentation.

**Action**: Continue work on PRD #44 instead.
```

### Example 3: No Longer Needed

```bash
/prd-close 12 "Requirements changed, out of scope"
```

**Closure Comment:**
```markdown
## ⏸️ PRD #12 Closed - No Longer Needed

After discussion, this approach no longer aligns with project direction.
Requirements have evolved and this PRD is out of scope.

**Alternative Approach**: Using [different solution/approach] instead.
```

## Success Criteria

**Issue PRD:**
✅ **Closure comment posted** on the issue
✅ **GitHub issue closed** with the matching reason
✅ **Nothing committed**

**File PRD:**
✅ **PRD file updated** with completion/closure metadata
✅ **PRD archived** to `prds/done/` directory, on a branch
✅ **GitHub issue updated** with new PRD path and the closure comment
✅ **PR opened** with `Closes #[number]`, so the merge closes the issue

## Notes

- **No PR for an issue PRD**: closing one is two `gh` calls and touches nothing in the repository
- **A PR for a file PRD**: archiving the file is a commit, and `main` takes commits only through an approved PR (CLAUDE.md rule 8)
- **Comprehensive documentation**: Ensure issue comment clearly explains closure reason
- **Implementation references**: Link to external projects, repos, or PRs where functionality exists
- **Gap acknowledgment**: Be honest about what's implemented vs. what's missing

