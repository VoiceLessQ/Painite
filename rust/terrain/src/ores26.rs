//! Ore features (26.3 `OreFeature` with its placement modifiers) run as
//! one batch per chunk over a packed view of the 3x3 chunk region.
//!
//! Every placed feature reseeds its random from the decoration seed, so
//! the ores of a chunk depend on nothing but the blocks they read; the
//! game hands over the sections the batch can touch, this module returns
//! the block writes in vanilla order.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::Arc;

use serde_json::Value;

use crate::df26::Loader;
use crate::surface26::{resolve_anchor, zoomed_biome, Palette, QuartGrid};
use crate::xoroshiro::XoroshiroRandomSource;

/// `WorldgenRandom` over a Xoroshiro source: a `BitRandomSource`, so
/// every draw is the java.util.Random derivation over `next(bits)` =
/// the top bits of `nextLong`, not Xoroshiro's own methods.
pub struct WorldgenRandom {
    inner: XoroshiroRandomSource,
}

impl WorldgenRandom {
    pub fn from_seed(seed: i64) -> Self {
        Self { inner: XoroshiroRandomSource::from_legacy_seed(seed) }
    }

    fn next(&mut self, bits: u32) -> i32 {
        ((self.inner.next_long() as u64) >> (64 - bits)) as i32
    }

    pub fn next_int_bounded(&mut self, bound: i32) -> i32 {
        assert!(bound > 0, "Bound must be positive");
        if bound & (bound - 1) == 0 {
            return ((bound as i64 * self.next(31) as i64) >> 31) as i32;
        }
        loop {
            let sample = self.next(31);
            let modulo = sample % bound;
            if sample.wrapping_sub(modulo).wrapping_add(bound - 1) >= 0 {
                return modulo;
            }
        }
    }

    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 * 5.960_464_5e-8f32
    }

    /// `DOUBLE_MULTIPLIER` is a double field initialised from the float literal 1.110223E-16F.
    pub fn next_double(&mut self) -> f64 {
        let upper = self.next(26) as i64;
        let lower = self.next(27) as i64;
        let combined = (upper << 27) + lower;
        combined as f64 * (1.110_223e-16f32 as f64)
    }
}

/// `GenerationStep.Decoration.UNDERGROUND_ORES.ordinal()`.
pub const UNDERGROUND_ORES_STEP: i32 = 6;
/// Chunks per side of the region a feature stage may write.
const REGION_SIDE: usize = 3;
const REGION_CHUNKS: usize = REGION_SIDE * REGION_SIDE;
/// Heightmap raw data: 9 bits per column, 7 per long, 256 columns.
const HEIGHT_BITS: u32 = 9;
const HEIGHT_PER_LONG: usize = 7;
pub const HEIGHT_LONGS: usize = 256_usize.div_ceil(HEIGHT_PER_LONG);
/// States the stage wrote are kept in the view as `u16::MAX - palette id`,
/// above every global state id the game can hand over.
const WRITTEN_BASE: u16 = u16::MAX - 1024;

/// `RuleTest` over a global block state id at a position.
enum RuleTest {
    AlwaysTrue,
    /// Membership per block index.
    Tag(Arc<Vec<bool>>),
    Block(u16),
    Height { min: i32, max: i32 },
    Not(Box<RuleTest>),
    AllOf(Vec<RuleTest>),
    AnyOf(Vec<RuleTest>),
}

impl RuleTest {
    fn test(&self, block: u16, y: i32) -> bool {
        match self {
            RuleTest::AlwaysTrue => true,
            RuleTest::Tag(members) => members.get(block as usize).copied().unwrap_or(false),
            RuleTest::Block(b) => *b == block,
            RuleTest::Height { min, max } => *min <= y && y <= *max,
            RuleTest::Not(r) => !r.test(block, y),
            RuleTest::AllOf(rs) => rs.iter().all(|r| r.test(block, y)),
            RuleTest::AnyOf(rs) => rs.iter().any(|r| r.test(block, y)),
        }
    }
}

enum IntProvider {
    Constant(i32),
    Uniform { min: i32, max: i32 },
}

impl IntProvider {
    fn sample(&self, random: &mut WorldgenRandom) -> i32 {
        match self {
            IntProvider::Constant(v) => *v,
            IntProvider::Uniform { min, max } => random_between_inclusive(random, *min, *max),
        }
    }
}

enum HeightProvider {
    Constant(i32),
    Uniform { min: i32, max: i32 },
    Trapezoid { min: i32, max: i32, plateau: i32 },
}

impl HeightProvider {
    fn sample(&self, random: &mut WorldgenRandom) -> i32 {
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
        }
    }

    fn bounds(&self) -> (i32, i32) {
        match *self {
            HeightProvider::Constant(y) => (y, y),
            HeightProvider::Uniform { min, max } | HeightProvider::Trapezoid { min, max, .. } => (min, max.max(min)),
        }
    }
}

