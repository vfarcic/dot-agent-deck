# PRD #59: Orchestration Documentation

**Status**: In Progress
**Priority**: High
**Created**: 2026-04-21
**GitHub Issue**: [#59](https://github.com/vfarcic/dot-agent-deck/issues/59)

## Problem Statement

The multi-role agent orchestration system (PRD #58) is fully functional — users can define orchestrations, launch role agents, coordinate workflows, and achieve complex multi-agent tasks. However, no user-facing documentation exists. Users have no clear reference for:
- How to define orchestrations in `.dot-agent-deck.toml`
- What each configuration option (`start`, `description`, `prompt_template`, `clear`) does
- How the delegation and work-done workflow operates
- What the `/delegate` and `/work-done` skills are and how to use them
- Working examples they can copy and adapt (code-review workflow, TDD cycle, etc.)

Without documentation, the orchestration system is a hidden feature that requires reverse-engineering from code or live guidance.

## Solution Overview

Create comprehensive, user-facing documentation that covers:

1. **Orchestration Concepts** — What orchestrations are, when to use them, the agent coordination model
2. **TOML Configuration Guide** — The `[[orchestrations]]` syntax with all options explained
3. **Role Configuration** — Each config option (`name`, `command`, `start`, `description`, `prompt_template`, `clear`) with clear examples
4. **Delegation Protocol** — How `/delegate` and `/work-done` skills work, what messages look like, when to use them
5. **Orchestration Workflow** — Step-by-step walk-through of launching, delegating, and completing an orchestration
6. **Working Examples** — Copy-paste-ready example orchestrations:
   - Code-review: coder writes code, reviewer reviews, optional approval/delegation back to coder
   - TDD cycle: tester writes integration tests, coder implements, tester validates
   - Security audit: coder writes code, security auditor reviews for vulnerabilities
7. **Troubleshooting** — Common issues: agent not responding, `/work-done` skill not found, delegation parsing errors

Documentation should live in the main README or a dedicated `ORCHESTRATION.md` file, following the project's existing documentation style and patterns.

## Success Criteria

- Users can read documentation and understand how to define a basic two-agent orchestration
- All configuration options (`start`, `description`, `prompt_template`, `clear`) are explained with examples
- At least 2 complete, working orchestration examples are provided (code-review, TDD)
- `/delegate` and `/work-done` command syntax and behavior are clearly documented
- Documentation follows existing project style (markdown format, code blocks, links)
- Documentation is accurate and matches the current implementation (PRD #58 Phase 1c)
- Examples have been tested to verify they work as documented
- Users can troubleshoot common problems

## Scope

### In Scope
- User guide to orchestration concepts and terminology
- Complete TOML syntax reference with all options
- `/delegate` and `/work-done` skill reference (invocation, parameters, output)
- Step-by-step workflow explanation with screenshots/diagrams if helpful
- 2–3 complete, tested orchestration examples
- Troubleshooting guide for common issues
- Links to PRD #58 for technical design details
- Integration with existing README or new dedicated documentation file

### Out of Scope
- Implementation of new orchestration features
- Changes to the orchestration system itself
- API reference documentation (covered in code comments)
- Advanced topics like custom agent CLIs or remote agents (documented in PRD #58 as out-of-scope)
- Video tutorials or interactive demos

## Milestones

### M1: Documentation Structure and Concepts
- [ ] Outline documentation structure (sections, headings, flow)
- [ ] Write "What are Orchestrations?" section covering use cases and agent coordination model
- [ ] Write "When to Use Orchestrations" section with decision criteria
- [ ] Document the agent roles concept (orchestrator, worker roles, `start=true`)

### M2: Configuration Reference
- [ ] Document `[[orchestrations]]` TOML syntax
- [ ] Document each role config option: `name`, `command`, `start`, `description`, `prompt_template`, `clear`
- [ ] Include examples for each option
- [ ] Explain default values and validation rules

### M3: Delegation and Work-Done Protocol
- [ ] Document `/delegate` skill: syntax, parameters, how agents invoke it
- [ ] Document `/work-done` skill: syntax, parameters, output format
- [ ] Explain the handoff flow: delegation → prompt injection → agent execution → `/work-done`
- [ ] Document `work-done-{role-name}.md` file format and location

### M4: Complete Workflow Walk-Through
- [ ] Step-by-step explanation of launching an orchestration
- [ ] Visual flow diagram (ASCII or diagram) of orchestrator → delegate → worker → work-done → feedback
- [ ] Explain user interactions at each stage
- [ ] Explain the message bus and prompt injection process

### M5: Code-Review Example Orchestration
- [ ] Create a complete, copy-paste-ready code-review orchestration TOML
- [ ] Document roles: orchestrator, coder, reviewer
- [ ] Document prompts and flow
- [ ] Test the example end-to-end
- [ ] Include expected interaction patterns and CLI output

### M6: TDD Example Orchestration
- [ ] Create a complete, copy-paste-ready TDD orchestration TOML
- [ ] Document roles: orchestrator, tester, coder
- [ ] Document test-first workflow and feedback loop
- [ ] Test the example end-to-end
- [ ] Include expected interaction patterns

### M7: Troubleshooting and FAQ
- [ ] Document common issues: agent not responding, skill not found, delegation parsing errors, role name conflicts
- [ ] Provide solutions and debugging steps
- [ ] Document config validation errors and how to fix them
- [ ] Include tips for effective orchestration design

### M8: Final Review and Polish
- [ ] Review all sections for completeness and accuracy against PRD #58
- [ ] Verify all examples are tested and work correctly
- [ ] Check for typos, grammar, and clarity
- [ ] Ensure links and cross-references are correct
- [ ] Verify documentation follows project style guide
- [ ] Final sign-off

## Key Files

- `README.md` or `ORCHESTRATION.md` — Main documentation file
- Example TOML snippets in documentation
- No code changes required (documentation-only task)

## Success Validation

Documentation is complete and successful when:
1. A user unfamiliar with orchestrations can read the docs and create a working two-agent orchestration
2. All configuration options are explained with examples
3. Working orchestration examples exist and have been tested
4. A user encountering an error can find a solution in the troubleshooting section

## Dependencies

- PRD #58 (Multi-Role Agent Orchestration) must be complete — provides the feature being documented
- Project documentation style guide (existing README.md patterns)

## Risks

- **Accuracy drift**: Implementation may have changed since PRD #58; examples may not match current behavior. Mitigation: Test all examples end-to-end during documentation creation.
- **Complexity**: Orchestration concepts are abstract; documentation may be hard to follow. Mitigation: Use clear step-by-step examples and visual flow diagrams.
- **Incompleteness**: Missing key configuration options or edge cases. Mitigation: Cross-reference PRD #58 and code comments during writing.
