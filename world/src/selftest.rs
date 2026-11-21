//! `--selftest`: decode a real save and print what the decoder found.
//!
//! This exists because "it compiled" and "it drew something" are both compatible
//! with a decoder that is subtly wrong — wrong section offset, wrong palette
//! handling, wrong bit packing. The numbers printed here were first produced by
//! an independent JS decoder, then confirmed against Mojang's own
//! `Heightmaps`, so a Rust decoder that reproduces them is right for the same
//! reasons.
//!
//! No window, no GPU: this path is safe to run over a remote session.

use crate::nbt;
use crate::region::{self, Region};
use crate::world::{self, Blocks, HEIGHT, MIN_Y, SECTION_COUNT};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the overworld's region files live.
///
/// Two layouts exist and both are in use in the wild:
/// `<save>\region` (the documentation's layout, and what 1.12-era worlds have)
/// and `<save>\dimensions\minecraft\overworld\region` (what 26.3 writes).
/// Getting this wrong returns an empty world with no error at all, which is why
/// the miss is reported rather than falling back silently.
pub fn region_dir(save: &Path) -> Result<PathBuf> {
    let flat = save.join("region");
    if flat.is_dir() {
        return Ok(flat);
    }
    let modern = save
        .join("dimensions")
        .join("minecraft")
        .join("overworld")
        .join("region");
    if modern.is_dir() {
        return Ok(modern);
    }
    bail!(
        "no region folder in {} — looked for `region\\` and `dimensions\\minecraft\\overworld\\region\\`",
        save.display()
    )
}

