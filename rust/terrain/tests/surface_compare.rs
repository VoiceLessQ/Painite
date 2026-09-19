//! Bit-exact comparison of the surface pass against a chunk vanilla
//! generated: every block, both worldgen heightmaps and the
//! post-processing set.
//!
//! Needs `PAINITE_WG_ROOT` and `PAINITE_SURFACE_ORACLE` (one file or a
//! comma-separated list written by `/painite density surface <x> <z>`),
//! otherwise the test is skipped.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use painite_terrain::df26::Loader;
use painite_terrain::oracle::SurfaceOracle;
use painite_terrain::beard26::Beardifier;
use painite_terrain::state26::{pack_chunk, TerrainState};
use painite_terrain::lod26::ColumnLod;
use painite_terrain::surface26::{build_surface_stats, ChunkBlocks, Palette, QuartGrid, SurfaceConfig, FLAG_AIR, FLAG_FLUID, FLAG_MOTION_BLOCKING};

/// The flags the game would send, derived from names for the test.
fn flags_for(name: &str) -> u8 {
    if name.contains("\"minecraft:air\"") {
        FLAG_AIR
    } else if name.contains("\"minecraft:water\"") || name.contains("\"minecraft:lava\"") {
        // blocks_motion_in_heightmap does not list fluids (oracle floor heights).
        FLAG_FLUID
    } else {
        FLAG_MOTION_BLOCKING
    }
}

fn biome_zoom_seed(seed: i64) -> i64 {
    // BiomeManager.obfuscateSeed: sha256 of the little-endian long, first 8 bytes little-endian.
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.to_le_bytes());
    i64::from_le_bytes(digest[..8].try_into().unwrap())
}

/// The far-view record is a pure function of the surfaced chunk: two
/// builds agree, and every column matches a top-down scan of the blocks.
fn check_lod(cfg: &SurfaceConfig, chunk: &ChunkBlocks, quarts: &QuartGrid) {
    let t0 = Instant::now();
    let lod = ColumnLod::from_chunk(cfg, chunk, quarts);
    let lod_ms = t0.elapsed().as_secs_f64() * 1e3;
    assert_eq!(lod, ColumnLod::from_chunk(cfg, chunk, quarts));
    let min_y = chunk.vol.min[1];
    let top_y = min_y + chunk.vol.size[1] as i32 - 1;
    let mut solid = 0;
    for z in 0..16 {
        for x in 0..16 {
            let col = x + z * 16;
            let mut y = top_y;
            while y >= min_y && cfg.palette.is_air(chunk.get(cfg, x, y, z)) {
                y -= 1;
            }
            let expect_height = if y < min_y { min_y } else { y + 1 };
            assert_eq!(i32::from(lod.height[col]), expect_height, "height at {x},{z}");
            assert_eq!(lod.top[col], chunk.get(cfg, x, expect_height - 1, z), "top at {x},{z}");
            let (bx, bz) = (chunk.vol.min[0] + x as i32, chunk.vol.min[2] + z as i32);
            assert_eq!(lod.biome[col], quarts.get(bx >> 2, (expect_height - 1) >> 2, bz >> 2), "biome at {x},{z}");
            solid += usize::from(!cfg.palette.is_air(lod.top[col]));
        }
    }
    println!("lod {lod_ms:.3} ms, {solid} of 256 columns have a top block");
}

