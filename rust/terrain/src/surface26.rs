//! The surface pass: `MaterialSystem.buildSurface`, the material rule
//! tree, the eroded-badlands and frozen-ocean extensions and the two
//! worldgen heightmaps, run on the block array the fill produced.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::aquifer26::Aquifer;
use crate::carvers26::{CarveStats, CarverStage, CarvingMask};
use crate::df26::{full_id, java_floor_div, Loader, Mode, Node, PointCache, SampleCtx, SliceUniformAxes, Volume, ALL_AXES};
use crate::noise26::NoiseStack;
use crate::simplex::TemperatureNoises;
use crate::xoroshiro::XoroshiroPositionalRandomFactory;

type N = Arc<Node>;

pub const FLAG_AIR: u8 = 1;
pub const FLAG_FLUID: u8 = 2;
pub const FLAG_MOTION_BLOCKING: u8 = 4;

/// `DimensionType.WAY_BELOW_MIN_Y`.
const WAY_BELOW_MIN_Y: i32 = -32512;
const CLAY_BAND_COUNT: usize = 192;

/// Block states the surface can place, interned by canonical JSON so the
/// game can decode them back with the block state codec.
#[derive(Default, Clone)]
pub struct Palette {
    pub names: Vec<String>,
    index: HashMap<String, u16>,
    /// FLAG_* bits per entry, supplied by the game (or a test) once the
    /// names are known.
    pub flags: Vec<u8>,
}

impl Palette {
    pub fn intern_name(&mut self, name: &str) -> u16 {
        self.intern_canonical(format!("{{\"id\":\"{}\"}}", full_id(name)))
    }

    /// The canonical text of a block state in any datapack form: a bare
    /// id, `{"id": .., "properties": ..}` (26.3 codec) or the older
    /// `{"Name": .., "Properties": ..}`. Canonical is the 26.3 object form.
    pub fn canonical(v: &Value) -> Result<String, String> {
        match v {
            Value::String(s) => Ok(format!("{{\"id\":\"{}\"}}", full_id(s))),
            Value::Object(o) => {
                let name = o.get("id").or_else(|| o.get("Name")).and_then(Value::as_str).ok_or("block state: id missing")?;
                let mut canon = serde_json::Map::new();
                canon.insert("id".into(), Value::String(full_id(name)));
                if let Some(props) = o.get("properties").or_else(|| o.get("Properties")).and_then(Value::as_object).filter(|p| !p.is_empty()) {
                    canon.insert("properties".into(), Value::Object(props.clone()));
                }
                Ok(Value::Object(canon).to_string())
            }
            other => Err(format!("block state: unexpected {other}")),
        }
    }

    /// `result_state` in either datapack form.
    pub fn intern_json(&mut self, v: &Value) -> Result<u16, String> {
        let canon = Self::canonical(v)?;
        Ok(self.intern_canonical(canon))
    }

    fn intern_canonical(&mut self, canon: String) -> u16 {
        if let Some(&i) = self.index.get(&canon) {
            return i;
        }
        let i = self.names.len() as u16;
        self.names.push(canon.clone());
        self.index.insert(canon, i);
        self.flags.push(0);
        i
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn set_flags(&mut self, flags: &[u8]) -> Result<(), String> {
        if flags.len() != self.names.len() {
            return Err(format!("palette flags: {} given, {} entries", flags.len(), self.names.len()));
        }
        self.flags.copy_from_slice(flags);
        Ok(())
    }

    #[inline]
    pub fn is_air(&self, id: u16) -> bool {
        self.flags[id as usize] & FLAG_AIR != 0
    }

    #[inline]
    pub fn is_fluid(&self, id: u16) -> bool {
        self.flags[id as usize] & FLAG_FLUID != 0
    }

    #[inline]
    pub fn blocks_motion(&self, id: u16) -> bool {
        self.flags[id as usize] & FLAG_MOTION_BLOCKING != 0
    }
}

/// What `Biome.getTemperature` needs from a biome document.
#[derive(Clone, Debug)]
pub struct BiomeInfo {
    pub id: String,
    pub temperature: f32,
    pub frozen: bool,
}

/// `VerticalAnchor` resolved against the generation context.
pub(crate) fn resolve_anchor(v: &Value, min_y: i32, height: i32, sea_level: i32) -> Result<i32, String> {
    let o = v.as_object().ok_or("anchor: not an object")?;
    let int = |k: &str| o.get(k).and_then(Value::as_i64).map(|i| i as i32);
    if let Some(y) = int("absolute") {
        Ok(y)
    } else if let Some(off) = int("above_bottom") {
        Ok(min_y + off)
    } else if let Some(off) = int("below_top") {
        Ok(height - 1 + min_y - off)
    } else if let Some(off) = int("relative_to_sea_level") {
        Ok(sea_level + off)
    } else {
        Err(format!("anchor: unknown form {v}"))
    }
}

pub enum Condition {
    AbovePreliminarySurface,
    Biome(Vec<bool>),
    Hole,
    NoiseThreshold { noise: Arc<NoiseStack>, slot: usize, min: f64, max: f64, is_3d: bool },
    Not(Box<Condition>),
    Steep,
    StoneDepth { offset: i32, add_surface_depth: bool, secondary_depth_range: i32, ceiling: bool },
    Temperature,
    VerticalGradient { random: XoroshiroPositionalRandomFactory, true_at_and_below: i32, false_at_and_above: i32 },
    Water { offset: i32, surface_depth_multiplier: i32, add_stone_depth: bool },
    YAbove { anchor: i32, surface_depth_multiplier: i32, add_stone_depth: bool },
}

pub enum Rule {
    Bandlands,
    Block(u16),
    Condition(Condition, Box<Rule>),
    OreVein { ore: u16, raw_ore: u16, filler: u16, raw_ore_chance: f32, slot: usize, density: N, richness: N, filler_gap: N },
    Sequence(Vec<Rule>),
}

/// One world's compiled surface: rule tree, noises, clay bands, biomes.
pub struct SurfaceConfig {
    pub rule: Rule,
    pub palette: Palette,
    pub biomes: Vec<BiomeInfo>,
    chunk_surface_level: N,
    surface_noise: Arc<NoiseStack>,
    surface_secondary_noise: Arc<NoiseStack>,
    clay_bands_offset_noise: Arc<NoiseStack>,
    badlands_pillar: Arc<NoiseStack>,
    badlands_pillar_roof: Arc<NoiseStack>,
    badlands_surface: Arc<NoiseStack>,
    iceberg_pillar: Arc<NoiseStack>,
    iceberg_pillar_roof: Arc<NoiseStack>,
    iceberg_surface: Arc<NoiseStack>,
    clay_bands: [u16; CLAY_BAND_COUNT],
    noise_random: XoroshiroPositionalRandomFactory,
    ore_random: XoroshiroPositionalRandomFactory,
    temperature_noises: TemperatureNoises,
    noise_slots: usize,
    ore_slots: usize,
    pub min_y: i32,
    pub height: i32,
    pub sea_level: i32,
    pub default_block: u16,
    pub air: u16,
    pub water: u16,
    pub lava: u16,
    packed_ice: u16,
    snow_block: u16,
    eroded_badlands: Option<usize>,
    frozen_ocean: Option<usize>,
    deep_frozen_ocean: Option<usize>,
}

struct Parser<'a> {
    loader: &'a mut Loader,
    palette: &'a mut Palette,
    biome_index: &'a HashMap<String, usize>,
    min_y: i32,
    height: i32,
    sea_level: i32,
    noise_slots: usize,
    ore_slots: usize,
    depth: usize,
}

