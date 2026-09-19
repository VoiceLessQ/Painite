//! The carvers of 26.3 (`NoiseBasedChunkGenerator.generateCarvers`):
//! every source chunk within 8 walks its cave and canyon tunnels into a
//! bit mask over this chunk, then the mask is applied column by column
//! with the aquifer deciding air, water or lava and the material rule
//! re-topping dirt that lost its grass. Both halves are bit-exact ports;
//! every random is the legacy LCG, as in vanilla.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::Value;

use crate::aquifer26::{Aquifer, Substance};
use crate::df26::{full_id, Loader};
use crate::surface26::{resolve_anchor, Ctx, Palette};
use crate::xoroshiro::LegacyRandomSource;

const SIN_SCALE: f64 = 10430.378350470453;

/// `Mth.SIN`: 65536 floats, built once (the tunnels read it five times a step).
fn sin_table() -> &'static [f32] {
    static TABLE: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| (0..65536).map(|i| ((i as f64) / SIN_SCALE).sin() as f32).collect())
}

/// `Mth.sin`.
#[inline]
pub(crate) fn mth_sin(i: f64) -> f32 {
    sin_table()[((i * SIN_SCALE) as i64 & 65535) as usize]
}

/// `Mth.cos`: the same table a quarter turn on.
#[inline]
pub(crate) fn mth_cos(i: f64) -> f32 {
    sin_table()[((i * SIN_SCALE + 16384.0) as i64 & 65535) as usize]
}

const PI_F: f32 = std::f32::consts::PI;
/// `(float) (Math.PI * 2)`.
const TWO_PI_F: f32 = 6.283_185_5_f32;
/// `(float) (Math.PI / 2)`.
const HALF_PI_F: f32 = 1.570_796_4_f32;

enum FloatProvider {
    Constant(f32),
    Uniform { min: f32, max: f32 },
    Trapezoid { min: f32, max: f32, plateau: f32 },
}

impl FloatProvider {
    fn sample(&self, random: &mut LegacyRandomSource) -> f32 {
        match *self {
            FloatProvider::Constant(v) => v,
            FloatProvider::Uniform { min, max } => random.next_float() * (max - min) + min,
            FloatProvider::Trapezoid { min, max, plateau } => {
                let range = max - min;
                let plateau_start = (range - plateau) / 2.0;
                let plateau_end = range - plateau_start;
                min + random.next_float() * plateau_end + random.next_float() * plateau_start
            }
        }
    }
}

enum IntProvider {
    Constant(i32),
    Uniform { min: i32, max: i32 },
    BiasedToBottom { min: i32, max: i32 },
    VeryBiasedToBottom { min: i32, max: i32 },
}

impl IntProvider {
    fn sample(&self, random: &mut LegacyRandomSource) -> i32 {
        match *self {
            IntProvider::Constant(v) => v,
            IntProvider::Uniform { min, max } => random.next_int_bound(max - min + 1) + min,
            IntProvider::BiasedToBottom { min, max } => {
                let a = random.next_int_bound(max - min + 1) + 1;
                min + random.next_int_bound(a)
            }
            IntProvider::VeryBiasedToBottom { min, max } => {
                let a = random.next_int_bound(max - min + 1) + 1;
                let b = random.next_int_bound(a) + 1;
                min + random.next_int_bound(b)
            }
        }
    }
}

enum HeightProvider {
    Constant(i32),
    Uniform { min: i32, max: i32 },
    Trapezoid { min: i32, max: i32, plateau: i32 },
    BiasedToBottom { min: i32, max: i32, inner: i32 },
    VeryBiasedToBottom { min: i32, max: i32, inner: i32 },
}

fn random_between_inclusive(random: &mut LegacyRandomSource, min: i32, max: i32) -> i32 {
    random.next_int_bound(max - min + 1) + min
}

/// `Mth.nextInt`: inclusive both ends, min when the range is empty.
fn mth_next_int(random: &mut LegacyRandomSource, min: i32, max: i32) -> i32 {
    if min >= max {
        min
    } else {
        random.next_int_bound(max - min + 1) + min
    }
}

impl HeightProvider {
    fn sample(&self, random: &mut LegacyRandomSource) -> i32 {
        match *self {
            HeightProvider::Constant(y) => y,
            HeightProvider::Uniform { min, max } => {
                if min > max {
                    min
                } else {
                    random_between_inclusive(random, min, max)
                }
            }
            HeightProvider::Trapezoid { min, max, plateau } => {
                if min > max {
                    return min;
                }
                let range = max - min;
                if plateau >= range {
                    return random_between_inclusive(random, min, max);
                }
                let plateau_start = (range - plateau) / 2;
                let plateau_end = range - plateau_start;
                min + random_between_inclusive(random, 0, plateau_end) + random_between_inclusive(random, 0, plateau_start)
            }
            HeightProvider::BiasedToBottom { min, max, inner } => {
                if max - min - inner < 0 {
                    return min;
                }
                let limit = random.next_int_bound(max - min - inner + 1);
                random.next_int_bound(limit + inner) + min
            }
            HeightProvider::VeryBiasedToBottom { min, max, inner } => {
                if max - min - inner < 0 {
                    return min;
                }
                let upper = mth_next_int(random, min + inner, max);
                let biased_upper = mth_next_int(random, min, upper - 1);
                mth_next_int(random, min, biased_upper - 1 + inner)
            }
        }
    }
}

