//! Bit-exact comparison of the Rust density tree against vanilla.
//!
//! Needs two environment variables, otherwise the test is skipped:
//! `PAINITE_WG_ROOT`: directory holding `data/minecraft/worldgen/`
//! (extract it from the loom `minecraft-merged.jar`);
//! `PAINITE_ORACLE`: the file written by `/painite density probe <n>`.

use std::collections::BTreeMap;
use std::path::Path;

use painite_terrain::df26::{Loader, Mode};
use painite_terrain::oracle::Oracle;

struct KeyStats {
    total: usize,
    mismatches: usize,
    worst_ulps: u32,
    examples: Vec<String>,
}

fn ulps(a: f32, b: f32) -> u32 {
    if a.to_bits() == b.to_bits() {
        return 0;
    }
    if a.is_nan() || b.is_nan() {
        return u32::MAX;
    }
    let ia = a.to_bits() as i64;
    let ib = b.to_bits() as i64;
    let sa = if a.is_sign_negative() { i32::MIN as i64 - ia } else { ia };
    let sb = if b.is_sign_negative() { i32::MIN as i64 - ib } else { ib };
    (sa - sb).unsigned_abs().min(u32::MAX as u64) as u32
}

#[test]
fn matches_vanilla_oracle() {
    let (Ok(root), Ok(oracle_path)) = (std::env::var("PAINITE_WG_ROOT"), std::env::var("PAINITE_ORACLE")) else {
        eprintln!("PAINITE_WG_ROOT / PAINITE_ORACLE not set, skipping");
        return;
    };
    let oracle = Oracle::load(Path::new(&oracle_path)).expect("oracle file");
    let mut loader = Loader::new(Path::new(&root), oracle.header.seed);
    let router = loader.router(&oracle.header.settings).expect("router");

    let mut stats: BTreeMap<String, KeyStats> = BTreeMap::new();
    for s in &oracle.samples {
        let ours = if let Some(name) = s.key.strip_prefix("noise:") {
            loader.noise(name).expect("noise").get(s.x as f64, s.y as f64, s.z as f64)
        } else {
            router.get(&s.key).unwrap_or_else(|| panic!("router key {}", s.key)).eval(s.x, s.y, s.z, Mode::Point)
        };
        let theirs = s.value();
        let entry = stats.entry(s.key.clone()).or_insert(KeyStats { total: 0, mismatches: 0, worst_ulps: 0, examples: Vec::new() });
        entry.total += 1;
        let d = ulps(ours, theirs);
        if d != 0 {
            entry.mismatches += 1;
            entry.worst_ulps = entry.worst_ulps.max(d);
            if entry.examples.len() < 3 {
                entry.examples.push(format!(
                    "({}, {}, {}) vanilla {} ({:08x}) rust {} ({:08x}) {} ulps",
                    s.x, s.y, s.z, theirs, theirs.to_bits(), ours, ours.to_bits(), d
                ));
            }
        }
    }

    let mut failed = false;
    for (key, st) in &stats {
        if st.mismatches == 0 {
            println!("{key}: {}/{} exact", st.total, st.total);
        } else {
            failed = true;
            println!("{key}: {} of {} mismatch, worst {} ulps", st.mismatches, st.total, st.worst_ulps);
            for e in &st.examples {
                println!("    {e}");
            }
        }
    }
    assert!(!failed, "density oracle mismatch, see output");
}
