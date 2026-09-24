//! One world's compiled terrain state: the noise router, the aquifer
//! configuration and the recognised overworld fill shape, built once
//! from the datapack documents the game hands over and shared by every
//! worker thread.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::aquifer26::{Aquifer, AquiferConfig, Fluid, FluidPicker};
use crate::carvers26::{CarverBiomeCache, CarverStage, CarvingMask};
use crate::climate26::BiomeStage;
use crate::df26::{Loader, Node, Volume};
use crate::beard26::Beardifier;
use crate::fill26::{fill_fused, FillStats, OverworldShape};
use crate::lod26::{ColumnLod, LodStore};
use crate::ores26::{OreBatch, OreStage, Region, Section, HEIGHT_LONGS};
use crate::surface26::{build_surface, build_surface_carved, CarveJob, ChunkBlocks, QuartGrid, SurfaceConfig};
use crate::xoroshiro::XoroshiroPositionalRandomFactory;

/// Highest palette id the byte handoff can carry (bit 7 is the
/// post-processing flag).
pub const MAX_PALETTE: usize = 128;
pub const POST_PROCESS_FLAG: u8 = 0x80;

/// Why a world cannot use the fused fill.
#[derive(Debug)]
pub enum Unsupported {
    /// The final_density tree is not the vanilla overworld shape.
    Shape,
    /// The settings use the legacy random source or have no aquifers.
    Settings(String),
    /// A document failed to load or parse.
    Load(String),
}

/// A fill waiting for its surface pass, with the aquifer the fill
/// built so the carvers can ask it about the blocks they open.
struct PendingFill {
    substance: Vec<u8>,
    aquifer: Aquifer,
}

/// Running totals of the carve pass since the world loaded, for the
/// probe report.
#[derive(Default, Debug, Clone, Copy)]
pub struct CarveTotals {
    pub chunks: u64,
    pub mask_ns: u64,
    pub apply_ns: u64,
    pub carved: u64,
    pub aquifer_calls: u64,
    pub top_material_calls: u64,
}

pub static CARVE_TOTALS: Mutex<CarveTotals> = Mutex::new(CarveTotals {
    chunks: 0,
    mask_ns: 0,
    apply_ns: 0,
    carved: 0,
    aquifer_calls: 0,
    top_material_calls: 0,
});

pub struct TerrainState {
    final_density: Arc<Node>,
    aquifer: AquiferConfig,
    aquifer_random: XoroshiroPositionalRandomFactory,
    picker: FluidPicker,
    min_y: i32,
    height: i32,
    pub surface: SurfaceConfig,
    /// The biome stage, when the world supplied its biome parameters.
    pub biomes: Option<BiomeStage>,
    /// `BiomeManager.obfuscateSeed(seed)`, handed over by the game.
    pub biome_zoom_seed: i64,
    /// The ore stage, when the world supplied its block index.
    pub ores: Option<OreStage>,
    /// The carver stage, when every carver the biomes name loaded.
    pub carvers: Option<CarverStage>,
    /// Why the carver stage is missing, for the log.
    pub carvers_declined: Option<String>,
    /// Carver biome per chunk (`ChunkAccess.carverBiome`) for source
    /// chunks the biome cache does not hold.
    carver_biomes: Mutex<CarverBiomeCache>,
    /// Fills waiting for their surface pass, by chunk position.
    pending: Mutex<HashMap<(i32, i32), PendingFill>>,
    /// Ore batches waiting for their sections, by chunk position.
    ore_pending: Mutex<HashMap<(i32, i32), OreBatch>>,
    /// Far-view column records of every chunk the native surfaced
    /// while far view is on.
    pub lod: LodStore,
    /// Set by `keep_lod` when the game turns far view on; until then the
    /// surface pass builds no records.
    keep_lod: AtomicBool,
    /// Biome output kept for the surface and ore passes of the chunk and
    /// its eight neighbours.
    biome_cache: Mutex<BiomeCache>,
}

/// Chunks the biome stage runs ahead of the surface pass: the status
/// ladder keeps a ring of BIOMES-done, TERRAIN-pending chunks around
/// the loaded area, a few thousand at a 32 chunk view distance. Older
/// entries fall out first; a miss makes the game hand the grid over.
pub const BIOME_CACHE_CHUNKS: usize = 16384;

