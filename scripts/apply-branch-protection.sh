#!/usr/bin/env bash
set -euo pipefail

# Apply (or remove) the `main` branch ruleset that requires changes to land via
# a reviewed pull request.
#
#   scripts/apply-branch-protection.sh status   # show what is configured now
#   scripts/apply-branch-protection.sh apply    # create/update the ruleset
#   scripts/apply-branch-protection.sh delete   # remove it (full override)
#
# READ docs/develop/governance.md BEFORE RUNNING `apply`. In particular:
#
#   1. `main` currently takes two direct pushes from CI — the changelog commit
#      in release.yml and the docs chart bump in docs-publish.yml. Both are
#      rejected with `GH006` under this ruleset unless the RELEASE_TOKEN secret
#      is set to an admin PAT. Applying this without that secret breaks the very
#      next release, exactly as it did for v0.35.6 (CLAUDE.md rule 8).
#   2. REQUIRED_APPROVALS=1 needs a second maintainer to be useful. Nobody can
#      approve their own pull request, so while there is one collaborator every
#      pull request that person opens is unmergeable without a bypass. Onboard
#      the second maintainer (MAINTAINERS.md) before applying with approvals=1,
#      or apply with REQUIRED_APPROVALS=0 to require a PR but not a review.
#   3. `apply` sends a full `PUT`, so it REPLACES the ruleset rather than
#      merging into it: every rule and bypass actor that payload() does not
#      emit is deleted, and the call still reports success. Anything configured
#      in the GitHub UI but absent here is therefore lost on the next `apply` —
#      keep payload() in step with the live ruleset, and run `status`
#      afterwards to confirm what survived.

REPO="${REPO:-vfarcic/dot-agent-deck}"
# Overridable, like every other tunable here. existing_ruleset_id's ambiguity
# error tells the operator to set this to disambiguate duplicate rulesets, so an
# unconditional assignment would make that documented recovery path a dead end.
RULESET_NAME="${RULESET_NAME:-main-protected}"

# Require an approving review before merge. Set to 0 to require a PR but no
# approval (useful as a first step while there is only one maintainer).
#
# GitHub counts an approval only from an account with write or admin permission,
# so "one approving review" already means "a maintainer approved" — the
# collaborator list is the maintainer list (MAINTAINERS.md).
#
# `.github/CODEOWNERS` exists but is not a second gate: it holds a single
# pathless rule (`* @vfarcic @prageethw`) and `require_code_owner_review` stays
# false below, so any maintainer's approval satisfies this count. It is there
# purely as a ROUTER — GitHub omits the author when auto-requesting review from
# code owners, so the pathless rule requests the other maintainer on every pull
# request without anyone remembering a flag. What was deliberately rejected is
# *per-path* ownership: with one shared maintainer set there is nothing to
# route, and a hardcoded path list goes stale silently on every rename.
REQUIRED_APPROVALS="${REQUIRED_APPROVALS:-1}"

# Repository-role bypass. Role id 5 is `admin`.
#
# `always` lets an admin (and any token acting as one, including the
# RELEASE_TOKEN PAT that CI uses) push directly. This is what keeps releases
# working. The cost is honest and worth stating: enforcement against the owner's
# own hands is then a matter of habit, not of mechanism. The stricter
# alternative is to drop this bypass and give CI a GitHub App token as the
# bypass actor instead — note that the default GITHUB_TOKEN *cannot* be a bypass
# actor on a user-owned repo (`422: Actor GitHub Actions integration must be
# part of the ruleset source or owner organization`).
ADMIN_BYPASS_MODE="${ADMIN_BYPASS_MODE:-always}"

