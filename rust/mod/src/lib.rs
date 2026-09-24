//! JNI surface for `me.apika.painite.PainiteNative`. Two calls per
//! chunk-stage; the arrangement logic lives in painite-sched.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use jni::JNIEnv;
use jni::objects::{JByteArray, JByteBuffer, JClass, JIntArray, JObjectArray, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jlongArray, jobjectArray};
use jni::objects::JLongArray;
use painite_sched::{Job, Limits, Scheduler, Stage, Ticket};
use painite_terrain::beard26::Beardifier;
use painite_terrain::lod26::{ClientStore, SendTracker};
use painite_terrain::lodmesh26;
use painite_terrain::state26::{TerrainState, Unsupported};

/// Run an entry point's body, returning `fallback` if it panics. A panic
/// crossing `extern "system"` aborts the JVM with no crash report or
/// save; caught here, the call declines like any other refusal and the
/// panic hook has already logged the message.
fn guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(fallback)
}

static SCHED: OnceLock<Scheduler> = OnceLock::new();

/// Returned when the scheduler is not initialised or the stage id is
/// unknown; Java then runs the body ungated.
const NO_TICKET: jlong = -1;

#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_init(
    _env: JNIEnv,
    _class: JClass,
    max_features: jint,
    max_inflight: jint,
) -> jint {
    guard(0, || {
        let limits = Limits {
            max_features: max_features.max(1) as usize,
            max_inflight: max_inflight.max(1) as usize,
        };
        let first = SCHED.set(Scheduler::new(limits)).is_ok();
        if first { 1 } else { 0 }
    })
}

fn stage_of(stage: jint) -> Option<Stage> {
    u8::try_from(stage).ok().and_then(Stage::from_u8)
}

/// Register a job. Returns `seq << 1 | granted`, or NO_TICKET when the
/// scheduler is unavailable (Java then runs the body ungated).
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_submit(
    _env: JNIEnv,
    _class: JClass,
    stage: jint,
    cx: jint,
    cz: jint,
    level: jint,
) -> jlong {
    guard(NO_TICKET, || {
        let (Some(sched), Some(stage)) = (SCHED.get(), stage_of(stage)) else {
            return NO_TICKET;
        };
        let r = sched.submit(Job { stage, cx, cz }, level);
        ((r.seq << 1) | u64::from(r.granted)) as jlong
    })
}

