#!/usr/bin/env bash
#
# Print the automated reviewers' findings on a pull request that nobody has
# answered yet (issue #2081). Read-only: it posts, edits and resolves nothing.
#
# Usage: unanswered-findings.sh <pr-number>
#
# Run it from inside the repository checkout, after wait-reviews.sh has
# returned for the current head: `gh` resolves {owner}/{repo} from the git
# remote. It prints one KEY=value record per line:
#
#   HEAD=<sha>                                  the head the verdict is about
#   NO_REVIEW=<name>                            no review by that app to read:
#                                               Qodo's comment does not name the
#                                               head, or Greptile posted no summary
#   UNPARSED=<name>                             reviewers.json lists an app this
#                                               script has no reader for
#   ANSWERED=<name> <n> <how>: <title>          <how> is fixed, thread or comment
#   UNANSWERED=<name> <n> <severity>: <title>
#   UNANSWERED_COUNT=<count>
#   FINDINGS=answered|unanswered
#
# FINDINGS=answered means every finding read was answered. It does NOT mean a
# review was obtained: read NO_REVIEW= and UNPARSED= for that, since an app
# that never reviewed has no findings to leave unanswered.
#
# Which findings: Qodo's are the numbered items of the "Code Review by Qodo"
# issue comment that names the head commit, above its "Previous reviews"
# section — every tier, its informational one included, which exists only in
# that comment. Greptile's are the numbered items under "Findings" in its
# summary comment; it reviews once, when the PR opens, so that list is its
# review of the head too.
#
# A finding is answered when any one of these holds:
#
#   fixed    Qodo itself marks it resolved (struck through, "✓ Resolved",
#            naming the resolving commit). Qodo judges that against the code,
#            so it is the one fix-commit signal that needs no guessing.
#   thread   the review thread that carries the finding is resolved, or has a
#            reply from an account that is not a bot. Greptile's summary links
#            its thread; a Qodo thread is the one opened by Qodo whose text
#            contains the finding's title.
#   comment  an issue comment on the PR, from an account that is not a bot,
#            contains the finding's title (or, for Qodo, its finding id from the
#            "View" link). This is the way to answer an informational Qodo item,
#            which has no thread.
#
# Titles are compared after HTML entities, tags, backslashes and backticks are
# stripped, case-insensitively, because the same finding is spelled "Exact PTY
# pin" in Qodo's summary and "Exact pty pin" in its inline comment. Quote the
# title in your reply; numbers change between Qodo's reviews, titles do not.
#
# Exit status is 0 whenever FINDINGS= was printed, and 2 with ERROR= on bad
# usage, a malformed reviewers.json, or a GitHub read that failed.
#
# REVIEWERS_FILE overrides the registry path; it exists for the tests in
# xtask/linkage-check/src/review_findings.rs.

set -uo pipefail

die() { echo "ERROR=$*"; exit 2; }

[ $# -eq 1 ] || die "usage: unanswered-findings.sh <pr-number>"
pr="$1"
printf '%s\n' "$pr" | grep -qE '^[0-9]+$' || die "not a pull request number: $pr"

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd) || die "cannot find the script's directory"
registry="${REVIEWERS_FILE:-$here/reviewers.json}"

command -v jq >/dev/null || die "jq not found"
command -v gh >/dev/null || die "gh not found"
[ -r "$registry" ] || die "cannot read $registry"
jq -e '.reviewers | type == "array" and length > 0' "$registry" >/dev/null 2>&1 ||
  die "$registry has no reviewers list"

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT

gh pr view "$pr" --json headRefOid,url >"$tmp/pr.json" 2>/dev/null ||
  die "cannot read pull request #$pr"
head=$(jq -re '.headRefOid' "$tmp/pr.json" 2>/dev/null) || die "pull request #$pr has no head"
slug=$(jq -re '.url | capture("github\\.com/(?<o>[^/]+)/(?<r>[^/]+)/pull/") | "\(.o) \(.r)"' "$tmp/pr.json" 2>/dev/null) ||
  die "cannot tell the repository from pull request #$pr's url"