# Let the Renovate GitHub App bypass the pull_request rule.
#
# Renovate is an App, not a collaborator, so the RepositoryRole bypass above does
# not cover it — apps are a separate actor_type. Without this entry, raising
# REQUIRED_APPROVALS to 1 silently stalls every automerge group in renovate.json
# (Rust patch crates, Rust minors at >=1.0, devbox packages, GitHub Actions, and
# the docs-site npm deps), because a bot cannot approve its own pull request and
# GitHub counts approvals only from write/admin accounts. Nothing errors; the
# pull requests simply sit there, which is a slow and confusing way to find out.
#
# `pull_request` mode rather than `always`: Renovate may merge a pull request
# that lacks the required approvals, but still cannot push directly to main. That
# is strictly narrower than the admin bypass above.
#
# Defaults on. While REQUIRED_APPROVALS=0 there is nothing to bypass, so enabling
# it early is inert — and the alternative is remembering this at the exact moment
# a second maintainer is onboarded, which is precisely when attention is
# elsewhere. Set RENOVATE_BYPASS=false to leave it out and review every
# dependency bump by hand instead.
RENOVATE_BYPASS="${RENOVATE_BYPASS:-true}"
# `gh api /apps/renovate --jq .id` -> 2740 (the public Renovate app).
RENOVATE_APP_ID="${RENOVATE_APP_ID:-2740}"

# The status checks that must pass before a pull request may merge, as a
# space-separated list of check-run names. Added to the live ruleset on
# 2026-08-11; see docs/develop/governance.md step 6.
#
# These four are the jobs in ci.yml that establish *objective* correctness, two
# of which (`build-macos`, `build-windows`) no local gate can replace at all.
# `Greptile Review` is deliberately absent even though its check-run has a real
# pending state and would be safe to require: an approval is the judgment call,
# and deliberately waiving a Greptile finding is a legitimate approval, so
# requiring the reviewer would turn advice into a veto.
#
# The names must match ci.yml's job ids exactly — those jobs carry no `name:`
# override, so the job id is the check name. A context that never reports is
# worse than one that fails: it leaves the pull request permanently unmergeable
# with nothing red to fix (governance.md records #416, a fork whose workflows
# had never run). A fork of this repository whose CI does not produce these four
# check names therefore needs a way to apply the pull-request gate without them,
# which is what ALLOW_NO_REQUIRED_CHECKS below exists for.
#
# `-` rather than `:-`, unlike the tunables above: with `:-` an explicitly empty
# value falls back to the default, which would make that escape hatch a dead
# end. Unset still means "the four defaults".
# `e2e-deterministic` joined the four on 2026-09-05 (issue #908): a lane nothing
# requires is a lane that stays red, and it was red on 16 of 21 open PRs when
# that was measured. EDITING THIS DEFAULT DOES NOT CHANGE THE LIVE REPOSITORY —
# the ruleset only moves when someone runs `apply`, which sends a full `PUT`
# (warning 3 above). Do not run `apply` until the open pull requests have
# rebased past the flake fixes, or every one of them becomes unmergeable at
# once; then run `status` and read the contexts back.
REQUIRED_CHECKS="${REQUIRED_CHECKS-build build-macos build-windows security e2e-deterministic}"

# Acknowledge that an empty REQUIRED_CHECKS is intended. Without this set to
# `true`, an empty (or whitespace-only) REQUIRED_CHECKS is a hard error rather
# than an instruction to omit the rule.
#
# The extra step is the point. Because `apply` sends a full `PUT`, omitting the
# rule does not merely decline to add it — it STRIPS every required status check
# off a live repository, with no error, no warning and a success exit: the same
# class of silent weakening that warning 3 above exists for. An empty
# environment variable is an ordinary way to arrive there by accident (a CI step
# with an unset variable, a sourced env file, a mistyped export), so the
# dangerous branch is made affirmative: reaching it takes a second, plainly
# named variable that reads as a decision.
#
# This flag never removes anything by itself. With REQUIRED_CHECKS non-empty it
# is inert and the contexts are emitted exactly as they would be otherwise; all
# it does is permit what an empty REQUIRED_CHECKS already asked for.
ALLOW_NO_REQUIRED_CHECKS="${ALLOW_NO_REQUIRED_CHECKS:-false}"

# Whether a pull request must be up to date with `main` before merging. `false`
# deliberately: with a dozen pull requests open, `true` means near-continuous
# rebasing for no correctness gain, and ci.yml re-verifies `main` after every
# merge anyway (its `push:` trigger) so a bad interaction still surfaces.
STRICT_REQUIRED_CHECKS="${STRICT_REQUIRED_CHECKS:-false}"

usage() { sed -n '4,28p' "$0" >&2; exit 64; }

