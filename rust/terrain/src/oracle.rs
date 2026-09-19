//! Reader for the `/painite density probe` output file.
//!
//! Each line is `key x y z hexbits decimal`; `key` is a router name
//! (`final_density`, `erosion`, ...) or `noise:<identifier>` for a raw
//! `Noise.get(x, y, z)` sample. The header line carries the world seed.

use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct OracleHeader {
    pub seed: i64,
    pub settings: String,
    pub min_y: i32,
    pub height: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OracleSample {
    pub key: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub bits: u32,
}

impl OracleSample {
    #[inline]
    pub fn value(&self) -> f32 {
        f32::from_bits(self.bits)
    }
}

#[derive(Clone, Debug)]
pub struct Oracle {
    pub header: OracleHeader,
    pub samples: Vec<OracleSample>,
}

impl Oracle {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let head = lines.next().ok_or("empty oracle file")?;
        let header = parse_header(head)?;
        let mut samples = Vec::new();
        for (n, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split(' ').collect();
            if f.len() < 5 {
                return Err(format!("line {}: expected 5+ fields, got {}", n + 2, f.len()));
            }
            let num = |s: &str| s.parse::<i32>().map_err(|e| format!("line {}: {e}", n + 2));
            samples.push(OracleSample {
                key: f[0].to_string(),
                x: num(f[1])?,
                y: num(f[2])?,
                z: num(f[3])?,
                bits: u32::from_str_radix(f[4], 16).map_err(|e| format!("line {}: {e}", n + 2))?,
            });
        }
        Ok(Self { header, samples })
    }
}

