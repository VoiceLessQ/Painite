//! 26.3 density function tree: JSON loader and f32 reference evaluator.
//!
//! Mirrors the compiled samplers in
//! `net.minecraft.world.level.levelgen.densityfunction.{op,generator}`
//! from 26.3-pre-1, including the constant folding `compileSampler`
//! applies (a division by a constant becomes a multiply by its
//! reciprocal, a subtraction of a constant becomes an addition of its
//! negation) because those change the result bits.
//!
//! Vanilla evaluates a point through `sampleValue` but fills cells
//! through `sampleVolume`, and the two paths associate a few
//! operations differently. `interpolated` samples its cell corners
//! through the volume path, so the evaluator carries a [`Mode`] and
//! reproduces both.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::noise26::{lerp, lerp3, NoiseParameters26, NoiseStack};
use crate::xoroshiro::{XoroshiroPositionalRandomFactory, XoroshiroRandomSource};

type N = Arc<Node>;

/// Which vanilla sampling path the value must match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `DensitySampler.sampleValue`.
    Point,
    /// One element of `DensitySampler.sampleVolume`: leaf arithmetic
    /// follows the volume path. Nodes whose volume path is not
    /// elementwise (`interpolated`) go through [`Node::sample_volume`].
    Volume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    #[inline]
    fn choose(self, x: i32, y: i32, z: i32) -> i32 {
        match self {
            Axis::X => x,
            Axis::Y => y,
            Axis::Z => z,
        }
    }

    fn index(self) -> usize {
        match self {
            Axis::X => 0,
            Axis::Y => 1,
            Axis::Z => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tiling {
    ClampToEdge,
    Repeat,
    MirroredRepeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    Euclidean,
    EuclideanSquared,
    Manhattan,
    Chebyshev,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundKind {
    Floor,
    Round,
    Ceil,
    Truncate,
}

/// `CubicSpline`: a constant or a multipoint spline over a coordinate function.
#[derive(Clone, Debug)]
pub enum Spline {
    Constant(f32),
    Multipoint {
        coordinate: N,
        locations: Vec<f32>,
        values: Vec<Spline>,
        derivatives: Vec<f32>,
    },
}

/// One compiled density function node.
#[derive(Clone, Debug)]
pub enum Node {
    Constant(f32),
    Add(N, N),
    ConstAdd(N, f32),
    Sub(N, N),
    ConstSub(f32, N),
    Mul(N, N),
    ConstMul(N, f32),
    Div(N, N),
    ConstDiv(f32, N),
    Min(N, N),
    ConstMin(N, f32),
    Max(N, N),
    ConstMax(N, f32),
    Abs(N),
    Square(N),
    Cube(N),
    Sqrt(N),
    HalfNegative(N),
    QuarterNegative(N),
    Reciprocal(N),
    Negate(N),
    Squeeze(N),
    Log(N),
    Sign(N),
    Pow(N, N),
    PowConstBase(f64, N),
    PowConstExp(N, f64),
    Round { kind: RoundKind, input: N, multiple: Option<N> },
    /// `NoiseFunction`; shifts are `None` when they compile to `zero()`.
    Noise { stack: Arc<NoiseStack>, xz_scale: f64, y_scale: f64, shift_x: Option<N>, shift_y: Option<N>, shift_z: Option<N> },
    /// `ShiftB`: axes permuted `(z, x, 0)`, times 4.
    ShiftB(Arc<NoiseStack>),
    Gradient { axis: Axis, tiling: Tiling, from_coordinate: i32, to_coordinate: i32, from_value: f32, factor: f32 },
    Lerp { alpha: N, first: N, second: N },
    Clamp { input: N, min: f32, max: f32 },
    RangeChoice { input: N, min_inclusive: f32, max_exclusive: f32, when_in_range: N, when_out_of_range: N },
    IntervalSelect { input: N, thresholds: Vec<f32>, functions: Vec<N> },
    Cache(N),
    BlendDensity(N),
    BlendAlpha,
    BlendOffset,
    Beardifier,
    Interpolated { cell_size_xz: i32, cell_size_y: i32, input: N },
    Slice { axis: Axis, coordinate: i32, input: N },
    FindTopSurface { density: N, upper_bound: N, lower_bound: i32, cell_height: i32 },
    DistanceToPoint { metric: Metric, x: i32, y: i32, z: i32 },
    Spline(Spline),
}

#[inline]
fn floor_mod(a: i32, m: i32) -> i32 {
    a.rem_euclid(m)
}

#[inline]
fn clamp(v: f32, min: f32, max: f32) -> f32 {
    if v < min { min } else { v.min(max) }
}

#[inline]
fn java_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() { f32::NAN } else { a.min(b) }
}

#[inline]
fn java_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() { f32::NAN } else { a.max(b) }
}

#[inline]
fn java_round(v: f32) -> f32 {
    let r = (v as f64 + 0.5).floor();
    r.clamp(i32::MIN as f64, i32::MAX as f64) as i32 as f32
}

#[inline]
fn round_to_integer(v: f32, kind: RoundKind) -> f32 {
    match kind {
        RoundKind::Floor => (v as f64).floor() as f32,
        RoundKind::Round => java_round(v),
        RoundKind::Ceil => (v as f64).ceil() as f32,
        RoundKind::Truncate => {
            if v > 0.0 { (v as f64).floor() as f32 } else { (v as f64).ceil() as f32 }
        }
    }
}

impl Node {
    /// Evaluate at a block position along the given vanilla path.
    pub fn eval(&self, x: i32, y: i32, z: i32, mode: Mode) -> f32 {
        self.eval_with(x, y, z, mode, &mut PointCache::disabled())
    }

    /// `eval` with the single-entry point cache `SamplerContext`
    /// keeps per `cache` node (a repeated query at the same position
    /// is answered from memory).
    pub fn eval_with(&self, x: i32, y: i32, z: i32, mode: Mode, pc: &mut PointCache) -> f32 {
        let volume = mode == Mode::Volume;
        match self {
            Node::Constant(c) => *c,
            Node::Add(l, r) => l.eval_with(x, y, z, mode, pc) + r.eval_with(x, y, z, mode, pc),
            Node::ConstAdd(l, c) => l.eval_with(x, y, z, mode, pc) + c,
            Node::Sub(l, r) => l.eval_with(x, y, z, mode, pc) - r.eval_with(x, y, z, mode, pc),
            Node::ConstSub(c, r) => c - r.eval_with(x, y, z, mode, pc),
            Node::Mul(l, r) => {
                let lv = l.eval_with(x, y, z, mode, pc);
                if !volume && lv == 0.0 { 0.0 } else { lv * r.eval_with(x, y, z, mode, pc) }
            }
            Node::ConstMul(l, c) => l.eval_with(x, y, z, mode, pc) * c,
            Node::Div(l, r) => {
                let lv = l.eval_with(x, y, z, mode, pc);
                if !volume && lv == 0.0 { 0.0 } else { lv / r.eval_with(x, y, z, mode, pc) }
            }
            Node::ConstDiv(c, r) => c / r.eval_with(x, y, z, mode, pc),
            Node::Min(l, r) => java_min(l.eval_with(x, y, z, mode, pc), r.eval_with(x, y, z, mode, pc)),
            Node::ConstMin(l, c) => java_min(l.eval_with(x, y, z, mode, pc), *c),
            Node::Max(l, r) => java_max(l.eval_with(x, y, z, mode, pc), r.eval_with(x, y, z, mode, pc)),
            Node::ConstMax(l, c) => java_max(l.eval_with(x, y, z, mode, pc), *c),
            Node::Abs(i) => i.eval_with(x, y, z, mode, pc).abs(),
            Node::Square(i) => {
                let v = i.eval_with(x, y, z, mode, pc);
                v * v
            }
            Node::Cube(i) => {
                let v = i.eval_with(x, y, z, mode, pc);
                v * v * v
            }
            Node::Sqrt(i) => (i.eval_with(x, y, z, mode, pc) as f64).sqrt() as f32,
            Node::HalfNegative(i) => {
                let v = i.eval_with(x, y, z, mode, pc);
                if v > 0.0 { v } else { v * 0.5 }
            }
            Node::QuarterNegative(i) => {
                let v = i.eval_with(x, y, z, mode, pc);
                if v > 0.0 { v } else { v * 0.25 }
            }
            Node::Reciprocal(i) => 1.0 / i.eval_with(x, y, z, mode, pc),
            Node::Negate(i) => -i.eval_with(x, y, z, mode, pc),
            Node::Squeeze(i) => {
                let c = clamp(i.eval_with(x, y, z, mode, pc), -1.0, 1.0);
                c / 2.0 - c * c * c / 24.0
            }
            Node::Log(i) => (i.eval_with(x, y, z, mode, pc) as f64).ln() as f32,
            Node::Sign(i) => {
                let v = i.eval_with(x, y, z, mode, pc);
                if v.is_nan() || v == 0.0 { v } else { v.signum() }
            }
            Node::Pow(b, e) => (b.eval_with(x, y, z, mode, pc) as f64).powf(e.eval_with(x, y, z, mode, pc) as f64) as f32,
            Node::PowConstBase(b, e) => b.powf(e.eval_with(x, y, z, mode, pc) as f64) as f32,
            Node::PowConstExp(b, e) => (b.eval_with(x, y, z, mode, pc) as f64).powf(*e) as f32,
            Node::Round { kind, input, multiple } => {
                let v = input.eval_with(x, y, z, mode, pc);
                match multiple {
                    None => round_to_integer(v, *kind),
                    Some(m) => {
                        let m = m.eval_with(x, y, z, mode, pc);
                        if m == 0.0 { v } else { round_to_integer(v / m, *kind) * m }
                    }
                }
            }
            Node::Noise { stack, xz_scale, y_scale, shift_x, shift_y, shift_z } => {
                match (shift_x, shift_y, shift_z) {
                    (None, None, None) => {
                        if volume {
                            stack.get_volume(x, y, z, *xz_scale, *y_scale, 1.0)
                        } else {
                            stack.get(x as f64 * xz_scale, y as f64 * y_scale, z as f64 * xz_scale)
                        }
                    }
                    _ => {
                        let sx = shift_x.as_ref().map_or(0.0, |s| s.eval_with(x, y, z, mode, pc)) as f64;
                        let sz = shift_z.as_ref().map_or(0.0, |s| s.eval_with(x, y, z, mode, pc)) as f64;
                        let nx = x as f64 * xz_scale + sx;
                        let ny = match shift_y {
                            None => y as f64 * y_scale,
                            Some(s) => y as f64 * y_scale + s.eval_with(x, y, z, mode, pc) as f64,
                        };
                        let nz = z as f64 * xz_scale + sz;
                        stack.get(nx, ny, nz)
                    }
                }
            }
            Node::ShiftB(stack) => {
                if volume {
                    stack.get_volume(z, x, 0, 0.25, 0.25, 4.0)
                } else {
                    stack.get(z as f64 * 0.25, x as f64 * 0.25, 0.0) * 4.0
                }
            }
            Node::Gradient { axis, tiling, from_coordinate, to_coordinate, from_value, factor } => {
                let c = axis.choose(x, y, z);
                let range = to_coordinate - from_coordinate;
                match tiling {
                    Tiling::ClampToEdge => {
                        let lo = (*from_coordinate).min(*to_coordinate);
                        let hi = (*from_coordinate).max(*to_coordinate);
                        let rel = c.clamp(lo, hi) - from_coordinate;
                        from_value + rel as f32 * factor
                    }
                    Tiling::Repeat => {
                        let rel = c - from_coordinate;
                        from_value + java_floor_mod(rel, range) as f32 * factor
                    }
                    Tiling::MirroredRepeat => {
                        let rel = c - from_coordinate;
                        let tile = java_floor_div(rel, range);
                        let local = rel - tile * range;
                        if (tile & 1) == 0 {
                            from_value + local as f32 * factor
                        } else {
                            from_value + (range - local) as f32 * factor
                        }
                    }
                }
            }
            Node::Lerp { alpha, first, second } => {
                let a = alpha.eval_with(x, y, z, mode, pc);
                if a == 0.0 {
                    first.eval_with(x, y, z, mode, pc)
                } else if a == 1.0 {
                    second.eval_with(x, y, z, mode, pc)
                } else {
                    lerp(a, first.eval_with(x, y, z, mode, pc), second.eval_with(x, y, z, mode, pc))
                }
            }
            Node::Clamp { input, min, max } => clamp(input.eval_with(x, y, z, mode, pc), *min, *max),
            Node::RangeChoice { input, min_inclusive, max_exclusive, when_in_range, when_out_of_range } => {
                let v = input.eval_with(x, y, z, mode, pc);
                if v >= *min_inclusive && v < *max_exclusive {
                    when_in_range.eval_with(x, y, z, mode, pc)
                } else {
                    when_out_of_range.eval_with(x, y, z, mode, pc)
                }
            }
            Node::IntervalSelect { input, thresholds, functions } => {
                let v = input.eval_with(x, y, z, mode, pc);
                let mut idx = functions.len() - 1;
                for (i, t) in thresholds.iter().enumerate() {
                    if v < *t {
                        idx = i;
                        break;
                    }
                }
                functions[idx].eval_with(x, y, z, mode, pc)
            }
            Node::BlendDensity(i) => i.eval_with(x, y, z, mode, pc),
            Node::Cache(i) => {
                if !pc.enabled {
                    return i.eval_with(x, y, z, mode, pc);
                }
                let key = Arc::as_ptr(i) as usize;
                let pos = ((x as i64) << 42) ^ ((z as i64 & 0x3f_ffff) << 20) ^ (y as i64 & 0xf_ffff);
                if let Some((p, v)) = pc.entries.get(&key) {
                    if *p == pos && !v.is_nan() {
                        return *v;
                    }
                }
                if let Some(v) = pc.volume_hit(key, x, y, z) {
                    return v;
                }
                let v = i.eval_with(x, y, z, mode, pc);
                pc.entries.insert(key, (pos, v));
                v
            }
            Node::BlendAlpha => 1.0,
            Node::BlendOffset | Node::Beardifier => 0.0,
            Node::Interpolated { cell_size_xz, cell_size_y, input } => {
                assert!(!volume, "interpolated has no elementwise volume path; use sample_volume");
                let xi = floor_mod(x, *cell_size_xz);
                let yi = floor_mod(y, *cell_size_y);
                let zi = floor_mod(z, *cell_size_xz);
                if xi == 0 && yi == 0 && zi == 0 {
                    return input.eval_with(x, y, z, Mode::Point, pc);
                }
                let vol = Volume::new([2, 2, 2], [x - xi, y - yi, z - zi], [*cell_size_xz, *cell_size_y, *cell_size_xz]);
                let b = input.sample_volume(&vol);
                let v = |ix: usize, iy: usize, iz: usize| b[vol.index(ix, iy, iz)];
                lerp3(
                    xi as f32 / *cell_size_xz as f32,
                    yi as f32 / *cell_size_y as f32,
                    zi as f32 / *cell_size_xz as f32,
                    v(0, 0, 0), v(1, 0, 0), v(0, 1, 0), v(1, 1, 0),
                    v(0, 0, 1), v(1, 0, 1), v(0, 1, 1), v(1, 1, 1),
                )
            }
            Node::Slice { axis, coordinate, input } => match axis {
                Axis::X => input.eval_with(*coordinate, y, z, mode, pc),
                Axis::Y => input.eval_with(x, *coordinate, z, mode, pc),
                Axis::Z => input.eval_with(x, y, *coordinate, mode, pc),
            },
            Node::FindTopSurface { density, upper_bound, lower_bound, cell_height } => {
                // The probe loop always samples density through the point path.
                let upper = upper_bound.eval_with(x, 0, z, mode, pc);
                Self::find_surface_from_with(density, x, z, upper, *lower_bound, *cell_height, pc)
            }
            Node::DistanceToPoint { metric, x: px, y: py, z: pz } => {
                let dx = (px - x) as f32;
                let dy = (py - y) as f32;
                let dz = (pz - z) as f32;
                match metric {
                    Metric::Euclidean => ((dx * dx + dy * dy + dz * dz) as f64).sqrt() as f32,
                    Metric::EuclideanSquared => dx * dx + dy * dy + dz * dz,
                    Metric::Manhattan => dx.abs() + dy.abs() + dz.abs(),
                    Metric::Chebyshev => java_max(java_max(dx.abs(), dy.abs()), dz.abs()),
                }
            }
            Node::Spline(s) => s.sample(x, y, z, mode, pc),
        }
    }
}

/// A sampling volume: `DensityVolume(sizeX, sizeY, sizeZ, min..., step...)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Volume {
    pub size: [usize; 3],
    pub min: [i32; 3],
    pub step: [i32; 3],
}

