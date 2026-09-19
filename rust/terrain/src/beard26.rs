//! Structure terrain adaptation (`Beardifier`): the density each rigid
//! structure piece and jigsaw junction adds around itself so the ground
//! rises under a village house or opens around a buried room.
//!
//! The game hands the pieces over as one flat `i32` array (built by
//! `TerrainBridge.beardPieces`, also written by the surface oracle):
//!
//! ```text
//! [n_rigid, n_junction, aff_min_x, aff_min_y, aff_min_z, aff_max_x, aff_max_y, aff_max_z,
//!  (min_x, min_y, min_z, max_x, max_y, max_z, adjustment, ground_delta) * n_rigid,
//!  (x, ground_y, z) * n_junction]
//! ```
//!
//! Vanilla evaluates every piece at every block of the affected box. A
//! piece only contributes inside its kernel radius, and an exact zero
//! added to a float is the identity, so this port walks each piece over
//! its own reach instead; the per-block sum order is unchanged.

use crate::df26::Volume;
use std::sync::OnceLock;

/// `TerrainAdjustment` ordinals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adjustment {
    None,
    Bury,
    BeardThin,
    BeardBox,
    Encapsulate,
}

#[derive(Clone, Copy, Debug)]
pub struct Rigid {
    pub min: [i32; 3],
    pub max: [i32; 3],
    pub adjustment: Adjustment,
    pub ground_delta: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Junction {
    pub x: i32,
    pub ground_y: i32,
    pub z: i32,
}

#[derive(Clone, Debug)]
pub struct Beardifier {
    pub rigids: Vec<Rigid>,
    pub junctions: Vec<Junction>,
    /// Every piece box and junction, inflated by 24: vanilla samples 0
    /// outside it.
    pub affected_min: [i32; 3],
    pub affected_max: [i32; 3],
}

const RADIUS: i32 = 12;
const KERNEL_SIZE: usize = 24;

fn kernel() -> &'static [f32] {
    static KERNEL: OnceLock<Vec<f32>> = OnceLock::new();
    KERNEL.get_or_init(|| {
        let mut k = vec![0.0f32; KERNEL_SIZE * KERNEL_SIZE * KERNEL_SIZE];
        for zi in 0..KERNEL_SIZE {
            for xi in 0..KERNEL_SIZE {
                for yi in 0..KERNEL_SIZE {
                    let dx = (xi as i32 - RADIUS) as f64;
                    let dz = (zi as i32 - RADIUS) as f64;
                    let dy = (yi as i32 - RADIUS) as f64 + 0.5;
                    let d = dx * dx + dy * dy + dz * dz;
                    // Math.pow(Math.E, x) and exp agree to the bit on every entry (checked on JDK 25).
                    k[zi * KERNEL_SIZE * KERNEL_SIZE + xi * KERNEL_SIZE + yi] = (-d / 16.0).exp() as f32;
                }
            }
        }
        k
    })
}

/// `Mth.fastInvSqrt`.
#[inline]
fn fast_inv_sqrt(x: f64) -> f64 {
    let xhalf = 0.5 * x;
    let i = 6910469410427058090i64 - (x.to_bits() as i64 >> 1);
    let x = f64::from_bits(i as u64);
    x * (1.5 - xhalf * x * x)
}

/// `getBuryContribution`.
#[inline]
fn bury(dx: f32, dy: f32, dz: f32) -> f32 {
    let d = dx * dx + dy * dy + dz * dz;
    if d >= 36.0 {
        0.0
    } else {
        1.0 - (d as f64).sqrt() as f32 / 6.0
    }
}

/// `getBeardContribution`.
#[inline]
fn beard(dx: i32, dy: i32, dz: i32, y_to_ground: i32, k: &[f32]) -> f32 {
    let xi = dx + RADIUS;
    let yi = dy + RADIUS;
    let zi = dz + RADIUS;
    let in_range = |v: i32| (0..KERNEL_SIZE as i32).contains(&v);
    if in_range(xi) && in_range(yi) && in_range(zi) {
        let dyo = y_to_ground as f32 + 0.5;
        let dsq = dx as f32 * dx as f32 + dyo * dyo + dz as f32 * dz as f32;
        let v = -dyo * fast_inv_sqrt((dsq / 2.0) as f64) as f32 / 2.0;
        v * k[(zi as usize) * KERNEL_SIZE * KERNEL_SIZE + (xi as usize) * KERNEL_SIZE + yi as usize]
    } else {
        0.0
    }
}