impl Parser<'_> {
    fn rule(&mut self, v: &Value) -> Result<Rule, String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("material rule nesting deeper than 64".into());
        }
        let out = match v {
            Value::String(id) => {
                let doc = self.loader.document("material_rule", id)?;
                self.rule(&doc).map_err(|e| format!("{id}: {e}"))
            }
            Value::Object(o) => self.rule_object(o),
            other => Err(format!("material rule: unexpected {other}")),
        };
        self.depth -= 1;
        out
    }

    fn rule_object(&mut self, o: &serde_json::Map<String, Value>) -> Result<Rule, String> {
        let ty = o.get("type").and_then(Value::as_str).ok_or("type: missing")?;
        let ty = ty.strip_prefix("minecraft:").unwrap_or(ty);
        Ok(match ty {
            "bandlands" => Rule::Bandlands,
            "block" => Rule::Block(self.palette.intern_json(o.get("result_state").ok_or("result_state: missing")?)?),
            "condition" => {
                let c = self.condition(o.get("if_true").ok_or("if_true: missing")?)?;
                let r = self.rule(o.get("then_run").ok_or("then_run: missing")?)?;
                Rule::Condition(c, Box::new(r))
            }
            "sequence" => {
                let seq = o.get("sequence").and_then(Value::as_array).ok_or("sequence: missing")?;
                if seq.is_empty() {
                    return Err("sequence: empty".into());
                }
                let mut rules = Vec::with_capacity(seq.len());
                for r in seq {
                    rules.push(self.rule(r)?);
                }
                if rules.len() == 1 {
                    rules.pop().unwrap()
                } else {
                    Rule::Sequence(rules)
                }
            }
            "ore_vein" => {
                let ore = self.palette.intern_json(o.get("ore_block").ok_or("ore_block: missing")?)?;
                let raw_ore = self.palette.intern_json(o.get("raw_ore_block").ok_or("raw_ore_block: missing")?)?;
                let filler = self.palette.intern_json(o.get("filler_block").ok_or("filler_block: missing")?)?;
                let raw_ore_chance = o.get("raw_ore_chance").and_then(Value::as_f64).ok_or("raw_ore_chance: missing")? as f32;
                let density = self.function(o.get("density").ok_or("density: missing")?)?;
                let richness = self.function(o.get("richness").ok_or("richness: missing")?)?;
                let filler_gap = self.function(o.get("filler_gap").ok_or("filler_gap: missing")?)?;
                let slot = self.ore_slots;
                self.ore_slots += 1;
                Rule::OreVein { ore, raw_ore, filler, raw_ore_chance, slot, density, richness, filler_gap }
            }
            other => return Err(format!("material rule type {other}: unsupported")),
        })
    }

    /// A density function compiled the way `DensityFunctionCompiler`
    /// does for any sampler: with the slice-uniform-axes rewrite.
    fn function(&mut self, v: &Value) -> Result<N, String> {
        let n = self.loader.parse(v)?;
        Ok(SliceUniformAxes::new().rewrite(&n, ALL_AXES))
    }

    fn condition(&mut self, v: &Value) -> Result<Condition, String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("material condition nesting deeper than 64".into());
        }
        let out = match v {
            Value::String(id) => {
                let doc = self.loader.document("material_condition", id)?;
                self.condition(&doc).map_err(|e| format!("{id}: {e}"))
            }
            Value::Object(o) => self.condition_object(o),
            other => Err(format!("material condition: unexpected {other}")),
        };
        self.depth -= 1;
        out
    }

    fn condition_object(&mut self, o: &serde_json::Map<String, Value>) -> Result<Condition, String> {
        let ty = o.get("type").and_then(Value::as_str).ok_or("type: missing")?;
        let ty = ty.strip_prefix("minecraft:").unwrap_or(ty);
        let int = |k: &str| -> Result<i32, String> { o.get(k).and_then(Value::as_i64).map(|i| i as i32).ok_or_else(|| format!("{k}: missing")) };
        let boolean = |k: &str| -> Result<bool, String> { o.get(k).and_then(Value::as_bool).ok_or_else(|| format!("{k}: missing")) };
        Ok(match ty {
            "above_preliminary_surface" => Condition::AbovePreliminarySurface,
            "biome" => {
                let mut set = vec![false; self.biome_index.len()];
                let list = match o.get("biome_is").ok_or("biome_is: missing")? {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string).ok_or("biome_is: not a string".to_string())).collect::<Result<_, _>>()?,
                    other => return Err(format!("biome_is: unexpected {other}")),
                };
                for id in list {
                    if id.starts_with('#') {
                        return Err(format!("biome_is: tag {id} unsupported"));
                    }
                    // A biome absent from this world's registry can never match.
                    if let Some(&i) = self.biome_index.get(&full_id(&id)) {
                        set[i] = true;
                    }
                }
                Condition::Biome(set)
            }
            "hole" => Condition::Hole,
            "noise_threshold" => {
                let id = o.get("noise").and_then(Value::as_str).ok_or("noise: missing")?;
                let noise = self.loader.noise(id)?;
                let min = o.get("min_threshold").and_then(Value::as_f64).ok_or("min_threshold: missing")?;
                let max = o.get("max_threshold").and_then(Value::as_f64).unwrap_or(f64::MAX);
                let is_3d = o.get("is_3d").and_then(Value::as_bool).unwrap_or(false);
                let slot = self.noise_slots;
                self.noise_slots += 1;
                Condition::NoiseThreshold { noise, slot, min, max, is_3d }
            }
            "not" => Condition::Not(Box::new(self.condition(o.get("invert").ok_or("invert: missing")?)?)),
            "steep" => Condition::Steep,
            "stone_depth" => {
                let surface_type = o.get("surface_type").and_then(Value::as_str).ok_or("surface_type: missing")?;
                let ceiling = match surface_type {
                    "ceiling" => true,
                    "floor" => false,
                    other => return Err(format!("surface_type {other}: unknown")),
                };
                Condition::StoneDepth {
                    offset: int("offset")?,
                    add_surface_depth: boolean("add_surface_depth")?,
                    secondary_depth_range: int("secondary_depth_range")?,
                    ceiling,
                }
            }
            "temperature" => Condition::Temperature,
            "vertical_gradient" => {
                let name = o.get("random_name").and_then(Value::as_str).ok_or("random_name: missing")?;
                let random = self.loader.root_factory().from_hash_of(&full_id(name)).fork_positional();
                Condition::VerticalGradient {
                    random,
                    true_at_and_below: self.anchor(o.get("true_at_and_below").ok_or("true_at_and_below: missing")?)?,
                    false_at_and_above: self.anchor(o.get("false_at_and_above").ok_or("false_at_and_above: missing")?)?,
                }
            }
            "water" => Condition::Water {
                offset: int("offset")?,
                surface_depth_multiplier: int("surface_depth_multiplier")?,
                add_stone_depth: boolean("add_stone_depth")?,
            },
            "y_above" => Condition::YAbove {
                anchor: self.anchor(o.get("anchor").ok_or("anchor: missing")?)?,
                surface_depth_multiplier: int("surface_depth_multiplier")?,
                add_stone_depth: boolean("add_stone_depth")?,
            },
            other => return Err(format!("material condition type {other}: unsupported")),
        })
    }

    fn anchor(&self, v: &Value) -> Result<i32, String> {
        resolve_anchor(v, self.min_y, self.height, self.sea_level)
    }
}

impl SurfaceConfig {
    /// Build from the noise settings document and the registry documents
    /// the loader can reach. `biome_ids` fixes the biome index order the
    /// caller will use in quart grids.
    pub fn load(loader: &mut Loader, settings: &Value, biome_ids: &[String]) -> Result<Self, String> {
        let sea_level = settings.get("sea_level").and_then(Value::as_i64).ok_or("sea_level: missing")? as i32;
        let noise = settings.get("noise").ok_or("noise: missing")?;
        let min_y = noise.get("min_y").and_then(Value::as_i64).ok_or("noise.min_y: missing")? as i32;
        let height = noise.get("height").and_then(Value::as_i64).ok_or("noise.height: missing")? as i32;
        let mut palette = Palette::default();
        let air = palette.intern_name("minecraft:air");
        let default_block = palette.intern_json(settings.get("default_block").ok_or("default_block: missing")?)?;
        let water = palette.intern_name("minecraft:water");
        let lava = palette.intern_name("minecraft:lava");
        let packed_ice = palette.intern_name("minecraft:packed_ice");
        let snow_block = palette.intern_name("minecraft:snow_block");

        let mut biomes = Vec::with_capacity(biome_ids.len());
        let mut biome_index = HashMap::new();
        for id in biome_ids {
            let id = full_id(id);
            let doc = loader.document("biome", &id)?;
            let temperature = doc.get("temperature").and_then(Value::as_f64).ok_or_else(|| format!("biome {id}: temperature missing"))? as f32;
            let frozen = match doc.get("temperature_modifier").and_then(Value::as_str).unwrap_or("none") {
                "none" => false,
                "frozen" => true,
                other => return Err(format!("biome {id}: temperature_modifier {other} unknown")),
            };
            biome_index.insert(id.clone(), biomes.len());
            biomes.push(BiomeInfo { id, temperature, frozen });
        }
        let biome_slot = |name: &str| biome_index.get(name).copied();
        let eroded_badlands = biome_slot("minecraft:eroded_badlands");
        let frozen_ocean = biome_slot("minecraft:frozen_ocean");
        let deep_frozen_ocean = biome_slot("minecraft:deep_frozen_ocean");

        let rule_doc = settings.get("material_rule").ok_or("material_rule: missing")?;
        let mut parser = Parser { loader, palette: &mut palette, biome_index: &biome_index, min_y, height, sea_level, noise_slots: 0, ore_slots: 0, depth: 0 };
        let rule = parser.rule(rule_doc)?;
        let noise_slots = parser.noise_slots;
        let ore_slots = parser.ore_slots;

        let router = loader.router("minecraft:overworld")?;
        let chunk_surface_level = router.get("chunk_surface_level").cloned().ok_or("noise_router.chunk_surface_level: missing")?;
        let noise_random = *loader.root_factory();
        let ore_random = noise_random.from_hash_of("minecraft:ore").fork_positional();
        let clay_bands = generate_bands(&mut noise_random.from_hash_of("minecraft:clay_bands"), &mut palette);
        let mut get = |id: &str| loader.noise(id);
        Ok(Self {
            surface_noise: get("minecraft:surface")?,
            surface_secondary_noise: get("minecraft:surface_secondary")?,
            clay_bands_offset_noise: get("minecraft:clay_bands_offset")?,
            badlands_pillar: get("minecraft:badlands_pillar")?,
            badlands_pillar_roof: get("minecraft:badlands_pillar_roof")?,
            badlands_surface: get("minecraft:badlands_surface")?,
            iceberg_pillar: get("minecraft:iceberg_pillar")?,
            iceberg_pillar_roof: get("minecraft:iceberg_pillar_roof")?,
            iceberg_surface: get("minecraft:iceberg_surface")?,
            rule,
            palette,
            biomes,
            chunk_surface_level,
            clay_bands,
            noise_random,
            ore_random,
            temperature_noises: TemperatureNoises::new(),
            noise_slots,
            ore_slots,
            min_y,
            height,
            sea_level,
            default_block,
            air,
            water,
            lava,
            packed_ice,
            snow_block,
            eroded_badlands,
            frozen_ocean,
            deep_frozen_ocean,
        })
    }