/// Free the job's zone. Returns the seqs of parked jobs that may run now.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_release(
    mut env: JNIEnv,
    _class: JClass,
    stage: jint,
    cx: jint,
    cz: jint,
    seq: jlong,
) -> jlongArray {
    guard(std::ptr::null_mut(), || {
        let empty = |env: &mut JNIEnv| env.new_long_array(0).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut());
        if seq < 0 {
            return empty(&mut env);
        }
        let (Some(sched), Some(stage)) = (SCHED.get(), stage_of(stage)) else {
            return empty(&mut env);
        };
        let freed = sched.release(Ticket::from_parts(Job { stage, cx, cz }, seq as u64));
        if freed.is_empty() {
            return empty(&mut env);
        }
        let vals: Vec<jlong> = freed.iter().map(|&s| s as jlong).collect();
        match env.new_long_array(vals.len() as i32) {
            Ok(arr) => {
                if env.set_long_array_region(&arr, 0, &vals).is_err() {
                    return std::ptr::null_mut();
                }
                arr.into_raw()
            }
            Err(_) => std::ptr::null_mut(),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_active(_env: JNIEnv, _class: JClass) -> jint {
    guard(-1, || {
        SCHED.get().map_or(-1, |s| s.active() as jint)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_waiting(_env: JNIEnv, _class: JClass) -> jint {
    guard(-1, || {
        SCHED.get().map_or(-1, |s| s.waiting() as jint)
    })
}

static TERRAIN: RwLock<Option<TerrainState>> = RwLock::new(None);

fn string_array(env: &mut JNIEnv, arr: &JObjectArray) -> Result<Vec<String>, jni::errors::Error> {
    let n = env.get_array_length(arr)?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let o = env.get_object_array_element(arr, i)?;
        let js = JString::from(o);
        out.push(env.get_string(&js)?.into());
    }
    Ok(out)
}

/// Compile a world's terrain from datapack documents. `kinds`, `ids` and
/// `bodies` are parallel arrays (`density_function`, `noise`,
/// `noise_settings`, `material_rule`, `material_condition` or `biome`;
/// full identifier; JSON text). Returns 1 when the native can serve
/// this world, 0 when it cannot (Java keeps the vanilla path), -1 on a
/// malformed call. `terrainPaletteFlags` must follow before any fill.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainInit(
    mut env: JNIEnv,
    _class: JClass,
    seed: jlong,
    biome_zoom_seed: jlong,
    settings_id: JString,
    kinds: JObjectArray,
    ids: JObjectArray,
    bodies: JObjectArray,
) -> jint {
    guard(0, || {
        let settings_id: String = match env.get_string(&settings_id) {
            Ok(s) => s.into(),
            Err(_) => return -1,
        };
        let (Ok(kinds), Ok(ids), Ok(bodies)) = (string_array(&mut env, &kinds), string_array(&mut env, &ids), string_array(&mut env, &bodies)) else {
            return -1;
        };
        if kinds.len() != ids.len() || ids.len() != bodies.len() {
            return -1;
        }
        let mut documents = Vec::with_capacity(bodies.len());
        for ((kind, id), body) in kinds.into_iter().zip(ids).zip(bodies) {
            match serde_json::from_str(&body) {
                Ok(v) => documents.push((kind, id, v)),
                Err(_) => return -1,
            }
        }
        match TerrainState::build(seed, biome_zoom_seed, &settings_id, documents) {
            Ok(state) => {
                *TERRAIN.write().unwrap() = Some(state);
                1
            }
            Err(Unsupported::Shape) | Err(Unsupported::Settings(_)) => {
                *TERRAIN.write().unwrap() = None;
                0
            }
            Err(Unsupported::Load(_)) => {
                *TERRAIN.write().unwrap() = None;
                0
            }
        }
    })
}

/// Drop the compiled terrain (world unload). Dirty far-view regions
/// are written first; a failure here is lost, so Java flushes before.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainClear(_env: JNIEnv, _class: JClass) {
    guard((), || {
        let mut guard = TERRAIN.write().unwrap();
        if let Some(state) = guard.as_ref() {
            let _ = state.lod.flush();
        }
        LOD_TRACKERS.lock().unwrap().clear();
        *guard = None;
    })
}

/// Turn far-view records on and name the directory they persist to. 1
/// on success, 0 without a compiled terrain, -1 when it cannot be
/// created (the records are then kept in memory only).
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLodDir(mut env: JNIEnv, _class: JClass, dir: JString) -> jint {
    guard(-1, || {
        let dir: String = match env.get_string(&dir) {
            Ok(s) => s.into(),
            Err(_) => return -1,
        };
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return 0;
        };
        state.keep_lod();
        match state.lod.set_dir(std::path::Path::new(&dir)) {
            Ok(()) => 1,
            Err(_) => -1,
        }
    })
}

/// A chunk's far-view record: 256 surface heights, 256 top palette
/// ids, 256 biome indices (`x + z * 16`), or null when the chunk was
/// never surfaced by the native.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLod(env: JNIEnv, _class: JClass, chunk_x: jint, chunk_z: jint) -> jni::sys::jintArray {
    guard(std::ptr::null_mut(), || {
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        match state.lod.get(chunk_x, chunk_z) {
            Some(record) => int_array(&env, &record.to_ints()),
            None => std::ptr::null_mut(),
        }
    })
}

/// Write every changed far-view region. Returns the count written, 0
/// without a directory or terrain, -1 on a write error.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLodFlush(_env: JNIEnv, _class: JClass) -> jint {
    guard(-1, || {
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return 0;
        };
        match state.lod.flush() {
            Ok(n) => n as jint,
            Err(_) => -1,
        }
    })
}

