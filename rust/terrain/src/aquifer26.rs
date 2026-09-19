//! 26.3 `Aquifer.NoiseBasedAquifer`: decides, for every non-solid block
//! of the fill, whether it is air, water, lava or (because of a
//! pressure barrier) the default block after all.
//!
//! One instance per chunk column, as vanilla builds one per
//! `NoiseChunk`. Density function samples go through the point path
//! (vanilla samples them with `sampleValue` on the caching context).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::df26::{Loader, Mode, Node, PointCache, SliceUniformAxes, Volume, ALL_AXES};
use crate::xoroshiro::XoroshiroPositionalRandomFactory;

/// `DimensionType.WAY_BELOW_MIN_Y` (`MIN_Y << 4` with `MIN_Y = -2032`).
pub const WAY_BELOW_MIN_Y: i32 = -32512;
/// `DimensionType.MIN_Y * 2`, the empty status level.
pub const EMPTY_STATUS_LEVEL: i32 = -4064;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fluid {
    Air,
    Water,
    Lava,
}

/// What a block position becomes: `None` is the default block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Substance {
    Air,
    Water,
    Lava,
}

impl Substance {
    /// The id the chunk oracle writes (0 = default block).
    pub fn oracle_id(s: Option<Substance>) -> u8 {
        match s {
            None => 0,
            Some(Substance::Air) => 1,
            Some(Substance::Water) => 2,
            Some(Substance::Lava) => 3,
        }
    }
}

/// `Aquifer.FluidStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FluidStatus {
    pub level: i32,
    pub fluid: Fluid,
}

impl FluidStatus {
    #[inline]
    pub fn at(self, y: i32) -> Fluid {
        if y < self.level { self.fluid } else { Fluid::Air }
    }
}

/// `NoiseBasedChunkGenerator.createFluidPicker`.
#[derive(Clone, Copy, Debug)]
pub struct FluidPicker {
    pub sea_level: i32,
    pub sea_fluid: Fluid,
}

impl FluidPicker {
    #[inline]
    pub fn compute_fluid(&self, _x: i32, y: i32, _z: i32) -> FluidStatus {
        if y < (-54).min(self.sea_level) {
            FluidStatus { level: -54, fluid: Fluid::Lava }
        } else {
            FluidStatus { level: self.sea_level, fluid: self.sea_fluid }
        }
    }
}

/// `Aquifer.Config`: the six density functions from `noise_settings.aquifers`.
#[derive(Clone, Debug)]
pub struct AquiferConfig {
    pub barrier: Arc<Node>,
    pub floodedness: Arc<Node>,
    pub spread: Arc<Node>,
    pub lava: Arc<Node>,
    pub exclusion: Arc<Node>,
    pub surface_level: Arc<Node>,
}

impl AquiferConfig {
    /// Parse and compile the `aquifers` object of a noise settings file.
    pub fn load(loader: &mut Loader, settings: &Value) -> Result<Self, String> {
        let a = settings.get("aquifers").and_then(Value::as_object).ok_or("aquifers missing")?;
        let mut rule = SliceUniformAxes::new();
        let mut get = |key: &str| -> Result<Arc<Node>, String> {
            let v = a.get(key).ok_or_else(|| format!("aquifers.{key} missing"))?;
            let n = loader.parse(v).map_err(|e| format!("aquifers.{key}: {e}"))?;
            Ok(rule.rewrite(&n, ALL_AXES))
        };
        Ok(Self {
            barrier: get("barrier")?,
            floodedness: get("fluid_level_floodedness")?,
            spread: get("fluid_level_spread")?,
            lava: get("lava")?,
            exclusion: get("exclusion")?,
            surface_level: get("surface_level")?,
        })
    }
}

const SURFACE_SAMPLING_OFFSETS_IN_CHUNKS: [[i32; 2]; 13] = [
    [0, 0], [-2, -1], [-1, -1], [0, -1], [1, -1], [-3, 0], [-2, 0], [-1, 0], [1, 0], [-2, 1], [-1, 1], [0, 1], [1, 1],
];

#[inline]
fn grid_x(b: i32) -> i32 {
    b >> 4
}
#[inline]
fn from_grid_x(g: i32, off: i32) -> i32 {
    (g << 4) + off
}
#[inline]
fn grid_y(b: i32) -> i32 {
    b.div_euclid(12)
}
#[inline]
fn from_grid_y(g: i32, off: i32) -> i32 {
    g * 12 + off
}