fn load_float_provider(v: &Value) -> Result<FloatProvider, String> {
    if let Some(n) = v.as_f64() {
        return Ok(FloatProvider::Constant(n as f32));
    }
    let kind = v.get("type").and_then(Value::as_str).map(full_id).ok_or("float provider: type missing")?;
    let f = |k: &str| v.get(k).and_then(Value::as_f64).map(|x| x as f32).ok_or_else(|| format!("float provider {kind}: {k} missing"));
    match kind.as_str() {
        "minecraft:constant" => Ok(FloatProvider::Constant(f("value")?)),
        "minecraft:uniform" => Ok(FloatProvider::Uniform { min: f("min_inclusive")?, max: f("max_exclusive")? }),
        "minecraft:trapezoid" => Ok(FloatProvider::Trapezoid { min: f("min")?, max: f("max")?, plateau: f("plateau")? }),
        other => Err(format!("float provider {other}: unsupported")),
    }
}

fn load_int_provider(v: &Value) -> Result<IntProvider, String> {
    if let Some(n) = v.as_i64() {
        return Ok(IntProvider::Constant(n as i32));
    }
    let kind = v.get("type").and_then(Value::as_str).map(full_id).ok_or("int provider: type missing")?;
    let int = |k: &str| v.get(k).and_then(Value::as_i64).map(|i| i as i32).ok_or_else(|| format!("int provider {kind}: {k} missing"));
    match kind.as_str() {
        "minecraft:constant" => Ok(IntProvider::Constant(int("value")?)),
        "minecraft:uniform" => Ok(IntProvider::Uniform { min: int("min_inclusive")?, max: int("max_inclusive")? }),
        "minecraft:biased_to_bottom" => Ok(IntProvider::BiasedToBottom { min: int("min_inclusive")?, max: int("max_inclusive")? }),
        "minecraft:very_biased_to_bottom" => Ok(IntProvider::VeryBiasedToBottom { min: int("min_inclusive")?, max: int("max_inclusive")? }),
        other => Err(format!("int provider {other}: unsupported")),
    }
}

fn load_height_provider(v: &Value, min_y: i32, height: i32, sea_level: i32) -> Result<HeightProvider, String> {
    let anchor = |k: &str| resolve_anchor(v.get(k).ok_or_else(|| format!("height provider: {k} missing"))?, min_y, height, sea_level);
    let inner = || v.get("inner").and_then(Value::as_i64).unwrap_or(1) as i32;
    match v.get("type").and_then(Value::as_str).map(full_id).as_deref() {
        None => Ok(HeightProvider::Constant(resolve_anchor(v, min_y, height, sea_level)?)),
        Some("minecraft:constant") => Ok(HeightProvider::Constant(anchor("value")?)),
        Some("minecraft:uniform") => Ok(HeightProvider::Uniform { min: anchor("min_inclusive")?, max: anchor("max_inclusive")? }),
        Some("minecraft:trapezoid") => Ok(HeightProvider::Trapezoid {
            min: anchor("min_inclusive")?,
            max: anchor("max_inclusive")?,
            plateau: v.get("plateau").and_then(Value::as_i64).unwrap_or(0) as i32,
        }),
        Some("minecraft:biased_to_bottom") => Ok(HeightProvider::BiasedToBottom { min: anchor("min_inclusive")?, max: anchor("max_inclusive")?, inner: inner() }),
        Some("minecraft:very_biased_to_bottom") => {
            Ok(HeightProvider::VeryBiasedToBottom { min: anchor("min_inclusive")?, max: anchor("max_inclusive")?, inner: inner() })
        }
        Some(other) => Err(format!("height provider {other}: unsupported")),
    }
}

/// `CaveWorldCarver`.
struct CaveCarver {
    probability: f32,
    y: HeightProvider,
    count: IntProvider,
    thickness: FloatProvider,
    weird_thickness_bias: bool,
    room_vertical_radius_multiplier: FloatProvider,
    horizontal_radius_multiplier: FloatProvider,
    vertical_radius_multiplier: FloatProvider,
    start_vertical_radius_multiplier: FloatProvider,
    floor_level: FloatProvider,
}

/// `CanyonWorldCarver.Shape`.
struct CanyonShape {
    distance_factor: FloatProvider,
    thickness: FloatProvider,
    width_smoothness: i32,
    horizontal_radius_factor: FloatProvider,
    vertical_radius_default_factor: f32,
    vertical_radius_center_factor: f32,
    y_scale: FloatProvider,
}

