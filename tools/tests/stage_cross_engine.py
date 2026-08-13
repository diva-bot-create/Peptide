#!/usr/bin/env python3
"""stage_cross_engine.py — run a stage in BOTH engines and compare what actually animates.

The static inventory proves the right objects were emitted; stage_live_check proves the
Fraymakers ones run. Neither answers the question that decides a port: does the converted
stage animate the SAME WAY the original does?

This runs the stage in SSF2 and in Fraymakers, reads each engine's own animation clock, and
compares the lengths. The conversion doubles every keyframe (SSF2 runs at 30fps, Fraymakers at
60), so the contract is checkable:

    fraymakers_total == 2 x ssf2_playable

where `ssf2_playable` is `totalFrames`, MINUS ONE when the clip's last frame carries a control
action. In SSF2 a frame's code runs BEFORE the frame renders, so a final frame holding
`gotoAndPlay` (or `endAttack`) jumps away without ever being shown — the clip reports N frames
and plays N-1. bowserscastle has both kinds: `bowsers_podoboos_bg` 82 -> 164 and
`bowsers_bubbles_bg` 40 -> 80 double cleanly, while `bowserSpectator` reports 142 and converts
to 282, which is 2x141, not 2x142. The live tree can't see whether that last frame carries
code, so BOTH are accepted and the report says which one matched.

A length outside both is a real defect with a visible symptom: an element whose loop is longer
or shorter than the original plays at the wrong speed or drifts out of phase with the stage.

Pairing is by ANIMATION LENGTH, not by name — the two engines name nothing alike (SSF2
`bowsers_podoboos_bg` vs `bowserscastlessf2_bowsers_podoboos_bg`), and the emitted ids carry a
stage prefix that isn't derivable in reverse. Length is the property under test, so any
element whose length pairs uniquely is checkable; ambiguous ones are reported, not guessed.

Usage:
  tools/tests/stage_cross_engine.py <stage>      # e.g. bowserscastle
"""
import os, re, subprocess, sys, time, collections

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "build", "release", "peptide")
SSF2_LOG, FM_LOG = "/tmp/xeng_ssf2.log", "/tmp/xeng_fm.log"


def boot(engine, stage, log, ready_pat, settle):
    subprocess.run(["pkill", "-f", "peptide ssf2 session"], capture_output=True)
    subprocess.run(["pkill", "-f", "peptide session"], capture_output=True)
    subprocess.run(["pkill", "-f", "SSF2-patched"], capture_output=True)
    time.sleep(3)
    subprocess.run(["rm", "-rf", os.path.expanduser("~/.peptide/session")], capture_output=True)
    f = open(log, "w")
    cmd = [BIN] + (["ssf2"] if engine == "ssf2" else []) + ["session", "--char", "mario", "--stage", stage]
    p = subprocess.Popen(cmd, stdout=f, stderr=f)
    for _ in range(60):
        time.sleep(3)
        if re.search(ready_pat, open(log, encoding="utf8", errors="replace").read()):
            break
    time.sleep(settle)
    return p


def tell(engine, cmd):
    pre = ["ssf2"] if engine == "ssf2" else []
    subprocess.run([BIN] + pre + ["tell", cmd], capture_output=True, text=True)


def await_walk(log, marker, quiet=6.0, cap=420.0, since=0):
    """Wait for a tree walk to FINISH, by watching its output settle.

    A fixed sleep is a guess, and a wrong guess truncates the walk mid-traversal — a deeper
    walk silently lost the thwomp that a shallower one had found, which reads as "the hazard
    isn't ported" rather than "the tool stopped reading".

    Waits for the walk's OWN output (`marker`) rather than for the log to grow: the session
    streams per-frame ANIM telemetry the whole time, so mere growth means nothing, and keying
    on it declares the walk finished before it has produced a single row. SSF2 in particular
    builds the entire tree and returns it in one write, so there is nothing at all until there
    is everything.

    `since` is a byte offset to look for the marker AFTER, for repeated commands: the same
    marker from an earlier round is still sitting in the log and would otherwise satisfy the
    wait immediately.
    """
    def seen():
        return marker in open(log, encoding="utf8", errors="replace").read()[since:]

    last, stable, waited, began = -1, 0.0, 0.0, False
    while waited < cap:
        time.sleep(1.0)
        waited += 1.0
        size = os.path.getsize(log)
        if not began:
            began = seen()
            last = size
            continue
        if size == last:
            stable += 1.0
            if stable >= quiet:
                return True
        else:
            stable = 0.0
        last = size
    return False


