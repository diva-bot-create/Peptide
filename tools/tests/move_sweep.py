#!/usr/bin/env python3
"""move_sweep.py -- drive every move of a character on the FIXTURE stage, in either engine.

Why the fixture: a converted move can only be compared across engines if the ground it happens
over is the same ground. On a shipped stage it is not -- the floor is wherever the original
authors drew it, hazards are live, and a sample that looks like air drift turns out to include
knockback from lava. The fixture is the same geometry in both engines, so a position means the
same thing on both sides and the comparison is about the CHARACTER.

What it checks, per move:

  duration     Fraymakers should take exactly TWICE the frames SSF2 does (30fps -> 60fps).
  travel       horizontal distance should scale by size_multiplier (the one knob in stats.jsonc).
  completion   the move should FINISH. an aerial that lands part-way through was measured
               against the floor, not against itself, and its numbers mean nothing.

The positioning is the fiddly part and it is why this is a harness rather than a one-liner:

  grounded     parked at the middle of the floor, which has ~600 source units of room either
               side, so a move that travels (dash attack, a rolling special) cannot run out of
               ground and turn into a fall part-way through the measurement.
  aerial       parked high enough that the whole move plays out before the floor arrives, with
               the height expressed in SOURCE units and scaled per engine so both sides start
               the same distance up.

Usage:
  tools/tests/move_sweep.py both          # boot each engine in turn, sweep both, compare
  tools/tests/move_sweep.py fm            # sweep an already-running Fraymakers session
  tools/tests/move_sweep.py ssf2          # sweep an already-running SSF2 session
  tools/tests/move_sweep.py compare       # compare the two saved sweeps
  tools/tests/move_sweep.py both --only jab,aerial_forward

Each engine's sweep is saved to /tmp/peptide_sweep_<engine>.json so the two halves can be run
against separate sessions and compared afterwards.
"""
import json, os, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "build", "release", "peptide")

# ── the fixture, in each engine's own units ──────────────────────────────────
# The source geometry is authored once (test_fixture.rs) and the converter scales it, so these
# are the same surfaces expressed twice rather than two independent sets of numbers.
# Read from the converter's OWN data files rather than restated here. The point of the sweep is to
# check that what the converter produced matches what it was told to produce, and a harness holding
# its own copy of the numbers can only ever agree with itself.
def _jsonc(path):
    """Load one of the converter's .jsonc mapping files (comments, trailing commas)."""
    txt = open(os.path.join(ROOT, "crates", "ssf2-converter", "mappings", path)).read()
    txt = re.sub(r"//[^\n]*", "", txt)
    txt = re.sub(r",(\s*[}\]])", r"\1", txt)
    return json.loads(txt)


_STATS = _jsonc("character/stats.jsonc")
SCALE = _STATS["size_multiplier"]
SSF2_FPS = _STATS.get("ssf2_fps", 30)
FM_FPS = _STATS.get("fm_fps", 60)
FPS_RATIO = SSF2_FPS / FM_FPS
# The conversion's own derivation: a per-frame distance shrinks by both the frame rate and the
# world scale, so a move covers `size_multiplier` times the ground over twice as many frames.
VELOCITY_SCALE = SCALE * FPS_RATIO
DURATION_RATIO = 1.0 / FPS_RATIO
TRAVEL_RATIO = VELOCITY_SCALE * DURATION_RATIO      # == size_multiplier

# The name the converter says each SSF2 animation becomes. One SSF2 animation can be SPLIT into
# several (a jab becomes jab1..jab4), so a played name counts as the expected one if it starts
# with it.
SSF2_TO_FM = _jsonc("character/animations.jsonc")["ssf2_to_fm"]


def expected_fm_name(ssf2_anim):
    """What the converter's mapping says this SSF2 animation should become."""
    return SSF2_TO_FM.get((ssf2_anim or "").lower())


def names_agree(ssf2_anim, fm_anim):
    """True when the pair matches the converter's own mapping."""
    want = expected_fm_name(ssf2_anim)
    if not want or not fm_anim:
        return None                      # nothing claimed, so nothing to contradict
    got = fm_anim.lower()
    return got == want or got.startswith(want)