#[test]
fn surface_matches_vanilla() {
    let (Ok(root), Ok(paths)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_SURFACE_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_SURFACE_ORACLE not set, skipping");
        return;
    };
    let mut failures = 0;
    for path in paths.split(',') {
        let oracle = SurfaceOracle::load(Path::new(path)).expect("surface oracle");
        let beard = oracle.beard.as_deref().map(|flat| Beardifier::from_flat(flat).expect("beard pieces"));
        println!(
            "== {path}: chunk {} {} eligible {:?} beard {}",
            oracle.chunk_x,
            oracle.chunk_z,
            oracle.eligible,
            beard.as_ref().map_or("none".to_string(), |b| format!("{} rigid {} junctions", b.rigids.len(), b.junctions.len()))
        );
        // An ineligible dump from before the pieces were written has no input for the beard; report only.
        let expect_match = oracle.eligible != Some(false) || beard.is_some();
        let mut loader = Loader::new(Path::new(&root), oracle.seed);
        let settings = loader.noise_settings("minecraft:overworld").expect("settings");
        let biome_ids = loader.ids_of("biome").expect("biome ids");
        let mut cfg = SurfaceConfig::load(&mut loader, &settings, &biome_ids).expect("surface config");
        let flags: Vec<u8> = cfg.palette.names.iter().map(|n| flags_for(n)).collect();
        cfg.palette.set_flags(&flags).unwrap();
        let state = TerrainState::build_from_loader(&mut loader, biome_zoom_seed(oracle.seed), "minecraft:overworld").expect("terrain state");

        let t0 = Instant::now();
        let (substance, _) = state.fill_with(oracle.chunk_x, oracle.chunk_z, beard.as_ref());
        let fill_ms = t0.elapsed().as_secs_f64() * 1e3;
        let tf = Instant::now();
        let mut chunk = ChunkBlocks::from_fill(&cfg, oracle.chunk_x, oracle.chunk_z, &substance);
        let from_fill_ms = tf.elapsed().as_secs_f64() * 1e3;

        let biome_index: HashMap<&str, u16> = cfg.biomes.iter().enumerate().map(|(i, b)| (b.id.as_str(), i as u16)).collect();
        let (min_x, min_y, min_z) = QuartGrid::origin(oracle.chunk_x, oracle.chunk_z, oracle.min_y);
        assert_eq!(min_y, oracle.quart_min_y);
        let quarts = QuartGrid {
            min_x,
            min_y,
            min_z,
            size_y: oracle.quart_count,
            data: oracle.quarts.iter().map(|&q| biome_index[oracle.biomes[q as usize].as_str()]).collect(),
        };
        let t1 = Instant::now();
        let stats = build_surface_stats(&cfg, &mut chunk, &quarts, biome_zoom_seed(oracle.seed));
        let surface_ms = t1.elapsed().as_secs_f64() * 1e3;
        println!("{stats:?}");
        let tp = Instant::now();
        let packed = pack_chunk(&chunk);
        let pack_ms = tp.elapsed().as_secs_f64() * 1e3;
        println!("from_fill {from_fill_ms:.2} ms, pack {pack_ms:.2} ms ({} words)", packed.len());
        check_lod(&cfg, &chunk, &quarts);

        // Compare by canonical name: the oracle palette order is arbitrary.
        let theirs_palette: Vec<String> = oracle
            .palette
            .iter()
            .map(|n| Palette::canonical(&serde_json::from_str::<serde_json::Value>(n).expect("oracle palette json")).expect("oracle palette"))
            .collect();
        let ours_name = |id: u16| cfg.palette.names[id as usize].as_str();
        let mut block_mismatch = 0;
        let mut post_mismatch = 0;
        let verbose = std::env::var("PAINITE_SURFACE_VERBOSE").is_ok();
        let limit = if verbose { 400 } else { 12 };
        let mut first: Vec<String> = Vec::new();
        for i in 0..chunk.blocks.len() {
            let theirs = oracle.blocks[i];
            let want = theirs_palette[(theirs & 0x7fff) as usize].as_str();
            let got = ours_name(chunk.blocks[i]);
            let y = i % oracle.height as usize;
            let x = (i / oracle.height as usize) % 16;
            let z = i / oracle.height as usize / 16;
            if want != got {
                block_mismatch += 1;
                if first.len() < limit {
                    first.push(format!("block ({x},{},{z}) want {want} got {got}", oracle.min_y + y as i32));
                }
            }
            if (theirs & 0x8000 != 0) != chunk.post_process[i] {
                post_mismatch += 1;
                if first.len() < limit {
                    first.push(format!("post ({x},{},{z}) want {} got {} ({want})", oracle.min_y + y as i32, theirs & 0x8000 != 0, chunk.post_process[i]));
                }
            }
        }
        let mut height_mismatch = 0;
        for col in 0..256 {
            if chunk.surface_height[col] != oracle.surface_height[col] || chunk.floor_height[col] != oracle.floor_height[col] {
                height_mismatch += 1;
                if first.len() < limit {
                    first.push(format!(
                        "heights col ({},{}) want {}/{} got {}/{}",
                        col % 16,
                        col / 16,
                        oracle.surface_height[col],
                        oracle.floor_height[col],
                        chunk.surface_height[col],
                        chunk.floor_height[col]
                    ));
                }
            }
        }
        println!(
            "fill {fill_ms:.2} ms, surface {surface_ms:.2} ms; mismatches: blocks {block_mismatch}, post {post_mismatch}, heights {height_mismatch}"
        );
        for line in &first {
            println!("  {line}");
        }
        if block_mismatch + post_mismatch + height_mismatch > 0 && expect_match {
            failures += 1;
        }
    }
    assert_eq!(failures, 0, "{failures} oracle chunk(s) differ");
}

