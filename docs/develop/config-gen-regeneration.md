# Config-gen baseline regeneration (PRD #116)

> **Developer / maintainer reference.** This page documents the repeatable workflow behind PRD #116 — regenerating what the AI config generator *would* produce for a project and diffing it against that project's hand-tuned `.dot-agent-deck.toml`, to drive prompt/role-library edits. It is intentionally excluded from the published documentation site and renders as plain Markdown here on GitHub.

The deck ships an AI config generator: in the TUI, the `generate-config` action (default `g`) renders `assets/config_gen_prompt.md` (plus the role library `assets/roles.toml`) for the current project and hands it to an agent pane; the agent explores the repo and proposes an initial `.dot-agent-deck.toml`, which the user then hand-edits. PRD #116 treats **the hand-tuned configs that already live in real projects as the corpus**: regenerate what the generator would produce for a project, diff it by region against that project's live `.dot-agent-deck.toml`, and turn the recurring gaps into targeted prompt/role edits. The live config files *are* the ground truth — there are no captured baselines to store in this repo. This doc is how you re-run the comparison as the prompt evolves or as more projects accumulate (issue #183).

## Where the tooling lives

- `examples/render_config_gen_prompt.rs` — prints the **exact** post-render prompt the deck would send for a directory (`config_gen::config_gen_prompt`, with `{dir}` and `{roles}` interpolated). The auditable "what was sent" capture.
- `examples/diff_config.rs` — region-structured diff between a regenerated config and a project's live config, parsed with the deck's own `ProjectConfig` types so field semantics (defaults for `watch`, `reactive_panes`, `clear`, …) match the running binary. Emits Markdown grouped by region.
- `assets/config_gen_prompt.md`, `assets/roles.toml` — the prompt and role library under study/edit. They are embedded into the binary via `include_str!` in `src/config_gen.rs`, so editing the asset is enough to change behavior (rebuild required for the example to pick it up).

Build the toolchain with the project's pinned Rust (rustup shims are broken in this repo — see the toolchain note in team memory): prefix `PATH` with `.devbox/nix/profile/default/bin`, or run inside `devbox shell`.

## The engine (match production)

Production config-gen feeds the rendered prompt to a live `claude`/`opencode` **agent pane** with full tools — not the `src/llm.rs` API client — so the agent discovers the repo itself. Mirror that when regenerating:

- **Run the agent against the real repo with tools ENABLED**, from the project directory, so it performs the same step-1 discovery production does. This replaces PRD #116's earlier single-shot / filesystem-disabled capture, which required a hand-authored project snapshot that drifted from the real repo — the agent now reads the live repo directly, and there is nothing to maintain.
- **Model — `claude-haiku-4-5`** (the deck's documented default, `src/config.rs`). Don't substitute Sonnet/Opus: a stronger model understates the baseline gap a real user would see.
- **Determinism — not pinnable.** The `claude` CLI exposes no temperature flag, and tool-driven exploration adds variance. Don't chase exact reproducibility: reason only about **structural** deltas (role presence, pane count, release-flow shape, command specificity) and trust **cross-project recurrence** — a delta seen in 2+ independent projects — over any single run. Re-run 2–3× when a delta's stability is in doubt.

## Running it (one project)

```bash
# 0. Build the toolchain into PATH (rustup shims are broken here).
export PATH="$PWD/.devbox/nix/profile/default/bin:$PATH"

DIR=/home/vfarcic/code/dot-ai          # the real project to analyze

# 1. Render the exact prompt the deck would send for that project.
cargo run --quiet --example render_config_gen_prompt -- "$DIR" > /tmp/gen-input.md

# 2. Append a short non-interactive override so the run emits a config instead of
#    negotiating with a user — and, since tools are ON against a REAL repo, forbid
#    it from writing the project's actual .dot-agent-deck.toml.
cat >> /tmp/gen-input.md <<'EOF'

---

## NON-INTERACTIVE CAPTURE (overrides the interactive steps above)
- You ARE allowed to read the repo with your tools — perform step 1's discovery yourself.
- Do NOT ask the user anything and do NOT wait for confirmation; skip step 5's negotiation.
- Do NOT write or modify ANY file in the repo. You have write tools but must not use them — skip step 6's file write entirely; only print the config.
- Output a short (≤1 paragraph) rationale, then the COMPLETE proposed `.dot-agent-deck.toml` (modes, plus an orchestration if one applies) in a single fenced ```toml block, and nothing after it.
EOF

# 3. Generate from INSIDE the repo with tools enabled, model pinned to the deck default.
( cd "$DIR" && claude -p --model claude-haiku-4-5 < /tmp/gen-input.md ) > /tmp/raw-output.md

# 4. Extract the FIRST fenced ```toml block as the regenerated config.
#    Robust against a stray second ```toml block: capture only the first and exit
#    the instant it closes, so nothing after the first closing fence is appended.
awk '
  /^```toml/ && !started { started=1; infile=1; next }
  infile && /^```/       { exit }
  infile                 { print }
' /tmp/raw-output.md > /tmp/regenerated.toml

# 5. Diff the regenerated config against the project's LIVE config, by region.
cargo run --quiet --example diff_config -- /tmp/regenerated.toml "$DIR/.dot-agent-deck.toml"
```

Everything lands in `/tmp` (or `$CLAUDE_JOB_DIR/tmp`) — nothing is committed to this repo. After editing `assets/config_gen_prompt.md` / `assets/roles.toml`, rebuild and re-run steps 1–5 to confirm the regenerated config moved closer to the live one.

### Diff-tool pairing caveat

`diff_config` matches modes and orchestrations **by name**, and the generator picks those names nondeterministically (the prompt does not — and should not — dictate them). When a regenerated name differs from the live config's, the tool reports the roles as disjoint (all B-only + U-only) instead of pairing them field-by-field, which *understates* the real overlap. To get a role-paired diff, copy the regenerated config and align only the cosmetic mode/orchestration `name = "…"` lines to the live config's names before diffing. (`name` lines for modes/orchestrations don't collide with role names, so a targeted string replace is safe.)

## The date gate (apply before treating any delta as a signal)

A delta is only a user signal if the relevant prompt/role capability **existed when the user last edited the config**. Otherwise the divergence is time-drift — "generated by an older prompt and never regenerated" — not a preference. Before treating any region's delta as evidence:

- Find the config's last-edit date: `git -C <project> log -1 --format=%ci -- .dot-agent-deck.toml`.
- Find when the relevant prompt capability shipped in **this** repo: `git log` over `assets/config_gen_prompt.md` / `assets/roles.toml` (e.g. the prompt began *proposing orchestrations* on 2026-04-27, commit `3b83478`; the orchestration *engine* landed 2026-04-21).
- If the config predates the capability, **exclude** that region from the comparison and flag it as drift, not preference.

A pattern is worth a prompt edit only if it (a) clears the date gate **and** (b) appears in 2+ independent projects **or** is an obvious universal improvement. Single-project structural choices and nondeterminism artifacts are noted but not acted on.

## Recording findings

Keep the distilled conclusions — which deltas recurred, which prompt/role edits they motivated, and the before/after validation — in the PRD (e.g. under `prds/done/`) as the provenance record. Do **not** add per-project captures, baselines, or diffs to this repo: the corpus is the live `.dot-agent-deck.toml` files in the real projects, and the comparison is cheap to re-run from scratch.