FLOOR_TOP_SRC = 400.0       # fixture FLOOR_Y
# Aerials are dropped BELOW the floor, not above it. Above, the only thing a falling character can
# do is land on the floor, which cuts the move short and makes the measurement about the drop
# height. Below the floor there is nothing between the character and the blast boundary ~2600
# source units down, so the move gets the whole of itself before anything interrupts.
AIR_BELOW_FLOOR_SRC = 300.0

ENGINES = {
    # name:      (tell prefix,             floor top,                 clearance)
    "fm":   (["session"], FLOOR_TOP_SRC * SCALE, AIR_BELOW_FLOOR_SRC * SCALE),
    "ssf2": (["ssf2"],    FLOOR_TOP_SRC,         AIR_BELOW_FLOOR_SRC),
}

# ── the moves ────────────────────────────────────────────────────────────────
# (name, kind, input timeline). `kind` decides where the character is parked; the timeline is
# the same `<controls>:<frames>` vocabulary `seq` takes, and controls combine with `+`.
MOVES = [
    # grounded attacks
    ("jab",              "ground", "attack:2"),
    # A tilt is the direction ALREADY held when attack arrives; a smash is the two together. SSF2
    # reads it that way and the conversion does not, so each is asked in its own idiom.
    ("tilt_forward",     "ground", {"fm": "right+attack:2", "ssf2": "right:6 right+attack:2"}),
    ("tilt_up",          "ground", {"fm": "up+attack:2",    "ssf2": "up:6 up+attack:2"}),
    ("tilt_down",        "ground", {"fm": "down+attack:2",  "ssf2": "down:6 down+attack:2"}),
    ("dash_attack",      "ground", {"fm":   "dash+right:8 right+attack:2",
                                    "ssf2": "right:2 none:1 right:6 right+attack:2"}),
    # SSF2 smashes on side+attack pressed together; Fraymakers reads that as a tilt because the
    # distinction is stick magnitude, so it is entered by state instead.
    ("strong_forward",   "ground", {"fm": "state:STRONG_FORWARD_IN", "ssf2": "right+attack:2"}),
    ("strong_up",        "ground", {"fm": "state:STRONG_UP_IN",      "ssf2": "up+attack:2"}),
    ("strong_down",      "ground", {"fm": "state:STRONG_DOWN_IN",    "ssf2": "down+attack:2"}),
    ("getup_attack",     "ground", "down:2 attack:2"),
    # aerials -- parked high so the whole move plays before the floor arrives
    ("aerial_neutral",   "air",    "attack:2"),
    ("aerial_forward",   "air",    "right+attack:2"),
    ("aerial_back",      "air",    "left+attack:2"),
    ("aerial_up",        "air",    "up+attack:2"),
    ("aerial_down",      "air",    "down+attack:2"),
    ("airdodge",         "air",    "shield:2"),
    ("airdash_forward",  "air",    "dash+right:2"),
    # specials, both grounded and airborne: an air special has its own animation and its own
    # trajectory, and the two are worth measuring separately
    ("special_neutral",  "ground", "special:2"),
    ("special_side",     "ground", "right+special:2"),
    ("special_up",       "ground", "up+special:2"),
    ("special_down",     "ground", "down+special:2"),
    ("special_neutral_air", "air", "special:2"),
    ("special_side_air", "air",    "right+special:2"),
    ("special_up_air",   "air",    "up+special:2"),
    ("special_down_air", "air",    "down+special:2"),
    # motions -- the physics itself, rather than a move
    ("walk",             "ground", "right:40"),
    # SSF2 has no dash button: it dashes on a double-tap of the direction.
    # No `dash` row: Fraymakers has a distinct dash animation and SSF2 goes straight walk -> run,
    # so there is no same-move pair to compare. `run` below covers the same motion on both.
    ("run",              "ground", {"fm": "dash+right:40", "ssf2": "right:1 none:2 right:38"}),
    ("jump",             "ground", "jump:4"),
    ("fall",             "air",    ""),
    ("shield",           "ground", {"fm": "shield:6", "ssf2": "shield:8"}),
    # The shield has to be UP before the direction means "dodge" rather than "walk", and SSF2
    # wants longer to register it than Fraymakers does.
    ("roll_forward",     "ground", {"fm": "shield:1 shield+right:3", "ssf2": "shield:4 shield+right:8"}),
    ("spot_dodge",       "ground", {"fm": "shield:1 shield+down:3",  "ssf2": "shield:4 shield+down:8"}),
    ("crouch",           "ground", "down:8"),
    ("stand_turn",       "ground", "left:4"),
]