impl Volume {
    pub fn new(size: [usize; 3], min: [i32; 3], step: [i32; 3]) -> Self {
        assert!(size.iter().all(|&s| s > 0) && step.iter().all(|&s| s > 0));
        Self { size, min, step }
    }

    /// `NoiseBasedChunkGenerator.chunkVolume`.
    pub fn chunk(chunk_x: i32, chunk_z: i32, min_y: i32, height: i32) -> Self {
        Self::new([16, height as usize, 16], [chunk_x * 16, min_y, chunk_z * 16], [1, 1, 1])
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.size[0] * self.size[1] * self.size[2]
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `DensityVolume.indexUnchecked`.
    #[inline]
    pub fn index(&self, x: usize, y: usize, z: usize) -> usize {
        y + (x + z * self.size[0]) * self.size[1]
    }

    #[inline]
    pub fn block(&self, axis: usize, i: usize) -> i32 {
        self.min[axis] + i as i32 * self.step[axis]
    }

    #[inline]
    pub fn max_block(&self, axis: usize) -> i32 {
        self.min[axis] + self.size[axis] as i32 * self.step[axis] - 1
    }

    /// Same x/z extent and step, and `other`'s rows lie on this volume's y lattice.
    pub fn covers_rows(&self, other: &Volume) -> bool {
        self.step == other.step
            && self.min[0] == other.min[0]
            && self.min[2] == other.min[2]
            && self.size[0] == other.size[0]
            && self.size[2] == other.size[2]
            && other.min[1] >= self.min[1]
            && (other.min[1] - self.min[1]) % self.step[1] == 0
            && other.max_block(1) <= self.max_block(1)
    }

    /// `DensityVolume.indexOfBlock`: the element holding this block, if
    /// the block lies on the volume's step lattice.
    #[inline]
    pub fn index_of_block(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        let rel = [x - self.min[0], y - self.min[1], z - self.min[2]];
        if self.step == [1, 1, 1] {
            let inside = (0..3).all(|a| rel[a] >= 0 && rel[a] < self.size[a] as i32);
            return inside.then(|| self.index(rel[0] as usize, rel[1] as usize, rel[2] as usize));
        }
        for a in 0..3 {
            if rel[a] < 0 || rel[a] >= self.size[a] as i32 * self.step[a] || rel[a].rem_euclid(self.step[a]) != 0 {
                return None;
            }
        }
        Some(self.index((rel[0] / self.step[0]) as usize, (rel[1] / self.step[1]) as usize, (rel[2] / self.step[2]) as usize))
    }

    /// Visit every element in buffer order with its block position.
    #[inline]
    pub fn for_each(&self, mut f: impl FnMut(usize, i32, i32, i32)) {
        let mut i = 0;
        for z in 0..self.size[2] {
            let bz = self.block(2, z);
            for x in 0..self.size[0] {
                let bx = self.block(0, x);
                for y in 0..self.size[1] {
                    f(i, bx, self.block(1, y), bz);
                    i += 1;
                }
            }
        }
    }
}

/// Single-entry point cache per `cache` node, as `SamplerContext`
/// keeps for `sampleValueCached`.
#[derive(Default)]
pub struct PointCache {
    enabled: bool,
    entries: HashMap<usize, (i64, f32)>,
    /// Volume buffers a point query may be answered from, as
    /// `sampleValueCached` does with the cache cell's last volume.
    volumes: Vec<(usize, Volume, Arc<Vec<f32>>)>,
}

impl PointCache {
    pub fn new() -> Self {
        Self { enabled: true, entries: HashMap::new(), volumes: Vec::new() }
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    /// Adopt the volume buffers of a sampling context (latest per node).
    pub fn adopt_volumes(&mut self, ctx: &SampleCtx) {
        for ((node, vol), buf) in &ctx.volumes {
            self.volumes.retain(|(n, _, _)| n != node);
            self.volumes.push((*node, *vol, buf.clone()));
        }
    }

    fn volume_hit(&self, node: usize, x: i32, y: i32, z: i32) -> Option<f32> {
        for (n, vol, buf) in &self.volumes {
            if *n == node {
                return vol.index_of_block(x, y, z).map(|i| buf[i]);
            }
        }
        None
    }
}

/// Per-call state for [`Node::sample_volume`]: `cache` buffers keyed by
/// the cached input and the volume, as `SamplerContext` keeps them.
#[derive(Default)]
pub struct SampleCtx {
    pub(crate) volumes: HashMap<(usize, Volume), Arc<Vec<f32>>>,
    pool: Vec<Vec<f32>>,
    /// Nanoseconds spent inside noise leaves (diagnostic, see FillStats).
    pub noise_ns: u64,
    /// Nanoseconds spent inside spline nodes, including their inputs.
    pub spline_ns: u64,
}

/// One side of a `range_choice` over a volume.
enum RowsSample {
    Const(f32),
    Buf(Vec<f32>),
}

impl SampleCtx {
    /// A zero-filled buffer of `len`, reused from the pool when possible.
    fn zeroed(&mut self, len: usize) -> Vec<f32> {
        match self.pool.pop() {
            Some(mut v) => {
                v.clear();
                v.resize(len, 0.0);
                v
            }
            None => vec![0.0; len],
        }
    }

    fn filled(&mut self, len: usize, value: f32) -> Vec<f32> {
        let mut v = self.zeroed(len);
        if value != 0.0 {
            v.fill(value);
        }
        v
    }

    fn give(&mut self, v: Vec<f32>) {
        if self.pool.len() < 64 {
            self.pool.push(v);
        }
    }
}

impl Node {
    fn find_surface_from(density: &Node, x: i32, z: i32, upper: f32, lower_bound: i32, cell_height: i32) -> f32 {
        Self::find_surface_from_with(density, x, z, upper, lower_bound, cell_height, &mut PointCache::disabled())
    }

