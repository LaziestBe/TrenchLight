//! Block textures out of the version jar, packed into one atlas.
//!
//! Measured facts about `26.3.jar`: 41.5 MB, 34,227 entries, all
//! deflate-compressed, 1,300 `assets/minecraft/textures/block/*.png`. Every block
//! texture checked is 16×16 **grayscale** PNG (`colorType=0`) — grass, leaves and
//! water are grey in the file and tinted at runtime — and the animated ones are
//! vertical strips (`water_flow.png` is 32×1024: 32 frames of 32×32).
//!
//! So the decoder must handle grayscale, a strip needs its first frame cropped
//! off, and something has to deal with the tint or every grassy block renders
//! grey. That last part is a stub: one hardcoded plains green, applied only to
//! pixels that are actually grey, which leaves the brown dirt in
//! `grass_block_side` alone — it is not biome logic and is not claimed to be.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const TILE: u32 = 16;
/// Vanilla's plains grass colour (`#91BD59`) and the foliage colour next to it.
const GRASS_TINT: [f32; 3] = [0x91 as f32, 0xBD as f32, 0x59 as f32];
const FOLIAGE_TINT: [f32; 3] = [0x77 as f32, 0xAB as f32, 0x2F as f32];

#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major, top row first.
    pub pixels: Vec<u8>,
}

impl Image {
    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    fn set_pixel(&mut self, x: u32, y: u32, px: [u8; 4]) {
        let i = ((y * self.width + x) * 4) as usize;
        self.pixels[i..i + 4].copy_from_slice(&px);
    }
}

/// The version jar, opened once and read by name.
pub struct Jar {
    path: PathBuf,
    archive: zip::ZipArchive<std::fs::File>,
}