/// `CanyonWorldCarver`.
struct CanyonCarver {
    probability: f32,
    y: HeightProvider,
    vertical_rotation: FloatProvider,
    shape: CanyonShape,
}

enum Carver {
    Cave(CaveCarver),
    Canyon(CanyonCarver),
}

/// Block position of a chunk's corner and its centre, as `ChunkPos` gives them.
#[derive(Clone, Copy)]
struct ChunkPos {
    min_x: i32,
    min_z: i32,
}

impl ChunkPos {
    fn new(chunk_x: i32, chunk_z: i32) -> Self {
        Self { min_x: chunk_x * 16, min_z: chunk_z * 16 }
    }
}

/// `CarvingMask`: one bit per block of the chunk between `min_y` and
/// `max_y`, indexed `y - min_y + (z + (x << 4)) * height`.
pub struct CarvingMask {
    pub min_y: i32,
    pub max_y: i32,
    height: usize,
    words: Vec<u64>,
    any: bool,
}

impl CarvingMask {
    pub fn new(min_y: i32, max_y: i32) -> Self {
        let height = (max_y - min_y + 1) as usize;
        Self { min_y, max_y, height, words: vec![0; (256 * height).div_ceil(64)], any: false }
    }

    #[inline]
    fn index(&self, x: i32, y: i32, z: i32) -> usize {
        (y - self.min_y) as usize + ((z + (x << 4)) as usize) * self.height
    }

    #[inline]
    fn carve(&mut self, x: i32, y: i32, z: i32) {
        let i = self.index(x, y, z);
        self.words[i >> 6] |= 1 << (i & 63);
        self.any = true;
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32, z: i32) -> bool {
        let i = self.index(x, y, z);
        self.words[i >> 6] & (1 << (i & 63)) != 0
    }

    pub fn is_empty(&self) -> bool {
        !self.any
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Bit index into the chunk: the same segments `CarvingMask.visit`
    /// walks, as `(x, z, bottom_y, top_y)` in visiting order.
    fn runs(&self) -> Vec<(usize, usize, i32, i32)> {
        let mut out = Vec::new();
        let height = self.height;
        for column in 0..256usize {
            let x = column >> 4;
            let z = column & 15;
            let base = column * height;
            let mut y = 0usize;
            while y < height {
                if !self.bit(base + y) {
                    y += 1;
                    continue;
                }
                let start = y;
                while y < height && self.bit(base + y) {
                    y += 1;
                }
                out.push((x, z, start as i32 + self.min_y, (y - 1) as i32 + self.min_y));
            }
        }
        out
    }

    #[inline]
    fn bit(&self, i: usize) -> bool {
        self.words[i >> 6] & (1 << (i & 63)) != 0
    }
}

/// `WorldCarver.canReach`.
fn can_reach(chunk: ChunkPos, x: f64, z: f64, current_step: i32, total_steps: i32, thickness: f32) -> bool {
    let x_mid = (chunk.min_x + 8) as f64;
    let z_mid = (chunk.min_z + 8) as f64;
    let xd = x - x_mid;
    let zd = z - z_mid;
    let remaining = (total_steps - current_step) as f64;
    let rr = (thickness + 2.0 + 16.0) as f64;
    xd * xd + zd * zd - remaining * remaining <= rr * rr
}

/// `WorldCarver.carveEllipsoid`.
#[allow(clippy::too_many_arguments)]
fn carve_ellipsoid(
    chunk: ChunkPos,
    x: f64,
    y: f64,
    z: f64,
    horizontal_radius: f64,
    vertical_radius: f64,
    mask: &mut CarvingMask,
    skip: &mut dyn FnMut(f64, f64, f64, i32) -> bool,
) {
    let center_x = (chunk.min_x + 8) as f64;
    let center_z = (chunk.min_z + 8) as f64;
    let max_delta = 16.0 + horizontal_radius * 2.0;
    if (x - center_x).abs() > max_delta || (z - center_z).abs() > max_delta {
        return;
    }
    let min_x_index = ((x - horizontal_radius).floor() as i32 - chunk.min_x - 1).max(0);
    let max_x_index = ((x + horizontal_radius).floor() as i32 - chunk.min_x).min(15);
    let min_y = ((y - vertical_radius).floor() as i32 - 1).max(mask.min_y);
    let max_y = ((y + vertical_radius).floor() as i32 + 1).min(mask.max_y);
    let min_z_index = ((z - horizontal_radius).floor() as i32 - chunk.min_z - 1).max(0);
    let max_z_index = ((z + horizontal_radius).floor() as i32 - chunk.min_z).min(15);
    for x_index in min_x_index..=max_x_index {
        let world_x = chunk.min_x + x_index;
        let xd = (world_x as f64 + 0.5 - x) / horizontal_radius;
        for z_index in min_z_index..=max_z_index {
            let world_z = chunk.min_z + z_index;
            let zd = (world_z as f64 + 0.5 - z) / horizontal_radius;
            if xd * xd + zd * zd >= 1.0 {
                continue;
            }
            let mut world_y = max_y;
            while world_y > min_y {
                let yd = (world_y as f64 - 0.5 - y) / vertical_radius;
                if !skip(xd, yd, zd, world_y) {
                    mask.carve(x_index, world_y, z_index);
                }
                world_y -= 1;
            }
        }
    }
}

impl CaveCarver {
    fn is_start_chunk(&self, random: &mut LegacyRandomSource) -> bool {
        random.next_float() <= self.probability
    }

