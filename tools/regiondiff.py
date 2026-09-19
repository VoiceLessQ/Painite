"""Per-chunk block-state comparison between two Anvil worlds.

    python3 tools/regiondiff.py A B                     chunks differing, per status
    python3 tools/regiondiff.py A B detail [status] [N]  block-level pairs on N chunks

A world is the save directory (run/world_x); the region folder is found
for both the pre-26 layout (region/) and 26.x (dimensions/<ns>/<dim>/region).
"""
import sys, os, struct, zlib, gzip, hashlib, json, collections

def read_nbt(buf, pos, tag):
    if tag == 0: return None, pos
    if tag == 1: return buf[pos], pos + 1
    if tag == 2: return struct.unpack_from(">h", buf, pos)[0], pos + 2
    if tag == 3: return struct.unpack_from(">i", buf, pos)[0], pos + 4
    if tag == 4: return struct.unpack_from(">q", buf, pos)[0], pos + 8
    if tag == 5: return struct.unpack_from(">f", buf, pos)[0], pos + 4
    if tag == 6: return struct.unpack_from(">d", buf, pos)[0], pos + 8
    if tag == 7:
        n = struct.unpack_from(">i", buf, pos)[0]; pos += 4
        return bytes(buf[pos:pos + n]), pos + n
    if tag == 8:
        n = struct.unpack_from(">H", buf, pos)[0]; pos += 2
        return buf[pos:pos + n].decode("utf-8", "replace"), pos + n
    if tag == 9:
        t = buf[pos]; n = struct.unpack_from(">i", buf, pos + 1)[0]; pos += 5
        out = []
        for _ in range(n):
            v, pos = read_nbt(buf, pos, t); out.append(v)
        return out, pos
    if tag == 10:
        out = {}
        while True:
            t = buf[pos]; pos += 1
            if t == 0: return out, pos
            n = struct.unpack_from(">H", buf, pos)[0]; pos += 2
            name = buf[pos:pos + n].decode("utf-8", "replace"); pos += n
            v, pos = read_nbt(buf, pos, t); out[name] = v
    if tag == 11:
        n = struct.unpack_from(">i", buf, pos)[0]; pos += 4
        return list(struct.unpack_from(">%di" % n, buf, pos)), pos + 4 * n
    if tag == 12:
        n = struct.unpack_from(">i", buf, pos)[0]; pos += 4
        return list(struct.unpack_from(">%dq" % n, buf, pos)), pos + 8 * n
    raise ValueError("tag %d" % tag)

def root(buf):
    assert buf[0] == 10
    n = struct.unpack_from(">H", buf, 1)[0]
    v, _ = read_nbt(buf, 3 + n, 10)
    return v

def region_dir(world, dim="minecraft/overworld"):
    for cand in (os.path.join(world, "dimensions", dim, "region"),
                 os.path.join(world, "region")):
        if os.path.isdir(cand): return cand
    raise SystemExit("no region dir under %s" % world)

def chunks(world):
    d = region_dir(world)
    for f in sorted(os.listdir(d)):
        if not f.endswith(".mca"): continue
        data = open(os.path.join(d, f), "rb").read()
        for i in range(1024):
            off = int.from_bytes(data[i*4:i*4+3], "big") * 4096
            cnt = data[i*4+3]
            if off == 0 or cnt == 0: continue
            ln = struct.unpack_from(">i", data, off)[0]; comp = data[off+4]
            raw = data[off+5:off+4+ln]
            if comp == 2: raw = zlib.decompress(raw)
            elif comp == 1: raw = gzip.decompress(raw)
            elif comp == 3: pass
            else: raise ValueError("compression %d in %s" % (comp, f))
            nbt = root(raw)
            yield (nbt.get("xPos"), nbt.get("zPos")), nbt

def canon(v):
    if isinstance(v, dict): return {k: canon(v[k]) for k in sorted(v)}
    if isinstance(v, list): return [canon(x) for x in v]
    if isinstance(v, bytes): return v.hex()
    return v

def fingerprint(nbt):
    secs = []
    for s in nbt.get("sections", []):
        bs = s.get("block_states")
        if bs is None: continue
        secs.append((s.get("Y"), canon(bs)))
    secs.sort(key=lambda t: (t[0] if t[0] is not None else -999))
    return hashlib.sha1(json.dumps(secs, sort_keys=True).encode()).hexdigest()