/// Where the ore prefill spends its time: each vein function sampled
/// alone over the prefill volume. `cargo test --release --test
/// surface_compare ore_prefill_timing -- --ignored --nocapture`.
#[test]
#[ignore]
fn ore_prefill_timing() {
    use painite_terrain::df26::{SampleCtx, SliceUniformAxes, Volume, ALL_AXES};
    let Ok(root) = std::env::var("PAINITE_WG_ROOT") else {
        eprintln!("PAINITE_WG_ROOT not set, skipping");
        return;
    };
    let mut loader = Loader::new(Path::new(&root), 259984522);
    // PAINITE_ORE_Y=lo,hi narrows the y range; PAINITE_ORE_SNIPPETS names a
    // file of JSON lines (ids or inline nodes) timed instead of the list below.
    let (y_lo, y_hi): (i32, i32) = match std::env::var("PAINITE_ORE_Y") {
        Ok(s) => {
            let (a, b) = s.split_once(',').expect("PAINITE_ORE_Y=lo,hi");
            (a.parse().unwrap(), b.parse().unwrap())
        }
        Err(_) => (-64, 80),
    };
    let vol = Volume::new([16, (y_hi - y_lo) as usize, 16], [374 * 16, y_lo, 46 * 16], [1, 1, 1]);
    let reps = 20;
    let from_file: Vec<String> = std::env::var("PAINITE_ORE_SNIPPETS")
        .ok()
        .map(|p| std::fs::read_to_string(p).expect("snippet file").lines().map(|l| serde_json::from_str::<serde_json::Value>(l).map(|v| v.as_str().map_or(l.to_string(), |s| s.to_string())).expect("snippet line")).collect())
        .unwrap_or_default();
    let builtin = [
        "minecraft:overworld/ore_vein/toggle",
        "minecraft:overworld/ore_vein/mask",
        "minecraft:overworld/ore_vein/richness",
        "minecraft:overworld/ore_vein/copper_density",
        "minecraft:overworld/ore_vein/iron_density",
        "minecraft:overworld/ore_vein/gap",
        r#"{"type":"minecraft:interpolated","cell_size_xz":4,"cell_size_y":8,"input":{"type":"minecraft:range_choice","input":"minecraft:y","max_exclusive":57.0,"min_inclusive":-64.0,"when_in_range":{"type":"minecraft:noise","noise":"minecraft:ore_vein_a","xz_scale":4.0,"y_scale":4.0},"when_out_of_range":1.0}}"#,
        r#"{"type":"minecraft:interpolated","cell_size_xz":4,"cell_size_y":8,"input":{"type":"minecraft:noise","noise":"minecraft:ore_vein_a","xz_scale":4.0,"y_scale":4.0}}"#,
        r#"{"type":"minecraft:interpolated","cell_size_xz":4,"cell_size_y":8,"input":{"type":"minecraft:range_choice","input":"minecraft:y","max_exclusive":57.0,"min_inclusive":-64.0,"when_in_range":{"type":"minecraft:noise","noise":"minecraft:ore_veininess","xz_scale":1.5,"y_scale":1.5},"when_out_of_range":0.0}}"#,
        r#"{"type":"minecraft:range_choice","input":"minecraft:overworld/ore_vein/toggle","max_exclusive":0.4,"min_inclusive":-0.4,"when_in_range":-1.0,"when_out_of_range":1.0}"#,
        r#"{"type":"minecraft:noise","noise":"minecraft:ore_vein_a","xz_scale":4.0,"y_scale":4.0}"#,
        r#"{"type":"minecraft:abs","input":"minecraft:overworld/ore_vein/toggle"}"#,
        r#"{"type":"minecraft:range_choice","input":"minecraft:y","max_exclusive":50.0,"min_inclusive":0.0,"when_in_range":-1.0,"when_out_of_range":1.0}"#,
        r#"{"type":"minecraft:add","left":"minecraft:overworld/ore_vein/toggle","right":-1.0}"#,
        r#"{"type":"minecraft:add","left":"minecraft:overworld/ore_vein/toggle","right":"minecraft:overworld/ore_vein/toggle"}"#,
    ];
    let ids: Vec<&str> = if from_file.is_empty() { builtin.to_vec() } else { from_file.iter().map(String::as_str).collect() };
    println!("volume y {y_lo}..{y_hi}, {} snippets", ids.len());
    for id in ids {
        let v = if id.starts_with('{') { serde_json::from_str(id).expect("snippet json") } else { serde_json::Value::String(id.into()) };
        let n = loader.parse(&v).expect(id);
        let n = SliceUniformAxes::new().rewrite(&n, ALL_AXES);
        let mut best = f64::INFINITY;
        for _ in 0..reps {
            let mut ctx = SampleCtx::default();
            let t = Instant::now();
            let out = n.sample_volume_with(&vol, &mut ctx);
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(out);
        }
        println!("{}: best of {reps} {best:.3} ms", &id[..id.len().min(90)]);
    }
}

