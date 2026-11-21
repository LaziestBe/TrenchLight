//! Loading everything a frame needs: a square of chunks, their textures, one
//! merged mesh.
//!
//! Kept out of `main.rs` so the same load can be driven headlessly (a future
//! `--dump-mesh`) and so a failure names the step it happened in rather than
//! surfacing as an empty window.
//!
//! The merge is the batching: every chunk's vertices and indices are appended to
//! one pair of buffers with the indices rebased, so a 3×3 area is **one** draw
//! call. Drawing nine chunks separately on this GPU would cost nine state changes
//! per frame for no benefit.

use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use crate::mesh::{self, Vertex};
use crate::region::{self, Region};
use crate::selftest::save_info;
use crate::textures::{self, Atlas, Jar};
use crate::world::{self, Blocks, HEIGHT, MIN_Y};

pub struct Scene {
    pub centre: (i32, i32),
    pub radius: i32,
    /// Chunks meshed, which is not always `(2r+1)²`: a region file can be short.
    pub chunks: usize,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub faces: usize,
    pub skipped: usize,
    pub missing_textures: Vec<String>,
    pub atlas: Atlas,
    /// Lowest and highest solid block Y across the area, for framing the camera.
    pub surface: (i32, i32),
}

impl Scene {
    /// One line for the exit summary. Reads every field back so nothing in the
    /// load report is write-only.
    pub fn summary(&self) -> String {
        let side = 2 * self.radius + 1;
        format!(
            "{} of {}x{} chunks, {} faces, {} triangles, {} skipped blocks, {} missing textures",
            self.chunks,
            side,
            side,
            self.faces,
            self.indices.len() / 3,
            self.skipped,
            self.missing_textures.len()
        )
    }
}

/// Load a `(2 * radius + 1)²` square of chunks around `centre`.
pub fn load(
    save: &Path,
    jar: &Path,
    centre: Option<(i32, i32)>,
    radius: i32,
    outside_unloaded_solid: bool,
) -> Result<Scene> {
    let info = save_info(save)?;
    if info.regions.is_empty() {
        bail!("no region folder under {} — is that a world?", save.display());
    }
    let (cx, cz) = centre
        .or(info.spawn_chunk)
        .context("no --chunk given and level.dat has no usable spawn")?;
    let radius = radius.max(0);

    // Which chunks exist in the area. Only this decides which sides are walls;
    // a chunk that is missing from the save is not "solid neighbour".
    let mut wanted: Vec<(i32, i32)> = Vec::new();
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            wanted.push((cx + dx, cz + dz));
        }
    }
    let wanted_set: HashSet<(i32, i32)> = wanted.iter().copied().collect();

    let started = Instant::now();
    // One open file per region: a 32×32 block area usually lives in one, but a
    // larger radius spans several and reopening the same .mca per chunk would
    // re-read its 4 KiB header every time.
    let mut regions: HashMap<(i32, i32), Region> = HashMap::new();
    let mut decoded: Vec<((i32, i32), world::Chunk)> = Vec::new();
    let mut blocks = Blocks::new();
    let mut missing: Vec<(i32, i32)> = Vec::new();
    for &(x, z) in &wanted {
        let (rx, rz) = region::region_of(x, z);
        if !regions.contains_key(&(rx, rz)) {
            let path = region::region_file(&info.regions, rx, rz);
            if !path.exists() {
                continue;
            }
            regions.insert((rx, rz), Region::open(&path)?);
        }
        let region = &regions[&(rx, rz)];
        let raw = region.chunk(x, z)?;
        let Some(raw) = raw else {
            missing.push((x, z));
            continue;
        };
        decoded.push(((x, z), world::decode_chunk(&raw, &mut blocks)?));
    }
    if decoded.is_empty() {
        bail!("no chunks around ({cx},{cz}) could be read — wrong --chunk or wrong save");
    }
    let decode_ms = started.elapsed().as_millis();

    let started = Instant::now();
    let mut jar = Jar::open(jar)?;
    let atlas = textures::build_atlas(&mut jar)?;
    let atlas_ms = started.elapsed().as_millis();

    let started = Instant::now();
    let air = blocks.air_like();
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut faces = 0usize;
    let mut skipped = 0usize;
    let mut missing_textures: Vec<String> = Vec::new();
    let mut surface = (i32::MAX, i32::MIN);

    for ((x, z), chunk) in &decoded {
        // A side is a wall only when the neighbour is not one of the chunks we
        // just loaded. Order: +x, −x, +z, −z.
        let outside = [
            !wanted_set.contains(&(x + 1, *z)) && outside_unloaded_solid,
            !wanted_set.contains(&(x - 1, *z)) && outside_unloaded_solid,
            !wanted_set.contains(&(*x, z + 1)) && outside_unloaded_solid,
            !wanted_set.contains(&(*x, z - 1)) && outside_unloaded_solid,
        ];
        let m = mesh::build(chunk, &blocks, &atlas, [*x, *z], outside);
        let base = vertices.len() as u32;
        vertices.extend_from_slice(&m.vertices);
        indices.extend(m.indices.iter().map(|i| i + base));
        faces += m.faces;
        skipped += m.skipped;
        for name in m.missing_textures {
            if !missing_textures.contains(&name) {
                missing_textures.push(name);
            }
        }
        for zz in 0..16usize {
            for xx in 0..16usize {
                if let Some(y) = world::top_solid(chunk, xx, zz, &air) {
                    surface.0 = surface.0.min(y);
                    surface.1 = surface.1.max(y);
                }
            }
        }
    }
    let mesh_ms = started.elapsed().as_millis();
    if surface.0 == i32::MAX {
        surface = (MIN_Y, MIN_Y + HEIGHT as i32 - 1);
    }

    let mut region_names: Vec<String> = regions
        .keys()
        .map(|(rx, rz)| format!("r.{rx}.{rz}.mca"))
        .collect();
    region_names.sort();

    println!(
        "world:       {} ({} / DataVersion {})",
        save.display(),
        info.version_name,
        info.data_version
    );
    println!(
        "area:        {}x{} chunks around ({cx},{cz}) from {} — surface Y {}..{}  (decode {decode_ms} ms)",
        (2 * radius + 1) as usize,
        (2 * radius + 1) as usize,
        region_names.join(", "),
        surface.0,
        surface.1
    );
    println!(
        "chunks:      {} meshed{}  ({} blocks skipped as non-cubes)",
        decoded.len(),
        if missing.is_empty() {
            String::new()
        } else {
            format!(", {} absent from the save", missing.len())
        },
        skipped
    );
    println!(
        "atlas:       {} tiles, {}x{} px  (build {atlas_ms} ms)",
        atlas.loaded, atlas.image.width, atlas.image.height
    );
    println!(
        "mesh:        {faces} faces, {} vertices, {} triangles, 1 draw call  ({mesh_ms} ms)",
        vertices.len(),
        indices.len() / 3
    );
    if !missing_textures.is_empty() {
        println!(
            "mesh:        WARNING {} textures missing from the atlas: {}",
            missing_textures.len(),
            missing_textures.join(", ")
        );
    }

    Ok(Scene {
        centre: (cx, cz),
        radius,
        chunks: decoded.len(),
        vertices,
        indices,
        faces,
        skipped,
        missing_textures,
        atlas,
        surface,
    })
}
