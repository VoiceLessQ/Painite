//! Far-view mesh from column records: coarse cells by scale, one top quad
//! per run of equal cells along x, side quads where a neighbouring cell is
//! lower. Output is a flat vertex stream in the game's position-colour
//! layout, four vertices per quad in the game's quad order (the caller
//! draws with the shared sequential quad index buffer), `VERTEX_BYTES` each.
use crate::lod26::{COARSE_CELLS, COARSE_SIDE, COLUMNS, ClientStore, RecordRef};

/// Bytes per vertex: x, y, z as f32 (x and z relative to the mesh origin, y absolute), then r g b a.
pub const VERTEX_BYTES: usize = 16;
/// Colour scale per face, the game's own top / x side / z side shading.
const SHADE: [u8; 3] = [255, 153, 204];
const SHADE_TOP: usize = 0;
const SHADE_X: usize = 1;
const SHADE_Z: usize = 2;

/// Blocks a skirt drops below the chunk's lowest cell.
const SKIRT_DEPTH: i16 = 8;
/// Neighbour records with the scale they are drawn at, in the order -x, +x, -z, +z; a missing one gets a skirt wall.
pub type Neighbours<'a> = [Option<(RecordRef<'a>, usize)>; 4];

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    height: i16,
    colour: [u8; 4],
}

/// A chunk as n x n cells of 16 / n blocks.
struct Grid {
    n: usize,
    cells: Vec<Cell>,
}

impl Grid {
    fn at(&self, x: usize, z: usize) -> Cell {
        self.cells[x + z * self.n]
    }

    fn size(&self) -> usize {
        16 / self.n
    }
}

/// Mesh tops sit this far under the block face, so a real chunk drawn at the same
/// place always covers them (the game's water surface is a ninth of a block down).
const UNDER: f32 = 0.125;

/// Colours the mesh draws with: one per palette id (r g b a), and for water one per
/// biome (the surface tinted for that biome, alpha the texture's) used when the top
/// block is `water`.
#[derive(Clone, Copy)]
pub struct Palette<'a> {
    pub colours: &'a [[u8; 4]],
    pub water: u16,
    pub water_by_biome: &'a [[u8; 4]],
}

impl<'a> Palette<'a> {
    pub fn plain(colours: &'a [[u8; 4]]) -> Self {
        Self { colours, water: u16::MAX, water_by_biome: &[] }
    }
}

fn colour_of(colours: &[[u8; 4]], top: u16) -> [u8; 4] {
    colours.get(top as usize).copied().unwrap_or([255, 0, 255, 255])
}