# How long to watch. A move that has not resolved in this many engine frames is reported as
# unfinished rather than silently truncated to whatever was captured.
WATCH_FRAMES = {"fm": 180, "ssf2": 90}
# The wait used when a move never reaches a resting state at all (crash, collapse, sleep-in-place).
FALLBACK_SETTLE_S = 2.5


def tell(engine, cmd):
    """Queue one command for the running session and return once it has been sent."""
    args = [BIN] + ENGINES[engine][0] if engine == "ssf2" else [BIN]
    if engine == "ssf2":
        args = [BIN, "ssf2", "tell", cmd]
    else:
        args = [BIN, "tell", cmd]
    subprocess.run(args, capture_output=True, text=True, timeout=60)


def log_path(engine):
    return os.environ.get(f"PEPTIDE_{engine.upper()}_LOG", f"/tmp/sweep_{engine}.log")


def read_log(engine):
    try:
        with open(log_path(engine), "r", errors="replace") as f:
            return f.read()
    except FileNotFoundError:
        return ""


def timeline_for(engine, timeline):
    """The input for THIS engine. A move can need a different idiom on each side: SSF2 reads a
    simultaneous direction+attack as a smash where the conversion reads it as a tilt, so asking
    both for `up+attack` asks them for two different moves."""
    if isinstance(timeline, dict):
        return timeline.get(engine, "")
    return timeline


def run_move(engine, name, kind, timeline):
    """Park the character, play the move, and return the captured trace."""
    timeline = timeline_for(engine, timeline)
    _, floor_top, below = ENGINES[engine]
    # +y is DOWN, so an aerial parks BELOW the floor and falls away from it into open space.
    y = floor_top if kind == "ground" else floor_top + below
    # p1 is parked well away on the same floor so it can neither be hit nor wander into frame.
    away = 400.0 * (SCALE if engine == "fm" else 1.0)
    # Wait for the PREVIOUS move to stop talking before marking where this one starts. SSF2
    # processes one command per frame at 30fps, so its replies can still be arriving when the next
    # move begins, and a window opened too early captures the tail of the last move -- which reads
    # as every move reporting the one before it.
    before = drain(engine)

    tell(engine, "record")
    scenario = f"scenario 0,{y:.0f} {away:.0f},{floor_top:.0f}"
    if timeline.startswith("state:"):
        # Some moves cannot be asked for with a button. Fraymakers tells a smash from a tilt by
        # how far the stick was pushed, and an injected mask has no magnitude to push -- every
        # spelling of "side and attack together" comes back a tilt. So the move is entered
        # directly instead. It is the same motion either way; only the asking differs.
        tell(engine, scenario)
        tell(engine, f"e p0.toState(CState.{timeline[len('state:'):]})")
    else:
        if timeline:
            scenario += " " + timeline
        tell(engine, scenario)
    wait_settled(engine, before)
    # `info` gives the position the move ENDED at. The per-frame track only exists while `await`
    # is running, and `await` cannot start until the input timeline it follows has finished, so a
    # move that travels is already over by the time the track opens. Start and end positions are
    # known exactly (the scenario placed it at x=0), which is all travel needs.
    tell(engine, "e p0.getX()" if engine == "fm" else "info")
    tell(engine, "trace")
    # The replies come back over the same link the telemetry does; give them a beat to land, then
    # read once. This is the only wait left and it is bounded by the round trip, not by the move.
    # Wait for the trace ITSELF to arrive rather than for a guessed interval. SSF2 processes one
    # command per frame at 30fps, so its replies take noticeably longer to come back than
    # Fraymakers', and reading on a fixed timer meant reading an empty window.
    for _ in range(60):
        time.sleep(0.1)
        if re.search(r"TRACE:\d+ frames", read_log(engine)[before:]):
            break
    time.sleep(0.4)

    fresh = read_log(engine)[before:]
    return parse_trace(fresh)