/// Per-chunk cost of the JNI entry points (fill_pending then
/// surface_packed) from 1, 8 and 16 threads at once, to separate
/// contention from the single-thread number. `cargo test --release
/// --test surface_compare surface_threads_timing -- --ignored --nocapture`.
#[test]
#[ignore]
fn surface_threads_timing() {
    let (Ok(root), Ok(path)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_SURFACE_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_SURFACE_ORACLE not set, skipping");
        return;
    };
    let oracle = SurfaceOracle::load(Path::new(path.split(',').next().unwrap())).expect("surface oracle");
    let mut loader = Loader::new(Path::new(&root), oracle.seed);
    let settings = loader.noise_settings("minecraft:overworld").expect("settings");
    let biome_ids = loader.ids_of("biome").expect("biome ids");
    let cfg = SurfaceConfig::load(&mut loader, &settings, &biome_ids).expect("surface config");
    let mut state = TerrainState::build_from_loader(&mut loader, biome_zoom_seed(oracle.seed), "minecraft:overworld").expect("terrain state");
    // The flags the game sends through terrainSetFlags; without them every block reads as solid.
    let flags: Vec<u8> = state.surface.palette.names.iter().map(|n| flags_for(n)).collect();
    state.surface.palette.set_flags(&flags).unwrap();
    let biome_index: HashMap<&str, u16> = cfg.biomes.iter().enumerate().map(|(i, b)| (b.id.as_str(), i as u16)).collect();
    let quarts: Vec<u16> = oracle.quarts.iter().map(|&q| biome_index[oracle.biomes[q as usize].as_str()]).collect();
    let per_thread = 24;
    for threads in [1usize, 8, 16] {
        let t0 = Instant::now();
        std::thread::scope(|s| {
            for t in 0..threads {
                let (state, quarts) = (&state, &quarts);
                s.spawn(move || {
                    for k in 0..per_thread {
                        let (cx, cz) = (oracle.chunk_x + (t * 8) as i32, oracle.chunk_z + k as i32);
                        let tc = Instant::now();
                        state.fill_pending(cx, cz, None);
                        let tf = tc.elapsed().as_secs_f64() * 1e3;
                        let words = state.surface_packed(cx, cz, Some(quarts.clone()), false).expect("surface");
                        std::hint::black_box(words);
                        if threads == 1 {
                            println!("  chunk {cx},{cz}: fill {tf:.2} ms, surface {:.2} ms", tc.elapsed().as_secs_f64() * 1e3 - tf);
                        }
                    }
                });
            }
        });
        let wall = t0.elapsed().as_secs_f64() * 1e3;
        println!("{threads} threads: {:.2} ms per chunk per thread ({} chunks, {wall:.0} ms wall)", wall / per_thread as f64, threads * per_thread);
    }
}

