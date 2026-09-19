//! Interval bounds of a density tree over one column of corner rows:
//! subtrees that do not read y are evaluated exactly at the column,
//! the rest is interval arithmetic with gradients exact over the row
//! range and noises at their stack bound. The fused fill uses it to
//! find the corner row above which the main density is provably
//! negative, so the five corner grids stop there.

use std::collections::HashMap;
use std::sync::Arc;

use crate::df26::{Axis, Mode, Node, PointCache, Spline, Tiling};
use crate::noise26::NoiseStack;

/// `[lo, hi]`; NaN anywhere means unknown.
pub type Interval = (f32, f32);

const ANY: Interval = (f32::NEG_INFINITY, f32::INFINITY);

/// Memos that survive across columns (`y_dep`) and within one column
/// (`points`, the exact values of y-independent subtrees).
pub struct BoundCtx {
    y_dep: HashMap<usize, bool>,
    points: HashMap<usize, f32>,
    pc: PointCache,
    x: i32,
    z: i32,
    /// The y the y-independent subtrees are evaluated at (any value).
    y_ref: i32,
}

impl Default for BoundCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl BoundCtx {
    pub fn new() -> Self {
        Self { y_dep: HashMap::new(), points: HashMap::new(), pc: PointCache::new(), x: 0, z: 0, y_ref: 0 }
    }

    /// Forget the column memos; the y-dependence memo is per tree and stays.
    pub fn start_column(&mut self, x: i32, z: i32, y_ref: i32) {
        self.points.clear();
        self.pc = PointCache::new();
        self.x = x;
        self.z = z;
        self.y_ref = y_ref;
    }
}

fn key(n: &Node) -> usize {
    n as *const Node as usize
}

/// Whether a subtree reads y anywhere.
pub fn depends_on_y(node: &Node, memo: &mut HashMap<usize, bool>) -> bool {
    if let Some(&d) = memo.get(&key(node)) {
        return d;
    }
    let d = match node {
        Node::Constant(_) | Node::BlendAlpha | Node::BlendOffset | Node::Beardifier | Node::ShiftB(_) => false,
        Node::Gradient { axis, .. } => matches!(axis, Axis::Y),
        Node::Noise { y_scale, shift_x, shift_y, shift_z, .. } => {
            *y_scale != 0.0 || [shift_x, shift_y, shift_z].iter().any(|s| s.as_ref().is_some_and(|n| depends_on_y(n, memo)))
        }
        Node::Slice { axis: Axis::Y, .. } => false,
        Node::Slice { input, .. } => depends_on_y(input, memo),
        Node::Interpolated { .. } | Node::FindTopSurface { .. } | Node::DistanceToPoint { .. } => true,
        Node::Add(l, r) | Node::Sub(l, r) | Node::Mul(l, r) | Node::Div(l, r) | Node::Min(l, r) | Node::Max(l, r) | Node::Pow(l, r) => {
            depends_on_y(l, memo) || depends_on_y(r, memo)
        }
        Node::ConstAdd(i, _)
        | Node::ConstSub(_, i)
        | Node::ConstMul(i, _)
        | Node::ConstDiv(_, i)
        | Node::ConstMin(i, _)
        | Node::ConstMax(i, _)
        | Node::Abs(i)
        | Node::Square(i)
        | Node::Cube(i)
        | Node::Sqrt(i)
        | Node::HalfNegative(i)
        | Node::QuarterNegative(i)
        | Node::Reciprocal(i)
        | Node::Negate(i)
        | Node::Squeeze(i)
        | Node::Log(i)
        | Node::Sign(i)
        | Node::PowConstBase(_, i)
        | Node::PowConstExp(i, _)
        | Node::Cache(i)
        | Node::BlendDensity(i) => depends_on_y(i, memo),
        Node::Round { input, multiple, .. } => depends_on_y(input, memo) || multiple.as_ref().is_some_and(|m| depends_on_y(m, memo)),
        Node::Lerp { alpha, first, second } => depends_on_y(alpha, memo) || depends_on_y(first, memo) || depends_on_y(second, memo),
        Node::Clamp { input, .. } => depends_on_y(input, memo),
        Node::RangeChoice { input, when_in_range, when_out_of_range, .. } => {
            depends_on_y(input, memo) || depends_on_y(when_in_range, memo) || depends_on_y(when_out_of_range, memo)
        }
        Node::IntervalSelect { input, functions, .. } => depends_on_y(input, memo) || functions.iter().any(|f| depends_on_y(f, memo)),
        Node::Spline(s) => spline_depends_on_y(s, memo),
    };
    memo.insert(key(node), d);
    d
}

