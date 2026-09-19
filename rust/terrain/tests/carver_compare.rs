//! Bit-exact comparison of the carver port against a chunk vanilla
//! carved: every block, both worldgen heightmaps and the post-processing
//! set, once on the fully native chunk (fill, surface, carve) and once
//! with the mask applied to vanilla's own surfaced blocks, which
//! isolates the carve from the passes before it.
//!
//! Needs `PAINITE_WG_ROOT`, `PAINITE_SURFACE_ORACLE` and
//! `PAINITE_CARVED_ORACLE` (comma-separated lists in the same order,
//! written together by `/painite density surface <x> <z>`), otherwise
//! the test is skipped.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use painite_terrain::beard26::Beardifier;
use painite_terrain::df26::{Loader, Volume};
use painite_terrain::oracle::SurfaceOracle;
use painite_terrain::state26::TerrainState;
use painite_terrain::surface26::{build_surface_carved, carve_only, CarveJob, ChunkBlocks, Palette, QuartGrid, SurfaceConfig, FLAG_AIR, FLAG_FLUID, FLAG_MOTION_BLOCKING};

fn flags_for(name: &str) -> u8 {
    if name.contains("\"minecraft:air\"") {
        FLAG_AIR
    } else if name.contains("\"minecraft:water\"") || name.contains("\"minecraft:lava\"") {
        FLAG_FLUID
    } else {
        FLAG_MOTION_BLOCKING
    }
}

fn biome_zoom_seed(seed: i64) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.to_le_bytes());
    i64::from_le_bytes(digest[..8].try_into().unwrap())
}

fn canonical_palette(oracle: &SurfaceOracle) -> Vec<String> {
    oracle
        .palette
        .iter()
        .map(|n| Palette::canonical(&serde_json::from_str::<serde_json::Value>(n).expect("oracle palette json")).expect("oracle palette"))
        .collect()
}

fn quart_grid(cfg: &SurfaceConfig, oracle: &SurfaceOracle) -> QuartGrid {
    let biome_index: HashMap<&str, u16> = cfg.biomes.iter().enumerate().map(|(i, b)| (b.id.as_str(), i as u16)).collect();
    let (min_x, min_y, min_z) = QuartGrid::origin(oracle.chunk_x, oracle.chunk_z, oracle.min_y);
    assert_eq!(min_y, oracle.quart_min_y);
    QuartGrid {
        min_x,
        min_y,
        min_z,
        size_y: oracle.quart_count,
        data: oracle.quarts.iter().map(|&q| biome_index[oracle.biomes[q as usize].as_str()]).collect(),
    }
}

/// Compares a chunk against the carved oracle; returns the mismatch count and the first few lines.
fn compare(cfg: &SurfaceConfig, chunk: &ChunkBlocks, carved: &SurfaceOracle) -> (usize, Vec<String>) {
    let theirs_palette = canonical_palette(carved);
    let mut mismatches = 0;
    let mut first = Vec::new();
    let limit = if std::env::var("PAINITE_CARVER_VERBOSE").is_ok() { 400 } else { 12 };
    for i in 0..chunk.blocks.len() {
        let theirs = carved.blocks[i];
        let want = theirs_palette[(theirs & 0x7fff) as usize].as_str();
        let got = cfg.palette.names[chunk.blocks[i] as usize].as_str();
        let y = i % carved.height as usize;
        let x = (i / carved.height as usize) % 16;
        let z = i / carved.height as usize / 16;
        if want != got {
            mismatches += 1;
            if first.len() < limit {
                first.push(format!("block ({x},{},{z}) want {want} got {got}", carved.min_y + y as i32));
            }
        }
        if (theirs & 0x8000 != 0) != chunk.post_process[i] {
            mismatches += 1;
            if first.len() < limit {
                first.push(format!("post ({x},{},{z}) want {} got {} ({want})", carved.min_y + y as i32, theirs & 0x8000 != 0, chunk.post_process[i]));
            }
        }
    }
    for col in 0..256 {
        if chunk.surface_height[col] != carved.surface_height[col] || chunk.floor_height[col] != carved.floor_height[col] {
            mismatches += 1;
            if first.len() < limit {
                first.push(format!(
                    "heights col ({},{}) want {}/{} got {}/{}",
                    col % 16,
                    col / 16,
                    carved.surface_height[col],
                    carved.floor_height[col],
                    chunk.surface_height[col],
                    chunk.floor_height[col]
                ));
            }
        }
    }
    (mismatches, first)
}

