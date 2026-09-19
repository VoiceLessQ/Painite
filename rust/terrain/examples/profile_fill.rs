//! Times each subtree of final_density on one chunk volume, with the
//! Perlin layer samples it took and the node kind at its root.
//! Usage: profile_fill <wg_root> <seed> [chunk_x chunk_z] [depth]

use std::path::Path;
use std::time::Instant;

use painite_terrain::df26::{Loader, Node, Volume};

fn kind(node: &Node) -> String {
    let full = format!("{node:?}");
    let name: String = full.chars().take_while(|c| c.is_alphanumeric()).collect();
    match node {
        Node::Noise { stack, xz_scale, y_scale, .. } => format!("Noise({} layers, xz {xz_scale}, y {y_scale})", stack.layers.len()),
        Node::Interpolated { cell_size_xz, cell_size_y, .. } => format!("Interpolated({cell_size_xz}x{cell_size_y})"),
        Node::RangeChoice { min_inclusive, max_exclusive, .. } => format!("RangeChoice[{min_inclusive}, {max_exclusive})"),
        Node::Constant(c) => format!("Constant({c})"),
        _ => name,
    }
}

fn time(label: &str, node: &Node, vol: &Volume) {
    let before = painite_terrain::noise26::LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);
    let t = Instant::now();
    let v = node.sample_volume(vol);
    std::hint::black_box(v.len());
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let samples = painite_terrain::noise26::LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed) - before;
    println!("{ms:>9.3} ms {samples:>7} smp  {label}  <{}>", kind(node));
    if let Node::Lerp { alpha, .. } = node {
        let a = alpha.sample_volume(vol);
        let zero = a.iter().filter(|&&v| v == 0.0).count();
        let one = a.iter().filter(|&&v| v == 1.0).count();
        println!("{:>25}alpha: {zero} at 0, {one} at 1, {} between of {}", "", a.len() - zero - one, a.len());
    }
}

fn walk(label: String, node: &Node, vol: &Volume, depth: usize) {
    time(&label, node, vol);
    if depth == 0 {
        return;
    }
    let kids: Vec<(&str, &Node)> = match node {
        Node::Add(l, r) | Node::Sub(l, r) | Node::Mul(l, r) | Node::Div(l, r) | Node::Min(l, r) | Node::Max(l, r) => {
            vec![("left", &**l), ("right", &**r)]
        }
        Node::ConstAdd(i, _) | Node::ConstMul(i, _) | Node::Squeeze(i) | Node::Cache(i) | Node::BlendDensity(i)
        | Node::Abs(i) | Node::Square(i) | Node::HalfNegative(i) | Node::QuarterNegative(i) | Node::ConstMin(i, _) | Node::ConstMax(i, _)
        | Node::ConstSub(_, i) | Node::ConstDiv(_, i) | Node::Cube(i) | Node::Sqrt(i) | Node::Reciprocal(i) | Node::Negate(i)
        | Node::Log(i) | Node::Sign(i) | Node::PowConstBase(_, i) | Node::PowConstExp(i, _) => vec![("input", &**i)],
        Node::Pow(l, r) => vec![("base", &**l), ("exp", &**r)],
        Node::Clamp { input, .. } | Node::Slice { input, .. } | Node::Round { input, .. } => vec![("input", &**input)],
        Node::FindTopSurface { density, upper_bound, .. } => vec![("density", &**density), ("upper", &**upper_bound)],
        Node::IntervalSelect { input, functions, .. } => {
            let mut v = vec![("input", &**input)];
            v.extend(functions.iter().map(|f| ("fn", &**f)));
            v
        }
        Node::Noise { shift_x, shift_y, shift_z, .. } => [("shift_x", shift_x), ("shift_y", shift_y), ("shift_z", shift_z)]
            .into_iter()
            .filter_map(|(k, n)| n.as_ref().map(|n| (k, &**n)))
            .collect(),
        Node::Interpolated { input, cell_size_xz, cell_size_y } => {
            let cells = [*cell_size_xz, *cell_size_y, *cell_size_xz];
            let mut cv = *vol;
            for a in 0..3 {
                let min_cell = vol.min[a].div_euclid(cells[a]);
                let max_cell = vol.max_block(a).div_euclid(cells[a]);
                let count = (max_cell - min_cell + 1) as usize;
                cv.size[a] = if vol.max_block(a).rem_euclid(cells[a]) == 0 { count } else { count + 1 };
                cv.min[a] = min_cell * cells[a];
                cv.step[a] = cells[a];
            }
            walk(format!("{label}/corners{:?}", cv.size), input, &cv, depth - 1);
            return;
        }
        Node::Lerp { alpha, first, second } => vec![("alpha", &**alpha), ("first", &**first), ("second", &**second)],
        Node::RangeChoice { input, when_in_range, when_out_of_range, .. } => {
            vec![("input", &**input), ("in", &**when_in_range), ("out", &**when_out_of_range)]
        }
        _ => vec![],
    };
    for (k, n) in kids {
        walk(format!("{label}/{k}"), n, vol, depth - 1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = Path::new(&args[1]);
    let seed: i64 = args[2].parse().unwrap();
    let cx: i32 = args.get(3).map_or(3, |s| s.parse().unwrap());
    let cz: i32 = args.get(4).map_or(7, |s| s.parse().unwrap());
    let depth: usize = args.get(5).map_or(6, |s| s.parse().unwrap());
    let mut loader = Loader::new(root, seed);
    let router = loader.router("minecraft:overworld").unwrap();
    let fd = router.get("final_density").unwrap();
    let vol = Volume::chunk(cx, cz, -64, 384);
    // warm up
    std::hint::black_box(fd.sample_volume(&vol).len());
    let before = painite_terrain::noise26::LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);
    let before_pt = painite_terrain::noise26::POINT_LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);
    let mut best = f64::MAX;
    let mut best_noise = 0.0;
    for _ in 0..10 {
        let mut ctx = painite_terrain::df26::SampleCtx::default();
        let t = Instant::now();
        std::hint::black_box(fd.sample_volume_with(&vol, &mut ctx).len());
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if ms < best {
            best = ms;
            best_noise = ctx.noise_ns as f64 * 1e-6;
        }
    }
    let ms = best;
    let samples = (painite_terrain::noise26::LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed) - before) / 10;
    let pt = (painite_terrain::noise26::POINT_LAYER_SAMPLES.load(std::sync::atomic::Ordering::Relaxed) - before_pt) / 10;
    println!(
        "chunk ({cx}, {cz}): best of 10 {ms:.3} ms (noise {best_noise:.3} ms), {samples} Perlin layer samples ({pt} via point path), {:.1} ns per sample",
        ms * 1e6 / samples as f64
    );
    walk("final_density".to_string(), fd, &vol, depth);
}
