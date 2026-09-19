//! Far-view columns: one surface height, top block and biome per
//! column, read off the surface pass and kept per region on disk so a
//! restart keeps them. Chunks the native did not surface leave a hole.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::surface26::{ChunkBlocks, QuartGrid, SurfaceConfig};

pub const COLUMNS: usize = 256;
/// Bytes of one chunk record on disk: three 16-bit values per column.
pub const RECORD_BYTES: usize = COLUMNS * 9;
/// Record bytes of version 1 and 2 files: no water depth or floor.
const RECORD_BYTES_V2: usize = COLUMNS * 6;
const CHUNKS_PER_REGION: usize = 32 * 32;
const BITMAP_BYTES: usize = CHUNKS_PER_REGION / 8;
const MAGIC: &[u8; 4] = b"PLOD";
const VERSION: u16 = 3;
const HEADER_BYTES: usize = 4 + 2 + 16 + BITMAP_BYTES + CHUNKS_PER_REGION;
/// Record stages: the generator's surface, or refreshed from the finished chunk (trees, snow, placed blocks).
pub const STAGE_SURFACE: u8 = 0;
pub const STAGE_FINAL: u8 = 1;
/// Regions kept in memory; the oldest loaded is written out and dropped past this.
pub const MAX_REGIONS: usize = 256;

/// One chunk's columns, `x + z * 16`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColumnLod {
    /// `STAGE_SURFACE` from the generator, `STAGE_FINAL` once refreshed from the finished chunk.
    pub stage: u8,
    /// `WORLD_SURFACE_WG` first-available height: the top non-air block plus one, `min_y` for an empty column.
    pub height: [i16; COLUMNS],
    /// Palette id of the block under `height`; air for an empty column.
    pub top: [u16; COLUMNS],
    /// Biome index (biome document order) of the quart holding the top block.
    pub biome: [u16; COLUMNS],
    /// Water blocks from `height` down to the floor, 0 for a dry column (255 at most).
    pub depth: [u8; COLUMNS],
    /// Palette id of the block under the water; `top` for a dry column.
    pub floor: [u16; COLUMNS],
}

impl ColumnLod {
    pub fn empty(stage: u8) -> Self {
        Self { stage, height: [0; COLUMNS], top: [0; COLUMNS], biome: [0; COLUMNS], depth: [0; COLUMNS], floor: [0; COLUMNS] }
    }

    /// The record of a surfaced chunk.
    pub fn from_chunk(cfg: &SurfaceConfig, chunk: &ChunkBlocks, grid: &QuartGrid) -> Self {
        let mut out = Self::empty(STAGE_SURFACE);
        let (min_x, min_z) = (chunk.vol.min[0], chunk.vol.min[2]);
        let min_y = chunk.vol.min[1];
        for z in 0..16 {
            for x in 0..16 {
                let col = x + z * 16;
                let h = chunk.surface_height[col];
                let top_y = h - 1;
                out.height[col] = h as i16;
                out.top[col] = chunk.get(cfg, x, top_y, z);
                out.biome[col] = grid.get((min_x + x as i32) >> 2, top_y >> 2, (min_z + z as i32) >> 2);
                out.floor[col] = out.top[col];
                if out.top[col] == cfg.water {
                    // Down through the water to the floor block; the depth saturates at 255.
                    let mut y = top_y - 1;
                    let mut depth = 1u32;
                    while y >= min_y && chunk.get(cfg, x, y, z) == cfg.water {
                        depth += 1;
                        y -= 1;
                    }
                    out.depth[col] = depth.min(255) as u8;
                    out.floor[col] = if y >= min_y { chunk.get(cfg, x, y, z) } else { cfg.water };
                }
            }
        }
        out
    }

    /// The record as ints for the JNI side: heights, then tops, then biomes.
    pub fn to_ints(&self) -> Vec<i32> {
        let mut out = Vec::with_capacity(COLUMNS * 3);
        out.extend(self.height.iter().map(|&h| i32::from(h)));
        out.extend(self.top.iter().map(|&t| i32::from(t)));
        out.extend(self.biome.iter().map(|&b| i32::from(b)));
        out
    }

    /// The record's bytes as a batch entry or region file carries them, for tools.
    pub fn write_to_public(&self, out: &mut Vec<u8>) {
        self.write_to(out);
    }

    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        for h in self.height {
            out.extend_from_slice(&h.to_le_bytes());
        }
        for t in self.top {
            out.extend_from_slice(&t.to_le_bytes());
        }
        for b in self.biome {
            out.extend_from_slice(&b.to_le_bytes());
        }
        out.extend_from_slice(&self.depth);
        for f in self.floor {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }

    pub fn read_from(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let mut out = Self::read_from_v2(bytes)?;
        let depth_at = 3 * COLUMNS * 2;
        out.depth.copy_from_slice(&bytes[depth_at..depth_at + COLUMNS]);
        let floor_at = depth_at + COLUMNS;
        for c in 0..COLUMNS {
            out.floor[c] = u16::from_le_bytes([bytes[floor_at + c * 2], bytes[floor_at + c * 2 + 1]]);
        }
        Some(out)
    }

    /// A record without water depth (version 1 and 2 files): every column dry, the floor its top.
    fn read_from_v2(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES_V2 {
            return None;
        }
        let mut out = Self::empty(STAGE_SURFACE);
        let pair = |i: usize| [bytes[i * 2], bytes[i * 2 + 1]];
        for c in 0..COLUMNS {
            out.height[c] = i16::from_le_bytes(pair(c));
            out.top[c] = u16::from_le_bytes(pair(COLUMNS + c));
            out.biome[c] = u16::from_le_bytes(pair(2 * COLUMNS + c));
        }
        out.floor = out.top;
        Some(out)
    }
}

