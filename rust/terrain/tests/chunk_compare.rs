//! Bit-exact comparison of the cell-grid chunk fill against vanilla's
//! TERRAIN density buffer, plus a timing of the two halves.
//!
//! Needs `PAINITE_WG_ROOT` and `PAINITE_CHUNK_ORACLE` (the file written
//! by `/painite density chunk <x> <z>`), otherwise the test is skipped.

use std::path::Path;
use std::time::Instant;

use painite_terrain::aquifer26::{fill_chunk, Aquifer, AquiferConfig, Fluid, FluidPicker};
use painite_terrain::df26::{Loader, Volume};
use painite_terrain::fill26::{fill_fused, OverworldShape};
use painite_terrain::oracle::ChunkOracle;

#[test]
fn chunk_fill_matches_vanilla() {
    let (Ok(root), Ok(path)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_CHUNK_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_CHUNK_ORACLE not set, skipping");
        return;
    };
    let oracle = ChunkOracle::load(Path::new(&path)).expect("chunk oracle");
    let mut loader = Loader::new(Path::new(&root), oracle.seed);
    let router = loader.router("minecraft:overworld").expect("router");
    let final_density = router.get("final_density").expect("final_density");
    let vol = Volume::chunk(oracle.chunk_x, oracle.chunk_z, oracle.min_y, oracle.height);

    let settings = loader.noise_settings("minecraft:overworld").expect("settings");
    let config = AquiferConfig::load(&mut loader, &settings).expect("aquifer config");
    let sea_level = oracle.sea_level.unwrap_or(63);
    let picker = FluidPicker { sea_level, sea_fluid: Fluid::Water };
    let factory = loader.root_factory().from_hash_of("minecraft:aquifer").fork_positional();
    let make_aquifer = |v: &Volume| Aquifer::new(config.clone(), factory, v, picker);

    let t0 = Instant::now();
    let mut aquifer = make_aquifer(&vol);
    let (density, substance) = fill_chunk(final_density, &mut aquifer, &vol);
    let elapsed = t0.elapsed();
    if let Some(grid) = oracle.aq_grid {
        let (ours, cells, surfaces) = aquifer.debug_state();
        println!("aquifer grid vanilla {:?} rust {:?}", grid, ours);
        let theirs_cells: std::collections::HashMap<usize, _> = oracle.aq_cells.iter().map(|(i, l, s)| (*i, (*l, *s))).collect();
        let mut loc_bad = 0;
        let mut status_bad = 0;
        let mut shown = 0;
        for (i, l, s) in &cells {
            match theirs_cells.get(i) {
                None => {
                    loc_bad += 1;
                    if shown < 5 {
                        println!("    cell {i} computed in rust only: {l:?} {s:?}");
                        shown += 1;
                    }
                }
                Some((tl, ts)) => {
                    if tl != l {
                        loc_bad += 1;
                        if shown < 5 {
                            println!("    cell {i} location vanilla {tl:?} rust {l:?}");
                            shown += 1;
                        }
                    } else if ts != s && ts.is_some() && s.is_some() {
                        status_bad += 1;
                        if shown < 8 {
                            println!("    cell {i} at {l:?} status vanilla {ts:?} rust {s:?}");
                            shown += 1;
                        }
                    }
                }
            }
        }
        let theirs_surf: std::collections::HashMap<(i32, i32), i32> = oracle.aq_surfaces.iter().map(|(x, z, l)| ((*x, *z), *l)).collect();
        let mut surf_bad = 0;
        for (x, z, l) in &surfaces {
            if let Some(tl) = theirs_surf.get(&(*x, *z)) {
                if tl != l {
                    surf_bad += 1;
                    if surf_bad <= 5 {
                        println!("    surface ({x}, {z}) vanilla {tl} rust {l}");
                    }
                }
            }
        }
        println!("aquifer cells: {} rust, {} vanilla, {loc_bad} location diffs, {status_bad} status diffs, {surf_bad} surface diffs of {}", cells.len(), oracle.aq_cells.len(), surfaces.len());
        let names = ["floodedness", "exclusion", "spread"];
        let mut probe_bad = 0;
        for (i, vals) in &oracle.aq_probes {
            let Some((loc, _)) = theirs_cells.get(i) else { continue };
            let ours = aquifer.debug_probe(*loc);
            for k in 0..3 {
                let (cached, raw) = vals[k];
                if ours[k].to_bits() != cached || cached != raw {
                    probe_bad += 1;
                    if probe_bad <= 12 {
                        println!("    cell {i} at {loc:?} {}: vanilla cached {:08x} uncached {:08x} rust {:08x}", names[k], cached, raw, ours[k].to_bits());
                    }
                }
            }
        }
        println!("aquifer probes: {probe_bad} differing values over {} cells", oracle.aq_probes.len());
    }
    if !oracle.substance.is_empty() {
        let mut sub_mismatch = 0usize;
        let mut first_sub = Vec::new();
        for (i, (a, b)) in substance.iter().zip(&oracle.substance).enumerate() {
            if a != b {
                sub_mismatch += 1;
                if first_sub.len() < 5 {
                    let y = i % oracle.height as usize;
                    let x = (i / oracle.height as usize) % 16;
                    let z = i / oracle.height as usize / 16;
                    first_sub.push(format!("idx {i} (x{x} y{} z{z}): vanilla substance {b} rust {a} density {}", y as i32 + oracle.min_y, density[i]));
                }
            }
        }
        println!("substance: {sub_mismatch} mismatches of {}", substance.len());
        for f in &first_sub {
            println!("    {f}");
        }
        assert_eq!(sub_mismatch, 0, "substance mismatch");
    }

    assert_eq!(density.len(), oracle.bits.len(), "value count");
    let mut mismatches = 0usize;
    let mut first = Vec::new();
    for (i, (ours, theirs)) in density.iter().zip(&oracle.bits).enumerate() {
        if ours.to_bits() != *theirs {
            mismatches += 1;
            if first.len() < 5 {
                let y = i % oracle.height as usize;
                let x = (i / oracle.height as usize) % 16;
                let z = i / oracle.height as usize / 16;
                first.push(format!("idx {i} (x{x} y{} z{z}): vanilla {:08x} rust {:08x}", y as i32 + oracle.min_y, theirs, ours.to_bits()));
            }
        }
    }
    println!(
        "chunk ({}, {}): {} values, {} density mismatches, {:.3} ms first run (density + aquifer)",
        oracle.chunk_x, oracle.chunk_z, oracle.bits.len(), mismatches, elapsed.as_secs_f64() * 1e3
    );
    for f in &first {
        println!("    {f}");
    }

    // Steady-state timing: repeat the fill a few times.
    let reps = 5;
    let t1 = Instant::now();
    for _ in 0..reps {
        let v = Volume::chunk(oracle.chunk_x + 1, oracle.chunk_z, oracle.min_y, oracle.height);
        let mut aq = make_aquifer(&v);
        std::hint::black_box(fill_chunk(final_density, &mut aq, &v).1.len());
    }
    println!("steady state: {:.3} ms per chunk over {reps} reps", t1.elapsed().as_secs_f64() * 1e3 / reps as f64);

    // Fused overworld fill: same bits, one pass.
    let shape = OverworldShape::recognise(final_density).expect("overworld shape");
    let mut aq = make_aquifer(&vol);
    let (fd, fs, stats) = fill_fused(&shape, &mut aq, &vol, None, true);
    let d_bad = fd.iter().zip(&density).filter(|(a, b)| a.to_bits() != b.to_bits()).count();
    let s_bad = fs.iter().zip(&substance).filter(|(a, b)| (*a & 0x0f) != **b).count();
    if oracle.substance.iter().any(|s| *s >= 16) {
        let flag_bad = fs.iter().zip(&oracle.substance).filter(|(a, b)| **a != **b).count();
        println!("fused fluid-update flags: {flag_bad} diffs vs vanilla");
        assert_eq!(flag_bad, 0, "fluid update flag mismatch");
    }
    println!("fused (verify): {d_bad} density diffs, {s_bad} substance diffs vs reference; {stats:?}");
    assert_eq!(d_bad + s_bad, 0, "fused fill differs from reference");
    // PAINITE_REPS lengthens only the fused loop so a profiler sees mostly fused work.
    let reps: usize = std::env::var("PAINITE_REPS").ok().and_then(|s| s.parse().ok()).unwrap_or(reps);
    let t2 = Instant::now();
    let mut last = None;
    let mut new_ms = 0.0;
    for _ in 0..reps {
        let v = Volume::chunk(oracle.chunk_x + 1, oracle.chunk_z, oracle.min_y, oracle.height);
        let tn = Instant::now();
        let mut aq = make_aquifer(&v);
        new_ms += tn.elapsed().as_secs_f64() * 1e3 / reps as f64;
        let r = fill_fused(&shape, &mut aq, &v, None, false);
        last = Some(r.2);
        std::hint::black_box(r.1.len());
    }
    println!("fused steady state: {:.3} ms per chunk over {reps} reps (aquifer new {new_ms:.3} ms); {:?}", t2.elapsed().as_secs_f64() * 1e3 / reps as f64, last.unwrap());
    let mut aq = make_aquifer(&vol);
    let (fd2, fs2, _) = fill_fused(&shape, &mut aq, &vol, None, false);
    println!("aquifer counters: {:?}", aq.counters);
    let skip_bad = fs2.iter().zip(&substance).filter(|(a, b)| (*a & 0x0f) != **b).count();
    let skip_dbad = fd2.iter().zip(&density).filter(|(a, b)| a.to_bits() != b.to_bits()).count();
    println!("fused (skip on): {skip_dbad} density diffs, {skip_bad} substance diffs vs reference");
    assert_eq!(skip_bad, 0, "cell skip changed substances");
    let flag_bad = fs2.iter().zip(&fs).filter(|(a, b)| a != b).count();
    assert_eq!(flag_bad, 0, "cell skip changed fluid update flags");
    // The skips (solid cells, air rows) against the full path on the chunks around: same substance bytes.
    let mut skip_diffs = 0;
    for (dx, dz) in [(-2, -2), (-1, 0), (0, 3), (2, -1), (3, 3), (-3, 1), (1, -3), (4, 0), (0, -4), (-4, -4), (5, 2), (2, 5)] {
        let v = Volume::chunk(oracle.chunk_x + dx, oracle.chunk_z + dz, oracle.min_y, oracle.height);
        let mut aq_full = make_aquifer(&v);
        let (_, full, _) = fill_fused(&shape, &mut aq_full, &v, None, true);
        let mut aq_skip = make_aquifer(&v);
        let (_, skip, st) = fill_fused(&shape, &mut aq_skip, &v, None, false);
        let bad = full.iter().zip(&skip).filter(|(a, b)| a != b).count();
        println!("chunk ({}, {}): {bad} substance diffs skip vs full, {} air cells, {} rows skipped", v.min[0] >> 4, v.min[2] >> 4, st.air_cells_skipped, st.corner_rows_skipped);
        skip_diffs += bad;
    }
    assert_eq!(skip_diffs, 0, "a skip changed substances on a neighbouring chunk");
    assert_eq!(mismatches, 0, "chunk density mismatch");
}
