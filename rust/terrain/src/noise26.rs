//! 26.3 noise stack: float Perlin octaves seeded per octave by name.
//!
//! Mirrors `net.minecraft.world.level.levelgen.synth.{GradientNoise,
//! PerlinNoise, NoiseStack, NormalNoise}` from 26.3-pre-1. The layout
//! differs from the pre-26.3 `NormalNoise` in three ways that all
//! change the output bits: values are `f32` from the gradient dot
//! product outward, each octave is its own `PerlinNoise` seeded with
//! `fromHashOf("octave_N")` off two forked positional factories, and
//! the amplitude normalisation is folded into a per-layer float factor.
//! `perlin.rs` (Ferrite's f64 port) is kept for the legacy blended
//! noise and older code paths; nothing here calls into it.

use std::cell::Cell;

use crate::xoroshiro::{XoroshiroPositionalRandomFactory, XoroshiroRandomSource};

// Both are const already; the lint misreads the const block.
thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static LAYER_SAMPLES: Cell<u64> = const { Cell::new(0) };
    #[allow(clippy::missing_const_for_thread_local)]
    static POINT_LAYER_SAMPLES: Cell<u64> = const { Cell::new(0) };
}

/// Perlin lattice samples this thread has taken (one per stack layer per
/// point). Per thread so worker threads share no cache line; read it
/// around a workload on the same thread to size the work.
pub fn layer_samples() -> u64 {
    LAYER_SAMPLES.with(Cell::get)
}

/// Subset of [`layer_samples`] taken through the point path (`get`, `get_2d`).
pub fn point_layer_samples() -> u64 {
    POINT_LAYER_SAMPLES.with(Cell::get)
}

#[inline]
fn count_layers(n: usize) {
    LAYER_SAMPLES.with(|c| c.set(c.get() + n as u64));
}

#[inline]
fn count_point_layers(n: usize) {
    POINT_LAYER_SAMPLES.with(|c| c.set(c.get() + n as u64));
}

/// `GradientNoise.GRADIENT`, 16 entries.
const GRADIENT: [[i32; 3]; 16] = [
    [1, 1, 0], [-1, 1, 0], [1, -1, 0], [-1, -1, 0],
    [1, 0, 1], [-1, 0, 1], [1, 0, -1], [-1, 0, -1],
    [0, 1, 1], [0, -1, 1], [0, 1, -1], [0, -1, -1],
    [1, 1, 0], [0, -1, 1], [-1, 1, 0], [0, -1, -1],
];

/// `GradientNoise.ROUND_OFF` as a double (3.3554432E7).
const ROUND_OFF: f64 = 33554432.0;
/// `NormalNoise.INPUT_FACTOR`.
pub const INPUT_FACTOR: f64 = 1.0181268882175227;
/// `PerlinNoise.STANDARD_DEVIATION`.
const STANDARD_DEVIATION: f64 = 0.2702247831245211;
/// `NormalNoise.TARGET_DEVIATION`.
const TARGET_DEVIATION: f64 = 0.3333333333333333;

#[inline]
fn half_round_off() -> f64 {
    // Math.nextDown(16777216.0): the largest double below 2^24.
    f64::from_bits(16777216.0f64.to_bits() - 1)
}

/// `GradientNoise.wrap`.
#[inline]
pub fn wrap(x: f64) -> f64 {
    let half = half_round_off();
    if x >= -half && x < half {
        x
    } else {
        x - (x / ROUND_OFF + 0.5).floor() * ROUND_OFF
    }
}

/// `Mth.smoothstep(float)`.
#[inline]
pub fn smoothstep(x: f32) -> f32 {
    x * x * x * (x * (x * 6.0 - 15.0) + 10.0)
}

/// `Mth.lerp(float, float, float)`.
#[inline]
pub fn lerp(alpha: f32, p0: f32, p1: f32) -> f32 {
    p0 + alpha * (p1 - p0)
}

/// `Mth.lerp2(float ...)`.
#[inline]
pub fn lerp2(a1: f32, a2: f32, x00: f32, x10: f32, x01: f32, x11: f32) -> f32 {
    lerp(a2, lerp(a1, x00, x10), lerp(a1, x01, x11))
}

/// `Mth.lerp3(float ...)`.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn lerp3(
    a1: f32, a2: f32, a3: f32,
    x000: f32, x100: f32, x010: f32, x110: f32,
    x001: f32, x101: f32, x011: f32, x111: f32,
) -> f32 {
    lerp(a3, lerp2(a1, a2, x000, x100, x010, x110), lerp2(a1, a2, x001, x101, x011, x111))
}

#[inline]
fn grad_dot(hash: i32, x: f32, y: f32, z: f32) -> f32 {
    let g = GRADIENT[(hash & 15) as usize];
    g[0] as f32 * x + g[1] as f32 * y + g[2] as f32 * z
}

/// One `PerlinNoise` octave: a 256-entry permutation and a double offset.
#[derive(Clone, Debug)]
pub struct PerlinNoise {
    perms: [u8; 256],
    offset_x: f64,
    offset_y: f64,
    offset_z: f64,
}

impl PerlinNoise {
    /// `new PerlinNoise(random)`: three doubles scaled by 256, then a
    /// Fisher-Yates shuffle drawing `nextInt(256 - i)`.
    pub fn new(random: &mut XoroshiroRandomSource) -> Self {
        let offset_x = random.next_double() * 256.0;
        let offset_y = random.next_double() * 256.0;
        let offset_z = random.next_double() * 256.0;
        let mut perms = [0u8; 256];
        for (i, p) in perms.iter_mut().enumerate() {
            *p = i as u8;
        }
        for i in 0..256usize {
            let offset = random.next_int_bounded((256 - i) as i32) as usize;
            perms.swap(i, offset + i);
        }
        Self { perms, offset_x, offset_y, offset_z }
    }

