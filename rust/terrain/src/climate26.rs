//! The biome stage: `Climate.Sampler` over the chunk's quart grid and
//! the `Climate.RTree` nearest-parameter search that
//! `MultiNoiseBiomeSource` answers with.

use std::sync::Arc;

use serde_json::Value;

use crate::df26::{Loader, Mode, Node, PointCache, SampleCtx, Volume};

const DIMENSIONS: usize = 7;
const CHILDREN_PER_NODE: usize = 19;

/// `Climate.Parameter`: a closed quantized interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parameter {
    pub min: i64,
    pub max: i64,
}

impl Parameter {
    #[inline]
    fn distance(&self, target: i64) -> i64 {
        let above = target - self.max;
        let below = self.min - target;
        if above > 0 { above } else { below.max(0) }
    }

    fn span(&self, other: Option<Parameter>) -> Parameter {
        match other {
            None => *self,
            Some(o) => Parameter { min: self.min.min(o.min), max: self.max.max(o.max) },
        }
    }
}

/// `Climate.quantizeCoord`.
#[inline]
pub fn quantize(coord: f32) -> i64 {
    (coord * 10000.0f32) as i64
}

enum RNode {
    Leaf { space: [Parameter; DIMENSIONS], leaf: usize },
    Sub { space: [Parameter; DIMENSIONS], children: Vec<RNode> },
}

impl RNode {
    fn space(&self) -> &[Parameter; DIMENSIONS] {
        match self {
            RNode::Leaf { space, .. } | RNode::Sub { space, .. } => space,
        }
    }
}

/// `Node.distance`.
#[inline]
fn space_distance(space: &[Parameter; DIMENSIONS], target: &[i64; DIMENSIONS]) -> i64 {
    let mut d = 0i64;
    for i in 0..DIMENSIONS {
        let x = space[i].distance(target[i]);
        d += x * x;
    }
    d
}

fn build_space(children: &[RNode]) -> [Parameter; DIMENSIONS] {
    let mut bounds: [Option<Parameter>; DIMENSIONS] = [None; DIMENSIONS];
    for child in children {
        for (b, p) in bounds.iter_mut().zip(child.space()) {
            *b = Some(p.span(*b));
        }
    }
    bounds.map(|b| b.expect("non-empty"))
}

#[inline]
fn center(space: &[Parameter; DIMENSIONS], dim: usize, absolute: bool) -> i64 {
    let p = space[dim];
    let c = (p.min + p.max) / 2;
    if absolute { c.abs() } else { c }
}

