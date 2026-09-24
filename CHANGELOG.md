# Changelog

All notable changes to Painite are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
follow [Semantic Versioning](https://semver.org/); the `-alpha` suffix
marks pre-release builds.

Every number below was measured on one machine: Ryzen 9 5900X (12
cores, 24 threads), JDK 25, 3 GB heap, MC 26.3-pre-1, seed
`painite-smoke-1`, fresh world per run. The burst is 1024 chunks
force-loaded at once after two warm-up sets; the flight is 60 strips
of 2x21 chunks at one per second.

## [Unreleased]

### Changed

- Licence is now GPL-3.0-only instead of MIT. Modified builds and
  mods built on Painite's code must publish their source. The Ferrite
  code in rust/terrain keeps its MIT notice.
- Far-view records are only built when `-Dpainite.lod=true`. Before,
  every native chunk kept one in memory anyway, up to about 600 MB.
- Noise sample counters are per thread, so workers no longer contend
  on one shared counter.
- CI runs `cargo test` and lints test code too.

### Fixed

- The native terrain stage only serves the overworld it was built for.
  Other dimensions that reuse the overworld settings got its biomes and
  seed.
- A dimension whose height differs from its noise settings now stays on
  vanilla. Before, it crashed every chunk or shifted terrain 64 blocks.
- A rejected native chunk falls back to vanilla's fill instead of
  throwing "native fill lost".
- A panic in the native no longer kills the JVM; the call declines and
  the chunk goes vanilla.
- `pow` with exponent -1 gave x instead of 1/x.
- `default_fluid` in the 26.3 format was ignored and always read as
  water.
- Far-view records refreshed from finished chunks sat one block low
  and showed the block under the surface.

## [0.1.0-alpha] - 2026-09-19

### Changed

- **Features gate and native terrain stage are now default on.**
  Both were opt-in since the first builds. Kill switches:
  `-Dpainite.parallelFeatures=false` and `-Dpainite.rustTerrain=false`.
  Evidence, same session, 2026-09-08 and 09-09:

  | configuration | chunks/s to FULL | CPU ms per chunk |
  |---|---:|---:|
  | vanilla serial | 238 | 56.2 |
  | native terrain | 260 | 42.8 |
  | both, 4 workers | 501 to 552 | 37.6 to 39.4 |

  Flight (demand-bound at 48 chunks/s in every row): vanilla 33.0,
  native terrain 25.7, both with 4 workers 27.7 CPU ms per full chunk.
  Output: the native terrain stage is bit-exact against the vanilla
  code on ten oracle chunks (fill, surface, biomes, structure terrain
  adaptation); the features gate widens vanilla's own border-order
  spread by 1.3 to 1.6x with no new block-pair class (tools/spread.py).

- **Native terrain stage, five throughput passes.** Output unchanged:
  every pass is bit-exact on the ten oracle chunks and the order-spread
  test passes. Busy
  timers over the same 1024-chunk burst, 4 workers, same session:

  | stage timer | before | after |
  |---|---:|---:|
  | surface pass | 6.17 s | 5.27 s |
  | fused fill | 9.88 s | 9.48 s |
  | biome stage | 1.69 s | 1.19 s |
  | quart grid read (Java) | 0.42 s | 0 |
  | burst, CPU ms per chunk | 35.8 | 34.0 |
  | chunks/s to FULL | 571 | 600 |

  What changed: ore vein richness is sampled only where vein density
  is positive and a range choice with one answer skips its mask; the
  surface column walk drops vein ops from bands they cannot reach and
  runs constant deep bands as a plain loop; a lerp side is sampled
  only where the alpha does not saturate; the aquifer nearest-cell
  search is branchless; the biome parameter tree is flattened with a
  per-column memo of the fixed-dimension distances; and the 6x6 biome
  quart grid the surface and ore passes need is assembled in the
  native from its own biome output instead of read from the game per
  quart (a miss falls back to the game read, counted as
  `gridFallback` in the probe report).

- **Carvers run in the native terrain stage.** Caves and canyons are
  walked into a carving mask and applied on the surfaced chunk in the
  same native call as the surface pass, with the fill's aquifer
  deciding air, water or lava and the surface rule re-topping dirt
  that lost its grass. Bit-exact against vanilla on ten fresh oracle
  chunks, both as a whole chunk and as the mask applied to vanilla's
  own surfaced blocks; the order-spread test passes. Same burst
  protocol, one session, before and after:

  | stage timer | before | after |
  |---|---:|---:|
  | carvers (Java) | 2.10 s | 0 |
  | surface pass (now including the carve) | 5.39 s | 6.24 s |
  | burst, CPU ms per chunk | 34.1 | 32.4 to 33.5 |
  | chunks/s to FULL | 598 | 605 to 613 |

  Kill switch: `-Dpainite.rustCarvers=false`. A datapack carver using
  a value provider the port does not have leaves the carvers to the
  game for that world (the load log says `carvers vanilla`).

- **Corner grids stop where the terrain is provably air.** An interval
  bound of the main density per corner column finds the row above
  which every corner is negative; above it and above the aquifer's
  sampling height the fill reads no noise, interpolates nothing and
  places the sea-level fluid or air per block. Substance bytes are
  identical to the full path on the chunk oracle and on twelve chunks
  around it; the surface and carver oracles are unchanged and the
  order-spread test passes. Same burst protocol, one session:

  | stage timer | before | after |
  |---|---:|---:|
  | fused fill | 10.84 s | 7.81 to 7.90 s |
  | burst, CPU ms per chunk | 33.5 | 30.2 to 31.2 |
  | chunks/s to FULL | 605 | 598 to 622 |

- **Ore placement reuses the chunk between heightmap probes.** An ore
  feature starts with up to 81 heightmap reads around its origin and
  63% of the calls in a burst place nothing after them; every read went
  through the region's chunk lookup. The region now keeps the chunk it
  last read and answers repeat probes from it (same object, same
  value, random stream untouched). Kill switch:
  `-Dpainite.oreProbe=false`. Two on/off pairs, same burst protocol:

  | stage timer | off | on |
  |---|---:|---:|
  | ore placement | 2.50 to 2.70 s | 2.17 to 2.21 s |
  | features job mean | 2.98 to 3.15 ms | 2.85 to 2.86 ms |

  Order-spread test passes on both on-worlds.

- **Corner noise: lattice corners read from a per-column table, 8 wide.**
  The AVX2 `addToVolume` port kept its lattice refresh scalar: twelve
  permutation reads and eight gradient dot products per lane, then an
  8x8 transpose of the results into lane vectors. The column now
  builds one 16-entry dot table per corner offset and each lane only
  walks the permutation to a gradient index; the corner values come
  from that table with `permutevar` on both halves. A group of eight
  y points inside one lattice cell takes sixteen broadcasts instead.
  Bit-exact against the scalar path (six volumes), the density chunk
  oracle, and the ten surface and carved oracle chunks. Same session,
  release build, one thread, then fr4 bursts (1024 chunks):

  | measure | before | after |
  |---|---:|---:|
  | Perlin leaf, fine y scale, ns per point | 5.6 to 5.9 | 3.6 to 3.7 |
  | Perlin leaf, cell per point, ns per point | 12.1 to 12.2 | 7.2 to 7.3 |
  | fused fill, steady state, ms per chunk | 2.43 to 2.48 | 2.22 to 2.24 |
  | burst terrain.fill.total, s | 7.65 to 7.80 | 6.97 to 7.02 |
  | burst CPU ms per chunk | 30.1 to 30.7 | 28.8 to 29.9 |

- **Ore vein prefill: a `range_choice` samples each branch only over
  the rows that read it.** The volume evaluator used to sample the
  input, count the points in range, scan the rows for a clean split,
  fill both constant branches and select; five passes over the buffer
  for two constants. Now one pass records which rows read which
  branch, a `minecraft:y` input needs no buffer at all, a constant
  branch is written straight over the other side, and a sampled
  branch covers only its rows (widened to whole cells, runs under two
  cells apart merged). `cache` nodes hand a sub-volume its rows by
  copy from a cached volume that contains it, so the vein toggle is
  sampled once per chunk. Exact on the density, chunk, surface, carved
  and biome oracles. Same session, release build, one thread over the
  ten oracle chunks, then two fr4 bursts against two from before:

  | measure | before | after |
  |---|---:|---:|
  | ore prefill, ms per chunk | 0.28 to 0.68 | 0.18 to 0.42 |
  | surface pass, ms per chunk | 0.76 to 1.13 | 0.70 to 0.95 |
  | burst terrain.surface.native, s | 6.26 to 6.58 | 5.84 to 5.90 |
  | burst CPU ms per chunk | 28.8 to 29.9 | 28.2 to 28.7 |

### Added

- **Native TERRAIN stage** (`rust/terrain`): density tree and the 26.3
  noise stack in f32, fused fill with aquifer, surface rules folded
  per band, biome sampling, structure terrain adaptation, packed
  section writeback. Corner grid noise runs on AVX2 lanes when the CPU
  has them, scalar otherwise, same bits either way.
- **Features gate** (`rust/sched`): FEATURES bodies run on a worker
  pool, admitted when no other running job writes within two chunks.
  Two native calls per chunk stage.
- **Probes**: `/painite probe report` and `reset` print per-stage,
  per-status, dispatcher, light and busy timers; the same report logs
  on server stop.
- **Drivers** in `tools/`: `ab.sh` burst and flight protocol,
  `rcon.py`, `spread.py` and `regiondiff.py` for the order-spread
  test, `oracle_dump.sh` for fresh oracle chunks, `jfr_split.py`.
- **Dev run properties** in build.gradle: `-Ppainite.<flag>=true|false`,
  `-Ppainite.workers=N`, `-Ppainite.heap`, `-Ppainite.vmArgs`,
  `-Ppainite.jfr`, `-Ppainite.quickPlay=<save>`.
- **Far view** (off by default, `-Dpainite.lod=true`
  turns it on): the native surface pass writes a column record per
  chunk under `<world>/painite/lod`; the server streams records to
  modded clients out to `-Dpainite.lodRadius`, the client keeps them
  on disk and draws region meshes built in Rust past the render
  distance, lit through the game's lightmap and fogged out to the far
  plane. Rejoin sends only records the client lacks (5871 on first
  join, 0 on the repeat, radius 96). Client cost at view 32 on a 4090,
  854x480: GPU 29 to 23% after coarse outer rings and greedy tops,
  4.1 to 2.65 M vertices per frame.
- **Feature probe** (`-Dpainite.featureProbe`): busy time per placed
  feature, printed with the probe report. Ores are 2.2 s of a 1600
  chunk burst, the other 75 features are flat (largest 0.45 s).
- **Flight strips** in `ab.sh`: `FLY_STEP` chunks per strip and a
  demand line against the full rate. At the spectator speed cap (5
  chunks/s, view 32, 325 chunks/s demand) vanilla and the default
  build both keep up at 345 full chunks/s; CPU per full chunk 34.6
  vs 16.3 ms, mean busy cores 12.1 vs 5.7.
- `tools/jfr_alloc.py`: allocation sites from a `jfr print` text
  dump. Burst bytes are 99% vanilla's (light section-map clone 23%,
  feature BlockPos churn ~25%).

### Kept off

- `-Dpainite.rustOres`: bit-exact but slower (features 2.58 to
  3.01 ms per chunk, 207 vs 229 chunks/s); the ore loop is packed
  reads HotSpot already runs at the floor.
- `-Dpainite.fastPack`: output identical, no measured win.
- `-Dpainite.parallelStructures`: the stages it gates cost 0.13 ms
  per chunk together, nothing to gain.
- `-Dpainite.lod`: the far view works but stays off. Distant Horizons
  covers the same need; the code is kept for the record format and
  the client store.

[Unreleased]: https://github.com/VoiceLessQ/Painite/compare/v0.1.0-alpha...HEAD
[0.1.0-alpha]: https://github.com/VoiceLessQ/Painite/releases/tag/v0.1.0-alpha