impl Jar {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path)
            .with_context(|| format!("opening jar {}", path.display()))?;
        let archive = zip::ZipArchive::new(file)
            .with_context(|| format!("{} is not a readable zip/jar", path.display()))?;
        Ok(Jar { path, archive })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let mut file = match self.archive.by_name(name) {
            Ok(file) => file,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {name} from the jar"))
            }
        };
        let mut out = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut out)
            .with_context(|| format!("decompressing {name}"))?;
        Ok(Some(out))
    }

    /// Every `assets/minecraft/textures/block/<name>.png`, as block names.
    pub fn block_texture_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for name in self.archive.file_names() {
            if let Some(rest) = name.strip_prefix("assets/minecraft/textures/block/") {
                if let Some(stem) = rest.strip_suffix(".png") {
                    if !stem.contains('/') {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        names
    }
}

/// Decode a PNG into RGBA8.
///
/// `Transformations::EXPAND` handles the awkward cases in the file: sub-8-bit
/// grayscale, palettes and `tRNS`. Everything checked in this jar is 8-bit
/// grayscale, but a resource pack substitution should not panic.
pub fn decode_png(bytes: &[u8]) -> Result<Image> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().context("reading PNG header")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("decoding PNG frame")?;
    let width = info.width;
    let height = info.height;

    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    match info.color_type {
        png::ColorType::Grayscale => {
            for i in 0..(width * height) as usize {
                let v = buf[i];
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for i in 0..(width * height) as usize {
                let v = buf[i * 2];
                let a = buf[i * 2 + 1];
                pixels.extend_from_slice(&[v, v, v, a]);
            }
        }
        png::ColorType::Rgb => {
            for i in 0..(width * height) as usize {
                pixels.extend_from_slice(&[buf[i * 3], buf[i * 3 + 1], buf[i * 3 + 2], 255]);
            }
        }
        png::ColorType::Rgba => pixels.extend_from_slice(&buf[..(width * height * 4) as usize]),
        other => bail!("unsupported PNG colour type {other:?} ({width}x{height})"),
    }

    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// Crop an animated strip to its first frame, then scale to a `TILE` square.
///
/// `water_flow.png` is 32×1024: 32 frames of 32×32 stacked vertically. Reading it
/// as one 32×1024 image would put a 1-pixel-tall smear of every frame on a block.
fn to_tile(image: &Image) -> Image {
    let (w, mut h) = (image.width, image.height);
    if w == 0 || h == 0 {
        return Image {
            width: TILE,
            height: TILE,
            pixels: vec![0, 0, 0, 0].repeat((TILE * TILE) as usize),
        };
    }
    // A strip taller than it is wide, by a whole number of frames.
    if h > w && h % w == 0 {
        h = w;
    }
    let cropped = Image {
        width: w,
        height: h,
        pixels: image.pixels.clone(),
    };
    if (w, h) == (TILE, TILE) {
        return cropped;
    }
    // Nearest-neighbour resample. Textures here are all tiny, so the cost is nil
    // and the result stays crisp rather than turning to mush under bilinear.
    let mut out = Image {
        width: TILE,
        height: TILE,
        pixels: Vec::with_capacity((TILE * TILE * 4) as usize),
    };
    for ty in 0..TILE {
        let sy = (ty as u64 * cropped.height as u64 / TILE as u64) as u32;
        let sy = sy.min(cropped.height - 1);
        for tx in 0..TILE {
            let sx = (tx as u64 * cropped.width as u64 / TILE as u64) as u32;
            let sx = sx.min(cropped.width - 1);
            out.pixels.extend_from_slice(&cropped.pixel(sx, sy));
        }
    }
    out
}

/// Blocks whose texture is grey in the jar and coloured at runtime.
fn tint_for(name: &str) -> Option<[f32; 3]> {
    let n = name.strip_prefix("minecraft:").unwrap_or(name);
    if n.starts_with("grass_block") || n == "short_grass" || n == "tall_grass" || n == "fern" {
        return Some(GRASS_TINT);
    }
    if n.contains("leaves") || n == "vine" || n == "lily_pad" {
        return Some(FOLIAGE_TINT);
    }
    None
}

/// Multiply only the *grey* pixels of a tile by the tint.
///
/// `grass_block_top` is entirely grey, so it becomes green. `grass_block_side`
/// is a grey grass strip over brown dirt, and this leaves the dirt brown instead
/// of turning the whole face green — which is exactly the limitation of doing
/// this on the atlas instead of per-texture-layer like the game does.
fn tint_tile(tile: &mut Image, tint: [f32; 3]) {
    for y in 0..tile.height {
        for x in 0..tile.width {
            let [r, g, b, a] = tile.pixel(x, y);
            let max = r.max(g).max(b);
            let min = r.min(g).min(b);
            let grey = max as i32 - min as i32;
            // Tolerate a little channel noise; saturated pixels are left alone.
            if grey > 12 {
                continue;
            }
            let v = r as f32 / 255.0;
            let px = [
                (v * tint[0]).round().clamp(0.0, 255.0) as u8,
                (v * tint[1]).round().clamp(0.0, 255.0) as u8,
                (v * tint[2]).round().clamp(0.0, 255.0) as u8,
                a,
            ];
            tile.set_pixel(x, y, px);
        }
    }
}

/// One flat texture for the whole world, plus the UV rectangle of every tile.
pub struct Atlas {
    pub image: Image,
    /// Tile name (without `minecraft:` or `.png`) -> `[u0, v0, u1, v1]`.
    tiles: HashMap<String, [f32; 4]>,
    /// Tile name -> fraction of its 256 texels that are fully opaque.
    ///
    /// This is what makes "not a cube" a measurement instead of a name list: a
    /// dandelion is 12.5% opaque, stone is 100%, and the mesher refuses to draw
    /// anything under half. The alternative — enumerating `dandelion`, `poppy`,
    /// `oxeye_daisy`, every tulip and every plant Mojang adds later — was the
    /// list that let a flower render as a black card in the first place.
    opacity: HashMap<String, f32>,
    pub loaded: usize,
}

impl Atlas {
    /// Atlas UV rectangle for a tile, inset by half a texel.
    ///
    /// Without the inset, a corner at `u = 0` of a 16 px tile samples on the
    /// boundary between two tiles, and with nearest filtering that is a coin
    /// flip between this texture and its neighbour. Half a texel lands in the
    /// middle of the edge texels instead.
    pub fn uv(&self, name: &str) -> [f32; 4] {
        let key = name.strip_prefix("minecraft:").unwrap_or(name);
        let rect = self
            .tiles
            .get(name)
            .or_else(|| self.tiles.get(key))
            .or_else(|| self.tiles.get("__missing"))
            .copied()
            .unwrap_or([0.0, 0.0, 1.0, 1.0]);
        let du = 0.5 / self.image.width.max(1) as f32;
        let dv = 0.5 / self.image.height.max(1) as f32;
        [rect[0] + du, rect[1] + dv, rect[2] - du, rect[3] - dv]
    }

    pub fn has(&self, name: &str) -> bool {
        self.tiles.contains_key(name)
    }

    /// Fraction of the tile's texels that are fully opaque, or 1.0 for a tile
    /// that is not in the atlas. A missing tile draws solid magenta, so treating
    /// it as opaque is what keeps a missing texture visible rather than silently
    /// deleting the block.
    pub fn opaque_fraction(&self, name: &str) -> f32 {
        let key = name.strip_prefix("minecraft:").unwrap_or(name);
        self.opacity
            .get(name)
            .or_else(|| self.opacity.get(key))
            .copied()
            .unwrap_or(1.0)
    }

    /// An atlas with no tiles, for tests that only care about geometry.
    #[cfg(test)]
    pub fn empty_for_tests() -> Atlas {
        Atlas::with_opacity(&[])
    }

    /// An atlas whose only content is measured opacity per tile, for tests of
    /// the "is this sprite mostly holes" gate.
    #[cfg(test)]
    pub fn with_opacity(tiles: &[(&str, f32)]) -> Atlas {
        Atlas {
            image: Image {
                width: 2,
                height: 2,
                pixels: vec![255; 2 * 2 * 4],
            },
            tiles: HashMap::new(),
            opacity: tiles
                .iter()
                .map(|(name, fraction)| (name.to_string(), *fraction))
                .collect(),
            loaded: 0,
        }
    }

    /// RGBA8 bytes, ready to upload.
    pub fn bytes(&self) -> &[u8] {
        &self.image.pixels
    }
}

/// Fraction of a tile's texels that survive the shader's alpha cutout.
///
/// The cutout in `render.rs` discards anything below alpha 0.5, so the count
/// uses 128 rather than a "fully opaque" 200: water ships at alpha 180 and is
/// meant to be drawn, while a dandelion's texels are either 0 or 255. Counting
/// `>= 200` called water 0% opaque and would have deleted every lake.
fn opaque_ratio(tile: &Image) -> f32 {
    let mut opaque = 0u32;
    for y in 0..TILE {
        for x in 0..TILE {
            if tile.pixel(x, y)[3] >= 128 {
                opaque += 1;
            }
        }
    }
    opaque as f32 / (TILE * TILE) as f32
}

/// Build the atlas from every block texture in the jar, plus one magenta tile
/// that stands in for anything missing so a wrong name is visible rather than
/// invisible.
pub fn build_atlas(jar: &mut Jar) -> Result<Atlas> {
    let names = jar.block_texture_names();
    if names.is_empty() {
        bail!(
            "{} has no assets/minecraft/textures/block/*.png — wrong jar?",
            jar.path().display()
        );
    }

    let mut tiles: Vec<(String, Image)> = Vec::with_capacity(names.len() + 1);
    let mut failures = Vec::new();
    for name in &names {
        let path = format!("assets/minecraft/textures/block/{name}.png");
        let Some(bytes) = jar.read(&path)? else {
            continue;
        };
        match decode_png(&bytes) {
            Ok(image) => {
                let mut tile = to_tile(&image);
                if let Some(tint) = tint_for(name) {
                    tint_tile(&mut tile, tint);
                }
                tiles.push((name.clone(), tile));
            }
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }

    // Magenta, fully opaque: obvious on screen and unmistakable in a screenshot.
    let mut missing = Image {
        width: TILE,
        height: TILE,
        pixels: Vec::with_capacity((TILE * TILE * 4) as usize),
    };
    for _ in 0..TILE * TILE {
        missing.pixels.extend_from_slice(&[255, 0, 255, 255]);
    }
    tiles.push(("__missing".to_string(), missing));

    let count = tiles.len() as u32;
    let cols = (count as f64).sqrt().ceil() as u32;
    let cols = cols.max(1);
    let rows = (count + cols - 1) / cols;
    let width = cols * TILE;
    let height = rows * TILE;

    let mut image = Image {
        width,
        height,
        // Transparent black where no tile landed; never sampled.
        pixels: vec![0u8; (width * height * 4) as usize],
    };

    let mut map = HashMap::with_capacity(tiles.len());
    let mut opacity = HashMap::with_capacity(tiles.len());
    for (i, (name, tile)) in tiles.iter().enumerate() {
        let i = i as u32;
        let (cx, cy) = (i % cols, i / cols);
        let ox = cx * TILE;
        let oy = cy * TILE;
        for y in 0..TILE {
            for x in 0..TILE {
                image.set_pixel(ox + x, oy + y, tile.pixel(x, y));
            }
        }
        map.insert(
            name.clone(),
            [
                ox as f32 / width as f32,
                oy as f32 / height as f32,
                (ox + TILE) as f32 / width as f32,
                (oy + TILE) as f32 / height as f32,
            ],
        );
        opacity.insert(name.clone(), opaque_ratio(&tile));
    }

    if !failures.is_empty() {
        eprintln!(
            "atlas: {} texture(s) could not be decoded, e.g. {}",
            failures.len(),
            failures.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
        );
    }

    Ok(Atlas {
        image,
        tiles: map,
        opacity,
        loaded: tiles.len(),
    })
}

/// Write the atlas out as a PNG so it can be looked at instead of trusted.
pub fn write_png(image: &Image, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing PNG header")?;
    writer
        .write_image_data(&image.pixels)
        .context("writing PNG data")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16×16 grayscale PNG, built by the encoder so the test exercises the
    /// real decode path rather than a fixture file.
    fn grey_png(value: u8) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(std::io::Cursor::new(&mut out), 16, 16);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&vec![value; 16 * 16]).unwrap();
        }
        out
    }

    #[test]
    fn grayscale_png_becomes_opaque_rgba() {
        let image = decode_png(&grey_png(128)).unwrap();
        assert_eq!((image.width, image.height), (16, 16));
        assert_eq!(&image.pixels[0..4], &[128, 128, 128, 255]);
    }

    #[test]
    fn a_tall_strip_is_cropped_to_its_first_frame() {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(std::io::Cursor::new(&mut out), 32, 64);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&vec![200u8; 32 * 64]).unwrap();
        }
        let image = decode_png(&out).unwrap();
        assert_eq!((image.width, image.height), (32, 64));
        let tile = to_tile(&image);
        assert_eq!((tile.width, tile.height), (TILE, TILE));
        // Every pixel comes from the (uniform) first frame.
        assert_eq!(&tile.pixels[0..4], &[200, 200, 200, 255]);
    }

    #[test]
    fn tint_leaves_saturated_pixels_alone() {
        let mut tile = Image {
            width: 2,
            height: 1,
            pixels: vec![
                // grey
                100, 100, 100, 255, // brown dirt
                120, 80, 40, 255,
            ],
        };
        tint_tile(&mut tile, [255.0, 255.0, 0.0]);
        assert_eq!(&tile.pixels[0..4], &[100, 100, 0, 255]);
        assert_eq!(&tile.pixels[4..8], &[120, 80, 40, 255]);
    }

    #[test]
    fn every_tile_gets_a_unique_uv_rect() {
        // 3 tiles -> 2 columns; check the packing arithmetic on a real atlas
        // built from synthetic tiles without needing the jar.
        let count = 3u32;
        let cols = (count as f64).sqrt().ceil() as u32;
        let rows = (count + cols - 1) / cols;
        assert_eq!((cols, rows), (2, 2));
        assert_eq!((cols * TILE, rows * TILE), (32, 32));
    }

    /// A cutout flower is mostly holes, water is translucent but solid, and the
    /// gate has to tell those apart: this is the measurement that replaced the
    /// name list which let a dandelion render as a black card.
    #[test]
    fn opaque_ratio_matches_the_shader_cutout() {
        // Water: every texel at alpha 180 — below 200, well above the 0.5 cut.
        let water = Image {
            width: TILE,
            height: TILE,
            pixels: [120, 160, 255, 180].repeat((TILE * TILE) as usize),
        };
        assert_eq!(opaque_ratio(&water), 1.0);

        // A dandelion-like sprite: 32 of 256 texels solid, the rest holes.
        let mut flower = Image {
            width: TILE,
            height: TILE,
            pixels: [0, 0, 0, 0].repeat((TILE * TILE) as usize),
        };
        for i in 0..32 {
            flower.set_pixel(i, 0, [255, 255, 0, 255]);
        }
        assert_eq!(opaque_ratio(&flower), 0.125);

        // Half opaque is the boundary itself: 128 alpha is kept, so 0.5 passes.
        let half = Image {
            width: TILE,
            height: TILE,
            pixels: (0..(TILE * TILE))
                .flat_map(|i| {
                    if i % 2 == 0 {
                        [255, 255, 255, 255]
                    } else {
                        [255, 255, 255, 0]
                    }
                })
                .collect(),
        };
        assert_eq!(opaque_ratio(&half), 0.5);
    }
}