    fn find_surface_from_with(density: &Node, x: i32, z: i32, upper: f32, lower_bound: i32, cell_height: i32, pc: &mut PointCache) -> f32 {
        let top = ((upper / cell_height as f32) as f64).floor() as i32 * cell_height;
        if top <= lower_bound {
            return lower_bound as f32;
        }
        let mut probe = top;
        while probe >= lower_bound {
            if density.eval_with(x, probe, z, Mode::Point, pc) > 0.0 {
                return probe as f32;
            }
            probe -= cell_height;
        }
        lower_bound as f32
    }

    /// `DensitySampler.sampleVolume`: the whole buffer for a volume,
    /// with `cache` nodes memoised per volume like `SamplerContext`.
    pub fn sample_volume(&self, vol: &Volume) -> Vec<f32> {
        let mut ctx = SampleCtx::default();
        self.sample_volume_ctx(vol, &mut ctx)
    }

    /// `sample_volume` sharing a caller-owned context across calls.
    pub fn sample_volume_with(&self, vol: &Volume, ctx: &mut SampleCtx) -> Vec<f32> {
        self.sample_volume_ctx(vol, ctx)
    }

    /// `node` over the points where `needed(alpha)` holds; the rest of the
    /// buffer is never read by the caller. Below `MASKED_POINT_RATIO` a
    /// plain noise leaf takes the point path over just those points; a
    /// side nobody reads is not sampled at all.
    fn sample_where(node: &Node, vol: &Volume, ctx: &mut SampleCtx, alpha: &[f32], needed: impl Fn(f32) -> bool) -> Vec<f32> {
        let count = alpha.iter().filter(|&&v| needed(v)).count();
        if count == 0 {
            return ctx.zeroed(alpha.len());
        }
        let sparse = count * MASKED_POINT_RATIO.1 < alpha.len() * MASKED_POINT_RATIO.0;
        if let (true, Node::Noise { stack, xz_scale, y_scale, shift_x: None, shift_y: None, shift_z: None }) = (sparse, node) {
            let mut out = ctx.zeroed(alpha.len());
            let t = std::time::Instant::now();
            vol.for_each(|i, x, y, z| {
                if needed(alpha[i]) {
                    out[i] = stack.get(x as f64 * xz_scale, y as f64 * y_scale, z as f64 * xz_scale);
                }
            });
            ctx.noise_ns += t.elapsed().as_nanos() as u64;
            return out;
        }
        node.sample_volume_ctx(vol, ctx)
    }

    fn sample_volume_ctx(&self, vol: &Volume, ctx: &mut SampleCtx) -> Vec<f32> {
        match self {
            Node::Interpolated { cell_size_xz, cell_size_y, input } => {
                let cells = [*cell_size_xz, *cell_size_y, *cell_size_xz];
                let aligned = (0..3).all(|a| (vol.step[a] == cells[a] || vol.size[a] == 1) && java_floor_mod(vol.min[a], cells[a]) == 0);
                if aligned {
                    input.sample_volume_ctx(vol, ctx)
                } else if vol.step == [1, 1, 1] {
                    Self::sample_with_block_step(input, vol, *cell_size_xz, *cell_size_y, ctx)
                } else {
                    let block = Volume::new(
                        [vol.size[0] * vol.step[0] as usize, vol.size[1] * vol.step[1] as usize, vol.size[2] * vol.step[2] as usize],
                        vol.min,
                        [1, 1, 1],
                    );
                    let b = Self::sample_with_block_step(input, &block, *cell_size_xz, *cell_size_y, ctx);
                    let mut out = ctx.zeroed(vol.len());
                    for z in 0..vol.size[2] {
                        for x in 0..vol.size[0] {
                            for y in 0..vol.size[1] {
                                out[vol.index(x, y, z)] =
                                    b[block.index(x * vol.step[0] as usize, y * vol.step[1] as usize, z * vol.step[2] as usize)];
                            }
                        }
                    }
                    out
                }
            }
            Node::BlendDensity(i) => i.sample_volume_ctx(vol, ctx),
            Node::Cache(i) => {
                let key = (Arc::as_ptr(i) as usize, *vol);
                if let Some(b) = ctx.volumes.get(&key).cloned() {
                    let mut out = ctx.zeroed(b.len());
                    out.copy_from_slice(&b);
                    return out;
                }
                // A cached volume holding these rows on the same lattice serves them by copy.
                let covering = ctx.volumes.iter().find(|((p, v), _)| *p == key.0 && v.covers_rows(vol)).map(|((_, v), b)| (*v, b.clone()));
                if let Some((big, b)) = covering {
                    let rows = vol.size[1];
                    let off = ((vol.min[1] - big.min[1]) / big.step[1]) as usize;
                    let mut out = ctx.zeroed(vol.len());
                    for col in 0..vol.size[0] * vol.size[2] {
                        let src = col * big.size[1] + off;
                        out[col * rows..(col + 1) * rows].copy_from_slice(&b[src..src + rows]);
                    }
                    return out;
                }
                let b = Arc::new(i.sample_volume_ctx(vol, ctx));
                ctx.volumes.insert(key, b.clone());
                let mut out = ctx.zeroed(b.len());
                out.copy_from_slice(&b);
                out
            }
            Node::Slice { axis, coordinate, input } => {
                let a = axis.index();
                let mut thin = *vol;
                thin.size[a] = 1;
                thin.min[a] = *coordinate;
                let b = input.sample_volume_ctx(&thin, ctx);
                let mut out = ctx.zeroed(vol.len());
                for z in 0..vol.size[2] {
                    for x in 0..vol.size[0] {
                        for y in 0..vol.size[1] {
                            let (tx, ty, tz) = match axis {
                                Axis::X => (0, y, z),
                                Axis::Y => (x, 0, z),
                                Axis::Z => (x, y, 0),
                            };
                            out[vol.index(x, y, z)] = b[thin.index(tx, ty, tz)];
                        }
                    }
                }
                ctx.give(b);
                out
            }
            Node::FindTopSurface { density, upper_bound, lower_bound, cell_height } => {
                assert_eq!(vol.size[1], 1, "find_top_surface: cannot sample with sizeY != 1");
                let mut out = upper_bound.sample_volume_ctx(vol, ctx);
                let mut pc = PointCache::new();
                pc.adopt_volumes(ctx);
                vol.for_each(|i, x, _, z| out[i] = Self::find_surface_from_with(density, x, z, out[i], *lower_bound, *cell_height, &mut pc));
                out
            }
            Node::Spline(s) => {
                let mut coords = HashMap::new();
                let t = std::time::Instant::now();
                let out = s.sample_volume_ctx(vol, ctx, &mut coords);
                ctx.spline_ns += t.elapsed().as_nanos() as u64;
                out
            }
            Node::Add(l, r) | Node::Sub(l, r) | Node::Mul(l, r) | Node::Div(l, r) | Node::Min(l, r) | Node::Max(l, r) | Node::Pow(l, r) => {
                let mut a = l.sample_volume_ctx(vol, ctx);
                let b = r.sample_volume_ctx(vol, ctx);
                for (x, y) in a.iter_mut().zip(&b) {
                    *x = match self {
                        Node::Add(..) => *x + y,
                        Node::Sub(..) => *x - y,
                        Node::Mul(..) => *x * y,
                        Node::Div(..) => *x / y,
                        Node::Min(..) => java_min(*x, *y),
                        Node::Max(..) => java_max(*x, *y),
                        _ => (*x as f64).powf(*y as f64) as f32,
                    };
                }
                ctx.give(b);
                a
            }
            Node::ConstAdd(i, c) | Node::ConstMul(i, c) | Node::ConstMin(i, c) | Node::ConstMax(i, c) => {
                let mut a = i.sample_volume_ctx(vol, ctx);
                for x in a.iter_mut() {
                    *x = match self {
                        Node::ConstAdd(..) => *x + c,
                        Node::ConstMul(..) => *x * c,
                        Node::ConstMin(..) => java_min(*x, *c),
                        _ => java_max(*x, *c),
                    };
                }
                a
            }
            Node::ConstSub(c, i) | Node::ConstDiv(c, i) => {
                let mut a = i.sample_volume_ctx(vol, ctx);
                for x in a.iter_mut() {
                    *x = if matches!(self, Node::ConstSub(..)) { c - *x } else { c / *x };
                }
                a
            }
            Node::PowConstBase(c, i) => {
                let mut a = i.sample_volume_ctx(vol, ctx);
                for x in a.iter_mut() {
                    *x = c.powf(*x as f64) as f32;
                }
                a
            }
            Node::PowConstExp(i, c) => {
                let mut a = i.sample_volume_ctx(vol, ctx);
                for x in a.iter_mut() {
                    *x = (*x as f64).powf(*c) as f32;
                }
                a
            }
            Node::Abs(i) | Node::Square(i) | Node::Cube(i) | Node::Sqrt(i) | Node::HalfNegative(i) | Node::QuarterNegative(i)
            | Node::Reciprocal(i) | Node::Negate(i) | Node::Squeeze(i) | Node::Log(i) | Node::Sign(i) => {
                let mut a = i.sample_volume_ctx(vol, ctx);
                for v in a.iter_mut() {
                    *v = Self::unary(self, *v);
                }
                a
            }
            Node::Round { kind, input, multiple } => {
                let mut a = input.sample_volume_ctx(vol, ctx);
                match multiple {
                    None => a.iter_mut().for_each(|v| *v = round_to_integer(*v, *kind)),
                    Some(m) => {
                        let m = m.sample_volume_ctx(vol, ctx);
                        for (v, m) in a.iter_mut().zip(&m) {
                            *v = if *m == 0.0 { *v } else { round_to_integer(*v / m, *kind) * m };
                        }
                    }
                }
                a
            }
            Node::Noise { stack, xz_scale, y_scale, shift_x, shift_y, shift_z } => {
                let mut out = ctx.zeroed(vol.len());
                match (shift_x, shift_y, shift_z) {
                    (None, None, None) => {
                        let t = std::time::Instant::now();
                        stack.add_to_volume(&mut out, vol.size, vol.min, vol.step, *xz_scale, *y_scale, 1.0);
                        ctx.noise_ns += t.elapsed().as_nanos() as u64;
                    }
                    _ => {
                        let sx = shift_x.as_ref().map(|s| s.sample_volume_ctx(vol, ctx));
                        let sy = shift_y.as_ref().map(|s| s.sample_volume_ctx(vol, ctx));
                        let sz = shift_z.as_ref().map(|s| s.sample_volume_ctx(vol, ctx));
                        let t = std::time::Instant::now();
                        vol.for_each(|i, x, y, z| {
                            let nx = x as f64 * xz_scale + sx.as_ref().map_or(0.0, |b| b[i] as f64);
                            let ny = y as f64 * y_scale + sy.as_ref().map_or(0.0, |b| b[i] as f64);
                            let nz = z as f64 * xz_scale + sz.as_ref().map_or(0.0, |b| b[i] as f64);
                            out[i] = stack.get(nx, ny, nz);
                        });
                        ctx.noise_ns += t.elapsed().as_nanos() as u64;
                    }
                }
                out
            }
            Node::Lerp { alpha, first, second } => {
                let mut a = alpha.sample_volume_ctx(vol, ctx);
                // A side is only read where alpha does not saturate to the other one.
                let f = Self::sample_where(first, vol, ctx, &a, |v| v != 1.0);
                let s = Self::sample_where(second, vol, ctx, &a, |v| v != 0.0);
                for i in 0..a.len() {
                    a[i] = if a[i] == 0.0 { f[i] } else if a[i] == 1.0 { s[i] } else { lerp(a[i], f[i], s[i]) };
                }
                ctx.give(f);
                ctx.give(s);
                a
            }
            Node::RangeChoice { input, min_inclusive, max_exclusive, when_in_range, when_out_of_range } => {
                let (lo, hi) = (*min_inclusive, *max_exclusive);
                let rows = vol.size[1];
                // Which rows read each branch; `minecraft:y` needs no input volume.
                let mut need_in = vec![0u8; rows];
                let mut need_out = vec![0u8; rows];
                let v = if let Node::Gradient { axis: Axis::Y, .. } = input.as_ref() {
                    for y in 0..rows {
                        let e = input.eval(0, vol.block(1, y), 0, Mode::Volume);
                        need_in[y] = (e >= lo && e < hi) as u8;
                        need_out[y] = 1 - need_in[y];
                    }
                    None
                } else {
                    let v = input.sample_volume_ctx(vol, ctx);
                    for column in v.chunks_exact(rows) {
                        for y in 0..rows {
                            let inside = (column[y] >= lo) & (column[y] < hi);
                            need_in[y] |= inside as u8;
                            need_out[y] |= !inside as u8;
                        }
                    }
                    Some(v)
                };
                let (all_in, all_out) = (!need_out.contains(&1), !need_in.contains(&1));
                if all_in || all_out {
                    if let Some(v) = v {
                        ctx.give(v);
                    }
                    let branch = if all_out { when_out_of_range } else { when_in_range };
                    return branch.sample_volume_ctx(vol, ctx);
                }
                let by_rows = rows >= 16 && vol.step[1] == 1;
                let a = Self::sample_needed_rows(when_in_range, &need_in, by_rows, vol, ctx);
                let o = Self::sample_needed_rows(when_out_of_range, &need_out, by_rows, vol, ctx);
                // Merge onto whichever side has a buffer; a constant side is written over it.
                let (mut out, from_a, c) = match (a, o) {
                    (RowsSample::Buf(a), RowsSample::Buf(o)) => {
                        let mut out = a;
                        match &v {
                            Some(v) => {
                                for i in 0..out.len() {
                                    if !(v[i] >= lo && v[i] < hi) {
                                        out[i] = o[i];
                                    }
                                }
                            }
                            None => {
                                for (dst, src) in out.chunks_exact_mut(rows).zip(o.chunks_exact(rows)) {
                                    for y in 0..rows {
                                        if need_out[y] == 1 {
                                            dst[y] = src[y];
                                        }
                                    }
                                }
                            }
                        }
                        ctx.give(o);
                        if let Some(v) = v {
                            ctx.give(v);
                        }
                        return out;
                    }
                    (RowsSample::Buf(a), RowsSample::Const(c)) => (a, true, c),
                    (RowsSample::Const(c), RowsSample::Buf(o)) => (o, false, c),
                    (RowsSample::Const(ca), RowsSample::Const(co)) => (ctx.filled(vol.len(), ca), true, co),
                };
                match v {
                    Some(v) => {
                        for i in 0..out.len() {
                            if (v[i] >= lo && v[i] < hi) != from_a {
                                out[i] = c;
                            }
                        }
                        ctx.give(v);
                    }
                    None => {
                        let need = if from_a { &need_out } else { &need_in };
                        for column in out.chunks_exact_mut(rows) {
                            for y in 0..rows {
                                if need[y] == 1 {
                                    column[y] = c;
                                }
                            }
                        }
                    }
                }
                out
            }
            Node::IntervalSelect { input, thresholds, functions } => {
                let v = input.sample_volume_ctx(vol, ctx);
                let bufs: Vec<Vec<f32>> = functions.iter().map(|f| f.sample_volume_ctx(vol, ctx)).collect();
                let mut out = ctx.zeroed(vol.len());
                for i in 0..out.len() {
                    let mut idx = functions.len() - 1;
                    for (t, th) in thresholds.iter().enumerate() {
                        if v[i] < *th {
                            idx = t;
                            break;
                        }
                    }
                    out[i] = bufs[idx][i];
                }
                ctx.give(v);
                for b in bufs {
                    ctx.give(b);
                }
                out
            }
            Node::Clamp { input, min, max } => {
                let mut a = input.sample_volume_ctx(vol, ctx);
                for v in a.iter_mut() {
                    *v = clamp(*v, *min, *max);
                }
                a
            }
            // Leaves whose volume path is elementwise.
            Node::ShiftB(stack) => {
                // Transposed volume: noise x from block z, noise y from block x, z = 0.
                let t_size = [vol.size[2], vol.size[0], 1];
                let t_min = [vol.min[2], vol.min[0], 0];
                let t_step = [vol.step[2], vol.step[0], 1];
                let mut t = ctx.zeroed(t_size[0] * t_size[1]);
                let t0 = std::time::Instant::now();
                stack.add_to_volume(&mut t, t_size, t_min, t_step, 0.25, 0.25, 4.0);
                ctx.noise_ns += t0.elapsed().as_nanos() as u64;
                let mut out = ctx.zeroed(vol.len());
                let t_index = |nx: usize, ny: usize| ny + nx * t_size[1];
                for z in 0..vol.size[2] {
                    for x in 0..vol.size[0] {
                        let v = t[t_index(z, x)];
                        for y in 0..vol.size[1] {
                            out[vol.index(x, y, z)] = v;
                        }
                    }
                }
                ctx.give(t);
                out
            }
            Node::Constant(c) => {
                let mut out = ctx.zeroed(vol.len());
                out.fill(*c);
                out
            }
            Node::Gradient { axis, .. } => {
                // One value per coordinate along the axis, copied across the other two.
                let a = axis.index();
                let table: Vec<f32> = (0..vol.size[a])
                    .map(|k| {
                        let c = vol.block(a, k);
                        self.eval(if a == 0 { c } else { 0 }, if a == 1 { c } else { 0 }, if a == 2 { c } else { 0 }, Mode::Volume)
                    })
                    .collect();
                let mut out = ctx.zeroed(vol.len());
                let mut i = 0;
                for z in 0..vol.size[2] {
                    for x in 0..vol.size[0] {
                        for y in 0..vol.size[1] {
                            out[i] = table[[x, y, z][a]];
                            i += 1;
                        }
                    }
                }
                out
            }
            Node::BlendAlpha | Node::BlendOffset | Node::Beardifier | Node::DistanceToPoint { .. } => {
                let mut out = ctx.zeroed(vol.len());
                vol.for_each(|i, x, y, z| out[i] = self.eval(x, y, z, Mode::Volume));
                out
            }
        }
    }