    /// `MaterialSystem.getSurfaceDepth`.
    fn surface_depth(&self, x: i32, z: i32) -> i32 {
        let noise = self.surface_noise.get(x as f64, 0.0, z as f64) as f64;
        (noise * 2.75 + 3.0 + self.noise_random.at(x, 0, z).next_double() * 0.25) as i32
    }

    /// `MaterialSystem.getBand`.
    fn band(&self, x: i32, y: i32, z: i32) -> u16 {
        let offset = java_round_f32(self.clay_bands_offset_noise.get(x as f64, 0.0, z as f64) * 4.0f32);
        let n = CLAY_BAND_COUNT as i32;
        self.clay_bands[((y + offset + n) % n) as usize]
    }

    /// `Biome.getTemperature` for a biome at a block position.
    fn temperature(&self, biome: usize, x: i32, y: i32, z: i32) -> f32 {
        let info = &self.biomes[biome];
        self.temperature_noises.height_adjusted(info.temperature, info.frozen, x, y, z, self.sea_level)
    }
}

/// `Math.round(float)` for the values `getBand` sees.
fn java_round_f32(v: f32) -> i32 {
    (v as f64 + 0.5).floor().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// `MaterialSystem.generateBands` and `makeBands`.
fn generate_bands(random: &mut crate::xoroshiro::XoroshiroRandomSource, palette: &mut Palette) -> [u16; CLAY_BAND_COUNT] {
    let terracotta = palette.intern_name("minecraft:terracotta");
    let orange = palette.intern_name("minecraft:orange_terracotta");
    let yellow = palette.intern_name("minecraft:yellow_terracotta");
    let brown = palette.intern_name("minecraft:brown_terracotta");
    let red = palette.intern_name("minecraft:red_terracotta");
    let white = palette.intern_name("minecraft:white_terracotta");
    let light_gray = palette.intern_name("minecraft:light_gray_terracotta");
    let mut bands = [terracotta; CLAY_BAND_COUNT];
    let len = CLAY_BAND_COUNT as i32;
    let mut i = 0i32;
    while i < len {
        i += random.next_int_bounded(5) + 1;
        if i < len {
            bands[i as usize] = orange;
        }
        i += 1;
    }
    let mut make = |random: &mut crate::xoroshiro::XoroshiroRandomSource, base_width: i32, state: u16| {
        let count = next_int_between_inclusive(random, 6, 15);
        for _ in 0..count {
            let width = base_width + random.next_int_bounded(3);
            let start = random.next_int_bounded(len);
            let mut p = 0;
            while start + p < len && p < width {
                bands[(start + p) as usize] = state;
                p += 1;
            }
        }
    };
    make(random, 1, yellow);
    make(random, 2, brown);
    make(random, 1, red);
    let white_count = next_int_between_inclusive(random, 9, 15);
    let mut ix = 0;
    let mut start = 0i32;
    while ix < white_count && start < len {
        bands[start as usize] = white;
        if start - 1 > 0 && random.next_boolean() {
            bands[(start - 1) as usize] = light_gray;
        }
        if start + 1 < len && random.next_boolean() {
            bands[(start + 1) as usize] = light_gray;
        }
        ix += 1;
        start += random.next_int_bounded(16) + 4;
    }
    bands
}

fn next_int_between_inclusive(random: &mut crate::xoroshiro::XoroshiroRandomSource, min: i32, max: i32) -> i32 {
    random.next_int_bounded(max - min + 1) + min
}

/// The biome quart grid a chunk's surface pass reads: the chunk's own
/// 4x4 quart columns plus one quart of margin on each side, every
/// quart layer of the world. Index `y + (x + z * 6) * size_y`.
pub struct QuartGrid {
    pub min_x: i32,
    pub min_y: i32,
    pub min_z: i32,
    pub size_y: usize,
    pub data: Vec<u16>,
}

impl QuartGrid {
    pub const SIZE_XZ: usize = 6;

    /// The grid origin for a chunk.
    pub fn origin(chunk_x: i32, chunk_z: i32, min_block_y: i32) -> (i32, i32, i32) {
        (chunk_x * 4 - 1, min_block_y >> 2, chunk_z * 4 - 1)
    }

    #[inline]
    pub fn get(&self, qx: i32, qy: i32, qz: i32) -> u16 {
        // ChunkAccess.getNoiseBiome clamps the quart y into the chunk.
        let y = (qy - self.min_y).clamp(0, self.size_y as i32 - 1) as usize;
        let x = (qx - self.min_x).clamp(0, Self::SIZE_XZ as i32 - 1) as usize;
        let z = (qz - self.min_z).clamp(0, Self::SIZE_XZ as i32 - 1) as usize;
        self.data[y + (x + z * Self::SIZE_XZ) * self.size_y]
    }
}

/// `LinearCongruentialGenerator.next`.
#[inline]
fn lcg_next(rval: i64, c: i64) -> i64 {
    rval.wrapping_mul(rval.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)).wrapping_add(c)
}

#[inline]
fn fiddle(rval: i64) -> f64 {
    let uniform = (rval >> 24).rem_euclid(1024) as f64 / 1024.0;
    (uniform - 0.5) * 0.9
}

/// `BiomeManager.getBiome(x, y, z)`: the biome zoom.
pub fn zoomed_biome(seed: i64, grid: &QuartGrid, x: i32, y: i32, z: i32) -> u16 {
    let abs_x = x - 2;
    let abs_y = y - 2;
    let abs_z = z - 2;
    let parent_x = abs_x >> 2;
    let parent_y = abs_y >> 2;
    let parent_z = abs_z >> 2;
    // Every candidate corner holding the same biome makes the fiddle moot.
    let first = grid.get(parent_x, parent_y, parent_z);
    let uniform = (1..8).all(|i| {
        grid.get(parent_x + ((i >> 2) & 1), parent_y + ((i >> 1) & 1), parent_z + (i & 1)) == first
    });
    if uniform {
        return first;
    }
    let fract_x = (abs_x & 3) as f64 / 4.0;
    let fract_y = (abs_y & 3) as f64 / 4.0;
    let fract_z = (abs_z & 3) as f64 / 4.0;
    let mut min_i = 0;
    let mut min_distance = f64::INFINITY;
    for i in 0..8 {
        let x_even = i & 4 == 0;
        let y_even = i & 2 == 0;
        let z_even = i & 1 == 0;
        let corner_x = if x_even { parent_x } else { parent_x + 1 };
        let corner_y = if y_even { parent_y } else { parent_y + 1 };
        let corner_z = if z_even { parent_z } else { parent_z + 1 };
        let dx = if x_even { fract_x } else { fract_x - 1.0 };
        let dy = if y_even { fract_y } else { fract_y - 1.0 };
        let dz = if z_even { fract_z } else { fract_z - 1.0 };
        let [fx, fy, fz] = corner_fiddle(seed, corner_x, corner_y, corner_z);
        let next = (dz + fz) * (dz + fz) + (dy + fy) * (dy + fy) + (dx + fx) * (dx + fx);
        if min_distance > next {
            min_i = i;
            min_distance = next;
        }
    }
    let bx = if min_i & 4 == 0 { parent_x } else { parent_x + 1 };
    let by = if min_i & 2 == 0 { parent_y } else { parent_y + 1 };
    let bz = if min_i & 1 == 0 { parent_z } else { parent_z + 1 };
    grid.get(bx, by, bz)
}

/// The fiddle offsets of one quart corner (`BiomeManager.getBiome`'s
/// three `getFiddle` calls for that corner).
#[inline]
fn corner_fiddle(seed: i64, corner_x: i32, corner_y: i32, corner_z: i32) -> [f64; 3] {
    let mut rval = lcg_next(seed, corner_x as i64);
    rval = lcg_next(rval, corner_y as i64);
    rval = lcg_next(rval, corner_z as i64);
    rval = lcg_next(rval, corner_x as i64);
    rval = lcg_next(rval, corner_y as i64);
    rval = lcg_next(rval, corner_z as i64);
    let fx = fiddle(rval);
    rval = lcg_next(rval, seed);
    let fy = fiddle(rval);
    rval = lcg_next(rval, seed);
    let fz = fiddle(rval);
    [fx, fy, fz]
}

/// The fiddle offsets of every quart corner a chunk's lookups touch,
/// computed once each: a non-uniform lookup then costs eight distance
/// checks instead of eight LCG chains. Corners outside the chunk's
/// range are computed directly.
pub struct FiddleCache {
    min_x: i32,
    min_y: i32,
    min_z: i32,
    size_y: usize,
    data: Vec<[f64; 3]>,
    /// Per parent cell: the biome when all eight corners agree, MIXED, or UNKNOWN.
    uniform: Vec<u16>,
}

impl FiddleCache {
    const SIZE_XZ: usize = QuartGrid::SIZE_XZ + 1;
    const UNKNOWN: u16 = u16::MAX;
    const MIXED: u16 = u16::MAX - 1;

