# Adaptive issue and pull request labeling

The repository uses a GitHub Agentic Workflow to classify newly opened issues and same-repository pull requests. The live workflow is disabled unless the repository variable `ISSUE_LABELER_ENABLED` is exactly `true`; reusable calls from the manual batch workflow bypass that activation gate so maintainers can preview and test the classifier before enabling automatic runs.

## Files and ownership

- `.github/workflows/issue-labeler.md` is the human-maintained live agentic workflow. It applies labels, reads and writes correction memory, and handles automatic events plus reusable single-item calls.
- `.github/workflows/issue-labeler-preview.md` is the human-maintained read-only preview worker. It has no repo-memory tool, stages every safe output, and disables generated diagnostic issues, so it cannot apply labels, create issues, or mutate learning history.
- `.github/workflows/issue-labeler-batch.yml` accepts a type and up to 20 comma- or space-separated numbers. It fans out one isolated reusable-workflow run per item, previews by default, and calls the live worker only when `apply` is explicitly enabled.
- `*.lock.yml` files are generated GitHub Actions workflows. Never edit them directly.

Both agentic workflow sources are modified derivatives of [`dosu-ai/auto-label`](https://github.com/dosu-ai/auto-label/tree/83084acbe5cdf17fa2be717ea054f1681635f7a4), licensed under Apache-2.0. Attribution and the applicable license are in [`THIRD_PARTY_NOTICES.md`](../../THIRD_PARTY_NOTICES.md) and [`LICENSES/Apache-2.0.txt`](../../LICENSES/Apache-2.0.txt). The repository and its original code remain MIT-licensed under the root `LICENSE`.

## Taxonomy

The safe-output allowlist is the enforcement boundary. The model can request only these labels, and a requested label must also exist in the repository.

| Group | Labels | Rule |
| --- | --- | --- |
| Type | `bug`, `feature`, `enhancement`, `documentation`, `question` | At most one. `feature` is new user-visible behavior; `enhancement` improves existing behavior or maintainer tooling. |
| Area | `source`, `config`, `dependencies`, `tests`, `ci-cd`, `devbox` | Zero or more when the content makes the area explicit. The deterministic pull request path labeler may apply these first. |
| Component | `daemon`, `tui`, `desktop` | Zero or more. A **different axis** from Area: Area says what kind of file changed, Component says which of the three shipped surfaces it belongs to — the background daemon, the terminal front-end, or the Tauri GUI under `desktop/`. Multiple are normal, because a protocol change is routinely `daemon` plus every client that consumes it. Absent for work that is genuinely surface-independent. |
| Priority | `priority:high`, `priority:medium`, `priority:low` | Issues only. High is reserved for security, data loss, release blockers, or widespread core breakage. |
| Size | `size:high`, `size:medium`, `size:low` | One when estimable. Low is less than a day, medium is roughly one to three days, and high is broader architectural or multi-part work. |
| Triage | `needs-triage` | Issues only, when there is not enough information to choose a type or priority confidently. |

The per-item cap is **6**, raised from 5 when the Component axis was added: a fully classified issue can legitimately carry a type, an area, a component, a priority and a size, and leaving the cap at 5 would have made the new axis silently evict an existing label rather than sit alongside it.

The workflow cannot apply authority or lifecycle labels including `PRD`, `duplicate`, `good first issue`, `help wanted`, `invalid`, `manual-review`, `stale`, or `wontfix`. Those decisions remain human-owned.

## Who owns the triage taxonomy (PRD #421)

[PRD #421](https://github.com/vfarcic/dot-agent-deck/blob/main/prds/421-issue-triage-labels-and-dispatch-claims.md) proposes a second mechanism for the same `priority:*` / `size:*` / `needs-triage` vocabulary: a `triage-issues` repo skill invoked from a `[[scheduled_tasks]]` cron entry on a maintainer's deck. **Precedence, as of this workflow: the workflow owns classification labeling for this repository — type, area, priority, size, and `needs-triage`, on issues and pull requests.** PRD #421's Phase 1 is not enabled against this repository, and enabling it here alongside `ISSUE_LABELER_ENABLED` is a configuration error, not a redundancy.

**The dispatch claim is a different job and does not collide.** PRD #421's Phases 2 and 3 — the deck-owned `agent-deck/claimed` label, the claimant record, and claim-aware `dispatch_decision` — are outside this workflow entirely, and structurally so rather than by convention: the safe-output `allowed` list is a closed allowlist, so the workflow cannot apply `agent-deck/claimed` whatever the model proposes, and it has no label-removal capability at all. Nothing here needs to change when those phases land.

**Why the two classification mechanisms must not both run.** The failure is worse than duplicate work:

- **Divergent vocabulary, not just double-labeling.** This repository's live labels are colon-separated (`priority:high`), which is what this workflow's allowlist enforces; PRD #421 records the hyphenated form (`priority-high`) as settled. Both mechanisms running would create and apply two parallel taxonomies on the same issue, and neither can read the other's.
- **Correction memory would be poisoned.** This workflow treats every non-bot `labeled` / `unlabeled` event as maintainer ground truth. A local triage agent applies labels through `gh` under a maintainer's own credential, so GitHub reports the sender as a `User` and the bot skip in the activation condition does not fire — the second mechanism's output would be ingested and learned from as if a human had corrected the classifier.
- **Every such event also costs a run.** Feedback-mode runs are metered against the same rolling AI-credit guardrail as classification.

**What PRD #421 Phase 1 still does that this does not**, and the reason the decision belongs to the maintainers rather than to this document: it sweeps the *existing* backlog automatically. This workflow is event-driven, so it never reaches an issue opened before it was enabled; retro-triage is the batch workflow's manual 20-item runs. Against that, one premise of PRD #421's design has since changed — its "Why the triage classifier runs locally" rests on there being no LLM credential in CI "and no intent to add one", and the `OPENAI_API_KEY` this workflow uses is that intent arrived. The PRD's own durability note cuts the same way: its schedule lives in `~/.config/dot-agent-deck/schedules.toml` on one machine and "vanishes with the machine", where this workflow's trigger is versioned in the repository.

Whichever way that is settled, the interlock above holds: **exactly one mechanism applies the classification taxonomy to this repository at a time**, and the taxonomy it applies is the one the repository's labels are actually named for.

## Preview, apply, and activation

Run **Issue labeler batch** from the Actions tab. Choose `issue` or `pull_request`, supply up to 20 numbers, and leave `apply` disabled. Each item gets its own run, and proposed labels appear in the run summary with no issue, pull-request, or memory writes. The parser rejects empty input, non-positive integers, and lists longer than 20; duplicate numbers are collapsed.

The preview job grants its reusable workflow `actions: write` because the compiler-generated conclusion job persists the AI-credit cache. The agent job receives a read-only token and read-only GitHub tools, generated diagnostic issues are disabled, and `staged: true` prevents the separate safe-output job from applying labels.

Review the preview across obvious bugs, features, documentation, ambiguous reports, and differently sized work before using `apply`. An apply run invokes the live worker, adds accepted labels, and records predictions for future correction learning. Set the repository variable only after the sample is acceptable **and after confirming no PRD #421 `triage-issues` schedule is pointed at this repository** (see the section above — a second labeler is read as maintainer feedback, not as a duplicate):

```sh
gh variable set ISSUE_LABELER_ENABLED --body true
```

Disable automatic runs without editing or redeploying workflows:

```sh
gh variable delete ISSUE_LABELER_ENABLED
```

Automatic pull request classification intentionally skips forks because it uses `OPENAI_API_KEY`. The existing deterministic path labeler continues to label pull requests independently.

## Learning behavior

The live workflow stores `feedback-issue.jsonl`, `feedback-pull_request.jsonl`, and `predictions.jsonl` on the dedicated `memory/issue-labeler` branch. The branch is operational state, not source history, and should not be merged into `main`. Before the agent runs, the workflow snapshots those files outside the writable agent mount. A post-agent validator rejects other filenames, malformed JSONL, unexpected fields, non-taxonomy labels, wrong item URLs, entries beyond the documented limits, and any change that is not exactly the prediction or correction implied by the triggering event and structured classifier output.

Every live classification records its selected labels, including an empty label array when the model abstains. The prediction represents the classifier decision rather than proof that the separate GitHub label write succeeded. The empty record is important: if a maintainer later adds a selectable label, the workflow can learn from the complete false negative. Human additions and removals of selectable labels snapshot the item's current selectable labels and upsert one feedback entry by URL. Bot-generated label events are rejected before the agent job starts so labels applied by Actions do not spend credits or teach the model; items the workflow never classified and changes to blocked labels also do not teach it.

The validator is the workflow's one safety property that does not rest on the model behaving, so it is covered by tests rather than by inspection. `xtask/linkage-check/src/issue_labeler_memory.rs` extracts the step's script from the Markdown source and runs it under `node` against fixture memory trees — the accepts it must not block (a labeling-mode prediction; a correction carrying more labels than the 6 a prediction may, since those two caps differ deliberately) and the rejects it exists for (a row smuggled in for an untouched issue, a row for another repository, a non-taxonomy label, an extra file in the clone, feedback about an item the workflow never classified, and a feedback-mode run that tries to apply labels). The validator carries its own copy of the taxonomy and of the prediction cap, because it bounds what may be *stored* rather than what may be applied; `issue_labeler_validator_policy_matches_the_frontmatter` asserts that copy still agrees with `add-labels.max` and `add-labels.allowed`, so raising the cap or widening the allowlist without teaching the validator fails the suite rather than failing the workflow at run time. A companion test asserts the extracted script is embedded verbatim in the generated `.lock.yml`, so an edit that lands without a recompile fails the suite instead of leaving it testing a script no runner executes. They run in `cargo test-fast` and skip with a `SKIP:` line where `node` is unavailable.

The `Extract label policy for the agent` setup step is covered the same way, in `xtask/linkage-check/src/issue_labeler_policy.rs`, and for a reason worth recording: it had never once produced `/tmp/gh-aw/agent/label-policy.json`. Its search was `^\s*allowed:` over the whole frontmatter, where three keys are spelled `allowed:` — `network`'s `[defaults]` comes first, then the tools list, and only then the label list — so it matched `[defaults]`, failed to parse it as JSON, and took the `bail()` path, which prints and **exits 0**. Every run silently fell back to reading the workflow source, which the prompt reserves for the file being missing, and nothing went red because nothing asserted the step's output. The search is now scoped to the `add-labels:` block, and the tests drive the real script under `python3` against both this repository's own frontmatter and a synthetic one carrying the decoy keys.

For sufficiently large stores, a deterministic BM25 prefilter ranks historical feedback before the model selects a small set of relevant examples. Cold stores fall back to direct recent-example selection. Stored issue and pull request text remains untrusted data and must never be followed as instructions.

## Maintaining the workflows

Install the authoring compiler locally; it is not a project runtime dependency and does not belong in `devbox.json`:

```sh
gh extension install github/gh-aw --pin v0.86.2
```

After editing either Markdown source, regenerate and validate both locks with the pinned compiler:

```sh
gh aw compile issue-labeler issue-labeler-preview --validate --strict --approve
```

Then run the additional workflow checks and normal repository gates:

```sh
gh aw compile issue-labeler issue-labeler-preview --validate --strict --no-emit
cargo test-fast
cargo fmt --check
cargo clippy --workspace --all-targets --features e2e -- -D warnings
```

The Codex engine uses `gpt-5-mini` and reads the repository Actions secret `OPENAI_API_KEY`. GitHub reads use `gh-aw`'s `gh-proxy` mode with the agent job's read-only token; labels and memory are written later by separate compiler-generated jobs. Threat detection is fail-closed, so detected prompt injection or a detector failure blocks both safe outputs and memory persistence. Rotate or replace the OpenAI secret through GitHub's secret interface; never place its value in source, workflow inputs, logs, or model-visible environment variables. The checked-in `.env.vals.yaml` entry is only a local `vals` reference to the external secret.

## Cost and quality checks

The upstream project reported roughly US$0.10 per event with its default Claude model, which is context rather than a budget promise for this Codex configuration. A private trial on 2026-08-20 classified a synthetic crash report as `bug` with high confidence, consumed 0.736 AI credits in 27.5 seconds of model time, and consumed another 0.454 credits for threat detection. The staged output reported that it would add `bug`, and a direct read confirmed the issue remained unlabeled. The first trial also caught an inefficient MCP invocation path before release; switching to `gh-proxy` reduced the classifier from 5.680 credits and an abstention to the successful 0.736-credit run. The classifier and threat detector are each capped at 10 AI credits, for a 20-credit maximum per item. Automatic runs share a 100-credit rolling daily guardrail; intentional `workflow_dispatch` and `workflow_call` runs bypass that gh-aw guardrail, so a maintainer-triggered 20-item batch has a theoretical 400-credit ceiling and must be sized deliberately. Recheck actual usage and classification quality across a repository-specific sample before activation because compilation and smoke tests establish wiring and safety, not accuracy.