fn random_between_inclusive(random: &mut WorldgenRandom, min: i32, max: i32) -> i32 {
    random.next_int_bounded(max - min + 1) + min
}

enum Modifier {
    Count(IntProvider),
    InSquare,
    HeightRange(HeightProvider),
    Biome,
    Rarity(i32),
}

struct Target {
    rule: RuleTest,
    /// Palette id of the state to place.
    state: u16,
}

struct OreFeature {
    targets: Vec<Target>,
    size: i32,
    discard_chance: f32,
}

struct PlacedOre {
    feature: OreFeature,
    placement: Vec<Modifier>,
    /// Sections (index from the bottom) the feature can read or write.
    section_lo: usize,
    section_hi: usize,
}

/// The ore stage of one world: the placed ore features the game may
/// batch, block tags and the block state index it reads sections with.
pub struct OreStage {
    /// Placed feature ids the stage serves, in the order the game indexes them.
    pub placed_ids: Vec<String>,
    placed: Vec<PlacedOre>,
    /// `biome_has[biome][placed]`: `BiomeGenerationSettings.hasFeature`.
    biome_has: Vec<Vec<bool>>,
    /// Block index per global block state id.
    state_block: Vec<u16>,
    /// `BlockState.isAir` per global block state id.
    state_air: Vec<bool>,
    /// Block index per palette id, for states this stage wrote itself.
    palette_block: Vec<u16>,
    min_y: i32,
    section_count: usize,
}

/// A batch handed over by the game before its sections are known.
pub struct OreBatch {
    pub seeds: Vec<i64>,
    pub placed: Vec<u16>,
    pub grid: QuartGrid,
}