/// Cells per side of a coarse record: one per 4x4 columns.
pub const COARSE_SIDE: usize = 4;
pub const COARSE_CELLS: usize = COARSE_SIDE * COARSE_SIDE;
/// Bytes of a coarse record over the wire: height, top, depth, floor and biome per cell.
pub const COARSE_BYTES: usize = COARSE_CELLS * 9;

/// A chunk at one cell per 4x4 columns, the tallest column of each: what the
/// outer rings get over the wire, `x + z * 4`. Kept in memory only.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CoarseLod {
    pub height: [i16; COARSE_CELLS],
    pub top: [u16; COARSE_CELLS],
    pub depth: [u8; COARSE_CELLS],
    pub floor: [u16; COARSE_CELLS],
    pub biome: [u16; COARSE_CELLS],
}

impl CoarseLod {
    pub fn empty() -> Self {
        Self { height: [0; COARSE_CELLS], top: [0; COARSE_CELLS], depth: [0; COARSE_CELLS], floor: [0; COARSE_CELLS], biome: [0; COARSE_CELLS] }
    }

    pub fn of(rec: &ColumnLod) -> Self {
        let mut out = Self { height: [i16::MIN; COARSE_CELLS], ..Self::empty() };
        for c in 0..COLUMNS {
            let cell = (c % 16) / COARSE_SIDE + ((c / 16) / COARSE_SIDE) * COARSE_SIDE;
            if rec.height[c] > out.height[cell] {
                out.height[cell] = rec.height[c];
                out.top[cell] = rec.top[c];
                out.depth[cell] = rec.depth[c];
                out.floor[cell] = rec.floor[c];
                out.biome[cell] = rec.biome[c];
            }
        }
        out
    }

    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        for h in self.height {
            out.extend_from_slice(&h.to_le_bytes());
        }
        for t in self.top {
            out.extend_from_slice(&t.to_le_bytes());
        }
        out.extend_from_slice(&self.depth);
        for f in self.floor {
            out.extend_from_slice(&f.to_le_bytes());
        }
        for b in self.biome {
            out.extend_from_slice(&b.to_le_bytes());
        }
    }

    pub fn read_from(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < COARSE_BYTES {
            return None;
        }
        let mut out = Self::empty();
        let depth_at = COARSE_CELLS * 4;
        let floor_at = depth_at + COARSE_CELLS;
        let biome_at = floor_at + COARSE_CELLS * 2;
        for c in 0..COARSE_CELLS {
            out.height[c] = i16::from_le_bytes([bytes[c * 2], bytes[c * 2 + 1]]);
            out.top[c] = u16::from_le_bytes([bytes[(COARSE_CELLS + c) * 2], bytes[(COARSE_CELLS + c) * 2 + 1]]);
            out.depth[c] = bytes[depth_at + c];
            out.floor[c] = u16::from_le_bytes([bytes[floor_at + c * 2], bytes[floor_at + c * 2 + 1]]);
            out.biome[c] = u16::from_le_bytes([bytes[biome_at + c * 2], bytes[biome_at + c * 2 + 1]]);
        }
        Some(out)
    }
}

