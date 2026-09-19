#!/usr/bin/env python3
"""Bucket JFR execution samples of a chunk generation burst by pipeline stage.

    python3 tools/jfr_split.py run/painite_fr4.jfr [--window 40] [--jfr <path to jfr tool>]
                               [--from HH:MM:SS --to HH:MM:SS] [--leaves 20]

Reads jdk.ExecutionSample (Java) and jdk.NativeMethodSample events through
`jfr print --json`, keeps the samples inside the window (by default the
last --window seconds of the recording, which is where tools/ab.sh's
measured burst sits), and prints:
  - samples per thread family (Worker-Main, Painite, Server thread, ...)
  - exclusive buckets: each sample counted once, by the first marker
    found walking the stack from the leaf outwards
  - inclusive buckets: a sample counts for every marker on its stack
  - the top leaf methods
Markers are substrings of "class.method" in mojmap names; extend MARKERS
when a new stage appears.
"""
import argparse, collections, datetime, json, os, subprocess, sys

# Order matters for the exclusive view: first match from the leaf wins.
MARKERS = [
    ("native terrain", ["PainiteNative", "painite/terrain/TerrainBridge", "Java_me_apika"]),
    ("ores", ["OreFeature", "OreBridge"]),
    ("features", ["applyBiomeDecoration", "generateFeatures", "ChunkStatusTasks.generateFeatures", "FeaturePlaceContext", "levelgen/feature/"]),
    ("structures", ["levelgen/structure/", "StructureStart", "createStructures", "createReferences"]),
    ("beardifier", ["Beardifier"]),
    ("surface", ["buildSurface", "SurfaceSystem", "MaterialSystem", "SurfaceRules", "levelgen/material/"]),
    ("carvers", ["applyCarvers", "generateCarvers", "levelgen/carver/"]),
    ("density / noise", ["NoiseChunk", "DensityFunction", "InterpolatedFunction", "densityfunction/", "levelgen/synth/", "NoiseBasedChunkGenerator.doFill", "fillFromNoise", "Aquifer"]),
    ("biomes / climate", ["Climate$", "MultiNoiseBiomeSource", "createBiomes", "fillBiomesFromNoise", "BiomeManager"]),
    ("lighting", ["level/lighting/", "ThreadedLevelLightEngine", "LightEngine"]),
    ("serialisation", ["SerializableChunkData", "ChunkSerializer", "RegionFile", "IOWorker", "ChunkStorage", "NbtIo", "pwrite0", "Deflater", "lstat0", "BufferedOutputStream", "RegionFileStorage"]),
    ("chunk load / empty", ["ChunkMap.readChunk", "scheduleChunkLoad", "ChunkMap.lambda$scheduleChunkLoad"]),
    ("dispatcher", ["ChunkTaskDispatcher", "ConsecutiveExecutor", "ChunkMap", "ChunkHolder", "GenerationChunkHolder", "ChunkGenerationTask"]),
    ("server tick", ["tickChunks", "ServerLevel.tick", "MinecraftServer.tickServer", "ServerChunkCache.tick"]),
    ("gc / jvm", ["java/lang/ref", "jdk/internal/misc/Unsafe.park", "ForkJoinPool"]),
]

# Mechanisms every stage uses; attributed to the calling stage, and
# reported on their own as "section access by stage".
MECHANISMS = [
    ("section access", ["PalettedContainer", "BitStorage", "LevelChunkSection", "ProtoChunk.setBlockState", "ProtoChunk.getBlockState", "BulkSectionAccess", "LevelHeightAccessor", "Palette"]),
    ("heightmap", ["Heightmap"]),
]

FAMILIES = ["Worker-Main", "Painite", "Server thread", "IO-Worker", "main", "RCON", "Netty"]

# Native leaves that mean a thread is waiting, not working.
IDLE_LEAVES = ["Unsafe.park", "SocketDispatcher", "Net.poll", "epollWait", "EPoll", "Thread.sleep", "Object.wait", "PlainSocketImpl", "Native.epoll"]


def run_jfr(jfr_tool, path, event):
    # jfr print truncates stacks to 5 frames unless told otherwise.
    out = subprocess.run([jfr_tool, "print", "--json", "--stack-depth", "64", "--events", event, path], capture_output=True, text=True, check=True)
    return json.loads(out.stdout)["recording"]["events"]


def parse_time(s):
    # 2026-09-06T22:49:30.022395305-01:00 -> aware datetime (trim to microseconds)
    main, tz = s[:-6], s[-6:]
    if "." in main:
        head, frac = main.split(".")
        main = f"{head}.{frac[:6]}"
    return datetime.datetime.fromisoformat(main + tz)


def frame_name(fr):
    m = fr.get("method") or {}
    t = (m.get("type") or {}).get("name") or "?"
    return f"{t}.{m.get('name', '?')}"


def leaf_is(leaf, mech_label):
    keys = dict(MECHANISMS)[mech_label]
    return any(k in leaf for k in keys)