impl Rigid {
    #[inline]
    fn contribution(&self, bx: i32, by: i32, bz: i32, k: &[f32]) -> f32 {
        let dx = 0.max((self.min[0] - bx).max(bx - self.max[0]));
        let dz = 0.max((self.min[2] - bz).max(bz - self.max[2]));
        let ground_y = self.min[1] + self.ground_delta;
        let dy_to_ground = by - ground_y;
        match self.adjustment {
            Adjustment::None => 0.0,
            Adjustment::Bury => bury(dx as f32, dy_to_ground as f32 / 2.0, dz as f32),
            Adjustment::BeardThin => beard(dx, dy_to_ground, dz, dy_to_ground, k) * 0.8,
            Adjustment::BeardBox => {
                let dy = 0.max((ground_y - by).max(by - self.max[1]));
                beard(dx, dy, dz, dy_to_ground, k) * 0.8
            }
            Adjustment::Encapsulate => {
                let dy = 0.max((self.min[1] - by).max(by - self.max[1]));
                bury(dx as f32 / 2.0, dy as f32 / 2.0, dz as f32 / 2.0) * 0.8
            }
        }
    }

    /// Blocks outside this box get an exact 0 from `contribution`.
    fn reach(&self) -> ([i32; 3], [i32; 3]) {
        let ground_y = self.min[1] + self.ground_delta;
        (
            [self.min[0] - RADIUS, self.min[1].min(ground_y) - RADIUS, self.min[2] - RADIUS],
            [self.max[0] + RADIUS, self.max[1].max(ground_y) + RADIUS, self.max[2] + RADIUS],
        )
    }
}

impl Junction {
    fn reach(&self) -> ([i32; 3], [i32; 3]) {
        ([self.x - RADIUS, self.ground_y - RADIUS, self.z - RADIUS], [self.x + RADIUS, self.ground_y + RADIUS, self.z + RADIUS])
    }
}

/// Where the beard buffer may be non-zero, in volume index space
/// (inclusive bounds).
#[derive(Clone, Copy, Debug)]
pub struct Reach {
    pub lo: [usize; 3],
    pub hi: [usize; 3],
}

impl Reach {
    pub fn touches(&self, lo: [usize; 3], hi: [usize; 3]) -> bool {
        (0..3).all(|a| lo[a] <= self.hi[a] && hi[a] >= self.lo[a])
    }
}

impl Beardifier {
    /// Parse the flat layout; `None` for an empty or malformed array.
    pub fn from_flat(flat: &[i32]) -> Option<Self> {
        if flat.len() < 8 {
            return None;
        }
        let n_rigid = usize::try_from(flat[0]).ok()?;
        let n_junction = usize::try_from(flat[1]).ok()?;
        if flat.len() != 8 + n_rigid * 8 + n_junction * 3 {
            return None;
        }
        let affected_min = [flat[2], flat[3], flat[4]];
        let affected_max = [flat[5], flat[6], flat[7]];
        let mut rigids = Vec::with_capacity(n_rigid);
        let mut at = 8;
        for _ in 0..n_rigid {
            let r = &flat[at..at + 8];
            let adjustment = match r[6] {
                0 => Adjustment::None,
                1 => Adjustment::Bury,
                2 => Adjustment::BeardThin,
                3 => Adjustment::BeardBox,
                4 => Adjustment::Encapsulate,
                _ => return None,
            };
            rigids.push(Rigid { min: [r[0], r[1], r[2]], max: [r[3], r[4], r[5]], adjustment, ground_delta: r[7] });
            at += 8;
        }
        let mut junctions = Vec::with_capacity(n_junction);
        for _ in 0..n_junction {
            let j = &flat[at..at + 3];
            junctions.push(Junction { x: j[0], ground_y: j[1], z: j[2] });
            at += 3;
        }
        Some(Self { rigids, junctions, affected_min, affected_max })
    }

    /// `sampleValue` at one block.
    pub fn sample(&self, bx: i32, by: i32, bz: i32) -> f32 {
        let p = [bx, by, bz];
        if (0..3).any(|a| p[a] < self.affected_min[a] || p[a] > self.affected_max[a]) {
            return 0.0;
        }
        let k = kernel();
        let mut v = 0.0f32;
        for r in &self.rigids {
            v += r.contribution(bx, by, bz, k);
        }
        for j in &self.junctions {
            v += beard(bx - j.x, by - j.ground_y, bz - j.z, by - j.ground_y, k) * 0.4;
        }
        v
    }

