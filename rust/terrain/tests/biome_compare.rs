//! The biome stage against the quart grids vanilla generated (the inner
//! 4x4 columns of each surface oracle dump). Needs `PAINITE_WG_ROOT`
//! with a `biome_parameters/overworld.json` document and
//! `PAINITE_SURFACE_ORACLE`.

use std::path::Path;
use std::time::Instant;

use painite_terrain::climate26::BiomeStage;
use painite_terrain::df26::Loader;
use painite_terrain::oracle::SurfaceOracle;
use painite_terrain::state26::TerrainState;

fn biome_zoom_seed(seed: i64) -> i64 {
    // BiomeManager.obfuscateSeed: sha256 of the little-endian long, first 8 bytes little-endian.
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.to_le_bytes());
    i64::from_le_bytes(digest[..8].try_into().unwrap())
}

/// The grid the native assembles from its kept biome output against the
/// 6x6 grid vanilla read for the surface pass: neighbour lookup, index
/// order and the miss path.
#[test]
fn native_grid_matches_vanilla() {
    let (Ok(root), Ok(paths)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_SURFACE_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_SURFACE_ORACLE not set, skipping");
        return;
    };
    let mut failures = 0;
    for path in paths.split(',') {
        let oracle = SurfaceOracle::load(Path::new(path)).expect("surface oracle");
        let mut loader = Loader::new(Path::new(&root), oracle.seed);
        let state = TerrainState::build_from_loader(&mut loader, biome_zoom_seed(oracle.seed), "minecraft:overworld").expect("terrain state");
        let biome_ids = loader.ids_of("biome").expect("biome ids");
        let (cx, cz) = (oracle.chunk_x, oracle.chunk_z);
        assert!(state.native_grid(cx, cz).is_none(), "grid before any biome stage ran");
        for dz in -1..=1 {
            for dx in -1..=1 {
                if (dx, dz) != (1, 1) {
                    state.chunk_biomes(cx + dx, cz + dz).expect("biome stage");
                }
            }
        }
        assert!(state.native_grid(cx, cz).is_none(), "grid with one neighbour missing");
        state.chunk_biomes(cx + 1, cz + 1).expect("biome stage");
        let grid = state.native_grid(cx, cz).expect("grid with all nine chunks");
        let q = oracle.quart_count;
        assert_eq!(grid.size_y, q);
        assert_eq!(grid.data.len(), oracle.quarts.len());
        let mut mismatch = 0;
        for (i, (&mine, &theirs)) in grid.data.iter().zip(&oracle.quarts).enumerate() {
            if biome_ids[mine as usize] != oracle.biomes[theirs as usize] {
                if mismatch < 6 {
                    println!("  quart {} ({},{},{}) want {} got {}", i, (i / q) % 6, i % q, i / q / 6, oracle.biomes[theirs as usize], biome_ids[mine as usize]);
                }
                mismatch += 1;
            }
        }
        println!("{path}: {mismatch} of {} grid quarts differ", grid.data.len());
        if mismatch > 0 {
            failures += 1;
        }
    }
    assert_eq!(failures, 0);
}

#[test]
fn biomes_match_vanilla() {
    let (Ok(root), Ok(paths)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_SURFACE_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_SURFACE_ORACLE not set, skipping");
        return;
    };
    let mut failures = 0;
    for path in paths.split(',') {
        let oracle = SurfaceOracle::load(Path::new(path)).expect("surface oracle");
        let mut loader = Loader::new(Path::new(&root), oracle.seed);
        let biome_ids = loader.ids_of("biome").expect("biome ids");
        let stage = BiomeStage::load(&mut loader, &biome_ids, oracle.min_y, oracle.height).expect("biome stage");
        let t0 = Instant::now();
        let ours = stage.chunk_biomes(oracle.chunk_x, oracle.chunk_z);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let mut best = painite_terrain::climate26::BiomeStats { noise_ms: f64::MAX, search_ms: f64::MAX };
        for _ in 0..5 {
            let (_, st) = stage.chunk_biomes_stats(oracle.chunk_x, oracle.chunk_z);
            if st.noise_ms + st.search_ms < best.noise_ms + best.search_ms {
                best = st;
            }
        }
        println!("  best of 5: noise {:.3} ms, search {:.3} ms", best.noise_ms, best.search_ms);
        let tree_mismatch = stage.cross_check_search(oracle.chunk_x, oracle.chunk_z, 20_000);
        println!("  tree vs column search: {tree_mismatch} of 20000 random lookups differ");
        assert_eq!(tree_mismatch, 0, "column search differs from the tree search");
        let q = oracle.quart_count;
        let mut mismatch = 0;
        let mut first = Vec::new();
        for z in 0..4 {
            for x in 0..4 {
                for y in 0..q {
                    let theirs = &oracle.biomes[oracle.quarts[y + ((x + 1) + (z + 1) * 6) * q] as usize];
                    let mine = &biome_ids[ours[y + (x + z * 4) * q] as usize];
                    if theirs != mine {
                        mismatch += 1;
                        if first.len() < 6 {
                            first.push(format!("quart ({x},{y},{z}) want {theirs} got {mine}"));
                        }
                    }
                }
            }
        }
        println!("{path}: {ms:.2} ms, {mismatch} of {} quarts differ", 16 * q);
        for l in &first {
            println!("  {l}");
        }
        if mismatch > 0 {
            failures += 1;
        }
    }
    assert_eq!(failures, 0);
}