impl OreStage {
    /// Load from the `block_index` document, the ore-typed `feature` and
    /// `placed_feature` documents and the biome documents (feature
    /// membership). Placed features whose feature or placement uses an
    /// unsupported form are left out rather than failing the stage.
    pub fn load(loader: &mut Loader, palette: &mut Palette, biome_ids: &[String], min_y: i32, height: i32, sea_level: i32) -> Result<Self, String> {
        let index = loader.document("block_index", "minecraft:overworld")?;
        let blocks: Vec<String> = index
            .get("blocks")
            .and_then(Value::as_array)
            .ok_or("block_index: blocks missing")?
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| "block_index: block id not a string".to_string()))
            .collect::<Result<_, _>>()?;
        let block_index: HashMap<&str, u16> = blocks.iter().enumerate().map(|(i, b)| (b.as_str(), i as u16)).collect();
        let state_block: Vec<u16> = index
            .get("states")
            .and_then(Value::as_array)
            .ok_or("block_index: states missing")?
            .iter()
            .map(|v| v.as_u64().and_then(|b| u16::try_from(b).ok()).ok_or_else(|| "block_index: bad state entry".to_string()))
            .collect::<Result<_, _>>()?;
        if state_block.iter().any(|&b| b as usize >= blocks.len()) {
            return Err("block_index: state refers to a missing block".into());
        }
        let mut state_air = vec![false; state_block.len()];
        for v in index.get("air").and_then(Value::as_array).ok_or("block_index: air missing")? {
            let id = v.as_u64().ok_or("block_index: bad air entry")? as usize;
            *state_air.get_mut(id).ok_or("block_index: air id out of range")? = true;
        }
        let mut tags: HashMap<String, Arc<Vec<bool>>> = HashMap::new();
        for (tag, members) in index.get("tags").and_then(Value::as_object).ok_or("block_index: tags missing")? {
            let mut set = vec![false; blocks.len()];
            for m in members.as_array().ok_or("block_index: tag not a list")? {
                let name = m.as_str().ok_or("block_index: tag member not a string")?;
                if let Some(&b) = block_index.get(name) {
                    set[b as usize] = true;
                }
            }
            tags.insert(crate::df26::full_id(tag), Arc::new(set));
        }
        let section_count = (height >> 4) as usize;
        let mut placed_ids = Vec::new();
        let mut placed = Vec::new();
        for id in loader.ids_of("placed_feature")? {
            let doc = loader.document("placed_feature", &id)?;
            let Some(feature_id) = doc.get("feature").and_then(Value::as_str) else {
                continue;
            };
            let Ok(feature_doc) = loader.document("feature", feature_id) else {
                continue;
            };
            if feature_doc.get("type").and_then(Value::as_str).map(crate::df26::full_id).as_deref() != Some("minecraft:ore") {
                continue;
            }
            let feature = match load_feature(&feature_doc, palette, &block_index, &tags) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let placement = match load_placement(&doc, min_y, height, sea_level) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let (hmin, hmax) = placement
                .iter()
                .find_map(|m| if let Modifier::HeightRange(h) = m { Some(h.bounds()) } else { None })
                .unwrap_or((min_y, min_y + height - 1));
            let reach = 2 + max_radius(feature.size) + 1;
            let lo = ((hmin - reach - min_y).max(0) >> 4) as usize;
            let hi = (((hmax + reach - min_y).max(0) >> 4) as usize).min(section_count - 1);
            placed_ids.push(id);
            placed.push(PlacedOre { feature, placement, section_lo: lo.min(hi), section_hi: hi });
        }
        let placed_index: HashMap<&str, usize> = placed_ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        let mut biome_has = Vec::with_capacity(biome_ids.len());
        for biome in biome_ids {
            let doc = loader.document("biome", biome)?;
            let mut has = vec![false; placed.len()];
            for step in doc.get("features").and_then(Value::as_array).ok_or_else(|| format!("biome {biome}: features missing"))? {
                for f in step.as_array().ok_or_else(|| format!("biome {biome}: step not a list"))? {
                    if let Some(&i) = f.as_str().map(crate::df26::full_id).as_deref().and_then(|id| placed_index.get(id)) {
                        has[i] = true;
                    }
                }
            }
            biome_has.push(has);
        }
        let palette_block = palette
            .names
            .iter()
            .map(|canon| {
                serde_json::from_str::<Value>(canon)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_str).map(crate::df26::full_id))
                    .and_then(|id| block_index.get(id.as_str()).copied())
                    .unwrap_or(u16::MAX)
            })
            .collect();
        if state_block.len() >= WRITTEN_BASE as usize {
            return Err(format!("block_index: {} states, too many for the section view", state_block.len()));
        }
        Ok(Self { placed_ids, placed, biome_has, state_block, state_air, palette_block, min_y, section_count })
    }

    pub fn section_count(&self) -> usize {
        self.section_count
    }

    /// The section range (inclusive, from the bottom) a batch can touch,
    /// or `None` when a placed index is unknown.
    pub fn plan(&self, batch: &OreBatch) -> Option<(usize, usize)> {
        let mut range: Option<(usize, usize)> = None;
        for &p in &batch.placed {
            let ore = self.placed.get(p as usize)?;
            range = Some(match range {
                None => (ore.section_lo, ore.section_hi),
                Some((lo, hi)) => (lo.min(ore.section_lo), hi.max(ore.section_hi)),
            });
        }
        range
    }

    /// Run the batch over the region, in the order the game gave it.
    pub fn apply(&self, batch: &OreBatch, region: &mut Region, biome_zoom_seed: i64) {
        for (&seed, &p) in batch.seeds.iter().zip(&batch.placed) {
            let Some(ore) = self.placed.get(p as usize) else {
                continue;
            };
            let mut random = WorldgenRandom::from_seed(seed);
            let origin = (region.chunk_x << 4, self.min_y, region.chunk_z << 4);
            self.modify(ore, p as usize, 0, origin, &mut random, region, batch, biome_zoom_seed);
        }
    }

    /// `FeaturePlacer.place`: depth-first through the modifiers, then the feature.
    #[allow(clippy::too_many_arguments)]
    fn modify(&self, ore: &PlacedOre, placed: usize, index: usize, pos: (i32, i32, i32), random: &mut WorldgenRandom, region: &mut Region, batch: &OreBatch, zoom_seed: i64) {
        let Some(modifier) = ore.placement.get(index) else {
            self.place(&ore.feature, random, pos, region);
            return;
        };
        let mut out: Vec<(i32, i32, i32)> = Vec::new();
        match modifier {
            Modifier::Count(count) => {
                let n = count.sample(random);
                out.extend(std::iter::repeat_n(pos, n.max(0) as usize));
            }
            Modifier::InSquare => {
                let x = random.next_int_bounded(16) + pos.0;
                let z = random.next_int_bounded(16) + pos.2;
                out.push((x, pos.1, z));
            }
            Modifier::HeightRange(h) => out.push((pos.0, h.sample(random), pos.2)),
            Modifier::Biome => {
                let biome = zoomed_biome(zoom_seed, &batch.grid, pos.0, pos.1, pos.2) as usize;
                if self.biome_has.get(biome).is_some_and(|has| has[placed]) {
                    out.push(pos);
                }
            }
            Modifier::Rarity(chance) => {
                if random.next_float() < 1.0f32 / (*chance as f32) {
                    out.push(pos);
                }
            }
        }
        for next in out {
            self.modify(ore, placed, index + 1, next, random, region, batch, zoom_seed);
        }
    }

    /// `OreFeature.place`.
    fn place(&self, feature: &OreFeature, random: &mut WorldgenRandom, origin: (i32, i32, i32), region: &mut Region) -> bool {
        let dir = random.next_float() * (PI as f32);
        let spread_xy = feature.size as f32 / 8.0;
        let max_radius = max_radius(feature.size);
        let (ox, oy, oz) = origin;
        let x0 = ox as f64 + (dir as f64).sin() * spread_xy as f64;
        let x1 = ox as f64 - (dir as f64).sin() * spread_xy as f64;
        let z0 = oz as f64 + (dir as f64).cos() * spread_xy as f64;
        let z1 = oz as f64 - (dir as f64).cos() * spread_xy as f64;
        let y0 = (oy + random.next_int_bounded(3) - 2) as f64;
        let y1 = (oy + random.next_int_bounded(3) - 2) as f64;
        let ceil_spread = spread_xy.ceil() as i32;
        let x_start = ox - ceil_spread - max_radius;
        let y_start = oy - 2 - max_radius;
        let z_start = oz - ceil_spread - max_radius;
        let size_xz = 2 * (ceil_spread + max_radius);
        let size_y = 2 * (2 + max_radius);
        for xprobe in x_start..=x_start + size_xz {
            for zprobe in z_start..=z_start + size_xz {
                if y_start <= region.ocean_floor_height(xprobe, zprobe) {
                    return self.do_place(feature, random, [x0, x1, z0, z1, y0, y1], (x_start, y_start, z_start), size_xz, size_y, region);
                }
            }
        }
        false
    }

    /// `OreFeature.doPlace`.
    #[allow(clippy::too_many_arguments)]
    fn do_place(&self, feature: &OreFeature, random: &mut WorldgenRandom, ends: [f64; 6], start: (i32, i32, i32), size_xz: i32, size_y: i32, region: &mut Region) -> bool {
        let [x0, x1, z0, z1, y0, y1] = ends;
        let (x_start, y_start, z_start) = start;
        let size = feature.size as usize;
        let mut placed = 0;
        let mut tested = BitSet::default();
        let mut data = vec![0f64; size * 4];
        for i in 0..size {
            let step = i as f32 / feature.size as f32;
            let xx = lerp(step as f64, x0, x1);
            let yy = lerp(step as f64, y0, y1);
            let zz = lerp(step as f64, z0, z1);
            let ss = random.next_double() * feature.size as f64 / 16.0;
            let r = ((mth_sin(((PI as f32) * step) as f64) + 1.0f32) as f64 * ss + 1.0) / 2.0;
            data[i * 4] = xx;
            data[i * 4 + 1] = yy;
            data[i * 4 + 2] = zz;
            data[i * 4 + 3] = r;
        }
        for i1 in 0..size.saturating_sub(1) {
            if data[i1 * 4 + 3] <= 0.0 {
                continue;
            }
            for i2 in i1 + 1..size {
                if data[i2 * 4 + 3] <= 0.0 {
                    continue;
                }
                let dx = data[i1 * 4] - data[i2 * 4];
                let dy = data[i1 * 4 + 1] - data[i2 * 4 + 1];
                let dz = data[i1 * 4 + 2] - data[i2 * 4 + 2];
                let dr = data[i1 * 4 + 3] - data[i2 * 4 + 3];
                if dr * dr > dx * dx + dy * dy + dz * dz {
                    if dr > 0.0 {
                        data[i2 * 4 + 3] = -1.0;
                    } else {
                        data[i1 * 4 + 3] = -1.0;
                    }
                }
            }
        }
        for i in 0..size {
            let r = data[i * 4 + 3];
            if r < 0.0 {
                continue;
            }
            let xx = data[i * 4];
            let yy = data[i * 4 + 1];
            let zz = data[i * 4 + 2];
            let x_min = ((xx - r).floor() as i32).max(x_start);
            let y_min = ((yy - r).floor() as i32).max(y_start);
            let z_min = ((zz - r).floor() as i32).max(z_start);
            let x_max = ((xx + r).floor() as i32).max(x_min);
            let y_max = ((yy + r).floor() as i32).max(y_min);
            let z_max = ((zz + r).floor() as i32).max(z_min);
            for x in x_min..=x_max {
                let xd = (x as f64 + 0.5 - xx) / r;
                if xd * xd >= 1.0 {
                    continue;
                }
                for y in y_min..=y_max {
                    let yd = (y as f64 + 0.5 - yy) / r;
                    if xd * xd + yd * yd >= 1.0 {
                        continue;
                    }
                    for z in z_min..=z_max {
                        let zd = (z as f64 + 0.5 - zz) / r;
                        if xd * xd + yd * yd + zd * zd >= 1.0 || region.outside_build_height(y) {
                            continue;
                        }
                        let bit = (x - x_start) as i64 + (y - y_start) as i64 * size_xz as i64 + (z - z_start) as i64 * size_xz as i64 * size_y as i64;
                        if tested.set(bit) {
                            continue;
                        }
                        if !region.can_write(x, z) {
                            continue;
                        }
                        let Some(state) = region.get(x, y, z) else {
                            continue;
                        };
                        let block = self.block_of(state);
                        for target in &feature.targets {
                            if self.can_place_ore(feature, target, block, random, region, x, y, z) {
                                region.set(x, y, z, target.state);
                                placed += 1;
                                break;
                            }
                        }
                    }
                }
            }
        }
        placed > 0
    }

    /// `AbstractOreFeature.canPlaceOre`.
    #[allow(clippy::too_many_arguments)]
    fn can_place_ore(&self, feature: &OreFeature, target: &Target, block: u16, random: &mut WorldgenRandom, region: &Region, x: i32, y: i32, z: i32) -> bool {
        if !target.rule.test(block, y) {
            return false;
        }
        let skip_air_check = if feature.discard_chance <= 0.0 {
            true
        } else if feature.discard_chance >= 1.0 {
            false
        } else {
            random.next_float() >= feature.discard_chance
        };
        skip_air_check || !self.adjacent_to_air(region, x, y, z)
    }

    /// `AbstractOreFeature.isAdjacentToAir` in `Direction.values()` order.
    fn adjacent_to_air(&self, region: &Region, x: i32, y: i32, z: i32) -> bool {
        const NEIGHBOURS: [(i32, i32, i32); 6] = [(0, -1, 0), (0, 1, 0), (0, 0, -1), (0, 0, 1), (-1, 0, 0), (1, 0, 0)];
        NEIGHBOURS.iter().any(|&(dx, dy, dz)| match region.get(x + dx, y + dy, z + dz) {
            Some(state) => self.is_air(state),
            None => true,
        })
    }

    fn block_of(&self, state: u16) -> u16 {
        if state >= WRITTEN_BASE {
            return self.palette_block.get((u16::MAX - state) as usize).copied().unwrap_or(u16::MAX);
        }
        self.state_block.get(state as usize).copied().unwrap_or(u16::MAX)
    }

    fn is_air(&self, state: u16) -> bool {
        state < WRITTEN_BASE && self.state_air.get(state as usize).copied().unwrap_or(false)
    }
}