/// `RTree.sort`: stable sort by the centre on `dimension`, then on each
/// following dimension cyclically.
fn sort_by_center<T>(items: &mut [T], space_of: impl Fn(&T) -> [Parameter; DIMENSIONS], dimension: usize, absolute: bool) {
    items.sort_by(|a, b| {
        for d in 0..DIMENSIONS {
            let dim = (dimension + d) % DIMENSIONS;
            let ord = center(&space_of(a), dim, absolute).cmp(&center(&space_of(b), dim, absolute));
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

fn cost(space: &[Parameter; DIMENSIONS]) -> i64 {
    space.iter().map(|p| (p.max - p.min).abs()).sum()
}

/// `RTree.bucketize` on an index order: buckets of
/// `c ^ floor(log_c(n - 0.01))` consecutive nodes.
fn bucketize(order: &[usize], n_total: usize) -> Vec<Vec<usize>> {
    let n = n_total as f64;
    let c = CHILDREN_PER_NODE as f64;
    let expected = c.powf(((n - 0.01).ln() / c.ln()).floor()) as usize;
    let mut buckets = Vec::new();
    let mut current = Vec::new();
    for &i in order {
        current.push(i);
        if current.len() >= expected {
            buckets.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        buckets.push(current);
    }
    buckets
}

/// `RTree.build`. Java sorts one list in place per dimension, so each
/// dimension's stable sort starts from the previous dimension's order;
/// the index vector replays that.
fn build(mut children: Vec<RNode>) -> RNode {
    if children.len() == 1 {
        return children.pop().unwrap();
    }
    if children.len() <= CHILDREN_PER_NODE {
        children.sort_by_key(|leaf| {
            let mut total = 0i64;
            for d in 0..DIMENSIONS {
                let p = leaf.space()[d];
                total += ((p.min + p.max) / 2).abs();
            }
            total
        });
        let space = build_space(&children);
        return RNode::Sub { space, children };
    }
    let n = children.len();
    let mut order: Vec<usize> = (0..n).collect();
    let mut min_cost = i64::MAX;
    let mut min_dimension = 0;
    let mut min_buckets: Vec<Vec<usize>> = Vec::new();
    for d in 0..DIMENSIONS {
        sort_by_center(&mut order, |&i| *children[i].space(), d, false);
        let buckets = bucketize(&order, n);
        let total: i64 = buckets
            .iter()
            .map(|b| {
                let mut bounds: [Option<Parameter>; DIMENSIONS] = [None; DIMENSIONS];
                for &i in b {
                    for (bound, p) in bounds.iter_mut().zip(children[i].space()) {
                        *bound = Some(p.span(*bound));
                    }
                }
                cost(&bounds.map(|x| x.unwrap()))
            })
            .sum();
        if min_cost > total {
            min_cost = total;
            min_dimension = d;
            min_buckets = buckets;
        }
    }
    // Move the nodes into their buckets (each index appears once).
    let mut slots: Vec<Option<RNode>> = children.into_iter().map(Some).collect();
    let mut subtrees: Vec<RNode> = min_buckets
        .into_iter()
        .map(|b| {
            let nodes: Vec<RNode> = b.into_iter().map(|i| slots[i].take().expect("unique")).collect();
            let space = build_space(&nodes);
            RNode::Sub { space, children: nodes }
        })
        .collect();
    sort_by_center(&mut subtrees, |s| *s.space(), min_dimension, true);
    let built: Vec<RNode> = subtrees
        .into_iter()
        .map(|b| match b {
            RNode::Sub { children, .. } => build(children),
            leaf => leaf,
        })
        .collect();
    let space = build_space(&built);
    RNode::Sub { space, children: built }
}

/// `Climate.RTree` over biome indices.
pub struct RTree {
    root: RNode,
    /// Parameter space and biome index per leaf, by leaf id.
    leaves: Vec<([Parameter; DIMENSIONS], u16)>,
    /// The same tree flattened depth-first: a node's children are the
    /// contiguous run `children.0..children.1`.
    flat: Vec<FlatNode>,
    /// Flat node index per leaf id.
    leaf_node: Vec<u32>,
}

struct FlatNode {
    space: [Parameter; DIMENSIONS],
    children: (u32, u32),
    /// Leaf id, or `u32::MAX` for an inner node.
    leaf: u32,
}

/// Per block column memo for `RTree::search_in_column`: a node's
/// distance over every dimension but depth, computed on first visit
/// in the column. `stamp` marks which column the value belongs to.
#[derive(Default)]
pub struct ColumnSearch {
    stamp: Vec<u32>,
    partial: Vec<i64>,
    current: u32,
    target: [i64; DIMENSIONS],
}

/// The dimension the depth function drives; every other climate value
/// is a function of x and z alone.
const DEPTH_DIMENSION: usize = 4;

impl RTree {
    /// Build from (parameter point, biome index) pairs in registry order.
    pub fn new(values: &[([Parameter; DIMENSIONS], u16)]) -> Result<Self, String> {
        if values.is_empty() {
            return Err("biome parameters: empty".into());
        }
        let leaves: Vec<RNode> = values.iter().enumerate().map(|(leaf, (space, _))| RNode::Leaf { space: *space, leaf }).collect();
        let root = build(leaves);
        let mut flat = Vec::new();
        let mut leaf_node = vec![u32::MAX; values.len()];
        fn record(node: &RNode, at: usize, leaf_node: &mut [u32]) -> FlatNode {
            let leaf = match node {
                RNode::Leaf { leaf, .. } => {
                    leaf_node[*leaf] = at as u32;
                    *leaf as u32
                }
                RNode::Sub { .. } => u32::MAX,
            };
            FlatNode { space: *node.space(), children: (0, 0), leaf }
        }
        // Siblings sit side by side; each one's own children follow later.
        fn flatten_children(children: &[RNode], flat: &mut Vec<FlatNode>, leaf_node: &mut [u32]) -> (u32, u32) {
            let first = flat.len();
            for (k, c) in children.iter().enumerate() {
                let r = record(c, first + k, leaf_node);
                flat.push(r);
            }
            for (k, c) in children.iter().enumerate() {
                if let RNode::Sub { children: grand, .. } = c {
                    flat[first + k].children = flatten_children(grand, flat, leaf_node);
                }
            }
            (first as u32, (first + children.len()) as u32)
        }
        let r = record(&root, 0, &mut leaf_node);
        flat.push(r);
        if let RNode::Sub { children, .. } = &root {
            flat[0].children = flatten_children(children, &mut flat, &mut leaf_node);
        }
        Ok(Self { root, leaves: values.to_vec(), flat, leaf_node })
    }

    /// Start a column: `target` in every dimension but depth.
    pub fn column(&self, target: &[i64; DIMENSIONS], col: &mut ColumnSearch) {
        if col.stamp.len() != self.flat.len() {
            col.stamp = vec![0; self.flat.len()];
            col.partial = vec![0; self.flat.len()];
            col.current = 0;
        }
        if col.current == u32::MAX {
            col.stamp.iter_mut().for_each(|s| *s = 0);
            col.current = 0;
        }
        col.current += 1;
        col.target = *target;
    }

    /// Node distance for the column's target at `depth`: the six fixed
    /// dimensions from the column memo plus the depth term. Same sum
    /// `space_distance` forms, in a different order (exact in i64).
    #[inline]
    fn node_distance(&self, col: &mut ColumnSearch, node: u32, depth: i64) -> i64 {
        let n = node as usize;
        if col.stamp[n] != col.current {
            let space = &self.flat[n].space;
            let mut d = 0i64;
            for i in (0..DIMENSIONS).filter(|&i| i != DEPTH_DIMENSION) {
                let x = space[i].distance(col.target[i]);
                d += x * x;
            }
            col.partial[n] = d;
            col.stamp[n] = col.current;
        }
        let dd = self.flat[n].space[DEPTH_DIMENSION].distance(depth);
        col.partial[n] + dd * dd
    }

    /// `search` on the flat tree with the column memo: the same walk,
    /// same comparisons, same answer.
    pub fn search_in_column(&self, col: &mut ColumnSearch, depth: i64, candidate: Option<usize>) -> usize {
        fn go(tree: &RTree, col: &mut ColumnSearch, node: u32, depth: i64, candidate: Option<usize>) -> Option<usize> {
            let n = &tree.flat[node as usize];
            if n.leaf != u32::MAX {
                return Some(n.leaf as usize);
            }
            let mut min_distance = candidate.map_or(i64::MAX, |c| tree.node_distance(col, tree.leaf_node[c], depth));
            let mut closest = candidate;
            for child in n.children.0..n.children.1 {
                let child_distance = tree.node_distance(col, child, depth);
                if min_distance > child_distance {
                    if let Some(l) = go(tree, col, child, depth, closest) {
                        let leaf_distance = if tree.flat[child as usize].leaf == l as u32 {
                            child_distance
                        } else {
                            tree.node_distance(col, tree.leaf_node[l], depth)
                        };
                        if min_distance > leaf_distance {
                            min_distance = leaf_distance;
                            closest = Some(l);
                        }
                    }
                }
            }
            closest
        }
        go(self, col, 0, depth, candidate).expect("tree has leaves")
    }

    /// `RTree.search`: the leaf id closest to `target`, starting from the
    /// previous result as vanilla's thread-local candidate does.
    pub fn search(&self, target: &[i64; DIMENSIONS], candidate: Option<usize>) -> usize {
        fn go(tree: &RTree, node: &RNode, target: &[i64; DIMENSIONS], candidate: Option<usize>) -> Option<usize> {
            match node {
                RNode::Leaf { leaf, .. } => Some(*leaf),
                RNode::Sub { children, .. } => {
                    let mut min_distance = candidate.map_or(i64::MAX, |c| space_distance(&tree.leaves[c].0, target));
                    let mut closest = candidate;
                    for child in children {
                        let child_distance = space_distance(child.space(), target);
                        if min_distance > child_distance {
                            let leaf = go(tree, child, target, closest);
                            if let Some(l) = leaf {
                                let leaf_distance = if matches!(child, RNode::Leaf { leaf: id, .. } if *id == l) {
                                    child_distance
                                } else {
                                    space_distance(&tree.leaves[l].0, target)
                                };
                                if min_distance > leaf_distance {
                                    min_distance = leaf_distance;
                                    closest = Some(l);
                                }
                            }
                        }
                    }
                    closest
                }
            }
        }
        go(self, &self.root, target, candidate).expect("tree has leaves")
    }

    pub fn value(&self, leaf: usize) -> u16 {
        self.leaves[leaf].1
    }
}

/// The compiled biome stage for one world.
pub struct BiomeStage {
    tree: RTree,
    temperature: Arc<Node>,
    humidity: Arc<Node>,
    continentalness: Arc<Node>,
    erosion: Arc<Node>,
    depth: Arc<Node>,
    weirdness: Arc<Node>,
    min_y: i32,
    height: i32,
}

impl BiomeStage {
    /// `documents` must hold a `biome_parameters` document for
    /// `minecraft:overworld` shaped `{"values": [{"biome": id,
    /// "space": [t_min, t_max, h_min, h_max, c.., e.., d.., w.., offset]}]}`
    /// with the game's quantized longs, and the router must be the one
    /// the fill uses.
    pub fn load(loader: &mut Loader, biome_ids: &[String], min_y: i32, height: i32) -> Result<Self, String> {
        let doc = loader.document("biome_parameters", "minecraft:overworld")?;
        let values = doc.get("values").and_then(Value::as_array).ok_or("biome_parameters: values missing")?;
        let index: std::collections::HashMap<&str, u16> = biome_ids.iter().enumerate().map(|(i, id)| (id.as_str(), i as u16)).collect();
        let mut pairs = Vec::with_capacity(values.len());
        for v in values {
            let id = v.get("biome").and_then(Value::as_str).ok_or("biome_parameters: biome missing")?;
            let full = crate::df26::full_id(id);
            let &bi = index.get(full.as_str()).ok_or_else(|| format!("biome_parameters: unknown biome {id}"))?;
            let space = v.get("space").and_then(Value::as_array).ok_or("biome_parameters: space missing")?;
            if space.len() != 13 {
                return Err(format!("biome_parameters: space has {} values, expected 13", space.len()));
            }
            let mut params = [Parameter { min: 0, max: 0 }; DIMENSIONS];
            for d in 0..6 {
                let min = space[2 * d].as_i64().ok_or("biome_parameters: bad long")?;
                let max = space[2 * d + 1].as_i64().ok_or("biome_parameters: bad long")?;
                params[d] = Parameter { min, max };
            }
            let offset = space[12].as_i64().ok_or("biome_parameters: bad offset")?;
            params[6] = Parameter { min: offset, max: offset };
            pairs.push((params, bi));
        }
        let tree = RTree::new(&pairs)?;
        let router = loader.router("minecraft:overworld")?;
        let key = |k: &str| router.get(k).cloned().ok_or_else(|| format!("noise_router.{k}: missing"));
        Ok(Self {
            tree,
            temperature: key("temperature")?,
            humidity: key("vegetation")?,
            continentalness: key("continents")?,
            erosion: key("erosion")?,
            depth: key("depth")?,
            weirdness: key("ridges")?,
            min_y,
            height,
        })
    }

    /// `BiomeSource.getNoiseBiome(quartX, quartY, quartZ)` through the
    /// uncached resolver: one climate sample and one tree search.
    pub fn biome_at_quart(&self, quart_x: i32, quart_y: i32, quart_z: i32) -> u16 {
        let (x, y, z) = (quart_x << 2, quart_y << 2, quart_z << 2);
        let mut pc = PointCache::new();
        let mut at = |n: &Node| quantize(n.eval_with(x, y, z, Mode::Point, &mut pc));
        let target = [
            at(&self.temperature),
            at(&self.humidity),
            at(&self.continentalness),
            at(&self.erosion),
            at(&self.depth),
            at(&self.weirdness),
            0,
        ];
        self.tree.value(self.tree.search(&target, None))
    }

    /// Biome index per quart of a chunk, `y + (x + z * 4) * (height / 4)`,
    /// exactly `MultiNoiseBiomeSource.createResolverForChunk` followed by
    /// `fillBiomesFromNoise` (x, y, z order of lookups).
    pub fn chunk_biomes(&self, chunk_x: i32, chunk_z: i32) -> Vec<u16> {
        self.chunk_biomes_stats(chunk_x, chunk_z).0
    }

    /// `chunk_biomes` with the time split between the six climate
    /// samples and the tree searches.
    pub fn chunk_biomes_stats(&self, chunk_x: i32, chunk_z: i32) -> (Vec<u16>, BiomeStats) {
        let t0 = std::time::Instant::now();
        let quart_y = (self.height / 4) as usize;
        let vol = Volume::new([4, quart_y, 4], [chunk_x * 16, self.min_y, chunk_z * 16], [4, 4, 4]);
        let mut ctx = SampleCtx::default();
        let t = self.temperature.sample_volume_with(&vol, &mut ctx);
        let h = self.humidity.sample_volume_with(&vol, &mut ctx);
        let c = self.continentalness.sample_volume_with(&vol, &mut ctx);
        let e = self.erosion.sample_volume_with(&vol, &mut ctx);
        let d = self.depth.sample_volume_with(&vol, &mut ctx);
        let w = self.weirdness.sample_volume_with(&vol, &mut ctx);
        let mut stats = BiomeStats { noise_ms: t0.elapsed().as_secs_f64() * 1e3, ..BiomeStats::default() };
        let t1 = std::time::Instant::now();
        let mut out = vec![0u16; vol.len()];
        let mut last: Option<usize> = None;
        // Every climate value but depth is a function of the column. The
        // memos are sized by the tree, so they stay with the thread.
        thread_local! {
            static COLUMNS: std::cell::RefCell<Vec<ColumnSearch>> = std::cell::RefCell::new((0..16).map(|_| ColumnSearch::default()).collect());
        }
        let columns = COLUMNS.with(|c| c.take());
        let mut columns = columns;
        for (n, col) in columns.iter_mut().enumerate() {
            let i = vol.index(n % 4, 0, n / 4);
            let target = [quantize(t[i]), quantize(h[i]), quantize(c[i]), quantize(e[i]), 0, quantize(w[i]), 0];
            self.tree.column(&target, col);
        }
        // Section by section, then x, y, z inside: the order vanilla asks in.
        for section in 0..quart_y / 4 {
            for x in 0..4 {
                for y in 0..4 {
                    for z in 0..4 {
                        let qy = section * 4 + y;
                        let i = vol.index(x, qy, z);
                        let found = self.tree.search_in_column(&mut columns[x + z * 4], quantize(d[i]), last);
                        last = Some(found);
                        out[i] = self.tree.value(found);
                    }
                }
            }
        }
        COLUMNS.with(|c| *c.borrow_mut() = columns);
        stats.search_ms = t1.elapsed().as_secs_f64() * 1e3;
        (out, stats)
    }
}

impl BiomeStage {
    /// Test hook: `count` random targets around the chunk's climate
    /// values, each looked up by the tree search and by the column
    /// search with the same candidate chain; returns how many differ.
    pub fn cross_check_search(&self, chunk_x: i32, chunk_z: i32, count: usize) -> usize {
        let quart_y = (self.height / 4) as usize;
        let vol = Volume::new([4, quart_y, 4], [chunk_x * 16, self.min_y, chunk_z * 16], [4, 4, 4]);
        let mut ctx = SampleCtx::default();
        let t = self.temperature.sample_volume_with(&vol, &mut ctx);
        let h = self.humidity.sample_volume_with(&vol, &mut ctx);
        let c = self.continentalness.sample_volume_with(&vol, &mut ctx);
        let e = self.erosion.sample_volume_with(&vol, &mut ctx);
        let w = self.weirdness.sample_volume_with(&vol, &mut ctx);
        let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (chunk_x as u64).wrapping_mul(0x100_0000_01B3) ^ (chunk_z as u64);
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut col = ColumnSearch::default();
        let mut last_tree: Option<usize> = None;
        let mut last_col: Option<usize> = None;
        let mut differ = 0;
        for _ in 0..count {
            let i = (next() % vol.len() as u64) as usize;
            // Jitter in quantized units so targets land in gaps and on box edges too.
            let jitter = |v: f32, r: u64| quantize(v) + (r % 4001) as i64 - 2000;
            let target = [jitter(t[i], next()), jitter(h[i], next()), jitter(c[i], next()), jitter(e[i], next()), (next() % 25_000) as i64 - 5_000, jitter(w[i], next()), 0];
            let mut column_target = target;
            column_target[DEPTH_DIMENSION] = 0;
            self.tree.column(&column_target, &mut col);
            let a = self.tree.search(&target, last_tree);
            let b = self.tree.search_in_column(&mut col, target[DEPTH_DIMENSION], last_col);
            if a != b {
                differ += 1;
            }
            last_tree = Some(a);
            last_col = Some(b);
        }
        differ
    }
}

/// Where a chunk's biome stage spends its time.
#[derive(Clone, Copy, Debug, Default)]
pub struct BiomeStats {
    pub noise_ms: f64,
    pub search_ms: f64,
}