    pub fn new(grid: &QuartGrid) -> Self {
        // Block y - 2 >> 2 runs one quart below the grid; the +1 corner runs one above.
        let size_y = grid.size_y + 2;
        let len = Self::SIZE_XZ * Self::SIZE_XZ * size_y;
        Self { min_x: grid.min_x, min_y: grid.min_y - 1, min_z: grid.min_z, size_y, data: vec![[f64::NAN; 3]; len], uniform: vec![Self::UNKNOWN; len] }
    }

    #[inline]
    fn slot(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        let (x, y, z) = (x - self.min_x, y - self.min_y, z - self.min_z);
        let xz = Self::SIZE_XZ as i32;
        if x < 0 || z < 0 || y < 0 || x >= xz || z >= xz || y >= self.size_y as i32 {
            return None;
        }
        Some(y as usize + (x as usize + z as usize * Self::SIZE_XZ) * self.size_y)
    }

    /// The biome when the eight corners around a parent cell agree.
    #[inline]
    fn uniform(&mut self, grid: &QuartGrid, parent_x: i32, parent_y: i32, parent_z: i32) -> Option<u16> {
        let check = |grid: &QuartGrid| {
            let first = grid.get(parent_x, parent_y, parent_z);
            let same = (1..8).all(|i| grid.get(parent_x + ((i >> 2) & 1), parent_y + ((i >> 1) & 1), parent_z + (i & 1)) == first);
            if same { first } else { Self::MIXED }
        };
        let Some(i) = self.slot(parent_x, parent_y, parent_z) else {
            let v = check(grid);
            return (v != Self::MIXED).then_some(v);
        };
        if self.uniform[i] == Self::UNKNOWN {
            self.uniform[i] = check(grid);
        }
        let v = self.uniform[i];
        (v != Self::MIXED).then_some(v)
    }

    #[inline]
    fn corner(&mut self, seed: i64, corner_x: i32, corner_y: i32, corner_z: i32) -> [f64; 3] {
        let Some(i) = self.slot(corner_x, corner_y, corner_z) else {
            return corner_fiddle(seed, corner_x, corner_y, corner_z);
        };
        if self.data[i][0].is_nan() {
            self.data[i] = corner_fiddle(seed, corner_x, corner_y, corner_z);
        }
        self.data[i]
    }

    /// `zoomed_biome` through the cache; same result.
    pub fn zoomed_biome(&mut self, seed: i64, grid: &QuartGrid, x: i32, y: i32, z: i32) -> u16 {
        let abs_x = x - 2;
        let abs_y = y - 2;
        let abs_z = z - 2;
        let parent_x = abs_x >> 2;
        let parent_y = abs_y >> 2;
        let parent_z = abs_z >> 2;
        if let Some(b) = self.uniform(grid, parent_x, parent_y, parent_z) {
            return b;
        }
        let fract_x = (abs_x & 3) as f64 / 4.0;
        let fract_y = (abs_y & 3) as f64 / 4.0;
        let fract_z = (abs_z & 3) as f64 / 4.0;
        let mut min_i = 0;
        let mut min_distance = f64::INFINITY;
        for i in 0..8 {
            let x_even = i & 4 == 0;
            let y_even = i & 2 == 0;
            let z_even = i & 1 == 0;
            let corner_x = if x_even { parent_x } else { parent_x + 1 };
            let corner_y = if y_even { parent_y } else { parent_y + 1 };
            let corner_z = if z_even { parent_z } else { parent_z + 1 };
            let dx = if x_even { fract_x } else { fract_x - 1.0 };
            let dy = if y_even { fract_y } else { fract_y - 1.0 };
            let dz = if z_even { fract_z } else { fract_z - 1.0 };
            let [fx, fy, fz] = self.corner(seed, corner_x, corner_y, corner_z);
            let next = (dz + fz) * (dz + fz) + (dy + fy) * (dy + fy) + (dx + fx) * (dx + fx);
            if min_distance > next {
                min_i = i;
                min_distance = next;
            }
        }
        let bx = if min_i & 4 == 0 { parent_x } else { parent_x + 1 };
        let by = if min_i & 2 == 0 { parent_y } else { parent_y + 1 };
        let bz = if min_i & 1 == 0 { parent_z } else { parent_z + 1 };
        grid.get(bx, by, bz)
    }
}

/// A chunk's block column as the surface pass sees it: palette ids,
/// the post-processing set and the two worldgen heightmaps, kept in
/// step exactly as `ProtoChunk.setBlockState` keeps them.
pub struct ChunkBlocks {
    pub vol: Volume,
    pub blocks: Vec<u16>,
    pub post_process: Vec<bool>,
    /// `WORLD_SURFACE_WG` first-available heights, `x + z * 16`.
    pub surface_height: [i32; 256],
    /// `OCEAN_FLOOR_WG` first-available heights.
    pub floor_height: [i32; 256],
}

impl ChunkBlocks {
    /// From the fill's substance codes (0 default, 1 air, 2 water,
    /// 3 lava, bit 0x10 fluid update).
    pub fn from_fill(cfg: &SurfaceConfig, chunk_x: i32, chunk_z: i32, substance: &[u8]) -> Self {
        let vol = Volume::chunk(chunk_x, chunk_z, cfg.min_y, cfg.height);
        assert_eq!(substance.len(), vol.len());
        let mut blocks = Vec::with_capacity(substance.len());
        let mut post_process = Vec::with_capacity(substance.len());
        for &s in substance {
            blocks.push(match s & 0x0f {
                0 => cfg.default_block,
                1 => cfg.air,
                2 => cfg.water,
                3 => cfg.lava,
                other => panic!("fill substance {other}"),
            });
            post_process.push(s & crate::fill26::FLUID_UPDATE_BIT != 0);
        }
        let mut out = Self { vol, blocks, post_process, surface_height: [0; 256], floor_height: [0; 256] };
        out.prime_heightmaps(&cfg.palette);
        out
    }

    fn prime_heightmaps(&mut self, palette: &Palette) {
        let height = self.vol.size[1];
        for z in 0..16 {
            for x in 0..16 {
                let mut surface = self.vol.min[1];
                let mut floor = self.vol.min[1];
                for y in (0..height).rev() {
                    let id = self.blocks[self.vol.index(x, y, z)];
                    let block_y = self.vol.min[1] + y as i32;
                    if surface == self.vol.min[1] && !palette.is_air(id) {
                        surface = block_y + 1;
                    }
                    if floor == self.vol.min[1] && palette.blocks_motion(id) {
                        floor = block_y + 1;
                    }
                    if surface != self.vol.min[1] && floor != self.vol.min[1] {
                        break;
                    }
                }
                self.surface_height[x + z * 16] = surface;
                self.floor_height[x + z * 16] = floor;
            }
        }
    }

    #[inline]
    fn in_range(&self, y: i32) -> bool {
        y >= self.vol.min[1] && y < self.vol.min[1] + self.vol.size[1] as i32
    }

    /// `protoChunk.getBlockState`: void air outside the build height.
    #[inline]
    pub fn get(&self, cfg: &SurfaceConfig, x: usize, y: i32, z: usize) -> u16 {
        if self.in_range(y) {
            self.blocks[self.vol.index(x, (y - self.vol.min[1]) as usize, z)]
        } else {
            cfg.air
        }
    }

    /// `BlockColumn.setBlock` through `ProtoChunk.setBlockState`, with
    /// the fluid post-processing mark the surface column adds.
    pub(crate) fn set(&mut self, palette: &Palette, x: usize, y: i32, z: usize, id: u16) {
        if !self.in_range(y) {
            return;
        }
        if palette.is_fluid(id) {
            self.mark_post(x, y, z);
        }
        self.set_without_post(palette, x, y, z, id);
    }

    /// `ProtoChunk.markPosForPostProcessing`.
    pub(crate) fn mark_post(&mut self, x: usize, y: i32, z: usize) {
        if self.in_range(y) {
            let i = self.vol.index(x, (y - self.vol.min[1]) as usize, z);
            self.post_process[i] = true;
        }
    }

    /// `ProtoChunk.setBlockState` alone: the block and both heightmaps.
    pub(crate) fn set_without_post(&mut self, palette: &Palette, x: usize, y: i32, z: usize, id: u16) {
        if !self.in_range(y) {
            return;
        }
        let i = self.vol.index(x, (y - self.vol.min[1]) as usize, z);
        self.blocks[i] = id;
        let air = palette.is_air(id);
        let motion = palette.blocks_motion(id);
        let min_y = self.vol.min[1];
        let col = x + z * 16;
        let surface = Self::update_height(self.surface_height[col], y, !air, min_y, |yy| {
            !palette.is_air(self.blocks[self.vol.index(x, (yy - min_y) as usize, z)])
        });
        self.surface_height[col] = surface;
        let floor = Self::update_height(self.floor_height[col], y, motion, min_y, |yy| {
            palette.blocks_motion(self.blocks[self.vol.index(x, (yy - min_y) as usize, z)])
        });
        self.floor_height[col] = floor;
    }

    /// `Heightmap.update` for one map: returns the new first-available height.
    fn update_height(first: i32, y: i32, opaque: bool, min_y: i32, opaque_at: impl Fn(i32) -> bool) -> i32 {
        if y <= first - 2 {
            return first;
        }
        if opaque {
            if y >= first {
                return y + 1;
            }
        } else if first - 1 == y {
            let mut yy = y - 1;
            while yy >= min_y {
                if opaque_at(yy) {
                    return yy + 1;
                }
                yy -= 1;
            }
            return min_y;
        }
        first
    }