#[inline]
fn similarity(d1: i32, d2: i32) -> f64 {
    1.0 - (d2 - d1) as f64 / 25.0
}

/// Two smallest of two sorted pairs.
#[inline]
fn merge2(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    (a.0.min(b.0), a.0.max(b.0).min(a.1.min(b.1)))
}

#[inline]
fn clamped_map(v: f64, from_min: f64, from_max: f64, to_min: f64, to_max: f64) -> f64 {
    let f = (v - from_min) / (from_max - from_min);
    if f < 0.0 {
        to_min
    } else if f > 1.0 {
        to_max
    } else {
        to_min + f * (to_max - to_min)
    }
}

#[inline]
fn map(v: f64, from_min: f64, from_max: f64, to_min: f64, to_max: f64) -> f64 {
    let f = (v - from_min) / (from_max - from_min);
    to_min + f * (to_max - to_min)
}

/// A computed aquifer cell: index, location, status as (level, oracle fluid id).
pub type DebugCell = (usize, (i32, i32, i32), Option<(i32, u8)>);

/// Where `compute_substance` calls end up, for sizing the per-block work.
#[derive(Default, Debug, Clone, Copy)]
pub struct AquiferCounters {
    /// Calls that reached the twelve-cell search.
    pub deep: usize,
    /// Deep calls that returned after the search without a pressure test.
    pub near_exit: usize,
    /// Barrier noise point samples.
    pub barrier_samples: usize,
    /// Aquifer cell statuses computed (each samples up to three noises).
    pub status_computes: usize,
}

/// One chunk column's aquifer state.
pub struct Aquifer {
    pub counters: AquiferCounters,
    config: AquiferConfig,
    factory: XoroshiroPositionalRandomFactory,
    picker: FluidPicker,
    min_grid: [i32; 3],
    grid_size_x: i32,
    grid_size_z: i32,
    location_cache: Vec<Option<(i32, i32, i32)>>,
    status_cache: Vec<Option<FluidStatus>>,
    surface_cache: HashMap<(i32, i32), i32>,
    skip_sampling_above_y: i32,
    pub should_schedule_fluid_update: bool,
    point_cache: PointCache,
    /// The twelve candidate cells of the last (x, z, grid y) column: cache index, location y, dx*dx + dz*dz.
    column_key: (i32, i32, i32),
    /// The same twelve cells by grid anchor (x, y, z): cache index and location.
    cells: [(usize, i32, i32, i32); 12],
    cells_key: (i32, i32, i32),
    column: [(usize, i32, i32); 12],
}

impl Aquifer {
    /// `NoiseBasedAquifer` constructor for the chunk `volume`;
    /// `factory` is the `minecraft:aquifer` positional random.
    pub fn new(config: AquiferConfig, factory: XoroshiroPositionalRandomFactory, volume: &Volume, picker: FluidPicker) -> Self {
        let min_grid_x = grid_x(volume.min[0] - 5);
        let max_grid_x = grid_x(volume.max_block(0) - 5) + 1;
        let grid_size_x = max_grid_x - min_grid_x + 1;
        let min_grid_y = grid_y(volume.min[1] + 1) - 1;
        let max_grid_y = grid_y(volume.max_block(1) + 1) + 1;
        let grid_size_y = max_grid_y - min_grid_y + 1;
        let min_grid_z = grid_x(volume.min[2] - 5);
        let max_grid_z = grid_x(volume.max_block(2) - 5) + 1;
        let grid_size_z = max_grid_z - min_grid_z + 1;
        let total = (grid_size_x * grid_size_y * grid_size_z) as usize;
        let mut a = Self {
            counters: AquiferCounters::default(),
            config,
            factory,
            picker,
            min_grid: [min_grid_x, min_grid_y, min_grid_z],
            grid_size_x,
            grid_size_z,
            location_cache: vec![None; total],
            status_cache: vec![None; total],
            surface_cache: HashMap::new(),
            skip_sampling_above_y: 0,
            should_schedule_fluid_update: false,
            point_cache: PointCache::new(),
            column_key: (i32::MIN, i32::MIN, i32::MIN),
            cells: [(0, 0, 0, 0); 12],
            cells_key: (i32::MIN, i32::MIN, i32::MIN),
            column: [(0, 0, 0); 12],
        };
        let max_surface = a.max_surface_level(
            from_grid_x(min_grid_x, 0),
            from_grid_x(min_grid_z, 0),
            from_grid_x(max_grid_x, 9),
            from_grid_x(max_grid_z, 9),
        );
        let max_adjusted = max_surface + 8;
        let skip_grid_y = grid_y(max_adjusted + 12) + 1;
        a.skip_sampling_above_y = from_grid_y(skip_grid_y, 11) - 1;
        a
    }