    fn unary(op: &Node, v: f32) -> f32 {
        match op {
            Node::Abs(_) => v.abs(),
            Node::Square(_) => v * v,
            Node::Cube(_) => v * v * v,
            Node::Sqrt(_) => (v as f64).sqrt() as f32,
            Node::HalfNegative(_) => if v > 0.0 { v } else { v * 0.5 },
            Node::QuarterNegative(_) => if v > 0.0 { v } else { v * 0.25 },
            Node::Reciprocal(_) => 1.0 / v,
            Node::Negate(_) => -v,
            Node::Squeeze(_) => {
                let c = clamp(v, -1.0, 1.0);
                c / 2.0 - c * c * c / 24.0
            }
            Node::Log(_) => (v as f64).ln() as f32,
            Node::Sign(_) => if v.is_nan() || v == 0.0 { v } else { v.signum() },
            _ => unreachable!(),
        }
    }

    /// `InterpolatedFunction.Sampler.sampleWithBlockStep`: evaluate the
    /// child on the cell corner grid, then `fillCell` every cell.
    /// `range_choice` when the input is uniform per y row (a function of
    /// y alone, as the ore vein and cave shapers use): each branch is
    /// sampled only on the rows that select it, widened to whole 8-block
    /// cells so interpolated inputs see the same cells they would in a
    /// full-volume pass. Same values as the full pass, fewer of them.
    /// `None` when rows are mixed or the volume is a single row.
    /// A `range_choice` branch over the rows that read it, widened to
    /// whole 8-block cells; a point nobody reads is left at zero.
    fn sample_needed_rows(branch: &Node, need: &[u8], by_rows: bool, vol: &Volume, ctx: &mut SampleCtx) -> RowsSample {
        if let Node::Constant(c) = branch {
            return RowsSample::Const(*c);
        }
        if !by_rows {
            return RowsSample::Buf(branch.sample_volume_ctx(vol, ctx));
        }
        let rows = vol.size[1];
        let mut out = ctx.zeroed(vol.len());
        let mut y = 0;
        while y < rows {
            if need[y] == 0 {
                y += 1;
                continue;
            }
            // Runs closer than two cells share one sample: a corner grid per run costs more than the gap.
            let mut end = y;
            while end < rows && need[end..(end + 16).min(rows)].contains(&1) {
                end += 1;
            }
            let block_lo = java_floor_div(vol.min[1] + y as i32, 8) * 8;
            let block_hi = java_floor_div(vol.min[1] + end as i32 - 1, 8) * 8 + 8;
            let lo = (block_lo - vol.min[1]).max(0) as usize;
            let hi = ((block_hi - vol.min[1]) as usize).min(rows);
            let sub = Volume::new([vol.size[0], hi - lo, vol.size[2]], [vol.min[0], vol.min[1] + lo as i32, vol.min[2]], vol.step);
            let b = branch.sample_volume_ctx(&sub, ctx);
            for col in 0..vol.size[0] * vol.size[2] {
                let (src, dst) = (col * sub.size[1] + y - lo, col * rows + y);
                out[dst..dst + end - y].copy_from_slice(&b[src..src + end - y]);
            }
            ctx.give(b);
            y = end;
        }
        RowsSample::Buf(out)
    }

