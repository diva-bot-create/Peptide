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
  tools/tests/move_sweep.py fm            # sweep the running Fraymakers session
  tools/tests/move_sweep.py ssf2          # sweep the running SSF2 session
  tools/tests/move_sweep.py compare       # compare the two saved sweeps
  tools/tests/move_sweep.py fm --only jab,aerial_forward

Each engine's sweep is saved to /tmp/peptide_sweep_<engine>.json so the two halves can be run
against separate sessions and compared afterwards.
"""
import json, os, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "build", "release", "peptide")

# ── the fixture, in each engine's own units ──────────────────────────────────
# The source geometry is authored once (test_fixture.rs) and the converter scales it, so these
# are the same surfaces expressed twice rather than two independent sets of numbers.
SCALE = 1.3                 # size_multiplier from mappings/character/stats.jsonc
FLOOR_TOP_SRC = 400.0       # fixture FLOOR_Y
AIR_CLEARANCE_SRC = 1200.0  # drop height for aerials, in source units

ENGINES = {
    # name:      (tell prefix,             floor top,                 clearance)
    "fm":   (["session"], FLOOR_TOP_SRC * SCALE, AIR_CLEARANCE_SRC * SCALE),
    "ssf2": (["ssf2"],    FLOOR_TOP_SRC,         AIR_CLEARANCE_SRC),
}

# ── the moves ────────────────────────────────────────────────────────────────
# (name, kind, input timeline). `kind` decides where the character is parked; the timeline is
# the same `<controls>:<frames>` vocabulary `seq` takes, and controls combine with `+`.
MOVES = [
    # grounded attacks
    ("jab",              "ground", "attack:2"),
    ("tilt_forward",     "ground", "right+attack:2"),
    ("tilt_up",          "ground", "up+attack:2"),
    ("tilt_down",        "ground", "down+attack:2"),
    ("dash_attack",      "ground", "dash+right:8 right+attack:2"),
    ("strong_forward",   "ground", "right:1 right+attack:3"),
    ("strong_up",        "ground", "up:1 up+attack:3"),
    ("strong_down",      "ground", "down:1 down+attack:3"),
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
    ("dash",             "ground", "dash+right:12"),
    ("run",              "ground", "dash+right:40"),
    ("jump",             "ground", "jump:4"),
    ("fall",             "air",    ""),
    ("shield",           "ground", "shield:6"),
    ("roll_forward",     "ground", "shield:1 shield+right:3"),
    ("spot_dodge",       "ground", "shield:1 shield+down:3"),
    ("crouch",           "ground", "down:8"),
    ("stand_turn",       "ground", "left:4"),
]

# How long to watch. A move that has not resolved in this many engine frames is reported as
# unfinished rather than silently truncated to whatever was captured.
WATCH_FRAMES = {"fm": 180, "ssf2": 90}


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


def run_move(engine, name, kind, timeline):
    """Park the character, play the move, and return the captured trace."""
    _, floor_top, clearance = ENGINES[engine]
    y = floor_top if kind == "ground" else floor_top - clearance
    # p1 is parked well away on the same floor so it can neither be hit nor wander into frame.
    away = 400.0 * (SCALE if engine == "fm" else 1.0)
    before = len(read_log(engine))

    tell(engine, "record")
    scenario = f"scenario 0,{y:.0f} {away:.0f},{floor_top:.0f}"
    if timeline:
        scenario += " " + timeline
    tell(engine, scenario)
    tell(engine, f"await {WATCH_FRAMES[engine]}")
    time.sleep(WATCH_FRAMES[engine] / 60.0 + 2.0)
    tell(engine, "trace")
    time.sleep(2.0)

    fresh = read_log(engine)[before:]
    return parse_trace(fresh)


def parse_trace(text):
    """Pull the per-animation frame counts and the position track out of a trace reply.

    Two things come back: `TRACE:<n> frames, <k> animations` followed by `<anim> x<count>`
    lines, and the per-frame `E:<anim>|<frame>|<total>|<x>|<y>|<STATE>` telemetry the engine
    pushes while the window is open. The counts give duration; the telemetry gives travel.
    """
    anims = re.findall(r"^\s*<<\s+(\S+) x(\d+)\s*$", text, re.M)
    track = [
        (m.group(1), int(m.group(2)), float(m.group(4)), float(m.group(5)))
        for m in re.finditer(r"E:([A-Za-z0-9_]+)\|(\d+)\|(-?\d+)\|(-?[\d.]+)\|(-?[\d.]+)", text)
    ]
    return {
        "animations": [(a, int(n)) for a, n in anims],
        "track": track,
    }


def summarize(name, kind, cap):
    """Reduce a capture to the numbers worth comparing."""
    anims = cap["animations"]
    track = cap["track"]
    # The move is whatever ran between the neutral states either side of it. `stand` (and its
    # airborne equivalent) bookend every capture because the scenario resets to neutral first.
    NEUTRAL = {"stand", "fall_loop", "fall", "idle", "stand_loop"}
    core = [(a, n) for a, n in anims if a.lower() not in NEUTRAL]
    frames = sum(n for _, n in core)
    xs = [x for _, _, x, _ in track]
    ys = [y for _, _, _, y in track]
    landed = any(a.lower().startswith(("land", "aerial_")) and a.lower().endswith("_land")
                 for a, _ in anims)
    return {
        "move": name,
        "kind": kind,
        "animations": [a for a, _ in core],
        "frames": frames,
        "travel_x": (max(xs) - min(xs)) if xs else 0.0,
        "travel_y": (max(ys) - min(ys)) if ys else 0.0,
        "landed_early": kind == "air" and landed,
        "resolved": bool(core),
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
        if row["landed_early"]:
            flag = "  LANDED EARLY"
        print(f"  {name:22} {row['frames']:>4} frames  dx={row['travel_x']:>7.1f}"
              f"  dy={row['travel_y']:>7.1f}  {','.join(row['animations'][:3])}{flag}")
    path = f"/tmp/peptide_sweep_{engine}.json"
    with open(path, "w") as f:
        json.dump(out, f, indent=1)
    print(f"\nwrote {path}")
    return out


def compare():
    """Put the two sweeps side by side and judge them against the conversion's own rules."""
    try:
        fm = {r["move"]: r for r in json.load(open("/tmp/peptide_sweep_fm.json"))}
        ss = {r["move"]: r for r in json.load(open("/tmp/peptide_sweep_ssf2.json"))}
    except FileNotFoundError as e:
        print(f"missing a sweep: {e}. run both engines first.")
        return 1

    print(f"{'move':22} {'ssf2':>6} {'fm':>6} {'x2?':>7}   {'dx ssf2':>8} {'dx fm':>8} {'x1.3?':>7}")
    print("-" * 78)
    bad = 0
    for name, _, _ in MOVES:
        a, b = ss.get(name), fm.get(name)
        if not a or not b:
            continue
        dur = f"{b['frames']/a['frames']:.2f}" if a["frames"] else "-"
        trav = f"{b['travel_x']/a['travel_x']:.2f}" if a["travel_x"] > 1.0 else "-"
        durbad = a["frames"] and abs(b["frames"] / a["frames"] - 2.0) > 0.12
        travbad = a["travel_x"] > 1.0 and abs(b["travel_x"] / a["travel_x"] - SCALE) > 0.15
        mark = ""
        if durbad or travbad or b["landed_early"] or a["landed_early"]:
            mark = "  <-"
            bad += 1
        print(f"{name:22} {a['frames']:>6} {b['frames']:>6} {dur:>7}   "
              f"{a['travel_x']:>8.1f} {b['travel_x']:>8.1f} {trav:>7}{mark}")
    print(f"\n{bad} move(s) outside tolerance (duration x2 +/-6%, travel x{SCALE} +/-12%)")
    return 1 if bad else 0


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("fm", "ssf2", "compare"):
        print(__doc__)
        return 2
    mode = sys.argv[1]
    if mode == "compare":
        return compare()
    only = None
    if "--only" in sys.argv:
        only = set(sys.argv[sys.argv.index("--only") + 1].split(","))
    print(f"sweeping {mode} on the fixture ({len(MOVES) if not only else len(only)} moves)\n")
    sweep(mode, only)
    return 0


if __name__ == "__main__":
    sys.exit(main())