    /// Debug view of the caches: (min grid, grid size x, grid size z,
    /// skipSamplingAboveY), computed cells, surface cache.
    pub fn debug_state(&self) -> (([i32; 3], i32, i32, i32), Vec<DebugCell>, Vec<(i32, i32, i32)>) {
        let grid = (self.min_grid, self.grid_size_x, self.grid_size_z, self.skip_sampling_above_y);
        let mut cells = Vec::new();
        for (i, loc) in self.location_cache.iter().enumerate() {
            if let Some(l) = loc {
                let status = self.status_cache[i].map(|s| {
                    (s.level, match s.fluid {
                        Fluid::Air => 1u8,
                        Fluid::Water => 2,
                        Fluid::Lava => 3,
                    })
                });
                cells.push((i, *l, status));
            }
        }
        let mut surfaces: Vec<(i32, i32, i32)> = self.surface_cache.iter().map(|((x, z), l)| (*x, *z, *l)).collect();
        surfaces.sort();
        (grid, cells, surfaces)
    }

    /// `skipSamplingAboveY`: above it every non-solid block is the global fluid.
    pub fn skip_sampling_above_y(&self) -> i32 {
        self.skip_sampling_above_y
    }

    /// Point samples of floodedness, exclusion (at `pos`) and spread
    /// (at the fluid cell of `pos`), for debugging against vanilla.
    pub fn debug_probe(&self, pos: (i32, i32, i32)) -> [f32; 3] {
        let (x, y, z) = pos;
        [
            self.config.floodedness.eval(x, y, z, Mode::Point),
            self.config.exclusion.eval(x, y, z, Mode::Point),
            self.config.spread.eval(x.div_euclid(16), y.div_euclid(40), z.div_euclid(16), Mode::Point),
        ]
    }

    fn max_surface_level(&mut self, min_x: i32, min_z: i32, max_x: i32, max_z: i32) -> i32 {
        let (qx0, qx1) = (min_x >> 2, max_x >> 2);
        let (qz0, qz1) = (min_z >> 2, max_z >> 2);
        let vol = Volume::new([(qx1 - qx0 + 1) as usize, 1, (qz1 - qz0 + 1) as usize], [qx0 << 2, 0, qz0 << 2], [4, 1, 4]);
        let mut ctx = crate::df26::SampleCtx::default();
        let buf = self.config.surface_level.sample_volume_with(&vol, &mut ctx);
        self.point_cache.adopt_volumes(&ctx);
        let mut max_y = i32::MIN;
        for z in 0..vol.size[2] {
            for x in 0..vol.size[0] {
                let level = (buf[vol.index(x, 0, z)] as f64).floor() as i32;
                self.surface_cache.insert((vol.block(0, x), vol.block(2, z)), level);
                max_y = max_y.max(level);
            }
        }
        max_y
    }

    fn surface_level(&mut self, x: i32, z: i32) -> i32 {
        let qx = (x >> 2) << 2;
        let qz = (z >> 2) << 2;
        if let Some(v) = self.surface_cache.get(&(qx, qz)) {
            return *v;
        }
        let v = (self.config.surface_level.eval_with(qx, 0, qz, Mode::Point, &mut self.point_cache) as f64).floor() as i32;
        self.surface_cache.insert((qx, qz), v);
        v
    }

    #[inline]
    fn index(&self, gx: i32, gy: i32, gz: i32) -> usize {
        let x = gx - self.min_grid[0];
        let y = gy - self.min_grid[1];
        let z = gz - self.min_grid[2];
        ((y * self.grid_size_z + z) * self.grid_size_x + x) as usize
    }