fn setup() -> Option<(String, Vec<(String, String)>)> {
    let (Ok(root), Ok(surface), Ok(carved)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_SURFACE_ORACLE"), std::env::var("PAINITE_CARVED_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_SURFACE_ORACLE / PAINITE_CARVED_ORACLE not set, skipping");
        return None;
    };
    let pairs: Vec<(String, String)> = surface.split(',').map(str::to_string).zip(carved.split(',').map(str::to_string)).collect();
    Some((root, pairs))
}

struct World {
    cfg: SurfaceConfig,
    state: TerrainState,
}

fn world(root: &str, seed: i64) -> World {
    let mut loader = Loader::new(Path::new(root), seed);
    let settings = loader.noise_settings("minecraft:overworld").expect("settings");
    let biome_ids = loader.ids_of("biome").expect("biome ids");
    let mut cfg = SurfaceConfig::load(&mut loader, &settings, &biome_ids).expect("surface config");
    let flags: Vec<u8> = cfg.palette.names.iter().map(|n| flags_for(n)).collect();
    cfg.palette.set_flags(&flags).unwrap();
    let state = TerrainState::build_from_loader(&mut loader, biome_zoom_seed(seed), "minecraft:overworld").expect("terrain state");
    assert!(state.has_carvers(), "the WG root has no loadable carver stage: {:?}", state.carvers_declined);
    World { cfg, state }
}

/// Fill, surface and carve natively, then compare with the carved dump.
#[test]
fn carved_chunk_matches_vanilla() {
    let Some((root, pairs)) = setup() else { return };
    let mut failures = 0;
    for (surface_path, carved_path) in &pairs {
        let surface = SurfaceOracle::load(Path::new(surface_path)).expect("surface oracle");
        let carved = SurfaceOracle::load(Path::new(carved_path)).expect("carved oracle");
        assert_eq!((surface.seed, surface.chunk_x, surface.chunk_z), (carved.seed, carved.chunk_x, carved.chunk_z), "oracle pair mismatch");
        let beard = surface.beard.as_deref().map(|flat| Beardifier::from_flat(flat).expect("beard pieces"));
        println!("== {carved_path}: chunk {} {} eligible {:?}", carved.chunk_x, carved.chunk_z, carved.eligible);
        let w = world(&root, surface.seed);
        let quarts = quart_grid(&w.cfg, &surface);
        let (substance, _, mut aquifer) = w.state.fill_with_aquifer(surface.chunk_x, surface.chunk_z, beard.as_ref());
        let mut chunk = ChunkBlocks::from_fill(&w.cfg, surface.chunk_x, surface.chunk_z, &substance);
        let t0 = Instant::now();
        let mask = w.state.carve_mask(surface.chunk_x, surface.chunk_z).expect("mask");
        let cold_ms = t0.elapsed().as_secs_f64() * 1e3;
        let t0 = Instant::now();
        let _ = w.state.carve_mask(surface.chunk_x, surface.chunk_z);
        let mask_ms = t0.elapsed().as_secs_f64() * 1e3;
        let stage = w.state.carvers.as_ref().unwrap();
        let stats = build_surface_carved(&w.cfg, &mut chunk, &quarts, biome_zoom_seed(surface.seed), Some(CarveJob { stage, mask: &mask, aquifer: &mut aquifer }));
        println!("mask cold {cold_ms:.3} ms, warm {mask_ms:.3} ms ({} bits), {:?}", mask.count(), stats.carve);
        let (mismatches, first) = compare(&w.cfg, &chunk, &carved);
        println!("mismatches: {mismatches}");
        for line in &first {
            println!("  {line}");
        }
        if mismatches > 0 && surface.eligible != Some(false) {
            failures += 1;
        }
    }
    assert_eq!(failures, 0, "{failures} oracle chunk(s) differ");
}

