#!/usr/bin/env bash
# Signing from the build server with the YubiKey on the laptop: the laptop's gpg-agent,
# forwarded over ssh, stands in for a local one.
#
# gpg talks to its agent over a Unix socket, so forwarding the laptop's agent socket into a
# GnuPG home of its own on this machine lets `release.sh tag` and `release.sh sign` run here
# while the private key stays on the laptop's YubiKey: each signature is one PKSIGN over the
# forwarded socket, and the PIN and the touch happen on the laptop. The laptop forwards its
# "extra" socket, which gpg-agent serves in restricted mode: signing and decryption, no key
# export and no control of the agent. The home here holds only the maintainers' public keys
# from release/keys and `no-autostart`, so nothing on this machine ever starts an agent in
# it: the socket is the forwarded one or none.
#
#   etc/release-remote.sh setup [HOME]   create HOME, import release/keys, print the ssh command
#   etc/release-remote.sh check [HOME]   the forwarded agent answers and holds the signing key
#   etc/release-remote.sh socket [HOME]  print the agent socket path gpg uses for HOME
#
# HOME defaults to ~/.gnupg-tn-release; the Makefile passes RELEASE_REMOTE_GNUPGHOME. The
# signing key is RELEASE_GPG_KEY, else git's user.signingkey, as release.sh resolves it.
# Exit codes: 0 ok; 1 the agent does not hold the signing key; 2 usage or setup problem.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
KEYS_DIR=$ROOT/release/keys

die() {
    local rc=$1
    shift
    printf 'error: %s\n' "$*" >&2
    exit "$rc"
}

info() { printf '%s\n' "$*" >&2; }

# gpg keeps the agent socket of a non-default home under /run/user when that directory
# exists, else in the home itself; gpgconf answers for both.
socket_path() { gpgconf --homedir "$HOMEDIR" --list-dirs agent-socket; }

agent_answers() { gpg-connect-agent --homedir "$HOMEDIR" --no-autostart /bye >/dev/null 2>&1; }

# The key release.sh will sign with, without a trailing `!`.
key_spec() {
    local k=${RELEASE_GPG_KEY:-}
    if [ -z "$k" ]; then k=$(git -C "$ROOT" config --get user.signingkey 2>/dev/null || true); fi
    printf '%s\n' "${k%!}"
}

cmd_setup() {
    local tool f n=0 sock key
    for tool in gpg gpgconf gpg-connect-agent; do
        command -v "$tool" >/dev/null 2>&1 || die 2 "missing required tool: $tool"
    done
    [ -d "$KEYS_DIR" ] || die 2 "$KEYS_DIR is missing: run git submodule update --init -- release"
    if [ ! -d "$HOMEDIR" ]; then mkdir -m 700 "$HOMEDIR"; fi
    chmod 700 "$HOMEDIR"
    printf 'no-autostart\n' >"$HOMEDIR/gpg.conf"
    for f in "$KEYS_DIR"/*.asc; do
        [ -f "$f" ] || continue
        gpg --homedir "$HOMEDIR" --batch --quiet --import "$f" 2>/dev/null || die 2 "gpg could not import $f"
        n=$((n + 1))
    done
    [ "$n" -gt 0 ] || die 2 "no maintainer keys in $KEYS_DIR"
    # the socket directory under /run/user, where gpg uses one; ssh -R binds the socket in it
    gpgconf --homedir "$HOMEDIR" --create-socketdir >/dev/null 2>&1 || true
    sock=$(socket_path)
    [ -d "$(dirname "$sock")" ] || die 2 "the socket directory $(dirname "$sock") does not exist"
    if [ -e "$sock" ] && ! agent_answers; then
        rm -f "$sock"
        info "removed the stale socket $sock"
    fi
    key=$(key_spec)
    info "GnuPG home for signing over ssh: $HOMEDIR"
    info "  maintainer keys imported: $n from $KEYS_DIR"
    info "  agent socket gpg uses here: $sock"
    if [ -n "$key" ]; then
        info "  signing key: $key"
    else
        info "  signing key: none yet; set RELEASE_GPG_KEY=<fingerprint> or git config user.signingkey"
    fi
    info ""
    info "From the laptop, with the YubiKey plugged in and gpg --card-status showing it, connect with"
    info ""
    info "  ssh -R $sock:\"\$(gpgconf --list-dirs agent-extra-socket)\" $(id -un)@$(hostname)"
    info ""
    info "and in that session run make release-tag-remote or make release-sign-remote from $ROOT."
    info "A forward that fails to bind (\"remote port forwarding failed\") means a socket was left"
    info "behind: run this setup again, then reconnect."
}

cmd_check() {
    local sock key listing fpr grips g answer
    grep -qx 'no-autostart' "$HOMEDIR/gpg.conf" 2>/dev/null ||
        die 2 "$HOMEDIR is not set up for signing over ssh: run make release-setup-remote"
    sock=$(socket_path)
    [ -S "$sock" ] || die 2 "no agent socket at $sock: connect from the laptop with the ssh -R command make release-setup-remote prints"
    agent_answers || die 2 "nothing answers at $sock: the ssh session's forward is gone; reconnect from the laptop"
    key=$(key_spec)
    [ -n "$key" ] || die 2 "no signing key: set RELEASE_GPG_KEY=<fingerprint> or git config user.signingkey"
    listing=$(gpg --homedir "$HOMEDIR" --batch --with-colons --with-keygrip --list-keys -- "$key" 2>/dev/null) ||
        die 2 "signing key $key is not in $HOMEDIR: run make release-setup-remote again"
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

case "${1:-}" in
    setup | check | socket) ;;
    *)
        printf 'usage: %s setup|check|socket [GNUPGHOME]\n' "${0##*/}" >&2
        exit 2
        ;;
esac
HOMEDIR=${2:-$HOME/.gnupg-tn-release}
case "$HOMEDIR" in
    /*) ;;
    *) die 2 "GNUPGHOME must be an absolute path, got '$HOMEDIR'" ;;
esac
case "$1" in
    setup) cmd_setup ;;
    check) cmd_check ;;
    socket) socket_path ;;
esac
