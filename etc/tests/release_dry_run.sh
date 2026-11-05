#!/usr/bin/env bash
# Dry run of the whole release of this repository: every step of release/release.sh runs with
# RELEASE_DRY_RUN=1 on a copy of this repository's tracked tree, with this repository's own
# release.conf, and a directory (RELEASE_DIR) stands in for the GitHub release.
#
# The fixture follows tests/dry_run_test.sh of tn-release-signing:
#   signing-work/, signing.git   the release/ submodule's tree with keys/ holding only the
#                                throwaway key alice (ed25519, no passphrase) and KEYS.md as it
#                                is, so release.conf's MIN_SIGNATURES meets the real threshold
#   consumer-work/, consumer.git this repository's tracked files (the working copies), with
#                                signing.git as the submodule at release/, on main
#   clone/                       git clone --recurse-submodules of consumer.git, where the
#                                steps run: prep, tag, check-tag, notes, draft, build, sign,
#                                verify (with the rebuild) and publish for v<Cargo.toml version>
# The build is real: hook_build builds the program twice with etc/Dockerfile in BUILD_IMAGE,
# and verify rebuilds it, so the run also proves the build reproducible; it needs docker
# with buildx and the network (the images, crates.io). RELEASE_BUILD_STUB=1 replaces the one
# docker build (tn_build_program) with a script that prints the five --version lines, for a
# quick run; it is refused in CI, which always builds for real. prep's git-cliff and the
# --version checks run in docker either way. cargo (prep's cargo update) and gh are stubs
# that log their calls; the run requires that gh never ran and that origin got no tag.
#
#   bash etc/tests/release_dry_run.sh [DIR]
#
# DIR (new or empty) keeps the fixture for inspection; without it a temp dir is removed. It
# prints one "ok N - what" or "not ok N - what" line per check and a result line last.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd -P)
T_NAME=${0##*/}
T_PASS=0
T_FAIL=0
T_DIR=
T_KEEP=0
T_DONE=0
T_OUT=
T_ERR=
T_RC=

bail() {
    printf 'Bail out! %s\n' "$*"
    exit 2
}

STUB=${RELEASE_BUILD_STUB:-0}
case "$STUB" in
    0 | 1) ;;
    *) bail "RELEASE_BUILD_STUB must be 0 or 1" ;;
esac
if [ "$STUB" = 1 ] && [ "${GITHUB_ACTIONS:-}" = true ]; then
    bail "RELEASE_BUILD_STUB=1 is refused in CI, which runs the real build"
fi

for tool in git gpg gpgconf docker tar gzip awk sed; do
    command -v "$tool" >/dev/null 2>&1 || bail "missing required tool: $tool"
done
docker info >/dev/null 2>&1 || bail "docker does not answer"
[ -f "$ROOT/release/release.sh" ] || bail "release/ is not checked out: run git submodule update --init -- release"

t_exit() {
    local rc=$?
    if [ "$T_DONE" != 1 ]; then printf 'Bail out! %s stopped early (exit %s)\n' "$T_NAME" "$rc"; fi
    if [ -n "${GNUPGHOME:-}" ]; then gpgconf --homedir "$GNUPGHOME" --kill all >/dev/null 2>&1 || true; fi
    if [ "$T_KEEP" = 0 ] && [ -n "$T_DIR" ]; then rm -rf "$T_DIR"; fi
}

if [ -n "${1:-}" ]; then
    mkdir -p "$1"
    [ -z "$(ls -A "$1")" ] || bail "$1 is not empty"
    T_DIR=$(cd "$1" && pwd -P)
    T_KEEP=1
else
    T_DIR=$(mktemp -d "${TMPDIR:-/tmp}/tn-p2p-allow-release.XXXXXX")
    T_DIR=$(cd "$T_DIR" && pwd -P)
fi
trap t_exit EXIT