/// A record as the client holds it: every column, or the coarse cells of an outer ring.
#[derive(Clone, Copy)]
pub enum RecordRef<'a> {
    Full(&'a ColumnLod),
    Coarse(&'a CoarseLod),
}

/// A 32x32 chunk region of records, present or not per chunk.
struct RegionLod {
    records: Vec<Option<Box<ColumnLod>>>,
    dirty: bool,
}

impl RegionLod {
    fn empty() -> Self {
        Self { records: (0..CHUNKS_PER_REGION).map(|_| None).collect(), dirty: false }
    }

    /// Parse a region file; None when it is not ours, another version,
    /// or built under a different palette or biome list.
    fn parse(bytes: &[u8], hash: &[u8; 16]) -> Option<Self> {
        if bytes.len() < 22 + BITMAP_BYTES || &bytes[0..4] != MAGIC || &bytes[6..22] != hash {
            return None;
        }
        let mut region = Self::empty();
        for (slot, record) in region_records_any(bytes)? {
            region.records[slot] = Some(Box::new(record));
        }
        Some(region)
    }

    fn encode(&self, hash: &[u8; 16]) -> Vec<u8> {
        let present = self.records.iter().filter(|r| r.is_some()).count();
        let mut out = Vec::with_capacity(HEADER_BYTES + present * RECORD_BYTES);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(hash);
        let mut bitmap = [0u8; BITMAP_BYTES];
        for (slot, record) in self.records.iter().enumerate() {
            if record.is_some() {
                bitmap[slot / 8] |= 1 << (slot % 8);
            }
        }
        out.extend_from_slice(&bitmap);
        for record in &self.records {
            out.push(record.as_ref().map_or(0, |r| r.stage));
        }
        for record in self.records.iter().flatten() {
            record.write_to(&mut out);
        }
        out
    }
}

struct Inner {
    dir: Option<PathBuf>,
    regions: HashMap<(i32, i32), RegionLod>,
    /// Load order, oldest first, for eviction.
    order: VecDeque<(i32, i32)>,
}

/// The records of one world, by region, with the directory they
/// persist to once the game names it.
pub struct LodStore {
    /// Digest of the palette names and biome ids; a region file built under another set is discarded.
    hash: [u8; 16],
    /// Regions kept in memory before the oldest is written and dropped.
    max_regions: usize,
    inner: Mutex<Inner>,
    /// Bumped per insert so a sender's walk knows when to look again.
    generation: AtomicU64,
    /// Chunks whose record was replaced after being possibly sent, with the count at that time.
    refreshed: Mutex<Vec<(u64, (i32, i32))>>,
}

fn region_slot(chunk_x: i32, chunk_z: i32) -> ((i32, i32), usize) {
    ((chunk_x >> 5, chunk_z >> 5), ((chunk_x & 31) + (chunk_z & 31) * 32) as usize)
}

/// Records of a region file by slot (x + z * 32), for tools; the palette hash is not checked.
pub fn region_records(bytes: &[u8]) -> Option<Vec<(usize, ColumnLod)>> {
    if bytes.len() < 22 + BITMAP_BYTES || &bytes[0..4] != MAGIC {
        return None;
    }
    region_records_any(bytes)
}

/// Records by slot of a version 1 (no stages, all generator surface), 2 (no water depth) or 3 file; magic and hash already checked.
/// Records from older files count as the generator's stage, so the next load refreshes them.
fn region_records_any(bytes: &[u8]) -> Option<Vec<(usize, ColumnLod)>> {
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    let bitmap = &bytes[22..22 + BITMAP_BYTES];
    let (stages, mut at): (Option<&[u8]>, usize) = match version {
        1 => (None, 22 + BITMAP_BYTES),
        2 => (None, HEADER_BYTES),
        3 => (Some(bytes.get(22 + BITMAP_BYTES..HEADER_BYTES)?), HEADER_BYTES),
        _ => return None,
    };
    let (read, size): (fn(&[u8]) -> Option<ColumnLod>, usize) = if version == 3 { (ColumnLod::read_from, RECORD_BYTES) } else { (ColumnLod::read_from_v2, RECORD_BYTES_V2) };
    let mut out = Vec::new();
    for slot in 0..CHUNKS_PER_REGION {
        if bitmap[slot / 8] & (1 << (slot % 8)) != 0 {
            let mut r = read(bytes.get(at..)?)?;
            r.stage = stages.map_or(STAGE_SURFACE, |s| s[slot]);
            out.push((slot, r));
            at += size;
        }
    }
    Some(out)
}

fn region_path(dir: &Path, region: (i32, i32)) -> PathBuf {
    dir.join(format!("r.{}.{}.plod", region.0, region.1))
}

impl LodStore {
    pub fn new(hash: [u8; 16]) -> Self {
        Self::with_max_regions(hash, MAX_REGIONS)
    }

    pub fn with_max_regions(hash: [u8; 16], max_regions: usize) -> Self {
        Self { hash, max_regions: max_regions.max(1), inner: Mutex::new(Inner { dir: None, regions: HashMap::new(), order: VecDeque::new() }), generation: AtomicU64::new(0), refreshed: Mutex::new(Vec::new()) }
    }

    /// One bit per chunk slot (x + z * 32) of a region, set where a record exists
    /// (`final_only`: only records refreshed from the finished chunk).
    pub fn present_bitmap(&self, rx: i32, rz: i32, final_only: bool) -> [u8; BITMAP_BYTES] {
        let mut inner = self.inner.lock().unwrap();
        let _ = self.load_region(&mut inner, (rx, rz));
        let mut out = [0u8; BITMAP_BYTES];
        if let Some(region) = inner.regions.get(&(rx, rz)) {
            for (slot, record) in region.records.iter().enumerate() {
                if record.as_ref().is_some_and(|r| !final_only || r.stage == STAGE_FINAL) {
                    out[slot / 8] |= 1 << (slot % 8);
                }
            }
        }
        out
    }

    /// The stage of a chunk's record, or None without one.
    pub fn stage(&self, chunk_x: i32, chunk_z: i32) -> Option<u8> {
        let (key, slot) = region_slot(chunk_x, chunk_z);
        let mut inner = self.inner.lock().unwrap();
        let _ = self.load_region(&mut inner, key);
        inner.regions.get(&key).and_then(|r| r.records[slot].as_ref().map(|r| r.stage))
    }

    /// Replace heights and tops from the finished chunk, keeping the biomes; the record
    /// becomes `STAGE_FINAL` and every sender forgets it was sent. A chunk without a
    /// record gets one with unknown biomes (index 0).
    pub fn refresh(&self, chunk_x: i32, chunk_z: i32, heights: &[i16; COLUMNS], tops: &[u16; COLUMNS], depths: &[u8; COLUMNS], floors: &[u16; COLUMNS]) -> io::Result<()> {
        let (key, slot) = region_slot(chunk_x, chunk_z);
        let mut inner = self.inner.lock().unwrap();
        let evicted = self.load_region(&mut inner, key);
        let region = inner.regions.get_mut(&key).expect("region loaded");
        let record = region.records[slot].get_or_insert_with(|| Box::new(ColumnLod::empty(STAGE_SURFACE)));
        record.height = *heights;
        record.top = *tops;
        record.depth = *depths;
        record.floor = *floors;
        record.stage = STAGE_FINAL;
        region.dirty = true;
        let n = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.refreshed.lock().unwrap().push((n, (chunk_x, chunk_z)));
        evicted
    }

    /// Chunks refreshed after count `since`, and the count now; the log is trimmed
    /// to the last 65536 entries.
    pub fn refreshed_since(&self, since: u64) -> (Vec<(i32, i32)>, u64) {
        let mut log = self.refreshed.lock().unwrap();
        if log.len() > 65536 {
            let cut = log.len() - 65536;
            log.drain(..cut);
        }
        let now = self.generation();
        (log.iter().filter(|(n, _)| *n > since).map(|(_, c)| *c).collect(), now)
    }

    /// Every record of a region as (chunk x, chunk z, record).
    pub fn region_records(&self, rx: i32, rz: i32) -> Vec<(i32, i32, ColumnLod)> {
        let mut inner = self.inner.lock().unwrap();
        let _ = self.load_region(&mut inner, (rx, rz));
        let Some(region) = inner.regions.get(&(rx, rz)) else {
            return Vec::new();
        };
        region
            .records
            .iter()
            .enumerate()
            .filter_map(|(slot, r)| r.as_deref().map(|r| (rx * 32 + (slot % 32) as i32, rz * 32 + (slot / 32) as i32, r.clone())))
            .collect()
    }

    /// Name the directory; regions touched before this stay in memory
    /// until the next flush. Creates the directory.
    pub fn set_dir(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        self.inner.lock().unwrap().dir = Some(dir.to_path_buf());
        Ok(())
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap().dir.clone()
    }

    /// Keep a chunk's record. Returns a write error from evicting an older region, if any.
    pub fn insert(&self, chunk_x: i32, chunk_z: i32, record: ColumnLod) -> io::Result<()> {
        let (key, slot) = region_slot(chunk_x, chunk_z);
        let mut inner = self.inner.lock().unwrap();
        let evicted = self.load_region(&mut inner, key);
        let region = inner.regions.get_mut(&key).expect("region loaded");
        region.records[slot] = Some(Box::new(record));
        region.dirty = true;
        self.generation.fetch_add(1, Ordering::Relaxed);
        evicted
    }

    /// Count of inserts so far.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// A chunk's record, from memory or its region file.
    pub fn get(&self, chunk_x: i32, chunk_z: i32) -> Option<ColumnLod> {
        let (key, slot) = region_slot(chunk_x, chunk_z);
        let mut inner = self.inner.lock().unwrap();
        // An eviction write error here is reported by the next flush.
        let _ = self.load_region(&mut inner, key);
        inner.regions.get(&key).and_then(|r| r.records[slot].as_deref().cloned())
    }

    /// Walk `candidates` (chunk, wants the full record) in order under one lock,
    /// appending an entry per present chunk that `sent` does not already cover at
    /// that level, up to `max`; returns the chunks taken with the level written.
    pub fn take_batch(&self, candidates: impl Iterator<Item = (i32, i32, bool)>, sent: &HashMap<(i32, i32), Level>, max: usize, out: &mut Vec<u8>) -> Vec<((i32, i32), Level)> {
        let mut taken = Vec::new();
        let mut inner = self.inner.lock().unwrap();
        for (x, z, full) in candidates {
            if taken.len() >= max {
                break;
            }
            let level = if full { Level::Full } else { Level::Coarse };
            if sent.get(&(x, z)).is_some_and(|&have| have >= level) {
                continue;
            }
            let (key, slot) = region_slot(x, z);
            let _ = self.load_region(&mut inner, key);
            if let Some(record) = inner.regions.get(&key).and_then(|r| r.records[slot].as_deref()) {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&z.to_le_bytes());
                let mut flags = u32::from(record.stage);
                if !full {
                    flags |= BATCH_COARSE;
                }
                out.extend_from_slice(&flags.to_le_bytes());
                match level {
                    Level::Full => record.write_to(out),
                    Level::Coarse => CoarseLod::of(record).write_to(out),
                }
                taken.push(((x, z), level));
            }
        }
        taken
    }

    /// Chunks present in a region file or memory, for the command.
    pub fn region_count(&self, chunk_x: i32, chunk_z: i32) -> usize {
        let (key, _) = region_slot(chunk_x, chunk_z);
        let mut inner = self.inner.lock().unwrap();
        let _ = self.load_region(&mut inner, key);
        inner.regions.get(&key).map_or(0, |r| r.records.iter().filter(|x| x.is_some()).count())
    }

    /// Write every dirty region; returns how many were written. Without
    /// a directory nothing is written and the records stay in memory.
    pub fn flush(&self) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        let Some(dir) = inner.dir.clone() else {
            return Ok(0);
        };
        let mut written = 0;
        let keys: Vec<(i32, i32)> = inner.regions.iter().filter(|(_, r)| r.dirty).map(|(k, _)| *k).collect();
        for key in keys {
            let bytes = inner.regions[&key].encode(&self.hash);
            write_atomic(&region_path(&dir, key), &bytes)?;
            inner.regions.get_mut(&key).unwrap().dirty = false;
            written += 1;
        }
        Ok(written)
    }

    /// Make sure a region is in memory, reading its file when there is
    /// one; past the cache bound the oldest region is written and dropped.
    fn load_region(&self, inner: &mut Inner, key: (i32, i32)) -> io::Result<()> {
        if inner.regions.contains_key(&key) {
            return Ok(());
        }
        let mut result = Ok(());
        if inner.regions.len() >= self.max_regions
            && let Some(old) = inner.order.pop_front()
            && let Some(region) = inner.regions.remove(&old)
            && region.dirty
            && let Some(dir) = inner.dir.as_ref()
        {
            result = write_atomic(&region_path(dir, old), &region.encode(&self.hash));
        }
        let region = inner
            .dir
            .as_ref()
            .and_then(|dir| fs::read(region_path(dir, key)).ok())
            .and_then(|bytes| RegionLod::parse(&bytes, &self.hash))
            .unwrap_or_else(RegionLod::empty);
        inner.regions.insert(key, region);
        inner.order.push_back(key);
        result
    }
}

