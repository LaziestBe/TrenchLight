//! Turning chunk NBT into blocks.
//!
//! Everything here was established by decoding a real world and
//! then cross-checking the result against Mojang's own `Heightmaps`: 1,792 of
//! 1,792 columns across 7 chunks agree. That cross-check is worth keeping in
//! mind, because the two things that would silently break a decoder — section Y
//! mapping and palette resolution — both show up as heightmap disagreement.
//!
//! ## What the data actually looks like (26.3, DataVersion 5023)
//!
//! * A chunk's root compound has **no `Level` wrapper**: `sections`, `Status`,
//!   `Heightmaps` and the rest sit at the top.
//! * 24 sections per chunk, `Y` from -4 to 19, as a `TAG_Byte`. Section `Y`
//!   covers world `Y = Y*16 .. Y*16+15`, so the world spans -64..319.
//! * `block_states.palette` is a `TAG_List` of **either strings or compounds**,
//!   both present in the same world:
//!     - strings: `["minecraft:bedrock", "minecraft:deepslate", ...]`
//!     - compounds: `{"": "minecraft:deepslate"}` for entries without
//!       properties, or `{"id": "...", "properties": {..}}` for those with
//!   There is no `Name`/`Properties` spelling anywhere. Reading the palette by
//!   shape rather than by name is what makes both work.
//! * `block_states.data` is a `TAG_Long_Array`, **padded** packing (an entry
//!   never straddles two longs), minimum 4 bits: 256 longs at 4 bits, 342 at 5,
//!   410 at 6 — the padded lengths, not the compact ones.
//! * Inside a section the order is `index = y*256 + z*16 + x`.
//! * `Heightmaps.WORLD_SURFACE` holds 9-bit values, 7 per long, where
//!   `value - 65` is the Y of the highest non-air block.

use crate::nbt::{Compound, Nbt};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;

pub const SECTION_HEIGHT: i32 = 16;
pub const MIN_Y: i32 = -64;
pub const SECTION_COUNT: usize = 24;
pub const HEIGHT: usize = SECTION_HEIGHT as usize * SECTION_COUNT;
pub const BLOCKS_PER_CHUNK: usize = 16 * HEIGHT * 16;
pub const AIR: &str = "minecraft:air";
pub const CAVE_AIR: &str = "minecraft:cave_air";
pub const VOID_AIR: &str = "minecraft:void_air";

/// Names to small integers, so a chunk is `Vec<u16>` rather than a pile of
/// `String`s. Air is always id 0 so "is this air" is a comparison against a
/// known constant.
#[derive(Debug, Default)]
pub struct Blocks {
    names: Vec<String>,
    ids: HashMap<String, u16>,
}

impl Blocks {
    pub fn new() -> Self {
        let mut blocks = Blocks::default();
        blocks.intern(AIR);
        blocks
    }

    pub fn intern(&mut self, name: &str) -> u16 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len() as u16;
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    pub fn lookup(&self, name: &str) -> Option<u16> {
        self.ids.get(name).copied()
    }

    pub fn name(&self, id: u16) -> &str {
        self.names
            .get(id as usize)
            .map(|s| s.as_str())
            .unwrap_or("<unknown>")
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Ids whose name is one of the three airs. Air is id 0 by construction, but
    /// a world can also palette `cave_air` and `void_air`, which look identical
    /// in a mesh and must be culled like air.
    pub fn air_like(&self) -> Vec<u16> {
        [AIR, CAVE_AIR, VOID_AIR]
            .iter()
            .filter_map(|n| self.lookup(n))
            .collect()
    }
}

/// One decoded chunk: 16 × 384 × 16 block ids, plus what the decoder observed.
#[derive(Debug)]
pub struct Chunk {
    pub blocks: Vec<u16>,
    pub sections_present: usize,
    pub non_air_sections: usize,
    pub solid: usize,
    pub data_version: i32,
    pub status: String,
    pub heightmaps: HashMap<String, Vec<i32>>,
    /// Palette list kinds seen, for the self-test: `("strings", 23)`.
    pub palette_kinds: HashMap<&'static str, usize>,
}

impl Chunk {
    #[inline]
    pub fn at(&self, x: usize, y: usize, z: usize) -> u16 {
        self.blocks[y * 256 + z * 16 + x]
    }