    fn carve(&self, random: &mut LegacyRandomSource, chunk: ChunkPos, source: ChunkPos, mask: &mut CarvingMask) {
        let max_distance = 112;
        let cave_count = self.count.sample(random);
        for _ in 0..cave_count {
            let x = (source.min_x + random.next_int_bound(16)) as f64;
            let y = self.y.sample(random) as f64;
            let z = (source.min_z + random.next_int_bound(16)) as f64;
            let horizontal_radius_multiplier = self.horizontal_radius_multiplier.sample(random) as f64;
            let vertical_radius_multiplier = self.vertical_radius_multiplier.sample(random) as f64;
            let start_vertical_radius_multiplier = self.start_vertical_radius_multiplier.sample(random) as f64;
            let floor_level = self.floor_level.sample(random) as f64;
            let mut skip = move |xd: f64, yd: f64, zd: f64, _y: i32| yd <= floor_level || xd * xd + yd * yd + zd * zd >= 1.0;
            let mut tunnels = 1;
            if random.next_int_bound(4) == 0 {
                let y_scale = self.room_vertical_radius_multiplier.sample(random) as f64;
                let thickness = 1.0 + random.next_float() * 6.0;
                let horizontal_radius = 1.5 + (mth_sin(HALF_PI_F as f64) * thickness) as f64;
                let vertical_radius = horizontal_radius * y_scale;
                carve_ellipsoid(chunk, x + 1.0, y, z, horizontal_radius, vertical_radius, mask, &mut skip);
                tunnels += random.next_int_bound(4);
            }
            for _ in 0..tunnels {
                let horizontal_rotation = random.next_float() * TWO_PI_F;
                let vertical_rotation = (random.next_float() - 0.5) / 4.0;
                let thickness = self.thickness(random);
                let distance = max_distance - random.next_int_bound(max_distance / 4);
                let tunnel_seed = random.next_long();
                self.create_tunnel(
                    chunk,
                    tunnel_seed,
                    x,
                    y,
                    z,
                    horizontal_radius_multiplier,
                    vertical_radius_multiplier,
                    thickness,
                    horizontal_rotation,
                    vertical_rotation,
                    0,
                    distance,
                    start_vertical_radius_multiplier,
                    mask,
                    &mut skip,
                );
            }
        }
    }

    fn thickness(&self, random: &mut LegacyRandomSource) -> f32 {
        let mut thickness = self.thickness.sample(random);
        if self.weird_thickness_bias && random.next_int_bound(10) == 0 {
            thickness *= random.next_float() * random.next_float() * 3.0 + 1.0;
        }
        thickness
    }