/// `Mth.ceil((size / 16.0F * 2.0F + 1.0F) / 2.0F)`.
fn max_radius(size: i32) -> i32 {
    ((size as f32 / 16.0 * 2.0 + 1.0) / 2.0).ceil() as i32
}

fn lerp(alpha: f64, p0: f64, p1: f64) -> f64 {
    p0 + alpha * (p1 - p0)
}

/// `Mth.sin`: the 65536-entry float table indexed by `(long)(i * SIN_SCALE) & 65535`.
fn mth_sin(i: f64) -> f32 {
    const SIN_SCALE: f64 = 10430.378350470453;
    let idx = ((i * SIN_SCALE) as i64 & 65535) as f64;
    (idx / SIN_SCALE).sin() as f32
}

/// A growable bit set with `java.util.BitSet.get/set` semantics.
#[derive(Default)]
struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    /// Sets the bit and returns whether it was already set.
    fn set(&mut self, bit: i64) -> bool {
        let bit = bit as u64;
        let word = (bit >> 6) as usize;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let mask = 1u64 << (bit & 63);
        let was = self.words[word] & mask != 0;
        self.words[word] |= mask;
        was
    }
}

fn load_rule(v: &Value, block_index: &HashMap<&str, u16>, tags: &HashMap<String, Arc<Vec<bool>>>) -> Result<RuleTest, String> {
    let kind = v.get("predicate_type").and_then(Value::as_str).map(crate::df26::full_id).ok_or("rule test: predicate_type missing")?;
    let rules = |key: &str| -> Result<Vec<RuleTest>, String> {
        v.get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("rule test {kind}: {key} missing"))?
            .iter()
            .map(|r| load_rule(r, block_index, tags))
            .collect()
    };
    Ok(match kind.as_str() {
        "minecraft:always_true" => RuleTest::AlwaysTrue,
        "minecraft:tag_match" => {
            let tag = v.get("tag").and_then(Value::as_str).map(crate::df26::full_id).ok_or("tag_match: tag missing")?;
            RuleTest::Tag(tags.get(&tag).cloned().ok_or_else(|| format!("tag_match: unknown tag {tag}"))?)
        }
        "minecraft:block_match" => {
            let block = v.get("block").and_then(Value::as_str).map(crate::df26::full_id).ok_or("block_match: block missing")?;
            RuleTest::Block(*block_index.get(block.as_str()).ok_or_else(|| format!("block_match: unknown block {block}"))?)
        }
        "minecraft:height_match" => RuleTest::Height {
            min: v.get("min_inclusive").and_then(Value::as_i64).ok_or("height_match: min_inclusive missing")? as i32,
            max: v.get("max_inclusive").and_then(Value::as_i64).ok_or("height_match: max_inclusive missing")? as i32,
        },
        "minecraft:not" => RuleTest::Not(Box::new(load_rule(v.get("rule").ok_or("not: rule missing")?, block_index, tags)?)),
        "minecraft:all_of" => RuleTest::AllOf(rules("rules")?),
        "minecraft:any_of" => RuleTest::AnyOf(rules("rules")?),
        other => return Err(format!("rule test {other}: unsupported")),
    })
}