    /// `ChunkAccess.getHighestFilledSectionIndex` as a block y: the top
    /// of the highest section holding any non-air block.
    fn highest_filled_block_y(&self, palette: &Palette) -> i32 {
        let min_y = self.vol.min[1];
        let sections = self.vol.size[1] / 16;
        for s in (0..sections).rev() {
            let y0 = s * 16;
            for z in 0..16 {
                for x in 0..16 {
                    for y in y0..y0 + 16 {
                        if !palette.is_air(self.blocks[self.vol.index(x, y, z)]) {
                            return min_y + (s as i32) * 16 + 15;
                        }
                    }
                }
            }
        }
        min_y - 1
    }
}

/// `MaterialRuleContext` plus the per-chunk prefilled buffers.
pub(crate) struct Ctx<'a> {
    pub(crate) cfg: &'a SurfaceConfig,
    pub(crate) chunk: &'a mut ChunkBlocks,
    quarts: &'a QuartGrid,
    biome_zoom_seed: i64,
    fiddles: FiddleCache,
    /// Distinct biomes in the quart grid; the zoomed biome is always one of them.
    grid_biomes: Vec<u16>,
    narrowed: Volume,
    preliminary: Vec<f32>,
    ore_density: Vec<Vec<f32>>,
    /// Richness per slot, sampled only over `ore_richness_rows` (narrowed row range) since it is read where density is positive.
    ore_richness: Vec<Vec<f32>>,
    ore_richness_rows: Vec<(usize, usize)>,
    /// Block rows `[lo, hi)` per slot holding a positive density anywhere in the chunk; the vein rule answers `None` outside them.
    ore_positive_rows: Vec<(i32, i32)>,
    point_cache: PointCache,
    // per column
    block_x: i32,
    block_z: i32,
    gradient_x: i32,
    gradient_z: i32,
    surface_depth: i32,
    surface_secondary: Option<f64>,
    min_surface_level: Option<i32>,
    noise_2d: Vec<Option<f64>>,
    // per block
    block_y: i32,
    water_height: i32,
    stone_depth_below: i32,
    stone_depth_above: i32,
    biome: Option<u16>,
    /// Index of the block in the narrowed prefill volume, if inside it.
    narrowed_index: Option<usize>,
    /// Bumped per block; a 3d noise slot is valid when its stamp matches.
    block_stamp: u32,
    noise_3d: Vec<(u32, f64)>,
    count_tests: bool,
    rule_calls: u64,
    condition_tests: [u64; 12],
    biome_lookups: u64,
    noise_2d_samples: u64,
    noise_3d_samples: u64,
}

impl Ctx<'_> {
    fn update_xz(&mut self, x: i32, z: i32, gradient_x: i32, gradient_z: i32) {
        self.block_x = x;
        self.block_z = z;
        self.gradient_x = gradient_x;
        self.gradient_z = gradient_z;
        self.surface_depth = self.cfg.surface_depth(x, z);
        self.surface_secondary = None;
        self.min_surface_level = None;
        self.noise_2d.iter_mut().for_each(|v| *v = None);
        self.block_stamp += 1;
        self.biome = None;
    }

    fn update_y(&mut self, stone_depth_above: i32, stone_depth_below: i32, water_height: i32, y: i32) {
        self.block_y = y;
        self.water_height = water_height;
        self.stone_depth_below = stone_depth_below;
        self.stone_depth_above = stone_depth_above;
        self.biome = None;
        self.block_stamp += 1;
        self.narrowed_index = self.narrowed.index_of_block(self.block_x, y, self.block_z);
    }

    /// `MaterialSystem.topMaterial`: the rule at one block with the
    /// gradients read from the heightmaps as they stand now.
    pub(crate) fn top_material(&mut self, x: usize, y: i32, z: usize, under_fluid: bool) -> Option<u16> {
        let block_x = self.chunk.vol.min[0] + x as i32;
        let block_z = self.chunk.vol.min[2] + z as i32;
        let heights = &self.chunk.surface_height;
        let gradient_x = heights[x.min(14) + 1 + z * 16] - heights[x.max(1) - 1 + z * 16];
        let gradient_z = heights[x + (z.min(14) + 1) * 16] - heights[x + (z.max(1) - 1) * 16];
        self.update_xz(block_x, block_z, gradient_x, gradient_z);
        self.update_y(1, 1, if under_fluid { y + 1 } else { i32::MIN }, y);
        self.rule_calls += 1;
        let cfg: &SurfaceConfig = self.cfg;
        self.apply(&cfg.rule)
    }

    fn surface_secondary(&mut self) -> f64 {
        if let Some(v) = self.surface_secondary {
            return v;
        }
        let v = self.cfg.surface_secondary_noise.get(self.block_x as f64, 0.0, self.block_z as f64) as f64;
        self.surface_secondary = Some(v);
        v
    }

    fn biome(&mut self) -> u16 {
        if let Some(b) = self.biome {
            return b;
        }
        let b = self.fiddles.zoomed_biome(self.biome_zoom_seed, self.quarts, self.block_x, self.block_y, self.block_z);
        self.biome_lookups += 1;
        self.biome = Some(b);
        b
    }

    fn min_surface_level(&mut self) -> i32 {
        if let Some(v) = self.min_surface_level {
            return v;
        }
        let x = (self.block_x - self.chunk.vol.min[0]) as usize;
        let z = (self.block_z - self.chunk.vol.min[2]) as usize;
        let preliminary = self.preliminary[x + z * 16];
        let v = (preliminary as f64).floor() as i32 + self.surface_depth - 8;
        self.min_surface_level = Some(v);
        v
    }

    fn noise(&mut self, noise: &NoiseStack, slot: usize, is_3d: bool) -> f64 {
        if is_3d {
            let (stamp, cached) = self.noise_3d[slot];
            if stamp == self.block_stamp {
                return cached;
            }
            let v = noise.get(self.block_x as f64, self.block_y as f64, self.block_z as f64) as f64;
            self.noise_3d_samples += 1;
            self.noise_3d[slot] = (self.block_stamp, v);
            v
        } else {
            if let Some(v) = self.noise_2d[slot] {
                return v;
            }
            let v = noise.get(self.block_x as f64, 0.0, self.block_z as f64) as f64;
            self.noise_2d_samples += 1;
            self.noise_2d[slot] = Some(v);
            v
        }
    }

    /// `MaterialRules.DensityGetter` with a prefilled buffer: `ore_density` or `ore_richness`.
    fn prefilled(&mut self, richness: bool, slot: usize, function: &Node) -> f32 {
        if let Some(i) = self.narrowed_index {
            if !richness {
                return self.ore_density[slot][i];
            }
            let size_y = self.narrowed.size[1];
            let (y, col) = (i % size_y, i / size_y);
            let (lo, hi) = self.ore_richness_rows[slot];
            if y >= lo && y < hi {
                return self.ore_richness[slot][(y - lo) + col * (hi - lo)];
            }
        }
        function.eval_with(self.block_x, self.block_y, self.block_z, Mode::Point, &mut self.point_cache)
    }

    // Temperature keeps vanilla's `!(t >= 0.15)` so a NaN reads as cold.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    fn test(&mut self, c: &Condition) -> bool {
        if self.count_tests {
            self.condition_tests[condition_variant(c)] += 1;
        }
        match c {
            Condition::AbovePreliminarySurface => self.block_y >= self.min_surface_level(),
            Condition::Biome(set) => {
                // A set holding none of the chunk's biomes is false everywhere, all of them true everywhere.
                let (mut any, mut all) = (false, true);
                for &b in &self.grid_biomes {
                    if set[b as usize] {
                        any = true;
                    } else {
                        all = false;
                    }
                }
                if !any {
                    false
                } else if all {
                    true
                } else {
                    set[self.biome() as usize]
                }
            }
            Condition::Hole => self.surface_depth <= 0,
            Condition::NoiseThreshold { noise, slot, min, max, is_3d } => {
                let v = self.noise(noise, *slot, *is_3d);
                v >= *min && v <= *max
            }
            Condition::Not(inner) => !self.test(inner),
            Condition::Steep => self.gradient_x <= -4 || self.gradient_z >= 4,
            Condition::StoneDepth { offset, add_surface_depth, secondary_depth_range, ceiling } => {
                let stone_depth = if *ceiling { self.stone_depth_below } else { self.stone_depth_above };
                let surface_depth = if *add_surface_depth { self.surface_depth } else { 0 };
                let secondary = if *secondary_depth_range == 0 {
                    0
                } else {
                    let s = self.surface_secondary();
                    map(s, -1.0, 1.0, 0.0, *secondary_depth_range as f64) as i32
                };
                stone_depth <= 1 + offset + surface_depth + secondary
            }
            Condition::Temperature => {
                let biome = self.biome() as usize;
                // coldEnoughToSnow = !warmEnoughToRain = temperature < 0.15
                !(self.cfg.temperature(biome, self.block_x, self.block_y, self.block_z) >= 0.15)
            }
            Condition::VerticalGradient { random, true_at_and_below, false_at_and_above } => {
                let y = self.block_y;
                if y <= *true_at_and_below {
                    true
                } else if y >= *false_at_and_above {
                    false
                } else {
                    let probability = map(y as f64, *true_at_and_below as f64, *false_at_and_above as f64, 1.0, 0.0);
                    (random.at(self.block_x, y, self.block_z).next_float() as f64) < probability
                }
            }
            Condition::Water { offset, surface_depth_multiplier, add_stone_depth } => {
                self.water_height == i32::MIN
                    || self.block_y + if *add_stone_depth { self.stone_depth_above } else { 0 }
                        >= self.water_height + offset + self.surface_depth * surface_depth_multiplier
            }
            Condition::YAbove { anchor, surface_depth_multiplier, add_stone_depth } => {
                self.block_y + if *add_stone_depth { self.stone_depth_above } else { 0 } >= anchor + self.surface_depth * surface_depth_multiplier
            }
        }
    }

    fn apply(&mut self, r: &Rule) -> Option<u16> {
        match r {
            Rule::Bandlands => Some(self.cfg.band(self.block_x, self.block_y, self.block_z)),
            Rule::Block(id) => Some(*id),
            Rule::Condition(c, then) => {
                if self.test(c) {
                    self.apply(then)
                } else {
                    None
                }
            }
            Rule::OreVein { ore, raw_ore, filler, raw_ore_chance, slot, density, richness, filler_gap } => {
                let d = self.prefilled(false, *slot, density);
                if d <= 0.0 {
                    return None;
                }
                let mut random = self.cfg.ore_random.at(self.block_x, self.block_y, self.block_z);
                if random.next_float() > d {
                    return None;
                }
                let r = self.prefilled(true, *slot, richness);
                if random.next_float() < r
                    && filler_gap.eval_with(self.block_x, self.block_y, self.block_z, Mode::Point, &mut self.point_cache) < 0.0
                {
                    Some(if random.next_float() < *raw_ore_chance { *raw_ore } else { *ore })
                } else {
                    Some(*filler)
                }
            }
            Rule::Sequence(rules) => {
                for r in rules {
                    if let Some(id) = self.apply(r) {
                        return Some(id);
                    }
                }
                None
            }
        }
    }
}