    /// Block id at a world Y, or `None` outside the build range.
    pub fn at_world_y(&self, x: usize, y: i32, z: usize) -> Option<u16> {
        if !(MIN_Y..MIN_Y + HEIGHT as i32).contains(&y) {
            return None;
        }
        Some(self.at(x, (y - MIN_Y) as usize, z))
    }
}

/// Resolve a `block_states.palette` list into block names, accepting both
/// encodings this game writes.
pub fn palette_names(palette: &Nbt) -> Result<Vec<String>> {
    let items = palette
        .as_list()
        .context("block_states.palette is not a list")?;
    let mut names = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match item {
            Nbt::String(s) => names.push(s.clone()),
            Nbt::Compound(c) => names.push(compound_palette_name(c).with_context(|| {
                format!("palette entry {i} is a compound without a usable block id")
            })?),
            other => bail!(
                "palette entry {i} is a {}, neither a string nor a compound",
                other.tag_name()
            ),
        }
    }
    Ok(names)
}

/// A palette compound is either `{"": "minecraft:x"}` or
/// `{"id": "minecraft:x", "properties": {...}}`.
fn compound_palette_name(c: &Compound) -> Result<String> {
    if let Some(Nbt::String(name)) = c.get("") {
        return Ok(name.clone());
    }
    if let Some(Nbt::String(id)) = c.get("id") {
        return Ok(id.clone());
    }
    if let Some(other) = c.get("Name") {
        if let Nbt::String(name) = other {
            return Ok(name.clone());
        }
    }
    Err(anyhow::anyhow!(
        "keys present: [{}]",
        c.keys().collect::<Vec<_>>().join(", ")
    ))
}

/// Unpack one section's packed longs into `dest`, writing 4096 entries starting
/// at the section's Y.
fn unpack_section(
    dest: &mut [u16],
    data: &[i64],
    ids: &[u16],
    section_y: i32,
    bits: u32,
) -> Result<()> {
    let per_long = (64 / bits) as usize;
    let expected = (4096 + per_long - 1) / per_long;
    if data.len() != expected {
        let compact = (4096 * bits as usize + 63) / 64;
        bail!(
            "section Y={section_y}: {} longs at {bits} bits, expected {expected} (padded) \
             or {compact} (compact); this is neither, so the packing is not what we think",
            data.len()
        );
    }
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let base = ((section_y * SECTION_HEIGHT + 64).max(0) as usize) * 256;
    if base + 4096 > dest.len() {
        bail!(
            "section Y={section_y} falls outside the {HEIGHT}-block build range"
        );
    }
    for k in 0..4096usize {
        let long = (data[k / per_long] as u64) >> ((k % per_long) as u32 * bits);
        let index = (long & mask) as usize;
        let id = *ids.get(index).with_context(|| {
            format!("section Y={section_y}: palette index {index} of {} entries", ids.len())
        })?;
        dest[base + k] = id;
    }
    Ok(())
}

/// Bits per entry for a palette of `len` names, with the format's minimum of 4.
pub fn bits_for(len: usize) -> u32 {
    let raw = if len <= 1 {
        0
    } else {
        (usize::BITS - (len - 1).leading_zeros()) as u32
    };
    raw.max(4)
}