/// The mask applied to vanilla's own surfaced blocks: any difference here
/// is the carve's, not the fill's or the surface's.
#[test]
fn carve_on_vanilla_surface_matches() {
    let Some((root, pairs)) = setup() else { return };
    let mut failures = 0;
    for (surface_path, carved_path) in &pairs {
        let surface = SurfaceOracle::load(Path::new(surface_path)).expect("surface oracle");
        let carved = SurfaceOracle::load(Path::new(carved_path)).expect("carved oracle");
        println!("== {carved_path}: chunk {} {}", carved.chunk_x, carved.chunk_z);
        let mut w = world(&root, surface.seed);
        // Vanilla's blocks in our palette; states the surface never places get interned here.
        let names = canonical_palette(&surface);
        let ids: Vec<u16> = names.iter().map(|n| w.cfg.palette.intern_json(&serde_json::from_str(n).unwrap()).unwrap()).collect();
        let flags: Vec<u8> = w.cfg.palette.names.iter().map(|n| flags_for(n)).collect();
        w.cfg.palette.set_flags(&flags).unwrap();
        let vol = Volume::chunk(surface.chunk_x, surface.chunk_z, surface.min_y, surface.height);
        let mut chunk = ChunkBlocks {
            vol,
            blocks: surface.blocks.iter().map(|&b| ids[(b & 0x7fff) as usize]).collect(),
            post_process: surface.blocks.iter().map(|&b| b & 0x8000 != 0).collect(),
            surface_height: surface.surface_height.clone().try_into().unwrap(),
            floor_height: surface.floor_height.clone().try_into().unwrap(),
        };
        let quarts = quart_grid(&w.cfg, &surface);
        let beard = surface.beard.as_deref().map(|flat| Beardifier::from_flat(flat).expect("beard pieces"));
        let (_, _, mut aquifer) = w.state.fill_with_aquifer(surface.chunk_x, surface.chunk_z, beard.as_ref());
        let mask = w.state.carve_mask(surface.chunk_x, surface.chunk_z).expect("mask");
        let stage = w.state.carvers.as_ref().unwrap();
        let stats = carve_only(&w.cfg, &mut chunk, &quarts, biome_zoom_seed(surface.seed), CarveJob { stage, mask: &mask, aquifer: &mut aquifer });
        println!("{} bits, {:?}", mask.count(), stats.carve);
        let (mismatches, first) = compare(&w.cfg, &chunk, &carved);
        println!("mismatches: {mismatches}");
        for line in &first {
            println!("  {line}");
        }
        if mismatches > 0 {
            failures += 1;
        }
    }
    assert_eq!(failures, 0, "{failures} oracle chunk(s) differ");
}

/// The carver biome from a single climate sample equals the biome stage's
/// chunk output at the same quart.
#[test]
fn carver_biome_matches_chunk_biomes() {
    let Some((root, pairs)) = setup() else { return };
    let surface = SurfaceOracle::load(Path::new(&pairs[0].0)).expect("surface oracle");
    let w = world(&root, surface.seed);
    let biomes = w.state.biomes.as_ref().expect("biome stage");
    let quart_y = (-surface.min_y >> 2) as usize;
    let mut differ = 0;
    for dx in -9..=9 {
        for dz in -9..=9 {
            let (cx, cz) = (surface.chunk_x + dx, surface.chunk_z + dz);
            let from_chunk = biomes.chunk_biomes(cx, cz)[quart_y];
            let from_point = w.state.carver_biome(cx, cz);
            if from_chunk != from_point {
                differ += 1;
            }
        }
    }
    assert_eq!(differ, 0, "{differ} source chunks disagree between the point sample and the chunk pass");
}