fn load_feature(doc: &Value, palette: &mut Palette, block_index: &HashMap<&str, u16>, tags: &HashMap<String, Arc<Vec<bool>>>) -> Result<OreFeature, String> {
    let size = doc.get("size").and_then(Value::as_i64).ok_or("ore: size missing")? as i32;
    if !(0..=64).contains(&size) {
        return Err(format!("ore: size {size} out of range"));
    }
    let discard_chance = doc.get("discard_chance_on_air_exposure").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let mut targets = Vec::new();
    for t in doc.get("targets").and_then(Value::as_array).ok_or("ore: targets missing")? {
        let rule = load_rule(t.get("target").ok_or("ore target: target missing")?, block_index, tags)?;
        let state = palette.intern_json(t.get("state").ok_or("ore target: state missing")?)?;
        targets.push(Target { rule, state });
    }
    Ok(OreFeature { targets, size, discard_chance })
}

fn load_int_provider(v: &Value) -> Result<IntProvider, String> {
    if let Some(n) = v.as_i64() {
        return Ok(IntProvider::Constant(n as i32));
    }
    let kind = v.get("type").and_then(Value::as_str).map(crate::df26::full_id).ok_or("int provider: type missing")?;
    let int = |k: &str| v.get(k).and_then(Value::as_i64).map(|i| i as i32).ok_or_else(|| format!("int provider {kind}: {k} missing"));
    match kind.as_str() {
        "minecraft:constant" => Ok(IntProvider::Constant(int("value")?)),
        "minecraft:uniform" => Ok(IntProvider::Uniform { min: int("min_inclusive")?, max: int("max_inclusive")? }),
        other => Err(format!("int provider {other}: unsupported")),
    }
}

