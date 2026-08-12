#!/usr/bin/env python3
"""move_termination.py — find moves that END EARLY in SSF2 but run to completion converted.

A bug class nothing else here can see, because every static check passes on it.

`timing_parity.py` confirms a converted animation is exactly twice its source range. That
can be perfectly true while the MOVE still diverges: SSF2 scripts routinely cut an
animation short (the move does its job and exits), so the animation is 23 source frames
long but only ever plays 17. The conversion, having correctly doubled 23 into 46, plays
all 46 — roughly a third longer than the move a player experiences in SSF2.

Measured on mario:
    jab        source range  7   SSF2 played  7   exact
    neutral-B  source range 23   SSF2 played 17   ends 6 source frames early

So the detector is: expected = the source range the splitter sliced (static, from
PEPTIDE_DUMP_ANIM_SPLITS); actual = what SSF2's engine-side recorder says actually
played; the GAP is the finding. A gap means the converted move outlives SSF2's.

This drives SSF2 only. The Fraymakers side needs no measurement: it plays the emitted
animation, whose length is already verified against the source.

Usage:
  tools/tests/move_termination.py <char> [<input> …]      # default: attack special jump
"""
import os, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SSFS = os.environ.get("SSF2_SSFS_DIR", os.path.join(ROOT, "..", "ssf2-ssfs"))
BIN = os.path.join(ROOT, "build", "release", "peptide")
LOG = "/tmp/move_termination.log"


def source_ranges(char):
    """FM animation -> source frame count, from the splitter's own record."""
    env = dict(os.environ, PEPTIDE_DUMP_ANIM_SPLITS="1", PEPTIDE_DUMP_ANIM_LABELS="1")
    r = subprocess.run([BIN, "convert", os.path.join(SSFS, f"{char}.ssf"), "-n", char,
                        "-o", os.path.join(ROOT, "build", "term_check"), "-y"],
                       capture_output=True, text=True, env=env)
    text = r.stderr + r.stdout
    totals = {m.group(1): int(m.group(2))
              for m in re.finditer(r"\[anim-labels\] (\S+) total=(\d+)", text)}
    out = {}
    for m in re.finditer(r"\[anim-split\] (\S+) <- (\S+) \[(\d+)\.\.(\d+|end)\)", text):
        fm, src, start, end = m.groups()
        start = int(start)
        end = totals.get(src, start) if end == "end" else int(end)
        out[fm] = max(0, end - start)
    return out


def ssf2_to_fm():
    """The converter's own SSF2-name -> Fraymakers-name map.

    SSF2's live label is an SSF2 name (`a`, `b`); the source ranges are keyed by Fraymakers
    name (`jab1`, `special_neutral`). This is the pairing key the converter itself uses
    (mappings/character/animations.jsonc :: ssf2_to_fm) — the harness must read it rather
    than guess, since the names genuinely differ (`a` -> `jab`, `b` -> `special_neutral`).
    Parsed with a regex because the file is JSONC with comments and duplicate keys.
    """
    path = os.path.join(ROOT, "crates", "ssf2-converter", "mappings", "character", "animations.jsonc")
    txt = open(path, encoding="utf8").read()
    # only the ssf2_to_fm block
    blk = txt.split('"ssf2_to_fm"', 1)[-1]
    out = {}
    for m in re.finditer(r'"([\w./-]+)"\s*:\s*"([\w./-]+)"', blk):
        out.setdefault(m.group(1), m.group(2))
    return out


