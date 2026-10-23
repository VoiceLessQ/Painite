# Security

Painite bundles a native library written in Rust, which most Minecraft
mods don't. This page says what that library does, what it can't do,
and how to check a jar yourself instead of taking our word for it.

Painite grew out of [Ferrite](https://github.com/VoiceLessQ/Ferrite)
and shares its build, loading and release setup. What differs is how
much of the mod lives in the native: about four fifths of Painite's
code is Rust, against a third of Ferrite's. The Java side is mostly
mixins that hand work to the library. So the library is the part worth
checking, and most of this page is about it.

## What the native library does

The jar carries prebuilt libraries under `assets/painite/natives/`:
`windows/painite.dll` and `painite_v3.dll`, `linux/libpainite.so` and
`libpainite_v3.so`, `linux-aarch64/libpainite.so`, and
`macos/libpainite.dylib`. At startup `PainiteNative.java` copies the one
that fits your OS and CPU to a temp file, marks it for deletion on exit
and loads it with `System.load`. The `_v3` builds load only when the
JVM reports AVX2, FMA and BMI2. On any other platform the mod logs a
warning and you get vanilla generation.

Most of what the native does is arithmetic: noise, density functions,
aquifers, surface rules, carvers, biome lookup, and the gate that keeps
feature workers out of each other's way. Numbers in, blocks out.

It has no network access and starts no processes. CI enforces that
twice. Before anything compiles, the `build` workflow fails if
`std::net`, sockets or `Command::new` show up in the Rust sources, if
`java.net`, `ProcessBuilder` or `Runtime.exec` show up in the Java
sources, or if an HTTP or TLS crate lands in `Cargo.lock`. Then, before
the jar is packed, `tools/check_natives.py` reads all six built
libraries and fails if one imports a socket, DNS, exec or spawn
function, or a runtime lookup (`dlopen`, `dlsym`, `GetProcAddress`)
that could fetch one later, links a network or shell DLL on Windows
(delay-loaded ones too), or exports anything other than the JNI entry
points.

The binary check matters more than the source check, because code
that opens a socket has to ask the OS for one, and the import table
lists every function a library asks for by name. It isn't proof. Code
written to hide could still make raw system calls (the Rust standard
library on Linux imports `syscall` for its own thread locking). But
hiding that way takes deliberate machine-level work in the source,
and anyone who reads the source or a disassembly will spot it.

Files are the one place it touches the system, and only for the far
view, which is off unless you start with `-Dpainite.lod=true`. With it
on, the server writes region files under `<world>/painite/lod` and a
modded client keeps its own copy under `<game dir>/painite/lod`. The
far view also sends those records to modded clients, over the game's
own connection. With the flag off, none of that runs.

Two smaller things. The operator command that dumps biome parameters
writes `painite_biome_parameters.json` to the server directory when
someone runs it. And a panic inside the native is caught at the JNI
boundary: the call declines and that chunk falls back to vanilla
instead of taking the JVM down.

`unsafe` in the Rust code covers the `#[unsafe(no_mangle)]` JNI exports,
two buffer copies at the JNI boundary, and AVX2 intrinsics in the noise
code that run only after a CPU feature check. Direct dependencies are
`jni`, `serde_json` and `md-5` plus the crates in this repo; the locked
tree is in `Cargo.lock`.

## How the jar is built

GitHub Actions assembles every release jar from the tagged commit. The
`build.yml` workflow compiles the natives on GitHub-hosted runners, one
job per platform, then packs them into the jar. Nobody builds a native
on their own machine and uploads it.

The Rust toolchain is pinned in `rust-toolchain.toml` and every crate
version in `Cargo.lock`. Builds still aren't byte-for-byte reproducible
across runner images, so a native you build locally can differ from the
released one while doing the same work.

## Checking it yourself

Releases carry a `SHA256SUMS` file and a signed build provenance
attestation. With the GitHub CLI installed:

```
gh attestation verify painite-<version>.jar --owner VoiceLessQ
```

A pass means that exact jar came out of `build.yml` in this repository
at the tagged commit. A hand-built jar doesn't pass. Releases before
0.2.0-alpha have no attestation; for those, compare the jar's SHA-256
against the asset on the GitHub release page.

### Check the libraries in a jar

This works on a release jar or on one you built. A jar is a zip, so
any zip tool opens it; then point the checker at the natives folder.
It needs Python 3, plus `nm` from binutils for the Linux libraries;
it reads the Windows and macOS ones itself.

```
unzip painite-<version>.jar 'assets/painite/natives/*' -d jar
python3 tools/check_natives.py jar/assets/painite/natives
```

Each library prints its import and export counts and `ok`, or a
`FAIL` line naming the symbol. To read the tables yourself instead:

- Linux: `nm -D --undefined-only libpainite.so` lists what it asks
  the OS for (memory, files, threads, math, unwinding), and
  `nm -D --defined-only` shows only `Java_me_apika_painite_*`.
- Windows: `objdump -p painite.dll` (MSYS2) or
  `dumpbin /imports painite.dll` (Visual Studio). The DLL list is `KERNEL32`, `ntdll`,
  `bcryptprimitives` and the C runtime; no `WS2_32`, `WININET` or
  `SHELL32`.
- macOS: `nm -u libpainite.dylib`, and `nm -gU` for the exports.

### Build it yourself

If you'd rather not trust our binaries at all, build your own jar and
run that. Every platform needs Git, JDK 25 and Rust through
[rustup](https://rustup.rs/); `rust-toolchain.toml` picks the stable
toolchain and its targets on the first build.

```
git clone https://github.com/VoiceLessQ/Painite.git
cd Painite
git checkout v<version>
```

**Linux** (x86-64 or ARM64): install GCC (`build-essential` on Debian
and Ubuntu, `gcc` on Fedora and Arch), then

```
./gradlew build
```

**Windows**: the native uses the GNU toolchain, so install
[MSYS2](https://www.msys2.org/) to `C:\msys64` and run
`pacman -S mingw-w64-x86_64-gcc` in its terminal. Then, in PowerShell:

```
.\gradlew.bat build
```

`.cargo/config.toml` expects gcc at `C:/msys64/mingw64/bin`. If yours
lives elsewhere, set `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER` and
`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_AR` to your `gcc.exe` and `ar.exe`
first, the way CI does.

**macOS**: install the Xcode command line tools
(`xcode-select --install`), then `./gradlew build`. That builds the
Apple silicon native only. On an Intel Mac, build the native with cargo
and let Gradle pack it:

```
rustup target add x86_64-apple-darwin
cargo build --release --target x86_64-apple-darwin --manifest-path=rust/mod/Cargo.toml
mkdir -p src/main/resources/assets/painite/natives/macos
cp target/x86_64-apple-darwin/release/libpainite.dylib src/main/resources/assets/painite/natives/macos/
./gradlew build -x buildRustLib -x copyRustDll
```

The jar lands in `build/libs/` and carries only your platform's native
(on x86-64 Linux and Windows, also the `_v3` build). Release jars carry
all six from 0.2.0-alpha on; 0.1.0-alpha had four, without the `_v3`
builds. `cargo test --workspace --release` runs the same Rust tests CI
runs.

Your build won't be byte-identical to ours, and its import list can
differ by a symbol or two when your Rust version is newer than the one
we released with. What should match is the shape: the same `Java_`
exports as the release, and no network or process imports.

Get Painite from this repository's releases page. That's the only
place we publish it.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository
(Security tab, "Report a vulnerability"), not a public issue, for
anything exploitable. Expect a reply within a week.

## Supported versions

Only the latest release gets fixes.