/// Per-player far-view send state, keyed by the player's entity id.
static LOD_TRACKERS: LazyLock<Mutex<HashMap<i64, SendTracker>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Records this client received; 32768 chunks is 48 MB at most.
static LOD_CLIENT: LazyLock<Mutex<ClientStore>> = LazyLock::new(|| Mutex::new(ClientStore::new(65536)));
/// Colour per palette id (r g b a), set by the client from the palette strings.
static LOD_COLOURS: LazyLock<Mutex<Vec<[u8; 4]>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Water's palette id and its surface colour per biome index.
static LOD_WATER: LazyLock<Mutex<(u16, Vec<[u8; 4]>)>> = LazyLock::new(|| Mutex::new((u16::MAX, Vec::new())));

/// Set the colour of every palette id as ARGB ints, in palette order.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientColours(env: JNIEnv, _class: JClass, argb: JIntArray) {
    guard((), || {
        let Some(values) = int_region(&env, &argb) else {
            return;
        };
        *LOD_COLOURS.lock().unwrap() = values.iter().map(|&c| [(c >> 16) as u8, (c >> 8) as u8, c as u8, (c >> 24) as u8]).collect();
    })
}

/// The water colour per biome index as ARGB, and the palette id water columns carry as their top.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientWaterColours(env: JNIEnv, _class: JClass, water: jint, argb: JIntArray) {
    guard((), || {
        let Some(values) = int_region(&env, &argb) else {
            return;
        };
        let id = if (0..=i32::from(u16::MAX)).contains(&water) { water as u16 } else { u16::MAX };
        *LOD_WATER.lock().unwrap() = (id, values.iter().map(|&c| [(c >> 16) as u8, (c >> 8) as u8, c as u8, (c >> 24) as u8]).collect());
    })
}

/// Persist the client store under `dir`, files tagged with the 16-byte world id; 1 on success.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientOpen(mut env: JNIEnv, _class: JClass, dir: JString, world_id: JByteArray) -> jint {
    guard(0, || {
        let Ok(dir) = env.get_string(&dir) else {
            return 0;
        };
        let dir: String = dir.into();
        let Ok(id) = env.convert_byte_array(&world_id) else {
            return 0;
        };
        let Ok(hash) = <[u8; 16]>::try_from(id.as_slice()) else {
            return 0;
        };
        match LOD_CLIENT.lock().unwrap().open(std::path::Path::new(&dir), hash) {
            Ok(()) => 1,
            Err(_) => 0,
        }
    })
}

/// Load a region's records from the client's files into memory once; how many came in.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientWarm(_env: JNIEnv, _class: JClass, rx: jint, rz: jint) -> jint {
    guard(0, || {
        LOD_CLIENT.lock().unwrap().warm(rx, rz) as jint
    })
}

/// One bit per chunk of a region the client holds (x + z * 32, low bit first), 128 bytes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientHave(env: JNIEnv, _class: JClass, rx: jint, rz: jint) -> jbyteArray {
    guard(std::ptr::null_mut(), || {
        let have = LOD_CLIENT.lock().unwrap().have(rx, rz);
        byte_array(&env, &have)
    })
}

/// Write the client's dirty region files; how many were written, -1 on an I/O error.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientFlush(_env: JNIEnv, _class: JClass) -> jint {
    guard(-1, || {
        match LOD_CLIENT.lock().unwrap().flush() {
            Ok(n) => n as jint,
            Err(_) => -1,
        }
    })
}

