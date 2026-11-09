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
# docker build (tn_build_program) with a small static ELF program that prints the five
# --version lines (mkelf below), for a quick run; it is refused in CI, which always builds for
# real. prep's git-cliff and the --version checks run in docker either way. cargo (prep's
# cargo update) and gh are stubs that log their calls; the run requires that gh never ran and
# that origin got no tag.
#
# The remote leg signs an unsigned copy of the release again the way the maintainer does it
# from the laptop, with make release-sign-remote REMOTE=buildbox:<clone>, and verifies it with
# make release-verify-remote. A stand-in ssh (RELEASE_REMOTE_SSH, which etc/release-remote.sh
# honors only in a dry run) links the fixture agent's extra socket where ssh -R would put it,
# at the agent socket of the server's GnuPG home, and runs the remote command here, so the
# clone is the server's checkout and its home signs through the restricted socket alone.
# verify then runs again against the clone in each state the server refuses: a local change
# to a tracked file, another branch, a detached HEAD and a commit behind origin's main.
#
# The gates a dry run skips (the comparison with the CI rebuild, the required checks on the
# tag's commit) run last, one case at a time, as release.sh's own tests call its functions,
# with a gh that answers from fixture files (through jq, as gh's --jq does). So do the static
# program check, the panic strategy check and etc/ci-rebuild.sh, which rebuilds the tag from a
# checkout of main whose origin has it, with the build stub in both modes.
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
RR=
RSOCK=

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

for tool in git gpg gpgconf gpg-connect-agent make docker tar gzip awk sed od jq; do
    command -v "$tool" >/dev/null 2>&1 || bail "missing required tool: $tool"
done
docker info >/dev/null 2>&1 || bail "docker does not answer"
[ -f "$ROOT/release/release.sh" ] || bail "release/ is not checked out: run git submodule update --init -- release"