# Render the ruleset's bypass_actors array. Kept out of the payload heredoc
# because the Renovate entry is conditional.
bypass_actors_json() {
  printf '{ "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "%s" }' \
    "$ADMIN_BYPASS_MODE"
  if [ "$RENOVATE_BYPASS" = "true" ]; then
    case "$RENOVATE_APP_ID" in
      ''|*[!0-9]*)
        echo >&2
        echo "error: RENOVATE_APP_ID must be numeric, got '$RENOVATE_APP_ID'." >&2
        return 1
        ;;
    esac
    printf ',\n    { "actor_id": %s, "actor_type": "Integration", "bypass_mode": "pull_request" }' \
      "$RENOVATE_APP_ID"
  fi
}

# Render the `required_status_checks` rule, or nothing when REQUIRED_CHECKS is
# empty *and* ALLOW_NO_REQUIRED_CHECKS says that is intended — an empty value on
# its own is refused here rather than quietly producing a checkless ruleset.
# Emits its own leading comma so payload()'s `rules` array stays valid JSON
# either way — the same shape bypass_actors_json uses for its conditional entry,
# and kept out of the payload heredoc for the same reason.
required_status_checks_rule_json() {
  local ctx first=true
  case "$STRICT_REQUIRED_CHECKS" in
    true|false) ;;
    *)
      echo >&2
      echo "error: STRICT_REQUIRED_CHECKS must be true or false, got '$STRICT_REQUIRED_CHECKS'." >&2
      return 1
      ;;
  esac
  # Validated rather than compared with `= true` alone: a typo like `yes` would
  # otherwise fail closed into the refusal below, whose message then blames
  # REQUIRED_CHECKS for a mistake made in this variable.
  case "$ALLOW_NO_REQUIRED_CHECKS" in
    true|false) ;;
    *)
      echo >&2
      echo "error: ALLOW_NO_REQUIRED_CHECKS must be true or false, got '$ALLOW_NO_REQUIRED_CHECKS'." >&2
      return 1
      ;;
  esac
  # Unquoted on purpose: word splitting is what turns the space-separated list
  # into one entry per context. `set -f` around it because unquoted expansion
  # also globs, and a stray `*` would otherwise become a list of filenames
  # instead of failing the validation below.
  set -f
  # shellcheck disable=SC2086
  set -- $REQUIRED_CHECKS
  set +f
  # Tested on the word count after splitting rather than with
  # `[ -z "$REQUIRED_CHECKS" ]`, so a whitespace-only value reaches the same
  # decision instead of slipping through as "omit the rule".
  if [ "$#" -eq 0 ]; then
    if [ "$ALLOW_NO_REQUIRED_CHECKS" != true ]; then
      echo >&2
      echo "error: REQUIRED_CHECKS is empty, which emits a ruleset with NO required status" >&2
      echo "checks. \`apply\` sends a full PUT, so on $REPO that REMOVES every check the" >&2
      echo "ruleset currently requires — silently, and with a success exit." >&2
      echo >&2
      echo "If a checkless ruleset is what you want (a fork whose CI produces different job" >&2
      echo "names, and which wants the pull-request gate without them), say so explicitly:" >&2
      echo >&2
      echo "  REQUIRED_CHECKS= ALLOW_NO_REQUIRED_CHECKS=true $0 apply" >&2
      echo >&2
      echo "Otherwise leave REQUIRED_CHECKS unset for this repository's four defaults, or set" >&2
      echo "it to your own job names. See docs/develop/governance.md." >&2
      return 1
    fi
    return 0
  fi
  for ctx in "$@"; do
    # A `"` or `\` would interpolate into the payload as JSON rather than as a
    # check name, the same class of problem the RENOVATE_APP_ID check catches.
    case "$ctx" in
      *'"'* | *\\*)
        echo >&2
        echo "error: REQUIRED_CHECKS entry '$ctx' contains a quote or backslash." >&2
        return 1
        ;;
    esac
  done
  printf ',\n    {\n      "type": "required_status_checks",\n      "parameters": {\n'
  printf '        "strict_required_status_checks_policy": %s,\n' "$STRICT_REQUIRED_CHECKS"
  printf '        "do_not_enforce_on_create": false,\n'
  printf '        "required_status_checks": [\n'
  for ctx in "$@"; do
    if [ "$first" = true ]; then first=false; else printf ',\n'; fi
    printf '          { "context": "%s" }' "$ctx"
  done
  printf '\n        ]\n      }\n    }'
}