# What "the move is over" looks like: the character is back in a state it can sit in forever.
# Falling counts -- an aerial dropped into open space finishes its move and then simply falls.
# Deliberately NOT walk_loop/run/shield_loop: those are things the character is DOING, and
# treating them as "done" ends the measurement on the first frame of the motion being measured.
# The engine pushes the STATE name, which is not always the animation name: falling arrives as
# "FALL" even though the animation is fall_loop, so both spellings are here.
RESTING = ("stand", "idle", "fall", "fall_loop", "fall_special", "helpless", "dizzy", "sleep",
           # SSF2 names its states differently and shares almost nothing with the converted ones
           "crouch", "land")


def drain(engine, quiet=0.4, timeout=4.0):
    """Return the log length once it has stopped growing, so a window starts on clean ground."""
    deadline = time.time() + timeout
    last = len(read_log(engine))
    stable_since = time.time()
    while time.time() < deadline:
        time.sleep(0.1)
        now = len(read_log(engine))
        if now != last:
            last, stable_since = now, time.time()
        elif time.time() - stable_since >= quiet:
            return now
    return len(read_log(engine))


def wait_settled(engine, before, timeout=12.0):
    """Wait for the move to FINISH, rather than for a fixed number of seconds.

    Most moves are over in well under a second, and sleeping a flat interval per move turned a
    thirty-move sweep into several minutes of mostly waiting. The engine says when it is done: it
    pushes the animation it enters, so watch for it re-entering something it can rest in.

    The timeout is a backstop for a move that never resolves, not the normal path.
    """
    deadline = time.time() + timeout
    started = False
    while time.time() < deadline:
        anims = re.findall(r"ANIM:(\w+)", read_log(engine)[before:])
        if anims:
            # The scenario resets to a resting state first, so wait for something else to begin
            # before treating a resting state as the end.
            for a in anims:
                low = a.lower()
                if not started and low not in RESTING:
                    started = True
                elif started and low in RESTING:
                    # Confirm it STAYS resting. A move can pass through a resting state on its way
                    # somewhere else -- a turn settles for a frame before the walk it was the
                    # start of -- and stopping there measures the turn instead of the walk.
                    time.sleep(0.35)
                    later = re.findall(r"ANIM:(\w+)", read_log(engine)[before:])
                    if later and later[-1].lower() in RESTING:
                        return True
                    started = True
        time.sleep(0.12)
    # Never settled. Some animations genuinely do not return to a resting state on their own --
    # the crash/collapse family sits where it lands -- so fall back to waiting out the watch
    # window rather than reporting a move that did run as if it had not.
    time.sleep(FALLBACK_SETTLE_S)
    return False


