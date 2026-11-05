#!/bin/sh
# Stands in for nft in the integration tests of the sync (tests/sync.rs), with sh and awk only.
# The tool clears the environment, so everything is found through $0: the state lives in
# state/ next to the path it runs as, each test's own link to it, so tests never share state.
#
#   state/argv.log      one line per call, the arguments joined by spaces
#   state/loaded.N.nft  what the Nth -f, -f - or -c -f - call was given, N counting from 1
#   state/table         the live table, one object per line, parsed from the last loaded program:
#                       `counter NAME`, `set NAME TYPE [interval]`, `chain NAME TYPE HOOK PRIO POLICY`,
#                       `rule TEXT` and `elem SET VALUE`; absent while no table is loaded, and a
#                       test deletes an `elem` line to delete that element by hand
#   state/fail-next     knob: the next -f exits 1 with an nft-like message, then the knob is gone
#   state/eperm         knob: every call prints nft's "Operation not permitted" and exits 1
#
# The two -j list calls print the JSON shape nft 1.0.9 prints, with both counters at 0.
# `-c -f -` checks nothing and changes nothing; `delete table` removes state/table.
# Any other call exits 64.
set -u
dir=$(dirname "$0")
state=$dir/state
table=$state/table
mkdir -p "$state" || exit 70
printf '%s\n' "$*" >>"$state/argv.log"

if [ -e "$state/eperm" ]; then
    echo "Operation not permitted (you must be root)" >&2
    echo "netlink: Error: cache initialization failed: Operation not permitted" >&2
    exit 1
fi

meta='{"metainfo": {"version": "1.0.9", "release_name": "Old Doc Yak #3", "json_schema_version": 1}}'

# record SOURCE: copies SOURCE, or stdin for -, to the next loaded.N.nft and sets $loaded
record() {
    n=1
    while [ -e "$state/loaded.$n.nft" ]; do
        n=$((n + 1))
    done
    loaded=$state/loaded.$n.nft
    if [ "$1" = - ]; then
        cat >"$loaded"
    elif [ -r "$1" ]; then
        cat "$1" >"$loaded"
    else
        printf 'internal:0:0-0: Error: Could not open file "%s": No such file or directory\n' "$1" >&2
        return 1
    fi
}

