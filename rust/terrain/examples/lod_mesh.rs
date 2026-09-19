//! Mesh cost from real far-view records: `cargo run --release --example lod_mesh -- <world>/painite/lod`.
//! The real region mesher over every region found, at scale 1, 2 and 4; prints us and vertices per chunk.
use painite_terrain::lod26::{ClientStore, STAGE_FINAL, region_records};
use painite_terrain::lodmesh26::{VERTEX_BYTES, mesh_region};
use std::time::Instant;

fn main() {
    let dir = std::env::args().nth(1).expect("lod dir");
    let mut store = ClientStore::new(1 << 20);
    let mut regions = Vec::new();
    let mut count = 0usize;
    for entry in std::fs::read_dir(&dir).expect("read dir") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "plod") {
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let mut parts = name.split('.').skip(1);
            let (rx, rz): (i32, i32) = (parts.next().unwrap().parse().unwrap(), parts.next().unwrap().parse().unwrap());
            let bytes = std::fs::read(&path).unwrap();
            let Some(recs) = region_records(&bytes) else { continue };
            let mut batch = Vec::new();
            for (slot, rec) in recs {
                batch.extend_from_slice(&(rx * 32 + (slot % 32) as i32).to_le_bytes());
                batch.extend_from_slice(&(rz * 32 + (slot / 32) as i32).to_le_bytes());
                batch.extend_from_slice(&(STAGE_FINAL as u32).to_le_bytes());
                rec.write_to_public(&mut batch);
                count += 1;
            }
            store.put_batch(&batch, 0, 0).expect("batch");
            regions.push((rx, rz));
        }
    }
    println!("{} records in {} regions from {}", count, regions.len(), dir);
    let colours = vec![[128u8, 128, 128, 255]; 8192];
    for scale in [1usize, 2, 4, 8, 16] {
        let mut verts = 0usize;
        let t = Instant::now();
        for _ in 0..3 {
            verts = 0;
            for &(rx, rz) in &regions {
                let mut out = Vec::new();
                let mut counts = Vec::new();
                mesh_region(&store, rx, rz, |_, _| scale, (0, 0, -1), &painite_terrain::lodmesh26::Palette::plain(&colours), &mut out, &mut counts);
                verts += out.len() / VERTEX_BYTES;
            }
        }
        let per_chunk_us = t.elapsed().as_secs_f64() * 1e6 / (3.0 * count as f64);
        println!("scale {}: {:.1} us/chunk build, {:.0} verts/chunk, {:.1} MB for all at 16 B/vert", scale, per_chunk_us, verts as f64 / count as f64, verts as f64 * 16.0 / 1e6);
    }
}