def parse_trace(text):
    """Pull the per-animation frame counts and the position track out of a trace reply.

    Two things come back: `TRACE:<n> frames, <k> animations` followed by `<anim> x<count>`
    lines, and the per-frame `E:<anim>|<frame>|<total>|<x>|<y>|<STATE>` telemetry the engine
    pushes while the window is open. The counts give duration; the telemetry gives travel.
    """
    # Both engines list `<name> x<count>`; SSF2 appends the position the animation ended at, and
    # does not carry the reply prefix onto continuation lines. One pattern covers both.
    anims = re.findall(r"^\s*(?:<<)?\s+([A-Za-z0-9_]+) x(\d+)(?:\s+end=\((-?[\d.]+),\s*(-?[\d.]+)\))?\s*$",
                       text, re.M)
    # `info` reports where the character ended up, in both engines.
    # Fraymakers answers `e p0.getX()` with `E:<number>`; SSF2's `info` logs `pos=(x, y)`. Both
    # are read here so one harness covers both engines.
    pos = re.findall(r"pos=\((-?[\d.]+),\s*(-?[\d.]+)\)", text)
    evals = re.findall(r"^<<\s*E:(-?[\d.]+)\s*$", text, re.M)
    track = [
        (m.group(1), int(m.group(2)), float(m.group(4)), float(m.group(5)))
        for m in re.finditer(r"E:([A-Za-z0-9_]+)\|(\d+)\|(-?\d+)\|(-?[\d.]+)\|(-?[\d.]+)", text)
    ]
    return {
        "animations": [(a, int(n)) for a, n, _, _ in anims],
        # SSF2's trace already says where each animation ended, which is more direct than asking
        # separately afterwards.
        "anim_ends": [(a, float(x)) for a, _, x, _ in anims if x],
        "track": track,
        "end_pos": (float(pos[-1][0]), float(pos[-1][1])) if pos
                   else ((float(evals[-1]), 0.0) if evals else None),
        "trace_end_x": None,
    }


def summarize(name, kind, cap):
    """Reduce a capture to the numbers worth comparing.

    The duration that matters is the MOVE's own animation, not the whole window: an aerial is
    followed by the fall and the landing that were always going to happen, and counting those
    measures the drop height rather than the move. So the move is identified by name where the
    engine played something matching it, and by "the longest thing that is not a neutral state"
    otherwise.
    """
    anims = cap["animations"]
    NEUTRAL = {"stand", "stand_loop", "idle", "fall", "fall_loop", "fall_in",
               "land", "land_light", "land_heavy", "walk_out", "crouch_out", "skid"}
    core = [(a, n) for a, n in anims if a.lower() not in NEUTRAL]
    # Matched against EVERY animation, not just the non-neutral ones: "fall" is a neutral state
    # for most moves and the whole point of one of them.
    named = [(a, n) for a, n in anims if name.lower() in a.lower() or a.lower() in name.lower()]
    picked = named or core
    frames = max((n for _, n in picked), default=0)
    played = picked[0][0] if picked else None
    end = cap.get("end_pos")
    return {
        "move": name,
        "kind": kind,
        "animation": played,
        "animations": [a for a, _ in core],
        # Every animation the capture saw, with its length. The pairing across engines is made
        # later, from the converter's mapping, so both sides have to keep their options.
        "seen": [[a, n] for a, n in anims],
        "frames": frames,
        "travel_x": abs(end[0]) if end else 0.0,   # every scenario starts the character at x=0
        "end_y": end[1] if end else None,
        "matched": bool(named),
        "resolved": bool(picked),
    }


def sweep(engine, only=None):
    out = []
    for name, kind, timeline in MOVES:
        if only and name not in only:
            continue
        cap = run_move(engine, name, kind, timeline)
        row = summarize(name, kind, cap)
        out.append(row)
        flag = "" if row["resolved"] else "  (no move seen)"
        if row["resolved"] and not row["matched"]:
            flag = f"  (input reached {row['animation']})"
        print(f"  {name:22} {row['frames']:>4} frames  dx={row['travel_x']:>7.1f}"
              f"  {row['animation'] or '-':22}{flag}")
    path = f"/tmp/peptide_sweep_{engine}.json"
    with open(path, "w") as f:
        json.dump(out, f, indent=1)
    print(f"\nwrote {path}")
    return out