    pub fn sample_with_block_step(input: &Node, vol: &Volume, cell_xz: i32, cell_y: i32, ctx: &mut SampleCtx) -> Vec<f32> {
        let cells = [cell_xz, cell_y, cell_xz];
        let mut min_cell = [0i32; 3];
        let mut count = [0usize; 3];
        let mut corners = [0usize; 3];
        for a in 0..3 {
            min_cell[a] = java_floor_div(vol.min[a], cells[a]);
            let max_cell = java_floor_div(vol.max_block(a), cells[a]);
            count[a] = (max_cell - min_cell[a] + 1) as usize;
            corners[a] = if java_floor_mod(vol.max_block(a), cells[a]) == 0 { count[a] } else { count[a] + 1 };
        }
        let cell_vol = Volume::new(corners, [min_cell[0] * cell_xz, min_cell[1] * cell_y, min_cell[2] * cell_xz], cells);
        let cb = input.sample_volume_ctx(&cell_vol, ctx);
        let mut out = vec![0.0f32; vol.len()];
        let inv_xz = 1.0f32 / cell_xz as f32;
        let inv_y = 1.0f32 / cell_y as f32;
        for cz in 0..count[2] {
            let nz = (cz + 1).min(corners[2] - 1);
            for cx in 0..count[0] {
                let nx = (cx + 1).min(corners[0] - 1);
                let mut v000 = cb[cell_vol.index(cx, 0, cz)];
                let mut v100 = cb[cell_vol.index(nx, 0, cz)];
                let mut v001 = cb[cell_vol.index(cx, 0, nz)];
                let mut v101 = cb[cell_vol.index(nx, 0, nz)];
                for cy in 0..count[1] {
                    let ny = (cy + 1).min(corners[1] - 1);
                    let v010 = cb[cell_vol.index(cx, ny, cz)];
                    let v110 = cb[cell_vol.index(nx, ny, cz)];
                    let v011 = cb[cell_vol.index(cx, ny, nz)];
                    let v111 = cb[cell_vol.index(nx, ny, nz)];
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
                    for z in z0..=z1 {
                        let out_z = (cell_out[2] + z) as usize;
                        let alpha_z = z as f32 * inv_xz;
                        let v00 = lerp(alpha_z, v000, v001);
                        let v01 = lerp(alpha_z, v010, v011);
                        let v10 = lerp(alpha_z, v100, v101);
                        let v11 = lerp(alpha_z, v110, v111);
                        for x in x0..=x1 {
                            let out_x = (cell_out[0] + x) as usize;
                            let alpha_x = x as f32 * inv_xz;
                            let v_0 = lerp(alpha_x, v00, v10);
                            let v_1 = lerp(alpha_x, v01, v11);
                            let step = (v_1 - v_0) * inv_y;
                            let mut value = v_0 + step * y0 as f32;
                            let base = vol.index(out_x, (cell_out[1] + y0) as usize, out_z);
                            for slot in &mut out[base..base + (y1 - y0 + 1) as usize] {
                                *slot = value;
                                value += step;
                            }
                        }
                    }
                    v000 = v010;
                    v100 = v110;
                    v001 = v011;
                    v101 = v111;
                }
            }
        }
        out
    }
}

impl Spline {
    /// `SplineFunction` volume path: coordinates sampled to buffers,
    /// the same scalar spline per element.
    fn sample_volume_ctx(&self, vol: &Volume, ctx: &mut SampleCtx, coords: &mut HashMap<usize, Arc<Vec<f32>>>) -> Vec<f32> {
        match self {
            Spline::Constant(c) => ctx.filled(vol.len(), *c),
            Spline::Multipoint { coordinate, locations, values, derivatives } => {
                let key = Arc::as_ptr(coordinate) as usize;
                let input = match coords.get(&key) {
                    Some(b) => b.clone(),
                    None => {
                        let b = Arc::new(coordinate.sample_volume_ctx(vol, ctx));
                        coords.insert(key, b.clone());
                        b
                    }
                };
                let bufs: Vec<Option<Vec<f32>>> = values
                    .iter()
                    .map(|v| if matches!(v, Spline::Constant(_)) { None } else { Some(v.sample_volume_ctx(vol, ctx, coords)) })
                    .collect();
                let at = |k: usize, i: usize| match (&values[k], &bufs[k]) {
                    (Spline::Constant(c), _) => *c,
                    (_, Some(b)) => b[i],
                    _ => unreachable!(),
                };
                let mut out = vec![0.0; vol.len()];
                let last = locations.len() - 1;
                for (i, o) in out.iter_mut().enumerate() {
                    let inp = input[i];
                    let start = find_interval_start(locations, inp);
                    *o = if start < 0 {
                        linear_extend(inp, locations, at(0, i), derivatives, 0)
                    } else if start as usize == last {
                        linear_extend(inp, locations, at(last, i), derivatives, last)
                    } else {
                        let s = start as usize;
                        let x1 = locations[s];
                        let x2 = locations[s + 1];
                        let t = (inp - x1) / (x2 - x1);
                        let d1 = derivatives[s];
                        let d2 = derivatives[s + 1];
                        let y1 = at(s, i);
                        let y2 = at(s + 1, i);
                        let a = d1 * (x2 - x1) - (y2 - y1);
                        let b = -d2 * (x2 - x1) + (y2 - y1);
                        lerp(t, y1, y2) + t * (1.0 - t) * lerp(t, a, b)
                    };
                }
                out
            }
        }
    }
}

#[inline]
fn java_floor_mod(a: i32, m: i32) -> i32 {
    let r = a % m;
    if r != 0 && ((r < 0) != (m < 0)) { r + m } else { r }
}

#[inline]
pub(crate) fn java_floor_div(a: i32, m: i32) -> i32 {
    let q = a / m;
    if (a % m != 0) && ((a < 0) != (m < 0)) { q - 1 } else { q }
}

impl Spline {
    fn sample(&self, x: i32, y: i32, z: i32, mode: Mode, pc: &mut PointCache) -> f32 {
        match self {
            Spline::Constant(c) => *c,
            Spline::Multipoint { coordinate, locations, values, derivatives } => {
                let input = coordinate.eval_with(x, y, z, mode, pc);
                let start = find_interval_start(locations, input);
                let last = locations.len() - 1;
                if start < 0 {
                    return linear_extend(input, locations, values[0].sample(x, y, z, mode, pc), derivatives, 0);
                }
                let start = start as usize;
                if start == last {
                    return linear_extend(input, locations, values[last].sample(x, y, z, mode, pc), derivatives, last);
                }
                let x1 = locations[start];
                let x2 = locations[start + 1];
                let t = (input - x1) / (x2 - x1);
                let d1 = derivatives[start];
                let d2 = derivatives[start + 1];
                let y1 = values[start].sample(x, y, z, mode, pc);
                let y2 = values[start + 1].sample(x, y, z, mode, pc);
                let a = d1 * (x2 - x1) - (y2 - y1);
                let b = -d2 * (x2 - x1) + (y2 - y1);
                lerp(t, y1, y2) + t * (1.0 - t) * lerp(t, a, b)
            }
        }
    }
}

/// `Mth.binarySearch(0, n, i -> input < locations[i]) - 1`.
fn find_interval_start(locations: &[f32], input: f32) -> i32 {
    let mut from = 0i32;
    let mut len = locations.len() as i32;
    while len > 0 {
        let half = len / 2;
        let middle = from + half;
        if input < locations[middle as usize] {
            len = half;
        } else {
            from = middle + 1;
            len -= half + 1;
        }
    }
    from - 1
}

fn linear_extend(input: f32, locations: &[f32], value: f32, derivatives: &[f32], index: usize) -> f32 {
    let d = derivatives[index];
    if d == 0.0 { value } else { value + d * (input - locations[index]) }
}

pub const AXIS_X: u8 = 1;
pub const AXIS_Y: u8 = 2;
pub const AXIS_Z: u8 = 4;
pub const ALL_AXES: u8 = 7;

/// Needed-point fraction (numerator, denominator) under which a lerp side
/// that is a plain noise leaf is sampled point by point.
const MASKED_POINT_RATIO: (usize, usize) = (1, 4);

impl Axis {
    fn bit(self) -> u8 {
        match self {
            Axis::X => AXIS_X,
            Axis::Y => AXIS_Y,
            Axis::Z => AXIS_Z,
        }
    }
}

impl Spline {
    fn for_each_coordinate(&self, f: &mut impl FnMut(&N)) {
        if let Spline::Multipoint { coordinate, values, .. } = self {
            f(coordinate);
            for v in values {
                v.for_each_coordinate(f);
            }
        }
    }

    fn map_coordinates(&self, f: &mut impl FnMut(&N) -> N) -> Spline {
        match self {
            Spline::Constant(c) => Spline::Constant(*c),
            Spline::Multipoint { coordinate, locations, values, derivatives } => Spline::Multipoint {
                coordinate: f(coordinate),
                locations: locations.clone(),
                values: values.iter().map(|v| v.map_coordinates(f)).collect(),
                derivatives: derivatives.clone(),
            },
        }
    }
}

impl Node {
    /// `DensityFunction.domainAxes`: which block axes the value depends on.
    pub fn domain_axes(&self, memo: &mut HashMap<usize, u8>) -> u8 {
        let key = self as *const Node as usize;
        if let Some(a) = memo.get(&key) {
            return *a;
        }
        let a = match self {
            Node::Constant(_) => 0,
            Node::Add(l, r) | Node::Sub(l, r) | Node::Mul(l, r) | Node::Div(l, r) | Node::Min(l, r) | Node::Max(l, r) | Node::Pow(l, r) => {
                l.domain_axes(memo) | r.domain_axes(memo)
            }
            Node::ConstAdd(i, _) | Node::ConstMul(i, _) | Node::ConstMin(i, _) | Node::ConstMax(i, _) | Node::ConstSub(_, i)
            | Node::ConstDiv(_, i) | Node::PowConstBase(_, i) | Node::PowConstExp(i, _) | Node::Abs(i) | Node::Square(i)
            | Node::Cube(i) | Node::Sqrt(i) | Node::HalfNegative(i) | Node::QuarterNegative(i) | Node::Reciprocal(i)
            | Node::Negate(i) | Node::Squeeze(i) | Node::Log(i) | Node::Sign(i) | Node::Cache(i) | Node::BlendDensity(i)
            | Node::Interpolated { input: i, .. } => i.domain_axes(memo),
            Node::Clamp { input, .. } => input.domain_axes(memo),
            Node::Round { input, multiple, .. } => input.domain_axes(memo) | multiple.as_ref().map_or(0, |m| m.domain_axes(memo)),
            Node::Noise { xz_scale, y_scale, shift_x, shift_y, shift_z, .. } => {
                let mut axes = ALL_AXES;
                if *y_scale == 0.0 {
                    axes &= !AXIS_Y;
                }
                if *xz_scale == 0.0 {
                    axes &= !(AXIS_X | AXIS_Z);
                }
                for s in [shift_x, shift_y, shift_z].into_iter().flatten() {
                    axes |= s.domain_axes(memo);
                }
                axes
            }
            Node::ShiftB(_) | Node::BlendAlpha | Node::BlendOffset => AXIS_X | AXIS_Z,
            Node::Beardifier | Node::DistanceToPoint { .. } => ALL_AXES,
            Node::Gradient { axis, .. } => axis.bit(),
            Node::Lerp { alpha, first, second } => alpha.domain_axes(memo) | first.domain_axes(memo) | second.domain_axes(memo),
            Node::RangeChoice { input, when_in_range, when_out_of_range, .. } => {
                input.domain_axes(memo) | when_in_range.domain_axes(memo) | when_out_of_range.domain_axes(memo)
            }
            Node::IntervalSelect { input, functions, .. } => {
                functions.iter().fold(input.domain_axes(memo), |a, f| a | f.domain_axes(memo))
            }
            Node::Slice { axis, input, .. } => input.domain_axes(memo) & !axis.bit(),
            Node::FindTopSurface { density, upper_bound, .. } => (density.domain_axes(memo) | upper_bound.domain_axes(memo)) & !AXIS_Y,
            Node::Spline(sp) => {
                let mut axes = 0;
                sp.for_each_coordinate(&mut |c| axes |= c.domain_axes(memo));
                axes
            }
        };
        memo.insert(key, a);
        a
    }

