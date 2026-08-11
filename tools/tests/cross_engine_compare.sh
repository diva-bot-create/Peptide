#!/usr/bin/env bash
# cross_engine_compare.sh — drive the SAME inputs on SSF2 and on the converted
# Fraymakers character, and diff what each engine actually did.
#
# This is the only check that can tell you a conversion is RIGHT rather than merely
# alive. Everything else we have compares the converted output against itself: the
# goldens catch drift, the error scan catches code that throws, parity_check compares
# hitbox numbers against the source data. None of them can see an animation that plays
# the wrong number of frames, a move that travels the wrong distance, or a state that
# doesn't come out at all — because there's nothing to compare against except SSF2.
#
# Inputs, not states. `toState`/`setState` mean different things in the two engines,
# so driving them isn't a fair comparison; a controller input is the one thing that
# means literally the same on both sides. Every step is paced by `await` (the engine's
# own frame counter), never by a wall-clock delay.
#
# Usage:
#   tools/tests/cross_engine_compare.sh <char> [<input> …]
#   tools/tests/cross_engine_compare.sh mario attack special
#
# Output: a per-input table of (animation, frames played, distance travelled) from each
# engine.
#
# WHAT THIS CAN AND CANNOT TELL YOU RIGHT NOW.
#   * ANIMATION IDENTITY is trustworthy on both sides. "the same input produces move X
#     here and move Y there" is a real answer, and it's the check nothing else could do.
#   * FRAME COUNTS are trustworthy on the Fraymakers side only. SSF2's are read by
#     polling, and every field is a separate reflection round trip over file-IPC, so a
#     sample lands roughly every ~14 SSF2 frames — a 14-frame attack finishes between two
#     samples and reports frames_seen=0. Faster polling is not the fix; SSF2 has to record
#     its own label+frame each tick and hand over the trace. Until then, read the SSF2
#     frame column as "did it play at all", not as a count.
#   * The eventual comparison is a RATIO, not equality: SSF2 runs at 30fps and Fraymakers
#     at 60, and the converter doubles frame counts, so an SSF2 animation of N frames
#     should read as ~2N in Fraymakers.
set -u
cd "$(cd "$(dirname "$0")/../.." && pwd)"

