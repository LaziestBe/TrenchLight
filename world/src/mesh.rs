//! Turning block ids into triangles.
//!
//! One chunk, one flat vertex buffer, no greedy merging: for every solid block,
//! each of its six faces is emitted unless the neighbour in that direction is
//! also solid. A single 16×16×384 column produces a few thousand triangles, and
//! merging coplanar faces would save a fraction of that while making every
//! texture bug harder to find.
//!
//! ## Coordinate conventions
//!
//! * Block `(x, y, z)` occupies the unit cube from `(x, y, z)` to
//!   `(x+1, y+1, z+1)`, with **Y up and Z south** (Minecraft's own axes).
//! * Corners are wound **counter-clockwise as seen from outside** the cube, so
//!   `FrontFace::Ccw` + `CullMode::Back` keeps the outside.
//! * UV `v = 0` is the **top** of a tile, matching image row order, so a side
//!   face puts the texture's top row at `y+1`.
//!
//! ## What is not here
//!
//! Non-cube shapes are skipped rather than approximated: stairs, slabs, fences,
//! panes, doors, torches, flowers, grass and the rest simply produce no
//! geometry. That leaves holes you can see through, which is honest — drawing a
//! poppy as a cube would be a lie about the world. Block-state JSON is the fix
//! and it is not stage 1.

use bytemuck::{Pod, Zeroable};

use crate::textures::Atlas;
use crate::world::{Blocks, Chunk, HEIGHT, MIN_Y};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    /// Atlas UV; `0..1` across the whole atlas, computed per tile.
    pub uv: [f32; 2],
    /// Per-face brightness, standing in for lighting.
    pub light: f32,
}

