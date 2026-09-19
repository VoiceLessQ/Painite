# Painite

Chunk generation for Minecraft 26.3 that uses the cores you paid for.

Here is the thing nobody tells you about a modern server: vanilla
builds most of every chunk on one thread, one chunk at a time. You can
have 24 threads and a fast disk, and the moment someone teleports
into fresh terrain, 20 of those threads sit there watching one of them
work. Painite fixes that. Feature placement runs on a pool of workers,
and the heavy terrain math lives in a native library that does the
same job for about half the CPU. New terrain shows up around 2.5
times faster on the machine we measure on.

## What it actually changes

Two things, both on by default.

**Features go parallel.** Trees, ores, cave decoration, all the stuff
vanilla places after the terrain exists, now run on several workers
at once. A small gate makes sure two workers never write to the same
area at the same time. That is the whole trick, and it is enough.

**Terrain goes native.** Noise, density, aquifers, surface rules,
carvers and biome sampling run in a native library instead of Java.
Same output. We check it bit for bit against vanilla on a set of
oracle chunks, and it has matched on every release.

## Numbers

All from one box: Ryzen 9 5900X (12 cores), JDK 25, 3 GB heap, fresh
world each run, 1024 chunks force-loaded at once after two warm-up
sets. Your numbers will differ. The ratio should not, much.

| configuration | chunks per second | CPU ms per chunk |
|---|---:|---:|
| vanilla | 238 | 56.2 |
| Painite, 4 workers | 598 to 622 | 28 to 30 |

One honest caveat. A single spectator flying at the speed cap with
view distance 32 gets chunks at the same rate either way, because the
game itself caps how fast it asks. What changes there is cost: about
16 ms of CPU per chunk instead of 35. The speedup shows up the moment
hundreds of chunks are wanted at once, which is what happens when
someone joins, someone teleports, three people wander off in different
directions, or you pregenerate a map. Those are the moments a server
falls behind. That is what this is for.

The full protocol and the thread tables exist; open an issue if you
want to argue with the method and we will post them.

## What the worlds look like

Normal vanilla worlds, with identical terrain. Features near chunk
borders are a different story, and one worth understanding before you
ask. Which side of a border gets its trees first depends on which
neighbour was generated first, in vanilla as much as here. The same
seed generated twice on an unmodified server already lands a few
hundred blocks of moss, dripstone, leaves and snow in different spots.
Painite widens that spread by roughly 1.5x. No new kind of difference,
though. A test in tools/ fails if that ever changes.

## Requirements

- Minecraft 26.3, Fabric Loader 0.18.4 or newer, Fabric API
- Java 25
- Windows x64, Linux x64, Linux ARM64 or macOS (Intel and Apple
  silicon). Anything else gets a warning in the log and plain vanilla
  generation. Nothing breaks, you just do not get the speed.

Install it on the server, or in your singleplayer instance if that
is where you play; players joining a server that has it do not need
it installed themselves.

## Knobs

There is no config file; we did not want one yet. JVM flags cover
A/B testing and the odd misbehaving worldgen mod:

```
-Dpainite.parallelFeatures=false   # serial feature placement, as vanilla
-Dpainite.rustTerrain=false        # vanilla terrain stage
-Dpainite.workers=4                # feature workers, default cores minus one (max 16); 4 measured as the knee
```

`/painite probe report` prints where the generation time went on a
running server. `/painite probe reset` clears it so you can measure a
clean window.

## Other mods

Best effort, said plainly. Datapack biomes and features are fine.
A datapack carver with a config the native stage does not recognise
gets vanilla carving for that world, and the log tells you so with
the words "carvers vanilla". Mods that swap out the chunk generator
entirely get no speedup at all. If something else goes wrong,
open an issue with the mod list and the log; that is usually enough
to find it.

## License

MIT. The noise and density code started from
[Ferrite](https://github.com/VoiceLessQ/Ferrite), also MIT. Details in
LICENSES.md.