BIN=./build/release/peptide
CHAR="${1:?usage: cross_engine_compare.sh <char> [input …]}"; shift
INPUTS=("$@"); [ ${#INPUTS[@]} -eq 0 ] && INPUTS=(attack special jump)
OUT="${COMPARE_OUT:-/tmp/cross_$CHAR}"
mkdir -p "$OUT"

# ── SSF2 side ────────────────────────────────────────────────────────────────
# The patched app + a session; `tell` queues a command, the session mirrors engine
# output to out.log. Park before each input: holding a direction walks the fighter off
# battlefield and into a death, and every observation after that is of a respawn.
ssf2_side() {
  pkill -f "peptide ssf2 session" 2>/dev/null; pkill -f "SSF2-patched" 2>/dev/null
  sleep 2; rm -rf "$HOME/.peptide/session" 2>/dev/null
  "$BIN" ssf2 session --char "$CHAR" > "$OUT/ssf2.log" 2>&1 &
  local sess=$!
  # Ask the ENGINE whether the match is up, rather than guessing from a log line.
  # `awaitmatch` polls until a character is readable and answers MATCHREADY either way;
  # grepping for ANIM: was a guess that failed whenever the engine happened not to emit
  # one before the timeout.
  local waited=0
  until grep -q "peptide ssf2 tell" "$OUT/ssf2.log" 2>/dev/null; do
    sleep 2; waited=$((waited+2))
    [ $waited -gt 90 ] && { echo "[ssf2] session never came up" >&2; kill $sess 2>/dev/null; return 1; }
  done
  "$BIN" ssf2 tell "awaitmatch p0 90" >/dev/null 2>&1
  waited=0
  until grep -q "MATCHREADY:" "$OUT/ssf2.log" 2>/dev/null; do
    sleep 2; waited=$((waited+2))
    [ $waited -gt 90 ] && { echo "[ssf2] never reached a match" >&2; kill $sess 2>/dev/null; return 1; }
  done
  if grep -q "MATCHREADY:none" "$OUT/ssf2.log"; then
    echo "[ssf2] match never became readable" >&2; kill $sess 2>/dev/null; return 1
  fi
  local i seen=0
  for i in "${INPUTS[@]}"; do
    "$BIN" ssf2 tell "reset"        >/dev/null 2>&1; sleep 1
    "$BIN" ssf2 tell "MARK $i"      >/dev/null 2>&1
    # seq and await MUST be queued back to back. `tell` appends to the session's
    # control file and the session drains it in order, so a sleep in between doesn't
    # "let the input land" — it lets the whole animation finish before the watcher
    # starts, and every observation comes back Stalled on the idle pose.
    "$BIN" ssf2 tell "seq $i:2"     >/dev/null 2>&1
    "$BIN" ssf2 tell "await p0 120" >/dev/null 2>&1
    # only wait for the watch itself to report
    local waited=0
    until [ "$(grep -c 'AWAIT:' "$OUT/ssf2.log")" -ge "$((++seen))" ] || [ $waited -ge 20 ]; do
      sleep 1; waited=$((waited+1))
    done
  done
  "$BIN" ssf2 tell "exit" >/dev/null 2>&1; sleep 2
  kill $sess 2>/dev/null
}

# ── Fraymakers side ──────────────────────────────────────────────────────────
fray_side() {
  local cmds=("spawn $CHAR,$CHAR thespire commandervideoassist" "awaitmatch")
  local i
  for i in "${INPUTS[@]}"; do
    cmds+=("scenario 0,73 140,73" "MARK $i" "seq $i:2" "await p0 120")
  done
  FRAY_CHAR="$CHAR" tools/runseq.sh 0.4 "${cmds[@]}" > "$OUT/fray.log" 2>&1
}

# Pull (input, animation, frames, distance) out of a log. `MARK <input>` is echoed back
# as an unknown command, which is exactly what makes it a usable separator.
extract() {
  awk -v engine="$1" '
    /MARK / { split($0, a, "MARK "); mark=a[2]; gsub(/[^a-z_]/, "", mark); next }
    /AWAIT:/ {
      anim=""; frames=""; x="";
      for (i=1; i<=NF; i++) {
        if ($i ~ /^anim=/)        { split($i,b,"="); anim=b[2] }
        if ($i ~ /^frames_seen=/) { split($i,b,"="); frames=b[2] }
        if ($i ~ /^pos=/)         { split($i,b,"[(,]"); x=b[2] }
      }
      if (mark != "") printf "%s\t%s\t%s\t%s\t%s\n", engine, mark, anim, frames, x
      mark=""
    }' "$2"
}

echo "[compare] $CHAR: ${INPUTS[*]}" >&2
fray_side
ssf2_side || echo "[compare] SSF2 side unavailable — Fraymakers results only" >&2

extract fray "$OUT/fray.log" > "$OUT/fray.tsv"
extract ssf2 "$OUT/ssf2.log" > "$OUT/ssf2.tsv" 2>/dev/null || : > "$OUT/ssf2.tsv"

printf '%-10s  %-24s  %-24s  %s\n' "input" "SSF2 anim (frames*)" "Fraymakers anim (frames)" "verdict"
printf '%-10s  %-24s  %-24s  %s\n' "-----" "-------------------" "------------------------" "-------"
for i in "${INPUTS[@]}"; do
  sa=$(awk -F'\t' -v k="$i" '$2==k {print $3; exit}' "$OUT/ssf2.tsv")
  sf=$(awk -F'\t' -v k="$i" '$2==k {print $4; exit}' "$OUT/ssf2.tsv")
  fa=$(awk -F'\t' -v k="$i" '$2==k {print $3; exit}' "$OUT/fray.tsv")
  ff=$(awk -F'\t' -v k="$i" '$2==k {print $4; exit}' "$OUT/fray.tsv")
  verdict="-"
  if [ -z "${sa:-}" ]; then verdict="SSF2 no observation"
  elif [ -z "${fa:-}" ]; then verdict="FRAYMAKERS DID NOTHING"
  elif [ "${sa}" = "stand" ] || [ "${sa}" = "idle" ]; then verdict="SSF2 didn't leave idle"
  else verdict="both moved"
  fi
  printf '%-10s  %-24s  %-24s  %s\n' \
    "$i" "${sa:-(none)} (${sf:-?})" "${fa:-(none)} (${ff:-?})" "$verdict"
done
echo
echo "* SSF2 frame counts are NOT reliable (sampled ~every 14 SSF2 frames over file-IPC);"
echo "  read them as did-it-play, not as counts. Animation names are reliable on both."
echo "raw: $OUT/ssf2.tsv  $OUT/fray.tsv  (logs alongside)"