fn spline_depends_on_y(s: &Spline, memo: &mut HashMap<usize, bool>) -> bool {
    match s {
        Spline::Constant(_) => false,
        Spline::Multipoint { coordinate, values, .. } => depends_on_y(coordinate, memo) || values.iter().any(|v| spline_depends_on_y(v, memo)),
    }
}

/// `PerlinNoise.edgeValue(2.0)`: two per octave, the bound vanilla uses.
fn stack_bound(stack: &Arc<NoiseStack>) -> f32 {
    stack.layers.iter().map(|l| l.amplitude.abs() * 2.0).sum()
}

fn unknown(i: Interval) -> bool {
    i.0.is_nan() || i.1.is_nan()
}

fn hull(vals: impl IntoIterator<Item = f32>) -> Interval {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for v in vals {
        if v.is_nan() {
            return ANY;
        }
        lo = lo.min(v);
        hi = hi.max(v);
    }
    (lo, hi)
}

fn monotone(i: Interval, f: impl Fn(f32) -> f32) -> Interval {
    if unknown(i) {
        return ANY;
    }
    hull([f(i.0), f(i.1)])
}

fn mul(a: Interval, b: Interval) -> Interval {
    if unknown(a) || unknown(b) {
        return ANY;
    }
    hull([a.0 * b.0, a.0 * b.1, a.1 * b.0, a.1 * b.1])
}

fn add(a: Interval, b: Interval) -> Interval {
    if unknown(a) || unknown(b) {
        return ANY;
    }
    (a.0 + b.0, a.1 + b.1)
}

fn union(a: Interval, b: Interval) -> Interval {
    if unknown(a) || unknown(b) {
        return ANY;
    }
    (a.0.min(b.0), a.1.max(b.1))
}

fn clamp(v: f32, min: f32, max: f32) -> f32 {
    if v < min { min } else { v.min(max) }
}

fn squeeze(v: f32) -> f32 {
    let c = clamp(v, -1.0, 1.0);
    c / 2.0 - c * c * c / 24.0
}