/// The rule tree folded for one y band of one chunk: sequences are
/// flattened, conditions decided by the band and the chunk's biome set
/// are gone, and what is left is a jump list. `Cond` skips to its end
/// index when the test fails.
enum Op<'a> {
    Cond(&'a Condition, usize),
    Rule(&'a Rule),
}

/// Rows from `y_lo` up to the next band share both folds: `deep`
/// assumes the block is below the column's preliminary surface level,
/// `full` does not.
struct Band<'a> {
    y_lo: i32,
    deep: Vec<Op<'a>>,
    full: Vec<Op<'a>>,
}

/// Row starts where some vertical gradient changes from decided to
/// random or from random to decided.
fn gradient_breaks(rule: &Rule, out: &mut Vec<i32>) {
    fn from_condition(c: &Condition, out: &mut Vec<i32>) {
        match c {
            Condition::VerticalGradient { true_at_and_below, false_at_and_above, .. } => {
                out.push(*true_at_and_below + 1);
                out.push(*false_at_and_above);
            }
            Condition::Not(inner) => from_condition(inner, out),
            _ => {}
        }
    }
    match rule {
        Rule::Condition(c, then) => {
            from_condition(c, out);
            gradient_breaks(then, out);
        }
        Rule::Sequence(rules) => rules.iter().for_each(|r| gradient_breaks(r, out)),
        Rule::Bandlands | Rule::Block(_) | Rule::OreVein { .. } => {}
    }
}

/// What a condition answers on every block of the band, if that is
/// already known: a vertical gradient outside its random rows, a biome
/// set holding none or all of the chunk's biomes, the preliminary
/// surface test on deep rows. Same answers `Ctx::test` gives.
fn decide(c: &Condition, y_lo: i32, y_hi: i32, deep: bool, grid_biomes: &[u16]) -> Option<bool> {
    match c {
        Condition::AbovePreliminarySurface => deep.then_some(false),
        Condition::Biome(set) => {
            let (mut any, mut all) = (false, true);
            for &b in grid_biomes {
                if set[b as usize] {
                    any = true;
                } else {
                    all = false;
                }
            }
            if !any {
                Some(false)
            } else if all {
                Some(true)
            } else {
                None
            }
        }
        Condition::Not(inner) => decide(inner, y_lo, y_hi, deep, grid_biomes).map(|b| !b),
        Condition::VerticalGradient { true_at_and_below, false_at_and_above, .. } => {
            if y_hi <= *true_at_and_below {
                Some(true)
            } else if y_lo >= *false_at_and_above {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Appends the fold of `rule` to `ops`. Returns true when what was
/// emitted always answers, so nothing after it in a sequence can run.
fn fold<'a>(rule: &'a Rule, y_lo: i32, y_hi: i32, deep: bool, grid_biomes: &[u16], ore_rows: &[(i32, i32)], ops: &mut Vec<Op<'a>>) -> bool {
    match rule {
        Rule::Condition(c, then) => match decide(c, y_lo, y_hi, deep, grid_biomes) {
            Some(true) => fold(then, y_lo, y_hi, deep, grid_biomes, ore_rows, ops),
            Some(false) => false,
            None => {
                let at = ops.len();
                ops.push(Op::Cond(c, 0));
                fold(then, y_lo, y_hi, deep, grid_biomes, ore_rows, ops);
                if ops.len() == at + 1 {
                    ops.pop();
                } else {
                    ops[at] = Op::Cond(c, ops.len());
                }
                false
            }
        },
        Rule::Sequence(rules) => rules.iter().any(|r| fold(r, y_lo, y_hi, deep, grid_biomes, ore_rows, ops)),
        Rule::Bandlands | Rule::Block(_) => {
            ops.push(Op::Rule(rule));
            true
        }
        Rule::OreVein { slot, .. } => {
            // The prefilled density is never positive outside the slot's rows, so the vein answers None there.
            let (lo, hi) = ore_rows[*slot];
            if lo < hi && y_lo < hi && y_hi >= lo {
                ops.push(Op::Rule(rule));
            }
            false
        }
    }
}

/// One band per run of rows on which every vertical gradient in the
/// tree keeps its verdict, each with its deep and full fold.
fn fold_bands<'a>(rule: &'a Rule, min_y: i32, max_y: i32, grid_biomes: &[u16], ore_rows: &[(i32, i32)]) -> Vec<Band<'a>> {
    let mut starts = Vec::new();
    gradient_breaks(rule, &mut starts);
    starts.retain(|&y| y > min_y && y <= max_y);
    starts.push(min_y);
    starts.sort_unstable();
    starts.dedup();
    let mut bands = Vec::with_capacity(starts.len());
    for (i, &y_lo) in starts.iter().enumerate() {
        let y_hi = starts.get(i + 1).map_or(max_y, |&next| next - 1);
        let mut deep = Vec::new();
        fold(rule, y_lo, y_hi, true, grid_biomes, ore_rows, &mut deep);
        let mut full = Vec::new();
        fold(rule, y_lo, y_hi, false, grid_biomes, ore_rows, &mut full);
        bands.push(Band { y_lo, deep, full });
    }
    bands
}

impl Ctx<'_> {
    /// `apply` over a folded jump list.
    fn apply_ops(&mut self, ops: &[Op], mut i: usize, end: usize) -> Option<u16> {
        while i < end {
            match ops[i] {
                Op::Rule(r) => {
                    if let Some(id) = self.apply(r) {
                        return Some(id);
                    }
                    i += 1;
                }
                Op::Cond(c, skip) => {
                    if self.test(c) {
                        if let Some(id) = self.apply_ops(ops, i + 1, skip) {
                            return Some(id);
                        }
                    }
                    i = skip;
                }
            }
        }
        None
    }
}

/// `Mth.map`.
#[inline]
fn map(value: f64, from_min: f64, from_max: f64, to_min: f64, to_max: f64) -> f64 {
    let t = (value - from_min) / (from_max - from_min);
    to_min + t * (to_max - to_min)
}

/// Where the surface pass spends its time, for the profile example.
#[derive(Default, Debug, Clone, Copy)]
pub struct SurfaceStats {
    pub prefill_ms: f64,
    pub preliminary_ms: f64,
    pub columns_ms: f64,
    /// Per-column work before the row walk: surface biome, gradients, surface depth, preliminary level.
    pub column_setup_ms: f64,
    pub rule_calls: u64,
    /// Rule calls below the column's preliminary surface level, and their time.
    pub deep_calls: u64,
    pub deep_ms: f64,
    /// Tests per `Condition` variant, `condition_variant` order.
    pub condition_tests: [u64; 12],
    pub biome_lookups: u64,
    pub noise_2d: u64,
    pub noise_3d: u64,
    /// The carve pass, when one ran.
    pub carve: CarveStats,
}

/// Index of a condition variant for the stats counters: above
/// preliminary, biome, hole, noise, not, steep, stone depth,
/// temperature, vertical gradient, water, y above.
pub fn condition_variant(c: &Condition) -> usize {
    match c {
        Condition::AbovePreliminarySurface => 0,
        Condition::Biome(_) => 1,
        Condition::Hole => 2,
        Condition::NoiseThreshold { .. } => 3,
        Condition::Not(_) => 4,
        Condition::Steep => 5,
        Condition::StoneDepth { .. } => 6,
        Condition::Temperature => 7,
        Condition::VerticalGradient { .. } => 8,
        Condition::Water { .. } => 9,
        Condition::YAbove { .. } => 10,
    }
}

/// `MaterialSystem.buildSurface` on a filled chunk.
pub fn build_surface(cfg: &SurfaceConfig, chunk: &mut ChunkBlocks, quarts: &QuartGrid, biome_zoom_seed: i64) {
    build_surface_inner(cfg, chunk, quarts, biome_zoom_seed, false, None, true);
}

