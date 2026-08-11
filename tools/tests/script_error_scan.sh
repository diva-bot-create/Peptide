#!/usr/bin/env bash
# script_error_scan.sh — drive a converted character through every playable state and
# report every hscript error the engine raises, deduped by origin.
#
# The spawn sweep (batch_spawn_test.sh) answers "does it launch and move" (P0). This
# answers "does its translated code actually RUN": Fraymakers traps a script error,
# logs it, and carries on with that handler dead, so a broken frame script is invisible
# from the outside. Peptide surfaces those as SCRIPTERR (PEPTIDE_ENGINE_LOGGING); this
# collects them into a per-character signature you can diff across a converter change.
#
# Usage:
#   tools/tests/script_error_scan.sh <id> [<id> …]         # scan, print the report
#   SCAN_OUT=<dir> tools/tests/script_error_scan.sh <id>    # also write <dir>/<id>.errors
#   SCAN_EXPORT=0 tools/tests/script_error_scan.sh <id>     # skip convert+export (reuse .fra)
#
# Diffing a converter change:
#   SCAN_OUT=/tmp/before tools/tests/script_error_scan.sh sandbag pichu falco
#   …make the change…
#   SCAN_OUT=/tmp/after  tools/tests/script_error_scan.sh sandbag pichu falco
#   diff -r /tmp/before /tmp/after
#
# An error line is `<char>:<animation>:<frameN>:…:<message>`, so the signature is stable
# across runs (no timestamps, no positions) and a diff shows exactly which handlers
# changed status.
#
# Two things the driving has to get right, or the run reports garbage:
#   - PARK BETWEEN STATES. driving FALL / the aerials leaves the character airborne, and
#     it drifts out of the blast zone and dies. everything after that errors on frame 1
#     of every action. `scenario` re-places both players and resets them to neutral, so
#     it runs before every state.
#   - TWO CHARACTERS. `scenario` addresses p0 AND p1, so a 1-character roster makes every
#     park half-fail on a null p1 and floods the signature with hscript-origin noise.
set -u
cd "$(cd "$(dirname "$0")/../.." && pwd)"

BIN=./build/release/peptide
SSFS="${SSF2_SSFS_DIR:-../ssf2-ssfs}"
SCAN_OUT="${SCAN_OUT:-}"
SCAN_EXPORT="${SCAN_EXPORT:-1}"
GAP="${SCAN_GAP:-1}"
PARK="${SCAN_PARK:-scenario 0,73 140,73}"
[ -n "$SCAN_OUT" ] && mkdir -p "$SCAN_OUT"

# Every state a character can be driven into from neutral. Aerials and the knockdown
# chain are included: they still run their frame scripts on the ground, and the park
# puts the character back before the next one.
STATES="STAND WALK_LOOP DASH RUN SKID CROUCH_IN CROUCH_LOOP CROUCH_OUT \
JUMP_SQUAT JUMP_IN JUMP_LOOP FALL LAND \
SHIELD_IN SHIELD_LOOP SHIELD_OUT ROLL SPOT_DODGE TECH \
HURT_LIGHT HURT_MEDIUM HURT_HEAVY TUMBLE CRASH_BOUNCE CRASH_GET_UP \
JAB DASH_ATTACK TILT_FORWARD TILT_UP TILT_DOWN \
STRONG_FORWARD_IN STRONG_FORWARD_ATTACK STRONG_UP_IN STRONG_UP_ATTACK \
STRONG_DOWN_IN STRONG_DOWN_ATTACK \
AERIAL_NEUTRAL AERIAL_FORWARD AERIAL_BACK AERIAL_UP AERIAL_DOWN \
SPECIAL_NEUTRAL SPECIAL_SIDE SPECIAL_UP SPECIAL_DOWN \
GRAB GRAB_HOLD GRAB_PUMMEL THROW_UP THROW_DOWN THROW_FORWARD THROW_BACK \
LEDGE_IN LEDGE_CLIMB LEDGE_ATTACK LEDGE_JUMP EMOTE"

scan_one() {
  local c="$1" log="/tmp/scan_${1}.log"

  if [ "$SCAN_EXPORT" = "1" ]; then
    echo "[$c] convert" >&2
    "$BIN" convert "$SSFS/$c.ssf" -y >/dev/null 2>&1 || { echo "[$c] CONVERT FAILED" >&2; return 1; }
    echo "[$c] export (publishing a fresh .fra — an old one silently lies)" >&2
    "$BIN" export --project "$PWD/characters/$c/$c.fraytools" >/dev/null 2>&1 \
      || { echo "[$c] EXPORT FAILED" >&2; return 1; }
  fi

  # One session. `toState` is enough to run a state's frame scripts; the move doesn't
  # need to connect, only to execute. The leading no-op read lets the character finish
  # constructing before the first park (a command sent too early reports its own
  # hscript-origin error and pollutes the signature).
  local cmds=("spawn $c,$c thespire commandervideoassist" "match.getCharacters()[0].getStateName()")
  local s
  for s in $STATES; do
    cmds+=("$PARK" "match.getCharacters()[0].toState(CState.$s)")
  done

  echo "[$c] driving ${#cmds[@]} commands across ${#STATES} states" >&2
  FRAY_CHAR="$c" tools/runseq.sh "$GAP" "${cmds[@]}" >"$log" 2>&1

  # Signature: error text only, deduped, sorted. No counts — a handler that fires every
  # frame would otherwise swamp the diff with timing noise.
  # `hscript`-origin lines are OUR OWN driving commands (a park that raced the character's
  # construction), not the character's code, so they're dropped: they vary with engine
  # timing and would show up as phantom drift between two runs of the same build.
  local sig launched crash
  sig=$(grep -o 'Script Interpret Error: .*' "$log" \
        | sed 's/^Script Interpret Error: //; s/ (origin:.*//' \
        | grep -v '^hscript:' \
        | sort -u)
  launched=$(grep -c 'LAUNCHED' "$log")
  # anchor on the crash REPORT, not the word: CRASH_BOUNCE / CRASH_GET_UP are states we
  # deliberately drive, and matching them read as 4 crashes on a clean run.
  crash=$(grep -c 'Engine exception\|peptide-bridge: engine crashed\|\[crash\]' "$log")

  {
    echo "# $c  launched=$launched crash=$crash errors=$(printf '%s' "$sig" | grep -c .)"
    printf '%s\n' "$sig"
  } | if [ -n "$SCAN_OUT" ]; then tee "$SCAN_OUT/$c.errors"; else cat; fi
}

for c in "$@"; do
  scan_one "$c"
  echo
done
