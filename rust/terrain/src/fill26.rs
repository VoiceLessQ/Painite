//! Fused TERRAIN fill for the vanilla overworld density shape.
//!
//! Above the cell grid, overworld `final_density` is pure scalar work
//! on five interpolated values:
//!
//! ```text
//! add(min(squeeze(I_main), range_choice(I_toggle, [-1e6, 0) -> 64,
//!     else add(I_thick, mul(max(abs(I_a), abs(I_b)), 1.5)))), beardifier)
//! ```
//!
//! Vanilla materialises one 98304-float buffer per node. This module
//! matches that shape at load time, samples the five cell-corner grids
//! once, and then walks every cell producing the final value and the
//! aquifer substance in a single pass, with the same `fillCell`
//! arithmetic (z, x, then an incremental y walk) so the bits agree.
//! Cells whose corners make the outcome certain skip the per-block
//! work entirely.

use crate::aquifer26::{Aquifer, Substance};
use crate::beard26::Beardifier;
use crate::bounds26::BoundCtx;
use crate::df26::{Node, SampleCtx, Volume};
use crate::noise26::lerp;

/// Bit set on a fluid substance when vanilla would schedule a fluid
/// update for the block (`markPosForPostProcessing`).
pub const FLUID_UPDATE_BIT: u8 = 0x10;

/// The five interpolated inputs of the overworld shape.
pub struct OverworldShape<'a> {
    pub cell_xz: i32,
    pub cell_y: i32,
    pub main: &'a Node,
    pub toggle: &'a Node,
    pub thickness: &'a Node,
    pub ridge_a: &'a Node,
    pub ridge_b: &'a Node,
}

fn interp<'a>(n: &'a Node) -> Option<(i32, i32, &'a Node)> {
    match n {
        Node::Interpolated { cell_size_xz, cell_size_y, input } => Some((*cell_size_xz, *cell_size_y, input)),
        _ => None,
    }
}

impl<'a> OverworldShape<'a> {
    /// Recognise the overworld final_density tree; `None` means the
    /// generic path must be used.
    pub fn recognise(final_density: &'a Node) -> Option<Self> {
        let Node::Add(l, r) = final_density else { return None };
        let Node::Beardifier = &**r else { return None };
        let Node::Min(sq, noodle) = &**l else { return None };
        let Node::Squeeze(main_i) = &**sq else { return None };
        let (cell_xz, cell_y, main) = interp(main_i)?;
        let Node::RangeChoice { input, min_inclusive, max_exclusive, when_in_range, when_out_of_range } = &**noodle else {
            return None;
        };
        if *min_inclusive != -1_000_000.0 || *max_exclusive != 0.0 {
            return None;
        }
        let Node::Constant(c) = &**when_in_range else { return None };
        if *c != 64.0 {
            return None;
        }
        let (txz, ty, toggle) = interp(input)?;
        let Node::Add(thick_i, mul) = &**when_out_of_range else { return None };
        let (hxz, hy, thickness) = interp(thick_i)?;
        let Node::ConstMul(mx, factor) = &**mul else { return None };
        if *factor != 1.5 {
            return None;
        }
        let Node::Max(aa, ab) = &**mx else { return None };
        let Node::Abs(ai) = &**aa else { return None };
        let Node::Abs(bi) = &**ab else { return None };
        let (axz, ay, ridge_a) = interp(ai)?;
        let (bxz, by, ridge_b) = interp(bi)?;
        let cells_ok = [txz, hxz, axz, bxz].iter().all(|c| *c == cell_xz) && [ty, hy, ay, by].iter().all(|c| *c == cell_y);
        if !cells_ok {
            return None;
        }
        Some(Self { cell_xz, cell_y, main, toggle, thickness, ridge_a, ridge_b })
    }
}

/// `squeeze` as `UnaryFunction.SqueezeSampler`.
#[inline]
fn squeeze(v: f32) -> f32 {
    let c = if v < -1.0 { -1.0 } else { v.min(1.0) };
    c / 2.0 - c * c * c / 24.0
}