def family(thread_name):
    for f in FAMILIES:
        if thread_name.startswith(f):
            return f
    return "other"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("recording")
    ap.add_argument("--window", type=float, default=40.0, help="seconds before the end of the recording to keep")
    ap.add_argument("--from", dest="t_from", help="HH:MM:SS local wall clock, overrides --window")
    ap.add_argument("--to", dest="t_to", help="HH:MM:SS local wall clock")
    ap.add_argument("--leaves", type=int, default=20)
    ap.add_argument("--callers", action="append", default=[], help="leaf substring: print its top callers (first frame outside the leaf's class) and marker")
    ap.add_argument("--stacks", action="append", default=[], help="leaf substring: print the three most common stacks (14 frames) under it")
    ap.add_argument("--jfr", default=os.path.expanduser("~/.jdks/jdk-25.0.4+7/bin/jfr"))
    args = ap.parse_args()

    java = run_jfr(args.jfr, args.recording, "jdk.ExecutionSample")
    native = run_jfr(args.jfr, args.recording, "jdk.NativeMethodSample")
    events = [(e, "java") for e in java] + [(e, "native") for e in native]
    if not events:
        print("no samples")
        return 1
    times = [parse_time(e["values"]["startTime"]) for e, _ in events]
    end = max(times)
    if args.t_from:
        day = end.astimezone().date()
        tz = end.astimezone().tzinfo
        t0 = datetime.datetime.combine(day, datetime.time.fromisoformat(args.t_from), tz)
        t1 = datetime.datetime.combine(day, datetime.time.fromisoformat(args.t_to), tz) if args.t_to else end
    else:
        t0, t1 = end - datetime.timedelta(seconds=args.window), end
    kept = [(e, kind) for (e, kind), t in zip(events, times) if t0 <= t <= t1]
    print(f"{args.recording}: {len(java)} java + {len(native)} native samples, window {t0.astimezone().time()} .. {t1.astimezone().time()}: {len(kept)} kept")

    by_family = collections.Counter()
    exclusive = collections.Counter()
    inclusive = collections.Counter()
    leaves = collections.Counter()
    unmarked_leaves = collections.Counter()
    callers = {pat: collections.Counter() for pat in args.callers}
    stacks = {pat: collections.Counter() for pat in args.stacks}
    mechanism_by_stage = collections.Counter()
    idle_native = 0
    for e, kind in kept:
        v = e["values"]
        frames = [frame_name(f) for f in (v.get("stackTrace") or {}).get("frames") or []]
        name = (v.get("sampledThread") or {}).get("javaName") or "?"
        leaf = frames[0] if frames else "?"
        if kind == "native" and any(k in leaf for k in IDLE_LEAVES):
            idle_native += 1
            continue
        by_family[family(name)] += 1
        leaves[leaf] += 1
        hit = None
        for fr in frames:
            for label, keys in MARKERS:
                if any(k in fr for k in keys):
                    hit = hit or label
                    break
        mech = None
        for fr in frames:
            for label, keys in MECHANISMS:
                if any(k in fr for k in keys):
                    mech = mech or label
                    break
        if hit is None and mech is not None:
            hit = mech
        if mech is not None and leaf_is(leaf, mech):
            mechanism_by_stage[f"{mech} <- {hit}"] += 1
        exclusive[hit or "unmarked"] += 1
        if hit is None:
            unmarked_leaves[f"{family(name)}: {leaf}"] += 1
        for pat, counter in callers.items():
            if pat in leaf:
                cls = leaf.rsplit(".", 1)[0]
                outside = next((f for f in frames[1:] if not f.startswith(cls)), "?")
                counter[f"{outside}  [{hit or 'unmarked'}]"] += 1
        for pat, counter in stacks.items():
            if pat in leaf:
                counter["\n      ".join(f.split("/")[-1] for f in frames[:14])] += 1
        seen = set()
        for fr in frames:
            for label, keys in MARKERS:
                if label not in seen and any(k in fr for k in keys):
                    seen.add(label)
        for label in seen:
            inclusive[label] += 1
    total = sum(by_family.values())
    print(f"busy samples {total} (native idle/blocked dropped: {idle_native})")
    print("\nby thread family:")
    for f, n in by_family.most_common():
        print(f"  {f:14s} {n:6d} {100 * n / total:5.1f}%")
    print("\nexclusive (first marker from the leaf):")
    for label, n in exclusive.most_common():
        print(f"  {label:28s} {n:6d} {100 * n / total:5.1f}%")
    print("\ninclusive (any marker on the stack):")
    for label, n in inclusive.most_common():
        print(f"  {label:28s} {n:6d} {100 * n / total:5.1f}%")
    print("\nmechanism leaves by calling stage:")
    for label, n in mechanism_by_stage.most_common(12):
        print(f"  {label:44s} {n:6d} {100 * n / total:5.1f}%")
    print(f"\ntop {args.leaves} leaves:")
    for leaf, n in leaves.most_common(args.leaves):
        print(f"  {n:6d} {100 * n / total:5.1f}%  {leaf}")
    print("\ntop unmarked leaves (extend MARKERS from these):")
    for leaf, n in unmarked_leaves.most_common(12):
        print(f"  {n:6d} {100 * n / total:5.1f}%  {leaf}")
    for pat, counter in callers.items():
        print(f"\ncallers of leaves matching '{pat}' ({sum(counter.values())} samples):")
        for caller, n in counter.most_common(10):
            print(f"  {n:6d}  {caller}")
    for pat, counter in stacks.items():
        print(f"\nstacks under leaves matching '{pat}' ({sum(counter.values())} samples):")
        for stack, n in counter.most_common(3):
            print(f"  {n:6d}  {stack}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
