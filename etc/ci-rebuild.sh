#!/usr/bin/env bash
# The CI rebuild of a release tag, for the rebuild job of the release workflow: on a GitHub
# runner, a host the build server does not control, it builds the program from the tag
# exactly as release.conf's tn_rebuild does, and records its sha256. sign, verify and publish
# (tn_check_ci_rebuild in release.conf) refuse a draft whose program differs from it.
#
#   etc/ci-rebuild.sh TAG OUTDIR
#
# It runs in a checkout of main with the release/ submodule, as every release job does, and
# fetches TAG from origin; a local TAG that differs from origin's is refused. The build is
# the tree of TAG's commit as git stores it, with that tree's etc/Dockerfile and the
# BUILD_IMAGE its release.conf pins, GIT_SHA the commit, and docker buildx build --pull
# --no-cache --provenance=false --sbom=false (tn_build_commit and tn_build_program in
# release.conf, sourced with release/release.sh, so the two builds cannot drift apart).
#
# It writes OUTDIR/program.sha256, the one line "<sha256>  tn-p2p-allow-sync" in sha256sum's
# format, and with GITHUB_STEP_SUMMARY set appends a line naming the tag, the commit and the
# sha256 to it. Exit codes as release.sh's: 0 done, 1 refused, 2 usage or precondition,
# 3 a failure worth a retry (the network).

set -euo pipefail

if [ $# -ne 2 ] || [ -z "$1" ] || [ -z "$2" ]; then
    printf 'usage: %s TAG OUTDIR\n' "${0##*/}" >&2
    exit 2
fi
case "$2" in
    /*) outdir=$2 ;;
    *) outdir=$PWD/$2 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
[ -f release/release.sh ] || {
    printf 'error: release/ is not checked out: run git submodule update --init -- release\n' >&2
    exit 2
}
# release.sh only defines its functions when sourced; load_conf sources release.conf
# shellcheck source=SCRIPTDIR/../release/release.sh
. release/release.sh
load_conf
parse_tag "$1"
setup
need tar

# The tag as origin has it, unforced: git refuses to replace a local tag that differs.
git_fetch "refs/tags/$TAG:refs/tags/$TAG"
COMMIT=$(tag_commit "$TAG") || exit

tn_build_commit "$WORK/program"
hex=$(sha256_hex "$WORK/program/$PROJECT") || exit
mkdir -p "$outdir"
printf '%s  %s\n' "$hex" "$PROJECT" >"$outdir/$TN_REBUILD_FILE"
info "CI rebuild of $TAG from $COMMIT: $PROJECT sha256 $hex, in $outdir/$TN_REBUILD_FILE"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    # shellcheck disable=SC2016 # Markdown code spans, not expansions
    printf -- '- CI rebuild of `%s` from `%s`: `%s` sha256 `%s`\n' "$TAG" "$COMMIT" "$PROJECT" "$hex" >>"$GITHUB_STEP_SUMMARY"
fi