t_exit() {
    local rc=$?
    if [ "$T_DONE" != 1 ]; then printf 'Bail out! %s stopped early (exit %s)\n' "$T_NAME" "$rc"; fi
    if [ -n "${GNUPGHOME:-}" ]; then gpgconf --homedir "$GNUPGHOME" --kill all >/dev/null 2>&1 || true; fi
    if [ -n "${RSOCK:-}" ]; then rm -f "$RSOCK"; fi
    if [ -n "${RR:-}" ]; then gpgconf --homedir "$RR" --remove-socketdir >/dev/null 2>&1 || true; fi
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

# the steps see only the variables each one sets; etc/release-remote.sh forwards every
# RELEASE_* and TN_P2P_ALLOW_* variable to the server
for v in $(env | sed -n -e 's/^\(RELEASE_[A-Za-z0-9_]*\)=.*/\1/p' -e 's/^\(TN_P2P_ALLOW_[A-Za-z0-9_]*\)=.*/\1/p'); do
    unset "$v"
done
unset GITHUB_ACTIONS GITHUB_EVENT_NAME GITHUB_RUN_ID GITHUB_RUN_ATTEMPT GITHUB_REPOSITORY REMOTE FAKE_SSH_NO_FORWARD

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

expect_out_or_err() {
    if grep -F -q -e "$1" "$T_OUT" "$T_ERR"; then t_ok "$2"; else t_fail "$2 (neither stdout nor stderr has '$1')"; fi
}

# expect_err_line TEXT1 TEXT2 WHAT: one line of stderr holds both texts. The second grep reads
# all its input (no -q), so that the first never dies of SIGPIPE under pipefail.
expect_err_line() {
    if grep -F -e "$1" "$T_ERR" | grep -F -e "$2" >/dev/null; then t_ok "$3"; else t_fail "$3 (no stderr line has '$1' and '$2')"; fi
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

# mkelf [-i INTERP] FILE LINE...: writes FILE, a static x86-64 ELF64 program (one PT_LOAD and
# no PT_INTERP program header) that prints the LINEs and exits 0. With -i it also has a
# PT_INTERP header naming INTERP, as a dynamically linked program has; that one is never run.
cat >"$FX/mkelf" <<'EOF'
#!/usr/bin/env bash
set -eu
interp=
if [ "$1" = -i ]; then
    interp=$2
    shift 2
fi
out=$1
shift
msg=
for line in "$@"; do msg=$msg$line$'\n'; done
# le VALUE N: VALUE as N little-endian bytes, in printf's octal escapes
le() {
    local v=$1 n=$2
    while [ "$n" -gt 0 ]; do
        printf '\\%03o' $((v & 255))
        v=$((v >> 8))
        n=$((n - 1))
    done
}
base=4194304
nph=1
if [ -n "$interp" ]; then nph=2; fi
code=$((64 + 56 * nph))
len=${#msg}
ioff=$((code + 33 + len))
size=$ioff
if [ -n "$interp" ]; then size=$((ioff + ${#interp} + 1)); fi
{
    # ELF header: \177ELF, ELFCLASS64, ELFDATA2LSB, EV_CURRENT, padding; ET_EXEC, EM_X86_64,
    # e_version, e_entry, e_phoff, e_shoff, e_flags, e_ehsize, e_phentsize, e_phnum, then no
    # section headers
    printf "\\177ELF$(le 2 1)$(le 1 1)$(le 1 1)$(le 0 9)$(le 2 2)$(le 62 2)$(le 1 4)$(le $((base + code)) 8)$(le 64 8)$(le 0 8)$(le 0 4)$(le 64 2)$(le 56 2)$(le "$nph" 2)$(le 64 2)$(le 0 2)$(le 0 2)"
    if [ -n "$interp" ]; then
        # PT_INTERP, PF_R, the path after the message
        printf "$(le 3 4)$(le 4 4)$(le "$ioff" 8)$(le $((base + ioff)) 8)$(le $((base + ioff)) 8)$(le $((${#interp} + 1)) 8)$(le $((${#interp} + 1)) 8)$(le 1 8)"
    fi
    # PT_LOAD, PF_R|PF_X, the whole file at base
    printf "$(le 1 4)$(le 5 4)$(le 0 8)$(le "$base" 8)$(le "$base" 8)$(le "$size" 8)$(le "$size" 8)$(le 4096 8)"
    # write(1, msg, len); exit(0): mov eax,1; mov edi,1; lea rsi,[rip+16]; mov edx,len;
    # syscall; mov eax,60; xor edi,edi; syscall, then the message
    printf "$(le 184 1)$(le 1 4)$(le 191 1)$(le 1 4)$(le 72 1)$(le 141 1)$(le 53 1)$(le 16 4)$(le 186 1)$(le "$len" 4)$(le 15 1)$(le 5 1)$(le 184 1)$(le 60 4)$(le 49 1)$(le 255 1)$(le 15 1)$(le 5 1)"
    printf '%s' "$msg"
    if [ -n "$interp" ]; then printf '%s\000' "$interp"; fi
} >"$out"
chmod 0755 "$out"
EOF
chmod 0755 "$FX/mkelf"

# the build stub: tn_build_program writes the program mkelf makes for the five --version lines
build_stub="
# dry-run fixture: a small static ELF program that prints the five --version lines stands in
# for the docker build of the program
tn_build_program() {
    mkdir -p \"\$1\"
    '$FX/mkelf' \"\$1/\$PROJECT\" \"\$PROJECT \$VERSION\" \"commit \$COMMIT\" \"target \$TRIPLE\" 'snapshot-versions 1' 'config-version 1'
}"

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
if [ "$STUB" = 1 ]; then printf '%s\n' "$build_stub" >>"$w/release.conf"; fi
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
expect_err "$r1 keeps panic = \"unwind\" in its release build" "check-tag finds the panic strategy unwind at the tag"
expect_err "dry run: not checking the CI results of $r1" "a dry run leaves the required checks on the tag's commit alone"

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

# an unsigned copy of the release for the signature over ssh below
rd2=$FX/release-dir-remote
cp -a "$rd" "$rd2"

step "$clone" rel RELEASE_DIR="$rd" RELEASE_YES=1 RELEASE_SUMS_SHA256="$sums" RELEASE_GPG_KEY="$fpr" \
    ./release/release.sh sign "$tag"
expect_rc 0 "sign adds alice's signature over SHA256SUMS"
expect_out "signed $tag as @alice: 1/1 signatures" "sign counts one of one required signature"
expect_err "dry run: not comparing tn-p2p-allow-sync with the CI rebuild of $tag" "a dry run signs without the CI rebuild"

step "$clone" rel RELEASE_DIR="$rd" TN_P2P_ALLOW_REBUILD=1 CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-}" \
    ./release/release.sh verify "$tag"
expect_rc 0 "verify accepts the signed release and its rebuild from the tag"
expect_out "verified $tag commit=$r1 signatures=1/1" "verify counts alice's signature"
expect_err "rebuilt $tag from $r1: the same tn-p2p-allow-sync, sha256 $b1" "the rebuild from the tag gives the released program"

# the same signature the way the maintainer makes it from the laptop: make release-sign-remote
# REMOTE=host:dir checks the key and the agent here, sets up the server's GnuPG home over one
# connection, then runs make release-sign in the server's checkout over a second one whose
# ssh -R forwards the agent's restricted extra socket to the socket gpg uses in that home. The
# stand-in ssh logs each connection's host and forward to FAKE_SSH_LOG, links the fixture
# agent's extra socket where the forward would put it (unless FAKE_SSH_NO_FORWARD=1), runs the
# remote command here, so REMOTE=buildbox:$clone makes the clone the server's checkout, and
# removes the link when the command ends, as sshd does when it closes the forward
FAKE_SSH_LOG=$FX/fake-ssh.log
export FAKE_SSH_LOG
cat >"$FX/fake-ssh" <<'EOF'
#!/usr/bin/env bash
# stands in for ssh in the dry run: ssh [-t] [-o opt] [-R remote:local] host command
set -eu
fwd=
while [ $# -gt 0 ]; do
    case $1 in
        -t) shift ;;
        -o) shift 2 ;;
        -R) fwd=$2; shift 2 ;;
        -*) printf 'fake ssh: unexpected option %s\n' "$1" >&2; exit 255 ;;
        *) break ;;
    esac
done
[ $# -eq 2 ] || { printf 'fake ssh: expected host and one command, got %s arguments\n' "$#" >&2; exit 255; }
printf '%s%s\n' "$1" "${fwd:+ -R $fwd}" >>"$FAKE_SSH_LOG"
if [ -n "$fwd" ] && [ "${FAKE_SSH_NO_FORWARD:-0}" != 1 ]; then
    [ ! -e "${fwd%%:*}" ] || exit 255   # a socket in the way: what ExitOnForwardFailure=yes gives
    ln -s "${fwd#*:}" "${fwd%%:*}"
    trap 'rm -f "${fwd%%:*}"' EXIT
fi
# not exec: the shell stays, so that its EXIT trap removes the link
bash -c "$2"
EOF
chmod 0755 "$FX/fake-ssh"
xsock=$(gpgconf --homedir "$GNUPGHOME" --list-dirs agent-extra-socket)
RR=$FX/gr

step "$clone" make release-sign-remote "TAG=$tag"
expect_rc 2 "make release-sign-remote refuses without REMOTE"
expect_out_or_err "usage: make release-sign-remote TAG=vX.Y.Z[-rcN] REMOTE=host[:dir]" "it prints the usage with REMOTE"

# outside a dry run the wrapper runs the real ssh alone; RELEASE_GPG_KEY gets it past the key
# check here, so the refusal shows wherever the wrapper makes it
step "$clone" env RELEASE_REMOTE_SSH="$FX/fake-ssh" RELEASE_GPG_KEY="$fpr" \
    make release-sign-remote "TAG=$tag" "REMOTE=buildbox:$clone"
expect_rc 2 "make release-sign-remote refuses a stand-in ssh outside a dry run"
expect_err "RELEASE_REMOTE_SSH is for dry runs" "it says the stand-in is for dry runs"

# ~/dir is dir: the stand-in runs the remote command here, so the missing directory is
# looked for under the clone, and the error is the prelude's
step "$clone" rel RELEASE_REMOTE_SSH="$FX/fake-ssh" make --no-print-directory release-setup-remote "REMOTE=buildbox:~/nowhere"
expect_rc 2 "make release-setup-remote fails on a checkout that is not there"
expect_err "no directory nowhere on buildbox: pass REMOTE=buildbox:<path of the checkout>" "it reads ~/nowhere as nowhere under the server's home and says so"
: >"$FAKE_SSH_LOG"

step "$clone" rel RELEASE_REMOTE_SSH="$FX/fake-ssh" RELEASE_REMOTE_GNUPGHOME="$RR" \
    make --no-print-directory release-setup-remote "REMOTE=buildbox:$clone"
expect_rc 0 "make release-setup-remote creates the server's GnuPG home over ssh"
expect_err "maintainer keys imported: 1 from " "setup imports alice's key alone"
expect_err "==> 1/1 buildbox:" "setup prints its one stage"
expect_err "==> done: buildbox is ready" "setup says the server is ready"
check "setup connects once, without a forward" test "$(cat "$FAKE_SSH_LOG" 2>/dev/null)" = buildbox
check "the server's home never starts an agent" grep -q -x -e no-autostart "$RR/gpg.conf"

RSOCK=$(RELEASE_REMOTE_GNUPGHOME="$RR" "$clone/etc/release-remote.sh" socket)
step "$clone" rel FAKE_SSH_NO_FORWARD=1 RELEASE_REMOTE_SSH="$FX/fake-ssh" RELEASE_REMOTE_GNUPGHOME="$RR" \
    RELEASE_DIR="$rd2" RELEASE_YES=1 RELEASE_SUMS_SHA256="$sums" RELEASE_GPG_KEY="$fpr" \
    make --no-print-directory release-sign-remote "TAG=$tag" "REMOTE=buildbox:$clone"
expect_rc 2 "make release-sign-remote stops when the forward brings no agent"
expect_err "no agent socket at $RSOCK" "the server names the socket it expects"

step "$clone" rel RELEASE_REMOTE_SSH="$FX/fake-ssh" RELEASE_REMOTE_GNUPGHOME="$RR" \
    RELEASE_DIR="$rd2" RELEASE_YES=1 RELEASE_SUMS_SHA256="$sums" RELEASE_GPG_KEY="$fpr" \
    make --no-print-directory release-sign-remote "TAG=$tag" "REMOTE=buildbox:$clone"
expect_rc 0 "make release-sign-remote signs on the server over the forwarded agent"
expect_out "signed $tag as @alice: 1/1 signatures" "the signature over ssh counts as alice's"
expect_err "==> 1/4 here:" "stage 1 checks the key and the agent here"
expect_err ", a file key here" "stage 1 finds alice's key in a file"
expect_err "==> 3/4 buildbox: connecting with the agent forwarded to $RSOCK" "stage 3 names the socket the forward binds"
expect_err_line "==> 4/4 " ": make release-sign TAG=$tag" "the server prints stage 4, make release-sign"
expect_err "==> release-sign $tag: fetches the draft's files" "make release-sign says what the step does"
expect_err "holds $fpr" "the server finds alice's key behind the forwarded socket"
expect_err "==> done: release-sign $tag on buildbox finished" "the run ends with done"
check "the step's connection forwards the fixture agent's extra socket" \
    test "$(tail -n 1 "$FAKE_SSH_LOG" 2>/dev/null)" = "buildbox -R $RSOCK:$xsock"
check "the connection before it sets the home up, without a forward" \
    test "$(tail -n 2 "$FAKE_SSH_LOG" 2>/dev/null | head -n 1)" = buildbox
check "the server's home never held a private key" test -z "$(ls -A "$RR/private-keys-v1.d" 2>/dev/null)"
check "the forwarded socket is gone once the step ends" test ! -e "$RSOCK" -a ! -L "$RSOCK"

step "$clone" rel RELEASE_REMOTE_SSH="$FX/fake-ssh" RELEASE_DIR="$rd2" TN_P2P_ALLOW_REBUILD=0 \
    make --no-print-directory release-verify-remote "TAG=$tag" "REMOTE=buildbox:$clone"
expect_rc 0 "make release-verify-remote accepts the release signed over ssh"
expect_out "verified $tag commit=$r1 signatures=1/1" "verify counts the signature made over ssh"
expect_err "==> 1/2 buildbox: connecting" "verify prints its first stage"
expect_err_line "==> 2/2 " ": make release-verify TAG=$tag" "the server prints stage 2, make release-verify"
check "verify connects without a forward" test "$(tail -n 1 "$FAKE_SSH_LOG" 2>/dev/null)" = buildbox

# the server's checkout must be main at origin's tip with no local changes: the same verify
# against the clone put in each state the step refuses, then put back, so a refusal is the
# checkout check's and a pass would have run verify
refused_remote() {
    step "$clone" rel RELEASE_REMOTE_SSH="$FX/fake-ssh" RELEASE_DIR="$rd2" TN_P2P_ALLOW_REBUILD=0 \
        make --no-print-directory release-verify-remote "TAG=$tag" "REMOTE=buildbox:$clone"
    expect_rc 2 "make release-verify-remote refuses a server checkout $1"
    expect_err "git checkout main && git pull --ff-only" "it says to check out main and pull"
}
printf 'a local change\n' >>"$clone/README.md"
refused_remote "with a change to a tracked file"
expect_err "has local changes to tracked files" "it names the local changes"
git -C "$clone" checkout -q -- README.md
git -C "$clone" checkout -q -b other
refused_remote "on a branch other than main"
expect_err "is on branch other, not on main" "it names the branch"
git -C "$clone" checkout -q main
git -C "$clone" branch -q -D other
git -C "$clone" checkout -q --detach
refused_remote "at a detached HEAD"
expect_err "is at a detached HEAD, not on main" "it says HEAD is detached"
git -C "$clone" checkout -q main
git -C "$clone" reset -q --hard HEAD~1
refused_remote "one commit behind origin/main"
expect_err "is at $main0, not at origin/main $r1 (1 behind, 0 ahead)" "it says how far behind it is"
git -C "$clone" reset -q --hard "$r1"
check "the server's checkout is clean on main at $r1 again" \
    test "$(git -C "$clone" symbolic-ref --short HEAD)@$(git -C "$clone" rev-parse HEAD)" = "main@$r1" \
    -a -z "$(git -C "$clone" status --porcelain --untracked-files=no)"

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

# version_case WHAT RC TEXT LINE...: tn_check_version on a static program printing LINE...,
# with $tag and the release commit, exits RC with TEXT on stderr, so a failure is the check's
# own and not, say, docker's.
version_case() {
    local what=$1 want=$2 text=$3
    shift 3
    "$FX/mkelf" "$FX/program" "$@"
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
# The release gates, one case at a time

# a static program passes; a dynamically linked one, or a script, is refused before it runs
gate_rel=$FX/gate-release
mkdir -p "$gate_rel/x"
tar -xzf "$rd/$tarball" -C "$gate_rel/x"
sourced tn_check_static "$gate_rel/x/$d/tn-p2p-allow-sync"
expect_rc 0 "the release's program has no PT_INTERP program header"
"$FX/mkelf" -i /lib/ld-musl-x86_64.so.1 "$FX/dynamic" "$v1" "$c1" "$t1" "snapshot-versions 1" "config-version 1"
sourced tn_check_version "$FX/dynamic"
expect_rc 1 "a program with a PT_INTERP program header fails the --version check"
expect_err "tn-p2p-allow-sync in $tarball has a PT_INTERP program header" "it says the program is linked dynamically"
sourced tn_check_version /bin/true
expect_rc 1 "the host's dynamically linked /bin/true fails it too"
expect_err "has a PT_INTERP program header" "it finds the PT_INTERP header of /bin/true"
printf '#!/bin/sh\necho %s\n' "$v1" >"$FX/script"
chmod 0755 "$FX/script"
sourced tn_check_version "$FX/script"
expect_rc 1 "a script fails it"
expect_err "tn-p2p-allow-sync in $tarball is not an x86-64 ELF64 program" "it says the script is no ELF program"

# the panic strategy stays unwind: a commit whose [profile.release] sets panic = "abort"
sourced tn_check_unwind "$r1"
expect_rc 0 "the release commit keeps panic = \"unwind\""
abort_blob=$(git -C "$clone" show "$r1:Cargo.toml" |
    awk '{ print } $0 == "[profile.release]" { print "panic = \"abort\"" }' | git -C "$clone" hash-object -w --stdin)
GIT_INDEX_FILE=$FX/abort.index git -C "$clone" read-tree "$r1"
GIT_INDEX_FILE=$FX/abort.index git -C "$clone" update-index --cacheinfo "100644,$abort_blob,Cargo.toml"
abort_tree=$(GIT_INDEX_FILE=$FX/abort.index git -C "$clone" write-tree)
abort=$(git -C "$clone" commit-tree -p "$r1" -m 'fixture: panic = "abort"' "$abort_tree")
sourced tn_check_unwind "$abort"
expect_rc 1 "a commit whose Cargo.toml sets panic = \"abort\" is refused"
expect_err "Cargo.toml at $abort sets the panic strategy to abort (line " "it names the file and the line"

# gh for the gates: logs its arguments to GH_FIXTURE/gh.log, then answers from the files there,
# applying --jq with jq as gh does: runs-WORKFLOW.json (run list --workflow WORKFLOW; GitHub's
# filters on --commit, --event and --branch applied, and each run cut to the --json fields as
# gh does; without the file the workflow has no runs), jobs-RUN.json and artifacts.json (api, a
# run's jobs and its artifacts), artifact-RUN-NAME/ (run download) and release.json (release
# view; without it the release does not exist). Nothing answers for a commit's check runs.
GHFX=$FX/gh-fixture
mkdir -p "$GHFX" "$FX/gate-bin"
cat >"$FX/gate-bin/gh" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$GH_FIXTURE/gh.log"
cmd="$1 $2"
shift 2
jqx=. dest= name= arg= wf= commit= event= branch= fields=
while [ $# -gt 0 ]; do
    case $1 in
        --jq) jqx=$2; shift 2 ;;
        -D) dest=$2; shift 2 ;;
        -n) name=$2; shift 2 ;;
        --workflow) wf=$2; shift 2 ;;
        --commit) commit=$2; shift 2 ;;
        --event) event=$2; shift 2 ;;
        --branch) branch=$2; shift 2 ;;
        --json) fields=$2; shift 2 ;;
        --repo | --limit) shift 2 ;;
        --paginate) shift ;;
        *) arg=$1; shift ;;
    esac