/// Write through a sibling temp file and rename, so a crash mid-write leaves the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("plod.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, path)
}

/// Read a region file's chunk count without keeping it (tools, tests).
pub fn count_file(path: &Path, hash: &[u8; 16]) -> Option<usize> {
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
    RegionLod::parse(&bytes, hash).map(|r| r.records.iter().filter(|x| x.is_some()).count())
}

/// Bytes of a batch entry's header: chunk x, chunk z (i32 each), then flags (stage in the low byte).
pub const BATCH_HEADER_BYTES: usize = 8 + 4;
/// Flag bit: the entry carries a coarse record (`COARSE_BYTES`) instead of a full one.
pub const BATCH_COARSE: u32 = 1 << 8;
/// Bytes of a full-record entry.
pub const BATCH_ENTRY_BYTES: usize = BATCH_HEADER_BYTES + RECORD_BYTES;
/// Bytes of a coarse-record entry.
pub const COARSE_ENTRY_BYTES: usize = BATCH_HEADER_BYTES + COARSE_BYTES;

/// How much of a chunk a client was sent, or holds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Coarse,
    Full,
}

/// Chunks within `far` of a centre, nearest ring first.
fn disc(cx: i32, cz: i32, far: i32) -> impl Iterator<Item = (i32, i32)> {
    let far2 = i64::from(far) * i64::from(far);
    (0..=far).flat_map(move |r| {
        let ring = move |dx: i32, dz: i32| (dx.abs() == r || dz.abs() == r) && i64::from(dx) * i64::from(dx) + i64::from(dz) * i64::from(dz) <= far2;
        (-r..=r).flat_map(move |dz| (-r..=r).map(move |dx| (dx, dz))).filter(move |&(dx, dz)| ring(dx, dz)).map(move |(dx, dz)| (cx + dx, cz + dz))
    })
}