owner=${slug% *}
repo=${slug#* }

gh api "repos/{owner}/{repo}/issues/$pr/comments" --paginate >"$tmp/comments.raw" 2>/dev/null &&
  jq -s 'add // []' "$tmp/comments.raw" >"$tmp/comments.json" 2>/dev/null ||
  die "cannot read pull request #$pr's comments"

# Every review thread with its comments. Paginated over threads; a thread with
# more than 100 comments has its later ones unread.
# shellcheck disable=SC2016 # GraphQL variables, not shell ones
gh api graphql --paginate \
  -f query='query($o:String!,$r:String!,$n:Int!,$endCursor:String){repository(owner:$o,name:$r){
    pullRequest(number:$n){reviewThreads(first:100,after:$endCursor){
      pageInfo{hasNextPage endCursor}
      nodes{isResolved comments(first:100){nodes{databaseId url body author{__typename login}}}}}}}}' \
  -f o="$owner" -f r="$repo" -F n="$pr" >"$tmp/threads.raw" 2>/dev/null &&
  jq -s '[.[].data.repository.pullRequest.reviewThreads.nodes[]]' "$tmp/threads.raw" >"$tmp/threads.json" 2>/dev/null ||
  die "cannot read pull request #$pr's review threads"

login_of() { jq -r --arg n "$1" '.reviewers[] | select(.name == $n) | .login' "$registry" | head -1; }