fn load_height_provider(v: &Value, min_y: i32, height: i32, sea_level: i32) -> Result<HeightProvider, String> {
    let anchor = |k: &str| resolve_anchor(v.get(k).ok_or_else(|| format!("height provider: {k} missing"))?, min_y, height, sea_level);
    match v.get("type").and_then(Value::as_str).map(crate::df26::full_id).as_deref() {
        None => Ok(HeightProvider::Constant(resolve_anchor(v, min_y, height, sea_level)?)),
        Some("minecraft:constant") => Ok(HeightProvider::Constant(anchor("value")?)),
        Some("minecraft:uniform") => Ok(HeightProvider::Uniform { min: anchor("min_inclusive")?, max: anchor("max_inclusive")? }),
        Some("minecraft:trapezoid") => Ok(HeightProvider::Trapezoid {
            min: anchor("min_inclusive")?,
            max: anchor("max_inclusive")?,
            plateau: v.get("plateau").and_then(Value::as_i64).unwrap_or(0) as i32,
        }),
        Some(other) => Err(format!("height provider {other}: unsupported")),
    }
}

fn load_placement(doc: &Value, min_y: i32, height: i32, sea_level: i32) -> Result<Vec<Modifier>, String> {
    let mut out = Vec::new();
    for m in doc.get("placement").and_then(Value::as_array).ok_or("placed feature: placement missing")? {
        let kind = m.get("type").and_then(Value::as_str).map(crate::df26::full_id).ok_or("placement: type missing")?;
        out.push(match kind.as_str() {
            "minecraft:count" => Modifier::Count(load_int_provider(m.get("count").ok_or("count: count missing")?)?),
            "minecraft:in_square" => Modifier::InSquare,
            "minecraft:height_range" => Modifier::HeightRange(load_height_provider(m.get("height").ok_or("height_range: height missing")?, min_y, height, sea_level)?),
            "minecraft:biome" => Modifier::Biome,
            "minecraft:rarity_filter" => Modifier::Rarity(m.get("chance").and_then(Value::as_i64).ok_or("rarity_filter: chance missing")? as i32),
            other => return Err(format!("placement {other}: unsupported")),
        });
    }
    Ok(out)
}

/// One section as the game stores it: bit storage over a palette of
/// global state ids (an empty palette means the storage holds the ids).
pub struct Section {
    bits: u32,
    raw: Vec<u64>,
    palette: Vec<u16>,
    /// Filled on the first write.
    decoded: Option<Vec<u16>>,
}