    #[allow(clippy::too_many_arguments)]
    fn create_tunnel(
        &self,
        chunk: ChunkPos,
        tunnel_seed: i64,
        mut x: f64,
        mut y: f64,
        mut z: f64,
        horizontal_radius_multiplier: f64,
        vertical_radius_multiplier: f64,
        thickness: f32,
        mut horizontal_rotation: f32,
        mut vertical_rotation: f32,
        step: i32,
        dist: i32,
        y_scale: f64,
        mask: &mut CarvingMask,
        skip: &mut dyn FnMut(f64, f64, f64, i32) -> bool,
    ) {
        let mut random = LegacyRandomSource::new(tunnel_seed);
        let split_point = random.next_int_bound(dist / 2) + dist / 4;
        let steep = random.next_int_bound(6) == 0;
        let mut y_rota = 0.0f32;
        let mut x_rota = 0.0f32;
        for current_step in step..dist {
            let horizontal_radius = 1.5 + (mth_sin((PI_F * current_step as f32 / dist as f32) as f64) * thickness) as f64;
            let vertical_radius = horizontal_radius * y_scale;
            let cos_x = mth_cos(vertical_rotation as f64);
            x += (mth_cos(horizontal_rotation as f64) * cos_x) as f64;
            y += mth_sin(vertical_rotation as f64) as f64;
            z += (mth_sin(horizontal_rotation as f64) * cos_x) as f64;
            vertical_rotation *= if steep { 0.92 } else { 0.7 };
            vertical_rotation += x_rota * 0.1;
            horizontal_rotation += y_rota * 0.1;
            x_rota *= 0.9;
            y_rota *= 0.75;
            x_rota += (random.next_float() - random.next_float()) * random.next_float() * 2.0;
            y_rota += (random.next_float() - random.next_float()) * random.next_float() * 4.0;
            if current_step == split_point && thickness > 1.0 {
                let seed_a = random.next_long();
                let thickness_a = random.next_float() * 0.5 + 0.5;
                self.create_tunnel(
                    chunk,
                    seed_a,
                    x,
                    y,
                    z,
                    horizontal_radius_multiplier,
                    vertical_radius_multiplier,
                    thickness_a,
                    horizontal_rotation - HALF_PI_F,
                    vertical_rotation / 3.0,
                    current_step,
                    dist,
                    1.0,
                    mask,
                    skip,
                );
                let seed_b = random.next_long();
                let thickness_b = random.next_float() * 0.5 + 0.5;
                self.create_tunnel(
                    chunk,
                    seed_b,
                    x,
                    y,
                    z,
                    horizontal_radius_multiplier,
                    vertical_radius_multiplier,
                    thickness_b,
                    horizontal_rotation + HALF_PI_F,
                    vertical_rotation / 3.0,
                    current_step,
                    dist,
                    1.0,
                    mask,
                    skip,
                );
                return;
            }
            if random.next_int_bound(4) != 0 {
                if !can_reach(chunk, x, z, current_step, dist, thickness) {
                    return;
                }
                carve_ellipsoid(
                    chunk,
                    x,
                    y,
                    z,
                    horizontal_radius * horizontal_radius_multiplier,
                    vertical_radius * vertical_radius_multiplier,
                    mask,
                    skip,
                );
            }
        }
    }
}

impl CanyonCarver {
    fn is_start_chunk(&self, random: &mut LegacyRandomSource) -> bool {
        random.next_float() <= self.probability
    }

    fn carve(&self, random: &mut LegacyRandomSource, chunk: ChunkPos, source: ChunkPos, mask: &mut CarvingMask, min_gen_y: i32, gen_depth: i32) {
        let max_distance = 112;
        let x = (source.min_x + random.next_int_bound(16)) as f64;
        let y = self.y.sample(random);
        let z = (source.min_z + random.next_int_bound(16)) as f64;
        let horizontal_rotation = random.next_float() * TWO_PI_F;
        let vertical_rotation = self.vertical_rotation.sample(random);
        let y_scale = self.shape.y_scale.sample(random) as f64;
        let thickness = self.shape.thickness.sample(random);
        let distance = (max_distance as f32 * self.shape.distance_factor.sample(random)) as i32;
        let tunnel_seed = random.next_long();
        self.do_carve(chunk, tunnel_seed, x, y as f64, z, thickness, horizontal_rotation, vertical_rotation, distance, y_scale, mask, min_gen_y, gen_depth);
    }

    #[allow(clippy::too_many_arguments)]
    fn do_carve(
        &self,
        chunk: ChunkPos,
        tunnel_seed: i64,
        mut x: f64,
        mut y: f64,
        mut z: f64,
        thickness: f32,
        mut horizontal_rotation: f32,
        mut vertical_rotation: f32,
        distance: i32,
        y_scale: f64,
        mask: &mut CarvingMask,
        min_gen_y: i32,
        gen_depth: i32,
    ) {
        let mut random = LegacyRandomSource::new(tunnel_seed);
        let width_factor_per_height = self.init_width_factors(&mut random, gen_depth);
        let mut y_rota = 0.0f32;
        let mut x_rota = 0.0f32;
        for current_step in 0..distance {
            let mut horizontal_radius = 1.5 + (mth_sin((current_step as f32 * PI_F / distance as f32) as f64) * thickness) as f64;
            let mut vertical_radius = horizontal_radius * y_scale;
            horizontal_radius *= self.shape.horizontal_radius_factor.sample(&mut random) as f64;
            vertical_radius = self.update_vertical_radius(&mut random, vertical_radius, distance as f32, current_step as f32);
            let xc = mth_cos(vertical_rotation as f64);
            let xs = mth_sin(vertical_rotation as f64);
            x += (mth_cos(horizontal_rotation as f64) * xc) as f64;
            y += xs as f64;
            z += (mth_sin(horizontal_rotation as f64) * xc) as f64;
            vertical_rotation *= 0.7;
            vertical_rotation += x_rota * 0.05;
            horizontal_rotation += y_rota * 0.05;
            x_rota *= 0.8;
            y_rota *= 0.5;
            x_rota += (random.next_float() - random.next_float()) * random.next_float() * 2.0;
            y_rota += (random.next_float() - random.next_float()) * random.next_float() * 4.0;
            if random.next_int_bound(4) != 0 {
                if !can_reach(chunk, x, z, current_step, distance, thickness) {
                    return;
                }
                let widths = &width_factor_per_height;
                let mut skip = |xd: f64, yd: f64, zd: f64, y1: i32| {
                    let y_index = y1 - min_gen_y;
                    (xd * xd + zd * zd) * widths[(y_index - 1) as usize] as f64 + yd * yd / 6.0 >= 1.0
                };
                carve_ellipsoid(chunk, x, y, z, horizontal_radius, vertical_radius, mask, &mut skip);
            }
        }
    }

