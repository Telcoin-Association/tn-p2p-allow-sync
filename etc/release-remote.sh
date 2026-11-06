#!/usr/bin/env bash
# A release step run on the build server from the laptop that holds the YubiKey.
#
# On the laptop, make release-<step>-remote calls `run`: it opens ssh to the server and runs
# `make release-<step>` in the checkout there, so docker, gh, git push and gpg all run on the
# server. For tag and sign it first checks the signing key and gpg-agent on the laptop, then
# forwards the laptop's gpg-agent into a GnuPG home of its own on the server: gpg there signs
# through the forward, each signature is one PKSIGN over it, and the PIN and the touch happen
# on the laptop. What is forwarded is the agent's "extra" socket, which gpg-agent serves in
# restricted mode: signing and decryption, no key export and no control of the agent. The home
# on the server holds only the maintainers' public keys from release/keys and `no-autostart`,
# so nothing there ever starts an agent in it: the socket is the forwarded one or none.
# For tag and sign the first connection (setup) stays open until the step is over: the
# server's /run/user/<uid>, where gpg puts the home's socket, exists only while the user has a
# session there, and the maintainer may be typing an ssh passphrase between the connections.
#
#   etc/release-remote.sh run REMOTE STEP [TAG]   laptop: the whole step over ssh; REMOTE is
#                                                 host or host:dir, STEP one of setup, prep,
#                                                 tag, build, sign, verify and publish, TAG
#                                                 required unless STEP is setup
#   etc/release-remote.sh setup                   server: create the home, import release/keys,
#                                                 remove a socket nothing answers on; prints
#                                                 the socket path alone on stdout
#   etc/release-remote.sh check                   server: the forwarded agent answers on the
#                                                 socket and holds the signing key
#   etc/release-remote.sh socket                  server: print the agent socket path gpg uses
#                                                 for the home
#   etc/release-remote.sh step STEP TAG [LABEL]   server: make release-STEP TAG=TAG in this
#                                                 checkout; tag and sign check the forwarded
#                                                 agent first and sign through it
#
# host is the ssh destination (an alias from ~/.ssh/config, or user@host); dir is the checkout
# on the server, relative to its home unless absolute, by default one named like this checkout.
# The home is RELEASE_REMOTE_GNUPGHOME, an absolute path on the server, else ~/.gnupg-tn-release
# there. On the laptop the signing key is RELEASE_GPG_KEY, else git's user.signingkey, as
# release.sh resolves it; run sends its fingerprint to the server as RELEASE_GPG_KEY, so the
# server needs none configured. run passes every RELEASE_* and TN_P2P_ALLOW_* variable of its
# environment on to the server; RELEASE_REMOTE_SSH, a stand-in for ssh, is for dry runs alone
# (RELEASE_DRY_RUN=1). Everything this script prints itself goes to stderr: stdout carries
# only the socket path of setup and socket, and the step's own output.
# run executes on the laptop, which may be a Mac whose bash is 3.2: like release.sh, this
# script stays bash 3.2 portable (no mapfile, coproc, associative arrays or ${x,,}, and the
# pattern of [[ =~ ]] always in a variable).
# Exit codes: 0 ok; 1 the agent does not hold the signing key; 2 usage or a precondition here
# or on the server; otherwise the remote step's own code; 255 ssh itself failed.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
KEYS_DIR=$ROOT/release/keys
STEPS='prep|tag|build|sign|verify|publish'

die() {
    local rc=$1
    shift
    printf 'error: %s\n' "$*" >&2
    exit "$rc"
}

info() { printf '%s\n' "$*" >&2; }

need() {
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || die 2 "missing required tool: $tool"
    done
}