/// One face: its neighbour offset, its four corners in CCW-outside order, and
/// the brightness vanilla gives that direction.
///
/// Corners are `(dx, dy, dz, u, v)`. The tables were worked out by hand and are
/// covered by a winding test below, because a flipped table renders nothing at
/// all once back-face culling is on.
struct FaceDef {
    /// Index into `FACES`; also the neighbour direction.
    dir: [i32; 3],
    light: f32,
    corners: [[f32; 5]; 4],
    /// Which tile rule to use: top, bottom or side.
    kind: TileKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileKind {
    Top,
    Bottom,
    Side,
}

const FACES: [FaceDef; 6] = [
    // +X, east
    FaceDef {
        dir: [1, 0, 0],
        light: 0.6,
        kind: TileKind::Side,
        corners: [
            [1.0, 0.0, 1.0, 0.0, 1.0],
            [1.0, 0.0, 0.0, 1.0, 1.0],
            [1.0, 1.0, 0.0, 1.0, 0.0],
            [1.0, 1.0, 1.0, 0.0, 0.0],
        ],
    },
    // -X, west
    FaceDef {
        dir: [-1, 0, 0],
        light: 0.6,
        kind: TileKind::Side,
        corners: [
            [0.0, 0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0, 1.0],
            [0.0, 1.0, 1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0, 0.0, 0.0],
        ],
    },
    // +Y, up
    FaceDef {
        dir: [0, 1, 0],
        light: 1.0,
        kind: TileKind::Top,
        corners: [
            [0.0, 1.0, 1.0, 0.0, 0.0],
            [1.0, 1.0, 1.0, 1.0, 0.0],
            [1.0, 1.0, 0.0, 1.0, 1.0],
            [0.0, 1.0, 0.0, 0.0, 1.0],
        ],
    },
    // -Y, down
    FaceDef {
        dir: [0, -1, 0],
        light: 0.5,
        kind: TileKind::Bottom,
        corners: [
            [0.0, 0.0, 0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0, 1.0, 1.0],
            [1.0, 0.0, 1.0, 1.0, 0.0],
            [0.0, 0.0, 1.0, 0.0, 0.0],
        ],
    },
    // +Z, south
    FaceDef {
        dir: [0, 0, 1],
        light: 0.8,
        kind: TileKind::Side,
        corners: [
            [0.0, 0.0, 1.0, 0.0, 1.0],
            [1.0, 0.0, 1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0, 1.0, 0.0],
            [0.0, 1.0, 1.0, 0.0, 0.0],
        ],
    },
    // -Z, north
    FaceDef {
        dir: [0, 0, -1],
        light: 0.8,
        kind: TileKind::Side,
        corners: [
            [1.0, 0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0, 1.0, 1.0],
            [0.0, 1.0, 0.0, 1.0, 0.0],
            [1.0, 1.0, 0.0, 0.0, 0.0],
        ],
    },
];

/// Blocks that have no full-cube geometry worth drawing in stage 1.
///
/// Matched as substrings of the block name without its `minecraft:` prefix. The
/// list is deliberately blunt: a false positive makes a block invisible, which
/// is easy to spot in a screenshot, while a false negative draws a flower as a
/// solid cube, which looks like a modelling bug rather than a missing feature.
const NOT_FULL_CUBE: &[&str] = &[
    "stairs",
    "slab",
    "fence",
    "pane",
    "door",
    "trapdoor",
    "torch",
    "lantern",
    "carpet",
    "rail",
    "candle",
    "button",
    "lever",
    "pressure_plate",
    "vine",
    "ladder",
    "chain",
    "flower_pot",
    "sapling",
    "chest",
    "spawner",
    "cobweb",
    "short_grass",
    "tall_grass",
    "fern",
    "flower",
    "mushroom",
    "bush",
    "wheat",
    "carrot",
    "potato",
    "beetroot",
    "banner",
    "anvil",
    "cauldron",
    "hopper",
    "repeater",
    "comparator",
    "piston",
    "observer",
    "amethyst_bud",
    "leaf_litter",
    "lichen",
    "kelp",
    "seagrass",
    "lily_pad",
    "sugar_cane",
    "bamboo",
    "scaffolding",
    "sign",
    "_wall",
];

fn short_name(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

pub fn is_full_cube(name: &str) -> bool {
    let n = short_name(name);
    if n == "air" || n == "cave_air" || n == "void_air" {
        return false;
    }
    !NOT_FULL_CUBE.iter().any(|needle| n.contains(needle))
}

/// Whether a block should produce cube geometry at all.
///
/// Two rules, in order:
///
/// 1. the name has to look like a cube ([`is_full_cube`]) — stairs, slabs and
///    the rest have no cube geometry worth faking;
/// 2. its own texture has to be mostly solid at the shader's alpha cutout.
///
/// Rule 2 is the one the old name list could not do. A dandelion is called
/// `dandelion`, not `flower`, so it passed rule 1 and rendered as a black card:
/// the texture's transparent texels are black RGB, and there was no alpha test.
/// Measuring the sprite instead of enumerating names also survives every plant
/// Mojang adds later — and it is why `opaque_fraction` exists in the atlas.
///
/// Water ships at alpha 180, above the 0.5 cutout, so a lake still draws. Glass
/// really is mostly holes, so it stops drawing as a solid cube: a hole you can
/// see through is honest about a shape this mesher cannot build.
pub fn is_drawable(name: &str, atlas: &Atlas) -> bool {
    if !is_full_cube(name) {
        return false;
    }
    [TileKind::Top, TileKind::Side, TileKind::Bottom]
        .into_iter()
        .all(|kind| atlas.opaque_fraction(&tile_name(name, kind)) >= 0.5)
}

/// Texture name for one face of a block.
///
/// Vanilla resolves this through `blockstates/*.json` and `models/block/*.json`.
/// Stage 1 skips those and uses two rules that cover every block in the spawn
/// column: one explicit exception (`grass_block` really is three textures) and
/// otherwise the block name itself. A name that resolves to nothing shows the
/// atlas's magenta placeholder rather than vanishing.
pub fn tile_name(block: &str, kind: TileKind) -> String {
    let n = short_name(block);
    match (n, kind) {
        // Fluids: vanilla ships `_still` and `_flow` since 1.13, and the plain
        // `water.png`/`lava.png` names do not exist. A static frame is the
        // `_still` one; the `_flow` strip is what a moving block would use.
        ("water", _) | ("bubble_column", _) => "water_still".to_string(),
        ("lava", _) => "lava_still".to_string(),
        ("flowing_water", _) => "water_flow".to_string(),
        ("flowing_lava", _) => "lava_flow".to_string(),
        ("grass_block", TileKind::Top) => "grass_block_top".to_string(),
        ("grass_block", TileKind::Side) => "grass_block_side".to_string(),
        ("grass_block", TileKind::Bottom) => "dirt".to_string(),
        ("mycelium", TileKind::Top) => "mycelium_top".to_string(),
        ("mycelium", TileKind::Side) => "mycelium_side".to_string(),
        ("mycelium", TileKind::Bottom) => "dirt".to_string(),
        ("podzol", TileKind::Top) => "podzol_top".to_string(),
        ("podzol", TileKind::Side) => "podzol_side".to_string(),
        ("podzol", TileKind::Bottom) => "dirt".to_string(),
        // Logs: the bark is on the sides and the rings on both ends.
        (other, TileKind::Side) if other.ends_with("_log") || other.ends_with("_wood") => {
            other.to_string()
        }
        (other, _) if other.ends_with("_log") => {
            let stem = other.trim_end_matches("_log");
            format!("{stem}_log_top")
        }
        (other, _) if other.ends_with("_wood") => {
            let stem = other.trim_end_matches("_wood");
            format!("{stem}_log_top")
        }
        (other, _) => other.to_string(),
    }
}

pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Faces actually emitted.
    pub faces: usize,
    /// Blocks that were skipped because they are not full cubes.
    pub skipped: usize,
    /// Blocks that looked like cubes by name but whose texture is mostly holes
    /// (a dandelion, a poppy). Counted apart from `skipped` so the self-test can
    /// say which rule did the work.
    pub skipped_transparent: usize,
    /// Block names whose texture is missing from the atlas.
    pub missing_textures: Vec<String>,
}

/// Build the mesh for one chunk.
///
/// `origin` is the chunk's world-space corner: block `(0, 0, 0)` of the chunk is
/// at `(origin.x, MIN_Y, origin.z)`. `outside` says, per horizontal side in the
/// order **+x, −x, +z, −z**, whether a neighbour outside the chunk counts as
/// solid. All-false draws the four walls of a lone column; all-true culls them,
/// which is what the self-test compares against the reference counter. Stage 2
/// passes a mix: a chunk with a loaded neighbour on one side and none on the
/// other would otherwise get a wall through the middle of the world.
pub fn build(
    chunk: &Chunk,
    blocks: &Blocks,
    atlas: &Atlas,
    origin: [i32; 2],
    outside: [bool; 4],
) -> Mesh {
    let air = blocks.air_like();
    // One decision per block id, made before the triples loop: `solid_at` asks
    // this question up to six times per solid block, and a HashMap lookup per
    // texel of the atlas per neighbour is not worth repeating 100k times.
    let drawable: Vec<bool> = (0..blocks.len())
        .map(|id| is_drawable(blocks.name(id as u16), atlas))
        .collect();
    let mut mesh = Mesh {
        vertices: Vec::new(),
        indices: Vec::new(),
        faces: 0,
        skipped: 0,
        skipped_transparent: 0,
        missing_textures: Vec::new(),
    };

    let solid_at = |x: i32, y: i32, z: i32| -> bool {
        if x < 0 || x > 15 || z < 0 || z > 15 {
            // Only the four horizontal sides can be outside, and only one of
            // them at a time: a corner neighbour (both out of range) is culled
            // when either side is, which is what the face being asked about
            // needs.
            let side = if x < 0 {
                Some(1)
            } else if x > 15 {
                Some(0)
            } else if z < 0 {
                Some(3)
            } else if z > 15 {
                Some(2)
            } else {
                None
            };
            return side.map(|i| outside[i]).unwrap_or(false);
        }
        if y < 0 || y >= HEIGHT as i32 {
            // Above and below the build range is air, so bedrock's underside and
            // the top of a build limit still get a face.
            return false;
        }
        let id = chunk.at(x as usize, y as usize, z as usize);
        // A skipped block must count as air here too, or the face of the block
        // behind it gets culled and the hole has no back. This is exactly the
        // discrepancy the self-test caught between the mesher and the counter.
        !air.contains(&id) && drawable[id as usize]
    };

    for y in 0..HEIGHT as i32 {
        for z in 0..16i32 {
            for x in 0..16i32 {
                let id = chunk.at(x as usize, y as usize, z as usize);
                if air.contains(&id) {
                    continue;
                }
                let name = blocks.name(id);
                if !is_full_cube(name) {
                    mesh.skipped += 1;
                    continue;
                }
                if !drawable[id as usize] {
                    mesh.skipped_transparent += 1;
                    continue;
                }
                for face in FACES.iter() {
                    let (dx, dy, dz) = (face.dir[0], face.dir[1], face.dir[2]);
                    if solid_at(x + dx, y + dy, z + dz) {
                        continue;
                    }
                    // Per face, not per block: a block with two visible faces
                    // used to emit both quads against the first one's vertices,
                    // so the second face drew a duplicate of the first and left
                    // its own square empty. That is the hole at the chunk's near
                    // corner in the first stage-1 screenshot.
                    let base = mesh.vertices.len() as u32;

                    let tile = tile_name(name, face.kind);
                    let [u0, v0, u1, v1] = atlas.uv(&tile);
                    if !atlas.has(&tile) && !mesh.missing_textures.contains(&tile) {
                        mesh.missing_textures.push(tile.clone());
                    }
                    let wx = (origin[0] * 16 + x) as f32;
                    let wy = (y + MIN_Y) as f32;
                    let wz = (origin[1] * 16 + z) as f32;
                    for corner in face.corners.iter() {
                        mesh.vertices.push(Vertex {
                            pos: [wx + corner[0], wy + corner[1], wz + corner[2]],
                            uv: [
                                u0 + (u1 - u0) * corner[3],
                                v0 + (v1 - v0) * corner[4],
                            ],
                            light: face.light,
                        });
                    }
                    mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                    mesh.faces += 1;
                }
            }
        }
    }

    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    }