done
case $cmd in
    "run list")
        if [ -f "$GH_FIXTURE/runs-$wf.json" ]; then cat "$GH_FIXTURE/runs-$wf.json"; else echo '[]'; fi |
            jq --arg c "$commit" --arg e "$event" --arg b "$branch" --arg f "$fields" '[.[]
                | select(($c == "" or .headSha == $c) and ($e == "" or .event == $e) and ($b == "" or .headBranch == $b))
                | with_entries(select(.key as $k | $f | split(",") | any(. == $k)))]' |
            jq -r "$jqx" ;;
    "run download")
        if [ ! -d "$GH_FIXTURE/artifact-$arg-$name" ]; then echo "no valid artifacts found to download" >&2; exit 1; fi
        cp "$GH_FIXTURE/artifact-$arg-$name"/* "$dest/" ;;
    "release view")
        if [ ! -f "$GH_FIXTURE/release.json" ]; then echo "release not found" >&2; exit 1; fi
        exec jq -r "$jqx" "$GH_FIXTURE/release.json" ;;
    api\ */actions/runs/*/jobs*)
        run=${cmd#api repos/*/actions/runs/}
        run=${run%%/*}
        if [ ! -f "$GH_FIXTURE/jobs-$run.json" ]; then echo "gh: Not Found (HTTP 404)" >&2; exit 1; fi
        exec jq -r "$jqx" "$GH_FIXTURE/jobs-$run.json" ;;
    api\ */artifacts*) exec jq -r "$jqx" "$GH_FIXTURE/artifacts.json" ;;
    *) echo "fixture gh: no answer for gh $cmd" >&2; exit 1 ;;