    fn existing_removed_axes(&self) -> u8 {
        let mut axes = 0;
        let mut n = self;
        while let Node::Slice { axis, input, .. } = n {
            axes |= axis.bit();
            n = input;
        }
        axes
    }
}

/// `DfRewriteRule.SLICE_UNIFORM_AXES` plus the compiler's cache
/// handling: subtrees that do not depend on an axis are wrapped in a
/// `slice` at 0 on that axis, so a volume evaluation samples them on
/// a slab and broadcasts. Value-neutral; changes the sample count.
pub struct SliceUniformAxes {
    axes_memo: HashMap<usize, u8>,
    rewritten: HashMap<(usize, u8), N>,
    /// Every source node seen, so a pointer key can never be reused
    /// by a later allocation while this rewriter lives.
    keep_alive: Vec<N>,
}

impl Default for SliceUniformAxes {
    fn default() -> Self {
        Self::new()
    }
}

impl SliceUniformAxes {
    pub fn new() -> Self {
        Self { axes_memo: HashMap::new(), rewritten: HashMap::new(), keep_alive: Vec::new() }
    }

    /// Rewrite `n` seen from a parent with domain `parent_axes` (the
    /// root is rewritten with `ALL_AXES`).
    pub fn rewrite(&mut self, n: &N, parent_axes: u8) -> N {
        if matches!(**n, Node::Constant(_) | Node::Gradient { .. }) {
            return n.clone();
        }
        let key = (Arc::as_ptr(n) as usize, parent_axes);
        if let Some(r) = self.rewritten.get(&key) {
            return r.clone();
        }
        self.keep_alive.push(n.clone());
        let axes = n.domain_axes(&mut self.axes_memo);
        let out = if parent_axes == axes {
            self.rewrite_children(n, axes)
        } else {
            let inner = self.rewrite_children(n, axes);
            let removed = parent_axes & !axes & !inner.existing_removed_axes();
            let mut f = inner;
            for axis in [Axis::X, Axis::Z, Axis::Y] {
                if removed & axis.bit() != 0 {
                    f = Arc::new(Node::Slice { axis, coordinate: 0, input: f });
                }
            }
            f
        };
        self.rewritten.insert(key, out.clone());
        out
    }

    fn rewrite_children(&mut self, n: &N, axes: u8) -> N {
        let mut r = |c: &N| self.rewrite(c, axes);
        let node = match &**n {
            Node::Constant(_) | Node::ShiftB(_) | Node::Gradient { .. } | Node::BlendAlpha | Node::BlendOffset | Node::Beardifier
            | Node::DistanceToPoint { .. } => return n.clone(),
            Node::Add(l, x) => Node::Add(r(l), r(x)),
            Node::Sub(l, x) => Node::Sub(r(l), r(x)),
            Node::Mul(l, x) => Node::Mul(r(l), r(x)),
            Node::Div(l, x) => Node::Div(r(l), r(x)),
            Node::Min(l, x) => Node::Min(r(l), r(x)),
            Node::Max(l, x) => Node::Max(r(l), r(x)),
            Node::Pow(l, x) => Node::Pow(r(l), r(x)),
            Node::ConstAdd(i, c) => Node::ConstAdd(r(i), *c),
            Node::ConstMul(i, c) => Node::ConstMul(r(i), *c),
            Node::ConstMin(i, c) => Node::ConstMin(r(i), *c),
            Node::ConstMax(i, c) => Node::ConstMax(r(i), *c),
            Node::ConstSub(c, i) => Node::ConstSub(*c, r(i)),
            Node::ConstDiv(c, i) => Node::ConstDiv(*c, r(i)),
            Node::PowConstBase(c, i) => Node::PowConstBase(*c, r(i)),
            Node::PowConstExp(i, c) => Node::PowConstExp(r(i), *c),
            Node::Abs(i) => Node::Abs(r(i)),
            Node::Square(i) => Node::Square(r(i)),
            Node::Cube(i) => Node::Cube(r(i)),
            Node::Sqrt(i) => Node::Sqrt(r(i)),
            Node::HalfNegative(i) => Node::HalfNegative(r(i)),
            Node::QuarterNegative(i) => Node::QuarterNegative(r(i)),
            Node::Reciprocal(i) => Node::Reciprocal(r(i)),
            Node::Negate(i) => Node::Negate(r(i)),
            Node::Squeeze(i) => Node::Squeeze(r(i)),
            Node::Log(i) => Node::Log(r(i)),
            Node::Sign(i) => Node::Sign(r(i)),
            Node::Round { kind, input, multiple } => {
                Node::Round { kind: *kind, input: r(input), multiple: multiple.as_ref().map(&mut r) }
            }
            Node::Noise { stack, xz_scale, y_scale, shift_x, shift_y, shift_z } => Node::Noise {
                stack: stack.clone(),
                xz_scale: *xz_scale,
                y_scale: *y_scale,
                shift_x: shift_x.as_ref().map(&mut r),
                shift_y: shift_y.as_ref().map(&mut r),
                shift_z: shift_z.as_ref().map(&mut r),
            },
            Node::Lerp { alpha, first, second } => Node::Lerp { alpha: r(alpha), first: r(first), second: r(second) },
            Node::Clamp { input, min, max } => Node::Clamp { input: r(input), min: *min, max: *max },
            Node::RangeChoice { input, min_inclusive, max_exclusive, when_in_range, when_out_of_range } => Node::RangeChoice {
                input: r(input),
                min_inclusive: *min_inclusive,
                max_exclusive: *max_exclusive,
                when_in_range: r(when_in_range),
                when_out_of_range: r(when_out_of_range),
            },
            Node::IntervalSelect { input, thresholds, functions } => Node::IntervalSelect {
                input: r(input),
                thresholds: thresholds.clone(),
                functions: functions.iter().map(&mut r).collect(),
            },
            // The compiler rewrites a cache's input as a fresh root.
            Node::Cache(i) => Node::Cache(self.rewrite(i, ALL_AXES)),
            Node::BlendDensity(i) => Node::BlendDensity(r(i)),
            Node::Interpolated { cell_size_xz, cell_size_y, input } => {
                Node::Interpolated { cell_size_xz: *cell_size_xz, cell_size_y: *cell_size_y, input: r(input) }
            }
            Node::Slice { axis, coordinate, input } => Node::Slice { axis: *axis, coordinate: *coordinate, input: r(input) },
            Node::FindTopSurface { density, upper_bound, lower_bound, cell_height } => Node::FindTopSurface {
                density: r(density),
                upper_bound: r(upper_bound),
                lower_bound: *lower_bound,
                cell_height: *cell_height,
            },
            Node::Spline(sp) => Node::Spline(sp.map_coordinates(&mut r)),
        };
        Arc::new(node)
    }
}

/// Loads datapack JSON under `<root>/data/<ns>/worldgen/` and seeds
/// noises the way `RandomState` does for a Xoroshiro world.
pub struct Loader {
    source: Source,
    seed: i64,
    root_factory: XoroshiroPositionalRandomFactory,
    functions: HashMap<String, N>,
    noises: HashMap<String, Arc<NoiseStack>>,
    loading: HashSet<String>,
    inline_caches: HashMap<String, N>,
    /// (kind, id) of every in-memory document, in arrival order.
    memory_order: Vec<(String, String)>,
}

fn split_id(id: &str) -> (String, String) {
    match id.split_once(':') {
        Some((ns, path)) => (ns.to_string(), path.to_string()),
        None => ("minecraft".to_string(), id.to_string()),
    }
}

pub(crate) fn full_id(id: &str) -> String {
    let (ns, path) = split_id(id);
    format!("{ns}:{path}")
}

fn get_f64(o: &serde_json::Map<String, Value>, key: &str) -> Result<f64, String> {
    o.get(key).and_then(Value::as_f64).ok_or_else(|| format!("{key}: missing or not a number"))
}

fn get_i32(o: &serde_json::Map<String, Value>, key: &str) -> Result<i32, String> {
    o.get(key).and_then(Value::as_i64).map(|v| v as i32).ok_or_else(|| format!("{key}: missing or not an integer"))
}

fn get_f32(o: &serde_json::Map<String, Value>, key: &str) -> Result<f32, String> {
    get_f64(o, key).map(|v| v as f32)
}

fn is_zero_constant(n: &N) -> bool {
    matches!(**n, Node::Constant(c) if c == 0.0)
}

/// Where datapack JSON comes from: a directory holding `data/`, or
/// documents handed over by the game keyed `"<kind>/<full id>"`.
enum Source {
    Dir(PathBuf),
    Memory(HashMap<String, Value>),
}

impl Loader {
    fn with_source(source: Source, seed: i64) -> Self {
        let mut random = XoroshiroRandomSource::from_legacy_seed(seed);
        Self {
            source,
            seed,
            root_factory: random.fork_positional(),
            functions: HashMap::new(),
            noises: HashMap::new(),
            loading: HashSet::new(),
            inline_caches: HashMap::new(),
            memory_order: Vec::new(),
        }
    }

    /// `root` is the directory holding `data/`; `seed` is the world seed.
    pub fn new(root: &Path, seed: i64) -> Self {
        Self::with_source(Source::Dir(root.to_path_buf()), seed)
    }

    /// Documents already decoded by the game: `kind` is
    /// `density_function`, `noise` or `noise_settings`, `id` the full
    /// identifier (`minecraft:overworld/erosion`).
    pub fn from_memory(seed: i64, documents: impl IntoIterator<Item = (String, String, Value)>) -> Self {
        let mut order = Vec::new();
        let map = documents
            .into_iter()
            .map(|(kind, id, v)| {
                let id = full_id(&id);
                order.push((kind.clone(), id.clone()));
                (format!("{kind}/{id}"), v)
            })
            .collect();
        let mut loader = Self::with_source(Source::Memory(map), seed);
        loader.memory_order = order;
        loader
    }

    fn read_json(&self, kind: &str, id: &str) -> Result<Value, String> {
        match &self.source {
            Source::Dir(root) => {
                let (ns, path) = split_id(id);
                let path = root.join("data").join(ns).join("worldgen").join(kind).join(format!("{path}.json"));
                let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
            }
            Source::Memory(map) => map.get(&format!("{kind}/{}", full_id(id))).cloned().ok_or_else(|| format!("{kind} {id}: not provided")),
        }
    }

    /// The noise registered under `id`, instantiated on first use.
    pub fn noise(&mut self, id: &str) -> Result<Arc<NoiseStack>, String> {
        let key = full_id(id);
        if let Some(n) = self.noises.get(&key) {
            return Ok(n.clone());
        }
        let params = NoiseParameters26::from_json(&self.read_json("noise", &key)?)?;
        let stack = Arc::new(params.instantiate(&self.root_factory, &key));
        self.noises.insert(key, stack.clone());
        Ok(stack)
    }

    /// The density function registered under `id`.
    pub fn function(&mut self, id: &str) -> Result<N, String> {
        let key = full_id(id);
        if let Some(n) = self.functions.get(&key) {
            return Ok(n.clone());
        }
        if !self.loading.insert(key.clone()) {
            return Err(format!("reference cycle through {key}"));
        }
        let json = self.read_json("density_function", &key)?;
        let node = self.parse(&json).map_err(|e| format!("{key}: {e}"))?;
        self.loading.remove(&key);
        self.functions.insert(key, node.clone());
        Ok(node)
    }

