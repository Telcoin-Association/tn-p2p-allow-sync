#!/bin/sh
# usage: capture.sh OUTDIR, run as root inside a container with CAP_NET_ADMIN
set -u
out=$1
d=$(dirname "$0")
mkdir -p "$out"
nft --version > "$out/version.txt" 2>&1
run() {
    name=$1
    shift
    "$@" > "$out/$name.out" 2> "$out/$name.err"
    echo $? > "$out/$name.rc"
}
run tables-none nft -j list tables inet
run absent nft -j list table inet tn_p2p_allow
run delete-absent nft delete table inet tn_p2p_allow
nft -f "$d/other.nft"
run tables-other nft -j list tables inet
nft delete table inet filter
for c in program ipv6 empty forms; do
    run "check-$c" sh -c "nft -c -f - < $d/$c.nft"
    run "load-$c" nft -f "$d/$c.nft"
    run "$c" nft -j list table inet tn_p2p_allow
    if [ "$c" = program ]; then
        run tables-present nft -j list tables inet
        nft -f "$d/other.nft"
        run tables-both nft -j list tables inet
        nft delete table inet filter
        run reload-program nft -f "$d/program.nft"
        run program-reloaded nft -j list table inet tn_p2p_allow
        run eperm-root-nocap-list setpriv --bounding-set=-net_admin nft -j list tables inet
        run eperm-root-nocap-load setpriv --bounding-set=-net_admin nft -f "$d/program.nft"
        run eperm-root-nocap-check sh -c "setpriv --bounding-set=-net_admin nft -c -f - < $d/program.nft"
        run eperm-user-list setpriv --reuid=65534 --regid=65534 --clear-groups nft -j list tables inet
        run eperm-user-table setpriv --reuid=65534 --regid=65534 --clear-groups nft -j list table inet tn_p2p_allow
        run eperm-user-load setpriv --reuid=65534 --regid=65534 --clear-groups nft -f "$d/program.nft"
        run eperm-user-check sh -c "setpriv --reuid=65534 --regid=65534 --clear-groups nft -c -f - < $d/program.nft"
        run delete-present nft delete table inet tn_p2p_allow
        nft -f "$d/program.nft"
    fi
    nft delete table inet tn_p2p_allow
done
run syntax-error sh -c "printf 'table inet tn_p2p_allow { bogus }\n' | nft -c -f -"
run missing-file nft -f /nonexistent.nft
# counters after traffic: 192.0.2.10 is on lo, so a datagram to it leaves from it and is listed;
# one to 127.0.0.1 leaves from 127.0.0.1 and is not
run load-traffic nft -f "$d/traffic.nft"
ip addr add 192.0.2.10/32 dev lo
bash -c 'for i in 1 2 3; do printf x > /dev/udp/192.0.2.10/49590; done; for i in 1 2; do printf yy > /dev/udp/127.0.0.1/49594; done; printf z > /dev/udp/127.0.0.1/40000'
run traffic nft -j list table inet tn_p2p_allow
run reload-traffic nft -f "$d/traffic.nft"
run traffic-reloaded nft -j list table inet tn_p2p_allow
nft delete table inet tn_p2p_allow
# The cases below come after all the others so that the handles in the files above stay as they were.
# dormant: the table switched off by hand. A datagram from 127.0.0.1, which is not listed, to a P2P port
# is counted before the table is dormant, not while it is, and again once the program is re-applied
run load-dormant nft -f "$d/program.nft"
bash -c 'printf x > /dev/udp/127.0.0.1/49590'
run dormant-add nft add table inet tn_p2p_allow '{ flags dormant; }'
bash -c 'for i in 1 2; do printf x > /dev/udp/127.0.0.1/49590; done'
run dormant nft -j list table inet tn_p2p_allow
run dormant-tables nft -j list tables inet
run dormant-text nft list table inet tn_p2p_allow
run reapply-dormant nft -f "$d/program.nft"
bash -c 'for i in 1 2; do printf x > /dev/udp/127.0.0.1/49590; done'
run dormant-reapplied nft -j list table inet tn_p2p_allow
run dormant-reapplied-text nft list table inet tn_p2p_allow
nft delete table inet tn_p2p_allow
# the same flag on a table holding every element form: which garbage nft 1.0.x prints depends on the content
nft -f "$d/forms.nft"
nft add table inet tn_p2p_allow '{ flags dormant; }'
run dormant-forms nft -j list table inet tn_p2p_allow
nft delete table inet tn_p2p_allow
for c in adjacent single-port; do
    run "check-$c" sh -c "nft -c -f - < $d/$c.nft"
    run "load-$c" nft -f "$d/$c.nft"
    run "$c" nft -j list table inet tn_p2p_allow
    run "reload-$c" nft -f "$d/$c.nft"
    run "$c-reloaded" nft -j list table inet tn_p2p_allow
    nft delete table inet tn_p2p_allow
done
# hand the files back to whoever owns the checkout
chown -R "$(stat -c %u:%g "$d")" "$out"
