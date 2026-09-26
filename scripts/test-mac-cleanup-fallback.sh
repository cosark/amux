#!/bin/bash
# scripts/mac-cleanup-fallback.sh (DESKT-58): launchd runs the cleanup tick only
# when the amux scheduler has gone quiet, and says so on the board, once a day.
# The tick is a stub here, so each cell observes the fallback's DECISION; the
# tick itself is tested by its own suites.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
FB="$HERE/mac-cleanup-fallback.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
[ -x "$FB" ] || { echo "FAIL: $FB missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { if printf '%s' "$1" | grep -q -- "$2"; then echo yes; else echo no; fi; }

printf '#!/bin/bash\necho "stub tick ran snapshot_floor=$AMUX_CLEANUP_SNAPSHOT_FLOOR_GB"\necho x >> %s/ticks\n' "$FIX" > "$FIX/tick.sh"; chmod +x "$FIX/tick.sh"
printf '#!/bin/bash\nf="${@: -1}"; cat "${f#@}" >> %s/cards.log; echo >> %s/cards.log; printf 201\n' "$FIX" "$FIX" > "$FIX/card.sh"; chmod +x "$FIX/card.sh"
run() { AMUX_CLEANUP_LAST="$FIX/last" AMUX_CLEANUP_FALLBACK_TICK="bash $FIX/tick.sh" AMUX_CLEANUP_FALLBACK_LIB="$HERE/mac-cleanup-tick.sh" \
        AMUX_CLEANUP_STATE_DIR="$FIX/state" AMUX_CLEANUP_CARD_CMD="$FIX/card.sh --data @FILE" bash "$FB" 2>&1; }
ticks() { [ -f "$FIX/ticks" ] && grep -c x "$FIX/ticks" | tr -d ' ' || echo 0; }

echo "1. a fresh tick output means the scheduler is alive: do nothing"
echo fresh > "$FIX/last"
out=$(run)
check "it says nothing to do"                 "yes" "$(has "$out" 'scheduler ran within 45m, nothing to do')"
check "and runs no tick"                      "0"   "$(ticks)"
check "and files no card"                     "no"  "$([ -f "$FIX/cards.log" ] && echo yes || echo no)"

echo "2. a stale tick output means the scheduler is silent: run the tick and say so"
touch -t 202001010000 "$FIX/last"
out=$(run)
check "it names why it ran"                   "yes" "$(has "$out" 'is not running SCHED-465, so launchd is')"
check "the tick ran once"                     "1"   "$(ticks)"
check "with the schedule's snapshot knob"     "yes" "$(has "$out" 'stub tick ran snapshot_floor=1000000')"
check "its output replaced the stale file"    "yes" "$(grep -q 'stub tick ran' "$FIX/last" && echo yes || echo no)"
check "a scheduler-silent card was filed"     "yes" "$(has "$out" 'scheduler-silent card: filed (201)')"
check "the card says what happened"           "yes" "$(grep -q 'SCHED-465 did not run' "$FIX/cards.log" && echo yes || echo no)"

echo "3. silent again the same day: run the tick, but one card per 24h"
touch -t 202001010000 "$FIX/last"
out=$(run)
check "the tick ran again"                    "2"   "$(ticks)"
check "no second card"                        "yes" "$(has "$out" 'already filed within 24h')"
check "exactly one card in total"             "1"   "$(grep -c 'SCHED-465 did not run' "$FIX/cards.log" | tr -d ' ')"

echo "4. no tick output at all counts as silent"
rm -f "$FIX/last"
out=$(run)
check "a missing file is named as the reason" "yes" "$(has "$out" "no tick output at $FIX/last")"
check "and the tick ran"                      "3"   "$(ticks)"

echo "5. the staleness window is a knob"
touch -t 202001010000 "$FIX/last"
out=$(AMUX_CLEANUP_FALLBACK_STALE_MIN=99999999 run)
check "a window longer than the file's age does nothing" "3" "$(ticks)"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-fallback — all checks passed"; exit 0; fi
echo "mac-cleanup-fallback: $fails check(s) FAILED"; exit 1