/// What one player already holds. The walk hands out the nearest
/// records they lack; chunks without a record are not marked, so they
/// go out once surfaced.
#[derive(Default)]
pub struct SendTracker {
    sent: HashMap<(i32, i32), Level>,
    /// Store count up to which refreshed chunks have been forgotten from `sent`.
    seen: u64,
    /// Centre and store generation of a walk that found nothing to send.
    exhausted: Option<((i32, i32), u64)>,
}

impl SendTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append up to `max` entries for a player at chunk (cx, cz), nearest
    /// first within `far`: full records within `near`, coarse ones beyond;
    /// returns how many. Records more than 2 x far away are forgotten so a
    /// returning player gets them again.
    pub fn batch(&mut self, store: &LodStore, cx: i32, cz: i32, far: i32, near: i32, max: usize, out: &mut Vec<u8>) -> usize {
        let (refreshed, now) = store.refreshed_since(self.seen);
        for chunk in refreshed {
            self.sent.remove(&chunk);
        }
        self.seen = now;
        let generation = store.generation();
        if self.exhausted == Some(((cx, cz), generation)) {
            return 0;
        }
        let keep = far.saturating_mul(2);
        self.sent.retain(|&(x, z), _| (x - cx).abs() <= keep && (z - cz).abs() <= keep);
        let near2 = i64::from(near) * i64::from(near);
        let candidates = disc(cx, cz, far).map(|(x, z)| (x, z, i64::from(x - cx).pow(2) + i64::from(z - cz).pow(2) <= near2));
        let taken = store.take_batch(candidates, &self.sent, max, out);
        let count = taken.len();
        self.sent.extend(taken);
        if count == 0 {
            self.exhausted = Some(((cx, cz), generation));
        }
        count
    }

    /// The client already holds these chunks of a region in full; do not send them again.
    pub fn mark_have(&mut self, rx: i32, rz: i32, bitmap: &[u8]) {
        for slot in 0..CHUNKS_PER_REGION {
            if bitmap.get(slot / 8).is_some_and(|b| b & (1 << (slot % 8)) != 0) {
                self.sent.insert((rx * 32 + (slot % 32) as i32, rz * 32 + (slot / 32) as i32), Level::Full);
            }
        }
        self.exhausted = None;
    }

    pub fn sent_count(&self) -> usize {
        self.sent.len()
    }
}

/// The records a client received, bounded; the farthest from the last
/// centre go first when the bound is passed.
pub struct ClientStore {
    records: HashMap<(i32, i32), Box<ColumnLod>>,
    /// Bumped per 32x32 chunk region on every record that lands in it, so a mesh knows it is stale.
    generations: HashMap<(i32, i32), u64>,
    cap: usize,
    /// Region files under the client's own directory, written through and read back on approach.
    disk: Option<LodStore>,
    /// Regions whose file records have been loaded into memory.
    warmed: HashSet<(i32, i32)>,
    /// Outer-ring records at one cell per 4x4 columns; a full record replaces one.
    coarse: HashMap<(i32, i32), CoarseLod>,
}

/// Regions the client store keeps in memory from disk beyond the record map.
const CLIENT_DISK_REGIONS: usize = 64;

impl ClientStore {
    pub fn new(cap: usize) -> Self {
        Self { records: HashMap::new(), generations: HashMap::new(), cap: cap.max(1), disk: None, warmed: HashSet::new(), coarse: HashMap::new() }
    }

    /// Persist under `dir`, whose files are tagged with `hash` (the world id).
    pub fn open(&mut self, dir: &Path, hash: [u8; 16]) -> io::Result<()> {
        let store = LodStore::with_max_regions(hash, CLIENT_DISK_REGIONS);
        store.set_dir(dir)?;
        self.disk = Some(store);
        self.warmed.clear();
        Ok(())
    }

    /// Bring a region's file records into memory once; returns how many came in.
    pub fn warm(&mut self, rx: i32, rz: i32) -> usize {
        let Some(disk) = self.disk.as_ref() else {
            return 0;
        };
        if !self.warmed.insert((rx, rz)) {
            return 0;
        }
        let mut count = 0;
        for (x, z, record) in disk.region_records(rx, rz) {
            if let std::collections::hash_map::Entry::Vacant(e) = self.records.entry((x, z)) {
                e.insert(Box::new(record));
                count += 1;
            }
        }
        if count > 0 {
            *self.generations.entry((rx, rz)).or_insert(0) += count as u64;
        }
        count
    }

    /// One bit per chunk of a region the client holds in its final form, on disk or in memory;
    /// generator-surface records are left out so the finished ones replace them.
    pub fn have(&self, rx: i32, rz: i32) -> [u8; BITMAP_BYTES] {
        let mut out = self.disk.as_ref().map(|d| d.present_bitmap(rx, rz, true)).unwrap_or([0; BITMAP_BYTES]);
        for lz in 0..32 {
            for lx in 0..32 {
                if self.records.get(&(rx * 32 + lx, rz * 32 + lz)).is_some_and(|r| r.stage == STAGE_FINAL) {
                    let slot = (lx + lz * 32) as usize;
                    out[slot / 8] |= 1 << (slot % 8);
                }
            }
        }
        out
    }