impl Section {
    pub fn new(bits: u32, raw: Vec<u64>, palette: Vec<u16>) -> Result<Self, String> {
        if bits == 0 {
            if palette.len() != 1 {
                return Err("section: single-value palette needs one entry".into());
            }
        } else if bits > 16 {
            return Err(format!("section: {bits} bits"));
        } else if raw.len() < 4096_usize.div_ceil(64 / bits as usize) {
            return Err("section: storage too short".into());
        }
        Ok(Self { bits, raw, palette, decoded: None })
    }

    fn get(&self, index: usize) -> u16 {
        if let Some(d) = &self.decoded {
            return d[index];
        }
        if self.bits == 0 {
            return self.palette[0];
        }
        let per_long = 64 / self.bits as usize;
        let word = self.raw[index / per_long];
        let value = (word >> ((index % per_long) as u32 * self.bits)) & ((1u64 << self.bits) - 1);
        if self.palette.is_empty() {
            value as u16
        } else {
            self.palette.get(value as usize).copied().unwrap_or(u16::MAX)
        }
    }

    fn set(&mut self, index: usize, state: u16) {
        if self.decoded.is_none() {
            let all: Vec<u16> = (0..4096).map(|i| self.get(i)).collect();
            self.decoded = Some(all);
        }
        self.decoded.as_mut().unwrap()[index] = state;
    }
}

/// A write the game applies: section slot, position packed `x | y << 4 | z << 8`, palette id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    pub slot: u32,
    pub packed: u16,
    pub state: u16,
}

/// The 3x3 chunk region around one chunk as sections the game handed over.
pub struct Region {
    pub chunk_x: i32,
    pub chunk_z: i32,
    min_y: i32,
    height: i32,
    section_count: usize,
    /// Indexed `chunk_slot * section_count + section`, chunk slot `(dz + 1) * 3 + (dx + 1)`.
    sections: Vec<Option<Section>>,
    /// `OCEAN_FLOOR_WG` first-available height per column, `chunk_slot * 256 + (z << 4 | x)`.
    ocean_floor: Vec<i32>,
    pub writes: Vec<Write>,
}

impl Region {
    /// `heights`: the nine chunks' raw heightmap longs, slot order.
    pub fn new(chunk_x: i32, chunk_z: i32, min_y: i32, height: i32, heights: &[i64]) -> Result<Self, String> {
        if heights.len() != REGION_CHUNKS * HEIGHT_LONGS {
            return Err(format!("region: {} heightmap longs, expected {}", heights.len(), REGION_CHUNKS * HEIGHT_LONGS));
        }
        let section_count = (height >> 4) as usize;
        let mut ocean_floor = Vec::with_capacity(REGION_CHUNKS * 256);
        for slot in 0..REGION_CHUNKS {
            let raw = &heights[slot * HEIGHT_LONGS..(slot + 1) * HEIGHT_LONGS];
            for i in 0..256 {
                let word = raw[i / HEIGHT_PER_LONG] as u64;
                let value = (word >> ((i % HEIGHT_PER_LONG) as u32 * HEIGHT_BITS)) & ((1u64 << HEIGHT_BITS) - 1);
                ocean_floor.push(value as i32 + min_y);
            }
        }
        Ok(Self {
            chunk_x,
            chunk_z,
            min_y,
            height,
            section_count,
            sections: (0..REGION_CHUNKS * section_count).map(|_| None).collect(),
            ocean_floor,
            writes: Vec::new(),
        })
    }

    /// Install a section; `slot` is the chunk slot, `section` counts from the bottom.
    pub fn install(&mut self, slot: usize, section: usize, data: Section) -> Result<(), String> {
        if slot >= REGION_CHUNKS || section >= self.section_count {
            return Err(format!("region: section {section} of slot {slot} out of range"));
        }
        self.sections[slot * self.section_count + section] = Some(data);
        Ok(())
    }

    fn slot_of(&self, x: i32, z: i32) -> Option<usize> {
        let dx = (x >> 4) - self.chunk_x + 1;
        let dz = (z >> 4) - self.chunk_z + 1;
        if (0..REGION_SIDE as i32).contains(&dx) && (0..REGION_SIDE as i32).contains(&dz) {
            Some((dz * REGION_SIDE as i32 + dx) as usize)
        } else {
            None
        }
    }

    fn outside_build_height(&self, y: i32) -> bool {
        y < self.min_y || y >= self.min_y + self.height
    }

    /// `WorldGenRegion.ensureCanWrite` with the feature stage's write radius of one chunk.
    fn can_write(&self, x: i32, z: i32) -> bool {
        self.slot_of(x, z).is_some()
    }

    fn ocean_floor_height(&self, x: i32, z: i32) -> i32 {
        match self.slot_of(x, z) {
            Some(slot) => self.ocean_floor[slot * 256 + (((z & 15) << 4) | (x & 15)) as usize],
            None => self.min_y - 1,
        }
    }

