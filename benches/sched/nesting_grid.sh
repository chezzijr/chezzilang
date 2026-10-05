#!/bin/bash
# TICKET-211 — runner count by nesting shape. Usage: nesting_grid.sh <chezzi binary> <N>
# Shapes: {flat, nested in a spawned task, two deep, nested in an Executor job} x outer body
# {open, closed}, each 8 spawns of a `burn()` of N iterations, then `churn2.chz`/`churn8.chz`
# beside this script: 20000 top-level `parallel:` rounds of K tiny spawns (they ignore N). Each
# cell runs at T in {2, 4, 8}
# and prints "<shape> T=<t> <real> s real <user> s user".
set -euo pipefail
B=$(realpath "$1")
N=${2:-3000000}
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
hdr="import std.concurrency
fn burn():
    i := 0
    while i < $N:
        i += 1
fn burn_send(done: Channel[int]):
    burn()
    done.send(1)
fn fan():
    parallel:
        for _ in 0..8:
            spawn burn()
fn fan2():
    parallel:
        spawn fan()
fn fan_send(done: Channel[int]):
    fan()
    done.send(1)
fn fan2_send(done: Channel[int]):
    fan2()
    done.send(1)
"
printf '%s\nparallel:\n    for _ in 0..8:\n        spawn burn()\n' "$hdr" > "$dir/flat_closed.chz"
printf '%s\ndone := Channel[int](8)\nparallel:\n    for _ in 0..8:\n        spawn burn_send(done)\n    for _ in 0..8:\n        done.recv()\n' "$hdr" > "$dir/flat_open.chz"
printf '%s\nparallel:\n    spawn fan()\n' "$hdr" > "$dir/nested_closed.chz"
printf '%s\ndone := Channel[int](1)\nparallel:\n    spawn fan_send(done)\n    done.recv()\n' "$hdr" > "$dir/nested_open.chz"
printf '%s\nparallel:\n    spawn fan2()\n' "$hdr" > "$dir/twodeep_closed.chz"
printf '%s\ndone := Channel[int](1)\nparallel:\n    spawn fan2_send(done)\n    done.recv()\n' "$hdr" > "$dir/twodeep_open.chz"
printf '%s\nex := Executor()\nex.submit(fn(): fan())\nex.shutdown()\n' "$hdr" > "$dir/exec_closed.chz"
printf '%s\ndone := Channel[int](1)\nex := Executor()\nex.submit(fn(): fan_send(done))\ndone.recv()\nex.shutdown()\n' "$hdr" > "$dir/exec_open.chz"
here=$(dirname "$0")
uptime
for f in flat_closed flat_open nested_closed nested_open twodeep_closed twodeep_open exec_closed exec_open; do
  for t in 2 4 8; do
    TIMEFORMAT="$f T=$t %R s real %U s user"
    time (timeout 120 "$B" run --threads="$t" "$dir/$f.chz" >/dev/null)
  done
done
for k in 2 8; do
  for t in 2 4 8; do
    TIMEFORMAT="churn$k T=$t %R s real %U s user"
    time (timeout 120 "$B" run --threads="$t" "$here/churn$k.chz" >/dev/null)
  done
done