    /// The beard value per block of a step-1 volume, in `Volume::index`
    /// order, with the region that may be non-zero; `None` when nothing
    /// reaches the volume (every value would be 0).
    pub fn sample_volume(&self, vol: &Volume) -> Option<(Vec<f32>, Reach)> {
        assert_eq!(vol.step, [1, 1, 1]);
        // Clip a block box to the volume and the affected box, as index bounds.
        let clip = |lo: [i32; 3], hi: [i32; 3]| -> Option<([usize; 3], [usize; 3])> {
            let mut l = [0usize; 3];
            let mut h = [0usize; 3];
            for a in 0..3 {
                let lo_b = lo[a].max(self.affected_min[a]).max(vol.min[a]);
                let hi_b = hi[a].min(self.affected_max[a]).min(vol.max_block(a));
                if lo_b > hi_b {
                    return None;
                }
                l[a] = (lo_b - vol.min[a]) as usize;
                h[a] = (hi_b - vol.min[a]) as usize;
            }
            Some((l, h))
        };
        let k = kernel();
        let mut out: Option<Vec<f32>> = None;
        let mut reach: Option<Reach> = None;
        let mut visit = |lo: [i32; 3], hi: [i32; 3], f: &dyn Fn(i32, i32, i32) -> f32| {
            let Some((l, h)) = clip(lo, hi) else { return };
            let buf = out.get_or_insert_with(|| vec![0.0f32; vol.len()]);
            for z in l[2]..=h[2] {
                let bz = vol.min[2] + z as i32;
                for x in l[0]..=h[0] {
                    let bx = vol.min[0] + x as i32;
                    let base = vol.index(x, l[1], z);
                    for (i, y) in (l[1]..=h[1]).enumerate() {
                        buf[base + i] += f(bx, vol.min[1] + y as i32, bz);
                    }
                }
            }
            reach = Some(match reach {
                None => Reach { lo: l, hi: h },
                Some(r) => Reach { lo: std::array::from_fn(|a| r.lo[a].min(l[a])), hi: std::array::from_fn(|a| r.hi[a].max(h[a])) },
            });
        };
        for r in &self.rigids {
            if r.adjustment == Adjustment::None {
                continue;
            }
            let (lo, hi) = r.reach();
            visit(lo, hi, &|bx, by, bz| r.contribution(bx, by, bz, k));
        }
        for j in &self.junctions {
            let (lo, hi) = j.reach();
            visit(lo, hi, &|bx, by, bz| beard(bx - j.x, by - j.ground_y, bz - j.z, by - j.ground_y, k) * 0.4);
        }
        Some((out?, reach?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_matches_java() {
        // Bits printed by the same expressions on JDK 25.
        assert_eq!(fast_inv_sqrt(2.0).to_bits(), 0x3fe69f2aee57a7ac);
        assert_eq!(fast_inv_sqrt(0.5).to_bits(), 0x3ff69f2aee57a7ac);
        assert_eq!(fast_inv_sqrt(37.5).to_bits(), 0x3fc4df585eb9dc78);
        assert_eq!(fast_inv_sqrt(1234.25).to_bits(), 0x3f9d257d38739544);
        let k = kernel();
        let idx = (1 + RADIUS) as usize * 576 + (3 + RADIUS) as usize * 24 + (-2 + RADIUS) as usize;
        assert_eq!(beard(3, -2, 1, -2, k).to_bits(), (f32::from_bits(0x3e9aedda) * k[idx]).to_bits());
        assert_eq!(bury(3.0, -5.0 / 2.0, 2.0).to_bits(), 0x3e8999fe);
    }

    #[test]
    fn kernel_spot_values() {
        // Centre and one corner, (float) Math.pow(Math.E, -d / 16).
        let k = kernel();
        assert_eq!(k[12 * 576 + 12 * 24 + 12].to_bits(), ((-0.25f64 / 16.0).exp() as f32).to_bits());
        assert_eq!(k[0].to_bits(), ((-(144.0 + 132.25 + 144.0) / 16.0f64).exp() as f32).to_bits());
    }

    #[test]
    fn sparse_walk_matches_dense_sample() {
        let b = Beardifier {
            rigids: vec![
                Rigid { min: [3, 60, -5], max: [12, 68, 4], adjustment: Adjustment::BeardBox, ground_delta: 1 },
                Rigid { min: [-20, 40, 20], max: [-2, 50, 30], adjustment: Adjustment::Bury, ground_delta: 0 },
                Rigid { min: [8, 70, 8], max: [9, 72, 9], adjustment: Adjustment::Encapsulate, ground_delta: 0 },
                Rigid { min: [0, 0, 0], max: [1, 1, 1], adjustment: Adjustment::None, ground_delta: 0 },
            ],
            junctions: vec![Junction { x: 13, ground_y: 61, z: 5 }],
            affected_min: [-44, 16, -29],
            affected_max: [37, 96, 54],
        };
        let vol = Volume::chunk(0, 0, -64, 384);
        let (buf, reach) = b.sample_volume(&vol).expect("reaches");
        let mut nonzero = 0;
        vol.for_each(|i, x, y, z| {
            assert_eq!(buf[i].to_bits(), b.sample(x, y, z).to_bits(), "at {x} {y} {z}");
            if buf[i] != 0.0 {
                nonzero += 1;
                let l = [x as usize, (y + 64) as usize, z as usize];
                assert!(reach.touches(l, l));
            }
        });
        assert!(nonzero > 1000, "{nonzero}");
    }

    #[test]
    fn out_of_reach_is_none() {
        let b = Beardifier::from_flat(&[1, 0, -100, 0, -100, -80, 20, -80, -76, 0, -76, -70, 10, -70, 3, 0]).unwrap();
        assert!(b.sample_volume(&Volume::chunk(0, 0, -64, 384)).is_none());
        assert!(Beardifier::from_flat(&[1, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }
}