fn level_dat(save: &Path) -> Result<nbt::Nbt> {
    let path = save.join("level.dat");
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    nbt::parse_gzip(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn dimension_name(save: &Path) -> &'static str {
    // Every world we have seen is a normal overworld save. A dimension split
    // (`<save>\DIM-1`) is a stage-1 non-goal; if one shows up, the region folder
    // probe simply fails with a clear message.
    let _ = save;
    "overworld"
}

pub struct Report {
    pub chunk: (i32, i32),
    pub region_path: PathBuf,
    pub solid: usize,
    pub distinct: usize,
    /// Faces when every non-air block counts as a cube — the number the
    /// independent JS decoder produced, so it can be compared across languages.
    pub faces: usize,
    pub heightmap_agreement: (usize, usize),
    /// Mesher output when a jar was supplied: `(culled, unculled)` faces.
    pub mesh_faces: Option<(usize, usize)>,
}

/// What `level.dat` says about a save, plus where its regions live.
pub struct SaveInfo {
    pub data_version: i32,
    pub version_name: String,
    pub spawn: Option<[f64; 3]>,
    pub spawn_chunk: Option<(i32, i32)>,
    pub regions: PathBuf,
}

pub fn save_info(save: &Path) -> Result<SaveInfo> {
    let level = level_dat(save)?;
    // `level.dat` is `{ Data: { ... } }` in every version that matters, but a
    // tool that rewrote the file might flatten it, so only unwrap when the key
    // is actually a compound.
    let root = level
        .as_compound()
        .context("level.dat root is not a compound")?;
    let data = match root.get("Data") {
        Some(inner @ nbt::Nbt::Compound(_)) => inner.as_compound()?,
        _ => root,
    };
    let data_version = data
        .get("DataVersion")
        .and_then(|v| v.as_i32().ok())
        .unwrap_or(0);
    let version_name = data
        .get("Version")
        .and_then(|v| v.as_compound().ok())
        .and_then(|v| v.get("Name"))
        .and_then(|v| v.as_str().ok())
        .unwrap_or("(absent)")
        .to_string();
    let spawn = data
        .get("spawn")
        .and_then(|v| v.as_compound().ok())
        .and_then(|v| {
            // 26.3 writes `spawn.pos` as an `int[3]`; older saves used a list of
            // doubles. Both are accepted, because only the chunk it lands in
            // matters.
            match v.get("pos") {
                Some(nbt::Nbt::IntArray(v)) => {
                    Some(v.iter().map(|&i| i as f64).collect::<Vec<f64>>())
                }
                Some(other) => other.as_list().ok().map(|pos| {
                    pos.iter()
                        .filter_map(|v| match v {
                            nbt::Nbt::Double(d) => Some(*d),
                            nbt::Nbt::Float(f) => Some(*f as f64),
                            nbt::Nbt::Int(i) => Some(*i as f64),
                            _ => None,
                        })
                        .collect::<Vec<f64>>()
                }),
                None => None,
            }
            .filter(|v| v.len() == 3)
        })
        .map(|v| [v[0], v[1], v[2]]);
    let spawn_chunk = spawn.map(|p| {
        (
            (p[0].floor() as i32).div_euclid(16),
            (p[2].floor() as i32).div_euclid(16),
        )
    });
    Ok(SaveInfo {
        data_version,
        version_name,
        spawn,
        spawn_chunk,
        regions: region_dir(save)?,
    })
}

/// Decode one chunk and print a report. Returns the numbers so they can be
/// asserted in tests without scraping stdout.
pub fn run(
    save: &Path,
    chunk: Option<(i32, i32)>,
    probes: &[(i32, i32, i32)],
    jar: Option<&Path>,
) -> Result<Report> {
    let info = save_info(save)?;
    let data_version = info.data_version;
    let version_name = info.version_name.clone();
    let spawn = info.spawn;
    let spawn_chunk = info.spawn_chunk;

    let dir = info.regions;
    let (cx, cz) = chunk
        .or(spawn_chunk)
        .context("no --chunk given and level.dat has no usable spawn")?;

    println!("save:        {}", save.display());
    println!("regions:     {}", dir.display());
    println!(
        "level.dat:   DataVersion {data_version}  version {version_name}  spawn {}",
        spawn
            .as_ref()
            .map(|p| format!("[{}, {}, {}]", p[0], p[1], p[2]))
            .unwrap_or_else(|| "(absent)".to_string())
    );
    println!("dimension:   {}", dimension_name(save));

    let (rx, rz) = region::region_of(cx, cz);
    let path = region::region_file(&dir, rx, rz);
    if !path.exists() {
        bail!(
            "chunk ({cx},{cz}) needs {} but it does not exist",
            path.display()
        );
    }
    let region = Region::open(&path)?;
    let raw = region
        .chunk(cx, cz)?
        .with_context(|| format!("chunk ({cx},{cz}) is not present in {}", path.display()))?;

    let mut blocks = Blocks::new();
    let chunk = world::decode_chunk(&raw, &mut blocks)?;
    let air = blocks.air_like();

    let mut kinds: Vec<(&&str, &usize)> = chunk.palette_kinds.iter().collect();
    kinds.sort();
    let kinds = kinds
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");

    // Heightmap cross-check: the assertion that catches a wrong decoder.
    let mut agreement = (0usize, 0usize);
    if let Some(hm) = chunk.heightmaps.get("WORLD_SURFACE") {
        let mut agree = 0usize;
        for z in 0..16usize {
            for x in 0..16usize {
                let expected = hm.get(z * 16 + x).copied().unwrap_or(i32::MIN);
                let actual = world::top_solid(&chunk, x, z, &air).unwrap_or(MIN_Y - 1);
                if expected == actual {
                    agree += 1;
                }
            }
        }
        agreement = (agree, 256);
    }

    let faces = world::exposed_faces(&chunk, &blocks, &|_| true);
    let mut surface = (i32::MAX, i32::MIN);
    for z in 0..16usize {
        for x in 0..16usize {
            if let Some(y) = world::top_solid(&chunk, x, z, &air) {
                surface.0 = surface.0.min(y);
                surface.1 = surface.1.max(y);
            }
        }
    }

    println!(
        "chunk:       ({cx},{cz}) from {} — Status={}  DataVersion={}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        chunk.status,
        chunk.data_version
    );
    println!(
        "sections:    {} of {SECTION_COUNT} present, {} with blocks   palette: {kinds}",
        chunk.sections_present, chunk.non_air_sections
    );

    // The block histogram: which names actually have to be textured, and how
    // many of them are not full cubes. Printed because "the decoder works" and
    // "the renderer will work" are different claims.
    let air = blocks.air_like();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for &id in &chunk.blocks {
        if air.contains(&id) {
            continue;
        }
        let name = blocks.name(id);
        match counts.iter_mut().find(|(n, _)| n == name) {
            Some((_, c)) => *c += 1,
            None => counts.push((name.to_string(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let shaps = counts
        .iter()
        .map(|(name, count)| format!("{}={count}", name.strip_prefix("minecraft:").unwrap_or(name)))
        .collect::<Vec<_>>()
        .join(" ");
    println!("block ids:   {shaps}");
    println!(
        "blocks:      {} solid of {} ({:.1}%), {} distinct ids",
        chunk.solid,
        world::BLOCKS_PER_CHUNK,
        100.0 * chunk.solid as f64 / world::BLOCKS_PER_CHUNK as f64,
        blocks.len()
    );
    println!(
        "surface:     Y {}..{}",
        if surface.0 == i32::MAX { MIN_Y - 1 } else { surface.0 },
        if surface.1 == i32::MIN { MIN_Y - 1 } else { surface.1 }
    );
    println!(
        "exposed:     {faces} faces ({} triangles)",
        faces * 2
    );
    println!(
        "heightmap:   {}/{} WORLD_SURFACE columns agree with the decoded blocks",
        agreement.0, agreement.1
    );

    // The mesher gate. `exposed_faces` and `mesh::build` are separate
    // implementations of the same culling rule; when they disagree, one of them
    // lost or duplicated a face, and that is much cheaper to find here than in a
    // screenshot.
    let mut mesh_faces = None;
    if let Some(jar_path) = jar {
        let mut jar = crate::textures::Jar::open(jar_path)?;
        let atlas = crate::textures::build_atlas(&mut jar)?;
        let culled = crate::mesh::build(&chunk, &blocks, &atlas, [cx, cz], [true; 4]);
        let unculled = crate::mesh::build(&chunk, &blocks, &atlas, [cx, cz], [false; 4]);
        let reference = world::exposed_faces(&chunk, &blocks, &|name| {
            crate::mesh::is_drawable(name, &atlas)
        });
        println!(
            "atlas:       {} tiles in {}x{} px",
            atlas.loaded, atlas.image.width, atlas.image.height
        );
        println!(
            "mesh:        {} faces culled ({} tris), {} with the column's walls ({} tris), \
             {} blocks skipped as non-cubes, {} as transparent sprites",
            culled.faces,
            culled.indices.len() / 3,
            unculled.faces,
            unculled.indices.len() / 3,
            culled.skipped,
            culled.skipped_transparent
        );
        if !culled.missing_textures.is_empty() {
            println!(
                "mesh:        WARNING {} textures missing: {}",
                culled.missing_textures.len(),
                culled.missing_textures.join(", ")
            );
        }
        println!(
            "mesh check:  independent counter says {reference} culled faces — {}",
            if reference == culled.faces { "agree" } else { "DISAGREE" }
        );
        if reference != culled.faces {
            bail!(
                "mesher and face counter disagree: {} vs {reference} faces",
                culled.faces
            );
        }
        mesh_faces = Some((culled.faces, unculled.faces));
    }

    for &(px, py, pz) in probes {
        let bx = px.rem_euclid(16) as usize;
        let bz = pz.rem_euclid(16) as usize;
        let block = chunk
            .at_world_y(bx, py, bz)
            .map(|id| blocks.name(id).to_string())
            .unwrap_or_else(|| format!("(outside Y {}..{})", MIN_Y, MIN_Y + HEIGHT as i32 - 1));
        println!(
            "probe:       world ({px},{py},{pz}) -> chunk ({},{}) local ({bx},{bz}) = {block}",
            px.div_euclid(16),
            pz.div_euclid(16)
        );
    }

    Ok(Report {
        chunk: (cx, cz),
        region_path: path,
        solid: chunk.solid,
        distinct: blocks.len(),
        faces,
        heightmap_agreement: agreement,
        mesh_faces,
    })
}

/// `<x>,<y>,<z>` — the form `--probe` accepts.
pub fn parse_probe(text: &str) -> Result<(i32, i32, i32)> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        bail!("--probe wants x,y,z, got '{text}'");
    }
    let parsed: Vec<i32> = parts
        .iter()
        .map(|p| p.parse::<i32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("--probe '{text}' has a non-integer component"))?;
    Ok((parsed[0], parsed[1], parsed[2]))
}

/// `<x>,<z>` for `--chunk`.
pub fn parse_chunk(text: &str) -> Result<(i32, i32)> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    if parts.len() != 2 {
        bail!("--chunk wants x,z, got '{text}'");
    }
    Ok((
        parts[0].parse().with_context(|| format!("--chunk '{text}'"))?,
        parts[1].parse().with_context(|| format!("--chunk '{text}'"))?,
    ))
}