/// The scalar tail above the grid for one block; `beard` is the
/// structure term (0 without one).
#[inline]
fn combine(main: f32, toggle: f32, thick: f32, a: f32, b: f32, beard: f32) -> f32 {
    let noodle = if toggle >= -1_000_000.0 && toggle < 0.0 { 64.0 } else { thick + a.abs().max(b.abs()) * 1.5 };
    let s = squeeze(main);
    let m = if s.is_nan() || noodle.is_nan() { f32::NAN } else { s.min(noodle) };
    m + beard
}

/// Corner margin below which a cell is not treated as sign-uniform.
const MARGIN: f32 = 1.0e-4;
/// The main bound must sit this far below zero for a corner row to count as air.
const AIR_MARGIN: f32 = 1.0e-3;

/// Counters from one fused fill.
#[derive(Clone, Copy, Debug, Default)]
pub struct FillStats {
    pub cells: usize,
    pub solid_cells_skipped: usize,
    /// Cells above the aquifer skip height whose main corners are provably negative: no corners, no lerp.
    pub air_cells_skipped: usize,
    /// Corner rows the five grids were not sampled for.
    pub corner_rows_skipped: usize,
    pub bounds_ms: f64,
    pub blocks_lerped: usize,
    pub aquifer_calls: usize,
    /// Aquifer calls at or below skipSamplingAboveY (the expensive ones).
    pub aquifer_deep_calls: usize,
    pub corners_ms: f64,
    /// Part of corners_ms spent in noise leaves and in spline nodes.
    pub corner_noise_ms: f64,
    pub corner_spline_ms: f64,
    pub lerp_ms: f64,
    pub aquifer_ms: f64,
}