/// Replace a chunk's heights and top blocks from the finished chunk; 1 when kept, 0 without terrain.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLodRefresh(env: JNIEnv, _class: JClass, cx: jint, cz: jint, heights: JIntArray, tops: JIntArray, depths: JIntArray, floors: JIntArray) -> jint {
    guard(0, || {
        const COLUMNS: usize = painite_terrain::lod26::COLUMNS;
        let (Some(h), Some(t), Some(d), Some(f)) = (int_region(&env, &heights), int_region(&env, &tops), int_region(&env, &depths), int_region(&env, &floors)) else {
            return 0;
        };
        if h.len() != COLUMNS || t.len() != COLUMNS || d.len() != COLUMNS || f.len() != COLUMNS {
            return 0;
        }
        let mut heights = [0i16; COLUMNS];
        let mut tops = [0u16; COLUMNS];
        let mut depths = [0u8; COLUMNS];
        let mut floors = [0u16; COLUMNS];
        for i in 0..COLUMNS {
            heights[i] = h[i].clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            tops[i] = t[i].clamp(0, u16::MAX as i32) as u16;
            depths[i] = d[i].clamp(0, 255) as u8;
            floors[i] = f[i].clamp(0, u16::MAX as i32) as u16;
        }
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return 0;
        };
        match state.lod.refresh(cx, cz, &heights, &tops, &depths, &floors) {
            Ok(()) => 1,
            Err(_) => 0,
        }
    })
}

/// The stage of a chunk's record: 0 generator surface, 1 finished chunk, -1 none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLodStage(_env: JNIEnv, _class: JClass, cx: jint, cz: jint) -> jint {
    guard(-1, || {
        let guard = TERRAIN.read().unwrap();
        guard.as_ref().and_then(|s| s.lod.stage(cx, cz)).map_or(-1, |s| s as jint)
    })
}

/// A player reports the chunks of a region it already holds; they are not sent again.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLodHave(env: JNIEnv, _class: JClass, player: jlong, rx: jint, rz: jint, bitmap: JByteArray) {
    guard((), || {
        let Ok(bits) = env.convert_byte_array(&bitmap) else {
            return;
        };
        LOD_TRACKERS.lock().unwrap().entry(player).or_default().mark_have(rx, rz, &bits);
    })
}

/// Records that have landed in region (rx, rz) since the store was cleared.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientRegionGeneration(_env: JNIEnv, _class: JClass, rx: jint, rz: jint) -> jlong {
    guard(0, || {
        LOD_CLIENT.lock().unwrap().region_generation(rx, rz) as jlong
    })
}

/// Position-colour vertices (16 bytes each) of region (rx, rz), each 8x8-chunk block at the
/// scale for its distance from the player's chunk (px, pz), relative to the region's block
/// corner, written into the direct buffer `dst` when they fit; `chunk_vertices` (1024 ints,
/// x + z * 32) gets each chunk's vertex count. Returns the byte length in the low 32 bits
/// (0 when there is nothing to draw), then the lowest and highest y as 16-bit values; a
/// length past the buffer's capacity means nothing was written.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientMeshRegion(env: JNIEnv, _class: JClass, rx: jint, rz: jint, px: jint, pz: jint, dst: JByteBuffer, chunk_vertices: JIntArray) -> jlong {
    guard(0, || {
        let colours = LOD_COLOURS.lock().unwrap().clone();
        let (water, water_by_biome) = LOD_WATER.lock().unwrap().clone();
        let palette = lodmesh26::Palette { colours: &colours, water, water_by_biome: &water_by_biome };
        let mut out = Vec::new();
        let mut counts = Vec::new();
        let bounds = lodmesh26::mesh_region(&LOD_CLIENT.lock().unwrap(), rx, rz, |cx, cz| lodmesh26::scale_for(cx, cz, px, pz), (0, 0, -1), &palette, &mut out, &mut counts);
        if out.is_empty() {
            return 0;
        }
        if let (Ok(address), Ok(capacity)) = (env.get_direct_buffer_address(&dst), env.get_direct_buffer_capacity(&dst))
            && capacity >= out.len()
        {
            unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), address, out.len()) };
            let ints: Vec<i32> = counts.iter().map(|&c| c as i32).collect();
            if env.set_int_array_region(&chunk_vertices, 0, &ints).is_err() {
                return 0;
            }
        }
        (out.len() as jlong) | ((bounds.min_y as u16 as jlong) << 32) | ((bounds.max_y as u16 as jlong) << 48)
    })
}