    /// The raw noise settings JSON.
    pub fn noise_settings(&self, settings_id: &str) -> Result<Value, String> {
        self.read_json("noise_settings", settings_id)
    }

    /// Any registry document by kind (`material_rule`, `biome`, ...).
    pub fn document(&self, kind: &str, id: &str) -> Result<Value, String> {
        self.read_json(kind, id)
    }

    /// Every id of a kind, in the order the game registers them for
    /// in-memory sources and sorted by path for a datapack directory.
    pub fn ids_of(&self, kind: &str) -> Result<Vec<String>, String> {
        match &self.source {
            Source::Dir(root) => {
                let dir = root.join("data").join("minecraft").join("worldgen").join(kind);
                let mut ids = Vec::new();
                let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                for entry in entries {
                    let path = entry.map_err(|e| e.to_string())?.path();
                    if path.extension().is_some_and(|x| x == "json") {
                        let stem = path.file_stem().and_then(|s| s.to_str()).ok_or("bad file name")?;
                        ids.push(format!("minecraft:{stem}"));
                    }
                }
                ids.sort();
                Ok(ids)
            }
            Source::Memory(_) => Ok(self.memory_order.iter().filter(|(k, _)| k == kind).map(|(_, id)| id.clone()).collect()),
        }
    }

    /// The world's root positional random factory (`RandomState.random`).
    /// The world seed the loader was built with.
    pub fn seed(&self) -> i64 {
        self.seed
    }

    pub fn root_factory(&self) -> &XoroshiroPositionalRandomFactory {
        &self.root_factory
    }

    /// Every `noise_router` entry of a noise settings file, by key.
    pub fn router(&mut self, settings_id: &str) -> Result<HashMap<String, N>, String> {
        let json = self.read_json("noise_settings", settings_id)?;
        let legacy = json.get("legacy_random_source").and_then(Value::as_bool).unwrap_or(false);
        if legacy {
            return Err("legacy_random_source worlds are not supported".to_string());
        }
        let router = json.get("noise_router").and_then(Value::as_object).ok_or("noise_router missing")?;
        let mut out = HashMap::new();
        let mut rule = SliceUniformAxes::new();
        for (k, v) in router {
            let n = self.parse(v).map_err(|e| format!("noise_router.{k}: {e}"))?;
            out.insert(k.clone(), rule.rewrite(&n, ALL_AXES));
        }
        Ok(out)
    }

    /// Parse an inline node, a numeric constant, or a reference.
    pub fn parse(&mut self, v: &Value) -> Result<N, String> {
        match v {
            Value::Number(n) => Ok(Arc::new(Node::Constant(n.as_f64().ok_or("bad number")? as f32))),
            Value::String(s) => self.function(s),
            Value::Object(o) => self.parse_object(o),
            other => Err(format!("unexpected JSON value {other}")),
        }
    }

    fn child(&mut self, o: &serde_json::Map<String, Value>, key: &str) -> Result<N, String> {
        let v = o.get(key).ok_or_else(|| format!("{key}: missing"))?;
        self.parse(v).map_err(|e| format!("{key}: {e}"))
    }

    fn parse_object(&mut self, o: &serde_json::Map<String, Value>) -> Result<N, String> {
        let ty = o.get("type").and_then(Value::as_str).ok_or("type: missing")?;
        let ty = ty.strip_prefix("minecraft:").unwrap_or(ty);
        let node = match ty {
            "constant" => Node::Constant(get_f32(o, "argument")?),
            "add" | "sub" | "mul" | "div" | "min" | "max" => {
                let l = self.child(o, "left")?;
                let r = self.child(o, "right")?;
                let lc = if let Node::Constant(c) = *l { Some(c) } else { None };
                let rc = if let Node::Constant(c) = *r { Some(c) } else { None };
                match (ty, lc, rc) {
                    ("add", Some(c), _) => Node::ConstAdd(r, c),
                    ("add", _, Some(c)) => Node::ConstAdd(l, c),
                    ("add", _, _) => Node::Add(l, r),
                    ("sub", Some(c), _) => Node::ConstSub(c, r),
                    ("sub", _, Some(c)) => Node::ConstAdd(l, -c),
                    ("sub", _, _) => Node::Sub(l, r),
                    ("mul", Some(c), _) => Node::ConstMul(r, c),
                    ("mul", _, Some(c)) => Node::ConstMul(l, c),
                    ("mul", _, _) => Node::Mul(l, r),
                    ("div", Some(c), _) => Node::ConstDiv(c, r),
                    ("div", _, Some(c)) => Node::ConstMul(l, 1.0 / c),
                    ("div", _, _) => Node::Div(l, r),
                    ("min", Some(c), _) => Node::ConstMin(r, c),
                    ("min", _, Some(c)) => Node::ConstMin(l, c),
                    ("min", _, _) => Node::Min(l, r),
                    ("max", Some(c), _) => Node::ConstMax(r, c),
                    ("max", _, Some(c)) => Node::ConstMax(l, c),
                    (_, _, _) => Node::Max(l, r),
                }
            }
            "abs" => Node::Abs(self.child(o, "input")?),
            "square" => Node::Square(self.child(o, "input")?),
            "cube" => Node::Cube(self.child(o, "input")?),
            "sqrt" => Node::Sqrt(self.child(o, "input")?),
            "half_negative" => Node::HalfNegative(self.child(o, "input")?),
            "quarter_negative" => Node::QuarterNegative(self.child(o, "input")?),
            "reciprocal" => Node::Reciprocal(self.child(o, "input")?),
            "negate" => Node::Negate(self.child(o, "input")?),
            "squeeze" => Node::Squeeze(self.child(o, "input")?),
            "log" => Node::Log(self.child(o, "input")?),
            "sign" => Node::Sign(self.child(o, "input")?),
            "pow" => {
                let base = self.child(o, "base")?;
                let exponent = self.child(o, "exponent")?;
                let bc = if let Node::Constant(c) = *base { Some(c) } else { None };
                let ec = if let Node::Constant(c) = *exponent { Some(c) } else { None };
                match (bc, ec) {
                    (Some(c), _) => Node::PowConstBase(c as f64, exponent),
                    (_, Some(e)) => {
                        let a = e.abs();
                        let special = if a == 0.5 {
                            Node::Sqrt(base)
                        } else if a == 1.0 {
                            return Ok(base);
                        } else if a == 2.0 {
                            Node::Square(base)
                        } else if a == 3.0 {
                            Node::Cube(base)
                        } else {
                            Node::PowConstExp(base, a as f64)
                        };
                        if e >= 0.0 { special } else { Node::Reciprocal(Arc::new(special)) }
                    }
                    _ => Node::Pow(base, exponent),
                }
            }
            "floor" | "round" | "ceil" | "truncate" => {
                let kind = match ty {
                    "floor" => RoundKind::Floor,
                    "round" => RoundKind::Round,
                    "ceil" => RoundKind::Ceil,
                    _ => RoundKind::Truncate,
                };
                let input = self.child(o, "input")?;
                let multiple = match o.get("multiple") {
                    None => None,
                    Some(m) => {
                        let m = self.parse(m)?;
                        if matches!(*m, Node::Constant(c) if c == 1.0) { None } else { Some(m) }
                    }
                };
                Node::Round { kind, input, multiple }
            }
            "noise" => {
                let name = o.get("noise").and_then(Value::as_str).ok_or("noise: missing")?.to_string();
                let stack = self.noise(&name)?;
                let mut shift = |key: &str| -> Result<Option<N>, String> {
                    match o.get(key) {
                        None => Ok(None),
                        Some(v) => {
                            let n = self.parse(v)?;
                            Ok(if is_zero_constant(&n) { None } else { Some(n) })
                        }
                    }
                };
                let shift_x = shift("shift_x")?;
                let shift_y = shift("shift_y")?;
                let shift_z = shift("shift_z")?;
                Node::Noise { stack, xz_scale: get_f64(o, "xz_scale")?, y_scale: get_f64(o, "y_scale")?, shift_x, shift_y, shift_z }
            }
            "shift" | "shift_a" | "shift_b" => {
                let name = o.get("noise").and_then(Value::as_str).ok_or("noise: missing")?.to_string();
                let stack = self.noise(&name)?;
                match ty {
                    "shift_b" => Node::ShiftB(stack),
                    _ => {
                        let y_scale = if ty == "shift" { 0.25 } else { 0.0 };
                        Node::ConstMul(
                            Arc::new(Node::Noise { stack, xz_scale: 0.25, y_scale, shift_x: None, shift_y: None, shift_z: None }),
                            4.0,
                        )
                    }
                }
            }
            "gradient" => {
                let axis = parse_axis(o.get("axis").and_then(Value::as_str).ok_or("axis: missing")?)?;
                let tiling = match o.get("tiling").and_then(Value::as_str) {
                    None | Some("clamp_to_edge") => Tiling::ClampToEdge,
                    Some("repeat") => Tiling::Repeat,
                    Some("mirrored_repeat") => Tiling::MirroredRepeat,
                    Some(t) => return Err(format!("tiling: unknown {t}")),
                };
                let from_coordinate = get_i32(o, "from_coordinate")?;
                let to_coordinate = get_i32(o, "to_coordinate")?;
                if from_coordinate == to_coordinate {
                    return Err("gradient: from_coordinate == to_coordinate".to_string());
                }
                let from_value = get_f32(o, "from_value")?;
                let to_value = get_f32(o, "to_value")?;
                let factor = (to_value - from_value) / (to_coordinate - from_coordinate) as f32;
                Node::Gradient { axis, tiling, from_coordinate, to_coordinate, from_value, factor }
            }
            "lerp" => Node::Lerp { alpha: self.child(o, "alpha")?, first: self.child(o, "first")?, second: self.child(o, "second")? },
            "clamp" => {
                let min = get_f32(o, "min")?;
                let max = get_f32(o, "max")?;
                if max < min {
                    return Err("clamp: max < min".to_string());
                }
                Node::Clamp { input: self.child(o, "input")?, min, max }
            }
            "range_choice" => Node::RangeChoice {
                input: self.child(o, "input")?,
                min_inclusive: get_f32(o, "min_inclusive")?,
                max_exclusive: get_f32(o, "max_exclusive")?,
                when_in_range: self.child(o, "when_in_range")?,
                when_out_of_range: self.child(o, "when_out_of_range")?,
            },
            "interval_select" => {
                let input = self.child(o, "input")?;
                let thresholds: Vec<f32> = o
                    .get("thresholds")
                    .and_then(Value::as_array)
                    .ok_or("thresholds: missing")?
                    .iter()
                    .map(|v| v.as_f64().map(|f| f as f32).ok_or_else(|| "thresholds: not a number".to_string()))
                    .collect::<Result<_, _>>()?;
                let functions_json = o.get("functions").and_then(Value::as_array).ok_or("functions: missing")?;
                let mut functions = Vec::with_capacity(functions_json.len());
                for f in functions_json {
                    functions.push(self.parse(f)?);
                }
                if functions.len() < 2 || thresholds.len() + 1 != functions.len() {
                    return Err("interval_select: thresholds must be one shorter than functions".to_string());
                }
                if thresholds.windows(2).any(|w| w[1] < w[0]) {
                    return Err("interval_select: thresholds must be non-decreasing".to_string());
                }
                Node::IntervalSelect { input, thresholds, functions }
            }
            "cache" => {
                let text = o.get("input").ok_or("input: missing")?.to_string();
                if let Some(n) = self.inline_caches.get(&text) {
                    return Ok(n.clone());
                }
                let n = Arc::new(Node::Cache(self.child(o, "input")?));
                self.inline_caches.insert(text, n.clone());
                return Ok(n);
            }
            "blend_density" => Node::BlendDensity(self.child(o, "input")?),
            "blend_alpha" => Node::BlendAlpha,
            "blend_offset" => Node::BlendOffset,
            "beardifier" => Node::Beardifier,
            "interpolated" => {
                let cell_size_xz = get_i32(o, "cell_size_xz")?;
                let cell_size_y = get_i32(o, "cell_size_y")?;
                if cell_size_xz <= 0 || cell_size_y <= 0 {
                    return Err("interpolated: cell sizes must be positive".to_string());
                }
                Node::Interpolated { cell_size_xz, cell_size_y, input: self.child(o, "input")? }
            }
            "slice" => Node::Slice {
                axis: parse_axis(o.get("axis").and_then(Value::as_str).ok_or("axis: missing")?)?,
                coordinate: get_i32(o, "coordinate")?,
                input: self.child(o, "input")?,
            },
            "find_top_surface" => {
                let cell_height = get_i32(o, "cell_height")?;
                if cell_height <= 0 {
                    return Err("find_top_surface: cell_height must be positive".to_string());
                }
                // compileSampler wraps the sampler in a y=0 slice.
                Node::Slice {
                    axis: Axis::Y,
                    coordinate: 0,
                    input: Arc::new(Node::FindTopSurface {
                        density: self.child(o, "density")?,
                        upper_bound: self.child(o, "upper_bound")?,
                        lower_bound: get_i32(o, "lower_bound")?,
                        cell_height,
                    }),
                }
            }
            "distance_to_point" => {
                let metric = match o.get("metric").and_then(Value::as_str).ok_or("metric: missing")? {
                    "euclidean" => Metric::Euclidean,
                    "euclidean_squared" => Metric::EuclideanSquared,
                    "manhattan" => Metric::Manhattan,
                    "chebyshev" => Metric::Chebyshev,
                    m => return Err(format!("metric: unknown {m}")),
                };
                let p = o.get("point").and_then(Value::as_array).ok_or("point: missing")?;
                if p.len() != 3 {
                    return Err("point: expected 3 integers".to_string());
                }
                let c = |i: usize| p[i].as_i64().map(|v| v as i32).ok_or_else(|| "point: not an integer".to_string());
                Node::DistanceToPoint { metric, x: c(0)?, y: c(1)?, z: c(2)? }
            }
            "spline" => Node::Spline(self.parse_spline(o.get("spline").ok_or("spline: missing")?)?),
            "old_blended_noise" => self.parse_old_blended(o)?,
            "end_outer_islands" => return Err("end_outer_islands is not supported".to_string()),
            other => return Err(format!("unknown density function type {other}")),
        };
        Ok(Arc::new(node))
    }