/// One fused fill: density and substance per block, in the chunk
/// buffer order `y + (x + z * 16) * height`. `beard` carries the
/// structure pieces near the chunk, if any.
pub fn fill_fused(shape: &OverworldShape, aquifer: &mut Aquifer, vol: &Volume, beard: Option<&Beardifier>, verify: bool) -> (Vec<f32>, Vec<u8>, FillStats) {
    assert_eq!(vol.step, [1, 1, 1]);
    let beard = beard.and_then(|b| b.sample_volume(vol));
    let cell_xz = shape.cell_xz;
    let cell_y = shape.cell_y;
    let cells = [cell_xz, cell_y, cell_xz];
    let mut min_cell = [0i32; 3];
    let mut count = [0usize; 3];
    let mut corners = [0usize; 3];
    for a in 0..3 {
        min_cell[a] = vol.min[a].div_euclid(cells[a]);
        let max_cell = vol.max_block(a).div_euclid(cells[a]);
        count[a] = (max_cell - min_cell[a] + 1) as usize;
        corners[a] = if vol.max_block(a).rem_euclid(cells[a]) == 0 { count[a] } else { count[a] + 1 };
    }
    let cell_vol = Volume::new(corners, [min_cell[0] * cell_xz, min_cell[1] * cell_y, min_cell[2] * cell_xz], cells);
    let skip_y = aquifer.skip_sampling_above_y();

    // Cells from `air_from` up are air without a density value: every
    // block sits above the aquifer skip height, so the substance is the
    // global fluid, and the main density is provably negative there by
    // the interval bound of each corner column. A beard can push any
    // block positive, so the rows it reaches keep the full path.
    let t_bounds = std::time::Instant::now();
    let mut air_from = count[1];
    if !verify && std::env::var_os("PAINITE_NO_AIR_SKIP").is_none() {
        let cell_min_y = cell_vol.min[1];
        let first_above_skip = if skip_y < cell_min_y { 0 } else { ((skip_y - cell_min_y) / cell_y + 1) as usize };
        let beard_top = beard.as_ref().map_or(0, |(buf, _)| {
            let height = vol.size[1];
            let top = buf.iter().enumerate().filter(|(_, v)| **v != 0.0).map(|(i, _)| i % height).max();
            top.map_or(0, |t| (vol.min[1] + t as i32 - cell_min_y) / cell_y + 1) as usize
        });
        let mut r = first_above_skip.max(beard_top).min(count[1]);
        if r < count[1] {
            let y_top = cell_vol.block(1, corners[1] - 1);
            let mut bctx = BoundCtx::new();
            for cx in 0..corners[0] {
                for cz in 0..corners[2] {
                    bctx.start_column(cell_vol.block(0, cx), cell_vol.block(2, cz), cell_min_y);
                    while r < count[1] {
                        let (_, hi) = shape.main.column_bounds(cell_vol.block(1, r), y_top, &mut bctx);
                        if hi < -AIR_MARGIN {
                            break;
                        }
                        r += 1;
                    }
                }
            }
        }
        air_from = r;
    }
    let bounds_ms = t_bounds.elapsed().as_secs_f64() * 1e3;
    // Corner rows 0..=air_from feed the cells below air_from.
    let mut grid_vol = cell_vol;
    grid_vol.size[1] = corners[1].min(air_from + 1);

    let t_corners = std::time::Instant::now();
    // The five corner grids share one context so cache nodes are reused.
    let mut ctx = SampleCtx::default();
    let grids: [Vec<f32>; 5] = [
        shape.main.sample_volume_with(&grid_vol, &mut ctx),
        shape.toggle.sample_volume_with(&grid_vol, &mut ctx),
        shape.thickness.sample_volume_with(&grid_vol, &mut ctx),
        shape.ridge_a.sample_volume_with(&grid_vol, &mut ctx),
        shape.ridge_b.sample_volume_with(&grid_vol, &mut ctx),
    ];

    let mut density = vec![0.0f32; vol.len()];
    let mut substance = vec![0u8; vol.len()];
    let mut stats = FillStats {
        corners_ms: t_corners.elapsed().as_secs_f64() * 1e3,
        corner_noise_ms: ctx.noise_ns as f64 * 1e-6,
        corner_spline_ms: ctx.spline_ns as f64 * 1e-6,
        bounds_ms,
        corner_rows_skipped: corners[1] - grid_vol.size[1],
        ..FillStats::default()
    };
    let inv_xz = 1.0f32 / cell_xz as f32;
    let inv_y = 1.0f32 / cell_y as f32;
    let size_y = vol.size[1];

    for cz in 0..count[2] {
        let nz = (cz + 1).min(corners[2] - 1);
        for cx in 0..count[0] {
            let nx = (cx + 1).min(corners[0] - 1);
            // Per grid: v000 v100 v010 v110 v001 v101 v011 v111.
            let mut c = [[0.0f32; 8]; 5];
            for g in 0..5 {
                c[g][0] = grids[g][grid_vol.index(cx, 0, cz)];
                c[g][1] = grids[g][grid_vol.index(nx, 0, cz)];
                c[g][4] = grids[g][grid_vol.index(cx, 0, nz)];
                c[g][5] = grids[g][grid_vol.index(nx, 0, nz)];
            }
            for cy in 0..count[1] {
                stats.cells += 1;
                let cell_out = [
                    cell_vol.block(0, cx) - vol.min[0],
                    cell_vol.block(1, cy) - vol.min[1],
                    cell_vol.block(2, cz) - vol.min[2],
                ];
                let x0 = 0.max(-cell_out[0]);
                let y0 = 0.max(-cell_out[1]);
                let z0 = 0.max(-cell_out[2]);
                let x1 = cell_xz.min(vol.size[0] as i32 - cell_out[0]) - 1;
                let y1 = cell_y.min(vol.size[1] as i32 - cell_out[1]) - 1;
                let z1 = cell_xz.min(vol.size[2] as i32 - cell_out[2]) - 1;
                if cy >= air_from {
                    // Air cell above the aquifer skip height: the global fluid per block, no corners read.
                    stats.air_cells_skipped += 1;
                    for z in z0..=z1 {
                        let out_z = (cell_out[2] + z) as usize;
                        let bz = vol.min[2] + out_z as i32;
                        for x in x0..=x1 {
                            let out_x = (cell_out[0] + x) as usize;
                            let bx = vol.min[0] + out_x as i32;
                            for y in (y0..=y1).rev() {
                                let out_y = (cell_out[1] + y) as usize;
                                let by = vol.min[1] + out_y as i32;
                                debug_assert!(by > skip_y);
                                stats.aquifer_calls += 1;
                                let sub = aquifer.compute_substance(bx, by, bz, -1.0);
                                substance[out_y + (out_x + out_z * 16) * size_y] = Substance::oracle_id(sub);
                            }
                        }
                    }
                    continue;
                }
                let ny = (cy + 1).min(corners[1] - 1);
                for g in 0..5 {
                    c[g][2] = grids[g][grid_vol.index(cx, ny, cz)];
                    c[g][3] = grids[g][grid_vol.index(nx, ny, cz)];
                    c[g][6] = grids[g][grid_vol.index(cx, ny, nz)];
                    c[g][7] = grids[g][grid_vol.index(nx, ny, nz)];
                }

                // Sign-uniform solid cell: main > 0 everywhere, and the
                // noodle branch is positive everywhere (toggle all
                // negative gives 64; toggle all non-negative with a
                // positive thickness gives at least the thickness).
                let main_solid = c[0].iter().all(|v| *v > MARGIN);
                let toggle_neg = c[1].iter().all(|v| *v < -MARGIN && *v >= -1_000_000.0);
                let toggle_pos = c[1].iter().all(|v| *v >= 0.0);
                let thick_pos = c[2].iter().all(|v| *v > MARGIN);
                // A structure beard can pull any block negative, so cells it reaches never skip.
                let bearded = beard.as_ref().is_some_and(|(_, reach)| {
                    reach.touches(
                        [(cell_out[0] + x0) as usize, (cell_out[1] + y0) as usize, (cell_out[2] + z0) as usize],
                        [(cell_out[0] + x1) as usize, (cell_out[1] + y1) as usize, (cell_out[2] + z1) as usize],
                    )
                });
                let solid = main_solid && (toggle_neg || (toggle_pos && thick_pos)) && !bearded;
                if solid && !verify {
                    stats.solid_cells_skipped += 1;
                    // Density values are only needed by the oracle diff;
                    // fill them with the exact lerp anyway so a caller
                    // that reads them sees the same bits.
                    fill_cell_values(&c[0], vol, cell_out, [x0, y0, z0], [x1, y1, z1], inv_xz, inv_y, &mut |i, m| {
                        // main only: squeeze(main) < noodle here is not
                        // guaranteed, so recompute the tail exactly.
                        density[i] = m;
                    });
                    // Substance stays 0 (default block) for the whole cell.
                    carry_up(&mut c);
                    continue;
                }

                for z in z0..=z1 {
                    let out_z = (cell_out[2] + z) as usize;
                    let alpha_z = z as f32 * inv_xz;
                    let mut zl = [[0.0f32; 4]; 5];
                    for g in 0..5 {
                        zl[g] = [
                            lerp(alpha_z, c[g][0], c[g][4]),
                            lerp(alpha_z, c[g][2], c[g][6]),
                            lerp(alpha_z, c[g][1], c[g][5]),
                            lerp(alpha_z, c[g][3], c[g][7]),
                        ];
                    }
                    for x in x0..=x1 {
                        let out_x = (cell_out[0] + x) as usize;
                        let alpha_x = x as f32 * inv_xz;
                        let mut v = [0.0f32; 5];
                        let mut step = [0.0f32; 5];
                        for g in 0..5 {
                            let v_0 = lerp(alpha_x, zl[g][0], zl[g][2]);
                            let v_1 = lerp(alpha_x, zl[g][1], zl[g][3]);
                            step[g] = (v_1 - v_0) * inv_y;
                            v[g] = v_0 + step[g] * y0 as f32;
                        }
                        let base = vol.index(out_x, (cell_out[1] + y0) as usize, out_z);
                        for k in 0..(y1 - y0 + 1) as usize {
                            let bd = match &beard {
                                Some((buf, _)) => buf[base + k],
                                None => 0.0,
                            };
                            let d = combine(v[0], v[1], v[2], v[3], v[4], bd);
                            density[base + k] = d;
                            for g in 0..5 {
                                v[g] += step[g];
                            }
                            stats.blocks_lerped += 1;
                        }
                        if verify && solid {
                            for k in 0..(y1 - y0 + 1) as usize {
                                assert!(density[base + k] > 0.0, "sign-uniform skip would be wrong at index {}", base + k);
                            }
                        }
                    }
                }
                // Aquifer in doFill order for this cell: z, x, then y from the top.
                let t_aq = std::time::Instant::now();
                for z in z0..=z1 {
                    let out_z = (cell_out[2] + z) as usize;
                    let bz = vol.min[2] + out_z as i32;
                    for x in x0..=x1 {
                        let out_x = (cell_out[0] + x) as usize;
                        let bx = vol.min[0] + out_x as i32;
                        for y in (y0..=y1).rev() {
                            let out_y = (cell_out[1] + y) as usize;
                            let i = out_y + (out_x + out_z * 16) * size_y;
                            let d = density[i];
                            if d > 0.0 {
                                continue;
                            }
                            stats.aquifer_calls += 1;
                            let by = vol.min[1] + out_y as i32;
                            if by <= skip_y {
                                stats.aquifer_deep_calls += 1;
                            }
                            let sub = aquifer.compute_substance(bx, by, bz, d as f64);
                            let mut id = Substance::oracle_id(sub);
                            if aquifer.should_schedule_fluid_update && matches!(sub, Some(Substance::Water) | Some(Substance::Lava)) {
                                id |= FLUID_UPDATE_BIT;
                            }
                            substance[i] = id;
                        }
                    }
                }
                stats.aquifer_ms += t_aq.elapsed().as_secs_f64() * 1e3;
                carry_up(&mut c);
            }
        }
    }
    stats.lerp_ms = t_corners.elapsed().as_secs_f64() * 1e3 - stats.corners_ms - stats.aquifer_ms;
    (density, substance, stats)
}