    #[inline]
    fn permute(&self, x: i32) -> i32 {
        self.perms[(x & 0xFF) as usize] as i32
    }

    /// `PerlinNoise.get(double, double, double)`.
    #[inline]
    pub fn get(&self, x: f64, y: f64, z: f64) -> f32 {
        let x = wrap(x) + self.offset_x;
        let y = wrap(y) + self.offset_y;
        let z = wrap(z) + self.offset_z;
        let fx = x.floor() as i32;
        let fy = y.floor() as i32;
        let fz = z.floor() as i32;
        let rx = (x - fx as f64) as f32;
        let ry = (y - fy as f64) as f32;
        let rz = (z - fz as f64) as f32;
        self.sample_and_lerp(fx, fy, fz, rx, ry, rz, ry)
    }

    /// One point of `PerlinNoise.addToVolume`: same lattice, but the
    /// dot products are built as `dotXz + gy * ry`, which rounds
    /// differently from `gradDot`. `x, y, z` are the scaled coordinates.
    #[inline]
    pub fn get_volume_style(&self, x: f64, y: f64, z: f64) -> f32 {
        let x = wrap(x) + self.offset_x;
        let y = wrap(y) + self.offset_y;
        let z = wrap(z) + self.offset_z;
        let fx = x.floor() as i32;
        let fy = y.floor() as i32;
        let fz = z.floor() as i32;
        let rx = (x - fx as f64) as f32;
        let ry = (y - fy as f64) as f32;
        let rz = (z - fz as f64) as f32;
        self.volume_corners_lerp(fx, fy, fz, rx, ry, rz, ry)
    }

    /// `PerlinNoise.get(double, double)`: 2D sample at y = 0.
    #[inline]
    pub fn get_2d(&self, x: f64, y: f64) -> f32 {
        self.get(wrap(x), 0.0, wrap(y))
    }

    /// `PerlinNoise.sampleAndLerp`: `ry` feeds the gradients,
    /// `ry_smooth` feeds the smoothstep (they differ for smeared noise).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn sample_and_lerp(&self, x: i32, y: i32, z: i32, rx: f32, ry: f32, rz: f32, ry_smooth: f32) -> f32 {
        let x0 = self.permute(x);
        let x1 = self.permute(x + 1);
        let xy00 = self.permute(x0 + y);
        let xy01 = self.permute(x0 + y + 1);
        let xy10 = self.permute(x1 + y);
        let xy11 = self.permute(x1 + y + 1);
        let d000 = grad_dot(self.permute(xy00 + z), rx, ry, rz);
        let d100 = grad_dot(self.permute(xy10 + z), rx - 1.0, ry, rz);
        let d010 = grad_dot(self.permute(xy01 + z), rx, ry - 1.0, rz);
        let d110 = grad_dot(self.permute(xy11 + z), rx - 1.0, ry - 1.0, rz);
        let d001 = grad_dot(self.permute(xy00 + z + 1), rx, ry, rz - 1.0);
        let d101 = grad_dot(self.permute(xy10 + z + 1), rx - 1.0, ry, rz - 1.0);
        let d011 = grad_dot(self.permute(xy01 + z + 1), rx, ry - 1.0, rz - 1.0);
        let d111 = grad_dot(self.permute(xy11 + z + 1), rx - 1.0, ry - 1.0, rz - 1.0);
        let ax = smoothstep(rx);
        let ay = smoothstep(ry_smooth);
        let az = smoothstep(rz);
        lerp3(ax, ay, az, d000, d100, d010, d110, d001, d101, d011, d111)
    }

    /// The `addToVolume` arithmetic for one lattice cell.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn volume_corners_lerp(&self, x: i32, y: i32, z: i32, rx: f32, ry: f32, rz: f32, ry_smooth: f32) -> f32 {
        let x0 = self.permute(x);
        let x1 = self.permute(x + 1);
        let xy00 = self.permute(x0 + y);
        let xy01 = self.permute(x0 + y + 1);
        let xy10 = self.permute(x1 + y);
        let xy11 = self.permute(x1 + y + 1);
        let g = |h: i32| GRADIENT[(self.permute(h) & 15) as usize];
        let dot_xz = |g: [i32; 3], x: f32, z: f32| g[0] as f32 * x + g[2] as f32 * z;
        let g000 = g(xy00 + z);
        let g100 = g(xy10 + z);
        let g010 = g(xy01 + z);
        let g110 = g(xy11 + z);
        let g001 = g(xy00 + z + 1);
        let g101 = g(xy10 + z + 1);
        let g011 = g(xy01 + z + 1);
        let g111 = g(xy11 + z + 1);
        let d000 = dot_xz(g000, rx, rz);
        let d100 = dot_xz(g100, rx - 1.0, rz);
        let d010 = dot_xz(g010, rx, rz);
        let d110 = dot_xz(g110, rx - 1.0, rz);
        let d001 = dot_xz(g001, rx, rz - 1.0);
        let d101 = dot_xz(g101, rx - 1.0, rz - 1.0);
        let d011 = dot_xz(g011, rx, rz - 1.0);
        let d111 = dot_xz(g111, rx - 1.0, rz - 1.0);
        let ax = smoothstep(rx);
        let ay = smoothstep(ry_smooth);
        let az = smoothstep(rz);
        lerp3(
            ax, ay, az,
            d000 + g000[1] as f32 * ry,
            d100 + g100[1] as f32 * ry,
            d010 + g010[1] as f32 * (ry - 1.0),
            d110 + g110[1] as f32 * (ry - 1.0),
            d001 + g001[1] as f32 * ry,
            d101 + g101[1] as f32 * ry,
            d011 + g011[1] as f32 * (ry - 1.0),
            d111 + g111[1] as f32 * (ry - 1.0),
        )
    }
}

/// Y points evaluated together by the lane path.
const LANES: usize = 8;