def load(world):
    """pos -> (status, fingerprint) for every chunk in the world."""
    return {pos: (nbt.get("Status"), fingerprint(nbt)) for pos, nbt in chunks(world)}

def decode_section(bs):
    pal = bs.get("palette", []); data = bs.get("data")
    def nm(p):
        if isinstance(p, str): return p
        name = p.get("id") or p.get("Name") or p.get("") or "?"
        props = p.get("properties") or {}
        return name + ("[" + ",".join("%s=%s" % kv for kv in sorted(props.items())) + "]" if props else "")
    names = [nm(p) for p in pal]
    if not data or len(pal) <= 1:
        return [names[0] if names else "?"] * 4096
    bits = max(4, (len(pal) - 1).bit_length()); per = 64 // bits; mask = (1 << bits) - 1
    out = []
    for word in data:
        w = word & 0xFFFFFFFFFFFFFFFF
        for _ in range(per):
            out.append(names[w & mask]); w >>= bits
            if len(out) == 4096: return out
    return out

def blocks(nbt):
    d = {}
    for s in nbt.get("sections", []):
        bs = s.get("block_states")
        if bs is None: continue
        y = s["Y"]; d[y - 256 if y > 127 else y] = decode_section(bs)
    return d

def block_pairs(na, nb):
    """(pairs Counter, border, interior) for one chunk pair."""
    ba, bb = blocks(na), blocks(nb)
    pairs = collections.Counter(); border = interior = 0
    for y in ba:
        if y not in bb: continue
        sa, sb = ba[y], bb[y]
        if sa == sb: continue
        for i in range(4096):
            if sa[i] != sb[i]:
                x = i & 15; z = (i >> 4) & 15
                if x in (0, 15) or z in (0, 15): border += 1
                else: interior += 1
                pairs[(sa[i], sb[i])] += 1
    return pairs, border, interior

def summary(wa, wb):
    a, b = load(wa), load(wb)
    both = set(a) & set(b)
    by_status = collections.defaultdict(lambda: [0, 0]); samples = []
    for p in sorted(both):
        st = a[p][0]; by_status[st][0] += 1
        if a[p][1] != b[p][1]:
            by_status[st][1] += 1
            if len(samples) < 8: samples.append(p)
    print("chunks: A=%d B=%d common=%d onlyA=%d onlyB=%d" % (len(a), len(b), len(both), len(a)-len(both), len(b)-len(both)))
    for st, (n, d) in sorted(by_status.items(), key=lambda x: -x[1][0]):
        print("  %-32s common=%5d differ=%5d" % (st, n, d))
    print("sample differing:", samples)

def detail(wa, wb, status, maxchunks):
    A = {p: n for p, n in chunks(wa) if n.get("Status") == status}
    B = {p: n for p, n in chunks(wb) if n.get("Status") == status}
    pairs = collections.Counter(); border = interior = 0; per_chunk = []; seen = 0
    for p in sorted(set(A) & set(B))[:maxchunks]:
        seen += 1
        pr, bo, it = block_pairs(A[p], B[p])
        border += bo; interior += it; pairs += pr
        if pr: per_chunk.append(sum(pr.values()))
    print("status %s: compared %d chunks, %d differ; differing blocks: border %d interior %d" % (status, seen, len(per_chunk), border, interior))
    if per_chunk: per_chunk.sort(); print("  blocks per differing chunk: median %d max %d" % (per_chunk[len(per_chunk)//2], per_chunk[-1]))
    for (a, b), c in pairs.most_common(8): print("  %6d %s -> %s" % (c, a.replace("minecraft:", ""), b.replace("minecraft:", "")))

if __name__ == "__main__":
    if len(sys.argv) < 3: raise SystemExit(__doc__)
    if len(sys.argv) > 3 and sys.argv[3] == "detail":
        detail(sys.argv[1], sys.argv[2], sys.argv[4] if len(sys.argv) > 4 else "minecraft:initialize_light",
               int(sys.argv[5]) if len(sys.argv) > 5 else 200)
    else:
        summary(sys.argv[1], sys.argv[2])