/// `build_surface` with the per-condition test counters on.
pub fn build_surface_stats(cfg: &SurfaceConfig, chunk: &mut ChunkBlocks, quarts: &QuartGrid, biome_zoom_seed: i64) -> SurfaceStats {
    build_surface_inner(cfg, chunk, quarts, biome_zoom_seed, true, None, true)
}

/// The carvers applied right after the surface pass, on the same
/// context (`generateCarvers` follows `buildSurface` in TERRAIN).
pub struct CarveJob<'a> {
    pub stage: &'a CarverStage,
    pub mask: &'a CarvingMask,
    pub aquifer: &'a mut Aquifer,
}

/// `build_surface` followed by `applyCarvingMask` when a job is given.
pub fn build_surface_carved(cfg: &SurfaceConfig, chunk: &mut ChunkBlocks, quarts: &QuartGrid, biome_zoom_seed: i64, carve: Option<CarveJob>) -> SurfaceStats {
    build_surface_inner(cfg, chunk, quarts, biome_zoom_seed, false, carve, true)
}

/// The carve pass alone on a chunk that already has its surface (a
/// vanilla dump in tests), with the same context the fused pass uses.
pub fn carve_only(cfg: &SurfaceConfig, chunk: &mut ChunkBlocks, quarts: &QuartGrid, biome_zoom_seed: i64, carve: CarveJob) -> SurfaceStats {
    build_surface_inner(cfg, chunk, quarts, biome_zoom_seed, false, Some(carve), false)
}

fn build_surface_inner(
    cfg: &SurfaceConfig,
    chunk: &mut ChunkBlocks,
    quarts: &QuartGrid,
    biome_zoom_seed: i64,
    count_tests: bool,
    carve: Option<CarveJob>,
    run_surface: bool,
) -> SurfaceStats {
    let mut stats = SurfaceStats::default();
    let t0 = std::time::Instant::now();
    let palette = &cfg.palette;
    let min_block_x = chunk.vol.min[0];
    let min_block_z = chunk.vol.min[2];
    let min_y = chunk.vol.min[1];
    let max_y = min_y + chunk.vol.size[1] as i32 - 1;
    let max_block_y = chunk.highest_filled_block_y(palette);
    let narrowed = Volume::new([16, (max_block_y - min_y + 1).max(1) as usize, 16], [min_block_x, min_y, min_block_z], [1, 1, 1]);
    let preliminary_vol = Volume::new([16, 1, 16], [min_block_x, 0, min_block_z], [1, 1, 1]);
    let preliminary = cfg.chunk_surface_level.sample_volume(&preliminary_vol);
    stats.preliminary_ms = t0.elapsed().as_secs_f64() * 1e3;
    let t1 = std::time::Instant::now();
    let mut ores = OrePrefill {
        density: vec![Vec::new(); cfg.ore_slots],
        richness: vec![Vec::new(); cfg.ore_slots],
        richness_rows: vec![(0, 0); cfg.ore_slots],
        positive_rows: vec![(0, 0); cfg.ore_slots],
    };
    {
        let mut sample_ctx = SampleCtx::default();
        prefill_ores(&cfg.rule, &narrowed, &mut sample_ctx, &mut ores);
    }
    stats.prefill_ms = t1.elapsed().as_secs_f64() * 1e3;
    let t2 = std::time::Instant::now();
    let mut ctx = Ctx {
        cfg,
        chunk,
        quarts,
        biome_zoom_seed,
        fiddles: FiddleCache::new(quarts),
        grid_biomes: {
            let mut v: Vec<u16> = quarts.data.clone();
            v.sort_unstable();
            v.dedup();
            v
        },
        narrowed,
        preliminary,
        ore_density: ores.density,
        ore_richness: ores.richness,
        ore_richness_rows: ores.richness_rows,
        ore_positive_rows: ores.positive_rows,
        point_cache: PointCache::new(),
        block_x: 0,
        block_z: 0,
        gradient_x: 0,
        gradient_z: 0,
        surface_depth: 0,
        surface_secondary: None,
        min_surface_level: None,
        noise_2d: vec![None; cfg.noise_slots],
        block_y: 0,
        water_height: i32::MIN,
        stone_depth_below: 0,
        stone_depth_above: 0,
        biome: None,
        narrowed_index: None,
        block_stamp: 0,
        noise_3d: vec![(0, 0.0); cfg.noise_slots],
        count_tests,
        rule_calls: 0,
        condition_tests: [0; 12],
        biome_lookups: 0,
        noise_2d_samples: 0,
        noise_3d_samples: 0,
    };
    let bands = fold_bands(&cfg.rule, min_y, max_y, &ctx.grid_biomes, &ctx.ore_positive_rows);
    if std::env::var("PAINITE_SURFACE_TRACE").is_ok() {
        for b in &bands {
            let show = |ops: &[Op]| -> String {
                ops.iter()
                    .map(|op| match op {
                        Op::Cond(c, skip) => format!("C{}->{skip}", condition_variant(c)),
                        Op::Rule(Rule::Block(id)) => format!("B{id}"),
                        Op::Rule(Rule::OreVein { slot, .. }) => format!("O{slot}"),
                        Op::Rule(_) => "R".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            eprintln!("band y>={}: deep [{}] full [{}]", b.y_lo, show(&b.deep), show(&b.full));
        }
    }
    // A deep fold that is empty or one opaque block needs nothing from the column walk: those rows take the plain loop below.
    let deep_fast: Vec<Option<Option<u16>>> = bands
        .iter()
        .map(|b| match b.deep.as_slice() {
            [] => Some(None),
            [Op::Rule(Rule::Block(id))] if !palette.is_air(*id) && !palette.is_fluid(*id) && palette.blocks_motion(*id) => Some(Some(*id)),
            _ => None,
        })
        .collect();
    for x in 0..16usize {
        if !run_surface {
            break;
        }
        for z in 0..16usize {
            let ts = std::time::Instant::now();
            let block_x = min_block_x + x as i32;
            let block_z = min_block_z + z as i32;
            let starting_height = ctx.chunk.surface_height[x + z * 16] + 1;
            let surface_biome = ctx.fiddles.zoomed_biome(biome_zoom_seed, quarts, block_x, starting_height, block_z) as usize;
            if Some(surface_biome) == cfg.eroded_badlands {
                eroded_badlands_extension(cfg, ctx.chunk, x, z, starting_height);
            }
            let height = ctx.chunk.surface_height[x + z * 16] + 1;
            let gradient_x = ctx.chunk.surface_height[x.min(14) + 1 + z * 16] - ctx.chunk.surface_height[x.max(1) - 1 + z * 16];
            let gradient_z = ctx.chunk.surface_height[x + (z.min(14) + 1) * 16] - ctx.chunk.surface_height[x + (z.max(1) - 1) * 16];
            ctx.update_xz(block_x, block_z, gradient_x, gradient_z);
            let mut stone_above = 0;
            let mut water_height = i32::MIN;
            let mut next_ceiling_stone_y = i32::MAX;
            let end_y = min_y;
            let mut y = height;
            let min_surface_level = ctx.min_surface_level();
            stats.column_setup_ms += ts.elapsed().as_secs_f64() * 1e3;
            let mut deep_start: Option<std::time::Instant> = None;
            let mut band = bands.len() - 1;
            // Set after a fast run: stone_above and water_height are recomputed before the next rule row needs them.
            let mut stale = false;
            let col = x + z * 16;
            while y >= end_y {
                if deep_start.is_none() && y < min_surface_level {
                    deep_start = Some(std::time::Instant::now());
                }
                if y < min_surface_level && y <= max_y {
                    while bands[band].y_lo > y {
                        band -= 1;
                    }
                    if let Some(fast) = deep_fast[band] {
                        let lo = bands[band].y_lo.max(end_y);
                        let base = ctx.chunk.vol.index(x, 0, z);
                        let mut rows = 0;
                        for yy in (lo..=y).rev() {
                            let i = base + (yy - min_y) as usize;
                            let old = ctx.chunk.blocks[i];
                            if palette.is_air(old) || palette.is_fluid(old) {
                                continue;
                            }
                            rows += 1;
                            if let Some(id) = fast {
                                if yy + 1 >= ctx.chunk.surface_height[col].min(ctx.chunk.floor_height[col]) {
                                    ctx.chunk.set(palette, x, yy, z, id);
                                } else {
                                    ctx.chunk.blocks[i] = id;
                                }
                            }
                        }
                        ctx.rule_calls += rows;
                        stats.deep_calls += rows;
                        stale = true;
                        y = lo - 1;
                        continue;
                    }
                }
                if stale {
                    stale = false;
                    stone_above = 0;
                    water_height = i32::MIN;
                    next_ceiling_stone_y = i32::MAX;
                    let mut up = y + 1;
                    while up <= height {
                        let b = ctx.chunk.get(cfg, x, up, z);
                        if palette.is_air(b) {
                            break;
                        } else if palette.is_fluid(b) {
                            water_height = up + 1;
                        } else {
                            stone_above += 1;
                        }
                        up += 1;
                    }
                }
                let old = ctx.chunk.get(cfg, x, y, z);
                if palette.is_air(old) {
                    stone_above = 0;
                    water_height = i32::MIN;
                } else if palette.is_fluid(old) {
                    if water_height == i32::MIN {
                        water_height = y + 1;
                    }
                } else {
                    if next_ceiling_stone_y >= y {
                        next_ceiling_stone_y = WAY_BELOW_MIN_Y;
                        let mut look = y - 1;
                        while look >= end_y - 1 {
                            let next = ctx.chunk.get(cfg, x, look, z);
                            if palette.is_air(next) || palette.is_fluid(next) {
                                next_ceiling_stone_y = look + 1;
                                break;
                            }
                            look -= 1;
                        }
                    }
                    stone_above += 1;
                    let stone_below = y - next_ceiling_stone_y + 1;
                    ctx.update_y(stone_above, stone_below, water_height, y);
                    if y >= min_y && y <= max_y {
                        ctx.rule_calls += 1;
                        stats.deep_calls += deep_start.is_some() as u64;
                        while bands[band].y_lo > y {
                            band -= 1;
                        }
                        let ops = if y < min_surface_level { &bands[band].deep } else { &bands[band].full };
                        if let Some(id) = ctx.apply_ops(ops, 0, ops.len()) {
                            ctx.chunk.set(palette, x, y, z, id);
                        }
                    }
                }
                y -= 1;
            }
            if let Some(t) = deep_start {
                stats.deep_ms += t.elapsed().as_secs_f64() * 1e3;
            }
            if Some(surface_biome) == cfg.frozen_ocean || Some(surface_biome) == cfg.deep_frozen_ocean {
                let min_surface_level = ctx.min_surface_level();
                frozen_ocean_extension(cfg, ctx.chunk, min_surface_level, surface_biome, x, z, starting_height);
            }
        }
    }
    stats.columns_ms = t2.elapsed().as_secs_f64() * 1e3;
    if let Some(job) = carve {
        let t3 = std::time::Instant::now();
        stats.carve = job.stage.apply(&mut ctx, job.mask, job.aquifer);
        stats.carve.apply_ms = t3.elapsed().as_secs_f64() * 1e3;
    }
    stats.rule_calls = ctx.rule_calls;
    stats.condition_tests = ctx.condition_tests;
    stats.biome_lookups = ctx.biome_lookups;
    stats.noise_2d = ctx.noise_2d_samples;
    stats.noise_3d = ctx.noise_3d_samples;
    stats
}

/// Per-slot vein buffers over the narrowed volume.
struct OrePrefill {
    density: Vec<Vec<f32>>,
    richness: Vec<Vec<f32>>,
    /// Narrowed rows `[lo, hi)` the richness buffer covers.
    richness_rows: Vec<(usize, usize)>,
    /// Block rows `[lo, hi)` holding a positive density, `(0, 0)` when none.
    positive_rows: Vec<(i32, i32)>,
}

fn prefill_ores(rule: &Rule, narrowed: &Volume, ctx: &mut SampleCtx, out: &mut OrePrefill) {
    match rule {
        Rule::OreVein { slot, density: d, richness: r, .. } => {
            let (density, richness) = (&mut out.density, &mut out.richness);
            let t = std::time::Instant::now();
            let l0 = crate::noise26::LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);
            density[*slot] = d.sample_volume_with(narrowed, ctx);
            let l1 = crate::noise26::LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);
            let t1 = t.elapsed().as_secs_f64() * 1e3;
            // Richness is only read where density is positive: sample the rows that hold one, widened to whole 8-block cells.
            let size_y = narrowed.size[1];
            let (mut lo, mut hi) = (usize::MAX, 0);
            for column in density[*slot].chunks_exact(size_y) {
                for (y, &v) in column.iter().enumerate() {
                    if v > 0.0 {
                        lo = lo.min(y);
                        hi = hi.max(y + 1);
                    }
                }
            }
            out.positive_rows[*slot] = if lo < hi { (narrowed.min[1] + lo as i32, narrowed.min[1] + hi as i32) } else { (0, 0) };
            let rows = if lo < hi {
                let block_lo = java_floor_div(narrowed.min[1] + lo as i32, 8) * 8;
                let block_hi = java_floor_div(narrowed.min[1] + hi as i32 - 1, 8) * 8 + 8;
                let lo = (block_lo - narrowed.min[1]).max(0) as usize;
                let hi = ((block_hi - narrowed.min[1]) as usize).min(size_y);
                let sub = Volume::new([16, hi - lo, 16], [narrowed.min[0], narrowed.min[1] + lo as i32, narrowed.min[2]], [1, 1, 1]);
                richness[*slot] = r.sample_volume_with(&sub, ctx);
                (lo, hi)
            } else {
                richness[*slot] = Vec::new();
                (0, 0)
            };
            out.richness_rows[*slot] = rows;
            let l2 = crate::noise26::LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);
            if std::env::var("PAINITE_SURFACE_TRACE").is_ok() {
                let positive = density[*slot].iter().filter(|&&d| d > 0.0).count();
                eprintln!(
                    "ore slot {slot}: density {t1:.2} ms {} layer samples, richness {:.2} ms {} layer samples, volume {:?}, positive {positive}, richness rows {rows:?}",
                    l1 - l0,
                    t.elapsed().as_secs_f64() * 1e3 - t1,
                    l2 - l1,
                    narrowed.size,
                );
            }
        }
        Rule::Condition(_, then) => prefill_ores(then, narrowed, ctx, out),
        Rule::Sequence(rules) => rules.iter().for_each(|r| prefill_ores(r, narrowed, ctx, out)),
        Rule::Bandlands | Rule::Block(_) => {}
    }
}