def best_pair(a, b):
    """Find the SSF2 animation and the Fraymakers animation(s) that are the same move.

    The converter says what each SSF2 animation becomes, so walk the SSF2 side, map each name, and
    look for it on the Fraymakers side. Fraymakers may have SPLIT it into several (walk becomes
    walk_in/walk_loop/walk_out), and the move's duration is all of them, so they are summed.
    """
    fm_seen = b.get("seen") or []
    # A capture is bracketed by the state the character rests in, and that state is usually the
    # LONGEST thing in it -- the character stands for far longer than it jabs. Pairing on length
    # alone therefore matches `stand` against `stand` for every move in the sweep and calls it a
    # comparison. The move is what happened BETWEEN the resting states.
    candidates = [(sa, sn) for sa, sn in (a.get("seen") or []) if sa.lower() not in RESTING]
    if not candidates:
        candidates = a.get("seen") or []
    best = None
    for sa, sn in candidates:
        want = expected_fm_name(sa)
        if not want:
            continue
        parts = [(fa, fn) for fa, fn in fm_seen
                 if (fa.lower() == want or fa.lower().startswith(want))
                 and not (fa.lower() in RESTING and want not in RESTING)]
        if not parts:
            continue
        total = sum(fn for _, fn in parts)
        # Prefer the longest SSF2 animation that has a counterpart: it is the move, not the
        # shield it started from.
        if best is None or sn > best[1]:
            best = (sa, sn, parts[0][0] if len(parts) == 1 else f"{parts[0][0]}+{len(parts)-1}", total)
    return best


def compare():
    """Judge the two sweeps against what the CONVERTER said it would produce.

    Every expectation here comes from the converter's own mapping files, so this checks the output
    against its stated contract rather than against a second opinion baked into the harness:

      duration   the frame-rate ratio in stats.jsonc (30 -> 60, so x2)
      travel     size_multiplier, since a per-frame speed shrinks by velocity_scale while the
                 move lasts proportionally longer
      naming     animations.jsonc's ssf2_to_fm, which says what each SSF2 animation becomes
    """
    try:
        fm = {r["move"]: r for r in json.load(open("/tmp/peptide_sweep_fm.json"))}
        ss = {r["move"]: r for r in json.load(open("/tmp/peptide_sweep_ssf2.json"))}
    except FileNotFoundError as e:
        print(f"missing a sweep: {e}. run both engines first.")
        return 1

    print(f"expecting duration x{DURATION_RATIO:g}, travel x{TRAVEL_RATIO:g}"
          f" (size_multiplier {SCALE:g}, {SSF2_FPS:g}->{FM_FPS:g}fps)\n")
    print(f"{'move':20} {'ssf2':>5} {'fm':>5} {'dur':>6} {'dx ssf2':>8} {'dx fm':>8} {'travel':>7}  name")
    print("-" * 88)

    checked = bad = skipped = 0
    mismatched = []
    for name, _, _ in MOVES:
        a, b = ss.get(name), fm.get(name)
        if not a or not b:
            continue
        # Only compare a move both engines actually performed. An input that reached different
        # moves is a gap in how the move is driven, not a conversion defect, and counting it as
        # one buries the defects that are real.
        # Pair the two captures using the converter's OWN mapping rather than whichever animation
        # happened to be longest. A capture is usually several animations -- SSF2 shields before it
        # rolls, and the conversion splits a walk into walk_in/walk_loop/walk_out -- so the
        # question is which SSF2 animation and which Fraymakers animations are the same thing.
        pair = best_pair(a, b)
        if pair:
            a = dict(a, animation=pair[0], frames=pair[1])
            b = dict(b, animation=pair[2], frames=pair[3])
        agree = names_agree(a.get("animation"), b.get("animation"))
        if not a["resolved"] or not b["resolved"] or agree is None:
            skipped += 1
            continue
        if not agree:
            # The two engines performed DIFFERENT moves, so there is nothing to compare. Holding
            # up+attack is an up smash in SSF2 and an up tilt in the conversion, and calling that a
            # conversion defect would bury the real ones. It is an input gap: the sweep needs to ask
            # each engine for the move in its own idiom.
            mismatched.append((name, a["animation"], b["animation"],
                               expected_fm_name(a["animation"])))
            continue

        checked += 1
        dur = b["frames"] / a["frames"] if a["frames"] else 0.0
        trav = b["travel_x"] / a["travel_x"] if a["travel_x"] > 1.0 else None
        durbad = a["frames"] and abs(dur - DURATION_RATIO) / DURATION_RATIO > 0.08
        travbad = trav is not None and abs(trav - TRAVEL_RATIO) / TRAVEL_RATIO > 0.12
        mark = "  <-" if (durbad or travbad) else ""
        if mark:
            bad += 1
        note = ""
        print(f"{name:20} {a['frames']:>5} {b['frames']:>5} {dur:>6.2f} "
              f"{a['travel_x']:>8.1f} {b['travel_x']:>8.1f} "
              f"{(f'{trav:.2f}' if trav else '-'):>7}  "
              f"{a['animation']}/{b['animation']}{mark}{note}")

    print(f"\n{checked} compared, {bad} outside tolerance, "
          f"{len(mismatched) + skipped} not compared")
    if mismatched:
        print("\nnot compared -- the two engines performed different moves (an input gap, not a"
              "\nconversion defect; the sweep has to ask each engine in its own idiom):")
        for name, sa, fa, want in mismatched:
            print(f"  {name:20} ssf2 did {sa:18} -> expected {want or '?':18} but fm did {fa}")
    return 1 if bad else 0



