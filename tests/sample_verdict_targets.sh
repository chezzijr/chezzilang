#!/bin/sh
# TICKET-245 — run deadlock-verdict test targets repeatedly and count the red rounds.
# A verdict bug shows as a rate, so one green run proves nothing (DEC-131: 2 of 8, 4 of 5).
#
#   sh tests/sample_verdict_targets.sh <debug|release> <rounds> <target>...
#
# One cargo command at a time. Prints one line per target and a final `red=<total>`;
# exits 1 when any round was red.
profile=$1
rounds=$2
shift 2
flag=
[ "$profile" = release ] && flag=--release
total=0
for target in "$@"; do
    red=0
    i=0
    while [ "$i" -lt "$rounds" ]; do
        i=$((i + 1))
        if ! cargo test $flag --test "$target" >/dev/null 2>&1; then
            red=$((red + 1))
        fi
    done
    echo "$target ($profile): $red red of $rounds rounds"
    total=$((total + red))
done
echo "red=$total"
[ "$total" -eq 0 ]
