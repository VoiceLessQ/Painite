//! Legacy `SimplexNoise` (2D path) and the three fixed biome
//! temperature noises built on it. Only `Biome.getTemperature` uses
//! these in worldgen; they are seeded from constants, not the world.

use crate::xoroshiro::LegacyRandomSource;

const GRADIENT: [[i32; 3]; 16] = [
    [1, 1, 0], [-1, 1, 0], [1, -1, 0], [-1, -1, 0], [1, 0, 1], [-1, 0, 1], [1, 0, -1], [-1, 0, -1],
    [0, 1, 1], [0, -1, 1], [0, 1, -1], [0, -1, -1], [1, 1, 0], [0, -1, 1], [-1, 1, 0], [0, -1, -1],
];

/// `SimplexNoise(random, discardNoiseOffset)`.
pub struct SimplexNoise {
    perms: [u8; 256],
    offset_x: f64,
    offset_y: f64,
}

impl SimplexNoise {
    pub fn new(random: &mut LegacyRandomSource, discard_offset: bool) -> Self {
        let scale = if discard_offset { 0.0 } else { 256.0 };
        let offset_x = random.next_double() * scale;
        let offset_y = random.next_double() * scale;
        let _offset_z = random.next_double() * scale;
        let mut perms = [0u8; 256];
        for (i, p) in perms.iter_mut().enumerate() {
            *p = i as u8;
        }
        for i in 0..256 {
            let offset = random.next_int_bound(256 - i as i32) as usize;
            perms.swap(i, offset + i);
        }
        Self { perms, offset_x, offset_y }
    }

    #[inline]
    fn permute(&self, x: i32) -> i32 {
        self.perms[(x & 0xff) as usize] as i32
    }

    #[inline]
    fn corner(index: usize, x: f64, y: f64, base: f64) -> f64 {
        let t0 = base - x * x - y * y - 0.0 * 0.0;
        if t0 < 0.0 {
            0.0
        } else {
            let t0 = t0 * t0;
            let g = GRADIENT[index];
            t0 * t0 * (g[0] as f64 * x + g[1] as f64 * y + g[2] as f64 * 0.0)
        }
    }

    /// `SimplexNoise.get(x, y)`.
    pub fn get_2d(&self, x_in: f64, y_in: f64) -> f32 {
        let sqrt3 = 3.0f64.sqrt();
        let f2 = 0.5 * (sqrt3 - 1.0);
        let g2 = (3.0 - sqrt3) / 6.0;
        let xin = x_in + self.offset_x;
        let yin = y_in + self.offset_y;
        let s = (xin + yin) * f2;
        let i = (xin + s).floor() as i32;
        let j = (yin + s).floor() as i32;
        let t = (i + j) as f64 * g2;
        let x0 = xin - (i as f64 - t);
        let y0 = yin - (j as f64 - t);
        let (i1, j1) = if x0 > y0 { (1, 0) } else { (0, 1) };
        let x1 = x0 - i1 as f64 + g2;
        let y1 = y0 - j1 as f64 + g2;
        let x2 = x0 - 1.0 + 2.0 * g2;
        let y2 = y0 - 1.0 + 2.0 * g2;
        let ii = i & 0xff;
        let jj = j & 0xff;
        let gi0 = (self.permute(ii + self.permute(jj)) % 12) as usize;
        let gi1 = (self.permute(ii + i1 + self.permute(jj + j1)) % 12) as usize;
        let gi2 = (self.permute(ii + 1 + self.permute(jj + 1)) % 12) as usize;
        let n0 = Self::corner(gi0, x0, y0, 0.5);
        let n1 = Self::corner(gi1, x1, y1, 0.5);
        let n2 = Self::corner(gi2, x2, y2, 0.5);
        (70.0 * (n0 + n1 + n2)) as f32
    }
}

/// `Biome.TEMPERATURE_NOISE`, `FROZEN_TEMPERATURE_NOISE` and
/// `BIOME_INFO_NOISE`, seeded from the vanilla constants.
pub struct TemperatureNoises {
    temperature: SimplexNoise,
    frozen: Vec<(SimplexNoise, f64, f32)>,
    biome_info: SimplexNoise,
}

impl Default for TemperatureNoises {
    fn default() -> Self {
        Self::new()
    }
}

impl TemperatureNoises {
    pub fn new() -> Self {
        let temperature = SimplexNoise::new(&mut LegacyRandomSource::new(1234), true);
        let mut r = LegacyRandomSource::new(3456);
        let frozen = vec![
            (SimplexNoise::new(&mut r, true), 1.0, 0.142_857_15f32),
            (SimplexNoise::new(&mut r, true), 0.5, 0.285_714_3f32),
            (SimplexNoise::new(&mut r, true), 0.25, 0.571_428_6f32),
        ];
        let biome_info = SimplexNoise::new(&mut LegacyRandomSource::new(2345), true);
        Self { temperature, frozen, biome_info }
    }

    fn frozen_get(&self, x: f64, y: f64) -> f32 {
        let mut value = 0.0f32;
        for (noise, frequency, amplitude) in &self.frozen {
            value += amplitude * noise.get_2d(x * frequency, y * frequency);
        }
        value
    }

    /// `TemperatureModifier.modifyTemperature` for the `frozen` modifier.
    pub fn modify_frozen(&self, x: i32, z: i32, base: f32) -> f32 {
        let large = (self.frozen_get(x as f64 * 0.05, z as f64 * 0.05) * 7.0f32) as f64;
        let edge = self.biome_info.get_2d(x as f64 * 0.2, z as f64 * 0.2) as f64;
        if large + edge < 0.3 {
            let small = self.biome_info.get_2d(x as f64 * 0.09, z as f64 * 0.09) as f64;
            if small < 0.8 {
                return 0.2;
            }
        }
        base
    }

    /// `Biome.getHeightAdjustedTemperature`.
    pub fn height_adjusted(&self, base: f32, frozen: bool, x: i32, y: i32, z: i32, sea_level: i32) -> f32 {
        let adjusted = if frozen { self.modify_frozen(x, z, base) } else { base };
        let snow_level = sea_level + 17;
        if y > snow_level {
            let v = self.temperature.get_2d((x as f32 / 8.0f32) as f64, (z as f32 / 8.0f32) as f64) * 8.0f32;
            adjusted - (v + y as f32 - snow_level as f32) * 0.05f32 / 40.0f32
        } else {
            adjusted
        }
    }
}
