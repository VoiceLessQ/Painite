//! How many corner rows of the main grid are provably air by value, and
//! what the five corner grids cost above them.
//! Usage: air_rows <wg_root> <seed> <cx> <cz> [<cx> <cz> ...]
use std::path::Path;
use std::time::Instant;
use painite_terrain::df26::{Loader, SampleCtx, Volume};
use painite_terrain::bounds26::BoundCtx;
use painite_terrain::fill26::OverworldShape;
use painite_terrain::state26::TerrainState;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut loader = Loader::new(Path::new(&args[1]), args[2].parse().unwrap());
    let router = loader.router("minecraft:overworld").unwrap();
    let fd = router.get("final_density").unwrap().clone();
    let shape = OverworldShape::recognise(&fd).unwrap();
    let state = TerrainState::build_from_loader(&mut loader, 0, "minecraft:overworld").unwrap();
    let mut i = 3;
    while i + 1 < args.len() {
        let (cx, cz): (i32, i32) = (args[i].parse().unwrap(), args[i + 1].parse().unwrap());
        i += 2;
        let cell_vol = Volume::new([5, 49, 5], [cx * 16, -64, cz * 16], [4, 8, 4]);
        let mut ctx = SampleCtx::default();
        let main = shape.main.sample_volume_with(&cell_vol, &mut ctx);
        // Per row: max over the 25 corners.
        let mut first_air_row = 49;
        let mut rows_air = 0;
        let mut row_max = Vec::new();
        for y in 0..49 {
            let m = (0..5).flat_map(|x| (0..5).map(move |z| (x, z))).map(|(x, z)| main[cell_vol.index(x, y, z)]).fold(f32::MIN, f32::max);
            row_max.push(m);
            if m < 0.0 {
                rows_air += 1;
            }
        }
        for y in (0..49).rev() {
            if row_max[y] >= 0.0 {
                first_air_row = y + 1;
                break;
            }
        }
        let time = |vol: &Volume| {
            let mut best = f64::MAX;
            for _ in 0..8 {
                let mut ctx = SampleCtx::default();
                let t = Instant::now();
                for n in [shape.main, shape.toggle, shape.thickness, shape.ridge_a, shape.ridge_b] {
                    std::hint::black_box(n.sample_volume_with(vol, &mut ctx).len());
                }
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
            }
            best
        };
        let full = time(&cell_vol);
        let below = Volume::new([5, first_air_row, 5], [cx * 16, -64, cz * 16], [4, 8, 4]);
        let trunc = time(&below);
        let top_solid_y = -64 + (first_air_row as i32 - 1) * 8;
        // Bound cut: per corner column, the lowest row from which the main bound stays below -1e-3.
        let tb = Instant::now();
        let mut bctx = BoundCtx::new();
        let mut bound_cut = 0usize;
        let mut worst_col = (0, 0);
        for x in 0..5 {
            for z in 0..5 {
                bctx.start_column(cx * 16 + x * 4, cz * 16 + z * 4, -64);
                let mut r = 49;
                while r > 0 {
                    let (_, hi) = shape.main.column_bounds(-64 + (r as i32 - 1) * 8, -64 + 48 * 8, &mut bctx);
                    if hi < -1e-3 {
                        r -= 1;
                    } else {
                        break;
                    }
                }
                if r > bound_cut {
                    bound_cut = r;
                    worst_col = (x, z);
                }
            }
        }
        let bound_ms = tb.elapsed().as_secs_f64() * 1e3;
        bctx.start_column(cx * 16 + worst_col.0 * 4, cz * 16 + worst_col.1 * 4, -64);
        let sample_hi = shape.main.column_bounds(-64 + (bound_cut as i32) * 8, -64 + 48 * 8, &mut bctx);
        print!("bound cut row {bound_cut} (empirical {first_air_row}), bounds {bound_ms:.3} ms, bound at cut {:?}; ", sample_hi);
        let mut best = f64::MAX;
        let mut best_stats = None;
        for _ in 0..8 {
            let t = Instant::now();
            let (_, stats) = state.fill_with(cx, cz, None);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            if ms < best {
                best = ms;
                best_stats = Some(stats);
            }
        }
        let st = best_stats.unwrap();
        print!(
            "fill {best:.3} ms (corners {:.3}, lerp {:.3}, aquifer {:.3}, bounds {:.3}; air cells {}, rows skipped {}); ",
            st.corners_ms, st.lerp_ms, st.aquifer_ms, st.bounds_ms, st.air_cells_skipped, st.corner_rows_skipped
        );
        let (_, _, aquifer) = state.fill_with_aquifer(cx, cz, None);
        let skip_y = aquifer.skip_sampling_above_y();
        let skip_row = ((skip_y + 64) as f32 / 8.0).ceil() as usize;
        let eff = first_air_row.max(skip_row);
        let eff_vol = Volume::new([5, eff.min(49), 5], [cx * 16, -64, cz * 16], [4, 8, 4]);
        let eff_ms = time(&eff_vol);
        print!("skip_y {skip_y} (row {skip_row}), effective saving {:.3} ms; ", full - eff_ms);
        println!(
            "chunk ({cx}, {cz}): rows with max<0: {rows_air}/49, contiguous air from row {first_air_row} (y > {top_solid_y}); five grids {full:.3} ms full, {trunc:.3} ms up to that row, saving {:.3} ms ({:.0}%); row max near the cut: {:?}",
            full - trunc,
            (full - trunc) / full * 100.0,
            &row_max[first_air_row.saturating_sub(3)..(first_air_row + 3).min(49)]
        );
    }
}