fn parse_header(line: &str) -> Result<OracleHeader, String> {
    let f: Vec<&str> = line.split(' ').collect();
    if f.first() != Some(&"#") || f.len() < 9 {
        return Err(format!("bad header: {line}"));
    }
    let mut seed = None;
    let mut settings = None;
    let mut min_y = None;
    let mut height = None;
    let mut i = 1;
    while i + 1 < f.len() {
        match f[i] {
            "seed" => seed = Some(f[i + 1].parse::<i64>().map_err(|e| e.to_string())?),
            "settings" => settings = Some(f[i + 1].to_string()),
            "min_y" => min_y = Some(f[i + 1].parse::<i32>().map_err(|e| e.to_string())?),
            "height" => height = Some(f[i + 1].parse::<i32>().map_err(|e| e.to_string())?),
            _ => {}
        }
        i += 2;
    }
    Ok(OracleHeader {
        seed: seed.ok_or("header: seed missing")?,
        settings: settings.ok_or("header: settings missing")?,
        min_y: min_y.ok_or("header: min_y missing")?,
        height: height.ok_or("header: height missing")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header_and_lines() {
        let text = "# seed -5 settings minecraft:overworld min_y -64 height 384 position_seed 0\n\
                    erosion 1 2 3 3f800000 1.0\n\
                    noise:minecraft:erosion 1 2 3 bf800000 -1.0\n";
        let o = Oracle::parse(text).unwrap();
        assert_eq!(o.header.seed, -5);
        assert_eq!(o.header.height, 384);
        assert_eq!(o.samples.len(), 2);
        assert_eq!(o.samples[0].value(), 1.0);
        assert_eq!(o.samples[1].key, "noise:minecraft:erosion");
        assert_eq!(o.samples[1].value(), -1.0);
    }

    #[test]
    fn rejects_short_line() {
        let text = "# seed 1 settings x min_y -64 height 384 position_seed 0\nerosion 1 2\n";
        assert!(Oracle::parse(text).is_err());
    }
}

/// The `/painite density chunk` output: one chunk column of
/// final_density in buffer index order `y + (x + z * 16) * height`.
#[derive(Clone, Debug)]
pub struct ChunkOracle {
    pub seed: i64,
    pub chunk_x: i32,
    pub chunk_z: i32,
    pub min_y: i32,
    pub height: i32,
    pub sea_level: Option<i32>,
    pub bits: Vec<u32>,
    /// Aquifer substance per value when the probe wrote one:
    /// 0 default block, 1 air, 2 water, 3 lava, 4 other.
    pub substance: Vec<u8>,
    /// `aq grid` line: min grid x/y/z, grid size x/z, skipSamplingAboveY.
    pub aq_grid: Option<([i32; 3], i32, i32, i32)>,
    /// `aq cell` lines: index, location, status (level, type) if computed.
    pub aq_cells: Vec<crate::aquifer26::DebugCell>,
    /// `aq surface` lines: quantized x, z, level.
    pub aq_surfaces: Vec<(i32, i32, i32)>,
    /// `aq probe` lines: cell index, then float bits of floodedness,
    /// exclusion and spread, each as (cached context, uncached).
    pub aq_probes: Vec<(usize, [(u32, u32); 3])>,
}

impl ChunkOracle {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut lines = text.lines();
        let head = lines.next().ok_or("empty chunk oracle file")?;
        let f: Vec<&str> = head.split(' ').collect();
        if f.len() < 10 || f[0] != "#" || f[1] != "seed" || f[3] != "chunk" || f[6] != "min_y" || f[8] != "height" {
            return Err(format!("bad header: {head}"));
        }
        let num = |s: &str| s.parse::<i32>().map_err(|e| e.to_string());
        let sea_level = f.iter().position(|w| *w == "sea_level").map(|i| num(f[i + 1])).transpose()?;
        let mut bits = Vec::new();
        let mut substance = Vec::new();
        let mut aq_grid = None;
        let mut aq_cells = Vec::new();
        let mut aq_surfaces = Vec::new();
        let mut aq_probes = Vec::new();
        for (n, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("aq ") {
                let f: Vec<&str> = rest.split(' ').collect();
                let err = |e: String| format!("line {}: {e}", n + 2);
                match f[0] {
                    "grid" if f.len() >= 8 => {
                        aq_grid = Some(([num(f[1])?, num(f[2])?, num(f[3])?], num(f[4])?, num(f[5])?, num(f[7])?));
                    }
                    "cell" if f.len() >= 6 => {
                        let index = f[1].parse::<usize>().map_err(|e| err(e.to_string()))?;
                        let loc = (num(f[2])?, num(f[3])?, num(f[4])?);
                        let status = if f[5] == "none" { None } else { Some((num(f[5])?, f[6].parse::<u8>().map_err(|e| err(e.to_string()))?)) };
                        aq_cells.push((index, loc, status));
                    }
                    "surface" if f.len() >= 4 => aq_surfaces.push((num(f[1])?, num(f[2])?, num(f[3])?)),
                    "probe" if f.len() >= 8 => {
                        let hex = |s: &str| u32::from_str_radix(s, 16).map_err(|e| err(e.to_string()));
                        aq_probes.push((
                            f[1].parse::<usize>().map_err(|e| err(e.to_string()))?,
                            [(hex(f[2])?, hex(f[3])?), (hex(f[4])?, hex(f[5])?), (hex(f[6])?, hex(f[7])?)],
                        ));
                    }
                    _ => return Err(err(format!("bad aq line: {line}"))),
                }
                continue;
            }
            let mut parts = line.split(' ');
            let hex = parts.next().unwrap_or("");
            bits.push(u32::from_str_radix(hex, 16).map_err(|e| format!("line {}: {e}", n + 2))?);
            if let Some(sub) = parts.next() {
                substance.push(sub.parse::<u8>().map_err(|e| format!("line {}: {e}", n + 2))?);
            }
        }
        if !substance.is_empty() && substance.len() != bits.len() {
            return Err("substance column present on some lines only".to_string());
        }
        Ok(Self {
            seed: f[2].parse::<i64>().map_err(|e| e.to_string())?,
            chunk_x: num(f[4])?,
            chunk_z: num(f[5])?,
            min_y: num(f[7])?,
            height: num(f[9])?,
            sea_level,
            bits,
            substance,
            aq_grid,
            aq_cells,
            aq_surfaces,
            aq_probes,
        })
    }
}

/// The file `/painite density surface <x> <z>` writes: one chunk between
/// buildSurface and the carvers.
pub struct SurfaceOracle {
    pub seed: i64,
    pub chunk_x: i32,
    pub chunk_z: i32,
    /// Whether the chunk had no beardifier and no blender (None for
    /// dumps from before the flag existed).
    pub eligible: Option<bool>,
    pub min_y: i32,
    pub height: i32,
    /// Canonical block state JSON per palette id.
    pub palette: Vec<String>,
    pub biomes: Vec<String>,
    pub quart_min_y: i32,
    pub quart_count: usize,
    /// Biome index per quart, `y + (x + z * 6) * quart_count`.
    pub quarts: Vec<u16>,
    /// `WORLD_SURFACE_WG` and `OCEAN_FLOOR_WG` first-available heights, `x + z * 16`.
    pub surface_height: Vec<i32>,
    pub floor_height: Vec<i32>,
    /// Palette id per block in `Volume::chunk` order, bit 0x8000 = post-processing.
    pub blocks: Vec<u16>,
    /// Structure pieces in the `beard26` flat layout, when the chunk had any.
    pub beard: Option<Vec<i32>>,
}

impl SurfaceOracle {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let header = lines.next().ok_or("empty oracle")?;
        let f: Vec<&str> = header.split_whitespace().collect();
        let mut kv = std::collections::HashMap::new();
        let mut i = 1;
        while i + 1 < f.len() {
            match f[i] {
                "chunk" => {
                    kv.insert("chunk_x", f[i + 1]);
                    kv.insert("chunk_z", *f.get(i + 2).ok_or("header: chunk z missing")?);
                    i += 3;
                }
                key => {
                    kv.insert(key, f[i + 1]);
                    i += 2;
                }
            }
        }
        let int = |k: &str| -> Result<i64, String> { kv.get(k).ok_or_else(|| format!("header: {k} missing"))?.parse::<i64>().map_err(|e| format!("header {k}: {e}")) };
        let seed = int("seed").unwrap_or(0);
        let chunk_x = int("chunk_x")? as i32;
        let chunk_z = int("chunk_z")? as i32;
        let eligible = int("eligible").ok().map(|v| v != 0);
        let min_y = int("min_y")? as i32;
        let height = int("height")? as i32;
        let palette_len = int("palette")? as usize;
        let biome_len = int("biomes")? as usize;
        let quart_min_y = int("quart_min_y")? as i32;
        let quart_count = int("quart_count")? as usize;
        let mut palette = Vec::with_capacity(palette_len);
        let mut biomes = Vec::with_capacity(biome_len);
        let mut quarts = Vec::new();
        let mut surface_height = vec![0; 256];
        let mut floor_height = vec![0; 256];
        let mut blocks = Vec::with_capacity(16 * 16 * height as usize);
        let mut beard = None;
        for line in lines {
            if let Some(rest) = line.strip_prefix("palette ") {
                palette.push(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("beard ") {
                beard = Some(rest.split_whitespace().map(|v| v.parse::<i32>().map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?);
            } else if let Some(rest) = line.strip_prefix("biome ") {
                biomes.push(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("quarts ") {
                quarts = rest.split_whitespace().map(|v| v.parse::<u16>().map_err(|e| e.to_string())).collect::<Result<_, _>>()?;
            } else if let Some(rest) = line.strip_prefix("heights ") {
                let v: Vec<i32> = rest.split_whitespace().map(|v| v.parse::<i32>().map_err(|e| e.to_string())).collect::<Result<_, _>>()?;
                if v.len() != 4 {
                    return Err(format!("heights line: {line}"));
                }
                let col = v[0] as usize + v[1] as usize * 16;
                surface_height[col] = v[2];
                floor_height[col] = v[3];
            } else if !line.is_empty() {
                blocks.push(line.parse::<u16>().map_err(|e| format!("block line {line}: {e}"))?);
            }
        }
        if palette.len() != palette_len || biomes.len() != biome_len {
            return Err("palette or biome count mismatch".into());
        }
        if quarts.len() != 6 * 6 * quart_count {
            return Err(format!("quarts: {} values, expected {}", quarts.len(), 6 * 6 * quart_count));
        }
        if blocks.len() != 16 * 16 * height as usize {
            return Err(format!("blocks: {} values, expected {}", blocks.len(), 16 * 16 * height));
        }
        Ok(Self { seed, chunk_x, chunk_z, eligible, min_y, height, palette, biomes, quart_min_y, quart_count, quarts, surface_height, floor_height, blocks, beard })
    }
}