/// `Mth.lerp` on 8 lanes: `p0 + alpha * (p1 - p0)`, the scalar op order.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn lerp_ps(alpha: std::arch::x86_64::__m256, p0: std::arch::x86_64::__m256, p1: std::arch::x86_64::__m256) -> std::arch::x86_64::__m256 {
    use std::arch::x86_64::*;
    _mm256_add_ps(p0, _mm256_mul_ps(alpha, _mm256_sub_ps(p1, p0)))
}

/// 16-entry f32 table read on 8 lanes: `permutevar` on each half, then
/// bit 3 of the index picks the half.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn lookup16(lo: std::arch::x86_64::__m256, hi: std::arch::x86_64::__m256, idx: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256 {
    use std::arch::x86_64::*;
    let a = _mm256_permutevar8x32_ps(lo, idx);
    let b = _mm256_permutevar8x32_ps(hi, idx);
    _mm256_blendv_ps(a, b, _mm256_castsi256_ps(_mm256_slli_epi32::<28>(idx)))
}

/// Which `(x, z)` corner offset each of the 8 lattice corners uses:
/// 0 `(rx, rz)`, 1 `(rx-1, rz)`, 2 `(rx, rz-1)`, 3 `(rx-1, rz-1)`.
const CORNER_XZ: [usize; 8] = [0, 1, 0, 1, 2, 3, 2, 3];

impl PerlinNoise {
    /// The 8 gradient indices of the lattice cell at `(x0/x1, fy, fz)`,
    /// corner order matching `lerp3`.
    #[inline]
    fn cell_gradients(&self, x0: i32, x1: i32, fy: i32, fz: i32) -> [i32; 8] {
        let xy00 = self.permute(x0 + fy);
        let xy01 = self.permute(x0 + fy + 1);
        let xy10 = self.permute(x1 + fy);
        let xy11 = self.permute(x1 + fy + 1);
        let g = |h: i32| self.permute(h) & 15;
        [
            g(xy00 + fz),
            g(xy10 + fz),
            g(xy01 + fz),
            g(xy11 + fz),
            g(xy00 + fz + 1),
            g(xy10 + fz + 1),
            g(xy01 + fz + 1),
            g(xy11 + fz + 1),
        ]
    }

