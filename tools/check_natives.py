"""Check built natives for network and process imports.

    python3 tools/check_natives.py DIR_OR_FILE ... [--expect N]

Reads each ELF (.so), PE (.dll) and Mach-O (.dylib, universal or thin)
and fails when one imports a socket, DNS, exec, spawn or runtime symbol
lookup (dlopen, dlsym, GetProcAddress), links a network or shell DLL
(delay-loaded ones included), or exports anything but Java_ entry
points. ELF goes through binutils nm; PE and Mach-O are parsed here. With --expect it also fails unless exactly N natives were found.
Exit code 1 on failure. SECURITY.md describes the same check.
"""
import argparse, os, re, struct, subprocess, sys

DENY = {
    "socket", "socketpair", "connect", "bind", "listen", "accept", "accept4",
    "send", "sendto", "sendmsg", "recv", "recvfrom", "recvmsg",
    "getaddrinfo", "gethostbyname", "gethostbyname_r", "gethostbyaddr",
    "execve", "execv", "execvp", "execvpe", "execl", "execlp", "execle",
    "fork", "vfork", "posix_spawn", "posix_spawnp", "system", "popen",
    "dlopen", "dlsym", "dlvsym", "ptrace",
}
DENY_DLL = {"ws2_32.dll", "wsock32.dll", "mswsock.dll", "wininet.dll", "winhttp.dll",
            "urlmon.dll", "iphlpapi.dll", "dnsapi.dll", "shell32.dll"}
DENY_PE = re.compile(r"^(CreateProcess|WinExec|ShellExecute|LoadLibrary|GetProcAddress)")
EXT = (".so", ".dll", ".dylib")

def run(*cmd):
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout

def elf(path):
    imports = [l.split()[-1].split("@")[0] for l in run("nm", "-D", "--undefined-only", path).splitlines() if l.strip()]
    exports = [l.split()[-1] for l in run("nm", "-D", "--defined-only", path).splitlines() if l.strip()]
    return imports, exports, []

def pe(path):
    d = open(path, "rb").read()
    pe_off = struct.unpack("<I", d[0x3c:0x40])[0]
    if d[pe_off:pe_off + 4] != b"PE\0\0":
        raise ValueError("no PE signature")
    nsect, = struct.unpack("<H", d[pe_off + 6:pe_off + 8])
    opt_size, = struct.unpack("<H", d[pe_off + 20:pe_off + 22])
    opt = pe_off + 24
    wide = struct.unpack("<H", d[opt:opt + 2])[0] == 0x20b  # PE32+
    dirs = opt + (112 if wide else 96)
    sects = [struct.unpack("<4I", d[s + 8:s + 24]) for s in range(opt + opt_size, opt + opt_size + 40 * nsect, 40)]

    def off(rva):
        for vsize, va, rawsize, raw in sects:
            if va <= rva < va + max(vsize, rawsize):
                return raw + rva - va
        raise ValueError(f"rva {rva:#x} outside sections")

    def cstr(rva):
        o = off(rva)
        return d[o:d.index(b"\0", o)].decode()

    def directory(i):
        return struct.unpack("<II", d[dirs + 8 * i:dirs + 8 * i + 8])

    def thunks(rva):
        size, flag = (8, 1 << 63) if wide else (4, 1 << 31)
        o, names = off(rva), []
        while (t := int.from_bytes(d[o:o + size], "little")):
            names.append(f"#ordinal{t & 0xffff}" if t & flag else cstr((t & 0x7fffffff) + 2))
            o += size
        return names

    exports, imports, dlls = [], [], []
    rva, size = directory(0)
    if size:
        e = off(rva)
        nnames, _, names_rva = struct.unpack("<III", d[e + 24:e + 36])
        exports = [cstr(struct.unpack("<I", d[off(names_rva) + 4 * i:off(names_rva) + 4 * i + 4])[0]) for i in range(nnames)]
    for index, step, name_at, ilt_at, iat_at in ((1, 20, 12, 0, 16), (13, 32, 4, 16, 12)):  # imports, delay imports
        rva, size = directory(index)
        if not size:
            continue
        o = off(rva)
        while any(d[o:o + step]):
            name_rva, ilt, iat = (struct.unpack("<I", d[o + k:o + k + 4])[0] for k in (name_at, ilt_at, iat_at))
            dlls.append(cstr(name_rva).lower())
            imports += thunks(ilt or iat)
            o += step
    return imports, exports, dlls

def macho_slices(data):
    if data[:4] == b"\xca\xfe\xba\xbe":
        for i in range(struct.unpack(">I", data[4:8])[0]):
            _, _, off, size, _ = struct.unpack(">5I", data[8 + 20 * i:28 + 20 * i])
            yield data[off:off + size]
    else:
        yield data

def macho(path):
    data = open(path, "rb").read()
    imports, exports = [], []
    for m in macho_slices(data):
        if m[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("not a 64-bit Mach-O slice")
        p = 32
        for _ in range(struct.unpack("<I", m[16:20])[0]):
            cmd, size = struct.unpack("<II", m[p:p + 8])
            if cmd == 2:  # LC_SYMTAB
                symoff, nsyms, stroff, _ = struct.unpack("<4I", m[p + 8:p + 24])
                for i in range(nsyms):
                    strx, typ, _, _, _ = struct.unpack("<IBBHQ", m[symoff + 16 * i:symoff + 16 * i + 16])
                    name = m[stroff + strx:m.index(b"\0", stroff + strx)].decode()
                    name = name[1:].split("$")[0] if name.startswith("_") else name
                    if typ & 0x01 and typ & 0x0e == 0x00:
                        imports.append(name)
                    elif typ & 0x01 and typ & 0x0e == 0x0e:
                        exports.append(name)
            p += size
    return imports, exports, []

def check(path):
    head = open(path, "rb").read(4)
    if head == b"\x7fELF":
        imports, exports, dlls = elf(path)
    elif head[:2] == b"MZ":
        imports, exports, dlls = pe(path)
    elif head in (b"\xca\xfe\xba\xbe", b"\xcf\xfa\xed\xfe"):
        imports, exports, dlls = macho(path)
    else:
        return [f"{path}: unknown binary format"]
    bad = [f"{path}: imports {s}" for s in sorted(set(imports)) if s in DENY or DENY_PE.match(s)]
    bad += [f"{path}: links {d}" for d in dlls if d in DENY_DLL]
    bad += [f"{path}: exports {s}" for s in sorted(set(exports)) if not s.startswith("Java_")]
    if not exports:
        bad.append(f"{path}: no exports found")
    print(f"{path}: {len(set(imports))} imports, {len(exports)} exports, {'FAIL' if bad else 'ok'}")
    return bad

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("paths", nargs="+")
    ap.add_argument("--expect", type=int)
    args = ap.parse_args()
    files = []
    for p in args.paths:
        if os.path.isdir(p):
            files += sorted(os.path.join(d, f) for d, _, fs in os.walk(p) for f in fs if f.endswith(EXT))
        else:
            files.append(p)
    bad = [b for f in files for b in check(f)]
    if args.expect is not None and len(files) != args.expect:
        bad.append(f"found {len(files)} natives, expected {args.expect}")
    for b in bad:
        print("FAIL", b)
    sys.exit(1 if bad else 0)

if __name__ == "__main__":
    main()