    /// `computeSubstance`: `None` means the default block.
    pub fn compute_substance(&mut self, x: i32, y: i32, z: i32, density: f64) -> Option<Substance> {
        if density > 0.0 {
            self.should_schedule_fluid_update = false;
            return None;
        }
        let global = self.picker.compute_fluid(x, y, z);
        if y > self.skip_sampling_above_y {
            self.should_schedule_fluid_update = false;
            return Some(fluid_to_substance(global.at(y)));
        }
        if global.at(y) == Fluid::Lava {
            self.should_schedule_fluid_update = false;
            return Some(Substance::Lava);
        }
        self.counters.deep += 1;
        let y_anchor = grid_y(y + 1);
        if self.column_key != (x, z, y_anchor) {
            self.load_column(x, z, y_anchor);
        }
        // Vanilla's four-slot insertion with `>=` keeps, among equal
        // distances, the later of the twelve first: sort by (d, -k).
        let mut keys = [0u64; 12];
        for (k, &(_, loc_y, dxz2)) in self.column.iter().enumerate() {
            let dy = loc_y - y;
            keys[k] = (((dxz2 + dy * dy) as u64) << 4) | (11 - k) as u64;
        }
        // Two nearest by tournament; most calls return on them alone.
        let mut pair = [(0u64, 0u64); 6];
        for p in 0..6 {
            let (a, b) = (keys[2 * p], keys[2 * p + 1]);
            pair[p] = (a.min(b), a.max(b));
        }
        let (n0, n1) = merge2(merge2(merge2(pair[0], pair[1]), merge2(pair[2], pair[3])), merge2(pair[4], pair[5]));
        let flowing = similarity(100, 144);
        let idx0 = self.column[11 - (n0 & 15) as usize].0;
        let sim12 = similarity((n0 >> 4) as i32, (n1 >> 4) as i32);
        let status1 = self.aquifer_status(idx0);
        let fluid = status1.at(y);
        if sim12 <= 0.0 {
            self.counters.near_exit += 1;
            if sim12 >= flowing {
                let status2 = self.aquifer_status(self.column[11 - (n1 & 15) as usize].0);
                self.should_schedule_fluid_update = status1 != status2;
            } else {
                self.should_schedule_fluid_update = false;
            }
            return Some(fluid_to_substance(fluid));
        }
        // Branchless: each key sinks through the four slots on min/max.
        let mut m = [u64::MAX; 4];
        for &key in &keys {
            let mut a = key;
            for slot in &mut m {
                let lo = (*slot).min(a);
                a = (*slot).max(a);
                *slot = lo;
            }
        }
        let mut dist = [0i32; 4];
        let mut idx = [0usize; 4];
        for s in 0..4 {
            dist[s] = (m[s] >> 4) as i32;
            idx[s] = self.column[11 - (m[s] & 15) as usize].0;
        }
        if fluid == Fluid::Water && self.picker.compute_fluid(x, y - 1, z).at(y - 1) == Fluid::Lava {
            self.should_schedule_fluid_update = true;
            return Some(fluid_to_substance(fluid));
        }
        let mut barrier_noise: Option<f64> = None;
        let status2 = self.aquifer_status(idx[1]);
        let barrier12 = sim12 * self.pressure(x, y, z, &mut barrier_noise, status1, status2);
        if density + barrier12 > 0.0 {
            self.should_schedule_fluid_update = false;
            return None;
        }
        let status3 = self.aquifer_status(idx[2]);
        let sim13 = similarity(dist[0], dist[2]);
        if sim13 > 0.0 {
            let barrier13 = sim12 * sim13 * self.pressure(x, y, z, &mut barrier_noise, status1, status3);
            if density + barrier13 > 0.0 {
                self.should_schedule_fluid_update = false;
                return None;
            }
        }
        let sim23 = similarity(dist[1], dist[2]);
        if sim23 > 0.0 {
            let barrier23 = sim12 * sim23 * self.pressure(x, y, z, &mut barrier_noise, status2, status3);
            if density + barrier23 > 0.0 {
                self.should_schedule_fluid_update = false;
                return None;
            }
        }
        let may_flow12 = status1 != status2;
        let may_flow23 = sim23 >= flowing && status2 != status3;
        let may_flow13 = sim13 >= flowing && status1 != status3;
        if !may_flow12 && !may_flow23 && !may_flow13 {
            self.should_schedule_fluid_update =
                sim13 >= flowing && similarity(dist[0], dist[3]) >= flowing && status1 != self.aquifer_status(idx[3]);
        } else {
            self.should_schedule_fluid_update = true;
        }
        Some(fluid_to_substance(fluid))
    }

    /// Resolve the twelve candidate cells around block column (x, z) at grid row `y_anchor`.
    fn load_column(&mut self, x: i32, z: i32, y_anchor: i32) {
        let anchor = (grid_x(x - 5), y_anchor, grid_x(z - 5));
        if self.cells_key != anchor {
            self.load_cells(anchor);
        }
        // The cells hold for every column of the anchor; only the flat distance moves.
        for (slot, &(index, lx, ly, lz)) in self.column.iter_mut().zip(&self.cells) {
            let dx = lx - x;
            let dz = lz - z;
            *slot = (index, ly, dx * dx + dz * dz);
        }
        self.column_key = (x, z, y_anchor);
    }