/// `MaterialSystem.erodedBadlandsExtension`.
fn eroded_badlands_extension(cfg: &SurfaceConfig, chunk: &mut ChunkBlocks, x: usize, z: usize, height: i32) {
    let block_x = chunk.vol.min[0] + x as i32;
    let block_z = chunk.vol.min[2] + z as i32;
    let surface = (cfg.badlands_surface.get(block_x as f64, 0.0, block_z as f64) as f64 * 8.25).abs();
    let pillar = (cfg.badlands_pillar.get(block_x as f64 * 0.2, 0.0, block_z as f64 * 0.2) * 15.0f32) as f64;
    let pillar_buffer = surface.min(pillar);
    if pillar_buffer <= 0.0 {
        return;
    }
    let pillar_floor = (cfg.badlands_pillar_roof.get(block_x as f64 * 0.75, 0.0, block_z as f64 * 0.75) as f64 * 1.5).abs();
    let extension_top = 64.0 + (pillar_buffer * pillar_buffer * 2.5).min((pillar_floor * 50.0).ceil() + 24.0);
    let start_y = extension_top.floor() as i32;
    if height > start_y {
        return;
    }
    let min_y = chunk.vol.min[1];
    let mut y = start_y;
    while y >= min_y {
        let old = chunk.get(cfg, x, y, z);
        if old == cfg.default_block {
            break;
        }
        if old == cfg.water {
            return;
        }
        y -= 1;
    }
    let mut y = start_y;
    while y >= min_y && cfg.palette.is_air(chunk.get(cfg, x, y, z)) {
        chunk.set(&cfg.palette, x, y, z, cfg.default_block);
        y -= 1;
    }
}

/// `MaterialSystem.frozenOceanExtension`.
fn frozen_ocean_extension(cfg: &SurfaceConfig, chunk: &mut ChunkBlocks, min_surface_level: i32, biome: usize, x: usize, z: usize, height: i32) {
    let block_x = chunk.vol.min[0] + x as i32;
    let block_z = chunk.vol.min[2] + z as i32;
    let surface = (cfg.iceberg_surface.get(block_x as f64, 0.0, block_z as f64) as f64 * 8.25).abs();
    let pillar = (cfg.iceberg_pillar.get(block_x as f64 * 1.28, 0.0, block_z as f64 * 1.28) * 15.0f32) as f64;
    let iceberg = surface.min(pillar);
    if iceberg <= 1.8 {
        return;
    }
    let roof = (cfg.iceberg_pillar_roof.get(block_x as f64 * 1.17, 0.0, block_z as f64 * 1.17) as f64 * 1.5).abs();
    let mut top = (iceberg * iceberg * 1.2).min((roof * 40.0).ceil() + 14.0);
    if cfg.temperature(biome, block_x, cfg.sea_level, block_z) > 0.1 {
        top -= 2.0;
    }
    if top <= 2.0 {
        return;
    }
    let sea_level = cfg.sea_level;
    let extension_bottom = sea_level as f64 - top - 7.0;
    top += sea_level as f64;
    let extension_top = top;
    let mut random = cfg.noise_random.at(block_x, 0, block_z);
    let max_snow_depth = 2 + random.next_int_bounded(4);
    let min_snow_height = sea_level + 18 + random.next_int_bounded(10);
    let mut snow_depth = 0;
    let mut y = height.max(top as i32 + 1);
    while y >= min_surface_level {
        let old = chunk.get(cfg, x, y, z);
        let place = (cfg.palette.is_air(old) && y < extension_top as i32 && random.next_double() > 0.01)
            || (old == cfg.water && y > extension_bottom as i32 && y < sea_level && random.next_double() > 0.15);
        if place {
            if snow_depth <= max_snow_depth && y > min_snow_height {
                chunk.set(&cfg.palette, x, y, z, cfg.snow_block);
                snow_depth += 1;
            } else {
                chunk.set(&cfg.palette, x, y, z, cfg.packed_ice);
            }
        }
        y -= 1;
    }
}