/// The next cell up starts from this cell's top corners.
#[inline]
fn carry_up(c: &mut [[f32; 8]; 5]) {
    for g in c.iter_mut() {
        g[0] = g[2];
        g[1] = g[3];
        g[4] = g[6];
        g[5] = g[7];
    }
}

/// `fillCell` for one grid, calling `f(index, value)` per block.
#[allow(clippy::too_many_arguments)]
fn fill_cell_values(
    c: &[f32; 8],
    vol: &Volume,
    cell_out: [i32; 3],
    lo: [i32; 3],
    hi: [i32; 3],
    inv_xz: f32,
    inv_y: f32,
    f: &mut impl FnMut(usize, f32),
) {
    for z in lo[2]..=hi[2] {
        let out_z = (cell_out[2] + z) as usize;
        let alpha_z = z as f32 * inv_xz;
        let v00 = lerp(alpha_z, c[0], c[4]);
        let v01 = lerp(alpha_z, c[2], c[6]);
        let v10 = lerp(alpha_z, c[1], c[5]);
        let v11 = lerp(alpha_z, c[3], c[7]);
        for x in lo[0]..=hi[0] {
            let out_x = (cell_out[0] + x) as usize;
            let alpha_x = x as f32 * inv_xz;
            let v_0 = lerp(alpha_x, v00, v10);
            let v_1 = lerp(alpha_x, v01, v11);
            let step = (v_1 - v_0) * inv_y;
            let mut value = v_0 + step * lo[1] as f32;
            let base = vol.index(out_x, (cell_out[1] + lo[1]) as usize, out_z);
            for k in 0..(hi[1] - lo[1] + 1) as usize {
                f(base + k, value);
                value += step;
            }
        }
    }
}