    fn init_width_factors(&self, random: &mut LegacyRandomSource, depth: i32) -> Vec<f32> {
        let mut out = vec![0.0f32; depth as usize];
        let mut width_factor = 1.0f32;
        for (y_index, slot) in out.iter_mut().enumerate() {
            if y_index == 0 || random.next_int_bound(self.shape.width_smoothness) == 0 {
                width_factor = 1.0 + random.next_float() * random.next_float();
            }
            *slot = width_factor * width_factor;
        }
        out
    }

    fn update_vertical_radius(&self, random: &mut LegacyRandomSource, vertical_radius: f64, distance: f32, current_step: f32) -> f64 {
        let vertical_multiplier = 1.0 - (0.5 - current_step / distance).abs() * 2.0;
        let factor = self.shape.vertical_radius_default_factor + self.shape.vertical_radius_center_factor * vertical_multiplier;
        factor as f64 * vertical_radius * (random.next_float() * (1.0 - 0.75) + 0.75) as f64
    }
}

fn load_carver(doc: &Value, min_y: i32, height: i32, sea_level: i32) -> Result<Carver, String> {
    let kind = doc.get("type").and_then(Value::as_str).map(full_id).ok_or("carver: type missing")?;
    let field = |k: &str| doc.get(k).ok_or_else(|| format!("carver {kind}: {k} missing"));
    let float = |k: &str| load_float_provider(field(k)?);
    let probability = field("probability")?.as_f64().ok_or("carver: probability not a number")? as f32;
    let y = load_height_provider(field("y")?, min_y, height, sea_level)?;
    match kind.as_str() {
        "minecraft:cave" => Ok(Carver::Cave(CaveCarver {
            probability,
            y,
            count: load_int_provider(field("count")?)?,
            thickness: float("thickness")?,
            weird_thickness_bias: doc.get("weird_thickness_bias").and_then(Value::as_bool).unwrap_or(false),
            room_vertical_radius_multiplier: float("room_vertical_radius_multiplier")?,
            horizontal_radius_multiplier: float("horizontal_radius_multiplier")?,
            vertical_radius_multiplier: float("vertical_radius_multiplier")?,
            start_vertical_radius_multiplier: doc.get("start_vertical_radius_multiplier").map_or(Ok(FloatProvider::Constant(1.0)), load_float_provider)?,
            floor_level: float("floor_level")?,
        })),
        "minecraft:canyon" => {
            let shape = field("shape")?;
            let sfield = |k: &str| shape.get(k).ok_or_else(|| format!("canyon shape: {k} missing"));
            let sfloat = |k: &str| load_float_provider(sfield(k)?);
            let snum = |k: &str| sfield(k)?.as_f64().map(|v| v as f32).ok_or_else(|| format!("canyon shape: {k} not a number"));
            Ok(Carver::Canyon(CanyonCarver {
                probability,
                y,
                vertical_rotation: float("vertical_rotation")?,
                shape: CanyonShape {
                    distance_factor: sfloat("distance_factor")?,
                    thickness: sfloat("thickness")?,
                    width_smoothness: sfield("width_smoothness")?.as_i64().ok_or("canyon shape: width_smoothness not a number")? as i32,
                    horizontal_radius_factor: sfloat("horizontal_radius_factor")?,
                    vertical_radius_default_factor: snum("vertical_radius_default_factor")?,
                    vertical_radius_center_factor: snum("vertical_radius_center_factor")?,
                    y_scale: sfloat("y_scale")?,
                },
            }))
        }
        other => Err(format!("carver {other}: unsupported")),
    }
}

/// The block id of a palette entry (`{"id": .., "properties": ..}`).
fn block_of(name: &str) -> Option<String> {
    let v: Value = serde_json::from_str(name).ok()?;
    v.get("id").and_then(Value::as_str).map(full_id)
}

/// Bounded memo of the carver biome per chunk (`ChunkAccess.carverBiome`).
#[derive(Default)]
pub struct CarverBiomeCache {
    map: HashMap<(i32, i32), u16>,
    order: VecDeque<(i32, i32)>,
}

impl CarverBiomeCache {
    const CAPACITY: usize = 65536;

    pub fn get(&self, key: (i32, i32)) -> Option<u16> {
        self.map.get(&key).copied()
    }