# Subtrees that are not stage BACKDROP ART, and so have no vfx counterpart by construction:
#   * the HUD/menu layer -- its own animating clips (scores, music info, sync banner)
#   * the fighters -- a character is its own port with its own timeline (mario reads as a
#     95-frame clip and would otherwise look like a missing 190-frame element)
#   * hazards -- these port to CustomGameObjects, not vfx, so they're a different comparison
# Hazards are detected from the CONVERTED SCRIPT rather than hardcoded: whatever the stage
# spawns via createCustomGameObject is a hazard on this stage.
NON_STAGE_ROOTS = ("menu_hud", "pause_menu", "help_menu", "training_menu",
                   # the KO overlay: parented under the stage, but match chrome rather than
                   # stage art, and it animates (65 frames) so it reads as a missing element
                   "screenkoholder")
CHARACTER_TOKEN = ["mario"]


def _frames_of(text):
    """{name: current frame} from an already-captured walk.

    Keyed by the RAW instance name, and looked up that way too — the first sample keys some
    elements by `parent > child`, and mixing the two schemes silently pairs unrelated objects,
    which is exactly what made an earlier version of the timing check produce nonsense.
    """
    out = {}
    for name, f in re.findall(r'"([^"]+)"\s*@\([^)]*\)[^\n]*?frame=(\d+)/\d+', text):
        out.setdefault(name, int(f))
    return out


def _labels_of(text):
    """{name: set of frame labels seen} from an already-captured walk.

    An SSF2 clip whose code jumps between frame labels is a state machine, not a loop: it only
    ever plays the labelled segments its script selects, never the whole timeline. Seeing more
    than one label on an element is how that shows up from outside, and it's the reason such an
    element's rate can't match a conversion that plays every frame end to end.
    """
    out = {}
    for name, lab in re.findall(
            r'"([^"]+)"\s*@\([^)]*\)[^\n]*?frame=\d+/\d+\s*\'([^\']+)\'', text):
        out.setdefault(name, set()).add(lab)
    return out


def hazard_and_character_names(stage, char):
    """Lowercase tokens whose SSF2 clips port to something other than a vfx."""
    sid = f"{stage}ssf2"
    path = os.path.join(ROOT, "stages", sid, "library", "scripts", "stage", f"{sid}Script.hx")
    out = {char.lower()}
    if os.path.exists(path):
        text = open(path, encoding="utf8").read()
        for ent in re.findall(r'createCustomGameObject\(\s*self\.getResource\(\)\.getContent\("([^"]+)"\)', text):
            out.add(ent.replace(sid, "").lower())
        for ent in re.findall(r'createStructure\(\s*self\.getResource\(\)\.getContent\("([^"]+)"\)', text):
            out.add(ent.replace(sid, "").lower())
    return {t for t in out if t}


def ticks_elapsed(engine, log, seconds):
    """Engine TICKS that pass over `seconds`, from the engine's own per-frame recorder.

    Timing has to be measured in ticks, not wall-clock: the two engines run at different rates
    (SSF2 30, Fraymakers 60) and neither runs at its nominal rate under load, so a seconds-based
    figure says nothing about whether an animation is stepping correctly.
    """
    tell(engine, "record")
    time.sleep(1)
    mark = len(open(log, encoding="utf8", errors="replace").read())
    time.sleep(seconds)
    tell(engine, "trace")
    time.sleep(4)
    tail = open(log, encoding="utf8", errors="replace").read()[mark:]
    m = re.search(r"TRACE:(\d+) frames", tail)
    return int(m.group(1)) if m else 0