# Echo the id of the `$RULESET_NAME` ruleset, or nothing if it does not exist.
#
# Deliberately no `2>/dev/null || true`: an auth, rate-limit or network failure
# must not be indistinguishable from "no such ruleset". `cmd_delete` treats an
# empty id as "nothing to remove" and reports success — if a failed lookup
# produced that empty string, the emergency override would claim to have lifted
# protection that is in fact still active. Callers assign on their own line
# (`local id` then `id="$(…)"`), so a non-zero return here propagates under
# `set -e` rather than being masked by `local`'s own exit status.
existing_ruleset_id() {
  local out count
  if ! out="$(gh api "repos/$REPO/rulesets" \
      --jq ".[] | select(.name == \"$RULESET_NAME\") | .id")"; then
    echo "error: could not list rulesets on $REPO." >&2
    echo "Refusing to guess whether $RULESET_NAME exists — check auth (gh auth status)," >&2
    echo "rate limits, and network, then retry." >&2
    return 1
  fi
  # GitHub identifies rulesets by numeric id and does not require names to be
  # unique, so this filter can match more than one. Returning them all would
  # interpolate `123\n456` into a URL; refusing is better than picking one,
  # because updating or deleting a single match while a same-named sibling keeps
  # enforcing is the same false-success failure the error handling above exists
  # to prevent.
  count="$(printf '%s' "$out" | grep -c . || true)"
  if [ "$count" -gt 1 ]; then
    echo "error: $count rulesets on $REPO are named '$RULESET_NAME':" >&2
    while IFS= read -r rid; do
      [ -n "$rid" ] && echo "  id=$rid" >&2
    done <<<"$out"
    echo "Refusing to act on an ambiguous target. Remove or rename the duplicates in" >&2
    echo "Settings > Rules, or set RULESET_NAME to address a specific one." >&2
    return 1
  fi
  printf '%s' "$out"
}

payload() {
  local actors checks_rule
  # `|| return 1` explicitly, rather than leaning on `set -e` to abort the
  # assignment. Bash ignores errexit inside a command substitution used as an
  # assignment's right-hand side, and *"if a shell function executes in a
  # context where -e is being ignored, none of the commands executed within the
  # function body will be affected by the -e setting"* — so in a caller written
  # as `body="$(payload)"` an errexit-only guard is silently inert: the refusal
  # below is printed, `checks_rule` is left empty, and payload happily emits a
  # ruleset with the required-checks rule missing. That is the very failure this
  # validation exists to prevent, reintroduced by the shape of the call site.
  # An explicit return propagates from a pipeline, a substitution and a direct
  # call alike, so no caller can un-arm it.
  actors="$(bypass_actors_json)" || return 1
  checks_rule="$(required_status_checks_rule_json)" || return 1
  cat <<JSON
{
  "name": "$RULESET_NAME",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [
    $actors
  ],
  "conditions": {
    "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] }
  },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": $REQUIRED_APPROVALS,
        "require_code_owner_review": false,
        "dismiss_stale_reviews_on_push": true,
        "require_last_push_approval": false,
        "required_review_thread_resolution": true,
        "allowed_merge_methods": ["merge", "squash", "rebase"]
      }
    }$checks_rule
  ]
}
JSON
}

# Echo "set" or "unset" for the RELEASE_TOKEN secret. Returns non-zero if the
# lookup itself failed, so callers can distinguish "the secret is missing" from
# "we could not find out" — the same distinction existing_ruleset_id preserves.
release_token_state() {
  local names
  if ! names="$(gh secret list --repo "$REPO" --json name --jq '.[].name')"; then
    return 1
  fi
  if printf '%s\n' "$names" | grep -qx 'RELEASE_TOKEN'; then
    echo set
  else
    echo unset
  fi
}

