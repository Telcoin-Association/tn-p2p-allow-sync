#!/bin/sh
# Stands in for nft in the unit tests of src/nft.rs.
# The runner clears the environment, so the fixtures are found through $0.
# It answers the calls the runner makes with the nft 1.0.9 captures and fails anything else with 64.
dir=$(dirname "$0")
fixture=$dir/ubuntu-24.04-1.0.9
case "$*" in
"-j list tables inet")
    exec cat "$fixture-tables-present.json"
    ;;
"-j list table inet tn_p2p_allow")
    exec cat "$fixture-program.json"
    ;;
"delete table inet tn_p2p_allow")
    exit 0
    ;;
"-c -f -")
    # read everything before writing anything, as nft does; the x keeps trailing newlines
    input=$(cat; printf x)
    input=${input%x}
    case "$input" in
    "")
        echo "fake-nft: no program on stdin" >&2
        exit 1
        ;;
    *bogus*)
        cat "$fixture-syntax-error.stderr" >&2
        exit 1
        ;;
    esac
    printf '%s' "$input"
    exit 0
    ;;
esac
case "$1" in
argv)
    # each argument, where stdin points, then the environment exactly as exec passed it
    for arg in "$@"; do
        printf 'arg=%s\n' "$arg"
    done
    printf 'stdin=%s\n' "$(readlink /proc/$$/fd/0)"
    tr '\0' '\n' < /proc/$$/environ | sed 's/^/env=/'
    exit 0
    ;;
-f)
    if [ $# -eq 2 ] && [ -r "$2" ]; then
        exit 0
    fi
    printf 'internal:0:0-0: Error: Could not open file "%s": No such file or directory\n\n\n' "$2" >&2
    exit 1
    ;;
eperm)
    cat "$fixture-eperm-list.stderr" >&2
    exit 1
    ;;
kill)
    kill -KILL $$
    ;;
esac
echo "fake-nft: unexpected arguments: $*" >&2
exit 64