# The newest comment by $1 whose body contains $2 (and $3, when given), written
# to $tmp/$4.body; 1 when there is none.
latest_comment() {
  jq -e -r --arg l "$1" --arg c "$2" --arg h "${3:-}" '
    [ .[] | select(.user.login == $l)
          | select((.body // "") | contains($c))
          | select($h == "" or ((.body // "") | contains($h))) ]
    | sort_by(.updated_at // .created_at) | last | .body // empty' \
    "$tmp/comments.json" >"$tmp/$4.body" 2>/dev/null && [ -s "$tmp/$4.body" ]
}

# Findings, one per line: name <TAB> number <TAB> severity <TAB> title <TAB>
# resolved(yes|no) <TAB> qodo finding id <TAB> greptile thread comment id
: >"$tmp/findings.tsv"

read_qodo() {
  local login
  login=$(login_of Qodo)
  if ! latest_comment "$login" "Code Review by Qodo" "$head" qodo; then
    echo "NO_REVIEW=Qodo"
    return
  fi
  awk '
    function flush() {
      if (num != "") print "Qodo\t" num "\t" sev "\t" title "\t" resolved "\t" fid "\t"
      num = ""
    }
    index($0, "<!-- qodo-previous-reviews:start -->") { flush(); exit }
    index($0, "Code Review by Qodo") { on = 1 }
    !on { next }
    {
      if (match($0, /alt="(Action required|Remediation recommended|Informational)"/)) {
        sev = substr($0, RSTART + 5, RLENGTH - 6)
      }
    }
    /^<summary>  *[0-9]+\.  / {
      flush()
      line = $0
      sub(/^<summary>  */, "", line)
      num = line
      sub(/\..*/, "", num)
      sub(/^[0-9]+\.  */, "", line)
      resolved = (index(line, "<s>") || index(line, "✓ Resolved")) ? "yes" : "no"
      sub(/ *<code>.*$/, "", line)
      sub(/<\/summary>.*$/, "", line)
      gsub(/<\/?s>/, "", line)
      title = line
      fid = ""
      next
    }
    num != "" && fid == "" && match($0, /app\.qodo\.ai\/findings\/[0-9a-f-]+/) {
      fid = substr($0, RSTART + 21, RLENGTH - 21)
    }
    END { flush() }
  ' "$tmp/qodo.body" >>"$tmp/findings.tsv"
}

read_greptile() {
  local login
  login=$(login_of Greptile)
  if ! latest_comment "$login" "<!-- greptile_summary -->" "" greptile; then
    echo "NO_REVIEW=Greptile"
    return
  fi
  awk '
    /^[0-9]+\. / && index($0, "**") {
      num = $0; sub(/\..*/, "", num)
      sev = ""
      if (match($0, /alt="P[0-9]"/)) sev = substr($0, RSTART + 5, RLENGTH - 6)
      title = $0
      sub(/^[^*]*\*\*/, "", title)
      sub(/\*\*.*$/, "", title)
      tid = ""
      if (match($0, /#discussion_r[0-9]+/)) tid = substr($0, RSTART + 13, RLENGTH - 13)
      print "Greptile\t" num "\t" sev "\t" title "\tno\t\t" tid
    }
  ' "$tmp/greptile.body" >>"$tmp/findings.tsv"
}

echo "HEAD=$head"
for name in $(jq -r '.reviewers[].name' "$registry"); do
  case "$name" in
    Qodo) read_qodo ;;
    Greptile) read_greptile ;;
    *) echo "UNPARSED=$name" ;;
  esac
done

jq -R -s -r \
  --slurpfile comments "$tmp/comments.json" \
  --slurpfile threads "$tmp/threads.json" \
  --arg qodo "$(login_of Qodo)" '
  def plain: gsub("<[^>]*>"; "") | gsub("&quot;"; "\"") | gsub("&#x27;|&#39;"; [39] | implode)
           | gsub("&lt;"; "<") | gsub("&gt;"; ">") | gsub("&nbsp;"; " ") | gsub("&amp;"; "&")
           | gsub("[\\\\`]"; "") | gsub("\\s+"; " ") | ltrimstr(" ") | rtrimstr(" ");
  def norm: plain | ascii_downcase;
  def human: (.__typename // "") != "Bot";
  def answered_thread: .isResolved or ([.comments.nodes[1:][] | select(.author | human)] | length > 0);
  ($comments[0]) as $cs
  | ($threads[0]) as $ts
  | [ split("\n")[] | select(length > 0) | split("\t")
      | { name: .[0], num: .[1], sev: .[2], title: (.[3] | plain), resolved: (.[4] == "yes"),
          fid: .[5], tid: .[6], key: (.[3] | norm) } ]
  | map(
      . as $f
      | ( if $f.resolved then "fixed"
          elif ([ $ts[]
                  | select(
                      if $f.tid != "" then (.comments.nodes[0].databaseId | tostring) == $f.tid
                      else ($f.name == "Qodo" and $f.key != ""
                            and (.comments.nodes[0].author.login // "" | sub("\\[bot\\]$"; "")) == ($qodo | sub("\\[bot\\]$"; ""))
                            and ((.comments.nodes[0].body // "") | norm | contains($f.key)))
                      end)
                  | select(answered_thread) ] | length > 0) then "thread"
          elif ([ $cs[]
                  | select((.user.type // "") != "Bot")
                  | (.body // "") as $b
                  | select(($f.key != "" and ($b | norm | contains($f.key)))
                           or ($f.fid != "" and ($b | contains($f.fid)))) ] | length > 0) then "comment"
          else "" end ) as $how
      | . + { how: $how } )
  | ( .[] | if .how != "" then "ANSWERED=\(.name) \(.num) \(.how): \(.title)"
            else "UNANSWERED=\(.name) \(.num) \(if .sev == "" then "unrated" else .sev end): \(.title)" end ),
    ( [ .[] | select(.how == "") ] | length
      | "UNANSWERED_COUNT=\(.)", "FINDINGS=\(if . == 0 then "answered" else "unanswered" end)" )
' "$tmp/findings.tsv" || die "cannot decide which findings are answered"