impl Node {
    /// Bounds of this tree at column (x, z) over block rows `y_lo..=y_hi`.
    /// Every value the point or volume path can produce lies inside,
    /// up to float rounding; callers keep a margin.
    pub fn column_bounds(&self, y_lo: i32, y_hi: i32, ctx: &mut BoundCtx) -> Interval {
        if !depends_on_y(self, &mut ctx.y_dep) {
            let k = key(self);
            if let Some(&v) = ctx.points.get(&k) {
                return (v, v);
            }
            let v = self.eval_with(ctx.x, ctx.y_ref, ctx.z, Mode::Point, &mut ctx.pc);
            ctx.points.insert(k, v);
            return if v.is_nan() { ANY } else { (v, v) };
        }
        let b = |n: &Node, ctx: &mut BoundCtx| n.column_bounds(y_lo, y_hi, ctx);
        match self {
            Node::Add(l, r) => add(b(l, ctx), b(r, ctx)),
            Node::ConstAdd(l, c) => add(b(l, ctx), (*c, *c)),
            Node::Sub(l, r) => {
                let (a, s) = (b(l, ctx), b(r, ctx));
                add(a, (-s.1, -s.0))
            }
            Node::ConstSub(c, r) => {
                let s = b(r, ctx);
                add((*c, *c), (-s.1, -s.0))
            }
            Node::Mul(l, r) => mul(b(l, ctx), b(r, ctx)),
            Node::ConstMul(l, c) => mul(b(l, ctx), (*c, *c)),
            Node::Div(l, r) => {
                let (a, d) = (b(l, ctx), b(r, ctx));
                if unknown(d) || (d.0 <= 0.0 && d.1 >= 0.0) {
                    ANY
                } else {
                    mul(a, (1.0 / d.1, 1.0 / d.0))
                }
            }
            Node::ConstDiv(c, r) => {
                let d = b(r, ctx);
                if unknown(d) || (d.0 <= 0.0 && d.1 >= 0.0) {
                    ANY
                } else {
                    mul((*c, *c), (1.0 / d.1, 1.0 / d.0))
                }
            }
            Node::Reciprocal(i) => {
                let d = b(i, ctx);
                if unknown(d) || (d.0 <= 0.0 && d.1 >= 0.0) {
                    ANY
                } else {
                    (1.0 / d.1, 1.0 / d.0)
                }
            }
            Node::Min(l, r) => {
                let (a, c) = (b(l, ctx), b(r, ctx));
                if unknown(a) || unknown(c) {
                    ANY
                } else {
                    (a.0.min(c.0), a.1.min(c.1))
                }
            }
            Node::ConstMin(l, c) => {
                let a = b(l, ctx);
                if unknown(a) {
                    ANY
                } else {
                    (a.0.min(*c), a.1.min(*c))
                }
            }
            Node::Max(l, r) => {
                let (a, c) = (b(l, ctx), b(r, ctx));
                if unknown(a) || unknown(c) {
                    ANY
                } else {
                    (a.0.max(c.0), a.1.max(c.1))
                }
            }
            Node::ConstMax(l, c) => {
                let a = b(l, ctx);
                if unknown(a) {
                    ANY
                } else {
                    (a.0.max(*c), a.1.max(*c))
                }
            }
            Node::Abs(i) => {
                let a = b(i, ctx);
                if unknown(a) {
                    ANY
                } else if a.0 >= 0.0 {
                    a
                } else if a.1 <= 0.0 {
                    (-a.1, -a.0)
                } else {
                    (0.0, (-a.0).max(a.1))
                }
            }
            Node::Square(i) => {
                let a = b(i, ctx);
                if unknown(a) {
                    ANY
                } else if a.0 >= 0.0 || a.1 <= 0.0 {
                    hull([a.0 * a.0, a.1 * a.1])
                } else {
                    (0.0, (a.0 * a.0).max(a.1 * a.1))
                }
            }
            Node::Cube(i) => monotone(b(i, ctx), |v| v * v * v),
            Node::Sqrt(i) => {
                let a = b(i, ctx);
                if unknown(a) || a.0 < 0.0 {
                    ANY
                } else {
                    (a.0.sqrt(), a.1.sqrt())
                }
            }
            Node::HalfNegative(i) => monotone(b(i, ctx), |v| if v > 0.0 { v } else { v * 0.5 }),
            Node::QuarterNegative(i) => monotone(b(i, ctx), |v| if v > 0.0 { v } else { v * 0.25 }),
            Node::Negate(i) => {
                let a = b(i, ctx);
                if unknown(a) {
                    ANY
                } else {
                    (-a.1, -a.0)
                }
            }
            Node::Squeeze(i) => monotone(b(i, ctx), squeeze),
            Node::Log(i) => {
                let a = b(i, ctx);
                if unknown(a) || a.0 <= 0.0 {
                    ANY
                } else {
                    monotone(a, |v| (v as f64).ln() as f32)
                }
            }
            Node::Sign(i) => monotone(b(i, ctx), |v| {
                if v > 0.0 {
                    1.0
                } else if v < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            }),
            Node::Clamp { input, min, max } => monotone(b(input, ctx), |v| clamp(v, *min, *max)),
            Node::Gradient { axis: Axis::Y, tiling, from_coordinate, to_coordinate, from_value, factor } => match tiling {
                Tiling::ClampToEdge => {
                    let lo = (*from_coordinate).min(*to_coordinate);
                    let hi = (*from_coordinate).max(*to_coordinate);
                    let at = |c: i32| from_value + (c.clamp(lo, hi) - from_coordinate) as f32 * factor;
                    hull([at(y_lo), at(y_hi)])
                }
                _ => {
                    let span = (to_coordinate - from_coordinate) as f32 * factor;
                    hull([*from_value, from_value + span])
                }
            },
            Node::Noise { stack, .. } => {
                let m = stack_bound(stack);
                (-m, m)
            }
            Node::Lerp { alpha, first, second } => {
                let (a, f, s) = (b(alpha, ctx), b(first, ctx), b(second, ctx));
                if unknown(a) || unknown(f) || unknown(s) {
                    return ANY;
                }
                let mut vals = Vec::with_capacity(8);
                for &av in &[a.0, a.1] {
                    for &fv in &[f.0, f.1] {
                        for &sv in &[s.0, s.1] {
                            vals.push(fv + av * (sv - fv));
                        }
                    }
                }
                // An alpha of exactly 0 or 1 selects a branch outright; the hull already covers it.
                hull(vals)
            }
            Node::RangeChoice { input, min_inclusive, max_exclusive, when_in_range, when_out_of_range } => {
                let v = b(input, ctx);
                if unknown(v) {
                    return ANY;
                }
                let can_in = v.1 >= *min_inclusive && v.0 < *max_exclusive;
                let can_out = v.0 < *min_inclusive || v.1 >= *max_exclusive;
                match (can_in, can_out) {
                    (true, false) => b(when_in_range, ctx),
                    (false, true) => b(when_out_of_range, ctx),
                    _ => union(b(when_in_range, ctx), b(when_out_of_range, ctx)),
                }
            }
            Node::IntervalSelect { input, thresholds, functions } => {
                let v = b(input, ctx);
                if unknown(v) {
                    return ANY;
                }
                let pick = |x: f32| {
                    let mut idx = functions.len() - 1;
                    for (i, t) in thresholds.iter().enumerate() {
                        if x < *t {
                            idx = i;
                            break;
                        }
                    }
                    idx
                };
                let (lo, hi) = (pick(v.0), pick(v.1));
                let mut out: Option<Interval> = None;
                for f in &functions[lo..=hi] {
                    let fb = b(f, ctx);
                    out = Some(match out {
                        None => fb,
                        Some(o) => union(o, fb),
                    });
                }
                out.unwrap_or(ANY)
            }
            Node::Cache(i) | Node::BlendDensity(i) => b(i, ctx),
            Node::Slice { axis, coordinate, input } => {
                // A slice on x or z with a y-dependent input: bound it at that coordinate.
                let (sx, sz) = (ctx.x, ctx.z);
                match axis {
                    Axis::X => ctx.x = *coordinate,
                    Axis::Z => ctx.z = *coordinate,
                    Axis::Y => unreachable!("a y slice never depends on y"),
                }
                let saved = std::mem::take(&mut ctx.points);
                let r = b(input, ctx);
                ctx.points = saved;
                ctx.x = sx;
                ctx.z = sz;
                r
            }
            _ => ANY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hull_and_mul_cover_sign_changes() {
        assert_eq!(mul((-1.0, 2.0), (-3.0, 0.5)), (-6.0, 3.0));
        assert_eq!(hull([1.0, -2.0, 0.5]), (-2.0, 1.0));
        assert_eq!(mul((f32::NAN, 1.0), (1.0, 2.0)), ANY);
    }

    #[test]
    fn squeeze_bound_is_monotone() {
        let i = monotone((-2.0, 0.5), squeeze);
        assert_eq!(i.0, squeeze(-1.0));
        assert_eq!(i.1, squeeze(0.5));
    }
}
