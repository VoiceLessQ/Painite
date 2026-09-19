"""Run-to-run spread test for chunk generation.

    python3 tools/spread.py --candidate W1 W2 ... [--baseline B1 B2 ...]
                            [--status terrain,initialize_light]
                            [--max-ratio 2.0] [--min-class 0.01] [--detail 200]

Worlds in one group were generated with the same seed and the same flags;
the test measures how much their block states differ from each other.
With --baseline (vanilla serial runs) it fails when the candidate group's
chunks-differ fraction exceeds max-ratio times the baseline's, or when
the candidate shows a block-pair class (a -> b, at least min-class of
its differing blocks) that the baseline never shows. Without a baseline
it only reports. Exit code 1 on failure.
"""
import argparse, collections, itertools, os, sys
sys.path.insert(0, os.path.dirname(__file__))
import regiondiff as rd

def load_group(worlds, statuses):
    """[(name, {pos: (status, fp)}, {pos: nbt for kept statuses})]"""
    out = []
    for w in worlds:
        fps, keep = {}, {}
        for pos, nbt in rd.chunks(w):
            st = nbt.get("Status")
            fps[pos] = (st, rd.fingerprint(nbt))
            if st in statuses: keep[pos] = nbt
        out.append((os.path.basename(w.rstrip("/")), fps, keep))
    return out

def group_stats(group, statuses, detail):
    """per status: mean differ fraction over pairs, block-pair counter, block total."""
    frac = {st: [] for st in statuses}
    pairs = {st: collections.Counter() for st in statuses}
    for (na, fa, ka), (nb, fb, kb) in itertools.combinations(group, 2):
        for st in statuses:
            common = [p for p in fa if p in fb and fa[p][0] == st and fb[p][0] == st]
            if not common: frac[st].append(0.0); continue
            diff = [p for p in common if fa[p][1] != fb[p][1]]
            frac[st].append(len(diff) / len(common))
            for p in sorted(diff)[:detail]:
                pr, _, _ = rd.block_pairs(ka[p], kb[p]); pairs[st] += pr
    return {st: (sum(v) / len(v) if v else 0.0) for st, v in frac.items()}, pairs

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--candidate", nargs="+", required=True)
    ap.add_argument("--baseline", nargs="*", default=[])
    ap.add_argument("--status", default="terrain,initialize_light")
    ap.add_argument("--max-ratio", type=float, default=2.0)
    ap.add_argument("--min-class", type=float, default=0.01)
    ap.add_argument("--detail", type=int, default=200)
    a = ap.parse_args()
    statuses = ["minecraft:" + s if ":" not in s else s for s in a.status.split(",")]
    if len(a.candidate) < 2: raise SystemExit("need at least two candidate worlds")
    if len(a.baseline) == 1: raise SystemExit("need zero or at least two baseline worlds")

    cand = load_group(a.candidate, statuses)
    cf, cp = group_stats(cand, statuses, a.detail)
    base = load_group(a.baseline, statuses) if a.baseline else None
    bf, bp = group_stats(base, statuses, a.detail) if base else ({}, {})

    failed = []
    for st in statuses:
        short = st.replace("minecraft:", "")
        line = "%-18s candidate differ %.3f" % (short, cf[st])
        if base:
            ratio = cf[st] / bf[st] if bf[st] else float("inf")
            line += "  baseline %.3f  ratio %.2f" % (bf[st], ratio)
            if ratio > a.max_ratio: failed.append("%s ratio %.2f > %.2f" % (short, ratio, a.max_ratio))
        print(line)
        total = sum(cp[st].values()) or 1
        for (x, y), c in cp[st].most_common(8):
            mark = ""
            if base and c / total >= a.min_class and (x, y) not in bp[st] and (y, x) not in bp[st]:
                mark = "  NEW"; failed.append("%s new class %s -> %s" % (short, x, y))
            print("  %6d %5.1f%% %s -> %s%s" % (c, 100.0 * c / total, x.replace("minecraft:", ""), y.replace("minecraft:", ""), mark))
    if failed:
        print("FAIL:", "; ".join(failed)); sys.exit(1)
    print("PASS" if base else "reported (no baseline)")

if __name__ == "__main__":
    main()