def ssf2_animations(stage, excluded):
    """{name: total_frames} for every SSF2 STAGE object with a real timeline (total > 1).

    Walks with indentation so ancestry is known: a clip is stage content only if it is not
    inside one of the HUD/menu subtrees.
    """
    # Wait for a LIVE match, not for a guessed number of seconds. SSF2 decrypts its assets on
    # demand and the time varies; a walk taken too early returns the LOADING SCREEN (a progress
    # bar reads as `instance18 frame=76/100`), which looks like a stage with almost no objects
    # rather than like a mistimed read. The extra settle after that is for the hazards, which
    # the stage spawns once the match is running.
    p = boot("ssf2", stage, SSF2_LOG, r"auto-launching", 0)
    for _ in range(40):
        tell("ssf2", "info")
        time.sleep(3)
        if re.search(r"p0: anim=", open(SSF2_LOG, encoding="utf8", errors="replace").read()):
            break
    time.sleep(25)
    try:
        # depth 4: the per-torch clips live INSIDE `bowsers_torches_lit_bg`, and the
        # converter promotes each child to its own vfx, so a depth-3 walk stops one level
        # above the objects the Fraymakers side actually has.
        tell("ssf2", "tree 4")
        await_walk(SSF2_LOG, "[object ")
        text = open(SSF2_LOG, encoding="utf8", errors="replace").read()
        # second sample, with the tick gap measured between them
        mark2 = len(text)
        ticks = ticks_elapsed("ssf2", SSF2_LOG, 4)
        tell("ssf2", "tree 4")
        await_walk(SSF2_LOG, "[object ")
        text2 = open(SSF2_LOG, encoding="utf8", errors="replace").read()[mark2:]
        # Extra walks purely to observe LABELS. Two samples are enough to measure a rate but not
        # to enumerate the labels a state-machine element visits: catching it mid-`wait` rather
        # than mid-`idle` is chance, and one label looks like an ordinary loop. Each walk is
        # about a second, so a few more samples buy the distinction cheaply.
        # spaced by the engine's OWN frame counter (`await` runs out its budget against a parked
        # character's looping animation), because back-to-back walks span a couple of seconds and
        # an element can sit in one segment far longer than that.
        extra = ""
        for _ in range(4):
            mark = len(open(SSF2_LOG, encoding="utf8", errors="replace").read())
            tell("ssf2", "await p0 150")
            await_walk(SSF2_LOG, "AWAIT:", since=mark)
            mark_tree = len(open(SSF2_LOG, encoding="utf8", errors="replace").read())
            tell("ssf2", "tree 4")
            await_walk(SSF2_LOG, "[object ", since=mark_tree)
            extra += open(SSF2_LOG, encoding="utf8", errors="replace").read()[mark:]
    finally:
        tell("ssf2", "exit")
        time.sleep(2)
        p.terminate()
    out, hazards, skip_depth = {}, {}, None
    frames1 = {}
    worlds = {}
    planes = {}
    # The SSF2 plane an element belongs to, read from its ancestry: these container clips are
    # the source's own depth model, and they're what the converter maps onto a Fraymakers
    # layer. Without this, "is it in the right layer" can only be answered on the FM side.
    PLANES = ("background", "midground", "foreground", "terrain")
    plane_at = {}
    # A deep walk is NOT atomic: it takes about a minute, and the live display list changes
    # under it, so index paths shift and the same object gets visited more than once
    # (`bowsers_podoboos_bg` appeared three times, at 50/82, 6/82 and 64/82). Identity is the
    # instance name plus its position, so a revisit collapses instead of inflating the count.
    seen_ids = set()
    parents = {}          # depth -> the nearest NAMED ancestor at that depth
    lengths_by_depth = {}  # depth -> the clip length recorded there (parent/child dedupe)
    # rows are "[idx]" THEN the depth indent, e.g. `[0]     [object menu_hud] "instance1878" …`
    row = re.compile(r'^(?:\[\d+\]\s?)?( *)\[object ([^\]]+)\]\s*"([^"]+)"\s*@\(([^)]*)\)'
                     r'\s*\S+\s*(?:HIDDEN\s*)?frame=(-?\d+)/(-?\d+)')
    for line in text.splitlines():
        line = line.replace("<< ", "")
        m = row.match(line)
        if not m:
            continue
        indent, cls, name, pos, total = m.group(1), m.group(2), m.group(3), m.group(4), m.group(6)
        depth = len(indent)
        # World position comes from the record's `abs=` — the node's world BOUNDING BOX
        # top-left. A node's own x/y is its registration point, which is not where the art
        # starts, and Fraymakers records a rasterised image's corner; comparing those two
        # produces a mapping that fits no element at all.
        ab = re.search(r"abs=\(([-\d.]+),([-\d.]+)\)", line)
        world = (float(ab.group(1)), float(ab.group(2))) if ab else None
        label = (re.search(r"'([^']+)'\s*$", line) or [None, None])[1] \
            if re.search(r"'([^']+)'\s*$", line) else None
        # remember a meaningful ancestor so an unnamed child can be reported by its parent
        if not name.startswith("instance"):
            parents[depth] = name
        for d in [d for d in plane_at if d >= depth]:
            plane_at.pop(d, None)
        if name.lower() in PLANES:
            plane_at[depth] = name.lower()
        cur_plane = plane_at.get(max(plane_at, default=-1)) if plane_at else None
        for d in [d for d in parents if d > depth]:
            parents.pop(d, None)
        if skip_depth is not None and depth > skip_depth:
            continue           # still inside a non-stage subtree
        skip_depth = None
        # the HUD/menu layer is identified by CLASS (its instances are named `instanceNNNN`)
        if cls in NON_STAGE_ROOTS or name in NON_STAGE_ROOTS:
            skip_depth = depth
            continue
        # a fighter or a hazard: ported, but not as a vfx. Hazards are still COMPARED —
        # against Fraymakers CustomGameObjects — just in their own section, because a hazard
        # is a state machine and its clock only means something next to the matching state.
        low = cls.lower()
        hit = next((tok for tok in excluded if tok in low), None)
        if hit:
            # Recorded whatever the frame count: a hazard's SSF2 clip is very often ONE frame
            # driven entirely by AS3 (the lava is 1/1, the same pattern as EmberWeather), so a
            # `> 1` filter hides exactly the hazards worth looking at.
            if hit not in CHARACTER_TOKEN:
                hazards[cls if name.startswith("instance") else name] = (int(total), label)
            skip_depth = depth
            continue
        t = int(total)
        if t > 1:
            # A container and the clip that drives it report the SAME length -- they are one
            # animation, not two (`bowsers_podoboos_bg` and its inner clip are both 82 frames).
            # Compare against EVERY ancestor, not just the immediate parent: the driving clip
            # can sit more than one level down, and counting both inflates the SSF2 side into
            # a phantom "missing in Fraymakers".
            for d in [d for d in lengths_by_depth if d >= depth]:
                lengths_by_depth.pop(d, None)
            if t in lengths_by_depth.values():
                lengths_by_depth[depth] = t
                continue
            lengths_by_depth[depth] = t
            ident = (name, pos)
            if ident in seen_ids:
                continue
            seen_ids.add(ident)
            key = name
            if name.startswith("instance"):
                anc = max((d for d in parents if d < depth), default=None)
                key = f"{parents[anc]} \u25b8 child" if anc is not None else cls
            out.setdefault(key, []).append(t)
            if world is not None:
                worlds.setdefault(key, world)
            if cur_plane:
                planes.setdefault(key, cur_plane)
            fr = re.search(r"frame=(\d+)/", line)
            if fr:
                frames1.setdefault(name, int(fr.group(1)))   # raw name: matches _frames_of
    labels = _labels_of(text)
    for sample in (text2, extra):
        for n, ls in _labels_of(sample).items():
            labels.setdefault(n, set()).update(ls)
    return ({k: v[0] for k, v in out.items()}, {k: len(v) for k, v in out.items()},
            hazards, worlds, planes, frames1, _frames_of(text2), ticks, labels)


