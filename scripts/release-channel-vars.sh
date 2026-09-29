# Sourced, not executed, by Taskfile.yml's homebrew-formula, homebrew-publish,
# scoop-manifest and scoop-publish tasks (issue #324). go-task runs those
# bodies in its built-in interpreter (mvdan/sh), so this file sticks to what
# that interpreter and bash both implement.
#
# Input, from the environment and never from the script text:
#   DAD_RELEASE_VERSION  the release tag, `v` + SemVer — what release.yml passes
#   DAD_CHANNEL_NAME     dot-agent-deck | dot-agent-deck-beta
#
# Why the environment: go-task substitutes `{{.VAR}}` into a command before the
# shell parses it, so a value spliced into shell text is code. Bound through a
# task's `env:` it is only ever data, and the validation below is what stops a
# malformed value reaching a file path, a heredoc or a commit message.
#
# Output, set in the sourcing shell:
#   VERSION_NO_V    the version without its leading `v`
#   BASE_URL        the release's download URL prefix
#   CLASS_NAME      the Homebrew formula class for the channel
#   CONFLICTS_WITH  the other channel's formula name

# Enumerated classes rather than ranges, and the same {0,18} core bound, so this
# accepts what release.yml's `prepare` gate accepts (plus the leading `v`); see
# the comments there for why each piece is spelled the way it is. `$` anchors at
# the end of the string in both bash's ERE and Go's RE2, so a trailing newline
# does not match.
_dad_d='[0123456789]'
_dad_nz='[123456789]'
_dad_alpha='[abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-]'
_dad_alnum='[0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-]'
_dad_core="(0|${_dad_nz}${_dad_d}{0,18})"
_dad_ident="(0|${_dad_nz}${_dad_d}*|${_dad_d}*${_dad_alpha}${_dad_alnum}*)"
_dad_semver_re="^v${_dad_core}[.]${_dad_core}[.]${_dad_core}(-${_dad_ident}([.]${_dad_ident})*)?([+]${_dad_alnum}+([.]${_dad_alnum}+)*)?"'$'

# The rejected value is deliberately not echoed: printed raw into a CI log it
# could start a line with `::` and forge a workflow command.
if [[ ! "${DAD_RELEASE_VERSION-}" =~ $_dad_semver_re ]]; then
  echo "Error: VERSION must be v<SemVer> (e.g. v1.2.3 or v1.2.3-rc.1); refusing the value given" >&2
  exit 1
fi

case "${DAD_CHANNEL_NAME-}" in
  dot-agent-deck)
    CLASS_NAME=DotAgentDeck
    CONFLICTS_WITH=dot-agent-deck-beta
    ;;
  dot-agent-deck-beta)
    CLASS_NAME=DotAgentDeckBeta
    CONFLICTS_WITH=dot-agent-deck
    ;;
  *)
    echo "Error: NAME must be dot-agent-deck or dot-agent-deck-beta; refusing the value given" >&2
    exit 1
    ;;
esac

VERSION_NO_V="${DAD_RELEASE_VERSION#v}"
BASE_URL="https://github.com/vfarcic/dot-agent-deck/releases/download/${DAD_RELEASE_VERSION}"

# dad_checksum ASSET [optional]: print ASSET's SHA-256 from dist/checksums.txt,
# relative to the caller's cwd, or fail. Call it as
# `x=$(dad_checksum ASSET) || exit 1`: the failure is a subshell's, so the
# caller has to propagate it. The value lands in a generated file, so it is
# held to the shape `shasum -a 256` prints — one line of 64 lowercase hex
# digits.
#
# `optional` lets an ASSET with no line at all print nothing and succeed, while
# a line that is present must still have that shape. It exists for the Windows
# binary: release.yml's build matrix has no Windows leg, so the published
# checksums.txt has no such line (v0.41.2's has none), and the manifest the
# previous Taskfile published for v0.41.2 carries `"hash": ""`. Failing here
# instead would abort `finalize` after the GitHub Release and the Homebrew
# formula are already out.
dad_checksum() {
  _dad_sum=$(grep "${1}\$" dist/checksums.txt | awk '{print $1}')
  if [ -z "$_dad_sum" ] && [ "${2-}" = optional ]; then
    return 0
  fi
  if [[ ! "$_dad_sum" =~ ^[0123456789abcdef]{64}$ ]]; then
    echo "Error: no single SHA-256 for ${1} in dist/checksums.txt — check that file" >&2
    return 1
  fi
  printf '%s' "$_dad_sum"
}