    fn load_cells(&mut self, anchor: (i32, i32, i32)) {
        let mut n = 0;
        for x1 in 0..=1 {
            for y1 in -1..=1 {
                for z1 in 0..=1 {
                    let gx = anchor.0 + x1;
                    let gy = anchor.1 + y1;
                    let gz = anchor.2 + z1;
                    let index = self.index(gx, gy, gz);
                    let location = match self.location_cache[index] {
                        Some(l) => l,
                        None => {
                            let mut random = self.factory.at(gx, gy, gz);
                            let l = (
                                from_grid_x(gx, random.next_int_bounded(10)),
                                from_grid_y(gy, random.next_int_bounded(9)),
                                from_grid_x(gz, random.next_int_bounded(10)),
                            );
                            self.location_cache[index] = Some(l);
                            l
                        }
                    };
                    self.cells[n] = (index, location.0, location.1, location.2);
                    n += 1;
                }
            }
        }
        self.cells_key = anchor;
    }

    // The negated comparisons mirror vanilla so NaN takes the same branch.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    fn pressure(&mut self, x: i32, y: i32, z: i32, barrier_noise: &mut Option<f64>, s1: FluidStatus, s2: FluidStatus) -> f64 {
        let t1 = s1.at(y);
        let t2 = s2.at(y);
        if (t1 == Fluid::Lava && t2 == Fluid::Water) || (t1 == Fluid::Water && t2 == Fluid::Lava) {
            return 2.0;
        }
        let diff = (s1.level - s2.level).abs();
        if diff == 0 {
            return 0.0;
        }
        let average = 0.5 * (s1.level + s2.level) as f64;
        let above = y as f64 + 0.5 - average;
        let base = diff as f64 / 2.0;
        let towards_middle = base - above.abs();
        let gradient = if above > 0.0 {
            let c = 0.0 + towards_middle;
            if c > 0.0 { c / 1.5 } else { c / 2.5 }
        } else {
            let c = 3.0 + towards_middle;
            if c > 0.0 { c / 3.0 } else { c / 10.0 }
        };
        let noise = if !(gradient < -2.0) && !(gradient > 2.0) {
            match *barrier_noise {
                Some(v) => v,
                None => {
                    self.counters.barrier_samples += 1;
                    let v = self.config.barrier.eval_with(x, y, z, Mode::Point, &mut self.point_cache) as f64;
                    *barrier_noise = Some(v);
                    v
                }
            }
        } else {
            0.0
        };
        2.0 * (noise + gradient)
    }

    fn aquifer_status(&mut self, index: usize) -> FluidStatus {
        if let Some(s) = self.status_cache[index] {
            return s;
        }
        let (x, y, z) = self.location_cache[index].expect("location computed before status");
        self.counters.status_computes += 1;
        let s = self.compute_fluid(x, y, z);
        self.status_cache[index] = Some(s);
        s
    }

    fn compute_fluid(&mut self, x: i32, y: i32, z: i32) -> FluidStatus {
        let global = self.picker.compute_fluid(x, y, z);
        let mut lowest_surface = i32::MAX;
        let top = y + 12;
        let bottom = y - 12;
        let mut center_under_global = false;
        for off in SURFACE_SAMPLING_OFFSETS_IN_CHUNKS {
            let sx = x + (off[0] << 4);
            let sz = z + (off[1] << 4);
            let surface = self.surface_level(sx, sz);
            let adjusted = surface + 8;
            let start = off[0] == 0 && off[1] == 0;
            if start && bottom > adjusted {
                return global;
            }
            let pokes_above = top > adjusted;
            if pokes_above || start {
                let at_surface = self.picker.compute_fluid(sx, adjusted, sz);
                if at_surface.at(adjusted) != Fluid::Air {
                    if start {
                        center_under_global = true;
                    }
                    if pokes_above {
                        return at_surface;
                    }
                }
            }
            lowest_surface = lowest_surface.min(surface);
        }
        let level = self.compute_surface_level(x, y, z, global, lowest_surface, center_under_global);
        FluidStatus { level, fluid: self.compute_fluid_type(x, y, z, global, level) }
    }

    fn compute_surface_level(&mut self, x: i32, y: i32, z: i32, global: FluidStatus, lowest_surface: i32, center_under_global: bool) -> i32 {
        let (partially, fully);
        if self.config.exclusion.eval_with(x, y, z, Mode::Point, &mut self.point_cache) > 0.0 {
            partially = -1.0;
            fully = -1.0;
        } else {
            let distance_below = (lowest_surface + 8) - y;
            let factor = if center_under_global { clamped_map(distance_below as f64, 0.0, 64.0, 1.0, 0.0) } else { 0.0 };
            let noise = (self.config.floodedness.eval_with(x, y, z, Mode::Point, &mut self.point_cache) as f64).clamp(-1.0, 1.0);
            let fully_threshold = map(factor, 1.0, 0.0, -0.3, 0.8);
            let partially_threshold = map(factor, 1.0, 0.0, -0.8, 0.4);
            partially = noise - partially_threshold;
            fully = noise - fully_threshold;
        }
        if fully > 0.0 {
            global.level
        } else if partially > 0.0 {
            self.randomized_surface_level(x, y, z, lowest_surface)
        } else {
            WAY_BELOW_MIN_Y
        }
    }

    fn randomized_surface_level(&mut self, x: i32, y: i32, z: i32, lowest_surface: i32) -> i32 {
        let cx = x.div_euclid(16);
        let cy = y.div_euclid(40);
        let cz = z.div_euclid(16);
        let middle = cy * 40 + 20;
        let spread = (self.config.spread.eval_with(cx, cy, cz, Mode::Point, &mut self.point_cache) * 10.0f32) as f64;
        let quantized = (spread / 3.0).floor() as i32 * 3;
        lowest_surface.min(middle + quantized)
    }

    fn compute_fluid_type(&mut self, x: i32, y: i32, z: i32, global: FluidStatus, level: i32) -> Fluid {
        let mut fluid = global.fluid;
        if level <= -10 && level != WAY_BELOW_MIN_Y && global.fluid != Fluid::Lava {
            let cx = x.div_euclid(64);
            let cy = y.div_euclid(40);
            let cz = z.div_euclid(64);
            let lava = self.config.lava.eval_with(cx, cy, cz, Mode::Point, &mut self.point_cache) as f64;
            if lava.abs() > 0.3 {
                fluid = Fluid::Lava;
            }
        }
        fluid
    }
}