    pub fn insert(&mut self, key: (i32, i32), biome: u16) {
        if self.map.insert(key, biome).is_none() {
            self.order.push_back(key);
            while self.order.len() > Self::CAPACITY {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

/// The carver stage of one world: every carver the biomes reference,
/// the carver list per biome, and the palette predicates the apply
/// step needs.
pub struct CarverStage {
    carvers: Vec<Carver>,
    /// Carver indices per biome, in the biome's list order.
    biome_carvers: Vec<Arc<[usize]>>,
    seed: i64,
    min_y: i32,
    height: i32,
    /// Per palette id: `#minecraft:uncarvable`, grass block or mycelium, dirt.
    uncarvable: Vec<bool>,
    grass: Vec<bool>,
    dirt: Vec<bool>,
}

impl CarverStage {
    /// Loads the carvers the biomes name. `Err` when a carver document
    /// uses a provider or type this port does not have, so the world
    /// keeps vanilla's carvers.
    pub fn load(loader: &mut Loader, biome_ids: &[String], palette: &Palette, seed: i64, min_y: i32, height: i32, sea_level: i32) -> Result<Self, String> {
        let mut carvers = Vec::new();
        let mut carver_index: HashMap<String, usize> = HashMap::new();
        let mut biome_carvers = Vec::with_capacity(biome_ids.len());
        for biome in biome_ids {
            let doc = loader.document("biome", biome)?;
            let list = match doc.get("carvers") {
                None => Vec::new(),
                Some(Value::Array(a)) => a.clone(),
                Some(Value::String(s)) => vec![Value::String(s.clone())],
                Some(other) => return Err(format!("biome {biome}: carvers is {other}")),
            };
            let mut indices = Vec::with_capacity(list.len());
            for entry in &list {
                let id = match entry {
                    Value::String(s) => full_id(s),
                    other => return Err(format!("biome {biome}: inline carver {other} unsupported")),
                };
                let index = match carver_index.get(&id) {
                    Some(&i) => i,
                    None => {
                        let carver_doc = loader.document("carver", &id)?;
                        let carver = load_carver(&carver_doc, min_y, height, sea_level).map_err(|e| format!("{id}: {e}"))?;
                        carvers.push(carver);
                        carver_index.insert(id.clone(), carvers.len() - 1);
                        carvers.len() - 1
                    }
                };
                indices.push(index);
            }
            biome_carvers.push(Arc::from(indices));
        }
        // The game sends the tag with its block index; a datapack directory (tests) gets vanilla's tag.
        let mut uncarvable_blocks: Vec<String> = Vec::new();
        match loader.document("block_index", "minecraft:overworld") {
            Ok(index) => {
                if let Some(members) = index.get("tags").and_then(|t| t.get("minecraft:uncarvable")).and_then(Value::as_array) {
                    for m in members {
                        if let Some(name) = m.as_str() {
                            uncarvable_blocks.push(full_id(name));
                        }
                    }
                }
            }
            Err(_) => uncarvable_blocks.push("minecraft:bedrock".to_string()),
        }
        let mut uncarvable = Vec::with_capacity(palette.names.len());
        let mut grass = Vec::with_capacity(palette.names.len());
        let mut dirt = Vec::with_capacity(palette.names.len());
        for name in &palette.names {
            let block = block_of(name).ok_or_else(|| format!("palette entry {name}: no block id"))?;
            uncarvable.push(uncarvable_blocks.contains(&block));
            grass.push(block == "minecraft:grass_block" || block == "minecraft:mycelium");
            dirt.push(block == "minecraft:dirt");
        }
        Ok(Self { carvers, biome_carvers, seed, min_y, height, uncarvable, grass, dirt })
    }

    /// The mask bounds vanilla uses for a fresh chunk: seven protected
    /// blocks below the top of the world.
    pub fn mask_bounds(&self) -> (i32, i32) {
        (self.min_y + 1, self.min_y + self.height - 1 - 7)
    }

    /// `generateCarvers` up to the mask: every source chunk within 8
    /// walks its carvers into this chunk. `source_biomes` holds the
    /// biome index at quart (4 cx, 0, 4 cz) of every source chunk,
    /// `(dx + 8) * 17 + dz + 8`, as `source_biomes` lays them out.
    pub fn build_mask(&self, chunk_x: i32, chunk_z: i32, source_biomes: &[u16; 289]) -> CarvingMask {
        let (mask_min, mask_max) = self.mask_bounds();
        let mut mask = CarvingMask::new(mask_min, mask_max);
        let chunk = ChunkPos::new(chunk_x, chunk_z);
        let mut random = LegacyRandomSource::new(0);
        for dx in -8..=8 {
            for dz in -8..=8 {
                let (sx, sz) = (chunk_x + dx, chunk_z + dz);
                let source = ChunkPos::new(sx, sz);
                let biome = source_biomes[((dx + 8) * 17 + dz + 8) as usize] as usize;
                let Some(list) = self.biome_carvers.get(biome) else {
                    continue;
                };
                for (index, &carver) in list.iter().enumerate() {
                    random.set_large_feature_seed(self.seed.wrapping_add(index as i64), sx, sz);
                    match &self.carvers[carver] {
                        Carver::Cave(c) => {
                            if c.is_start_chunk(&mut random) {
                                c.carve(&mut random, chunk, source, &mut mask);
                            }
                        }
                        Carver::Canyon(c) => {
                            if c.is_start_chunk(&mut random) {
                                c.carve(&mut random, chunk, source, &mut mask, self.min_y, self.height);
                            }
                        }
                    }
                }
            }
        }
        mask
    }

    /// `applyCarvingMask` on the surfaced chunk the context wraps.
    pub(crate) fn apply(&self, ctx: &mut Ctx, mask: &CarvingMask, aquifer: &mut Aquifer) -> CarveStats {
        let mut stats = CarveStats::default();
        if mask.is_empty() {
            return stats;
        }
        let cfg = ctx.cfg;
        let palette = &cfg.palette;
        let min_block_x = ctx.chunk.vol.min[0];
        let min_block_z = ctx.chunk.vol.min[2];
        for (x, z, bottom_y, top_y) in mask.runs() {
            let world_x = min_block_x + x as i32;
            let world_z = min_block_z + z as i32;
            let mut has_grass = false;
            let mut world_y = top_y;
            while world_y >= bottom_y {
                let old = ctx.chunk.get(cfg, x, world_y, z);
                if !self.uncarvable[old as usize] {
                    if self.grass[old as usize] {
                        has_grass = true;
                    }
                    stats.aquifer_calls += 1;
                    if let Some(sub) = aquifer.compute_substance(world_x, world_y, world_z, 0.0) {
                        let id = match sub {
                            Substance::Air => cfg.air,
                            Substance::Water => cfg.water,
                            Substance::Lava => cfg.lava,
                        };
                        stats.carved += 1;
                        let fluid = palette.is_fluid(id);
                        ctx.chunk.set_without_post(palette, x, world_y, z, id);
                        if aquifer.should_schedule_fluid_update && fluid {
                            ctx.chunk.mark_post(x, world_y, z);
                        }
                        if has_grass {
                            let below = ctx.chunk.get(cfg, x, world_y - 1, z);
                            if self.dirt[below as usize] {
                                stats.top_material_calls += 1;
                                if let Some(top) = ctx.top_material(x, world_y - 1, z, fluid) {
                                    ctx.chunk.set(palette, x, world_y - 1, z, top);
                                }
                            }
                        }
                    }
                }
                world_y -= 1;
            }
        }
        stats
    }
}

/// What one apply pass did, for the stage timer.
#[derive(Default, Debug, Clone, Copy)]
pub struct CarveStats {
    pub aquifer_calls: u64,
    pub carved: u64,
    pub top_material_calls: u64,
    pub mask_ms: f64,
    pub apply_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mth_cos_is_sin_shifted() {
        // cos(0) reads table entry 16384, the same entry sin(pi/2) reads.
        assert_eq!(mth_cos(0.0), mth_sin(HALF_PI_F as f64));
        assert_eq!(mth_sin(0.0), 0.0);
    }

    #[test]
    fn mask_runs_follow_visit_order() {
        let mut mask = CarvingMask::new(-63, 312);
        mask.carve(0, 10, 0);
        mask.carve(0, 11, 0);
        mask.carve(0, 20, 0);
        mask.carve(0, -63, 1);
        mask.carve(0, 312, 0);
        let runs = mask.runs();
        // Column (0,0) first: its two runs bottom-up, the top-of-column run
        // then column (0,1) even though its bit follows in the bit set.
        assert_eq!(runs, vec![(0, 0, 10, 11), (0, 0, 20, 20), (0, 0, 312, 312), (0, 1, -63, -63)]);
        assert_eq!(mask.count(), 5);
    }

    /// `PAINITE_SIN_TABLE=<file>` with `Float.floatToRawIntBits` of every
    /// `Mth.SIN` entry, one per line, as a JDK wrote it.
    #[test]
    fn sin_table_matches_jdk() {
        let Ok(path) = std::env::var("PAINITE_SIN_TABLE") else {
            eprintln!("PAINITE_SIN_TABLE not set, skipping");
            return;
        };
        let text = std::fs::read_to_string(path).expect("sin table");
        let mut differ = 0;
        for (i, line) in text.lines().enumerate() {
            let want: i32 = line.trim().parse().expect("bits");
            let got = sin_table()[i];
            if got.to_bits() as i32 != want {
                differ += 1;
                if differ < 5 {
                    eprintln!("entry {i}: jdk {} libm {}", f32::from_bits(want as u32), got);
                }
            }
        }
        assert_eq!(differ, 0, "{differ} table entries differ");
    }

    #[test]
    fn very_biased_int_uses_three_draws() {
        let p = IntProvider::VeryBiasedToBottom { min: 0, max: 14 };
        let mut r = LegacyRandomSource::new(1);
        let v = p.sample(&mut r);
        assert!((0..=14).contains(&v));
    }
}
