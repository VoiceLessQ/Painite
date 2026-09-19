#!/usr/bin/env python3
# Allocation report from a JFR recording: timeline, then sites and callers
# inside a window. Weighted by jdk.ObjectAllocationSample, so shares are
# estimates; ThreadAllocationStatistics has the exact per-thread totals.
#
#   jfr print --stack-depth 12 --events jdk.ObjectAllocationSample x.jfr > alloc.txt
#   jfr print --events jdk.GarbageCollection x.jfr > gc.txt
#   tools/jfr_alloc.py alloc.txt gc.txt [start,end]   (seconds from the first sample)
#
# Text form on purpose: `jfr print --json` writes about 70 KB per event.
import sys, re, collections
alloc, gcf = sys.argv[1], sys.argv[2]
win = [float(x) for x in sys.argv[3].split(",")] if len(sys.argv) > 3 else None
UNIT = {"bytes": 1, "kB": 1e3, "MB": 1e6, "GB": 1e9}
def secs(hms):
    h, m, s = hms.split(":"); return int(h) * 3600 + int(m) * 60 + float(s)
def parse(path):
    ev = None
    with open(path) as f:
        for line in f:
            if line.startswith("jdk."):
                ev = {"frames": []}; continue
            if ev is None: continue
            s = line.strip()
            if s == "}":
                yield ev; ev = None
            elif s.startswith("startTime"): ev["t"] = secs(s.split()[2])
            elif s.startswith("weight"):
                _, _, v, u = s.split(); ev["w"] = float(v) * UNIT[u]
            elif s.startswith("sumOfPauses"):
                _, _, v, u = s.split(); ev["pause"] = float(v) * {"ns": 1e-6, "us": 1e-3, "ms": 1, "s": 1000}[u]
            elif s.startswith("name ="): ev["name"] = s.split('"')[1]
            elif s.startswith("eventThread"): ev["thread"] = re.sub(r"[- #]\d+$", "", s.split('"')[1])
            elif s.startswith("objectClass"): ev["cls"] = s.split(" = ")[1].split(" (")[0]
            elif " line: " in s or s.endswith("(Native Method)") or re.match(r"^[\w$.<>]+\(.*\)", s):
                m = re.match(r"^([\w$.]+)\.([\w$<>]+)\(", s)
                if m: ev["frames"].append(m.group(1).split(".")[-1] + "." + m.group(2))
A = list(parse(alloc)); G = list(parse(gcf))
t0 = min(e["t"] for e in A)
tl = collections.Counter(); gtl = collections.Counter(); gn = collections.Counter()
for e in A: tl[int((e["t"] - t0) // 10) * 10] += e["w"]
for e in G: k = int((e["t"] - t0) // 10) * 10; gtl[k] += e["pause"]; gn[k] += 1
print("bucket allocGB gcN pause_ms")
for k in sorted(set(tl) | set(gtl)): print(f"{k:5d} {tl[k]/1e9:7.2f} {gn[k]:3d} {gtl[k]:7.0f}")
if win:
    A = [e for e in A if win[0] <= e["t"] - t0 < win[1]]; G = [e for e in G if win[0] <= e["t"] - t0 < win[1]]
    print(f"\nwindow {win}: {len(G)} GCs, pauses {sum(e['pause'] for e in G):.0f} ms, {collections.Counter(e['name'] for e in G)}")
tot = sum(e["w"] for e in A)
skip = ("Object.", "Arrays.", "String", "ArrayList", "HashMap", "ImmutableCollections", "Long2Object", "LongArray", "AbstractCollection", "Direct", "Invokers", "MethodHandle", "Collect", "Stream", "Nodes", "Spliterator", "ReferencePipeline", "LinkedList", "Iterator", "ObjectArrayList", "Int2Object", "Long2Int", "Holder.", "Optional", "Map.", "List.", "Collections", "AbstractList", "Long2Long", "IntArrayList", "ObjectOpenHashSet", "LongOpenHashSet", "Lambda", "Node.", "BitSet")
by_thread = collections.Counter(); by_site = collections.Counter(); by_owner = collections.Counter(); by_cls = collections.Counter(); chains = collections.defaultdict(collections.Counter)
for e in A:
    w = e["w"]; fr = e["frames"]
    by_thread[e.get("thread", "?")] += w; by_cls[e.get("cls", "?")] += w
    i = next((i for i, f in enumerate(fr) if not f.startswith(skip)), 0)
    site = fr[i] if fr else "?"
    by_site[site] += w; chains[site][" < ".join(fr[i+1:i+6])] += w
    ours = [f for f in fr if "painite" in f.lower() and "$painite$" not in f and "mixinextras" not in f]
    by_owner["painite:" + ours[0] if ours else "vanilla"] += w
print(f"\n== threads ({tot/1e9:.2f} GB sampled)")
for k, w in by_thread.most_common(8): print(f"{100*w/tot:5.1f}%  {k}")
print("\n== object class")
for k, w in by_cls.most_common(10): print(f"{100*w/tot:5.1f}%  {k}")
print("\n== owner")
for k, w in by_owner.most_common(12): print(f"{100*w/tot:5.1f}%  {k}")
print("\n== sites with callers")
for k, w in by_site.most_common(20):
    print(f"{100*w/tot:5.1f}%  {k}")
    for c, cw in chains[k].most_common(2): print(f"          {100*cw/tot:4.1f}%  {c[:200]}")