def fm_animations(stage):
    """{element: total_frames} for every converted vfx running in Fraymakers."""
    p = boot("fm", f"{stage}ssf2", FM_LOG, r"LAUNCHED|CRASH", 5)
    try:
        tell("fm", "awaitmatch p0 90")
        time.sleep(4)
        tell("fm", "tree")
        await_walk(FM_LOG, "[tickables]")
        text = open(FM_LOG, encoding="utf8", errors="replace").read()
        mark2 = len(text)
        fm_ticks = ticks_elapsed("fm", FM_LOG, 4)
        tell("fm", "tree")
        await_walk(FM_LOG, "[tickables]")
        text2 = open(FM_LOG, encoding="utf8", errors="replace").read()[mark2:]
        # extra samples for LABELS only, spaced by the engine's own frame counter — symmetric with
        # the SSF2 side. A segmented element is a custom game object here, and the question it has
        # to answer is which of its animations it actually plays, which two samples can't show.
        extra = ""
        for _ in range(4):
            mark = len(open(FM_LOG, encoding="utf8", errors="replace").read())
            tell("fm", "await p0 150")
            await_walk(FM_LOG, "AWAIT:", since=mark)
            mark_tree = len(open(FM_LOG, encoding="utf8", errors="replace").read())
            tell("fm", "tree")
            await_walk(FM_LOG, "[tickables]", since=mark_tree)
            extra += open(FM_LOG, encoding="utf8", errors="replace").read()[mark_tree:]
    finally:
        tell("fm", "exit")
        time.sleep(2)
        p.terminate()
    # only the converted elements: the engine's own character vfx use other labels
    vfx = collections.Counter()
    vfx_pos = collections.defaultdict(list)
    for x, y, _f, t in re.findall(
            r"Vfx @\(([-\d.]+),([-\d.]+)\) frame=(\d+)/(\d+) 'active'", text):
        vfx[int(t)] += 1
        vfx_pos[int(t)].append((float(x), float(y)))
    # A vfx and its display container report the SAME local position, so the layer stamped on
    # the display row can be joined onto the entity that carries the clock. The two facts come
    # from different walks (tick stack vs display list) and position is what ties them.
    layer_at = {}
    for x, y, lay in re.findall(
            r"HeapsContainer @\(([-\d.]+),([-\d.]+)\)[^\n]*? layer=(\w+)", text):
        layer_at[(round(float(x), 1), round(float(y), 1))] = lay

    # hazards + moving structures, with the state they're currently in
    objs = [(cls, int(t), lab) for cls, t, lab in re.findall(
        r"pxf\.entity\.(CustomGameObject|AnimatedLineSegmentStructure) @\([^)]*\) "
        r"frame=\d+/(\d+) '([^']+)'", text)]
    # {total: current frame} at each sample, so an advance per tick can be measured
    def clocks(t):
        out = {}
        for f, tot in re.findall(r"Vfx @\([^)]*\) frame=(\d+)/(\d+) 'active'", t):
            out.setdefault(int(tot), int(f))
        return out
    # every animation each custom game object is seen playing, keyed by its position: a segmented
    # element ports to a CGO, so "which labels does it play" is the question that decides whether
    # the branch was reproduced, and one sample only ever shows one of them.
    cgo_anims = {}
    for sample in (text, text2, extra):
        for x, y, lab in re.findall(
                r"pxf\.entity\.CustomGameObject @\(([-\d.]+),([-\d.]+)\) frame=\d+/\d+ '([^']+)'",
                sample):
            cgo_anims.setdefault((round(float(x)), round(float(y))), set()).add(lab)
    return vfx, objs, vfx_pos, layer_at, clocks(text), clocks(text2), fm_ticks, cgo_anims


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    stage = sys.argv[1]

    print(f"[{stage}] reading SSF2 animation clocks…", file=sys.stderr)
    excluded = hazard_and_character_names(stage, "mario")
    print(f"[{stage}] not compared as backdrop art: {sorted(excluded)}", file=sys.stderr)
    (ssf2, ssf2_counts, ssf2_hazards, ssf2_world, ssf2_plane,
     ssf2_f1, ssf2_f2, ssf2_ticks, ssf2_labels) = ssf2_animations(stage, excluded)
    print(f"[{stage}] reading Fraymakers animation clocks…", file=sys.stderr)
    fm, fm_objs, fm_pos, fm_layer, fm_c1, fm_c2, fm_ticks, fm_cgo_anims = fm_animations(stage)

    ssf2_by_len = collections.Counter()
    for k, v in ssf2.items():
        ssf2_by_len[v] += ssf2_counts.get(k, 1)
    # An SSF2 plane maps onto exactly one Fraymakers depth container. Anything else is a
    # layering defect: the element exists and animates correctly but draws in front of, or
    # behind, things it shouldn't.
    PLANE_TO_LAYER = {
        "background": "BACKGROUND_BEHIND",
        "midground": "BACKGROUND_BEHIND",
        "foreground": "FOREGROUND_STRUCTURES",
    }

    print(f"\n{'ssf2 object(s)':40}{'ssf2':>6}{'x2':>6}{'fm':>4}  {'plane -> layer':38} verdict")
    print("-" * 96)
    bad = matched = 0
    for length, count in sorted(ssf2_by_len.items(), key=lambda kv: -kv[0]):
        names = sorted(n for n, v in ssf2.items() if v == length)
        # accept 2xN and 2x(N-1): the latter is a clip whose last frame carries a control
        # action, which SSF2 runs but never renders (see the module docstring)
        want, want_ctrl = length * 2, (length - 1) * 2
        got, got_ctrl = fm.get(want, 0), fm.get(want_ctrl, 0)
        label = names[0] if len(names) == 1 else f"{names[0]} +{len(names)-1} more"
        # A segmented clip is converted DELIBERATELY short: only the segments its frame scripts can
        # reach are ported, so its Fraymakers length is a subset of the timeline and the doubling
        # contract does not apply. Checking it against 2x the whole timeline would demand back the
        # very frames the source never plays.
        seg_labels = sorted(set().union(*(ssf2_labels.get(n.split(" ▸ ")[0], set()) for n in names)))
        if len(seg_labels) > 1:
            print(f"{label[:39]:40}{length:>6}{'-':>6}{'-':>4}  "
                  f"{(ssf2_plane.get(names[0]) or '?'):38} "
                  f"segmented into {', '.join(repr(l) for l in seg_labels)} — "
                  f"plays a subset, not the {length}f timeline")
            continue
        if got >= count:
            verdict, matched = "ok", matched + 1
            shown = want
        elif got_ctrl >= count:
            verdict = "ok (last frame is a control frame: plays N-1)"
            matched += 1
            shown = want_ctrl
        elif got or got_ctrl:
            verdict = f"COUNT {max(got, got_ctrl)} in FM vs {count} in SSF2"
            shown = want
            bad += 1
        else:
            near = sorted(fm, key=lambda t: abs(t - want))
            verdict = (f"LENGTH MISMATCH — FM has {near[0]}, expected {want} or {want_ctrl}"
                       if near and abs(near[0] - want) <= 8
                       else f"MISSING — no Fraymakers element with {want} or {want_ctrl} frames")
            shown = want
            bad += 1
        # layer: SSF2's plane against the container the Fraymakers element landed in
        plane = next((ssf2_plane.get(n) for n in names if ssf2_plane.get(n)), None)
        fm_lay = None
        for p_ in fm_pos.get(shown, []):
            fm_lay = fm_layer.get((round(p_[0], 1), round(p_[1], 1)))
            if fm_lay:
                break
        want_lay = PLANE_TO_LAYER.get(plane or "")
        pair = f"{plane or '?'} -> {fm_lay or '?'}"
        if plane and fm_lay and want_lay and fm_lay != want_lay:
            pair += f" WANT {want_lay}"
            verdict = (verdict + "; " if verdict != "ok" else "") + "LAYER MISMATCH"
            bad += 1
        print(f"{label[:39]:40}{length:>6}{shown:>6}{max(got, got_ctrl):>4}  {pair[:37]:38} {verdict}")

    # count both accepted lengths per source clip (2N and, for a control-frame clip, 2(N-1))
    paired = 0
    for length in ssf2_by_len:
        paired += fm.get(length * 2, 0) or fm.get((length - 1) * 2, 0)
    # a segmented element's counterpart is deliberately a different length, so nothing pairs with
    # it by length — that is the design, not an orphan
    segmented = sum(1 for n in ssf2 if len(ssf2_labels.get(n.split(" ▸ ")[0], set())) > 1)
    unpaired = max(0, sum(fm.values()) - paired - segmented)
    print("-" * 96)
    if unpaired:
        print(f"note: {unpaired} Fraymakers element(s) had no SSF2 counterpart by length")
    print(f"{matched}/{len(ssf2_by_len)} animation lengths match the 30->60fps doubling contract\n")

    # ── timing ──────────────────────────────────────────────────────────────────
    # Matching LENGTHS says an element loops over the right number of frames; it says nothing
    # about the RATE. An element that steps every other tick has the correct length and plays
    # at half speed, and every other check here would pass it.
    #
    # No tick counter is needed, which matters because Fraymakers' per-frame recorder feeds an
    # in-process buffer rather than the session log. Within ONE engine every element advances
    # once per tick, so between two samples they must all advance by the SAME amount — an
    # element that doesn't is mistimed relative to its own stage. Comparing each element to its
    # engine's median makes the check self-referential and immune to how fast the host happened
    # to be. Across engines the two medians should differ by ~2x, which is the 30->60 step.
    def advance(f1, f2, total):
        if f1 is None or f2 is None or not total or total <= 1:
            return None
        return (f2 - f1) % total

    rows = []
    for length in sorted(ssf2_by_len, reverse=True):
        names = sorted(n for n, v in ssf2.items() if v == length)
        # the reported key may be `parent > child`; the frame samples are keyed by raw name
        nm = names[0]
        raw = nm.split(" \u25b8 ")[0] if " \u25b8 " in nm else nm
        src = raw if raw in ssf2_f1 else nm
        want = length * 2 if (length * 2) in fm_c1 else (length - 1) * 2
        rows.append((nm, length, advance(ssf2_f1.get(src), ssf2_f2.get(src), length),
                     advance(fm_c1.get(want), fm_c2.get(want), want),
                     sorted(ssf2_labels.get(src, ()))))

    # An advance is only meaningful while the clip has NOT wrapped more than once between
    # samples: `(f2-f1) % total` cannot tell one lap from three. Walks take about a second
    # each, so only the long clips qualify — the short ones are reported as unmeasurable
    # rather than guessed at.
    LAP_SAFE = 60
    def median(vals):
        v = sorted(x for x in vals if x is not None and x > 0)
        return v[len(v) // 2] if v else None
    # a segmented element is excluded from the medians: its advance is a net hop between
    # labelled segments, not a lap of the timeline, so folding it in would drag the stage
    # baseline that every other element is judged against.
    seg = lambda r: len(r[4]) > 1
    s_med = median(r[2] for r in rows if r[1] >= LAP_SAFE and not seg(r))
    f_med = median(r[3] for r in rows if r[1] >= LAP_SAFE and not seg(r))

    print("timing (frames advanced between two samples; all elements should agree)")
    print(f"  {'element':40}{'ssf2':>10}{'fraymakers':>14}  verdict")
    timing_bad = 0
    for nm, length, sa, fa, labs in rows:
        # Report the CAUSE where it's knowable. An element SSF2 drives across several frame
        # labels only ever plays those segments, so it can't match a conversion that plays the
        # whole timeline linearly — the rate is a symptom, the flattening is the defect.
        if len(labs) > 1:
            timing_bad += 1
            print(f"  {nm[:39]:40}{sa if sa is not None else '-':>10}"
                  f"{fa if fa is not None else '-':>14}  "
                  f"SEGMENTED — ssf2 branches between {', '.join(repr(l) for l in labs)} of "
                  f"{length}f; converted as the reachable segments chained linearly")
            continue
        if sa is None or fa is None:
            print(f"  {nm[:39]:40}{'-':>10}{'-':>14}  not measured")
            continue
        if length < LAP_SAFE:
            print(f"  {nm[:39]:40}{sa:>10}{fa:>14}  "
                  f"loops in {length}f — too short to time across two walks")
            continue
        # 25% of its engine's median is generous enough for sampling jitter and tight enough
        # to catch a half-rate or double-rate element
        s_off = s_med and abs(sa - s_med) > max(2, 0.25 * s_med)
        f_off = f_med and abs(fa - f_med) > max(2, 0.25 * f_med)
        if s_off or f_off:
            timing_bad += 1
        note = "ok" if not (s_off or f_off) else \
            f"RATE OFF — stage median is ssf2 {s_med} / fm {f_med}"
        print(f"  {nm[:39]:40}{sa:>10}{fa:>14}  {note}")
    if s_med and f_med:
        ratio = f_med / s_med
        print(f"  median advance: ssf2 {s_med}, fraymakers {f_med} ({ratio:.2f}x) — "
              f"{'matches the 30->60 step' if 1.5 <= ratio <= 2.5 else 'EXPECTED ~2x'}")
        if not 1.5 <= ratio <= 2.5:
            timing_bad += 1
    print()
    bad += timing_bad

    # ── placement correlation ───────────────────────────────────────────────────
    # The two engines don't share a coordinate system, and hardcoding a conversion would just
    # bake in an assumption. Instead the mapping is DERIVED from the elements that already
    # matched on animation length: if the port is faithful, one (scale, offset) explains every
    # element, and anything that needs a different one is misplaced. That makes the check
    # self-calibrating — it works on a stage whose coordinate model nobody has written down.
    pairs = []
    claimed = set()
    for length in ssf2_by_len:
        for want in (length * 2, (length - 1) * 2):
            if want in fm_pos and len(fm_pos[want]) == 1:
                names = [n for n, v in ssf2.items() if v == length]
                if len(names) == 1 and names[0] in ssf2_world:
                    pairs.append((names[0], ssf2_world[names[0]], fm_pos[want][0]))
                claimed.add(want)
                break
    # A segmented element is converted to a SUBSET of its timeline, so it can never pair on
    # length. Pair it by elimination instead — its position is just as checkable as anything
    # else's, and dropping it would shrink the placement fit to the point of proving nothing.
    seg_names = [n for n in ssf2 if len(ssf2_labels.get(n.split(" ▸ ")[0], set())) > 1]
    spare = [t for t in fm_pos if t not in claimed and len(fm_pos[t]) == 1]
    if len(seg_names) == 1 and len(spare) == 1 and seg_names[0] in ssf2_world:
        total = ssf2.get(seg_names[0], 0)
        print(f"  (paired by elimination: {seg_names[0]} plays {spare[0]}f in Fraymakers, "
              f"{spare[0] / 2:.0f} of its {total}f timeline)")
        pairs.append((seg_names[0], ssf2_world[seg_names[0]], fm_pos[spare[0]][0]))

    # Tolerance, and why it isn't tighter: an SSF2 bounding box is measured on a LIVE clip, so
    # it changes as the clip animates — the podoboos box was seen at 672x245, 673x253, 704x252
    # and 691x271 across samples of the same object. Placement can therefore only be compared
    # to within an element's own frame-to-frame movement, and a residual smaller than that is
    # not evidence of anything. Sampling both engines on a known frame would tighten it.
    TOL = 40.0

    if len(pairs) >= 2:
        # Least squares over every pair, not a line through two of them: a two-point fit puts
        # all of its error onto whichever point is left out, which reads as "that element is
        # misplaced" when the fit is what's wrong.
        def fit(axis):
            pts = [(sw[axis], fp[axis]) for _n, sw, fp in pairs]
            n = len(pts)
            sx = sum(a for a, _ in pts); sy = sum(b for _, b in pts)
            sxx = sum(a * a for a, _ in pts); sxy = sum(a * b for a, b in pts)
            den = n * sxx - sx * sx
            if abs(den) < 1e-9:
                return None
            k = (n * sxy - sx * sy) / den
            return k, (sy - k * sx) / n
        fx, fy = fit(0), fit(1)
        print("placement (mapping derived from the matched elements)")
        if fx and fy:
            print(f"  fm_x = {fx[0]:.3f} * ssf2_x + {fx[1]:.1f}      "
                  f"fm_y = {fy[0]:.3f} * ssf2_y + {fy[1]:.1f}")
            print(f"  {'element':44}{'ssf2 world':>20}{'fraymakers':>20}{'residual':>12}")
            worst = 0.0
            for n, sw, fp in sorted(pairs):
                px, py = fx[0] * sw[0] + fx[1], fy[0] * sw[1] + fy[1]
                dx, dy = fp[0] - px, fp[1] - py
                r = max(abs(dx), abs(dy))
                worst = max(worst, r)
                flag = "  <-- OFF" if r > TOL else ""
                print(f"  {n[:43]:44}{f'({sw[0]:.0f},{sw[1]:.0f})':>20}"
                      f"{f'({fp[0]:.0f},{fp[1]:.0f})':>20}{r:>11.1f}{flag}")
            print(f"  worst residual {worst:.1f}px "
                  f"({'consistent — one mapping explains every element' if worst <= TOL else 'INCONSISTENT — an element is misplaced'})")
        else:
            print("  not enough separation between elements to derive a mapping")
        print()

    # ── hazards ─────────────────────────────────────────────────────────────────
    # Compared, but SEPARATELY and without a pass/fail on length: a hazard is a state
    # machine (the thwomp has idle/rise/fall/land), so its clock only means something next
    # to the matching state. The two engines are rarely in the same state at sample time —
    # Fraymakers was in `entrance` while SSF2 sat in `idle` — so this reports both sides and
    # leaves the judgement to a state-aware comparison rather than inventing a verdict.
    if ssf2_hazards or fm_objs:
        print("hazards / moving structures (state machines — reported, not length-checked)")
        print(f"  {'ssf2 clip':40}{'frames':>8}  {'state':<18}")
        for name, (total, label) in sorted(ssf2_hazards.items()):
            print(f"  {name[:39]:40}{total:>8}  {label or '-':<18}")
        print(f"  {'fraymakers object':40}{'frames':>8}  {'state':<18}")
        for cls, total, label in sorted(fm_objs):
            print(f"  {cls[:39]:40}{total:>8}  {label:<18}")
        print()

    sys.exit(1 if bad else 0)