    fn sub(a: [f32; 5], b: [f32; 5]) -> [f32; 3] {
        [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
    }

    /// Every face's first triangle must point along its declared normal.
    /// This is the test that catches a flipped or mis-ordered corner table,
    /// which with back-face culling on renders as nothing at all.
    #[test]
    fn every_face_winds_along_its_normal() {
        for face in FACES.iter() {
            let a = face.corners[0];
            let b = face.corners[1];
            let c = face.corners[2];
            let n = cross(sub(b, a), sub(c, a));
            let expected = [
                face.dir[0] as f32,
                face.dir[1] as f32,
                face.dir[2] as f32,
            ];
            let dot = n[0] * expected[0] + n[1] * expected[1] + n[2] * expected[2];
            assert!(
                dot > 0.0,
                "face with normal {expected:?} winds the other way (dot {dot})"
            );
        }
    }

    /// The six normals must be one per axis direction, or a face is missing or
    /// duplicated.
    #[test]
    fn there_is_exactly_one_face_per_direction() {
        let mut dirs: Vec<[i32; 3]> = FACES.iter().map(|f| f.dir).collect();
        dirs.sort();
        assert_eq!(
            dirs,
            vec![
                [-1, 0, 0],
                [0, -1, 0],
                [0, 0, -1],
                [0, 0, 1],
                [0, 1, 0],
                [1, 0, 0]
            ]
        );
    }

    /// Every quad must index its own four vertices.
    ///
    /// This is the bug the first stage-1 screenshot exposed: `base` was taken
    /// once per block, so a block with two visible faces emitted both quads
    /// against the first face's vertices. The second face then drew a duplicate
    /// of the first and left its own square empty, which showed up as a 10 px
    /// hole at the chunk's near corner — half a block wide, at the one place
    /// where two walls meet.
    #[test]
    fn every_face_indexes_its_own_four_vertices() {
        let mut blocks = Blocks::new();
        let stone = blocks.intern("minecraft:stone");

        // One block in the middle of an otherwise empty chunk: six faces, and
        // no neighbour to cull any of them.
        let y = 100;
        let mut chunk = Chunk {
            blocks: vec![0; HEIGHT * 256],
            sections_present: 0,
            non_air_sections: 0,
            solid: 1,
            data_version: 0,
            status: String::new(),
            heightmaps: Default::default(),
            palette_kinds: Default::default(),
        };
        chunk.blocks[y * 256 + 8 * 16 + 8] = stone;

        let atlas = Atlas::empty_for_tests();
        let mesh = build(&chunk, &blocks, &atlas, [0, 0], [false; 4]);

        assert_eq!(mesh.faces, 6, "expected six faces on a lone block");
        assert_eq!(mesh.vertices.len(), 24);
        assert_eq!(mesh.indices.len(), 36);
        for (face, quad) in mesh.indices.chunks(6).enumerate() {
            for &idx in quad {
                let lo = (face * 4) as u32;
                assert!(
                    (lo..lo + 4).contains(&idx),
                    "face {face} indexes vertex {idx}, outside its own quad {lo}..{}",
                    lo + 4
                );
            }
        }
    }

    /// Each boundary flag must affect only its own side. A sideways or reversed
    /// mapping here culls one wall and draws the one through the middle of two
    /// adjacent chunks, which looks like terrain, not a bug.
    ///
    /// One block sitting on an edge has exactly one boundary side, so the count
    /// says which side the flag acted on: 6 faces normally, 5 when that side is
    /// culled, and 6 again when the flag is put on the opposite side.
    #[test]
    fn a_boundary_face_follows_its_own_side() {
        let mut blocks = Blocks::new();
        let stone = blocks.intern("minecraft:stone");
        let y = 100;
        let empty = || Chunk {
            blocks: vec![0; HEIGHT * 256],
            sections_present: 0,
            non_air_sections: 0,
            solid: 1,
            data_version: 0,
            status: String::new(),
            heightmaps: Default::default(),
            palette_kinds: Default::default(),
        };
        let atlas = Atlas::empty_for_tests();

        // x = 15, so the block's +x neighbour is outside the chunk.
        let mut east = empty();
        east.blocks[y * 256 + 8 * 16 + 15] = stone;
        assert_eq!(
            build(&east, &blocks, &atlas, [0, 0], [true, false, false, false]).faces,
            5,
            "+x culled when the +x flag is set"
        );
        assert_eq!(
            build(&east, &blocks, &atlas, [0, 0], [false, false, false, false]).faces,
            6,
            "+x drawn when no flag is set"
        );
        assert_eq!(
            build(&east, &blocks, &atlas, [0, 0], [false, true, false, false]).faces,
            6,
            "a -x flag must not cull the +x face"
        );

        // z = 15, so the block's +z neighbour is outside the chunk.
        let mut south = empty();
        south.blocks[y * 256 + 15 * 16 + 8] = stone;
        assert_eq!(
            build(&south, &blocks, &atlas, [0, 0], [false, false, true, false]).faces,
            5,
            "+z culled when the +z flag is set"
        );
        assert_eq!(
            build(&south, &blocks, &atlas, [0, 0], [false, false, false, true]).faces,
            6,
            "a -z flag must not cull the +z face"
        );
    }

    #[test]
    fn grass_block_uses_three_different_tiles() {
        assert_eq!(tile_name("minecraft:grass_block", TileKind::Top), "grass_block_top");
        assert_eq!(tile_name("minecraft:grass_block", TileKind::Side), "grass_block_side");
        assert_eq!(tile_name("minecraft:grass_block", TileKind::Bottom), "dirt");
    }

    #[test]
    fn a_log_uses_bark_on_the_sides_and_rings_on_the_ends() {
        assert_eq!(tile_name("minecraft:acacia_log", TileKind::Side), "acacia_log");
        assert_eq!(
            tile_name("minecraft:acacia_log", TileKind::Top),
            "acacia_log_top"
        );
        assert_eq!(
            tile_name("minecraft:acacia_log", TileKind::Bottom),
            "acacia_log_top"
        );
    }

    #[test]
    fn plain_blocks_are_their_own_texture() {
        assert_eq!(tile_name("minecraft:deepslate", TileKind::Side), "deepslate");
        assert_eq!(tile_name("minecraft:dirt", TileKind::Top), "dirt");
    }

    /// The spawn column's real contents: these must all be drawn, and the
    /// non-cubes in it must not be.
    #[test]
    fn the_spawn_columns_blocks_are_classified_correctly() {
        for cube in [
            "minecraft:deepslate",
            "minecraft:stone",
            "minecraft:granite",
            "minecraft:andesite",
            "minecraft:diorite",
            "minecraft:tuff",
            "minecraft:bedrock",
            "minecraft:dirt",
            "minecraft:lava",
            "minecraft:grass_block",
            "minecraft:gravel",
            "minecraft:copper_ore",
            "minecraft:acacia_leaves",
            "minecraft:coal_ore",
            "minecraft:water",
            "minecraft:iron_ore",
            "minecraft:deepslate_redstone_ore",
            "minecraft:deepslate_diamond_ore",
            "minecraft:acacia_log",
            "minecraft:lapis_ore",
        ] {
            assert!(is_full_cube(cube), "{cube} should be a full cube");
        }
        for not in ["minecraft:short_grass", "minecraft:air", "minecraft:oak_fence"] {
            assert!(!is_full_cube(not), "{not} should not be a full cube");
        }
    }

    /// The gate that replaced the name list for plants: a dandelion is a cube
    /// by name, but its sprite is 12.5% solid, so it must not be drawn. Water
    /// and stone must still draw.
    #[test]
    fn a_flower_is_skipped_by_its_measured_opacity() {
        let atlas = Atlas::with_opacity(&[
            ("dandelion", 0.125),
            ("poppy", 0.2),
            ("stone", 1.0),
            ("water_still", 1.0),
        ]);
        assert!(!is_drawable("minecraft:dandelion", &atlas));
        assert!(!is_drawable("minecraft:poppy", &atlas));
        assert!(is_drawable("minecraft:stone", &atlas));
        assert!(is_drawable("minecraft:water", &atlas));
        // Vanilla grass is three different sprites; all three must pass.
        let grass = Atlas::with_opacity(&[
            ("grass_block_top", 1.0),
            ("grass_block_side", 1.0),
            ("dirt", 1.0),
        ]);
        assert!(is_drawable("minecraft:grass_block", &grass));
        // …and one hole in that set is enough to drop the block.
        let holed = Atlas::with_opacity(&[
            ("grass_block_top", 1.0),
            ("grass_block_side", 0.1),
            ("dirt", 1.0),
        ]);
        assert!(!is_drawable("minecraft:grass_block", &holed));
    }
}