    /// `PerlinNoise.addToVolume` / `SmearedPerlinNoise.addToVolume`
    /// for one layer; `fudge` selects the smeared y treatment.
    #[allow(clippy::too_many_arguments)]
    pub fn add_to_volume(&self, out: &mut [f32], size: [usize; 3], min: [i32; 3], step: [i32; 3], xz_scale: f64, y_scale: f64, amplitude: f32, fudge: Option<f64>) {
        #[cfg(target_arch = "x86_64")]
        if size[1] >= LANES && std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: avx2 was just detected on this CPU.
            unsafe { self.add_to_volume_avx2(out, size, min, step, xz_scale, y_scale, amplitude, fudge) };
            return;
        }
        self.add_to_volume_scalar(out, size, min, step, xz_scale, y_scale, amplitude, fudge);
    }

    /// The y loop of `add_to_volume` taken `LANES` points at a time with
    /// AVX2. Every lane performs the scalar path's operations in the same
    /// order (no fused multiply-add), so the bits match. The permutation
    /// walk stays scalar per lane and yields a gradient index; the corner
    /// dot products come from a per-column 16-entry table read 8 wide,
    /// and the f64 coordinate prep and the f32 lerps run 8 wide.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
    unsafe fn add_to_volume_avx2(&self, out: &mut [f32], size: [usize; 3], min: [i32; 3], step: [i32; 3], xz_scale: f64, y_scale: f64, amplitude: f32, fudge: Option<f64>) {
        use std::arch::x86_64::*;
        assert!(out.len() >= size[0] * size[1] * size[2]);
        // SAFETY: the caller checked avx2; pointer accesses stay inside `out`.
        unsafe {
            let round_off = _mm256_set1_pd(ROUND_OFF);
            let half = _mm256_set1_pd(half_round_off());
            let neg_half = _mm256_set1_pd(-half_round_off());
            let p_half = _mm256_set1_pd(0.5);
            let zero_d = _mm256_setzero_pd();
            let off_y = _mm256_set1_pd(self.offset_y);
            let y_scale_v = _mm256_set1_pd(y_scale);
            let eps = _mm256_set1_pd(1.0e-7f32 as f64);
            let fudge_scale = _mm256_set1_pd(fudge.unwrap_or(0.0));
            let c6 = _mm256_set1_ps(6.0);
            let c15 = _mm256_set1_ps(15.0);
            let c10 = _mm256_set1_ps(10.0);
            let one = _mm256_set1_ps(1.0);
            let amp = _mm256_set1_ps(amplitude);
            let gcomp = |c: usize| -> [__m256; 2] {
                let mut t = [0.0f32; 16];
                for (g, v) in GRADIENT.iter().zip(t.iter_mut()) {
                    *v = g[c] as f32;
                }
                [_mm256_loadu_ps(t.as_ptr()), _mm256_loadu_ps(t.as_ptr().add(8))]
            };
            let gx = gcomp(0);
            let gy_tab = gcomp(1);
            let gz = gcomp(2);
            let mut gyt = [0.0f32; 16];
            _mm256_storeu_ps(gyt.as_mut_ptr(), gy_tab[0]);
            _mm256_storeu_ps(gyt.as_mut_ptr().add(8), gy_tab[1]);
            let mut index = 0usize;
            for iz in 0..size[2] {
                let bz = min[2] + iz as i32 * step[2];
                let z = wrap(bz as f64 * xz_scale) + self.offset_z;
                let fz = z.floor() as i32;
                let rz = (z - fz as f64) as f32;
                let az = _mm256_set1_ps(smoothstep(rz));
                for ix in 0..size[0] {
                    let bx = min[0] + ix as i32 * step[0];
                    let x = wrap(bx as f64 * xz_scale) + self.offset_x;
                    let fx = x.floor() as i32;
                    let rx = (x - fx as f64) as f32;
                    let x0 = self.permute(fx);
                    let x1 = self.permute(fx + 1);
                    let ax = _mm256_set1_ps(smoothstep(rx));
                    // `gx * cx + gz * cz` for all 16 gradients and the 4 (x, z) offsets.
                    let mut dtab = [[_mm256_setzero_ps(); 2]; 4];
                    let mut tab = [0.0f32; 64];
                    for (c, t) in dtab.iter_mut().enumerate() {
                        let cx = _mm256_set1_ps(if c & 1 == 0 { rx } else { rx - 1.0 });
                        let cz = _mm256_set1_ps(if c & 2 == 0 { rz } else { rz - 1.0 });
                        for h in 0..2 {
                            t[h] = _mm256_add_ps(_mm256_mul_ps(gx[h], cx), _mm256_mul_ps(gz[h], cz));
                            _mm256_storeu_ps(tab.as_mut_ptr().add(c * 16 + h * 8), t[h]);
                        }
                    }
                    let mut last_fy = i32::MIN;
                    let mut cur_g = [0i32; 8];
                    let mut iy = 0usize;
                    while iy < size[1] {
                        let mut by = [0.0f64; LANES];
                        for l in 0..LANES {
                            by[l] = (min[1] + (iy + l) as i32 * step[1]) as f64;
                        }
                        let mut fy = [0i32; LANES];
                        let mut ry = [0.0f32; LANES];
                        let mut ay_in = [0.0f32; LANES];
                        for h in 0..2 {
                            let original_y = _mm256_mul_pd(_mm256_loadu_pd(by.as_ptr().add(h * 4)), y_scale_v);
                            // wrap: identity inside +-half, else subtract a multiple of ROUND_OFF.
                            let inside = _mm256_and_pd(
                                _mm256_cmp_pd::<_CMP_GE_OQ>(original_y, neg_half),
                                _mm256_cmp_pd::<_CMP_LT_OQ>(original_y, half),
                            );
                            let q = _mm256_floor_pd(_mm256_add_pd(_mm256_div_pd(original_y, round_off), p_half));
                            let wrapped = _mm256_blendv_pd(_mm256_sub_pd(original_y, _mm256_mul_pd(q, round_off)), original_y, inside);
                            let y = _mm256_add_pd(wrapped, off_y);
                            let fl = _mm256_floor_pd(y);
                            let ry_d = _mm256_sub_pd(y, fl);
                            _mm_storeu_si128(fy.as_mut_ptr().add(h * 4) as *mut __m128i, _mm256_cvttpd_epi32(fl));
                            let (r, a) = if fudge.is_some() {
                                let a = _mm256_cvtpd_ps(ry_d);
                                let cond = _mm256_and_pd(
                                    _mm256_cmp_pd::<_CMP_GE_OQ>(original_y, zero_d),
                                    _mm256_cmp_pd::<_CMP_LT_OQ>(original_y, ry_d),
                                );
                                let limit = _mm256_blendv_pd(ry_d, original_y, cond);
                                let q = _mm256_floor_pd(_mm256_add_pd(_mm256_div_pd(limit, fudge_scale), eps));
                                let fudged = _mm256_mul_pd(_mm256_cvtepi32_pd(_mm256_cvttpd_epi32(q)), fudge_scale);
                                (_mm256_cvtpd_ps(_mm256_sub_pd(ry_d, fudged)), a)
                            } else {
                                let r = _mm256_cvtpd_ps(ry_d);
                                (r, r)
                            };
                            _mm_storeu_ps(ry.as_mut_ptr().add(h * 4), r);
                            _mm_storeu_ps(ay_in.as_mut_ptr().add(h * 4), a);
                        }
                        // smoothstep(x) = x*x*x*(x*(x*6-15)+10).
                        let xs = _mm256_loadu_ps(ay_in.as_ptr());
                        let ay = _mm256_mul_ps(
                            _mm256_mul_ps(_mm256_mul_ps(xs, xs), xs),
                            _mm256_add_ps(_mm256_mul_ps(xs, _mm256_sub_ps(_mm256_mul_ps(xs, c6), c15)), c10),
                        );
                        let ryv = _mm256_loadu_ps(ry.as_ptr());
                        let ry1 = _mm256_sub_ps(ryv, one);
                        // The 8 corners: `d + gy * ry` per lane, d and gy read by gradient index.
                        // fy rises with the lane, so equal ends mean one cell for the group.
                        let mut v = [_mm256_setzero_ps(); 8];
                        if fy[0] == fy[LANES - 1] {
                            if fy[0] != last_fy {
                                cur_g = self.cell_gradients(x0, x1, fy[0], fz);
                                last_fy = fy[0];
                            }
                            for k in 0..8 {
                                let g = cur_g[k] as usize;
                                let r = if k & 2 == 0 { ryv } else { ry1 };
                                let d = _mm256_set1_ps(tab[CORNER_XZ[k] * 16 + g]);
                                v[k] = _mm256_add_ps(d, _mm256_mul_ps(_mm256_set1_ps(gyt[g]), r));
                            }
                        } else {
                            let mut gidx = [[0i32; LANES]; 8];
                            for l in 0..LANES {
                                if fy[l] != last_fy {
                                    cur_g = self.cell_gradients(x0, x1, fy[l], fz);
                                    last_fy = fy[l];
                                }
                                for k in 0..8 {
                                    gidx[k][l] = cur_g[k];
                                }
                            }
                            for k in 0..8 {
                                let idx = _mm256_loadu_si256(gidx[k].as_ptr() as *const __m256i);
                                let r = if k & 2 == 0 { ryv } else { ry1 };
                                let t = dtab[CORNER_XZ[k]];
                                let d = lookup16(t[0], t[1], idx);
                                v[k] = _mm256_add_ps(d, _mm256_mul_ps(lookup16(gy_tab[0], gy_tab[1], idx), r));
                            }
                        }
                        let front = lerp_ps(ay, lerp_ps(ax, v[0], v[1]), lerp_ps(ax, v[2], v[3]));
                        let back = lerp_ps(ay, lerp_ps(ax, v[4], v[5]), lerp_ps(ax, v[6], v[7]));
                        let res = _mm256_mul_ps(amp, lerp_ps(az, front, back));
                        let n = LANES.min(size[1] - iy);
                        let dst = out.as_mut_ptr().add(index);
                        if n == LANES {
                            _mm256_storeu_ps(dst, _mm256_add_ps(_mm256_loadu_ps(dst), res));
                        } else {
                            let mut tail = [0.0f32; LANES];
                            _mm256_storeu_ps(tail.as_mut_ptr(), res);
                            for l in 0..n {
                                *dst.add(l) += tail[l];
                            }
                        }
                        index += n;
                        iy += n;
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_to_volume_scalar(&self, out: &mut [f32], size: [usize; 3], min: [i32; 3], step: [i32; 3], xz_scale: f64, y_scale: f64, amplitude: f32, fudge: Option<f64>) {
        let mut index = 0usize;
        for iz in 0..size[2] {
            let bz = min[2] + iz as i32 * step[2];
            let z = wrap(bz as f64 * xz_scale) + self.offset_z;
            let fz = z.floor() as i32;
            let rz = (z - fz as f64) as f32;
            let az = smoothstep(rz);
            for ix in 0..size[0] {
                let bx = min[0] + ix as i32 * step[0];
                let x = wrap(bx as f64 * xz_scale) + self.offset_x;
                let fx = x.floor() as i32;
                let rx = (x - fx as f64) as f32;
                let x0 = self.permute(fx);
                let x1 = self.permute(fx + 1);
                let ax = smoothstep(rx);
                let mut last_fy = i32::MIN;
                let mut d = [0.0f32; 8];
                let mut gy = [0.0f32; 8];
                for iy in 0..size[1] {
                    let by = min[1] + iy as i32 * step[1];
                    let original_y = by as f64 * y_scale;
                    let y = wrap(original_y) + self.offset_y;
                    let fy = y.floor() as i32;
                    let ry_d = y - fy as f64;
                    let (ry, ay) = match fudge {
                        None => {
                            let r = ry_d as f32;
                            (r, smoothstep(r))
                        }
                        Some(scale) => {
                            let ay = smoothstep(ry_d as f32);
                            let limit = if original_y >= 0.0 && original_y < ry_d { original_y } else { ry_d };
                            let fudged = (limit / scale + 1.0e-7f32 as f64).floor() as i32 as f64 * scale;
                            ((ry_d - fudged) as f32, ay)
                        }
                    };
                    if last_fy != fy {
                        let xy00 = self.permute(x0 + fy);
                        let xy01 = self.permute(x0 + fy + 1);
                        let xy10 = self.permute(x1 + fy);
                        let xy11 = self.permute(x1 + fy + 1);
                        let g = |h: i32| GRADIENT[(self.permute(h) & 15) as usize];
                        let corners = [
                            (g(xy00 + fz), rx, rz),
                            (g(xy10 + fz), rx - 1.0, rz),
                            (g(xy01 + fz), rx, rz),
                            (g(xy11 + fz), rx - 1.0, rz),
                            (g(xy00 + fz + 1), rx, rz - 1.0),
                            (g(xy10 + fz + 1), rx - 1.0, rz - 1.0),
                            (g(xy01 + fz + 1), rx, rz - 1.0),
                            (g(xy11 + fz + 1), rx - 1.0, rz - 1.0),
                        ];
                        for (k, (gr, cx, cz)) in corners.iter().enumerate() {
                            d[k] = gr[0] as f32 * cx + gr[2] as f32 * cz;
                            gy[k] = gr[1] as f32;
                        }
                        last_fy = fy;
                    }
                    out[index] += amplitude
                        * lerp3(
                            ax, ay, az,
                            d[0] + gy[0] * ry,
                            d[1] + gy[1] * ry,
                            d[2] + gy[2] * (ry - 1.0),
                            d[3] + gy[3] * (ry - 1.0),
                            d[4] + gy[4] * ry,
                            d[5] + gy[5] * ry,
                            d[6] + gy[6] * (ry - 1.0),
                            d[7] + gy[7] * (ry - 1.0),
                        );
                    index += 1;
                }
            }
        }
    }
}

/// `SmearedPerlinNoise`: a Perlin octave whose gradient y offset is
/// quantised to `fudge_y_scale` (the legacy blended terrain noise).
#[derive(Clone, Debug)]
pub struct SmearedPerlinNoise {
    pub base: PerlinNoise,
    pub fudge_y_scale: f64,
}

impl SmearedPerlinNoise {
    pub fn new(random: &mut XoroshiroRandomSource, fudge_y_scale: f64) -> Self {
        Self { base: PerlinNoise::new(random), fudge_y_scale }
    }

    #[inline]
    fn fudge_y(&self, original_y: f64, relative_y: f64) -> f64 {
        let limit = if original_y >= 0.0 && original_y < relative_y { original_y } else { relative_y };
        (limit / self.fudge_y_scale + 1.0e-7f32 as f64).floor() as i32 as f64 * self.fudge_y_scale
    }

    /// `SmearedPerlinNoise.get`.
    #[inline]
    pub fn get(&self, x: f64, y_in: f64, z: f64) -> f32 {
        let b = &self.base;
        let x = wrap(x) + b.offset_x;
        let y = wrap(y_in) + b.offset_y;
        let z = wrap(z) + b.offset_z;
        let fx = x.floor() as i32;
        let fy = y.floor() as i32;
        let fz = z.floor() as i32;
        let rx = (x - fx as f64) as f32;
        let ry = y - fy as f64;
        let rz = (z - fz as f64) as f32;
        let fudged = (ry - self.fudge_y(y_in, ry)) as f32;
        b.sample_and_lerp(fx, fy, fz, rx, fudged, rz, ry as f32)
    }

    /// One point of `SmearedPerlinNoise.addToVolume`.
    #[inline]
    pub fn get_volume_style(&self, x: f64, y_in: f64, z: f64) -> f32 {
        let b = &self.base;
        let x = wrap(x) + b.offset_x;
        let y = wrap(y_in) + b.offset_y;
        let z = wrap(z) + b.offset_z;
        let fx = x.floor() as i32;
        let fy = y.floor() as i32;
        let fz = z.floor() as i32;
        let rx = (x - fx as f64) as f32;
        let ry = y - fy as f64;
        let rz = (z - fz as f64) as f32;
        let fudged = (ry - self.fudge_y(y_in, ry)) as f32;
        b.volume_corners_lerp(fx, fy, fz, rx, fudged, rz, ry as f32)
    }
}

/// A stack layer's noise: plain or smeared Perlin.
#[derive(Clone, Debug)]
pub enum LayerNoise {
    Perlin(PerlinNoise),
    Smeared(SmearedPerlinNoise),
}

impl LayerNoise {
    #[inline]
    pub fn get(&self, x: f64, y: f64, z: f64) -> f32 {
        match self {
            LayerNoise::Perlin(p) => p.get(x, y, z),
            LayerNoise::Smeared(s) => s.get(x, y, z),
        }
    }

    #[inline]
    pub fn get_2d(&self, x: f64, y: f64) -> f32 {
        match self {
            LayerNoise::Perlin(p) => p.get_2d(x, y),
            LayerNoise::Smeared(s) => s.get(wrap(x), 0.0, wrap(y)),
        }
    }

    #[inline]
    pub fn get_volume_style(&self, x: f64, y: f64, z: f64) -> f32 {
        match self {
            LayerNoise::Perlin(p) => p.get_volume_style(x, y, z),
            LayerNoise::Smeared(s) => s.get_volume_style(x, y, z),
        }
    }
}

/// `NoiseStack.Layer`.
#[derive(Clone, Debug)]
pub struct Layer {
    pub noise: LayerNoise,
    pub frequency: f64,
    pub amplitude: f32,
}

/// `NoiseStack` of Perlin layers, summed in float.
#[derive(Clone, Debug)]
pub struct NoiseStack {
    pub layers: Vec<Layer>,
}

impl NoiseStack {
    /// `NoiseStack.get(x, y, z)`.
    #[inline]
    pub fn get(&self, x: f64, y: f64, z: f64) -> f32 {
        count_layers(self.layers.len());
        count_point_layers(self.layers.len());
        let mut value = 0.0f32;
        for layer in &self.layers {
            let f = layer.frequency;
            value += layer.amplitude * layer.noise.get(x * f, y * f, z * f);
        }
        value
    }

    /// `NoiseStack.get(x, y)`.
    #[inline]
    pub fn get_2d(&self, x: f64, y: f64) -> f32 {
        count_layers(self.layers.len());
        count_point_layers(self.layers.len());
        let mut value = 0.0f32;
        for layer in &self.layers {
            let f = layer.frequency;
            value += layer.amplitude * layer.noise.get_2d(x * f, y * f);
        }
        value
    }

    /// One point of `NoiseStack.addToVolume` into a zero-filled buffer:
    /// block coordinates times `scale * frequency`, per-layer amplitude
    /// `amplitude * layer.amplitude`, summed in layer order.
    #[inline]
    pub fn get_volume(&self, bx: i32, by: i32, bz: i32, xz_scale: f64, y_scale: f64, amplitude: f32) -> f32 {
        count_layers(self.layers.len());
        let mut value = 0.0f32;
        for layer in &self.layers {
            let f = layer.frequency;
            let fxz = xz_scale * f;
            let fy = y_scale * f;
            let a = amplitude * layer.amplitude;
            value += a * layer.noise.get_volume_style(bx as f64 * fxz, by as f64 * fy, bz as f64 * fxz);
        }
        value
    }

    /// `NoiseStack.addToVolume` into `out` (which the caller has
    /// filled): layer outer, then z, x, y with the permutation and
    /// gradient work reused per column, as `PerlinNoise.addToVolume`.
    pub fn add_to_volume(&self, out: &mut [f32], size: [usize; 3], min: [i32; 3], step: [i32; 3], xz_scale: f64, y_scale: f64, amplitude: f32) {
        count_layers(self.layers.len() * size[0] * size[1] * size[2]);
        for layer in &self.layers {
            let fxz = xz_scale * layer.frequency;
            let fy = y_scale * layer.frequency;
            let a = amplitude * layer.amplitude;
            match &layer.noise {
                LayerNoise::Perlin(p) => p.add_to_volume(out, size, min, step, fxz, fy, a, None),
                LayerNoise::Smeared(s) => s.base.add_to_volume(out, size, min, step, fxz, fy, a, Some(s.fudge_y_scale)),
            }
        }
    }

    /// `BlendedNoise.createFbm`: `-first_octave + 1` smeared octaves,
    /// coarsest first, drawn from one random source.
    pub fn create_fbm(random: &mut XoroshiroRandomSource, first_octave: i32, smear_scale_y: f64, mut value_factor: f64) -> Self {
        assert!(first_octave <= 0, "firstOctave>0");
        let octaves = -first_octave + 1;
        let mut factor = 1.0f64;
        value_factor /= 2f64.powi(octaves) - 1.0;
        let mut layers = Vec::with_capacity(octaves as usize);
        for _ in (0..octaves).rev() {
            layers.push(Layer {
                noise: LayerNoise::Smeared(SmearedPerlinNoise::new(random, smear_scale_y * factor)),
                frequency: factor,
                amplitude: value_factor as f32,
            });
            factor /= 2.0;
            value_factor *= 2.0;
        }
        Self { layers }
    }
}

/// `NormalNoise.Normalization`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Normalization {
    Disabled,
    Enabled,
    Legacy,
}

/// `NormalNoise.Parameters`: the datapack `worldgen/noise/*.json` record.
#[derive(Clone, Debug, PartialEq)]
pub struct NoiseParameters26 {
    pub base_amplitude: f64,
    pub base_octave: i32,
    pub octave_count: i32,
    pub normalize: Normalization,
    pub amplitude_modifiers: Vec<f64>,
}

#[derive(Clone, Copy, Debug)]
struct OctaveInfo {
    octave_index: i32,
    frequency: f64,
    amplitude: f64,
}

impl NoiseParameters26 {
    /// Parse one `worldgen/noise/<name>.json` object.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or("noise parameters: not an object")?;
        let base_amplitude = obj.get("base_amplitude").map(|x| x.as_f64().ok_or("base_amplitude")).transpose()?.unwrap_or(1.0);
        let base_octave = obj.get("base_octave").and_then(|x| x.as_i64()).ok_or("base_octave missing")? as i32;
        let octave_count = obj.get("octave_count").map(|x| x.as_i64().ok_or("octave_count")).transpose()?.unwrap_or(1) as i32;
        let normalize = match obj.get("normalize") {
            None => Normalization::Enabled,
            Some(serde_json::Value::Bool(true)) => Normalization::Enabled,
            Some(serde_json::Value::Bool(false)) => Normalization::Disabled,
            Some(serde_json::Value::String(s)) if s == "legacy" => Normalization::Legacy,
            Some(other) => return Err(format!("normalize: bad value {other}")),
        };
        let amplitude_modifiers = match obj.get("amplitude_modifiers") {
            None => Vec::new(),
            Some(a) => a
                .as_array()
                .ok_or("amplitude_modifiers: not an array")?
                .iter()
                .map(|x| x.as_f64().ok_or_else(|| "amplitude_modifiers: not a number".to_string()))
                .collect::<Result<_, _>>()?,
        };
        if !amplitude_modifiers.is_empty() && amplitude_modifiers.len() != octave_count as usize {
            return Err(format!(
                "amplitude_modifiers had size {}, but octave_count was {octave_count}",
                amplitude_modifiers.len()
            ));
        }
        Ok(Self { base_amplitude, base_octave, octave_count, normalize, amplitude_modifiers })
    }

    fn modifier(&self, index: usize) -> f64 {
        if self.amplitude_modifiers.is_empty() { 1.0 } else { self.amplitude_modifiers[index] }
    }

    /// `NormalNoise.buildOctaves`.
    fn build_octaves(&self) -> Vec<OctaveInfo> {
        let mut frequency = 2f64.powi(self.base_octave);
        let mut amplitude = self.base_amplitude;
        if self.normalize != Normalization::Disabled {
            let n = self.octave_count;
            amplitude = self.base_amplitude * (0.5f64.powi(-(n - 1)) / (0.5f64.powi(-n) - 1.0));
        }
        let mut octaves = Vec::with_capacity(self.octave_count as usize);
        for i in 0..self.octave_count as usize {
            let m = self.modifier(i);
            if m != 0.0 {
                octaves.push(OctaveInfo {
                    octave_index: self.base_octave + i as i32,
                    frequency,
                    amplitude: amplitude * m,
                });
            }
            frequency *= 2.0;
            amplitude *= 0.5;
        }
        octaves
    }

    /// `NormalNoise.computeNormalizationFactor`.
    fn normalization_factor(target_amplitude: f64, octaves: &[OctaveInfo]) -> f64 {
        let mut variance = 0.0f64;
        for o in octaves {
            let d = STANDARD_DEVIATION * o.amplitude.abs();
            variance += d * d;
        }
        let deviation = variance.sqrt();
        if deviation == 0.0 {
            0.0
        } else {
            let input_sum_deviation = deviation * 2f64.sqrt();
            let target_deviation = target_amplitude * TARGET_DEVIATION;
            target_deviation / input_sum_deviation
        }
    }

    /// `NormalNoise.computeParityNormalizationFactor`.
    fn parity_normalization_factor(&self) -> f64 {
        let mut min_octave = i32::MAX;
        let mut max_octave = i32::MIN;
        for i in 0..self.octave_count as usize {
            if self.modifier(i) != 0.0 {
                min_octave = min_octave.min(i as i32);
                max_octave = max_octave.max(i as i32);
            }
        }
        let span = max_octave.wrapping_sub(min_octave);
        let expected = 0.1 * (1.0 + 1.0 / (span as f64 + 1.0));
        self.base_amplitude * 0.5 * TARGET_DEVIATION / expected
    }

    /// The `(octaves, normalizationFactor)` pair the `NormalNoise`
    /// constructor computes.
    fn compile(&self) -> (Vec<OctaveInfo>, f64) {
        let octaves = self.build_octaves();
        let target_amplitude: f64 = octaves.iter().map(|o| o.amplitude.abs()).sum();
        let mut factor = Self::normalization_factor(target_amplitude, &octaves);
        if self.normalize == Normalization::Legacy && factor != 0.0 {
            factor = self.parity_normalization_factor();
        }
        (octaves, factor)
    }

    /// `NormalNoise.create(random)`: `random` is the source vanilla gets
    /// from `positionalFactory.fromHashOf("minecraft:<name>")`.
    pub fn create(&self, random: &mut XoroshiroRandomSource) -> NoiseStack {
        let first: XoroshiroPositionalRandomFactory = random.fork_positional();
        let second: XoroshiroPositionalRandomFactory = random.fork_positional();
        let (octaves, factor) = self.compile();
        let mut layers = Vec::with_capacity(octaves.len() * 2);
        for o in &octaves {
            let seed = format!("octave_{}", o.octave_index);
            let first_noise = PerlinNoise::new(&mut first.from_hash_of(&seed));
            let second_noise = PerlinNoise::new(&mut second.from_hash_of(&seed));
            let value_factor = (factor * o.amplitude) as f32;
            layers.push(Layer { noise: LayerNoise::Perlin(first_noise), frequency: o.frequency, amplitude: value_factor });
            layers.push(Layer { noise: LayerNoise::Perlin(second_noise), frequency: o.frequency * INPUT_FACTOR, amplitude: value_factor });
        }
        NoiseStack { layers }
    }

    /// `Noises.instantiate`: seed from the world's root positional
    /// factory by the full identifier, then build.
    pub fn instantiate(&self, root: &XoroshiroPositionalRandomFactory, identifier: &str) -> NoiseStack {
        let mut random = root.from_hash_of(identifier);
        self.create(&mut random)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_is_identity_inside_range() {
        assert_eq!(wrap(123.5), 123.5);
        assert_eq!(wrap(-16777216.0), -16777216.0);
        assert_ne!(wrap(16777216.0), 16777216.0);
    }

    /// Volumes covering the lane path: the corner grid at the blended
    /// noise's fine y scale, a cave noise where every point changes cell,
    /// block resolution with a tail, and a smeared layer with big scale.
    const LANE_CASES: [([usize; 3], [i32; 3], [i32; 3], f64, f64, Option<f64>); 6] = [
        ([5, 49, 5], [-64, -64, 128], [4, 8, 4], 0.25, 0.125 / 160.0, Some(8.0 / 160.0)),
        ([5, 49, 5], [0, -64, 0], [4, 8, 4], 1.0, 1.0, None),
        ([3, 13, 2], [17, -3, -40], [1, 1, 1], 0.25, 0.25, None),
        ([5, 9, 5], [1000, 0, -1000], [4, 8, 4], 0.5, 3.0, Some(0.5)),
        ([2, 8, 2], [-99999, 200, 4321], [4, 8, 4], 0.6, 0.9, Some(0.25)),
        ([4, 24, 4], [-6000, -64, 1200], [4, 4, 4], 0.25, 0.0, None),
    ];

    #[test]
    fn lanes_match_scalar() {
        let mut rng = XoroshiroRandomSource::from_legacy_seed(1234);
        let noise = PerlinNoise::new(&mut rng);
        for (size, min, step, xz, ys, fudge) in LANE_CASES {
            let len = size[0] * size[1] * size[2];
            let mut a = vec![0.5f32; len];
            let mut b = a.clone();
            noise.add_to_volume_scalar(&mut a, size, min, step, xz, ys, 0.7, fudge);
            noise.add_to_volume(&mut b, size, min, step, xz, ys, 0.7, fudge);
            let bad = a.iter().zip(&b).filter(|(x, y)| x.to_bits() != y.to_bits()).count();
            assert_eq!(bad, 0, "size {size:?} y_scale {ys} fudge {fudge:?}");
        }
    }

    /// `cargo test -p painite-terrain --release lanes_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn lanes_timing() {
        let mut rng = XoroshiroRandomSource::from_legacy_seed(1234);
        let noise = PerlinNoise::new(&mut rng);
        for (size, min, step, xz, ys, fudge) in &LANE_CASES[..2] {
            let len = size[0] * size[1] * size[2];
            let mut buf = vec![0.0f32; len];
            let reps = 2000;
            let t = std::time::Instant::now();
            for _ in 0..reps {
                noise.add_to_volume_scalar(&mut buf, *size, *min, *step, *xz, *ys, 0.7, *fudge);
            }
            let scalar = t.elapsed().as_nanos() as f64 / (reps * len) as f64;
            let t = std::time::Instant::now();
            for _ in 0..reps {
                noise.add_to_volume(&mut buf, *size, *min, *step, *xz, *ys, 0.7, *fudge);
            }
            let lanes = t.elapsed().as_nanos() as f64 / (reps * len) as f64;
            std::hint::black_box(&buf);
            println!("size {size:?} y_scale {ys} fudge {fudge:?}: scalar {scalar:.1} ns/point, lanes {lanes:.1} ns/point");
        }
    }

    #[test]
    fn smoothstep_endpoints() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert_eq!(smoothstep(0.5), 0.5);
    }

    #[test]
    fn octaves_skip_zero_modifiers() {
        let p = NoiseParameters26 {
            base_amplitude: 1.0,
            base_octave: -9,
            octave_count: 5,
            normalize: Normalization::Enabled,
            amplitude_modifiers: vec![1.0, 1.0, 0.0, 1.0, 1.0],
        };
        let (octaves, factor) = p.compile();
        assert_eq!(octaves.len(), 4);
        assert_eq!(octaves[2].octave_index, -6);
        assert_eq!(octaves[0].frequency, 2f64.powi(-9));
        assert!(factor > 0.0);
    }

    #[test]
    fn create_is_deterministic_and_bounded() {
        let p = NoiseParameters26::from_json(&serde_json::json!({
            "base_octave": -7, "octave_count": 4, "amplitude_modifiers": [1.0, 1.0, 1.0, 1.0]
        }))
        .unwrap();
        let root = XoroshiroRandomSource::from_legacy_seed(1234).fork_positional();
        let a = p.instantiate(&root, "minecraft:test");
        let b = p.instantiate(&root, "minecraft:test");
        assert_eq!(a.layers.len(), 8);
        for i in 0..200 {
            let (x, y, z) = (i as f64 * 3.7, i as f64 * 0.5, -(i as f64) * 2.1);
            let va = a.get(x, y, z);
            assert_eq!(va.to_bits(), b.get(x, y, z).to_bits());
            assert!(va.abs() <= 2.0, "value {va} out of range");
        }
    }
}