#[derive(Default)]
struct BiomeCache {
    map: HashMap<(i32, i32), Vec<u16>>,
    order: VecDeque<(i32, i32)>,
}

impl BiomeCache {
    fn insert(&mut self, key: (i32, i32), biomes: Vec<u16>) {
        if self.map.insert(key, biomes).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > BIOME_CACHE_CHUNKS {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

impl TerrainState {
    /// Build from documents keyed (kind, id) plus the settings id.
    pub fn build(seed: i64, biome_zoom_seed: i64, settings_id: &str, documents: Vec<(String, String, Value)>) -> Result<Self, Unsupported> {
        let mut loader = Loader::from_memory(seed, documents);
        Self::build_from_loader(&mut loader, biome_zoom_seed, settings_id)
    }

    /// Build from a loader that already has its documents (a datapack
    /// directory in tests).
    pub fn build_from_loader(loader: &mut Loader, biome_zoom_seed: i64, settings_id: &str) -> Result<Self, Unsupported> {
        let settings = loader.noise_settings(settings_id).map_err(Unsupported::Load)?;
        let sea_level = settings.get("sea_level").and_then(Value::as_i64).ok_or_else(|| Unsupported::Settings("sea_level".into()))? as i32;
        let sea_fluid = default_fluid(settings.get("default_fluid"))?;
        let noise = settings.get("noise").ok_or_else(|| Unsupported::Settings("noise".into()))?;
        let min_y = noise.get("min_y").and_then(Value::as_i64).ok_or_else(|| Unsupported::Settings("noise.min_y".into()))? as i32;
        let height = noise.get("height").and_then(Value::as_i64).ok_or_else(|| Unsupported::Settings("noise.height".into()))? as i32;
        if settings.get("aquifers").is_none() {
            return Err(Unsupported::Settings("aquifers".into()));
        }
        let router = loader.router(settings_id).map_err(Unsupported::Load)?;
        let final_density = router.get("final_density").cloned().ok_or_else(|| Unsupported::Load("final_density".into()))?;
        if OverworldShape::recognise(&final_density).is_none() {
            return Err(Unsupported::Shape);
        }
        let aquifer = AquiferConfig::load(loader, &settings).map_err(Unsupported::Load)?;
        let aquifer_random = loader.root_factory().from_hash_of("minecraft:aquifer").fork_positional();
        let biome_ids = loader.ids_of("biome").map_err(Unsupported::Load)?;
        let mut surface = SurfaceConfig::load(loader, &settings, &biome_ids).map_err(Unsupported::Load)?;
        let ores = if loader.document("block_index", "minecraft:overworld").is_ok() {
            Some(OreStage::load(loader, &mut surface.palette, &biome_ids, min_y, height, sea_level).map_err(Unsupported::Load)?)
        } else {
            None
        };
        if surface.palette.len() > MAX_PALETTE {
            return Err(Unsupported::Settings(format!("palette of {} states", surface.palette.len())));
        }
        let biomes = match BiomeStage::load(loader, &biome_ids, min_y, height) {
            Ok(b) => Some(b),
            Err(e) if e.contains("not provided") => None,
            Err(e) => return Err(Unsupported::Load(e)),
        };
        // A carver the port lacks leaves the carvers to the game, nothing else.
        let (carvers, carvers_declined) = if biomes.is_some() {
            match CarverStage::load(loader, &biome_ids, &surface.palette, loader.seed(), min_y, height, sea_level) {
                Ok(stage) => (Some(stage), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, Some("no biome stage".to_string()))
        };
        let lod = LodStore::new(lod_hash(&surface));
        Ok(Self {
            final_density,
            aquifer,
            aquifer_random,
            picker: FluidPicker { sea_level, sea_fluid },
            min_y,
            height,
            surface,
            biomes,
            biome_zoom_seed,
            ores,
            carvers,
            carvers_declined,
            carver_biomes: Mutex::new(CarverBiomeCache::default()),
            pending: Mutex::new(HashMap::new()),
            ore_pending: Mutex::new(HashMap::new()),
            lod,
            keep_lod: AtomicBool::new(false),
            biome_cache: Mutex::new(BiomeCache::default()),
        })
    }

    /// Start keeping far-view records of surfaced chunks.
    pub fn keep_lod(&self) {
        self.keep_lod.store(true, Ordering::Relaxed);
    }

    pub fn min_y(&self) -> i32 {
        self.min_y
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    /// Substance ids for one chunk column in `y + (x + z * 16) * height`
    /// order, with the fluid-update bit set where vanilla schedules one.
    pub fn fill(&self, chunk_x: i32, chunk_z: i32) -> (Vec<u8>, FillStats) {
        self.fill_with(chunk_x, chunk_z, None)
    }

    /// `fill` with the structure pieces that beard this chunk.
    pub fn fill_with(&self, chunk_x: i32, chunk_z: i32, beard: Option<&Beardifier>) -> (Vec<u8>, FillStats) {
        let (substance, stats, _) = self.fill_with_aquifer(chunk_x, chunk_z, beard);
        (substance, stats)
    }

    /// `fill_with` handing back the aquifer the fill used, for the carvers.
    pub fn fill_with_aquifer(&self, chunk_x: i32, chunk_z: i32, beard: Option<&Beardifier>) -> (Vec<u8>, FillStats, Aquifer) {
        let vol = Volume::chunk(chunk_x, chunk_z, self.min_y, self.height);
        let shape = OverworldShape::recognise(&self.final_density).expect("shape checked at build");
        let mut aquifer = Aquifer::new(self.aquifer.clone(), self.aquifer_random, &vol, self.picker);
        let (_, substance, stats) = fill_fused(&shape, &mut aquifer, &vol, beard, false);
        (substance, stats, aquifer)
    }

    /// Whether the native can carve: a carver stage and a biome stage.
    pub fn has_carvers(&self) -> bool {
        self.carvers.is_some() && self.biomes.is_some()
    }

    /// `ChunkAccess.carverBiome`: the biome at quart (4 cx, 0, 4 cz).
    pub fn carver_biome(&self, chunk_x: i32, chunk_z: i32) -> u16 {
        self.source_biomes(chunk_x, chunk_z)[8 * 17 + 8]
    }

    /// The carver biome of every source chunk within 8 of a chunk,
    /// `(dx + 8) * 17 + dz + 8`: from the kept biome output where a
    /// chunk has one, else sampled once and remembered. Each cache is
    /// locked once.
    pub fn source_biomes(&self, chunk_x: i32, chunk_z: i32) -> [u16; 289] {
        let mut out = [u16::MAX; 289];
        let quart_y = -self.min_y >> 2;
        let size_y = (self.height >> 2) as usize;
        if quart_y >= 0 && quart_y < self.height >> 2 {
            let cache = self.biome_cache.lock().unwrap();
            for (i, slot) in out.iter_mut().enumerate() {
                let key = (chunk_x + i as i32 / 17 - 8, chunk_z + i as i32 % 17 - 8);
                if let Some(biomes) = cache.map.get(&key).filter(|b| b.len() == 16 * size_y) {
                    *slot = biomes[quart_y as usize];
                }
            }
        }
        if out.iter().all(|&b| b != u16::MAX) {
            return out;
        }
        let stage = self.biomes.as_ref().expect("source_biomes needs the biome stage");
        let mut memo = self.carver_biomes.lock().unwrap();
        for (i, slot) in out.iter_mut().enumerate() {
            if *slot != u16::MAX {
                continue;
            }
            let key = (chunk_x + i as i32 / 17 - 8, chunk_z + i as i32 % 17 - 8);
            *slot = match memo.get(key) {
                Some(b) => b,
                None => {
                    let b = stage.biome_at_quart(key.0 << 2, 0, key.1 << 2);
                    memo.insert(key, b);
                    b
                }
            };
        }
        out
    }

    /// The carving mask of a chunk (`generateCarvers` before the apply).
    pub fn carve_mask(&self, chunk_x: i32, chunk_z: i32) -> Option<CarvingMask> {
        if !self.has_carvers() {
            return None;
        }
        let stage = self.carvers.as_ref()?;
        Some(stage.build_mask(chunk_x, chunk_z, &self.source_biomes(chunk_x, chunk_z)))
    }

    /// The biome stage for a chunk (quart biome indices), or None when
    /// the world gave no biome parameters. The result is kept so the
    /// surface and ore passes of this chunk and its neighbours can
    /// build their quart grid without the game.
    pub fn chunk_biomes(&self, chunk_x: i32, chunk_z: i32) -> Option<Vec<u16>> {
        let biomes = self.biomes.as_ref()?.chunk_biomes(chunk_x, chunk_z);
        self.biome_cache.lock().unwrap().insert((chunk_x, chunk_z), biomes.clone());
        Some(biomes)
    }

    /// The 6x6 quart grid around a chunk from the kept biome output, or
    /// None when any of the nine chunks is missing (never served, or
    /// evicted); the game then reads the grid itself.
    pub fn native_grid(&self, chunk_x: i32, chunk_z: i32) -> Option<QuartGrid> {
        let size_y = (self.height >> 2) as usize;
        let cache = self.biome_cache.lock().unwrap();
        let mut slots: [&[u16]; 9] = [&[]; 9];
        for (slot, entry) in slots.iter_mut().enumerate() {
            let key = (chunk_x + slot as i32 % 3 - 1, chunk_z + slot as i32 / 3 - 1);
            let biomes = cache.map.get(&key)?;
            if biomes.len() != 16 * size_y {
                return None;
            }
            *entry = biomes.as_slice();
        }
        let (min_x, min_y, min_z) = QuartGrid::origin(chunk_x, chunk_z, self.min_y);
        let mut data = vec![0u16; QuartGrid::SIZE_XZ * QuartGrid::SIZE_XZ * size_y];
        for qz in 0..QuartGrid::SIZE_XZ {
            for qx in 0..QuartGrid::SIZE_XZ {
                // Grid column 0 is the last column of the previous chunk, 5 the first of the next.
                let (cx, lx) = (qx.div_ceil(4), (qx + 3) % 4);
                let (cz, lz) = (qz.div_ceil(4), (qz + 3) % 4);
                let src = &slots[cx + cz * 3][(lx + lz * 4) * size_y..][..size_y];
                data[(qx + qz * QuartGrid::SIZE_XZ) * size_y..][..size_y].copy_from_slice(src);
            }
        }
        Some(QuartGrid { min_x, min_y, min_z, size_y, data })
    }

    /// The grid a pass runs with: the one the game handed over, checked,
    /// or the native one. None leaves the pending fill in place.
    fn grid_for(&self, chunk_x: i32, chunk_z: i32, quarts: Option<Vec<u16>>) -> Option<QuartGrid> {
        let Some(quarts) = quarts else {
            return self.native_grid(chunk_x, chunk_z);
        };
        let size_y = (self.height >> 2) as usize;
        if quarts.len() != QuartGrid::SIZE_XZ * QuartGrid::SIZE_XZ * size_y || quarts.iter().any(|&q| q as usize >= self.surface.biomes.len()) {
            return None;
        }
        let (min_x, min_y, min_z) = QuartGrid::origin(chunk_x, chunk_z, self.min_y);
        Some(QuartGrid { min_x, min_y, min_z, size_y, data: quarts })
    }

    /// Fill a chunk and keep the result for its surface pass.
    pub fn fill_pending(&self, chunk_x: i32, chunk_z: i32, beard: Option<&Beardifier>) {
        let (substance, _, aquifer) = self.fill_with_aquifer(chunk_x, chunk_z, beard);
        self.pending.lock().unwrap().insert((chunk_x, chunk_z), PendingFill { substance, aquifer });
    }

    /// Palette bytes (id | POST_PROCESS_FLAG) for a chunk as the fill
    /// left it, for the game to write when the surface pass is
    /// declined. `None` when no fill is pending.
    pub fn take_fill(&self, chunk_x: i32, chunk_z: i32) -> Option<Vec<u8>> {
        let PendingFill { substance, .. } = self.pending.lock().unwrap().remove(&(chunk_x, chunk_z))?;
        let chunk = ChunkBlocks::from_fill(&self.surface, chunk_x, chunk_z, &substance);
        Some(Self::encode(&chunk))
    }

    /// Run the surface pass on a pending fill with the biome quart grid
    /// (`QuartGrid` layout, biome indices in `SurfaceConfig::biomes`
    /// order), or with the native grid when `quarts` is None. `None`
    /// when nothing is pending or no usable grid exists; the fill stays
    /// pending in the second case so the caller can retry or take it.
    pub fn surface(&self, chunk_x: i32, chunk_z: i32, quarts: Option<Vec<u16>>) -> Option<Vec<u8>> {
        let grid = self.grid_for(chunk_x, chunk_z, quarts)?;
        let PendingFill { substance, .. } = self.pending.lock().unwrap().remove(&(chunk_x, chunk_z))?;
        let mut chunk = ChunkBlocks::from_fill(&self.surface, chunk_x, chunk_z, &substance);
        build_surface(&self.surface, &mut chunk, &grid, self.biome_zoom_seed);
        Some(Self::encode(&chunk))
    }

    fn encode(chunk: &ChunkBlocks) -> Vec<u8> {
        chunk
            .blocks
            .iter()
            .zip(&chunk.post_process)
            .map(|(&id, &post)| (id as u8) | if post { POST_PROCESS_FLAG } else { 0 })
            .collect()
    }

    /// Like `surface` but returns the chunk as packed sections, carved
    /// when asked and the stage exists. `None` for a carve request the
    /// native cannot serve, with the fill left pending.
    pub fn surface_packed(&self, chunk_x: i32, chunk_z: i32, quarts: Option<Vec<u16>>, carve: bool) -> Option<Vec<i64>> {
        if carve && !self.has_carvers() {
            return None;
        }
        let grid = self.grid_for(chunk_x, chunk_z, quarts)?;
        let PendingFill { substance, mut aquifer } = self.pending.lock().unwrap().remove(&(chunk_x, chunk_z))?;
        let mut chunk = ChunkBlocks::from_fill(&self.surface, chunk_x, chunk_z, &substance);
        if carve {
            let t0 = std::time::Instant::now();
            let stage = self.carvers.as_ref()?;
            let mask = stage.build_mask(chunk_x, chunk_z, &self.source_biomes(chunk_x, chunk_z));
            let mask_ns = t0.elapsed().as_nanos() as u64;
            let stats = build_surface_carved(&self.surface, &mut chunk, &grid, self.biome_zoom_seed, Some(CarveJob { stage, mask: &mask, aquifer: &mut aquifer }));
            let mut totals = CARVE_TOTALS.lock().unwrap();
            totals.chunks += 1;
            totals.mask_ns += mask_ns;
            totals.apply_ns += (stats.carve.apply_ms * 1e6) as u64;
            totals.carved += stats.carve.carved;
            totals.aquifer_calls += stats.carve.aquifer_calls;
            totals.top_material_calls += stats.carve.top_material_calls;
        } else {
            build_surface(&self.surface, &mut chunk, &grid, self.biome_zoom_seed);
        }
        if self.keep_lod.load(Ordering::Relaxed) {
            // A region write error at eviction is reported again by the next flush.
            let _ = self.lod.insert(chunk_x, chunk_z, ColumnLod::from_chunk(&self.surface, &chunk, &grid));
        }
        Some(pack_chunk(&chunk))
    }

    /// Like `take_fill` but packed.
    pub fn take_fill_packed(&self, chunk_x: i32, chunk_z: i32) -> Option<Vec<i64>> {
        let PendingFill { substance, .. } = self.pending.lock().unwrap().remove(&(chunk_x, chunk_z))?;
        let chunk = ChunkBlocks::from_fill(&self.surface, chunk_x, chunk_z, &substance);
        Some(pack_chunk(&chunk))
    }

    /// Placed ore feature ids the native serves, in index order.
    pub fn ore_placed_ids(&self) -> Option<&[String]> {
        self.ores.as_ref().map(|o| o.placed_ids.as_slice())
    }

    /// Keep an ore batch (feature seeds, placed indices, the chunk's
    /// quart grid or None for the native one) and answer with the
    /// section range (inclusive, from the bottom) the game must hand
    /// over. `None` rejects the batch.
    pub fn ore_plan(&self, chunk_x: i32, chunk_z: i32, seeds: Vec<i64>, placed: Vec<u16>, quarts: Option<Vec<u16>>) -> Option<(usize, usize)> {
        let ores = self.ores.as_ref()?;
        if seeds.len() != placed.len() || seeds.is_empty() {
            return None;
        }
        let grid = self.grid_for(chunk_x, chunk_z, quarts)?;
        let batch = OreBatch { seeds, placed, grid };
        let range = ores.plan(&batch)?;
        self.ore_pending.lock().unwrap().insert((chunk_x, chunk_z), batch);
        Some(range)
    }

    /// Run a planned batch. `heights`: nine `OCEAN_FLOOR_WG` raw
    /// heightmaps in chunk slot order; `meta`: six ints per section
    /// (`slot | section << 4`, palette length or 0 for the global
    /// palette, bits, palette offset, storage offset, storage length)
    /// into `palettes` (global state ids) and `storage`. Returns two
    /// ints per write: `slot | section << 4 | packed << 16` and the
    /// palette id. `None` when nothing was planned or the data is
    /// malformed; the batch is dropped either way.
    pub fn ore_apply(&self, chunk_x: i32, chunk_z: i32, heights: &[i64], meta: &[i32], palettes: &[i32], storage: &[i64]) -> Option<Vec<i32>> {
        let batch = self.ore_pending.lock().unwrap().remove(&(chunk_x, chunk_z))?;
        let ores = self.ores.as_ref()?;
        if heights.len() != 9 * HEIGHT_LONGS || !meta.len().is_multiple_of(6) {
            return None;
        }
        let mut region = Region::new(chunk_x, chunk_z, self.min_y, self.height, heights).ok()?;
        for m in meta.as_chunks::<6>().0 {
            let (key, palette_len, bits) = (m[0], m[1], m[2]);
            let (palette_off, storage_off, storage_len) = (m[3] as usize, m[4] as usize, m[5] as usize);
            let palette: Vec<u16> = palettes.get(palette_off..palette_off + palette_len.max(0) as usize)?.iter().map(|&id| u16::try_from(id).ok()).collect::<Option<_>>()?;
            let raw: Vec<u64> = storage.get(storage_off..storage_off + storage_len)?.iter().map(|&w| w as u64).collect();
            let section = Section::new(bits as u32, raw, palette).ok()?;
            region.install((key & 15) as usize, (key >> 4) as usize, section).ok()?;
        }
        ores.apply(&batch, &mut region, self.biome_zoom_seed);
        let mut out = Vec::with_capacity(region.writes.len() * 2);
        for w in &region.writes {
            out.push((w.slot | (w.packed as u32) << 16) as i32);
            out.push(w.state as i32);
        }
        Some(out)
    }
}

/// Digest of the palette names and biome ids a world compiled to; a
/// region file of column records built under another set is stale.
fn lod_hash(surface: &SurfaceConfig) -> [u8; 16] {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    for name in &surface.palette.names {
        h.update(name.as_bytes());
        h.update(b"\n");
    }
    h.update(b"--\n");
    for biome in &surface.biomes {
        h.update(biome.id.as_bytes());
        h.update(b"\n");
    }
    h.finalize().into()
}

/// `Mth.ceillog2`.
fn ceil_log2(n: u32) -> u32 {
    if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() }
}

/// The bits per entry vanilla's block state `Strategy` picks for a
/// palette of `n` entries: 0 for one value, 4 up to sixteen, then the
/// exact count up to 8. Larger palettes go global, which this
/// pipeline never produces (the whole surface palette is under 128,
/// but a section could in theory exceed 256 only with more states
/// than exist).
fn section_bits(n: usize) -> u32 {
    match ceil_log2(n as u32) {
        0 => 0,
        1..=4 => 4,
        b => b,
    }
}

/// The chunk as the game's own storage: per section a local palette
/// and `SimpleBitStorage` longs, then both worldgen heightmaps as
/// `Heightmap` raw longs, then the post-processing positions.
///
/// Layout (i64 words): `[sections, heightmap_longs]`, then per section
/// `[palette_len | bits << 16 | storage_longs << 32]`, the palette ids
/// four per word (16 bits each, low first), the storage words; then
/// the surface heightmap words, the floor heightmap words; then
/// `[post_count]` and one word per position `section << 16 | packed`
/// with `packed = x | y << 4 | z << 8` (ProtoChunk.packOffsetCoordinates).
pub fn pack_chunk(chunk: &ChunkBlocks) -> Vec<i64> {
    let vol = &chunk.vol;
    let height = vol.size[1];
    let sections = height / 16;
    let height_bits = ceil_log2(height as u32 + 1);
    let heightmap_longs = 256usize.div_ceil(64 / height_bits as usize);
    let mut out: Vec<i64> = vec![sections as i64, heightmap_longs as i64];
    let mut post: Vec<i64> = Vec::new();
    let mut local = vec![0u16; 4096];
    let mut palette: Vec<u16> = Vec::new();
    let mut lookup = vec![u16::MAX; chunk_palette_len(chunk)];
    for s in 0..sections {
        palette.clear();
        lookup.iter_mut().for_each(|v| *v = u16::MAX);
        for ly in 0..16 {
            for lz in 0..16 {
                for lx in 0..16 {
                    let i = vol.index(lx, s * 16 + ly, lz);
                    let id = chunk.blocks[i];
                    if chunk.post_process[i] {
                        post.push(((s as i64) << 16) | (lx as i64) | ((ly as i64) << 4) | ((lz as i64) << 8));
                    }
                    let slot = if lookup[id as usize] == u16::MAX {
                        let n = palette.len() as u16;
                        palette.push(id);
                        lookup[id as usize] = n;
                        n
                    } else {
                        lookup[id as usize]
                    };
                    local[(ly << 8) | (lz << 4) | lx] = slot;
                }
            }
        }
        let bits = section_bits(palette.len());
        let storage_longs = if bits == 0 { 0 } else { 4096usize.div_ceil(64 / bits as usize) };
        out.push((palette.len() as i64) | ((bits as i64) << 16) | ((storage_longs as i64) << 32));
        for chunk4 in palette.chunks(4) {
            let mut w = 0i64;
            for (k, &id) in chunk4.iter().enumerate() {
                w |= (id as i64) << (16 * k);
            }
            out.push(w);
        }
        if bits > 0 {
            let vpl = 64 / bits as usize;
            for c in 0..storage_longs {
                let mut w = 0u64;
                for k in 0..vpl {
                    let idx = c * vpl + k;
                    if idx >= 4096 {
                        break;
                    }
                    w |= (local[idx] as u64) << (k as u32 * bits);
                }
                out.push(w as i64);
            }
        }
    }
    for map in [&chunk.surface_height, &chunk.floor_height] {
        let vpl = 64 / height_bits as usize;
        for c in 0..heightmap_longs {
            let mut w = 0u64;
            for k in 0..vpl {
                let idx = c * vpl + k;
                if idx >= 256 {
                    break;
                }
                w |= ((map[idx] - vol.min[1]) as u64) << (k as u32 * height_bits);
            }
            out.push(w as i64);
        }
    }
    out.push(post.len() as i64);
    out.extend(post);
    out
}

fn chunk_palette_len(chunk: &ChunkBlocks) -> usize {
    chunk.blocks.iter().map(|&b| b as usize + 1).max().unwrap_or(1)
}

/// The settings' `default_fluid` block state in any datapack form: a bare
/// id, `{"id": ..}` (26.3 codec) or the older `{"Name": ..}`.
fn default_fluid(v: Option<&Value>) -> Result<Fluid, Unsupported> {
    let id = match v {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Object(o)) => o.get("id").or_else(|| o.get("Name")).and_then(Value::as_str),
        _ => None,
    };
    let id = id.ok_or_else(|| Unsupported::Settings("default_fluid".into()))?;
    match id.strip_prefix("minecraft:").unwrap_or(id) {
        "water" => Ok(Fluid::Water),
        "lava" => Ok(Fluid::Lava),
        "air" => Ok(Fluid::Air),
        _ => Err(Unsupported::Settings(format!("default_fluid {id}"))),
    }
}

#[cfg(test)]
mod pack_tests {
    use super::*;

    #[test]
    fn default_fluid_every_form() {
        let fluid = |v: Value| default_fluid(Some(&v)).ok();
        assert_eq!(fluid(serde_json::json!({"id": "minecraft:lava", "properties": {"level": "0"}})), Some(Fluid::Lava));
        assert_eq!(fluid(serde_json::json!({"Name": "minecraft:water", "Properties": {"level": "0"}})), Some(Fluid::Water));
        assert_eq!(fluid(serde_json::json!("minecraft:air")), Some(Fluid::Air));
        assert_eq!(fluid(serde_json::json!({"id": "minecraft:honey_block"})), None);
        assert!(default_fluid(None).is_err());
    }

    /// `SimpleBitStorage.get`.
    fn storage_get(words: &[i64], bits: u32, index: usize) -> u64 {
        let vpl = 64 / bits as usize;
        let cell = index / vpl;
        let shift = ((index - cell * vpl) as u32) * bits;
        ((words[cell] as u64) >> shift) & ((1u64 << bits) - 1)
    }

    #[test]
    fn packed_chunk_round_trips() {
        let vol = Volume::chunk(3, -2, -64, 384);
        let mut blocks = vec![1u16; vol.len()];
        let mut post = vec![false; vol.len()];
        for z in 0..16 {
            for x in 0..16 {
                for y in 0..200 {
                    let i = vol.index(x, y, z);
                    blocks[i] = ((x * 7 + y * 3 + z * 11) % 23) as u16;
                    if blocks[i] == 5 && y % 17 == 0 {
                        post[i] = true;
                    }
                }
            }
        }
        let mut surface = [0i32; 256];
        let mut floor = [0i32; 256];
        for (i, (s, f)) in surface.iter_mut().zip(floor.iter_mut()).enumerate() {
            *s = -64 + 136 + (i % 5) as i32;
            *f = -64 + 130;
        }
        let chunk = ChunkBlocks { vol, blocks: blocks.clone(), post_process: post.clone(), surface_height: surface, floor_height: floor };
        let blob = pack_chunk(&chunk);
        assert_eq!(blob[0], 24);
        assert_eq!(blob[1], 37);
        let mut at = 2;
        let mut posts = std::collections::HashSet::new();
        for s in 0..24usize {
            let header = blob[at] as u64;
            let n = (header & 0xffff) as usize;
            let bits = ((header >> 16) & 0xffff) as u32;
            let storage_longs = (header >> 32) as usize;
            at += 1;
            let palette: Vec<u16> = (0..n).map(|k| ((blob[at + k / 4] as u64) >> (16 * (k % 4))) as u16).collect();
            at += n.div_ceil(4);
            assert_eq!(bits, section_bits(n));
            for idx in 0..4096 {
                let slot = if bits == 0 { 0 } else { storage_get(&blob[at..at + storage_longs], bits, idx) as usize };
                let (x, y, z) = (idx & 15, (idx >> 8) & 15, (idx >> 4) & 15);
                assert_eq!(palette[slot], blocks[vol.index(x, s * 16 + y, z)], "section {s} idx {idx}");
            }
            at += storage_longs;
        }
        for map in [&surface, &floor] {
            for (i, &h) in map.iter().enumerate() {
                assert_eq!(storage_get(&blob[at..at + 37], 9, i) as i32, h + 64);
            }
            at += 37;
        }
        let count = blob[at] as usize;
        at += 1;
        for w in &blob[at..at + count] {
            let s = (w >> 16) as usize;
            let packed = (w & 0xffff) as usize;
            let (x, y, z) = (packed & 15, (packed >> 4) & 15, (packed >> 8) & 15);
            posts.insert(vol.index(x, s * 16 + y, z));
        }
        assert_eq!(at + count, blob.len());
        let expected: std::collections::HashSet<usize> = post.iter().enumerate().filter(|(_, p)| **p).map(|(i, _)| i).collect();
        assert_eq!(posts, expected);
    }
}