    fn section_index(&self, x: i32, y: i32, z: i32) -> Option<(usize, usize)> {
        if self.outside_build_height(y) {
            return None;
        }
        let slot = self.slot_of(x, z)?;
        let section = ((y - self.min_y) >> 4) as usize;
        Some((slot * self.section_count + section, (((y & 15) << 8) | ((z & 15) << 4) | (x & 15)) as usize))
    }

    /// The global state id at a position; `None` where the game would
    /// see air (outside the build height or the handed-over sections).
    pub fn get(&self, x: i32, y: i32, z: i32) -> Option<u16> {
        let (i, index) = self.section_index(x, y, z)?;
        self.sections[i].as_ref().map(|s| s.get(index))
    }

    /// Record a write and apply it to the view; `state` is a palette id,
    /// stored as `u16::MAX - state` (see `WRITTEN_BASE`).
    pub fn set(&mut self, x: i32, y: i32, z: i32, state: u16) {
        let Some((i, index)) = self.section_index(x, y, z) else {
            return;
        };
        let Some(section) = self.sections[i].as_mut() else {
            return;
        };
        section.set(index, u16::MAX - state);
        let slot = (i / self.section_count) as u32;
        let section_no = (i % self.section_count) as u32;
        self.writes.push(Write { slot: slot | section_no << 4, packed: (((z & 15) << 8) | ((y & 15) << 4) | (x & 15)) as u16, state });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset_matches_java_semantics() {
        let mut b = BitSet::default();
        assert!(!b.set(5));
        assert!(b.set(5));
        assert!(!b.set(1000));
        assert!(b.set(1000));
        assert!(!b.set(64));
    }

    #[test]
    fn mth_sin_table_endpoints() {
        assert_eq!(mth_sin(0.0), 0.0);
        assert!((mth_sin(PI / 2.0) - 1.0).abs() < 1e-3);
        assert!((mth_sin(PI) - 0.0).abs() < 1e-3);
    }

    #[test]
    fn max_radius_matches_vanilla_sizes() {
        assert_eq!(max_radius(17), 2);
        assert_eq!(max_radius(9), 2);
        assert_eq!(max_radius(33), 3);
        assert_eq!(max_radius(64), 5);
        assert_eq!(max_radius(3), 1);
    }

    #[test]
    fn section_packed_reads_and_decodes_on_write() {
        // 4 bits, 16 per long: entries 0..16 in order.
        let raw: Vec<u64> = (0..256).map(|_| (0..16).fold(0u64, |w, i| w | (i as u64) << (4 * i))).collect();
        let palette: Vec<u16> = (100..116).collect();
        let mut s = Section::new(4, raw, palette).unwrap();
        assert_eq!(s.get(0), 100);
        assert_eq!(s.get(15), 115);
        assert_eq!(s.get(4095), 115);
        s.set(15, 7);
        assert_eq!(s.get(15), 7);
        assert_eq!(s.get(14), 114);
        let single = Section::new(0, vec![], vec![42]).unwrap();
        assert_eq!(single.get(4095), 42);
        let global = Section::new(15, vec![1 | 2 << 15 | 3 << 30; 1024], vec![]).unwrap();
        assert_eq!(global.get(0), 1);
        assert_eq!(global.get(1), 2);
        assert_eq!(global.get(2), 3);
        assert_eq!(global.get(4), 1);
    }

    #[test]
    fn region_heights_and_slots() {
        let mut heights = vec![0i64; REGION_CHUNKS * HEIGHT_LONGS];
        // Column (x=3, z=2) of the centre slot 4: index 35, long 5, shift 0.
        heights[4 * HEIGHT_LONGS + 5] = 100;
        let mut r = Region::new(10, -3, -64, 384, &heights).unwrap();
        assert_eq!(r.ocean_floor_height(163, -46), 36);
        assert_eq!(r.ocean_floor_height(160, -48), -64);
        assert_eq!(r.slot_of(160, -48), Some(4));
        assert_eq!(r.slot_of(159, -49), Some(0));
        assert_eq!(r.slot_of(176, -32), Some(8));
        assert_eq!(r.slot_of(143, -48), None);
        assert_eq!(r.get(160, 0, -48), None);
        r.install(4, 4, Section::new(0, vec![], vec![9]).unwrap()).unwrap();
        assert_eq!(r.get(160, 0, -48), Some(9));
        assert_eq!(r.get(160, 16, -48), None);
        r.set(161, 1, -47, 3);
        assert_eq!(r.get(161, 1, -47), Some(u16::MAX - 3));
        assert_eq!(r.writes, vec![Write { slot: 4 | 4 << 4, packed: 1 << 8 | 1 << 4 | 1, state: 3 }]);
    }
}