    /// Write dirty regions; how many files were written.
    pub fn flush(&self) -> io::Result<usize> {
        self.disk.as_ref().map_or(Ok(0), |d| d.flush())
    }

    /// How many records have landed in a region since the store was cleared.
    pub fn region_generation(&self, rx: i32, rz: i32) -> u64 {
        self.generations.get(&(rx, rz)).copied().unwrap_or(0)
    }

    /// Take a batch as `batch` writes it; None leaves the store unchanged
    /// when the bytes are not whole entries. (cx, cz) is the player's chunk,
    /// the centre eviction measures from.
    pub fn put_batch(&mut self, bytes: &[u8], cx: i32, cz: i32) -> Option<usize> {
        // Check the whole batch before keeping any of it.
        let mut at = 0;
        let mut count = 0;
        while at < bytes.len() {
            let header = bytes.get(at..at + BATCH_HEADER_BYTES)?;
            let flags = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
            let body = if flags & BATCH_COARSE != 0 { COARSE_BYTES } else { RECORD_BYTES };
            bytes.get(at + BATCH_HEADER_BYTES..at + BATCH_HEADER_BYTES + body)?;
            at += BATCH_HEADER_BYTES + body;
            count += 1;
        }
        at = 0;
        while at < bytes.len() {
            let header = &bytes[at..at + BATCH_HEADER_BYTES];
            let x = i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let z = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            let flags = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
            at += BATCH_HEADER_BYTES;
            if flags & BATCH_COARSE != 0 {
                let record = CoarseLod::read_from(&bytes[at..])?;
                at += COARSE_BYTES;
                // A full record already held says more than this one.
                if !self.records.contains_key(&(x, z)) {
                    self.coarse.insert((x, z), record);
                    *self.generations.entry((x >> 5, z >> 5)).or_insert(0) += 1;
                }
                continue;
            }
            let mut record = ColumnLod::read_from(&bytes[at..])?;
            at += RECORD_BYTES;
            record.stage = flags as u8;
            if let Some(disk) = self.disk.as_ref() {
                let _ = disk.insert(x, z, record.clone());
            }
            self.coarse.remove(&(x, z));
            self.records.insert((x, z), Box::new(record));
            *self.generations.entry((x >> 5, z >> 5)).or_insert(0) += 1;
        }
        if self.records.len() > self.cap {
            let mut keys: Vec<(i32, i32)> = self.records.keys().copied().collect();
            keys.sort_by_key(|&(x, z)| std::cmp::Reverse(i64::from(x - cx).pow(2) + i64::from(z - cz).pow(2)));
            for key in keys.iter().take(self.records.len() - self.cap / 2) {
                self.records.remove(key);
                // Its region may be warmed again from disk when the player returns.
                self.warmed.remove(&(key.0 >> 5, key.1 >> 5));
            }
        }
        // Coarse records are 24x smaller, so many more fit; the farthest go first past the bound.
        let coarse_cap = self.cap * 8;
        if self.coarse.len() > coarse_cap {
            let mut keys: Vec<(i32, i32)> = self.coarse.keys().copied().collect();
            keys.sort_by_key(|&(x, z)| std::cmp::Reverse(i64::from(x - cx).pow(2) + i64::from(z - cz).pow(2)));
            for key in keys.iter().take(self.coarse.len() - coarse_cap / 2) {
                self.coarse.remove(key);
            }
        }
        Some(count)
    }