cmd_status() {
  echo "== rulesets on $REPO =="
  local listed
  # Two distinct outcomes that must not be conflated: `gh api --jq` exits 0 with
  # empty output when there are genuinely no rulesets, and non-zero when the
  # call failed. Printing "(none)" for the latter would report the branch as
  # unprotected when it may well be protected.
  if ! listed="$(gh api "repos/$REPO/rulesets" \
      --jq '.[] | "\(.id)  \(.name)  [\(.enforcement)]"')"; then
    echo "error: could not list rulesets on $REPO — protection state is UNKNOWN." >&2
    return 1
  fi
  echo "${listed:-(none)}"
  local id
  id="$(existing_ruleset_id)"
  if [ -n "$id" ]; then
    echo
    echo "== rules in $RULESET_NAME =="
    # `required_status_checks` prints its contexts rather than just its type:
    # the contexts are the half of this ruleset most easily lost to a partial
    # `apply` (see warning 3 in the header), so "the rule is present" is not
    # enough to confirm the gate is intact.
    gh api "repos/$REPO/rulesets/$id" --jq '
      .rules[]
      | if .type == "required_status_checks" then
          "\(.type): \([.parameters.required_status_checks[].context] | join(", "))"
        else .type end'
    echo
    echo "== bypass actors =="
    gh api "repos/$REPO/rulesets/$id" \
      --jq '.bypass_actors[]? | "\(.actor_type) id=\(.actor_id) mode=\(.bypass_mode)"'
  fi
  echo
  echo "== RELEASE_TOKEN secret =="
  local token_state
  if ! token_state="$(release_token_state)"; then
    echo "UNKNOWN — could not list secrets on $REPO. Do not apply until this resolves." >&2
    return 1
  fi
  if [ "$token_state" = set ]; then
    echo "set — CI can bypass"
  else
    echo "NOT SET — applying this ruleset will break the next release and /publish-docs"
  fi
}

cmd_apply() {
  local token_state
  if ! token_state="$(release_token_state)"; then
    echo "refusing to apply: could not determine whether RELEASE_TOKEN is set on" >&2
    echo "$REPO. Check auth and network — applying blind risks locking CI out of" >&2
    echo "main. See docs/develop/governance.md." >&2
    exit 1
  fi
  if [ "$token_state" != set ]; then
    echo "refusing to apply: RELEASE_TOKEN is not set on $REPO." >&2
    echo "release.yml and docs-publish.yml push directly to main; without an" >&2
    echo "admin PAT they will fail with GH006. See docs/develop/governance.md." >&2
    exit 1
  fi
  local id body
  id="$(existing_ruleset_id)"
  # Rendered into a variable before any API call rather than piped straight into
  # `gh`. payload() can refuse (a non-numeric RENOVATE_APP_ID, an unacknowledged
  # empty REQUIRED_CHECKS, a context carrying a quote), and piped it would still
  # have started `gh api --input -` and handed it empty stdin, leaving "is
  # anything sent" up to how `gh` treats an empty body. Rendering first means a
  # refusal aborts with nothing transmitted at all.
  #
  # What aborts it is payload()'s own `|| return 1`, NOT `set -e` — errexit is
  # ignored inside this substitution (see the note on payload). Keep that return
  # there; without it this line silently assigns a ruleset missing whichever
  # rule failed to render, which is the exact failure the validation exists for.
  body="$(payload)"
  if [ -n "$id" ]; then
    echo "updating ruleset $id ($RULESET_NAME)"
    printf '%s\n' "$body" | gh api --method PUT "repos/$REPO/rulesets/$id" --input - >/dev/null
  else
    echo "creating ruleset $RULESET_NAME"
    printf '%s\n' "$body" | gh api --method POST "repos/$REPO/rulesets" --input - >/dev/null
  fi
  echo "done."
  cmd_status
}

cmd_delete() {
  local id
  id="$(existing_ruleset_id)"
  if [ -z "$id" ]; then echo "no ruleset named $RULESET_NAME"; return 0; fi
  gh api --method DELETE "repos/$REPO/rulesets/$id"
  echo "deleted ruleset $id ($RULESET_NAME)"
}

case "${1:-}" in
  status) cmd_status ;;
  apply)  cmd_apply ;;
  delete) cmd_delete ;;
  *)      usage ;;
esac