/// The next far-view batch for a player at chunk (cx, cz): up to `max`
/// entries of chunk x, chunk z, flags and record, nearest first within `far`;
/// full records within `near` chunks, coarse ones beyond.
/// Null when nothing is due or no terrain is compiled.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLodBatch(env: JNIEnv, _class: JClass, player: jlong, cx: jint, cz: jint, far: jint, near: jint, max: jint) -> jbyteArray {
    guard(std::ptr::null_mut(), || {
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        let mut out = Vec::new();
        let count = {
            let mut trackers = LOD_TRACKERS.lock().unwrap();
            trackers.entry(player).or_default().batch(&state.lod, cx, cz, far.max(0), near.max(0), max.max(0) as usize, &mut out)
        };
        if count == 0 {
            return std::ptr::null_mut();
        }
        byte_array(&env, &out)
    })
}

/// Drop a player's send state; the next batch starts over.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainLodForget(_env: JNIEnv, _class: JClass, player: jlong) {
    guard((), || {
        LOD_TRACKERS.lock().unwrap().remove(&player);
    })
}

/// Keep a received batch on the client; (cx, cz) is the player's chunk.
/// Returns the count taken, -1 for a torn batch.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientPut(env: JNIEnv, _class: JClass, batch: JByteArray, cx: jint, cz: jint) -> jint {
    guard(-1, || {
        let Ok(bytes) = env.convert_byte_array(&batch) else {
            return -1;
        };
        match LOD_CLIENT.lock().unwrap().put_batch(&bytes, cx, cz) {
            Some(n) => n as jint,
            None => -1,
        }
    })
}

/// A received record as ints (heights, tops, biomes) or null.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientGet(env: JNIEnv, _class: JClass, cx: jint, cz: jint) -> jni::sys::jintArray {
    guard(std::ptr::null_mut(), || {
        let store = LOD_CLIENT.lock().unwrap();
        match store.get(cx, cz) {
            Some(record) => int_array(&env, &record.to_ints()),
            None => std::ptr::null_mut(),
        }
    })
}

/// How many chunk records the client holds.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientCount(_env: JNIEnv, _class: JClass) -> jint {
    guard(0, || {
        let store = LOD_CLIENT.lock().unwrap();
        (store.len() + store.coarse_len()) as jint
    })
}

/// Drop every received record, on leaving a world.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_lodClientClear(_env: JNIEnv, _class: JClass) {
    guard((), || {
        LOD_CLIENT.lock().unwrap().clear();
    })
}

/// The block states the compiled terrain can place, one canonical
/// `{"id":..,"properties":..}` JSON text per palette id, or null when
/// no terrain is compiled.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainPalette(mut env: JNIEnv, _class: JClass) -> jobjectArray {
    guard(std::ptr::null_mut(), || {
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        let names = &state.surface.palette.names;
        let Ok(string_class) = env.find_class("java/lang/String") else {
            return std::ptr::null_mut();
        };
        let Ok(arr) = env.new_object_array(names.len() as i32, string_class, JString::default()) else {
            return std::ptr::null_mut();
        };
        for (i, name) in names.iter().enumerate() {
            let Ok(js) = env.new_string(name) else {
                return std::ptr::null_mut();
            };
            if env.set_object_array_element(&arr, i as i32, js).is_err() {
                return std::ptr::null_mut();
            }
        }
        arr.into_raw()
    })
}