/// Decode a chunk compound into blocks.
pub fn decode_chunk(root: &Nbt, blocks: &mut Blocks) -> Result<Chunk> {
    let root = root.as_compound().context("chunk root is not a compound")?;

    // Reported, not required: a chunk written by an unfinished generation pass
    // is legal and simply has fewer blocks.
    let status = root
        .get("Status")
        .and_then(|s| s.as_str().ok())
        .unwrap_or("(absent)")
        .to_string();
    let data_version = root
        .get("DataVersion")
        .and_then(|v| v.as_i32().ok())
        .unwrap_or(0);

    let sections = root
        .get("sections")
        .context("chunk has no `sections` (an old-format chunk with a `Level` wrapper?)")?
        .as_list()
        .context("chunk `sections` is not a list")?;

    let air_ids = blocks.air_like();
    let mut out = vec![0u16; BLOCKS_PER_CHUNK];
    let mut sections_present = 0usize;
    let mut palette_kinds: HashMap<&'static str, usize> = HashMap::new();

    for section in sections {
        let section = section.as_compound().context("section is not a compound")?;
        let y = section
            .get("Y")
            .context("section has no `Y`")?
            .as_i32()
            .context("section `Y` is not an integer")?;
        sections_present += 1;

        let Some(states) = section.get("block_states") else {
            continue; // a light-only section
        };
        let states = states.as_compound().context("block_states is not a compound")?;
        let palette = states
            .get("palette")
            .context("block_states has no `palette`")?;
        let kind = match palette.as_list()?.first() {
            Some(Nbt::String(_)) => "strings",
            Some(Nbt::Compound(_)) => "compounds",
            _ => "empty",
        };
        *palette_kinds.entry(kind).or_insert(0) += 1;

        let names = palette_names(palette)?;
        if names.is_empty() {
            continue;
        }
        let ids: Vec<u16> = names.iter().map(|n| blocks.intern(n)).collect();

        match states.get("data") {
            Some(data) if names.len() > 1 => {
                let data = data.as_longs().context("block_states.data is not a long[]")?;
                unpack_section(&mut out, data, &ids, y, bits_for(names.len()))?;
            }
            _ => {
                // A single-entry palette (usually all air) carries no data at
                // all, and a solid fill is the whole section.
                let base = ((y * SECTION_HEIGHT + 64).max(0) as usize) * 256;
                if base + 4096 > out.len() {
                    bail!("section Y={y} falls outside the {HEIGHT}-block build range");
                }
                out[base..base + 4096].fill(ids[0]);
            }
        }

    }

    let solid = out.iter().filter(|id| !air_ids.contains(id)).count();
    let non_air_sections = (0..SECTION_COUNT)
        .filter(|s| {
            let base = s * 4096;
            out[base..base + 4096].iter().any(|id| !air_ids.contains(id))
        })
        .count();

    let mut heightmaps = HashMap::new();
    if let Some(Nbt::Compound(hm)) = root.get("Heightmaps") {
        for (name, value) in hm_entries(hm) {
            if let Ok(longs) = value.as_longs() {
                heightmaps.insert(name.to_string(), decode_height(longs, 256));
            }
        }
    }

    Ok(Chunk {
        blocks: out,
        sections_present,
        non_air_sections,
        solid,
        data_version,
        status,
        heightmaps,
        palette_kinds,
    })
}

fn hm_entries(c: &Compound) -> Vec<(&str, &Nbt)> {
    c.keys()
        .filter_map(|k| c.get(k).map(|v| (k, v)))
        .collect()
}

/// Heightmaps store 9 bits per entry, 7 per long, and the value is
/// `topmost_non_air_y + 65` — verified against 1,792 decoded columns.
pub fn decode_height(longs: &[i64], entries: usize) -> Vec<i32> {
    const BITS: u32 = 9;
    const PER_LONG: usize = (64 / BITS as usize) as usize; // 7
    let mask = (1u64 << BITS) - 1;
    let mut out = Vec::with_capacity(entries);
    for k in 0..entries {
        let long = longs.get(k / PER_LONG).copied().unwrap_or(0) as u64;
        let value = (long >> ((k % PER_LONG) as u32 * BITS)) & mask;
        out.push(value as i32 - 65);
    }
    out
}