usage() {
    local me=${0##*/}
    info "usage: $me run host[:dir] setup|prep|tag|build|sign|verify|publish [TAG]   on the laptop"
    info "       $me setup|check|socket                                              on the server"
    info "       $me step prep|tag|build|sign|verify|publish TAG [LABEL]             on the server"
    exit 2
}

# $1 matches the extended regex $2, in the C locale, where A-Z is the 26 letters and nothing
# else: these checks are what makes the words run puts on the ssh command line safe unquoted
matches() { (LC_ALL=C && [[ $1 =~ $2 ]]); }

is_step() { matches "$1" "^($STEPS)\$"; }

# ---------------------------------------------------------------------------------------------
# Server: the GnuPG home for the forwarded agent, and the step

# gpg keeps the agent socket of a non-default home under /run/user when that directory
# exists, else in the home itself; gpgconf answers for both.
socket_path() { gpgconf --homedir "$HOMEDIR" --list-dirs agent-socket; }

# gpg-connect-agent exits 0 whether an agent answers, nothing listens or the socket is dead,
# so the OK it prints in reply to NOP is what counts.
agent_answers() { gpg-connect-agent --homedir "$HOMEDIR" --no-autostart NOP /bye 2>&1 | grep -x OK >/dev/null; }

# The key release.sh will sign with, without a trailing `!`.
key_spec() {
    local k=${RELEASE_GPG_KEY:-}
    if [ -z "$k" ]; then k=$(git -C "$ROOT" config --get user.signingkey 2>/dev/null || true); fi
    printf '%s\n' "${k%!}"
}

cmd_setup() {
    local f n=0 sock
    need gpg gpgconf gpg-connect-agent
    [ -d "$KEYS_DIR" ] || die 2 "$KEYS_DIR is missing: run git submodule update --init -- release"
    if [ ! -d "$HOMEDIR" ]; then mkdir -m 700 "$HOMEDIR" || die 2 "cannot create $HOMEDIR"; fi
    chmod 700 "$HOMEDIR"
    printf 'no-autostart\n' >"$HOMEDIR/gpg.conf"
    for f in "$KEYS_DIR"/*.asc; do
        [ -f "$f" ] || continue
        gpg --homedir "$HOMEDIR" --batch --quiet --import "$f" >/dev/null 2>&1 || die 2 "gpg could not import $f"
        n=$((n + 1))
    done
    [ "$n" -gt 0 ] || die 2 "no maintainer keys in $KEYS_DIR"
    # the socket directory under /run/user, where gpg uses one; ssh -R binds the socket in it
    gpgconf --homedir "$HOMEDIR" --create-socketdir >/dev/null 2>&1 || true
    sock=$(socket_path)
    [ -d "$(dirname "$sock")" ] || die 2 "the socket directory $(dirname "$sock") does not exist"
    if { [ -e "$sock" ] || [ -L "$sock" ]; } && ! agent_answers; then
        rm -f "$sock"
        info "removed the stale socket $sock"
    fi
    info "GnuPG home for the forwarded agent: $HOMEDIR"
    info "  maintainer keys imported: $n from $KEYS_DIR"
    info "  agent socket gpg uses here: $sock"
    if [ -n "${RELEASE_GPG_KEY:-}" ]; then
        info "  signing key: ${RELEASE_GPG_KEY%!} (from the laptop)"
    else
        info "  signing key: none here; the laptop sends it with each tag or sign"
    fi
    printf '%s\n' "$sock"
}

cmd_check() {
    local sock key listing fpr grips g answer
    need gpg gpgconf gpg-connect-agent
    grep -qx 'no-autostart' "$HOMEDIR/gpg.conf" 2>/dev/null ||
        die 2 "$HOMEDIR is not set up for the forwarded agent: run etc/release-remote.sh setup here, or make release-setup-remote REMOTE=<this host> on the laptop"
    sock=$(socket_path)
    [ -S "$sock" ] || die 2 "no agent socket at $sock: nothing is forwarded here; run make release-tag-remote or release-sign-remote on the laptop, which forwards its gpg-agent"
    agent_answers || die 2 "nothing answers at $sock: the forward from the laptop is gone; run the step again from the laptop"
    key=$(key_spec)
    [ -n "$key" ] || die 2 "no signing key: set RELEASE_GPG_KEY=<fingerprint> or git config user.signingkey"
    listing=$(gpg --homedir "$HOMEDIR" --batch --with-colons --with-keygrip --list-keys -- "$key" 2>/dev/null) ||
        die 2 "signing key $key is not in $HOMEDIR, which holds the maintainer keys of $KEYS_DIR alone"
    fpr=$(printf '%s\n' "$listing" | awk -F: '$1 == "fpr" { print $10; exit }')
    grips=$(printf '%s\n' "$listing" | awk -F: '$1 == "grp" { print $10 }')
    [ -n "$grips" ] || die 2 "no keygrip for $key in $HOMEDIR"
    # HAVEKEY is one of the commands the restricted socket allows; a card key answers through
    # its stub, so a missing YubiKey shows only when the signature is asked for
    for g in $grips; do
        answer=$(gpg-connect-agent --homedir "$HOMEDIR" --no-autostart "HAVEKEY $g" /bye 2>/dev/null | head -n 1) || answer=
        if [ "$answer" = OK ]; then
            info "the agent at $sock holds $fpr"
            return 0
        fi
    done
    die 1 "the agent at $sock does not hold $fpr: is it the laptop's gpg-agent, forwarded with its extra socket, and does gpg --card-status on the laptop show the YubiKey?"
}

# make release-STEP in this checkout; tag and sign sign through the forwarded agent, so they
# check it first and point gpg at the home its socket belongs to
cmd_step() {
    local step=$1 tag=$2 label=${3:-}
    if [ "$step" = tag ] || [ "$step" = sign ]; then
        export GNUPGHOME=$HOMEDIR
        cmd_check
    fi
    need make
    info "==> ${label:+$label }$(uname -n): make release-$step TAG=$tag"
    cd "$ROOT"
    exec make --no-print-directory "release-$step" "TAG=$tag"
}

# ---------------------------------------------------------------------------------------------
# Laptop: the whole step over ssh
#
# Every connection is its own (ControlPath=none): a forward requested through a shared
# ControlMaster connection would outlive the run and make the next run's bind fail.

# Stage 1 of tag and sign: the signing key is here, in a file or on a card that answers, and
# gpg-agent answers, with the restricted socket the server gets. Sets FPR and LSOCK.
local_signing() {
    local key listing n card
    need gpg gpgconf gpg-connect-agent git
    key=$(key_spec)
    [ -n "$key" ] || die 2 "no signing key: set RELEASE_GPG_KEY=<fingerprint> or git config user.signingkey on this machine"
    listing=$(gpg --batch --with-colons --list-secret-keys -- "$key" 2>/dev/null) ||
        die 2 "no secret key for $key in this machine's GnuPG: run this on the laptop that holds the YubiKey"
    n=$(printf '%s\n' "$listing" | awk -F: '$1 == "sec" { n++ } END { print n + 0 }')
    [ "$n" = 1 ] || die 2 "signing key $key matches $n secret keys in this machine's GnuPG; give its fingerprint"
    FPR=$(printf '%s\n' "$listing" | awk -F: '$1 == "fpr" { print $10; exit }')
    matches "$FPR" '^[0-9A-F]+$' || die 2 "gpg lists no fingerprint for $key"
    # field 15 of a secret key line: the serial number of the OpenPGP card that holds the key,
    # `#` for a stub without a card, `+` for a key in a file
    card=$(printf '%s\n' "$listing" |
        awk -F: '($1 == "sec" || $1 == "ssb") && $15 != "" && $15 != "#" && $15 != "+" { print $15; exit }')
    if [ -n "$card" ]; then
        gpg --batch --card-status >/dev/null 2>&1 ||
            die 2 "the key is on an OpenPGP card (serial $card) and no card answers: plug the YubiKey in"
        info "    key $FPR on OpenPGP card $card, present"
    else
        info "    key $FPR, a file key here"
    fi
    # started if need be; the reply counts, not the exit code (see agent_answers)
    gpg-connect-agent NOP /bye 2>&1 | grep -x OK >/dev/null || die 2 "gpg-agent does not answer on this machine"
    LSOCK=$(gpgconf --list-dirs agent-extra-socket)
    [ -S "$LSOCK" ] ||
        die 2 "gpg-agent has no extra socket at $LSOCK: that restricted socket is what the server gets (GnuPG 2.1.13 or newer)"
    # no colon: ssh -R splits its argument on it
    matches "$LSOCK" '^/[A-Za-z0-9_./-]+$' ||
        die 2 "the extra socket path $LSOCK is not one ssh -R takes: an absolute path of letters, digits and _./- alone"
    info "    gpg-agent answers; its restricted socket $LSOCK is what the server gets"
}

# Connection 1 ended without giving the socket path: say why, from its exit code.
setup_failed() {
    case "$1" in
        0) die 2 "setup on $HOST printed no socket path" ;;
        255) die 255 "ssh to $HOST failed (exit 255)" ;;
        *) die "$1" "setup on $HOST failed (exit $1)" ;;
    esac
}

check_rsock() {
    matches "$RSOCK" '^/[A-Za-z0-9_./-]+$' ||
        die 2 "setup on $HOST printed '$RSOCK' for the agent socket, not a path ssh -R takes: an absolute path of letters, digits and _./- alone"
}

# Connection 1 of release-setup-remote: setup on the server, in the foreground. Sets RSOCK,
# the socket path setup prints there.
remote_setup() {
    local rc=0
    RSOCK=$("$SSH" -o ControlPath=none "$HOST" "${PRELUDE}${VARS}./etc/release-remote.sh setup" </dev/null) || rc=$?
    if [ "$rc" != 0 ] || [ -z "$RSOCK" ]; then setup_failed "$rc"; fi
    check_rsock
}

# Connection 1 of tag and sign: setup on the server, after which the connection stays open,
# its remote read waiting on a fifo, until end_hold closes the fifo once the step is over. The
# login session it keeps is what keeps the server's /run/user/<uid>, the directory the
# forward binds the socket in. Sets RSOCK and HOLD_PID.
hold_setup() {
    local rc
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/release-remote.XXXXXX") || die 2 "cannot create a temporary directory"
    trap 'end_hold kill' EXIT
    mkfifo "$WORK/hold"
    exec 3<>"$WORK/hold" # read-write: never blocks, and is the fifo's only writer
    "$SSH" -o ControlPath=none "$HOST" "${PRELUDE}${VARS}./etc/release-remote.sh setup && read -r _" \
        <"$WORK/hold" 3>&- >"$WORK/setup.out" &
    HOLD_PID=$!
    # the socket path is setup's only line of stdout: wait until all of it is there
    until [ -s "$WORK/setup.out" ] && [ -z "$(tail -c 1 "$WORK/setup.out")" ]; do
        if ! kill -0 "$HOLD_PID" 2>/dev/null; then
            rc=0
            wait "$HOLD_PID" || rc=$?
            HOLD_PID=
            setup_failed "$rc"
        fi
        sleep 1
    done
    read -r RSOCK <"$WORK/setup.out"
    check_rsock
}

# Ends connection 1: EOF on the fifo ends its remote read, and the connection is waited for.
# On any exit before the step is over (a failure, ^C) it is killed first, since a connection
# still being made would not see the EOF and, a background command, ignores ^C.
end_hold() {
    exec 3>&-
    if [ -n "${HOLD_PID:-}" ]; then
        if [ "${1:-}" = kill ]; then kill "$HOLD_PID" 2>/dev/null || true; fi
        wait "$HOLD_PID" 2>/dev/null || true
    fi
    HOLD_PID=
    if [ -n "${WORK:-}" ]; then rm -rf "$WORK"; fi
    WORK=
}

cmd_run() {
    local remote=$1 step=$2 tag=${3:-} signing=0 name value rc=0
    if [ "$step" = tag ] || [ "$step" = sign ]; then signing=1; fi

    HOST=${remote%%:*}
    case "$remote" in
        *:*) DIR=${remote#*:} ;;
        *) DIR=$(basename "$ROOT") ;;
    esac
    # no character a shell or ssh treats specially, and no leading - that ssh or cd would
    # take for an option
    if ! matches "$HOST" '^[A-Za-z0-9_.@][A-Za-z0-9_.@-]*$' || ! matches "$DIR" '^[A-Za-z0-9_./][A-Za-z0-9_./-]*$'; then
        info "error: REMOTE=$remote gives host '$HOST' and dir '$DIR'"
        info "usage: REMOTE=host[:dir]: host is the ssh destination, an alias or user@host (letters, digits and _.@-); dir is the checkout there, relative to its home unless absolute (letters, digits and _./-; default: $(basename "$ROOT"))"
        exit 2
    fi
    if [ "$step" != setup ]; then
        matches "$tag" '^[A-Za-z0-9_.+-]+$' || die 2 "TAG=$tag contains characters this wrapper does not pass over ssh"
    fi

    SSH=ssh
    if [ -n "${RELEASE_REMOTE_SSH:-}" ]; then
        [ "${RELEASE_DRY_RUN:-}" = 1 ] || die 2 "RELEASE_REMOTE_SSH is for dry runs: set RELEASE_DRY_RUN=1 or unset it"
        SSH=$RELEASE_REMOTE_SSH
    fi
    need "$SSH"

    # NAME=value words before the script on the remote command line; the character check
    # stands in for quoting. tag and sign forward the fingerprint stage 1 resolves in place of
    # the local RELEASE_GPG_KEY, which may be any key spec gpg takes.
    VARS=
    while IFS= read -r name; do
        case "$name" in
            RELEASE_REMOTE_SSH) continue ;;
            RELEASE_GPG_KEY) if [ "$signing" = 1 ]; then continue; fi ;;
            RELEASE_* | TN_P2P_ALLOW_*) ;;
            *) continue ;;
        esac
        matches "$name" '^(RELEASE|TN_P2P_ALLOW)_[A-Z0-9_]*$' || continue
        value=${!name-}
        matches "$value" '^[A-Za-z0-9_./:@%+=,-]*$' ||
            die 2 "$name contains characters this wrapper does not pass over ssh: letters, digits and _./:@%+=,- alone"
        VARS+="$name=$value "
    done < <(compgen -e | LC_ALL=C sort)

    PRELUDE="cd $DIR || { printf 'error: no directory $DIR on $HOST: pass REMOTE=$HOST:<path of the checkout>\\n' >&2; exit 2; }; "
    PRELUDE+="test -x etc/release-remote.sh || { printf 'error: $HOST:$DIR has no etc/release-remote.sh: pull main there first\\n' >&2; exit 2; }; "

    if [ "$step" = setup ]; then
        info "release-setup-remote: $HOST"
        info "==> 1/1 $HOST: the GnuPG home for the forwarded agent"
        remote_setup
        info "==> done: $HOST is ready for release-tag-remote and release-sign-remote"
        return 0
    fi

    info "release-$step-remote: $tag on $HOST, the step runs there and the YubiKey stays here"
    if [ "$signing" = 1 ]; then
        info "==> 1/4 here: the signing key and gpg-agent"
        local_signing
        VARS+="RELEASE_GPG_KEY=$FPR "
        info "==> 2/4 $HOST: the GnuPG home for the forwarded agent"
        hold_setup
        info "==> 3/4 $HOST: connecting with the agent forwarded to $RSOCK"
        info "    pinentry opens here when the server asks for the signature: enter the PIN and touch the YubiKey only then (an unprompted touch types a one-time password)"
        "$SSH" -t -o ControlPath=none -o ExitOnForwardFailure=yes -R "$RSOCK:$LSOCK" "$HOST" \
            "${PRELUDE}${VARS}./etc/release-remote.sh step $step $tag 4/4" 3>&- || rc=$?
        end_hold
    else
        info "==> 1/2 $HOST: connecting"
        "$SSH" -t -o ControlPath=none "$HOST" "${PRELUDE}${VARS}./etc/release-remote.sh step $step $tag 2/2" || rc=$?
    fi

    case "$rc" in
        0) info "==> done: release-$step $tag on $HOST finished" ;;
        255)
            if [ "$signing" = 1 ]; then
                die 255 "ssh to $HOST failed (exit 255); if it reported \"remote port forwarding failed\", a live session holds $RSOCK on $HOST or sshd forbids streamlocal forwards"
            fi
            die 255 "ssh to $HOST failed (exit 255)"
            ;;
        *) die "$rc" "release-$step $tag on $HOST failed (exit $rc)" ;;
    esac
}

# ---------------------------------------------------------------------------------------------

case "${1:-}" in
    run)
        [ $# -ge 3 ] || usage
        if [ "$3" = setup ]; then
            [ $# -eq 3 ] || usage
        elif ! is_step "$3" || [ $# -ne 4 ] || [ -z "$4" ]; then
            usage
        fi
        cmd_run "$2" "$3" "${4:-}"
        exit 0
        ;;
    setup | check | socket)
        [ $# -eq 1 ] || usage
        ;;
    step)
        if [ $# -lt 3 ] || [ $# -gt 4 ] || ! is_step "$2" || [ -z "$3" ]; then usage; fi
        ;;
    *) usage ;;
esac

HOMEDIR=${RELEASE_REMOTE_GNUPGHOME:-$HOME/.gnupg-tn-release}
case "$HOMEDIR" in
    /*) ;;
    *) die 2 "RELEASE_REMOTE_GNUPGHOME must be an absolute path, got '$HOMEDIR'" ;;
esac
case "$1" in
    setup) cmd_setup ;;
    check) cmd_check ;;
    socket)
        need gpgconf
        socket_path
        ;;
    step) cmd_step "$2" "$3" "${4:-}" ;;
esac