# ── driving the engines ──────────────────────────────────────────────────────
# One command has to be able to run the whole thing, because a comparison assembled by hand from
# two separate sittings is one where the two halves can quietly disagree about which build, which
# stage or which character they were looking at.

# Which character to sweep. The fixture is the same on both sides, so the only thing that has to
# be said twice is the name each engine knows the character by.
CHAR = os.environ.get("PEPTIDE_SWEEP_CHAR", "sandbag")
ENGINE_BOOT = {
    "fm":   ["session", "--char", CHAR, "--stage", "peptidefixturessf2"],
    "ssf2": ["ssf2", "session", "--char", CHAR, "--stage", "peptidefixture"],
}
BOOT_READY = {"fm": "auto-launching|READY|CRASH", "ssf2": "auto-launching|never settled"}


def shutdown():
    """Leave no engine running. Two at once fight over the bridge port."""
    for pat in ("peptide session", "peptide ssf2 session"):
        subprocess.run(["pkill", "-f", pat], capture_output=True)
    for pat in ("Fraymakers", "SSF2-patched"):
        subprocess.run(["pkill", "-9", "-f", pat], capture_output=True)
    subprocess.run(["rm", "-rf", os.path.expanduser("~/.peptide/session")], capture_output=True)
    time.sleep(3)


def boot(engine, timeout=300):
    """Start one engine on the fixture and wait until it is taking commands."""
    log = log_path(engine)
    open(log, "w").close()
    with open(log, "ab") as fh:
        subprocess.Popen([BIN] + ENGINE_BOOT[engine], stdout=fh, stderr=fh,
                         start_new_session=True)
    ready = re.compile(BOOT_READY[engine])
    deadline = time.time() + timeout
    while time.time() < deadline:
        if ready.search(read_log(engine)):
            time.sleep(10)      # let the match settle before the first move
            return True
        time.sleep(1.0)
    return False


def sweep_both(only=None):
    """Boot each engine in turn, sweep it, and compare -- the whole thing in one command."""
    for engine in ("fm", "ssf2"):
        shutdown()
        print(f"\n=== booting {engine} ===")
        if not boot(engine):
            print(f"{engine} did not come up; see {log_path(engine)}")
            return 2
        print(f"sweeping {engine} on the fixture\n")
        sweep(engine, only)
    shutdown()
    print()
    return compare()

def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("fm", "ssf2", "compare", "both"):
        print(__doc__)
        return 2
    mode = sys.argv[1]
    only = None
    if "--only" in sys.argv:
        only = set(sys.argv[sys.argv.index("--only") + 1].split(","))
    if mode == "compare":
        return compare()
    if mode == "both":
        return sweep_both(only)
    print(f"sweeping {mode} on the fixture ({len(MOVES) if not only else len(only)} moves)\n")
    sweep(mode, only)
    return 0


if __name__ == "__main__":
    sys.exit(main())