/// Highest non-air block in a column, or `None` for an empty column.
pub fn top_solid(chunk: &Chunk, x: usize, z: usize, air: &[u16]) -> Option<i32> {
    for y in (0..HEIGHT).rev() {
        if !air.contains(&chunk.at(x, y, z)) {
            return Some(y as i32 + MIN_Y);
        }
    }
    None
}

/// Count block faces that touch a non-solid neighbour.
///
/// `is_solid` decides which blocks count as opaque geometry. Passing `|_| true`
/// reproduces the reference decoder that produced the stage-1 numbers (every
/// non-air block is a cube); passing [`crate::mesh::is_full_cube`] makes the
/// count directly comparable to the mesher, which skips flowers and stairs.
///
/// Outside the chunk horizontally counts as solid, above and below the build
/// range as air — the same convention the mesher is given when it is asked for
/// the culled count. Two independent implementations of the same rule is the
/// point: they must agree exactly, and a disagreement means the mesher lost or
/// duplicated a face.
pub fn exposed_faces(
    chunk: &Chunk,
    blocks: &Blocks,
    is_solid: &dyn Fn(&str) -> bool,
) -> usize {
    let air = blocks.air_like();
    let solid_at = |x: i32, y: i32, z: i32| -> bool {
        if !(0..16).contains(&x) || !(0..16).contains(&z) {
            return true;
        }
        if !(0..HEIGHT as i32).contains(&y) {
            return false;
        }
        let id = chunk.at(x as usize, y as usize, z as usize);
        !air.contains(&id) && is_solid(blocks.name(id))
    };
    let mut faces = 0usize;
    for y in 0..HEIGHT as i32 {
        for z in 0..16i32 {
            for x in 0..16i32 {
                let id = chunk.at(x as usize, y as usize, z as usize);
                if air.contains(&id) || !is_solid(blocks.name(id)) {
                    continue;
                }
                for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1), (0, 1, 0), (0, -1, 0)]
                {
                    if !solid_at(x + dx, y + dy, z + dz) {
                        faces += 1;
                    }
                }
            }
        }
    }
    faces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_list(names: &[&str]) -> Nbt {
        Nbt::List(names.iter().map(|n| Nbt::String(n.to_string())).collect())
    }

    fn compound_list(names: &[&str]) -> Nbt {
        Nbt::List(
            names
                .iter()
                .map(|n| {
                    Nbt::Compound(Compound::from(vec![(
                        String::new(),
                        Nbt::String(n.to_string()),
                    )]))
                })
                .collect(),
        )
    }

    fn section(y: i8, palette: Nbt, data: Option<Vec<i64>>) -> Nbt {
        let mut states = vec![("palette".to_string(), palette)];
        if let Some(d) = data {
            states.push(("data".to_string(), Nbt::LongArray(d)));
        }
        Nbt::Compound(Compound::from(vec![
            ("Y".to_string(), Nbt::Byte(y)),
            ("block_states".to_string(), Nbt::Compound(Compound::from(states))),
        ]))
    }

    fn root(sections: Vec<Nbt>) -> Nbt {
        Nbt::Compound(Compound::from(vec![
            ("Status".to_string(), Nbt::String("minecraft:full".to_string())),
            ("DataVersion".to_string(), Nbt::Int(5023)),
            ("sections".to_string(), Nbt::List(sections)),
        ]))
    }

    #[test]
    fn bits_use_the_formats_minimum_of_four() {
        assert_eq!(bits_for(1), 4);
        assert_eq!(bits_for(2), 4);
        assert_eq!(bits_for(16), 4);
        assert_eq!(bits_for(17), 5);
        assert_eq!(bits_for(33), 6);
    }

    #[test]
    fn single_entry_palette_needs_no_data_and_fills_the_section() {
        let mut blocks = Blocks::new();
        let chunk = decode_chunk(
            &root(vec![section(-4, string_list(&["minecraft:bedrock"]), None)]),
            &mut blocks,
        )
        .expect("decodes");
        assert_eq!(chunk.solid, 4096);
        assert_eq!(chunk.at(0, 0, 0), blocks.lookup("minecraft:bedrock").unwrap());
        assert_eq!(chunk.at(15, 15, 15), blocks.lookup("minecraft:bedrock").unwrap());
    }

    #[test]
    fn both_palette_encodings_decode_identically() {
        let packed = vec![0i64; 256]; // 4 bits, 16 per long, all index 0
        let mut a = Blocks::new();
        let mut b = Blocks::new();
        let via_strings = decode_chunk(
            &root(vec![section(
                0,
                string_list(&["minecraft:stone", "minecraft:dirt"]),
                Some(packed.clone()),
            )]),
            &mut a,
        )
        .unwrap();
        let via_compounds = decode_chunk(
            &root(vec![section(
                0,
                compound_list(&["minecraft:stone", "minecraft:dirt"]),
                Some(packed),
            )]),
            &mut b,
        )
        .unwrap();
        assert_eq!(a.name(via_strings.at(3, 200, 7)), b.name(via_compounds.at(3, 200, 7)));
        assert_eq!(via_strings.solid, 4096);
    }

    #[test]
    fn packed_indices_land_in_the_right_blocks() {
        // 5 bits, 12 per long, palette of 17 so the 5-bit path is used.
        // Entry k gets palette index k % 17 by construction.
        let palette_len = 17usize;
        let names: Vec<String> = (0..palette_len)
            .map(|i| if i == 0 { AIR.to_string() } else { format!("minecraft:b{i}") })
            .collect();
        let mut data = vec![0i64; 342]; // padded length at 5 bits
        for k in 0..4096usize {
            let value = (k % palette_len) as u64;
            let long = k / 12;
            let shift = (k % 12) as u32 * 5;
            data[long] |= (value as i64) << shift;
        }
        let mut blocks = Blocks::new();
        let chunk = decode_chunk(
            &root(vec![section(1, string_list(&names.iter().map(|s| s.as_str()).collect::<Vec<_>>()), Some(data))]),
            &mut blocks,
        )
        .unwrap();
        for k in 0..4096usize {
            let y = 16 + k / 256; // section Y=1 starts at world Y=16
            let z = (k % 256) / 16;
            let x = k % 16;
            let row = (y as i32 - MIN_Y) as usize; // world Y -> array row
            let expected = &names[k % palette_len];
            assert_eq!(
                blocks.name(chunk.at(x, row, z)),
                expected,
                "index {k}"
            );
        }
    }

    #[test]
    fn a_wrong_long_count_is_reported_with_both_expectations() {
        let mut blocks = Blocks::new();
        // 17 entries -> 5 bits -> padded 342 longs; 320 is the compact length.
        let err = decode_chunk(
            &root(vec![section(
                0,
                string_list(&(0..17).map(|i| format!("minecraft:b{i}")).collect::<Vec<_>>()
                    .iter().map(|s| s.as_str()).collect::<Vec<_>>()),
                Some(vec![0i64; 320]),
            )]),
            &mut blocks,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("342"), "message was: {err}");
        assert!(err.contains("320"), "message was: {err}");
    }

    #[test]
    fn heightmap_values_are_offset_by_65() {
        // One 9-bit value of 65 means the top block is at Y=0.
        let mut longs = vec![0i64];
        longs[0] = 65;
        let heights = decode_height(&longs, 1);
        assert_eq!(heights[0], 0);
    }

    #[test]
    fn out_of_range_palette_index_is_an_error_not_a_panic() {
        let mut blocks = Blocks::new();
        // 4-bit data whose first entry is index 3 while the palette has 2 names.
        let mut data = vec![0i64; 256];
        data[0] = 3;
        let err = decode_chunk(
            &root(vec![section(
                0,
                string_list(&["minecraft:stone", "minecraft:dirt"]),
                Some(data),
            )]),
            &mut blocks,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("palette index 3"), "message was: {err}");
    }
}