    /// Whatever the client holds for a chunk, the full record first.
    pub fn get_any(&self, cx: i32, cz: i32) -> Option<RecordRef<'_>> {
        if let Some(full) = self.records.get(&(cx, cz)) {
            return Some(RecordRef::Full(full));
        }
        self.coarse.get(&(cx, cz)).map(RecordRef::Coarse)
    }

    pub fn coarse_len(&self) -> usize {
        self.coarse.len()
    }

    pub fn get(&self, cx: i32, cz: i32) -> Option<&ColumnLod> {
        self.records.get(&(cx, cz)).map(|r| r.as_ref())
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.coarse.clear();
        self.generations.clear();
        let _ = self.flush();
        self.disk = None;
        self.warmed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seed: i16) -> ColumnLod {
        let mut r = ColumnLod::empty(STAGE_FINAL);
        for c in 0..COLUMNS {
            r.height[c] = seed.wrapping_mul(7).wrapping_add(c as i16) - 64;
            r.top[c] = (c as u16).wrapping_mul(3).wrapping_add(seed as u16) % 127;
            r.biome[c] = (c as u16).wrapping_add(seed as u16) % 64;
        }
        r
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("painite_lod_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trip_sparse_region_across_stores() {
        let dir = temp_dir("rt");
        let hash = [7u8; 16];
        let store = LodStore::new(hash);
        store.set_dir(&dir).unwrap();
        // Negative chunks land in region (-1, -1) at slots that are not the record index.
        let chunks = [(-1, -1), (-32, -32), (-17, -3), (5, 9), (31, 0)];
        for (i, &(x, z)) in chunks.iter().enumerate() {
            store.insert(x, z, record(i as i16)).unwrap();
        }
        assert_eq!(store.flush().unwrap(), 2);
        assert_eq!(store.flush().unwrap(), 0);
        assert!(dir.join("r.-1.-1.plod").exists() && dir.join("r.0.0.plod").exists());
        let size = fs::metadata(dir.join("r.-1.-1.plod")).unwrap().len() as usize;
        assert_eq!(size, HEADER_BYTES + 3 * RECORD_BYTES);

        let again = LodStore::new(hash);
        again.set_dir(&dir).unwrap();
        for (i, &(x, z)) in chunks.iter().enumerate() {
            assert_eq!(again.get(x, z), Some(record(i as i16)), "chunk {x},{z}");
        }
        assert_eq!(again.get(6, 9), None);
        assert_eq!(again.region_count(0, 0), 2);
        assert_eq!(again.get(5, 9).unwrap().to_ints().len(), COLUMNS * 3);

        let other = LodStore::new([8u8; 16]);
        other.set_dir(&dir).unwrap();
        assert_eq!(other.get(5, 9), None, "a foreign hash discards the file");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eviction_writes_the_oldest_region() {
        let dir = temp_dir("evict");
        let store = LodStore::new([1u8; 16]);
        store.set_dir(&dir).unwrap();
        for r in 0..=MAX_REGIONS as i32 {
            store.insert(r * 32, 0, record(r as i16)).unwrap();
        }
        assert!(dir.join("r.0.0.plod").exists(), "region 0 written on eviction");
        assert!(!dir.join("r.1.0.plod").exists());
        assert_eq!(store.get(0, 0), Some(record(0)), "evicted region reads back from disk");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_dir_keeps_records_in_memory() {
        let store = LodStore::new([0u8; 16]);
        store.insert(3, 4, record(1)).unwrap();
        assert_eq!(store.flush().unwrap(), 0);
        assert_eq!(store.get(3, 4), Some(record(1)));
    }
    #[test]
    fn batch_walks_nearest_first_and_skips_what_was_sent() {
        let store = LodStore::new([0; 16]);
        for x in -3..=3 {
            for z in -3..=3 {
                store.insert(x, z, record((x * 8 + z) as i16)).unwrap();
            }
        }
        let mut tracker = SendTracker::new();
        let mut out = Vec::new();
        assert_eq!(tracker.batch(&store, 0, 0, 2, 2, 1, &mut out), 1);
        assert_eq!(&out[..8], &[0u8; 8], "the player's own chunk goes first");
        assert_eq!(out.len(), BATCH_ENTRY_BYTES);
        out.clear();
        // The rest of the radius-2 disc: 13 chunks in a disc, one already sent.
        assert_eq!(tracker.batch(&store, 0, 0, 2, 2, 100, &mut out), 12);
        assert_eq!(tracker.sent_count(), 13);
        assert_eq!(tracker.batch(&store, 0, 0, 2, 2, 100, &mut out), 0, "nothing left at this centre");
        // A new record inside the disc is picked up without moving.
        store.insert(1, 0, record(99)).unwrap();
        assert_eq!(tracker.batch(&store, 0, 0, 2, 2, 100, &mut out), 0, "already sent; the store is just newer");
        // Moving far away forgets the old ones, so coming back resends.
        let mut far = Vec::new();
        assert_eq!(tracker.batch(&store, 40, 40, 2, 2, 100, &mut far), 0);
        assert_eq!(tracker.sent_count(), 0);
        assert_eq!(tracker.batch(&store, 0, 0, 2, 2, 100, &mut far), 13);
    }

    #[test]
    fn batch_leaves_holes_unmarked_until_surfaced() {
        let store = LodStore::new([0; 16]);
        let mut tracker = SendTracker::new();
        let mut out = Vec::new();
        assert_eq!(tracker.batch(&store, 5, 5, 1, 1, 10, &mut out), 0);
        store.insert(5, 6, record(3)).unwrap();
        assert_eq!(tracker.batch(&store, 5, 5, 1, 1, 10, &mut out), 1);
        let x = i32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        let z = i32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        assert_eq!((x, z), (5, 6));
    }

    #[test]
    fn refresh_finalizes_a_record_and_makes_senders_resend_it() {
        let store = LodStore::new([0; 16]);
        let mut surface = record(1);
        surface.stage = STAGE_SURFACE;
        store.insert(3, 4, surface).unwrap();
        let mut tracker = SendTracker::new();
        let mut out = Vec::new();
        assert_eq!(tracker.batch(&store, 3, 4, 1, 1, 8, &mut out), 1);
        assert_eq!(out[8], STAGE_SURFACE, "stage rides in the entry");
        assert_eq!(tracker.batch(&store, 3, 4, 1, 1, 8, &mut out), 0, "sent once");
        let heights = [100i16; COLUMNS];
        let tops = [4200u16; COLUMNS];
        store.refresh(3, 4, &heights, &tops, &[0; COLUMNS], &tops).unwrap();
        assert_eq!(store.stage(3, 4), Some(STAGE_FINAL));
        assert_eq!(store.get(3, 4).unwrap().biome[5], record(1).biome[5], "biomes kept");
        out.clear();
        assert_eq!(tracker.batch(&store, 3, 4, 1, 1, 8, &mut out), 1, "resent after the refresh");
        assert_eq!(out[8], STAGE_FINAL);
        let mut client = ClientStore::new(8);
        assert_eq!(client.put_batch(&out, 3, 4), Some(1));
        assert_eq!(client.get(3, 4).unwrap().top[0], 4200);
        assert_ne!(client.have(0, 0)[(3 + 4 * 32) / 8] & (1 << ((3 + 4 * 32) % 8)), 0, "final records are reported");
        store.refresh(9, 9, &heights, &tops, &[0; COLUMNS], &tops).unwrap();
        assert_eq!(store.get(9, 9).unwrap().biome[0], 0, "a chunk without a record gets one");
    }

    #[test]
    fn version_2_region_files_still_read_as_dry_records() {
        let hash = [7u8; 16];
        let mut region = RegionLod::empty();
        let mut rec = record(3);
        rec.depth[0] = 9;
        rec.floor[0] = 77;
        region.records[5] = Some(Box::new(rec.clone()));
        let v3 = region.encode(&hash);
        assert_eq!(v3.len(), HEADER_BYTES + RECORD_BYTES);
        let mut v2 = v3[..HEADER_BYTES + RECORD_BYTES_V2].to_vec();
        v2[4..6].copy_from_slice(&2u16.to_le_bytes());
        let old = RegionLod::parse(&v2, &hash).unwrap();
        let got = old.records[5].as_ref().unwrap();
        assert_eq!((got.height, got.top, got.biome), (rec.height, rec.top, rec.biome));
        assert_eq!(got.stage, STAGE_SURFACE, "an old record is refreshed on the next load");
        assert_eq!(got.depth, [0; COLUMNS], "no depth in a version 2 file");
        assert_eq!(got.floor, got.top);
        let new = RegionLod::parse(&v3, &hash).unwrap();
        assert_eq!(new.records[5].as_deref(), Some(&rec), "a version 3 file round-trips depth and floor");
        let coarse = CoarseLod::of(&rec);
        let mut bytes = Vec::new();
        coarse.write_to(&mut bytes);
        assert_eq!(bytes.len(), COARSE_BYTES);
        assert_eq!(CoarseLod::read_from(&bytes).unwrap(), coarse);
    }

    #[test]
    fn client_store_persists_and_reports_what_it_holds() {
        let dir = std::env::temp_dir().join(format!("painite_client_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut store = ClientStore::new(64);
        store.open(&dir, [3; 16]).unwrap();
        let mut batch = Vec::new();
        for (x, z) in [(1, 2), (40, 2)] {
            batch.extend_from_slice(&(x as i32).to_le_bytes());
            batch.extend_from_slice(&(z as i32).to_le_bytes());
            batch.extend_from_slice(&(STAGE_FINAL as u32).to_le_bytes());
            record(7).write_to(&mut batch);
        }
        assert_eq!(store.put_batch(&batch, 0, 0), Some(2));
        assert_eq!(store.flush().unwrap(), 2);
        let mut again = ClientStore::new(64);
        again.open(&dir, [3; 16]).unwrap();
        assert!(again.get(1, 2).is_none(), "not in memory before warming");
        let have = again.have(0, 0);
        assert_eq!(have[(1 + 2 * 32) / 8] & (1 << ((1 + 2 * 32) % 8)), 1 << ((1 + 2 * 32) % 8));
        assert_eq!(again.warm(0, 0), 1);
        assert_eq!(again.warm(0, 0), 0, "warmed once");
        assert_eq!(again.get(1, 2).map(|r| r.height[0]), Some(record(7).height[0]));
        assert_eq!(again.region_generation(0, 0), 1);
        let mut tracker = SendTracker::new();
        tracker.mark_have(0, 0, &have);
        assert_eq!(tracker.sent_count(), 1);
        let mut wrong = ClientStore::new(64);
        wrong.open(&dir, [4; 16]).unwrap();
        assert_eq!(wrong.warm(0, 0), 0, "another world's files are ignored");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_store_round_trips_a_batch_and_evicts_the_farthest() {
        let store = LodStore::new([0; 16]);
        for x in 0..10 {
            store.insert(x, 0, record(x as i16)).unwrap();
        }
        let mut tracker = SendTracker::new();
        let mut out = Vec::new();
        assert_eq!(tracker.batch(&store, 0, 0, 9, 9, 100, &mut out), 10);
        let mut client = ClientStore::new(8);
        assert_eq!(client.put_batch(&out[..7], 0, 0), None, "a torn batch is refused");
        assert!(client.is_empty());
        assert_eq!(client.put_batch(&out, 0, 0), Some(10));
        assert_eq!(client.len(), 4, "over the cap of 8 it keeps the nearest half");
        assert_eq!(client.get(0, 0), Some(&record(0)));
        assert_eq!(client.get(3, 0), Some(&record(3)));
        assert_eq!(client.get(9, 0), None);
        client.clear();
        assert!(client.is_empty());
    }

    #[test]
    fn outer_ring_goes_coarse_and_fills_in_on_approach() {
        let store = LodStore::new([0; 16]);
        for x in 0..8 {
            store.insert(x, 0, record(x as i16)).unwrap();
        }
        let mut tracker = SendTracker::new();
        let mut out = Vec::new();
        // Full within 2 chunks (x 0..=2), coarse out to 7.
        assert_eq!(tracker.batch(&store, 0, 0, 7, 2, 100, &mut out), 8);
        assert_eq!(out.len(), 3 * BATCH_ENTRY_BYTES + 5 * COARSE_ENTRY_BYTES);
        let mut client = ClientStore::new(64);
        assert_eq!(client.put_batch(&out, 0, 0), Some(8));
        assert_eq!(client.len(), 3);
        assert_eq!(client.coarse_len(), 5);
        let coarse = CoarseLod::of(&record(5));
        assert!(matches!(client.get_any(5, 0), Some(RecordRef::Coarse(c)) if *c == coarse));
        assert_eq!(coarse.height[0], record(5).height[3 + 3 * 16], "tallest of the 4x4 wins");
        assert_eq!(client.region_generation(0, 0), 8);
        assert_eq!(client.put_batch(&out[..out.len() - 1], 0, 0), None, "a torn coarse entry is refused");
        // The player moves to x 5: the coarse chunks 3..=7 are sent in full, nothing twice.
        out.clear();
        assert_eq!(tracker.batch(&store, 5, 0, 7, 2, 100, &mut out), 5);
        assert_eq!(out.len(), 5 * BATCH_ENTRY_BYTES);
        assert_eq!(client.put_batch(&out, 5, 0), Some(5));
        assert_eq!(client.len(), 8);
        assert_eq!(client.coarse_len(), 0, "full records replace coarse ones");
        assert_eq!(tracker.batch(&store, 5, 0, 7, 2, 100, &mut out), 0);
        // A coarse entry for a chunk held in full is ignored.
        let mut stale = Vec::new();
        stale.extend_from_slice(&1i32.to_le_bytes());
        stale.extend_from_slice(&0i32.to_le_bytes());
        stale.extend_from_slice(&BATCH_COARSE.to_le_bytes());
        CoarseLod::of(&record(9)).write_to(&mut stale);
        assert_eq!(client.put_batch(&stale, 5, 0), Some(1));
        assert_eq!(client.coarse_len(), 0);
        assert_eq!(client.get(1, 0), Some(&record(1)));
    }
}