/// Per-palette-id flags (1 air, 2 fluid, 4 blocks motion in the
/// heightmap), same order as `terrainPalette`. Returns 1 on success,
/// 0 when the count does not match or no terrain is compiled.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainPaletteFlags(env: JNIEnv, _class: JClass, flags: JByteArray) -> jint {
    guard(0, || {
        let Ok(n) = env.get_array_length(&flags) else {
            return 0;
        };
        let mut buf = vec![0i8; n as usize];
        if env.get_byte_array_region(&flags, 0, &mut buf).is_err() {
            return 0;
        }
        let unsigned: Vec<u8> = buf.into_iter().map(|b| b as u8).collect();
        let mut guard = TERRAIN.write().unwrap();
        let Some(state) = guard.as_mut() else {
            return 0;
        };
        match state.surface.palette.set_flags(&unsigned) {
            Ok(()) => 1,
            Err(_) => 0,
        }
    })
}

fn byte_array(env: &JNIEnv, bytes: &[u8]) -> jbyteArray {
    let signed: &[i8] = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const i8, bytes.len()) };
    match env.new_byte_array(bytes.len() as i32) {
        Ok(arr) => {
            let arr: JByteArray = arr;
            if env.set_byte_array_region(&arr, 0, signed).is_err() {
                return std::ptr::null_mut();
            }
            arr.into_raw()
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Fill one chunk column and keep it for `terrainSurface` or
/// `terrainTake`. `beard` is the structure pieces near the chunk in the
/// `beard26` flat layout, or null. Returns 1 when a fill is pending, 0
/// when no terrain is compiled, -1 when the pieces are malformed (the
/// chunk is left to vanilla).
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainFill(env: JNIEnv, _class: JClass, chunk_x: jint, chunk_z: jint, beard: JIntArray) -> jint {
    guard(0, || {
        let beard = if beard.is_null() {
            None
        } else {
            let Ok(n) = env.get_array_length(&beard) else {
                return -1;
            };
            let mut flat = vec![0i32; n as usize];
            if env.get_int_array_region(&beard, 0, &mut flat).is_err() {
                return -1;
            }
            match Beardifier::from_flat(&flat) {
                Some(b) => Some(b),
                None => return -1,
            }
        };
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return 0;
        };
        state.fill_pending(chunk_x, chunk_z, beard.as_ref());
        1
    })
}

/// Run the surface pass on the pending fill of a chunk. `quarts` holds
/// biome indices (palette order of the biome documents) for the 6x6
/// quart columns around the chunk, every quart layer, indexed
/// `y + (x + z * 6) * (height / 4)`, or is null to use the biome output
/// the native kept for the chunk and its neighbours. Returns one byte
/// per block in `y + (x + z * 16) * height` order: palette id in the
/// low 7 bits, bit 7 = mark for post-processing; null when nothing is
/// pending or no grid is usable (the fill stays pending: hand a grid
/// over or take it).
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainSurface(
    env: JNIEnv,
    _class: JClass,
    chunk_x: jint,
    chunk_z: jint,
    quarts: JIntArray,
) -> jbyteArray {
    guard(std::ptr::null_mut(), || {
        let Ok(quarts) = quart_region(&env, &quarts) else {
            return std::ptr::null_mut();
        };
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        match state.surface(chunk_x, chunk_z, quarts) {
            Some(bytes) => byte_array(&env, &bytes),
            None => std::ptr::null_mut(),
        }
    })
}

fn long_array(env: &JNIEnv, words: &[i64]) -> jlongArray {
    match env.new_long_array(words.len() as i32) {
        Ok(arr) => {
            let arr: JLongArray = arr;
            if env.set_long_array_region(&arr, 0, words).is_err() {
                return std::ptr::null_mut();
            }
            arr.into_raw()
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// `terrainSurface` returning the chunk as packed sections, heightmap
/// raw data and post-processing positions (layout in
/// `painite_terrain::state26::pack_chunk`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainSurfacePacked(
    env: JNIEnv,
    _class: JClass,
    chunk_x: jint,
    chunk_z: jint,
    quarts: JIntArray,
    carve: jboolean,
) -> jlongArray {
    guard(std::ptr::null_mut(), || {
        let Ok(quarts) = quart_region(&env, &quarts) else {
            return std::ptr::null_mut();
        };
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        match state.surface_packed(chunk_x, chunk_z, quarts, carve != 0) {
            Some(words) => long_array(&env, &words),
            None => std::ptr::null_mut(),
        }
    })
}

/// 1 when the compiled terrain can run the carvers, else 0.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainCarvers(_env: JNIEnv, _class: JClass) -> jint {
    guard(0, || {
        let guard = TERRAIN.read().unwrap();
        guard.as_ref().is_some_and(|s| s.has_carvers()) as jint
    })
}

/// Carve totals since load: chunks, mask ns, apply ns, blocks carved,
/// aquifer calls, top material calls.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainCarveStats(env: JNIEnv, _class: JClass) -> jlongArray {
    guard(std::ptr::null_mut(), || {
        let t = *painite_terrain::state26::CARVE_TOTALS.lock().unwrap();
        let words = [t.chunks as i64, t.mask_ns as i64, t.apply_ns as i64, t.carved as i64, t.aquifer_calls as i64, t.top_material_calls as i64];
        long_array(&env, &words)
    })
}

/// `terrainTake` in the packed layout.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainTakePacked(env: JNIEnv, _class: JClass, chunk_x: jint, chunk_z: jint) -> jlongArray {
    guard(std::ptr::null_mut(), || {
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        match state.take_fill_packed(chunk_x, chunk_z) {
            Some(words) => long_array(&env, &words),
            None => std::ptr::null_mut(),
        }
    })
}