#[inline]
fn fluid_to_substance(f: Fluid) -> Substance {
    match f {
        Fluid::Air => Substance::Air,
        Fluid::Water => Substance::Water,
        Fluid::Lava => Substance::Lava,
    }
}

/// The fill loop of `NoiseBasedChunkGenerator.doFill`: density buffer
/// plus one substance per block, visiting z, x, then y from the top.
pub fn fill_chunk(final_density: &Node, aquifer: &mut Aquifer, vol: &Volume) -> (Vec<f32>, Vec<u8>) {
    let density = final_density.sample_volume(vol);
    let mut substance = vec![0u8; vol.len()];
    for z in 0..vol.size[2] {
        let bz = vol.block(2, z);
        for x in 0..vol.size[0] {
            let bx = vol.block(0, x);
            for y in (0..vol.size[1]).rev() {
                let by = vol.block(1, y);
                let i = vol.index(x, y, z);
                substance[i] = Substance::oracle_id(aquifer.compute_substance(bx, by, bz, density[i] as f64));
            }
        }
    }
    (density, substance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fluid_status_and_picker() {
        let p = FluidPicker { sea_level: 63, sea_fluid: Fluid::Water };
        assert_eq!(p.compute_fluid(0, -60, 0), FluidStatus { level: -54, fluid: Fluid::Lava });
        assert_eq!(p.compute_fluid(0, 10, 0).at(62), Fluid::Water);
        assert_eq!(p.compute_fluid(0, 10, 0).at(63), Fluid::Air);
    }

    #[test]
    fn helpers_match_java() {
        assert_eq!(grid_y(-1), -1);
        assert_eq!(grid_y(-12), -1);
        assert_eq!(grid_y(-13), -2);
        assert_eq!(grid_x(-1), -1);
        assert_eq!(similarity(100, 144), -0.76);
        assert_eq!(clamped_map(80.0, 0.0, 64.0, 1.0, 0.0), 0.0);
        assert_eq!(map(1.0, 1.0, 0.0, -0.3, 0.8), -0.3);
    }
}