esac
EOF
chmod 0755 "$FX/gate-bin/gh"

# gated DRAFT FUNCTION ARG...: FUNCTION of release.sh or release.conf after load_conf and
# parse_tag of $tag, outside a dry run, with COMMIT the release commit, REPO, WORK and
# REL_DRAFT=DRAFT set as the step that runs it sets them, and the fixture gh first on PATH.
gated() {
    local draft=$1
    shift
    rm -rf "$FX/gate-work"
    mkdir "$FX/gate-work"
    : >"$GHFX/gh.log"
    # shellcheck disable=SC2016 # the inner shell expands them
    step "$clone" env PATH="$FX/gate-bin:$PATH" GH_FIXTURE="$GHFX" bash -c \
        '. ./release/release.sh && load_conf && parse_tag "$1" && COMMIT=$2 REPO=$DEFAULT_REPO WORK=$3 REL_DRAFT=$4 && shift 4 && "$@"' \
        _ "$tag" "$r1" "$FX/gate-work" "$draft" "$@"
}

# wf_runs WORKFLOW ID=EVENT=BRANCH=STATE...: runs-WORKFLOW.json, the runs of WORKFLOW on the
# release commit, newest first as gh lists them; STATE is the conclusion of a completed run, or
# in_progress for a run that has not finished
wf_runs() {
    local wf=$1 r id event branch state concl sep=
    shift
    {
        printf '['
        for r in "$@"; do
            id=${r%%=*}
            r=${r#*=}
            event=${r%%=*}
            r=${r#*=}
            branch=${r%%=*}
            concl="\"${r#*=}\""
            state=completed
            if [ "$concl" = '"in_progress"' ]; then state=in_progress concl=null; fi
            printf '%s{"databaseId": %s, "status": "%s", "conclusion": %s, "headSha": "%s", "headBranch": "%s", "event": "%s"}' \
                "$sep" "$id" "$state" "$concl" "$r1" "$branch" "$event"
            sep=', '
        done
        printf ']\n'
    } >"$GHFX/runs-$wf.json"
}

# run_jobs RUN NAME=CONCLUSION...: jobs-RUN.json, the jobs of the run RUN, all completed
run_jobs() {
    local run=$1 j sep=
    shift
    {
        printf '{"total_count": %d, "jobs": [' "$#"
        for j in "$@"; do
            printf '%s{"name": "%s", "status": "completed", "conclusion": "%s"}' "$sep" "${j%=*}" "${j##*=}"
            sep=', '
        done
        printf ']}\n'
    } >"$GHFX/jobs-$run.json"
}

# no_runs: no workflow has a run, and no run has jobs
no_runs() { rm -f "$GHFX"/runs-*.json "$GHFX"/jobs-*.json; }

# the required checks on the tag's commit (hook_check_tag_extra, as check-tag runs it): the
# ten names of TN_REQUIRED_CHECKS, the jobs of ci.yaml and nft-real.yaml, each successful in
# the push-to-main run of its workflow. The other runs of ci.yaml on the commit, a dispatch, a
# push to another branch and a pull request, are not read.
ci_green=(fmt=success clippy=success test=success coverage=success deny=success audit=success release-dry-run=success)
nft_green=('nft-real (ubuntu-24.04)=success' 'nft-real (ubuntu-22.04)=success' 'nft-real (debian:13)=success')
api_runs=repos/Telcoin-Association/tn-p2p-allow-sync/actions/runs
no_runs
wf_runs ci.yaml 104=workflow_dispatch=main=failure 103=push=fix=failure 102=pull_request=main=failure 101=push=main=success
run_jobs 101 "${ci_green[@]}"
run_jobs 102 fmt=failure "${ci_green[@]:1}"
run_jobs 103 fmt=failure "${ci_green[@]:1}"
run_jobs 104 fmt=failure "${ci_green[@]:1}"
wf_runs nft-real.yaml 201=push=main=success
run_jobs 201 "${nft_green[@]}"
gated '' hook_check_tag_extra "$r1" always
expect_rc 0 "check-tag accepts a commit whose push to main passed every required job"
expect_err "CI passed on $r1: fmt, clippy, test, coverage, deny, audit, release-dry-run, nft-real (ubuntu-24.04), nft-real (ubuntu-22.04), nft-real (debian:13) (run 101 of ci.yaml, run 201 of nft-real.yaml)" \
    "it lists the ten required checks and the two runs whose jobs they are"
check "it lists the runs of ci.yaml and nft-real.yaml on the push of the release commit to main" \
    test "$(awk -v r="$r1" '$1 == "run" && $2 == "list" && $5 == "--workflow" && index($0, " --commit " r " --event push --branch main ") { w[$6] = 1 }
        END { print (w["ci.yaml"] && w["nft-real.yaml"]) }' "$GHFX/gh.log")" = 1
check "it reads the jobs of those two runs alone, and never a commit's check runs" \
    test "$(awk '$1 == "api" { printf "%s ", $2 }' "$GHFX/gh.log")" = "$api_runs/101/jobs?per_page=100 $api_runs/201/jobs?per_page=100 "

# a newer run still in progress, and an older one with a failed job: the newest completed run
# of each workflow counts. Skipped and neutral jobs, as a job skipped by its `if:` leaves, are
# passed over.
no_runs
wf_runs ci.yaml 107=push=main=in_progress 105=push=main=success 101=push=main=failure
run_jobs 101 "${ci_green[@]:0:2}" test=failure "${ci_green[@]:3}"
run_jobs 105 "${ci_green[@]}"
wf_runs nft-real.yaml 201=push=main=success
run_jobs 201 "${nft_green[@]}" fmt=skipped test=neutral release-dry-run=skipped
gated '' hook_check_tag_extra "$r1" always
expect_rc 0 "check-tag takes the newest completed push run of each workflow, and passes over skipped and neutral jobs"
expect_err "CI passed on $r1: fmt, clippy, test, coverage, deny, audit, release-dry-run, nft-real (ubuntu-24.04), nft-real (ubuntu-22.04), nft-real (debian:13) (run 105 of ci.yaml, run 201 of nft-real.yaml)" \
    "it reports the required checks as passed in run 105"

no_runs
wf_runs ci.yaml 101=push=main=failure
run_jobs 101 fmt=success clippy=success test=success coverage=skipped deny=neutral audit=failure release-dry-run=success
wf_runs nft-real.yaml 201=push=main=success
run_jobs 201 "${nft_green[@]}" audit=skipped
gated '' hook_check_tag_extra "$r1" always
expect_rc 1 "check-tag refuses a name with only skipped and neutral jobs, and a failed job beside a skipped one"
expect_err "CI has not passed on $r1: coverage (no job, only skipped or neutral ones), deny (no job, only skipped or neutral ones), audit (failure); " \
    "it counts the first two as missing and still sees the failure"

no_runs
wf_runs ci.yaml 101=push=main=failure
run_jobs 101 fmt=success clippy=failure "${ci_green[@]:2}"
wf_runs nft-real.yaml 201=push=main=cancelled
run_jobs 201 'nft-real (ubuntu-24.04)=success' 'nft-real (ubuntu-22.04)=cancelled'
gated '' hook_check_tag_extra "$r1" always
expect_rc 1 "check-tag refuses a commit with a failed, a cancelled and a missing required job"
expect_err "CI has not passed on $r1: clippy (failure), nft-real (ubuntu-22.04) (cancelled), nft-real (debian:13) (no job); " \
    "it names each of them and what it found"
expect_err "(run 101 of ci.yaml, run 201 of nft-real.yaml)" "it names the runs it read"

no_runs
wf_runs ci.yaml 101=push=main=success
run_jobs 101 "${ci_green[@]}"
wf_runs nft-real.yaml 202=push=main=in_progress
gated '' hook_check_tag_extra "$r1" always
expect_rc 1 "check-tag refuses a commit whose push run of nft-real.yaml is still running"
expect_err "the push-to-main run 202 of nft-real.yaml on $r1 is in_progress: wait for it to finish" "it names the run and its state"
check "it reads no jobs of the running run" test -z "$(grep -F "$api_runs/202/" "$GHFX/gh.log")"

# all green, but a dispatch on main and a push to another branch: no push-to-main run
no_runs
wf_runs ci.yaml 104=workflow_dispatch=main=success 103=push=fix=success
run_jobs 103 "${ci_green[@]}"
run_jobs 104 "${ci_green[@]}"
wf_runs nft-real.yaml 201=push=main=success
run_jobs 201 "${nft_green[@]}"
gated '' hook_check_tag_extra "$r1" always
expect_rc 1 "check-tag refuses all-green runs of ci.yaml that are a dispatch and a push to another branch"
expect_err "no push-to-main run of ci.yaml on $r1: wait for CI, or the commit is not on main" \
    "it says ci.yaml has no run on the push to main"
check "it reads the jobs of no run" test -z "$(awk '$1 == "api"' "$GHFX/gh.log")"

# nft-real.yaml ran on a pull request whose head branch is a fork's main, not on the push
no_runs
wf_runs ci.yaml 101=push=main=success
run_jobs 101 "${ci_green[@]}"
wf_runs nft-real.yaml 200=pull_request=main=success
run_jobs 200 "${nft_green[@]}"
gated '' hook_check_tag_extra "$r1" always
expect_rc 1 "check-tag refuses a commit with no push-to-main run of nft-real.yaml"
expect_err "no push-to-main run of nft-real.yaml on $r1: wait for CI, or the commit is not on main" "it names the workflow"

printf '{"isDraft": false, "isPrerelease": false, "author": {"login": "github-actions[bot]"}, "assets": []}\n' >"$GHFX/release.json"
gated '' hook_check_tag_extra "$r1" always
expect_rc 0 "the required checks of a published release are not asked for again"
expect_err "release $tag is published: not checking the CI results of $r1 again" "it says the release is published"
check "it lists no workflow run" test -z "$(awk '$1 == "run"' "$GHFX/gh.log")"
rm "$GHFX/release.json"

# the CI rebuild: the draft's program against the artifact of the release workflow's run on
# the push of the tag (hook_check_release_files, as sign, verify and publish run it)
wf_runs release.yaml "302=push=$tag=success" 301=push=v0.0.1=failure
mkdir -p "$GHFX/artifact-302-rebuild-$tag"
printf '%s  tn-p2p-allow-sync\n' "$packed" >"$GHFX/artifact-302-rebuild-$tag/program.sha256"
# artifacts NAME=EXPIRED...: artifacts.json, the artifacts of run 302
artifacts() {
    local a sep=
    {
        printf '{"total_count": %d, "artifacts": [' "$#"
        for a in "$@"; do
            printf '%s{"name": "%s", "expired": %s}' "$sep" "${a%=*}" "${a##*=}"
            sep=', '
        done
        printf ']}\n'
    } >"$GHFX/artifacts.json"
}
artifacts "rebuild-$tag=false"
gated true hook_check_release_files "$gate_rel"
expect_rc 0 "sign accepts a draft whose program the CI rebuild of $tag also gave"
expect_err "the CI rebuild of $tag from $r1 (run 302 of release.yaml) gave the same tn-p2p-allow-sync, sha256 $packed" \
    "it names the run and the sha256"
check "it looks for the release workflow's run on the push of $tag at the release commit" \
    grep -q -F -e "run list --repo Telcoin-Association/tn-p2p-allow-sync --workflow release.yaml --commit $r1 --event push " "$GHFX/gh.log"
check "it looks up that run's artifacts before the download" \
    test "$(awk '$1 == "api" && $2 == "repos/Telcoin-Association/tn-p2p-allow-sync/actions/runs/302/artifacts?per_page=100" { a = NR }
        $1 == "run" && $2 == "download" { d = NR } END { print (a && d && a < d) }' "$GHFX/gh.log")" = 1
check "it downloads that run's artifact rebuild-$tag" \
    grep -q -x -F -e "run download 302 --repo Telcoin-Association/tn-p2p-allow-sync -n rebuild-$tag -D $FX/gate-work/ci-rebuild" "$GHFX/gh.log"

drift=$(printf drift | sha256sum | awk '{ print $1 }')
printf '%s  tn-p2p-allow-sync\n' "$drift" >"$GHFX/artifact-302-rebuild-$tag/program.sha256"
gated true hook_check_release_files "$gate_rel"
expect_rc 1 "sign refuses a draft whose program the CI rebuild did not give"
expect_err "the draft's tn-p2p-allow-sync has sha256 $packed, but the CI rebuild of $tag from $r1 (run 302 of release.yaml) gave sha256 $drift" \
    "it names both sha256s"

artifacts real-nft-debian-13=false "rebuild-$tag=true"
gated true hook_check_release_files "$gate_rel"
expect_rc 1 "sign stops when the run's artifact rebuild-$tag has expired"
expect_err "the artifact rebuild-$tag of run 302 of release.yaml has expired" "it says the artifact expired"
expect_err "re-run the release workflow's rebuild job of run 302" "it says to re-run the rebuild job"
check "it does not try to download an expired artifact" \
    test -z "$(awk '$1 == "run" && $2 == "download"' "$GHFX/gh.log")"

artifacts real-nft-debian-13=false
gated true hook_check_release_files "$gate_rel"
expect_rc 1 "sign stops when the run has no artifact rebuild-$tag"
expect_err "run 302 of release.yaml for $tag has no artifact rebuild-$tag" "it says the artifact is missing"
expect_err "re-run the release workflow's rebuild job of run 302" "it says to re-run the rebuild job"

wf_runs release.yaml 301=push=v0.0.1=failure
gated true hook_check_release_files "$gate_rel"
expect_rc 2 "sign stops while the push of $tag has no run of the release workflow"
expect_err "wait for the CI rebuild of $tag, then run this step again" "it says to wait for the CI rebuild"

gated false hook_check_release_files "$gate_rel"
expect_rc 0 "verify of a published release does not look for the CI rebuild"
expect_err "release $tag is published: not comparing tn-p2p-allow-sync with the CI rebuild of $tag again" \
    "it says the release is published"

# etc/ci-rebuild.sh as the rebuild job runs it: in a checkout of main whose origin has the
# tag. The build stub stands in for docker in both modes; verify above rebuilt the tag with
# tn_build_commit, which the script builds with too.
cio=$FX/ci-origin.git
git -C "$FX" init -q --bare "$cio"
git -C "$cio" fetch -q "$clone" '+refs/heads/main:refs/heads/main' "+refs/tags/$tag:refs/tags/$tag"
fx_git -C "$FX" clone -q --recurse-submodules "$cio" "$FX/ci"
printf '%s\n' "$build_stub" >>"$FX/ci/release.conf"
"$FX/mkelf" "$FX/stub-program" "$v1" "$c1" "$t1" "snapshot-versions 1" "config-version 1"
stub_hex=$(sha256sum "$FX/stub-program" | awk '{ print $1 }')
step "$FX/ci" env GITHUB_STEP_SUMMARY="$FX/summary.md" ./etc/ci-rebuild.sh "$tag" "$FX/ci-out"
expect_rc 0 "etc/ci-rebuild.sh rebuilds $tag in a checkout of main"
check "its program.sha256 is one line: the sha256 of the build of $r1, two spaces, the program's name" \
    test "$(cat "$FX/ci-out/program.sha256" 2>/dev/null)" = "$stub_hex  tn-p2p-allow-sync" \
    -a "$(awk 'END { print NR }' "$FX/ci-out/program.sha256" 2>/dev/null)" = 1
check "it adds the tag, the commit and the sha256 to the step summary" \
    test "$(cat "$FX/summary.md" 2>/dev/null)" = "- CI rebuild of \`$tag\` from \`$r1\`: \`tn-p2p-allow-sync\` sha256 \`$stub_hex\`"

git -C "$FX/ci" tag -d "$tag" >/dev/null
git -C "$FX/ci" tag "$tag" "$r1"
step "$FX/ci" ./etc/ci-rebuild.sh "$tag" "$FX/ci-out2"
expect_rc 1 "etc/ci-rebuild.sh refuses a local $tag that differs from origin's"
expect_err "a local tag differs from the one on origin" "it says the local tag differs"
check "it writes no program.sha256 then" test ! -e "$FX/ci-out2/program.sha256"

# ---------------------------------------------------------------------------------------------
# Nothing left the machine.

check "gh never ran" test ! -e "$FX/gh.log"
check "origin received no tag" test -z "$(git -C "$FX/consumer.git" tag -l)"

finish