/// Cost of the zoomed biome lookup over every block of the oracle
/// chunk, and how many hit the uniform-corner shortcut.
#[test]
#[ignore]
fn zoomed_biome_timing() {
    use painite_terrain::surface26::zoomed_biome;
    let (Ok(root), Ok(path)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_SURFACE_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_SURFACE_ORACLE not set, skipping");
        return;
    };
    let oracle = SurfaceOracle::load(Path::new(path.split(',').next().unwrap())).expect("surface oracle");
    let mut loader = Loader::new(Path::new(&root), oracle.seed);
    let settings = loader.noise_settings("minecraft:overworld").expect("settings");
    let biome_ids = loader.ids_of("biome").expect("biome ids");
    let cfg = SurfaceConfig::load(&mut loader, &settings, &biome_ids).expect("surface config");
    let biome_index: HashMap<&str, u16> = cfg.biomes.iter().enumerate().map(|(i, b)| (b.id.as_str(), i as u16)).collect();
    let (min_x, min_y, min_z) = QuartGrid::origin(oracle.chunk_x, oracle.chunk_z, oracle.min_y);
    let grid = QuartGrid { min_x, min_y, min_z, size_y: oracle.quart_count, data: oracle.quarts.iter().map(|&q| biome_index[oracle.biomes[q as usize].as_str()]).collect() };
    let seed = biome_zoom_seed(oracle.seed);
    let (bx, bz) = (oracle.chunk_x * 16, oracle.chunk_z * 16);
    let mut uniform = 0usize;
    let mut total = 0usize;
    for x in 0..16 {
        for z in 0..16 {
            for y in -64..80 {
                let (px, py, pz) = ((bx + x - 2) >> 2, (y - 2) >> 2, (bz + z - 2) >> 2);
                let first = grid.get(px, py, pz);
                if (1..8).all(|i| grid.get(px + ((i >> 2) & 1), py + ((i >> 1) & 1), pz + (i & 1)) == first) {
                    uniform += 1;
                }
                total += 1;
            }
        }
    }
    let mut best = f64::INFINITY;
    for _ in 0..10 {
        let t = Instant::now();
        let mut acc = 0u64;
        for x in 0..16 {
            for z in 0..16 {
                for y in -64..80 {
                    acc += zoomed_biome(seed, &grid, bx + x, y, bz + z) as u64;
                }
            }
        }
        std::hint::black_box(acc);
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("zoomed_biome over {total} blocks: {best:.3} ms, uniform {uniform} ({:.0}%)", uniform as f64 * 100.0 / total as f64);
    let mut best = f64::INFINITY;
    let mut mismatches = 0;
    for _ in 0..10 {
        let mut cache = painite_terrain::surface26::FiddleCache::new(&grid);
        let t = Instant::now();
        let mut acc = 0u64;
        for x in 0..16 {
            for z in 0..16 {
                for y in -64..80 {
                    acc += cache.zoomed_biome(seed, &grid, bx + x, y, bz + z) as u64;
                }
            }
        }
        std::hint::black_box(acc);
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    for x in 0..16 {
        for z in 0..16 {
            let mut cache = painite_terrain::surface26::FiddleCache::new(&grid);
            for y in -64..320 {
                if cache.zoomed_biome(seed, &grid, bx + x, y, bz + z) != zoomed_biome(seed, &grid, bx + x, y, bz + z) {
                    mismatches += 1;
                }
            }
        }
    }
    println!("cached (fresh cache per rep): {best:.3} ms, mismatches vs direct over the full height: {mismatches}");
    assert_eq!(mismatches, 0);
}