/// The biome stage for a chunk: one biome index (order of the biome
/// documents) per quart, `y + (x + z * 4) * (height / 4)`, or null when
/// no terrain or no biome parameters are compiled.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainBiomes(env: JNIEnv, _class: JClass, chunk_x: jint, chunk_z: jint) -> jni::sys::jintArray {
    guard(std::ptr::null_mut(), || {
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        let Some(biomes) = state.chunk_biomes(chunk_x, chunk_z) else {
            return std::ptr::null_mut();
        };
        let ints: Vec<i32> = biomes.into_iter().map(i32::from).collect();
        match env.new_int_array(ints.len() as i32) {
            Ok(arr) => {
                let arr: JIntArray = arr;
                if env.set_int_array_region(&arr, 0, &ints).is_err() {
                    return std::ptr::null_mut();
                }
                arr.into_raw()
            }
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// The pending fill of a chunk without a surface pass, encoded like
/// `terrainSurface`, or null when nothing is pending.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainTake(env: JNIEnv, _class: JClass, chunk_x: jint, chunk_z: jint) -> jbyteArray {
    guard(std::ptr::null_mut(), || {
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        match state.take_fill(chunk_x, chunk_z) {
            Some(bytes) => byte_array(&env, &bytes),
            None => std::ptr::null_mut(),
        }
    })
}

/// A quart grid handed over by the game, or None for a null array
/// (the native grid). Err on a JNI failure or an out-of-range index.
fn quart_region(env: &JNIEnv, arr: &JIntArray) -> Result<Option<Vec<u16>>, ()> {
    if arr.is_null() {
        return Ok(None);
    }
    let ints = int_region(env, arr).ok_or(())?;
    ints.into_iter().map(|q| u16::try_from(q).map_err(|_| ())).collect::<Result<Vec<u16>, ()>>().map(Some)
}

fn int_region(env: &JNIEnv, arr: &JIntArray) -> Option<Vec<i32>> {
    let n = env.get_array_length(arr).ok()?;
    let mut buf = vec![0i32; n as usize];
    env.get_int_array_region(arr, 0, &mut buf).ok()?;
    Some(buf)
}

fn long_region(env: &JNIEnv, arr: &JLongArray) -> Option<Vec<i64>> {
    let n = env.get_array_length(arr).ok()?;
    let mut buf = vec![0i64; n as usize];
    env.get_long_array_region(arr, 0, &mut buf).ok()?;
    Some(buf)
}

fn int_array(env: &JNIEnv, ints: &[i32]) -> jni::sys::jintArray {
    match env.new_int_array(ints.len() as i32) {
        Ok(arr) => {
            let arr: JIntArray = arr;
            if env.set_int_array_region(&arr, 0, ints).is_err() {
                return std::ptr::null_mut();
            }
            arr.into_raw()
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Placed ore feature ids the native serves, in index order, or null
/// when no terrain or no ore stage is compiled.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainOrePlaced(mut env: JNIEnv, _class: JClass) -> jobjectArray {
    guard(std::ptr::null_mut(), || {
        let guard = TERRAIN.read().unwrap();
        let Some(ids) = guard.as_ref().and_then(TerrainState::ore_placed_ids) else {
            return std::ptr::null_mut();
        };
        let Ok(string_class) = env.find_class("java/lang/String") else {
            return std::ptr::null_mut();
        };
        let Ok(arr) = env.new_object_array(ids.len() as i32, string_class, JString::default()) else {
            return std::ptr::null_mut();
        };
        for (i, id) in ids.iter().enumerate() {
            let Ok(js) = env.new_string(id) else {
                return std::ptr::null_mut();
            };
            if env.set_object_array_element(&arr, i as i32, js).is_err() {
                return std::ptr::null_mut();
            }
        }
        arr.into_raw()
    })
}

/// Plan an ore batch: keeps it and returns `lo | hi << 16`, the
/// inclusive section range (from the bottom) the game must hand to
/// `terrainOreApply`, or -1 when the batch is rejected.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainOrePlan(
    env: JNIEnv,
    _class: JClass,
    chunk_x: jint,
    chunk_z: jint,
    seeds: JLongArray,
    placed: JIntArray,
    quarts: JIntArray,
) -> jint {
    guard(-1, || {
        let (Some(seeds), Some(placed), Ok(quarts)) = (long_region(&env, &seeds), int_region(&env, &placed), quart_region(&env, &quarts)) else {
            return -1;
        };
        let Ok(placed): Result<Vec<u16>, _> = placed.into_iter().map(u16::try_from).collect() else {
            return -1;
        };
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return -1;
        };
        match state.ore_plan(chunk_x, chunk_z, seeds, placed, quarts) {
            Some((lo, hi)) => (lo as i32) | (hi as i32) << 16,
            None => -1,
        }
    })
}

/// Run a planned ore batch over the handed-over sections; returns two
/// ints per write (`slot | section << 4 | packed << 16`, palette id)
/// or null when nothing was planned or the data is malformed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_me_apika_painite_PainiteNative_terrainOreApply(
    env: JNIEnv,
    _class: JClass,
    chunk_x: jint,
    chunk_z: jint,
    heights: JLongArray,
    meta: JIntArray,
    palettes: JIntArray,
    storage: JLongArray,
) -> jni::sys::jintArray {
    guard(std::ptr::null_mut(), || {
        let (Some(heights), Some(meta), Some(palettes), Some(storage)) =
            (long_region(&env, &heights), int_region(&env, &meta), int_region(&env, &palettes), long_region(&env, &storage))
        else {
            return std::ptr::null_mut();
        };
        let guard = TERRAIN.read().unwrap();
        let Some(state) = guard.as_ref() else {
            return std::ptr::null_mut();
        };
        match state.ore_apply(chunk_x, chunk_z, &heights, &meta, &palettes, &storage) {
            Some(writes) => int_array(&env, &writes),
            None => std::ptr::null_mut(),
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn guard_turns_a_panic_into_the_fallback() {
        assert_eq!(super::guard(-1, || -> i32 { panic!("boom") }), -1);
        assert_eq!(super::guard(-1, || 7), 7);
    }
}