# parse PROGRAM: the objects of the table the program leaves loaded, one per line
parse() {
    awk '
    function elements(set, text,    n, parts, i) {
        sub(/[}].*/, "", text)
        gsub(/,/, " ", text)
        n = split(text, parts, /[ \t]+/)
        for (i = 1; i <= n; i++)
            if (parts[i] != "")
                print "elem " set " " parts[i]
    }
    /^[ \t]*#/ { next }
    open_set != "" {
        buffer = buffer " " $0
        if (index($0, "}")) {
            elements(open_set, buffer)
            open_set = ""
        }
        next
    }
    /^add element inet tn_p2p_allow [a-z0-9_]+ [{]/ {
        buffer = $0
        sub(/^[^{]*[{]/, "", buffer)
        if (index(buffer, "}"))
            elements($5, buffer)
        else
            open_set = $5
        next
    }
    /^[ \t]*counter [a-z0-9_]+ [{] *[}]/ { print "counter " $2; next }
    /^[ \t]*set [a-z0-9_]+ [{]/ {
        type = $5
        sub(/;.*/, "", type)
        print "set " $2 " " type (index($0, "flags interval") ? " interval" : "")
        next
    }
    /^[ \t]*chain [a-z0-9_]+ [{]/ { chain = $2; next }
    chain != "" && /^[ \t]*type / {
        prio = $6; policy = $8
        sub(/;$/, "", prio); sub(/;$/, "", policy)
        print "chain " chain " " $2 " " $4 " " prio " " policy
        next
    }
    chain != "" && /^[ \t]*[}]/ { chain = ""; next }
    chain != "" && NF > 0 {
        rule = $0
        sub(/^[ \t]+/, "", rule)
        print "rule " rule
        next
    }
    ' "$1"
}

# listing: the table in the JSON shape of nft -j list table inet tn_p2p_allow
listing() {
    awk -v meta="$meta" '
    function json_element(value,    parts) {
        if (index(value, "/")) {
            split(value, parts, "/")
            return "{\"prefix\": {\"addr\": \"" parts[1] "\", \"len\": " parts[2] "}}"
        }
        return "\"" value "\""
    }
    # a rule as nft lists it, from its table line, so a test edits a verdict or a port list by hand there
    function json_rule(text,    expr, ports, saddr, words, n, name) {
        ports = text
        sub(/^[^{]*[{] */, "", ports)
        sub(/ *[}].*/, "", ports)
        # nft lists one port as a number and more as an anonymous set
        ports = (index(ports, ",") ? "{\"set\": [" ports "]}" : ports)
        expr = "{\"match\": {\"op\": \"==\", \"left\": {\"payload\": {\"protocol\": \"udp\", \"field\": \"dport\"}}, \"right\": " ports "}}"
        if (match(text, /ip6? saddr @[a-z0-9_]+/)) {
            saddr = substr(text, RSTART, RLENGTH)
            n = split(saddr, words, " ")
            expr = expr ", {\"match\": {\"op\": \"==\", \"left\": {\"payload\": {\"protocol\": \"" words[1] "\", \"field\": \"saddr\"}}, \"right\": \"" words[3] "\"}}"
        }
        if (match(text, /counter name "[a-z0-9_]+"/)) {
            name = substr(text, RSTART + 14, RLENGTH - 15)
            expr = expr ", {\"counter\": \"" name "\"}"
        }
        n = split(text, words, " ")
        return expr ", {\"" words[n] "\": null}"
    }
    $1 == "counter" { counters[++ncounters] = $2 }
    $1 == "set" { sets[++nsets] = $2; types[$2] = $3; interval[$2] = ($4 == "interval") }
    $1 == "chain" { chains[++nchains] = $0 }
    $1 == "rule" { rule = $0; sub(/^rule /, "", rule); rules[++nrules] = rule }
    $1 == "elem" { elems[$2] = elems[$2] (elems[$2] == "" ? "" : ", ") json_element($3) }
    END {
        handle = 1
        printf "{\"nftables\": [%s, {\"table\": {\"family\": \"inet\", \"name\": \"tn_p2p_allow\", \"handle\": %d}}", meta, handle
        for (i = 1; i <= ncounters; i++)
            printf ", {\"counter\": {\"family\": \"inet\", \"name\": \"%s\", \"table\": \"tn_p2p_allow\", \"handle\": %d, \"packets\": 0, \"bytes\": 0}}", counters[i], ++handle
        for (i = 1; i <= nsets; i++) {
            name = sets[i]
            printf ", {\"set\": {\"family\": \"inet\", \"name\": \"%s\", \"table\": \"tn_p2p_allow\", \"type\": \"%s\", \"handle\": %d", name, types[name], ++handle
            if (interval[name])
                printf ", \"flags\": [\"interval\"]"
            if (elems[name] != "")
                printf ", \"elem\": [%s]", elems[name]
            printf "}}"
        }
        for (i = 1; i <= nchains; i++) {
            split(chains[i], c, " ")
            printf ", {\"chain\": {\"family\": \"inet\", \"table\": \"tn_p2p_allow\", \"name\": \"%s\", \"handle\": %d, \"type\": \"%s\", \"hook\": \"%s\", \"prio\": %s, \"policy\": \"%s\"}}", c[2], ++handle, c[3], c[4], c[5], c[6]
            chain = c[2]
        }
        for (i = 1; i <= nrules; i++)
            printf ", {\"rule\": {\"family\": \"inet\", \"table\": \"tn_p2p_allow\", \"chain\": \"%s\", \"handle\": %d, \"expr\": [%s]}}", chain, ++handle, json_rule(rules[i])
        print "]}"
    }
    ' "$table"
}

case "$*" in
"-j list tables inet")
    if [ -e "$table" ]; then
        printf '{"nftables": [%s, {"table": {"family": "inet", "name": "tn_p2p_allow", "handle": 1}}]}\n' "$meta"
    else
        printf '{"nftables": [%s]}\n' "$meta"
    fi
    exit 0
    ;;
"-j list table inet tn_p2p_allow")
    if [ ! -e "$table" ]; then
        printf 'Error: No such file or directory\nlist table inet tn_p2p_allow\n                ^^^^^^^^^^^^\n' >&2
        exit 1
    fi
    listing
    exit 0
    ;;
"delete table inet tn_p2p_allow")
    if [ ! -e "$table" ]; then
        printf 'Error: No such file or directory\ndelete table inet tn_p2p_allow\n                 ^^^^^^^^^^^^\n' >&2
        exit 1
    fi
    rm -f "$table"
    exit 0
    ;;
"-c -f -")
    record - || exit 1
    exit 0
    ;;
esac
if [ $# -eq 2 ] && [ "$1" = -f ]; then
    record "$2" || exit 1
    if [ -e "$state/fail-next" ]; then
        rm -f "$state/fail-next"
        printf '%s:1:1-5: Error: simulated failure\n' "$2" >&2
        exit 1
    fi
    parse "$loaded" >"$table.new" && mv "$table.new" "$table" || exit 70
    exit 0
fi
echo "fake-nft: unexpected arguments: $*" >&2
exit 64
