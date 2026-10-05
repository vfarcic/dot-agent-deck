---
name: prd-create
description: Create documentation-first PRDs that guide development through user-facing content
user-invocable: true
---

# PRD Creation Slash Command

## Instructions

You are helping create a Product Requirements Document (PRD) for a new feature. **In this repository the PRD is its GitHub issue**: the issue body is the project management document — milestone tracking and implementation plan — and its comments are the record of progress and decisions. Nothing is written under `prds/` and nothing is committed (issue #1591).

This skill is a project-local fork of the `dot-ai` mirror of the same name (CLAUDE.md rule 13), changed only where it assumed the PRD is a file. The layout of an issue PRD — what goes in the body, what goes in comments, how to edit the body without losing someone else's edit — is in [`../prd-start/issue-prd.md`](../prd-start/issue-prd.md); read it before writing the body. Older PRDs that already have a `prds/<n>-*.md` file keep it; this skill never creates one.

## Process

### Step 1: Understand the Feature Concept
Ask the user to describe the feature idea to understand the core concept and scope.

### Step 2: Create the GitHub Issue FIRST, or Use the One Named
Create the GitHub issue immediately to get the issue ID, with the short initial body below. If the user names an existing issue instead — typically a PRD-labelled issue whose body is only a stub ("Detailed PRD: not written yet") — use that issue and keep its number; its current body is the starting point for the discussion, not something to discard.

**IMPORTANT: Add the "PRD" label to the issue for discoverability.**

**If the existing issue was opened by someone without write access**, do not write the PRD into it: its author can rewrite the body at any time, so `prd-source.sh` never treats it as a PRD. Create a new issue, write the PRD there, and link the original from it.

### Step 3: Write the PRD into the Issue Body
Work through the template sections below with the user, then write the result as the issue body, following [`../prd-start/issue-prd.md`](../prd-start/issue-prd.md) ("The body" and "Editing the body"): write it to `.dot-agent-deck/prd-[issue-id]-body.md` with your file-writing tool and `gh issue edit [issue-id] --body-file` that file. Write as you go — after each agreed section rather than once at the end — so a session that ends early has not lost the discussion.

### Step 4: Create PRD as a Project Management Document
Work through the PRD template focusing on project management, milestone tracking, and implementation planning. Documentation updates should be included as part of the implementation milestones.

**Key Principle**: Focus on 5-10 major milestones rather than exhaustive task lists. Each milestone should represent meaningful progress that can be clearly validated.

**Consider Including** (when applicable to the project/feature):
- **Tests** - If the project has tests, include a milestone for test coverage of new functionality
- **Documentation** - If the feature is user-facing, include a milestone for docs following existing project patterns

**Good Milestones Examples:**
- [ ] Core functionality implemented and working
- [ ] Tests passing for new functionality (if project has test suite)
- [ ] Documentation complete following existing patterns (if user-facing feature)
- [ ] Integration with existing systems working
- [ ] Feature ready for user testing

**Avoid Micro-Tasks:**
- ❌ Update README.md file
- ❌ Write test for function X
- ❌ Fix typo in documentation
- ❌ Individual file modifications

**Milestone Characteristics:**
- **Meaningful**: Represents significant progress toward completion
- **Testable**: Clear success criteria that can be validated
- **User-focused**: Relates to user value or feature capability
- **Manageable**: Can be completed in reasonable timeframe

### Step 5: Confirm It Reads as a PRD
```bash
bash .claude/skills/prd-start/prd-source.sh [issue-id]
```
It must print `SOURCE=issue`. `SOURCE=none` names what is missing in `REASON=` — usually the `## Milestones` section has no checkbox yet. This is the same check `/prd-queue` applies before it will dispatch the PRD, so a body that fails it is a PRD nobody can start.

## GitHub Issue Template

**Initial Issue Creation** — title `PRD: [Feature Name]`; the body is replaced by the full PRD in Step 3:
```markdown
**Priority**: [High/Medium/Low] · **Created**: [YYYY-MM-DD]

## Problem

[1-2 sentence problem description]

## Solution

[1-2 sentence solution overview]
```

**Don't forget to add the "PRD" label to the issue after creation.**

**The full PRD body** is the template in [`../prd-start/issue-prd.md`](../prd-start/issue-prd.md) ("The body"): Problem, Solution, Scope, Success Criteria, Milestones as checkboxes, Risks and Dependencies, and Decisions. Create the issue with `--body-file` too, for the reason that page gives.

## Discussion Guidelines

### PRD Planning Questions
1. **Problem Understanding**: "What specific problem does this feature solve for users?"
2. **User Impact**: "Walk me through the complete user journey — what will change for them?"
3. **Technical Scope**: "What are the core technical changes required?"
4. **Documentation Impact**: "Which existing docs need updates? What new docs are needed?"
5. **Integration Points**: "How does this feature integrate with existing systems?"
6. **Success Criteria**: "How will we know this feature is working well?"
7. **Implementation Phases**: "How can we deliver value incrementally?"
8. **Risk Assessment**: "What are the main risks and how do we mitigate them?"
9. **Dependencies**: "What other systems or features does this depend on?"
10. **Validation Strategy**: "How will we test and validate the implementation?"

### Discussion Tips:
- **Clarify ambiguity**: If something isn't clear, ask follow-up questions until you understand
- **Challenge assumptions**: Help the user think through edge cases, alternatives, and unintended consequences
- **Prioritize ruthlessly**: Help distinguish between must-have and nice-to-have based on user impact
- **Think about users**: Always bring the conversation back to user value, experience, and outcomes
- **Consider feasibility**: While not diving into implementation details, ensure scope is realistic
- **Focus on major milestones**: Create 5-10 meaningful milestones rather than exhaustive micro-tasks
- **Think cross-functionally**: Consider impact on different teams, systems, and stakeholders

**Note**: If any `gh` command fails with "command not found", inform the user that GitHub CLI is required and provide the installation link: https://cli.github.com/

**Note**: If creating the GitHub issue fails because the "PRD" label does not exist, create the label first (`gh label create "PRD" --description "Product Requirements Document" --color 0052CC`) and then retry creating the issue.

## Workflow

1. **Concept Discussion**: Get the basic idea and validate the need
2. **Create GitHub Issue FIRST** (or use the one named): Short concept description, PRD label
3. **Section-by-Section Discussion**: Work through each template section systematically, writing the body as each one is agreed
4. **Milestone Definition**: Define 5-10 major milestones that represent meaningful progress
5. **Confirm It Reads as a PRD**: `prd-source.sh` prints `SOURCE=issue`
6. **Review & Validation**: Ensure completeness and clarity

## ROADMAP.md

This repository has no `docs/ROADMAP.md`, and a PRD's priority is in its issue body. If one is ever added, updating it is a commit, so it would ride the next PR rather than this skill — creating a PRD commits nothing.

## Next Steps After PRD Creation

After completing the PRD, present the user with numbered options:

```
✅ PRD Created Successfully!

**PRD**: #[issue-id] — [issue URL] (the issue body is the PRD; nothing to commit)

What would you like to do next?

**1. Start working on this PRD now**
   Begin implementation immediately (recommended if you're ready to start)

**2. Leave it for later**
   It is already saved on GitHub, where `/prds-get` and `/prd-queue` will find it

Please enter 1 or 2:
```

### Option 1: Start Working Now

---

To start working on this PRD, run `/prd-start [issue-id]`

---

### Option 2: Leave It for Later

```
✅ PRD #[issue-id] is saved in its issue

To start working on it later, execute:
prd-start [issue-id]
```

## Important Notes

- **No commit, no branch, no PR**: the PRD is the issue body, so creating one touches nothing in the repository. That is the point of issue #1591 — `main` is protected (CLAUDE.md rule 8), and a commit to a planning document used to cost a branch, a PR, CI and an approval.
- **The trade-off is review**: a `prds/` file reached `main` through a reviewed PR, and an issue body does not. CLAUDE.md rule 13 records the choice.
- **Issue reference**: commits that implement the PRD reference `prd-[issue-id]` and the PR says `Closes #[issue-id]` (or `Refs #[issue-id]` when it ships only part of it).