/// A column's colour: its top block, or for water the surface blended over the floor
/// as the game shows it, the floor lit by the sky light left after `depth` blocks of
/// water (one level per block) through the lightmap's curve at the default brightness
/// setting, relative to the open-sky light the vertex already carries.
fn cell_colour(palette: &Palette, top: u16, depth: u8, floor: u16, biome: u16) -> [u8; 4] {
    let colours = palette.colours;
    let water = if top == palette.water { palette.water_by_biome.get(biome as usize).copied().unwrap_or_else(|| colour_of(colours, top)) } else { colour_of(colours, top) };
    if depth == 0 {
        return [water[0], water[1], water[2], 255];
    }
    let floor = colour_of(colours, floor);
    let light = f32::from(15u8.saturating_sub(depth)) / 15.0;
    let curved = light / (4.0 - 3.0 * light);
    // lightmap.fsh: mix(color, notGamma(color), BrightnessFactor), the slider's default 0.5.
    let lit = 0.5 * curved + 0.5 * (1.0 - (1.0 - curved).powi(4));
    let a = f32::from(water[3]) / 255.0;
    let mix = |w: u8, f: u8| (f32::from(w) * a + f32::from(f) * lit * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
    [mix(water[0], floor[0]), mix(water[1], floor[1]), mix(water[2], floor[2]), 255]
}

/// The tallest column per cell of a record at `scale` blocks per cell; a coarse
/// record never gets finer than its own 4-block cells.
fn grid(rec: RecordRef, scale: usize, palette: &Palette) -> Grid {
    match rec {
        RecordRef::Full(rec) => {
            let n = 16 / scale;
            let mut cells = vec![Cell { height: i16::MIN, colour: [0; 4] }; n * n];
            for c in 0..COLUMNS {
                let (x, z) = (c % 16, c / 16);
                let cell = &mut cells[x / scale + (z / scale) * n];
                if rec.height[c] > cell.height {
                    *cell = Cell { height: rec.height[c], colour: cell_colour(palette, rec.top[c], rec.depth[c], rec.floor[c], rec.biome[c]) };
                }
            }
            Grid { n, cells }
        }
        RecordRef::Coarse(rec) => {
            let n = (16 / scale).min(COARSE_SIDE);
            let per = COARSE_SIDE / n;
            let mut cells = vec![Cell { height: i16::MIN, colour: [0; 4] }; n * n];
            for c in 0..COARSE_CELLS {
                let (x, z) = (c % COARSE_SIDE, c / COARSE_SIDE);
                let cell = &mut cells[x / per + (z / per) * n];
                if rec.height[c] > cell.height {
                    *cell = Cell { height: rec.height[c], colour: cell_colour(palette, rec.top[c], rec.depth[c], rec.floor[c], rec.biome[c]) };
                }
            }
            Grid { n, cells }
        }
    }
}

/// Heights of the cells a neighbour draws along the edge facing us, resampled to our
/// `n` cells: the lowest drawn cell across each of ours, so a wall down to it never
/// leaves a gap whatever scale the neighbour is drawn at. None without a neighbour.
fn edge(rec: Option<(RecordRef, usize)>, side: usize, n: usize) -> Option<Vec<i16>> {
    let (rec, their_scale) = rec?;
    // The neighbour's own edge cells, at the size it is drawn.
    let (g, cell): (usize, usize) = match rec {
        RecordRef::Full(_) => (16 / their_scale, their_scale),
        RecordRef::Coarse(_) => {
            let g = (16 / their_scale).min(COARSE_SIDE);
            (g, 16 / g)
        }
    };
    let mut theirs = vec![i16::MIN; g];
    match rec {
        RecordRef::Full(rec) => {
            for along in 0..16 {
                for across in 0..cell {
                    let (x, z) = match side {
                        0 => (15 - across, along),
                        1 => (across, along),
                        2 => (along, 15 - across),
                        _ => (along, across),
                    };
                    let slot = &mut theirs[along / cell];
                    *slot = (*slot).max(rec.height[x + z * 16]);
                }
            }
        }
        RecordRef::Coarse(rec) => {
            let per = COARSE_SIDE / g;
            for (along, slot) in theirs.iter_mut().enumerate() {
                for i in 0..per {
                    let a = along * per + i;
                    for j in 0..per {
                        let d = if side.is_multiple_of(2) { COARSE_SIDE - 1 - j } else { j };
                        let (x, z) = if side < 2 { (d, a) } else { (a, d) };
                        *slot = (*slot).max(rec.height[x + z * COARSE_SIDE]);
                    }
                }
            }
        }
    }
    let size = 16 / n;
    let mut out = vec![i16::MAX; n];
    for (i, &h) in theirs.iter().enumerate() {
        let (b0, b1) = (i * cell, (i + 1) * cell);
        for slot in &mut out[b0 / size..b1.div_ceil(size)] {
            *slot = (*slot).min(h);
        }
    }
    Some(out)
}

/// Lowest and highest y written so far, kept next to the vertex stream.
#[derive(Clone, Copy)]
pub struct Bounds {
    pub min_y: i16,
    pub max_y: i16,
}

impl Bounds {
    pub const EMPTY: Self = Self { min_y: i16::MAX, max_y: i16::MIN };

    fn take(&mut self, y: i16) {
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }
}

fn push(out: &mut Vec<u8>, origin: (f32, f32), p: (i16, i16, i16), shade: usize, colour: [u8; 4]) {
    out.extend_from_slice(&(origin.0 + p.0 as f32).to_le_bytes());
    out.extend_from_slice(&(p.1 as f32 - UNDER).to_le_bytes());
    out.extend_from_slice(&(origin.1 + p.2 as f32).to_le_bytes());
    let k = SHADE[shade] as u32;
    out.extend_from_slice(&[((colour[0] as u32 * k) / 255) as u8, ((colour[1] as u32 * k) / 255) as u8, ((colour[2] as u32 * k) / 255) as u8, colour[3]]);
}

/// The quad a b c d (counter-clockwise seen from outside), four vertices.
fn quad(out: &mut Vec<u8>, origin: (f32, f32), p: [(i16, i16, i16); 4], shade: usize, colour: [u8; 4]) {
    for corner in p {
        push(out, origin, corner, shade, colour);
    }
}

/// A run of equal cells along x that is still growing down z.
struct Open {
    x0: usize,
    x1: usize,
    z0: usize,
    cell: Cell,
}

/// Mesh one chunk at `scale` (1, 2, 4, 8, 16) with `colours` indexed by palette id (r g b a);
/// `origin` is the chunk's block corner relative to the mesh origin. Top quads merge
/// along x and then down z where the runs line up exactly.
pub fn mesh_chunk(rec: RecordRef, neighbours: &Neighbours, scale: usize, palette: &Palette, origin: (f32, f32), out: &mut Vec<u8>, bounds: &mut Bounds) {
    let g = grid(rec, scale, palette);
    let n = g.n;
    let s = g.size() as i16;
    let edges: [Option<Vec<i16>>; 4] = std::array::from_fn(|side| edge(neighbours[side], side, n));
    // A missing neighbour gets a skirt: a wall down to just under this chunk's lowest cell, so an
    // edge with no record beyond it never shows as a floating slab.
    let skirt = g.cells.iter().map(|c| c.height).min().unwrap_or(0).saturating_sub(SKIRT_DEPTH);
    // Height of the cell beyond (x, z) in direction d.
    let beyond = |x: isize, z: isize, d: usize| -> i16 {
        if x < 0 || z < 0 || x >= n as isize || z >= n as isize {
            let along = if d < 2 { z } else { x } as usize;
            edges[d].as_ref().map_or(skirt, |e| e[along])
        } else {
            g.at(x as usize, z as usize).height
        }
    };
    let top = |out: &mut Vec<u8>, bounds: &mut Bounds, o: &Open, z1: usize| {
        let (x0, x1, z0, z1) = ((o.x0 as i16) * s, (o.x1 as i16) * s, (o.z0 as i16) * s, (z1 as i16) * s);
        let h = o.cell.height;
        bounds.take(h);
        quad(out, origin, [(x0, h, z0), (x0, h, z1), (x1, h, z1), (x1, h, z0)], SHADE_TOP, o.cell.colour);
    };
    let mut open: Vec<Open> = Vec::new();
    for z in 0..n {
        let mut row: Vec<Open> = Vec::new();
        let mut x = 0;
        while x < n {
            let cell = g.at(x, z);
            let mut run = 1;
            while x + run < n && g.at(x + run, z) == cell {
                run += 1;
            }
            // The same run directly above keeps growing; anything else starts here.
            match open.iter().position(|o| o.x0 == x && o.x1 == x + run && o.cell == cell) {
                Some(i) => row.push(open.swap_remove(i)),
                None => row.push(Open { x0: x, x1: x + run, z0: z, cell }),
            }
            x += run;
        }
        for o in open.drain(..) {
            top(out, bounds, &o, z);
        }
        open = row;
    }
    for o in open.drain(..) {
        top(out, bounds, &o, n);
    }
    // Side quads per cell, merged along the edge they lie on.
    for d in 0..4 {
        let (dx, dz): (isize, isize) = [(-1, 0), (1, 0), (0, -1), (0, 1)][d];
        for a in 0..n {
            let mut b = 0;
            while b < n {
                let (x, z) = if d < 2 { (a, b) } else { (b, a) };
                let cell = g.at(x, z);
                let low = beyond(x as isize + dx, z as isize + dz, d);
                if low >= cell.height {
                    b += 1;
                    continue;
                }
                let mut run = 1;
                while b + run < n {
                    let (nx, nz) = if d < 2 { (a, b + run) } else { (b + run, a) };
                    let same = g.at(nx, nz) == cell && beyond(nx as isize + dx, nz as isize + dz, d) == low;
                    if !same {
                        break;
                    }
                    run += 1;
                }
                let (b0, b1) = ((b as i16) * s, ((b + run) as i16) * s);
                let a0 = (a as i16) * s;
                let a1 = a0 + s;
                let p = match d {
                    0 => [(a0, low, b0), (a0, cell.height, b0), (a0, cell.height, b1), (a0, low, b1)],
                    1 => [(a1, low, b1), (a1, cell.height, b1), (a1, cell.height, b0), (a1, low, b0)],
                    2 => [(b1, low, a0), (b1, cell.height, a0), (b0, cell.height, a0), (b0, low, a0)],
                    _ => [(b0, low, a1), (b0, cell.height, a1), (b1, cell.height, a1), (b1, low, a1)],
                };
                bounds.take(low);
                quad(out, origin, p, if d < 2 { SHADE_X } else { SHADE_Z }, cell.colour);
                b += run;
            }
        }
    }
}

/// Chunks per region side; a region mesh's origin is its block corner.
pub const REGION: i32 = 32;
/// Chunks per side of the blocks a region's scale is chosen for.
pub const SCALE_BLOCK: i32 = 8;

/// Cell size for the 8x8-chunk block holding chunk (cx, cz), from the block's nearest
/// chunk distance to the player's chunk: 1 within 24 chunks, 2 within 48, 4 within 96,
/// 8 within 192, 16 beyond, so a cell stays about a block per 24 chunks of distance.
/// The Java side packs the same choice into its mesh key.
pub fn scale_for(cx: i32, cz: i32, px: i32, pz: i32) -> usize {
    let near = |c: i32, p: i32| {
        let b0 = c.div_euclid(SCALE_BLOCK) * SCALE_BLOCK;
        (b0 - p).max(p - (b0 + SCALE_BLOCK - 1)).max(0)
    };
    let d = near(cx, px).max(near(cz, pz));
    if d < 24 {
        1
    } else if d < 48 {
        2
    } else if d < 96 {
        4
    } else if d < 192 {
        8
    } else {
        16
    }
}

/// Mesh every held chunk of region (rx, rz), each at `scale(cx, cz)` (the game passes
/// `scale_for` around the player), positions relative to the region's block corner,
/// leaving out chunks within `hole.2` (Chebyshev) of chunk (hole.0, hole.1). `counts` gets
/// the vertices of each chunk in order (x + z * 32), so a draw can skip chunks by range.
/// Returns the y range of what was written.
pub fn mesh_region(store: &ClientStore, rx: i32, rz: i32, scale: impl Fn(i32, i32) -> usize, hole: (i32, i32, i32), palette: &Palette, out: &mut Vec<u8>, counts: &mut Vec<u32>) -> Bounds {
    let mut bounds = Bounds::EMPTY;
    counts.clear();
    let with_scale = |cx: i32, cz: i32| store.get_any(cx, cz).map(|r| (r, scale(cx, cz)));
    for lz in 0..REGION {
        for lx in 0..REGION {
            let (cx, cz) = (rx * REGION + lx, rz * REGION + lz);
            let before = out.len();
            let outside = (cx - hole.0).abs() > hole.2 || (cz - hole.1).abs() > hole.2;
            if outside && let Some(rec) = store.get_any(cx, cz) {
                let neighbours: Neighbours = [with_scale(cx - 1, cz), with_scale(cx + 1, cz), with_scale(cx, cz - 1), with_scale(cx, cz + 1)];
                mesh_chunk(rec, &neighbours, scale(cx, cz), palette, ((lx * 16) as f32, (lz * 16) as f32), out, &mut bounds);
            }
            counts.push(((out.len() - before) / VERTEX_BYTES) as u32);
        }
    }
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lod26::{CoarseLod, ColumnLod};

    fn flat(h: i16) -> ColumnLod {
        ColumnLod { height: [h; COLUMNS], ..ColumnLod::empty(0) }
    }

    fn mesh(rec: &ColumnLod, neighbours: &Neighbours, scale: usize, out: &mut Vec<u8>) -> Bounds {
        let mut bounds = Bounds::EMPTY;
        mesh_chunk(RecordRef::Full(rec), neighbours, scale, &Palette::plain(&COLOURS), (0.0, 0.0), out, &mut bounds);
        bounds
    }

    const COLOURS: [[u8; 4]; 2] = [[10, 20, 30, 255], [40, 50, 60, 255]];

    #[test]
    fn water_darkens_with_depth_over_its_floor() {
        let colours = [[0, 0, 200, 191], [200, 200, 100, 255]];
        let palette = Palette::plain(&colours);
        let dry = cell_colour(&palette, 1, 0, 1, 0);
        assert_eq!(dry, [200, 200, 100, 255]);
        let shallow = cell_colour(&palette, 0, 1, 1, 0);
        let deep = cell_colour(&palette, 0, 12, 1, 0);
        let bottomless = cell_colour(&palette, 0, 40, 1, 0);
        let by_biome = [[9, 9, 9, 191], [0, 100, 0, 191]];
        let tinted = Palette { colours: &colours, water: 0, water_by_biome: &by_biome };
        assert_eq!(cell_colour(&tinted, 0, 40, 1, 1), [0, 75, 0, 255], "water takes its biome's colour");
        assert_eq!(cell_colour(&tinted, 0, 40, 1, 7), [0, 0, 150, 255], "a biome past the table keeps the palette colour");
        assert!(shallow[0] > deep[0] && deep[0] > bottomless[0], "the floor fades with depth: {shallow:?} {deep:?} {bottomless:?}");
        assert_eq!(bottomless, [0, 0, 150, 255], "past 15 blocks only the surface is left, at its own alpha");
        let curved = (14.0 / 15.0) / (4.0 - 3.0 * 14.0 / 15.0);
        let lit = 0.5 * curved + 0.5 * (1.0 - (1.0_f32 - curved).powi(4));
        assert_eq!(shallow[2], (200.0 * 0.749_f32 + 100.0 * lit * 0.251).round() as u8);
    }

    #[test]
    fn flat_chunk_is_one_top_quad_and_skirts() {
        let rec = flat(64);
        let mut out = Vec::new();
        let bounds = mesh(&rec, &[None; 4], 4, &mut out);
        // one merged top plus one merged skirt wall per side
        assert_eq!(out.len(), (1 + 4) * 4 * VERTEX_BYTES);
        assert_eq!((bounds.min_y, bounds.max_y), (64 - SKIRT_DEPTH, 64));
        let mut again = Vec::new();
        mesh(&rec, &[None; 4], 4, &mut again);
        assert_eq!(out, again);
        assert_eq!(f32::from_le_bytes([out[4], out[5], out[6], out[7]]), 64.0 - UNDER, "tops sit just under the block face");
        assert_eq!(f32::from_le_bytes([out[32], out[33], out[34], out[35]]), 16.0, "the top spans the chunk");
    }

    #[test]
    fn tops_merge_down_z_only_where_runs_line_up() {
        // Two flat bands of different colour, split along z: two rectangles.
        let mut rec = flat(64);
        for c in 0..COLUMNS {
            if c / 16 >= 8 {
                rec.top[c] = 1;
            }
        }
        let mut out = Vec::new();
        mesh(&rec, &[Some((RecordRef::Full(&rec), 1)); 4], 1, &mut out);
        assert_eq!(out.len() / (4 * VERTEX_BYTES), 2);
        // A single raised column in the middle breaks its row and the rows around it stay whole.
        let mut peak = flat(64);
        peak.height[5 + 5 * 16] = 70;
        let mut out = Vec::new();
        let bounds = mesh(&peak, &[Some((RecordRef::Full(&peak), 1)); 4], 1, &mut out);
        // rows 0..5 one rect, row 5 three rects (left, peak, right), rows 6..16 one rect, four peak walls
        assert_eq!(out.len() / (4 * VERTEX_BYTES), 1 + 3 + 1 + 4);
        assert_eq!((bounds.min_y, bounds.max_y), (64, 70));
    }

    #[test]
    fn coarse_record_meshes_at_its_own_cells_and_edges_resample() {
        let mut rec = flat(64);
        for c in 0..COLUMNS {
            if c % 16 >= 8 {
                rec.height[c] = 70;
            }
        }
        let coarse = CoarseLod::of(&rec);
        let mut out = Vec::new();
        let mut bounds = Bounds::EMPTY;
        mesh_chunk(RecordRef::Coarse(&coarse), &[None; 4], 1, &Palette::plain(&COLOURS), (0.0, 0.0), &mut out, &mut bounds);
        // asked for scale 1, drawn at its 4-block cells: two tops, the step, and skirts: one per x side, two per z side (the step splits them)
        assert_eq!(out.len() / (4 * VERTEX_BYTES), 2 + 1 + 6);
        // A full neighbour beside a coarse chunk: the wall follows the coarse edge at the coarse cell size.
        let mut fine = Vec::new();
        let low = flat(60);
        mesh_chunk(RecordRef::Full(&low), &[Some((RecordRef::Coarse(&coarse), 1)), None, None, None], 1, &Palette::plain(&COLOURS), (0.0, 0.0), &mut fine, &mut bounds);
        let mut plain = Vec::new();
        mesh_chunk(RecordRef::Full(&low), &[Some((RecordRef::Full(&rec), 1)), None, None, None], 1, &Palette::plain(&COLOURS), (0.0, 0.0), &mut plain, &mut bounds);
        assert_eq!(fine.len(), plain.len(), "a higher neighbour raises no wall on the low side either way");
        // A high chunk beside a finer neighbour whose edge steps: the wall reaches the lowest drawn cell of each span.
        let mut steps = flat(60);
        for c in 0..COLUMNS {
            if c / 16 == 3 {
                steps.height[c] = 50;
            }
        }
        let high = flat(70);
        let mut wall = Vec::new();
        mesh_chunk(RecordRef::Full(&high), &[Some((RecordRef::Full(&steps), 1)), None, None, None], 4, &Palette::plain(&COLOURS), (0.0, 0.0), &mut wall, &mut bounds);
        // top, -x wall in two runs (rows 0..4 down to 50, rows 4..16 down to 60), three skirts
        assert_eq!(wall.len() / (4 * VERTEX_BYTES), 1 + 2 + 3);
        let first = &wall[4 * VERTEX_BYTES..5 * VERTEX_BYTES];
        assert_eq!(f32::from_le_bytes([first[4], first[5], first[6], first[7]]), 50.0 - UNDER, "the wall drops to the lowest fine cell in the span");
    }

    #[test]
    fn region_mesh_skips_the_hole_and_offsets_by_chunk() {
        let mut store = ClientStore::new(64);
        let mut batch = Vec::new();
        for (x, z) in [(0, 0), (1, 0), (33, 5)] {
            batch.extend_from_slice(&(x as i32).to_le_bytes());
            batch.extend_from_slice(&(z as i32).to_le_bytes());
            batch.extend_from_slice(&0u32.to_le_bytes());
            let mut bytes = Vec::new();
            flat(64).write_to(&mut bytes);
            batch.extend_from_slice(&bytes);
        }
        assert_eq!(store.put_batch(&batch, 0, 0), Some(3));
        assert_eq!(store.region_generation(0, 0), 2);
        assert_eq!(store.region_generation(1, 0), 1);
        let mut out = Vec::new();
        let mut counts = Vec::new();
        let bounds = mesh_region(&store, 0, 0, |_, _| 16, (0, 0, 0), &Palette::plain(&COLOURS), &mut out, &mut counts);
        assert_eq!(scale_for(0, 0, 0, 0), 1);
        assert_eq!(scale_for(33, 5, 0, 0), 2);
        assert_eq!(scale_for(-1, 0, 0, 0), 1, "the block just west starts at -8");
        assert_eq!(scale_for(200, 0, 0, 0), 16);
        assert_eq!(out.len(), (1 + 3) * 4 * VERTEX_BYTES, "one chunk in the hole, chunk 33,5 in the next region; one flat 16-scale quad with skirts on the three sides without a neighbour");
        assert_eq!(counts.len(), 1024);
        assert_eq!((counts[0], counts[1], counts[2]), (0, 16, 0), "the hole chunk counts nothing, chunk 1,0 the four quads");
        assert_eq!((bounds.min_y, bounds.max_y), (64 - SKIRT_DEPTH, 64));
        assert_eq!(f32::from_le_bytes([out[0], out[1], out[2], out[3]]), 16.0, "chunk 1,0 starts at x 16");
    }

    #[test]
    fn a_step_raises_one_wall_and_a_lower_neighbour_raises_the_border() {
        let mut rec = flat(64);
        for c in 0..COLUMNS {
            if c % 16 >= 8 {
                rec.height[c] = 70;
                rec.top[c] = 1;
            }
        }
        let mut out = Vec::new();
        mesh(&rec, &[None; 4], 8, &mut out);
        // tops: two runs per row (colours differ), each merged down both rows = 2; walls: the step (1) and
        // skirts on all four sides: -x 1, +x 1, -z 2 (two colours), +z 2
        assert_eq!(out.len() / (4 * VERTEX_BYTES), 2 + 1 + 6);
        let lower = flat(60);
        let mut with = Vec::new();
        mesh(&rec, &[Some((RecordRef::Full(&lower), 8)), None, None, None], 8, &mut with);
        assert_eq!(with.len() / (4 * VERTEX_BYTES), 2 + 1 + 6, "a real lower neighbour replaces the -x skirt");
        let wall = &with[2 * 4 * VERTEX_BYTES..3 * 4 * VERTEX_BYTES];
        assert_eq!(wall[12], (10 * 153 / 255) as u8, "x side shade on the low half colour");
        assert_eq!(f32::from_le_bytes([wall[4], wall[5], wall[6], wall[7]]), 60.0 - UNDER, "wall starts at the neighbour's height");
    }
}