    fn parse_spline(&mut self, v: &Value) -> Result<Spline, String> {
        match v {
            Value::Number(n) => Ok(Spline::Constant(n.as_f64().ok_or("bad number")? as f32)),
            Value::Object(o) => {
                let coordinate = self.child(o, "coordinate")?;
                let points = o.get("points").and_then(Value::as_array).ok_or("points: missing")?;
                if points.is_empty() {
                    return Err("spline: no points".to_string());
                }
                let mut locations = Vec::with_capacity(points.len());
                let mut values = Vec::with_capacity(points.len());
                let mut derivatives = Vec::with_capacity(points.len());
                for p in points {
                    let po = p.as_object().ok_or("point: not an object")?;
                    locations.push(get_f32(po, "location")?);
                    derivatives.push(get_f32(po, "derivative")?);
                    values.push(self.parse_spline(po.get("value").ok_or("value: missing")?)?);
                }
                if locations.windows(2).any(|w| w[1] <= w[0]) {
                    return Err("spline: locations must be strictly increasing".to_string());
                }
                Ok(Spline::Multipoint { coordinate, locations, values, derivatives })
            }
            other => Err(format!("spline: unexpected {other}")),
        }
    }

    /// `BlendedNoise.compileSampler`, expressed with ordinary nodes.
    fn parse_old_blended(&mut self, o: &serde_json::Map<String, Value>) -> Result<Node, String> {
        let xz_scale = get_f64(o, "xz_scale")?;
        let y_scale = get_f64(o, "y_scale")?;
        let xz_factor = get_f64(o, "xz_factor")?;
        let y_factor = get_f64(o, "y_factor")?;
        let smear = get_f64(o, "smear_scale_multiplier")?;
        let xz_m = 684.412 * xz_scale;
        let y_m = 684.412 * y_scale;
        let limit_smear_y = y_m * smear;
        let main_smear_y = limit_smear_y / y_factor;
        let mut random = self.root_factory.from_hash_of("minecraft:terrain");
        let min_limit = Arc::new(NoiseStack::create_fbm(&mut random, -15, limit_smear_y, 0.99998474f32 as f64));
        let max_limit = Arc::new(NoiseStack::create_fbm(&mut random, -15, limit_smear_y, 0.99998474f32 as f64));
        let main = Arc::new(NoiseStack::create_fbm(&mut random, -7, main_smear_y, 12.75));
        let plain = |stack: Arc<NoiseStack>, xz: f64, y: f64| {
            Arc::new(Node::Noise { stack, xz_scale: xz, y_scale: y, shift_x: None, shift_y: None, shift_z: None })
        };
        let choice = Arc::new(Node::Clamp {
            input: Arc::new(Node::ConstAdd(plain(main, xz_m / xz_factor, y_m / y_factor), 0.5)),
            min: 0.0,
            max: 1.0,
        });
        Ok(Node::Lerp { alpha: choice, first: plain(min_limit, xz_m, y_m), second: plain(max_limit, xz_m, y_m) })
    }
}

fn parse_axis(s: &str) -> Result<Axis, String> {
    match s {
        "x" => Ok(Axis::X),
        "y" => Ok(Axis::Y),
        "z" => Ok(Axis::Z),
        other => Err(format!("axis: unknown {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(v: f32) -> N {
        Arc::new(Node::Constant(v))
    }

    #[test]
    fn gradient_clamp_matches_formula() {
        let g = Node::Gradient {
            axis: Axis::Y,
            tiling: Tiling::ClampToEdge,
            from_coordinate: -64,
            to_coordinate: 320,
            from_value: 1.5,
            factor: (-1.5f32 - 1.5) / 384.0,
        };
        assert_eq!(g.eval(0, -64, 0, Mode::Point), 1.5);
        assert_eq!(g.eval(0, 320, 0, Mode::Point), -1.5);
        assert_eq!(g.eval(0, 1000, 0, Mode::Point), -1.5);
        assert_eq!(g.eval(0, 128, 0, Mode::Point), 1.5 + 192.0 * ((-1.5f32 - 1.5) / 384.0));
    }

    #[test]
    fn lerp_short_circuits_at_ends() {
        let n = Node::Lerp { alpha: c(0.0), first: c(2.0), second: Arc::new(Node::Reciprocal(c(0.0))) };
        assert_eq!(n.eval(0, 0, 0, Mode::Point), 2.0);
        let n = Node::Lerp { alpha: c(1.0), first: Arc::new(Node::Reciprocal(c(0.0))), second: c(3.0) };
        assert_eq!(n.eval(0, 0, 0, Mode::Point), 3.0);
        let n = Node::Lerp { alpha: c(0.25), first: c(0.0), second: c(4.0) };
        assert_eq!(n.eval(0, 0, 0, Mode::Point), 1.0);
    }

    #[test]
    fn mul_zero_short_circuit_only_on_point_path() {
        let n = Node::Mul(c(0.0), Arc::new(Node::Reciprocal(c(0.0))));
        assert_eq!(n.eval(0, 0, 0, Mode::Point), 0.0);
        assert!(n.eval(0, 0, 0, Mode::Volume).is_nan());
    }

    #[test]
    fn squeeze_and_quarter_negative() {
        let s = Node::Squeeze(c(2.0));
        assert_eq!(s.eval(0, 0, 0, Mode::Point), 0.5 - 1.0 / 24.0);
        let q = Node::QuarterNegative(c(-2.0));
        assert_eq!(q.eval(0, 0, 0, Mode::Point), -0.5);
        let q = Node::QuarterNegative(c(-0.0));
        assert!(q.eval(0, 0, 0, Mode::Point).is_sign_negative());
    }

    #[test]
    fn interval_select_strict_upper() {
        let n = Node::IntervalSelect { input: c(0.5), thresholds: vec![-0.5, 0.5], functions: vec![c(1.0), c(2.0), c(3.0)] };
        assert_eq!(n.eval(0, 0, 0, Mode::Point), 3.0);
        let n = Node::IntervalSelect { input: c(-1.0), thresholds: vec![-0.5, 0.5], functions: vec![c(1.0), c(2.0), c(3.0)] };
        assert_eq!(n.eval(0, 0, 0, Mode::Point), 1.0);
    }

    #[test]
    fn spline_interval_and_extrapolation() {
        let s = Spline::Multipoint {
            coordinate: Arc::new(Node::Gradient {
                axis: Axis::Y,
                tiling: Tiling::ClampToEdge,
                from_coordinate: 0,
                to_coordinate: 10,
                from_value: 0.0,
                factor: 0.1,
            }),
            locations: vec![0.0, 1.0],
            values: vec![Spline::Constant(0.0), Spline::Constant(1.0)],
            derivatives: vec![0.0, 2.0],
        };
        assert_eq!(s.sample(0, 5, 0, Mode::Point, &mut PointCache::disabled()), 0.5 + 0.5 * 0.5 * lerp(0.5, -1.0, -2.0 + 1.0));
        assert_eq!(s.sample(0, 10, 0, Mode::Point, &mut PointCache::disabled()), 1.0);
        assert_eq!(find_interval_start(&[0.0, 1.0], -1.0), -1);
        assert_eq!(find_interval_start(&[0.0, 1.0], 5.0), 1);
    }

    #[test]
    fn interpolated_is_exact_on_cell_corners_and_lerps_inside() {
        let g = Arc::new(Node::Gradient {
            axis: Axis::X,
            tiling: Tiling::ClampToEdge,
            from_coordinate: 0,
            to_coordinate: 64,
            from_value: 0.0,
            factor: 1.0,
        });
        let n = Node::Interpolated { cell_size_xz: 4, cell_size_y: 8, input: g };
        assert_eq!(n.eval(8, 0, 0, Mode::Point), 8.0);
        assert_eq!(n.eval(9, 3, 2, Mode::Point), 9.0);
    }

    #[test]
    fn find_top_surface_probes_downward() {
        let density = Arc::new(Node::Gradient {
            axis: Axis::Y,
            tiling: Tiling::ClampToEdge,
            from_coordinate: 0,
            to_coordinate: 100,
            from_value: 50.0,
            factor: -1.0,
        });
        let n = Node::FindTopSurface { density, upper_bound: c(100.0), lower_bound: -64, cell_height: 8 };
        assert_eq!(n.eval(0, 77, 0, Mode::Point), 48.0);
    }
}