# the steps see only the variables each one sets
for v in $(env | sed -n 's/^\(RELEASE_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$v"; done
unset GITHUB_ACTIONS GITHUB_EVENT_NAME GITHUB_RUN_ID GITHUB_RUN_ATTEMPT GITHUB_REPOSITORY TN_P2P_ALLOW_REBUILD

t_ok() {
    T_PASS=$((T_PASS + 1))
    printf 'ok %d - %s\n' $((T_PASS + T_FAIL)) "$1"
}

t_fail() {
    T_FAIL=$((T_FAIL + 1))
    printf 'not ok %d - %s\n' $((T_PASS + T_FAIL)) "$1"
    if [ -n "$T_OUT" ]; then
        printf '#   exit %s\n' "$T_RC"
        sed 's/^/#   out| /' "$T_OUT"
        sed 's/^/#   err| /' "$T_ERR"
    fi
}

# step DIR COMMAND...: runs COMMAND in DIR and keeps its stdout, stderr and exit status.
step() {
    local dir=$1
    shift
    T_OUT=$T_DIR/step.out
    T_ERR=$T_DIR/step.err
    (cd "$dir" && "$@") >"$T_OUT" 2>"$T_ERR" && T_RC=0 || T_RC=$?
}

expect_rc() {
    if [ "$T_RC" = "$1" ]; then t_ok "$2"; else t_fail "$2 (exit $T_RC, expected $1)"; fi
}

expect_out() {
    if grep -F -q -e "$1" "$T_OUT"; then t_ok "$2"; else t_fail "$2 (stdout lacks '$1')"; fi
}

expect_err() {
    if grep -F -q -e "$1" "$T_ERR"; then t_ok "$2"; else t_fail "$2 (stderr lacks '$1')"; fi
}

# check WHAT COMMAND...: COMMAND succeeds (a fact about the fixture, not a step's output).
check() {
    local what=$1
    shift
    if "$@"; then
        t_ok "$what"
    else
        T_OUT=
        t_fail "$what"
    fi
}

finish() {
    T_DONE=1
    printf '%s: %d passed, %d failed\n' "$T_NAME" "$T_PASS" "$T_FAIL"
    [ "$T_FAIL" = 0 ]
}

rel() { env RELEASE_DRY_RUN=1 "$@"; }

# git that may clone and fetch submodules from local paths, for the fixture's own commands;
# release.sh runs with git's defaults
fx_git() { git -c protocol.file.allow=always "$@"; }

# ---------------------------------------------------------------------------------------------
# Fixture

FX=$T_DIR
GNUPGHOME=$FX/gnupg
GIT_CONFIG_GLOBAL=$FX/gitconfig
GIT_CONFIG_NOSYSTEM=1
export GNUPGHOME GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM
mkdir -m 700 "$GNUPGHOME"
: >"$GIT_CONFIG_GLOBAL"
git -C "$FX" config --global user.name 'Fixture Maintainer'
git -C "$FX" config --global user.email fixture@example.invalid
git -C "$FX" config --global init.defaultBranch main
git -C "$FX" config --global commit.gpgSign false
git -C "$FX" config --global tag.gpgSign false
git -C "$FX" config --global gpg.program gpg
git -C "$FX" config --global advice.detachedHead false

gpg --batch --quiet --passphrase '' --quick-gen-key 'alice <alice@example.invalid>' ed25519 sign never 2>/dev/null
fpr=$(gpg --batch --with-colons --list-keys '<alice@example.invalid>' 2>/dev/null | awk -F: '$1 == "fpr" { print $10; exit }')
[ -n "$fpr" ] || bail "no throwaway key"
gpg --batch --armor --export "$fpr" >"$FX/alice.asc"

# cargo and gh: stubs that log their arguments; cargo succeeds, gh fails
bin=$FX/bin
mkdir "$bin"
# stub NAME STATUS: a NAME on PATH that logs its arguments to FX/NAME.log and exits STATUS
stub() {
    cat >"$bin/$1" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$FX/$1.log"
exit $2
EOF
    chmod 0755 "$bin/$1"
}
stub cargo 0
stub gh 1
PATH=$bin:$PATH
export PATH

# the signing repository: the checked-out release/ tree with alice as the only key
w=$FX/signing-work
git -C "$FX" init -q "$w"
git -C "$ROOT/release" archive --format=tar HEAD | tar -xf - -C "$w"
signing_sha=$(git -C "$ROOT/release" rev-parse HEAD)
rm -f "$w"/keys/*.asc
cp "$FX/alice.asc" "$w/keys/alice.asc"
git -C "$w" add -A
git -C "$w" commit -q -m "fixture: tn-release-signing $signing_sha with the key of alice"
git -C "$FX" init -q --bare "$FX/signing.git"
git -C "$FX/signing.git" fetch -q "$w" '+refs/heads/main:refs/heads/main'

# the consumer: this repository's tracked files, the submodule and the gitlink left out, then
# signing.git as release/
w=$FX/consumer-work
git -C "$FX" init -q "$w"
git -C "$ROOT" ls-files -s | awk -F '\t' '{ split($1, m, " ") } m[1] != "160000" && $2 != ".gitmodules" { print $2 }' >"$FX/files"
tar -cf - -C "$ROOT" -T "$FX/files" | tar -xf - -C "$w"
fx_git -C "$w" submodule add -q "$FX/signing.git" release
if [ "$STUB" = 1 ]; then
    cat >>"$w/release.conf" <<'EOF'

# dry-run fixture (RELEASE_BUILD_STUB=1): a script that prints the five --version lines
# stands in for the docker build of the program
tn_build_program() {
    mkdir -p "$1"
    cat >"$1/$PROJECT" <<STUB
#!/bin/sh
echo '$PROJECT $VERSION'
echo 'commit $COMMIT'
echo 'target $TRIPLE'
echo 'snapshot-versions 1'
echo 'config-version 1'
STUB
    chmod 0755 "$1/$PROJECT"
}
EOF
fi
git -C "$w" add -A
git -C "$w" commit -q -m 'chore: the tree of tn-p2p-allow-sync for a release dry run' \
    -m "The tracked files of $ROOT, with tn-release-signing $signing_sha and a throwaway key."
git -C "$FX" init -q --bare "$FX/consumer.git"
git -C "$FX/consumer.git" fetch -q "$w" '+refs/heads/main:refs/heads/main'
fx_git -C "$FX" clone -q --recurse-submodules "$FX/consumer.git" "$FX/clone"

clone=$FX/clone
rd=$FX/release-dir
version=$(awk -F '"' '/^\[/ { sec = $0 } sec == "[package]" && /^version *=/ { print $2; exit }' "$clone/Cargo.toml")
tag=v$version
tarball=tn-p2p-allow-sync-$tag-x86_64-unknown-linux-musl.tar.gz
main0=$(git -C "$clone" rev-parse HEAD)
mode=real
if [ "$STUB" = 1 ]; then mode=stub; fi
printf '# %s build, tag %s, tn-release-signing %s, fixture in %s\n' "$mode" "$tag" "$signing_sha" "$FX"

check "the fixture's release.conf is this repository's$(if [ "$STUB" = 1 ]; then printf ' plus the build stub'; fi)" \
    test "$(head -c "$(wc -c <"$ROOT/release.conf")" "$clone/release.conf")" = "$(cat "$ROOT/release.conf")"

# ---------------------------------------------------------------------------------------------
# The release, step by step

step "$clone" ./release/release.sh parse "$tag-adiri"
expect_rc 2 "parse refuses a channel suffix, which TAG_RE does not allow"

step "$clone" ./release/release.sh parse "$tag-rc1"
expect_rc 0 "parse accepts a release candidate"
expect_out "PRERELEASE=true" "a release candidate is a prerelease"

step "$clone" rel ./release/release.sh prep "$tag"
expect_rc 0 "prep writes $tag on main's tip, with git-cliff and cliff.toml"
expect_out "## [$tag] - " "prep prints the new CHANGELOG section"
expect_out "<!-- release-base: $main0 -->" "the section's release-base marker names main's tip"
expect_out "- The tree of tn-p2p-allow-sync for a release dry run" "git-cliff lists the fixture's commit"
check "prep ran cargo update --workspace" grep -q -x -e 'update --workspace' "$FX/cargo.log"
check "the [Unreleased] section stays above the anchor" \
    test "$(awk '/^## \[Unreleased\]/ { u = NR } $0 == "<!-- ANCHOR: releases -->" { a = NR } END { print (u && a && u < a) }' "$clone/CHANGELOG.md")" = 1

# the release PR merges alone: the release commit lands on origin's main
git -C "$clone" commit -q -a -m "release: $tag"
r1=$(git -C "$clone" rev-parse HEAD)
git -C "$FX/consumer.git" fetch -q "$clone" '+refs/heads/main:refs/heads/main'
git -C "$clone" fetch -q --no-recurse-submodules origin

step "$clone" rel RELEASE_YES=1 RELEASE_GPG_KEY="$fpr" ./release/release.sh tag "$tag"
expect_rc 0 "tag signs $tag with alice's key"
expect_err "dry run: not pushing; $tag stays a local tag" "tag leaves the tag local"

step "$clone" rel ./release/release.sh check-tag "$tag"
expect_rc 0 "check-tag accepts $tag"
expect_out "commit=$r1 signer=alice fingerprint=$fpr channel=stable version=$version features= prerelease=false" \
    "check-tag prints the commit, the signer and the version"

step "$clone" rel ./release/release.sh notes "$tag"
expect_rc 0 "notes renders the release body"
expect_out "| Tarball | \`$tarball\` (\`x86_64-unknown-linux-musl\`) |" "the body names the tarball"
expect_out "REPO=Telcoin-Association/tn-p2p-allow-sync" "the verify commands name this repository"

step "$clone" rel GITHUB_ACTIONS=true RELEASE_DIR="$rd" ./release/release.sh draft "$tag"
expect_rc 0 "draft creates the stand-in release"

step "$clone" rel RELEASE_DIR="$rd" CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-}" ./release/release.sh build "$tag"
expect_rc 0 "build builds twice, packs the tarball and uploads it with SHA256SUMS"
expect_out "verified $tag commit=$r1 signatures=0/1" "build verifies its unsigned upload"
expect_err "tn-p2p-allow-sync $version, commit $r1, target x86_64-unknown-linux-musl, snapshot-versions 1, config-version 1" \
    "the program in the tarball prints the five --version lines of $tag"
b1=$(sed -n "s/^build 1 of $tag: tn-p2p-allow-sync sha256 //p" "$T_ERR")
b2=$(sed -n "s/^build 2 of $tag: tn-p2p-allow-sync sha256 //p" "$T_ERR")
printf '# build 1 sha256 %s\n# build 2 sha256 %s\n' "${b1:-none}" "${b2:-none}"
check "the two builds gave the same program" test -n "$b1" -a "$b1" = "$b2"
sums=$(awk '/^SHA256SUMS sha256: / { print $3 }' "$T_OUT") || sums=
check "SHA256SUMS lists the tarball alone" test "$(awk '{ print $2 }' "$rd/SHA256SUMS")" = "$tarball"
listing=$(tar -tzf "$rd/$tarball" 2>/dev/null | LC_ALL=C sort) || listing=
d=${tarball%.tar.gz}
want=$(printf '%s\n' "$d/" "$d/tn-p2p-allow-sync" "$d/LICENSE-APACHE" "$d/LICENSE-MIT" "$d/NOTICE" | LC_ALL=C sort)
check "the tarball holds $d/ with the program and the three license files" test "$listing" = "$want"
packed=$(tar -xzOf "$rd/$tarball" "$d/tn-p2p-allow-sync" | sha256sum | awk '{ print $1 }') || packed=
check "the tarball's program is the one both builds gave" test "$packed" = "$b1"

step "$clone" rel RELEASE_DIR="$rd" RELEASE_YES=1 RELEASE_SUMS_SHA256="$sums" RELEASE_GPG_KEY="$fpr" \
    ./release/release.sh sign "$tag"
expect_rc 0 "sign adds alice's signature over SHA256SUMS"
expect_out "signed $tag as @alice: 1/1 signatures" "sign counts one of one required signature"

step "$clone" rel RELEASE_DIR="$rd" TN_P2P_ALLOW_REBUILD=1 CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-}" \
    ./release/release.sh verify "$tag"
expect_rc 0 "verify accepts the signed release and its rebuild from the tag"
expect_out "verified $tag commit=$r1 signatures=1/1" "verify counts alice's signature"
expect_err "rebuilt $tag from $r1: the same tn-p2p-allow-sync, sha256 $b1" "the rebuild from the tag gives the released program"

# a rebuild that differs fails verify; release.conf is changed in the clone only, which verify,
# unlike build and publish, does not compare with main
cat >>"$clone/release.conf" <<'EOF'

tn_build_program() {
    mkdir -p "$1"
    printf '#!/bin/sh\necho drift\n' >"$1/$PROJECT"
}
EOF
step "$clone" rel RELEASE_DIR="$rd" TN_P2P_ALLOW_REBUILD=1 ./release/release.sh verify "$tag"
expect_rc 1 "verify refuses a release that a rebuild from its tag does not reproduce"
expect_err "it was not built from its tag" "verify says the rebuild differs"
git -C "$clone" checkout -q -- release.conf

step "$clone" rel RELEASE_DIR="$rd" RELEASE_YES=1 ./release/release.sh publish "$tag"
expect_rc 0 "publish verifies the draft and publishes it"
expect_err "dry run: would run gh release edit $tag --draft=false --prerelease=false --latest" "publish stops short of gh"
expect_out "published $tag newest=yes" "publish reports the newest final release"

# ---------------------------------------------------------------------------------------------
# The --version check and the image pin, one case at a time

# sourced FUNCTION ARG...: FUNCTION of release.sh or release.conf, run in the clone after
# load_conf and parse_tag of $tag with COMMIT the release commit, as release.sh's own tests
# call its functions.
sourced() {
    # shellcheck disable=SC2016 # the inner shell expands them
    step "$clone" bash -c '. ./release/release.sh && load_conf && parse_tag "$1" && COMMIT=$2 && shift 2 && "$@"' \
        _ "$tag" "$r1" "$@"
}

# version_case WHAT RC TEXT LINE...: tn_check_version on a program printing LINE..., with $tag
# and the release commit, exits RC with TEXT on stderr, so a failure is the check's own and
# not, say, docker's.
version_case() {
    local what=$1 want=$2 text=$3 line
    shift 3
    {
        printf '#!/bin/sh\n'
        for line in "$@"; do printf "echo '%s'\n" "$line"; done
    } >"$FX/program"
    chmod 0755 "$FX/program"
    sourced tn_check_version "$FX/program"
    if [ "$T_RC" = "$want" ] && grep -F -q -e "$text" "$T_ERR"; then
        t_ok "$what"
    else
        t_fail "$what (expected exit $want and '$text' on stderr)"
    fi
}
v1="tn-p2p-allow-sync $version"
c1="commit $r1"
t1="target x86_64-unknown-linux-musl"
head3="--version from $tarball begins"
tail2="--version from $tarball must end with"
version_case "the five lines pass" 0 "snapshot-versions 1, config-version 1" "$v1" "$c1" "$t1" "snapshot-versions 1" "config-version 1"
version_case "a snapshot-versions list holding 1 passes" 0 "snapshot-versions 1,2, config-version 1" "$v1" "$c1" "$t1" "snapshot-versions 1,2" "config-version 1"
version_case "another version fails" 1 "$head3" "tn-p2p-allow-sync 0.0.0" "$c1" "$t1" "snapshot-versions 1" "config-version 1"
version_case "another commit fails" 1 "$head3" "$v1" "commit $main0" "$t1" "snapshot-versions 1" "config-version 1"
version_case "another target fails" 1 "$head3" "$v1" "$c1" "target x86_64-unknown-linux-gnu" "snapshot-versions 1" "config-version 1"
version_case "snapshot-versions without 1 fails" 1 "$tail2" "$v1" "$c1" "$t1" "snapshot-versions 2" "config-version 1"
version_case "config-version 2 fails" 1 "$tail2" "$v1" "$c1" "$t1" "snapshot-versions 1" "config-version 2"
version_case "a sixth line fails" 1 "$tail2" "$v1" "$c1" "$t1" "snapshot-versions 1" "config-version 1" "extra"
version_case "four lines fail" 1 "$tail2" "$v1" "$c1" "$t1" "snapshot-versions 1"

mkdir -p "$FX/tree-ok" "$FX/tree-bad"
cp "$clone/release.conf" "$FX/tree-ok/release.conf"
sed '/^BUILD_IMAGE=/d' "$clone/release.conf" >"$FX/tree-bad/release.conf"
sourced tn_tree_image "$FX/tree-ok"
expect_rc 0 "a tree's build image is read from its release.conf"
expect_out "$(sed -n "s/^BUILD_IMAGE='\(.*\)'\$/\1/p" "$ROOT/release.conf")" "the image is the BUILD_IMAGE line"
sourced tn_tree_image "$FX/tree-bad"
expect_rc 1 "a tree whose release.conf has no BUILD_IMAGE line is not built"

# ---------------------------------------------------------------------------------------------
# Nothing left the machine.

check "gh never ran" test ! -e "$FX/gh.log"
check "origin received no tag" test -z "$(git -C "$FX/consumer.git" tag -l)"

finish