def resolve(label, mapping, src):
    """The source frame count for an SSF2 label, via the map and the split records.

    A mapped base name may not be an emitted animation on its own: one SSF2 timeline can
    split into several FM animations (`jab` -> jab1/jab2/jab3), so the first piece is what
    a single button press plays.
    """
    for cand in (label, mapping.get(label, ""), mapping.get(label, "") + "1"):
        if cand and cand in src:
            return cand, src[cand]
    # The mapped name may name a FAMILY rather than an emitted animation (`jump` splits
    # into jump_in / jump_loop / jump_out). A single played count can't be compared to one
    # piece, and summing them is wrong when one of them loops, so say so instead of
    # inventing a number.
    base = mapping.get(label, "")
    if base:
        fam = sorted(k for k in src if k == base or k.startswith(base + "_") or
                     re.fullmatch(re.escape(base) + r"\d+", k))
        if len(fam) > 1:
            return "+".join(fam), None
    return None, None


def tell(cmd):
    subprocess.run([BIN, "ssf2", "tell", cmd], capture_output=True, text=True)


def boot(char):
    subprocess.run(["pkill", "-f", "peptide ssf2 session"], capture_output=True)
    subprocess.run(["pkill", "-f", "SSF2-patched"], capture_output=True)
    time.sleep(2)
    subprocess.run(["rm", "-rf", os.path.expanduser("~/.peptide/session")], capture_output=True)
    f = open(LOG, "w")
    p = subprocess.Popen([BIN, "ssf2", "session", "--char", char], stdout=f, stderr=f)
    # wait for the match by ASKING the engine, not by sleeping a guessed amount
    for _ in range(45):
        time.sleep(2)
        if "peptide ssf2 tell" in open(LOG).read():
            break
    tell("awaitmatch p0 90")
    for _ in range(45):
        time.sleep(1)
        if "MATCHREADY:" in open(LOG).read():
            break
    return p


def played(inp):
    """Drive one input and return {label: frames} from the engine-side recorder."""
    before = len(open(LOG).read())
    tell("record")
    tell(f"seq {inp}:2")
    time.sleep(4)          # let the move finish; the RECORDER is what measures it
    tell("trace")
    time.sleep(3)
    tail = open(LOG).read()[before:]
    runs = {}
    for m in re.finditer(r"^\s+(\S+) x(\d+)", tail, re.M):
        label, n = m.group(1), int(m.group(2))
        if label in ("stand", "idle"):
            continue
        runs[label] = max(runs.get(label, 0), n)
    return runs


if __name__ == "__main__":
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        sys.exit(2)
    char, inputs = args[0], (args[1:] or ["attack", "special", "jump"])

    print(f"[{char}] reading source ranges…", file=sys.stderr)
    src = source_ranges(char)
    names = ssf2_to_fm()
    print(f"[{char}] booting SSF2…", file=sys.stderr)
    proc = boot(char)

    print(f"\n{'input':10}{'ssf2 anim':16}{'played':>8}{'source':>8}{'gap':>7}  verdict")
    print("-" * 68)
    findings = 0
    try:
        for inp in inputs:
            runs = played(inp)
            if not runs:
                print(f"{inp:10}{'(nothing played)':16}")
                continue
            for label, n in sorted(runs.items(), key=lambda x: -x[1]):
                # the SSF2 label is the sub-animation; the converter maps it to an FM
                # animation of the same granularity (see AGENT_CONTEXT "animations")
                fm_name, expect = resolve(label, names, src)
                if expect is None:
                    why = (f"splits into {fm_name} — a played count can't be compared to one piece"
                           if fm_name else "no source range (unmapped label)")
                    print(f"{inp:10}{label:16}{n:>8}{'-':>8}{'':>7}  {why}")
                    continue
                gap = expect - n
                if gap > 1:
                    findings += 1
                    verdict = f"ENDS EARLY by {gap} source frames ({gap*2} converted)"
                elif gap < -1:
                    verdict = "plays LONGER than the source range"
                else:
                    verdict = "matches"
                print(f"{inp:10}{label+'->'+fm_name:16}{n:>8}{expect:>8}{gap:>7}  {verdict}")
    finally:
        tell("exit")
        time.sleep(2)
        proc.terminate()
    print(f"\n{findings} move(s) end early in SSF2 — the conversion plays them to the end")
    sys.exit(1 if findings else 0)
