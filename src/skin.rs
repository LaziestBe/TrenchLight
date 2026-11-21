//! Skins for an offline profile.
//!
//! A skin normally comes from the session server, and this launcher never talks
//! to it — so there is no upload, and pretending otherwise would be a lie in
//! the UI. What *can* be done offline is replace the player textures with a
//! resource pack: the game asks a pack for `entity/player/wide/<name>.png`
//! before its own jar, and an offline player has nothing better to show. Worth
//! knowing, because it cost a real test: the game ships **nine** default
//! characters, not one, and picks between them by UUID.
//!
//! Two honest consequences, both stated in the window:
//! - the arm model (classic or slim) is chosen by the game from the offline
//!   UUID, not by the texture, so a slim skin on a classic body has thick arms;
//! - every player *without* a skin of their own wears this one, because the
//!   textures it replaces are the ones those players fall back to.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// Folder name of the generated pack, and the id `options.txt` refers to.
const PACK: &str = "mcl-skin";
const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Big enough for the widest skin anyone ships, small enough that a mis-picked
/// wallpaper is refused before it is copied into every installation.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The nine skins the game ships, in the order its own folder lists them.
///
/// 1.21.9 and later stopped using a single `steve`/`alex` pair: there are nine
/// default characters, an offline profile is assigned one by its UUID, and any
/// of them can be the one on screen. A pack that replaces two of them is a pack
/// that does nothing six times out of nine — which is exactly what the first
/// build of this did, and it took a screenshot of the actual player to see it.
const DEFAULT_SKINS: [&str; 9] = [
    "alex", "ari", "efe", "kai", "makena", "noor", "steve", "sunny", "zuri",
];

/// Three keys, because three eras of the game read this file differently.
///
/// - 1.21.9 and later (26.3 needs resource format **97**) want `min_format` and
///   `max_format`; without them the primary parse fails and the game falls back
///   to the legacy field, which is what got the first version of this pack
///   dropped from `options.txt` with "no longer compatible".
/// - 1.20.2 to 1.21.8 read `supported_formats`.
/// - everything older only compares `pack_format`, and knows nothing of ranges.
///
/// Wide ranges are deliberate: the format number moves every release, and a
/// pack whose only job is to replace one texture should not go stale because of
/// it. Unknown keys are ignored, so declaring all three is safe.
const MCMETA: &str = concat!(
    r#"{"pack":{"description":"Trenchlight skin","pack_format":15,"#,
    r#""min_format":1,"max_format":999,"#,
    r#""supported_formats":{"min_inclusive":1,"max_inclusive":999}}}"#,
    "\n"
);

pub struct Skin {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// Width and height out of a PNG header, without decoding the image.
pub fn size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != PNG_SIG || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

/// A skin is 64 wide and either 32 or 64 tall; HD skins are multiples of that.
/// Anything else is a screenshot, an avatar or a photo, and the game would show
/// a quarter of it as a face.
pub fn is_skin(width: u32, height: u32) -> bool {
    width >= 64 && width % 64 == 0 && (height == width || height == width / 2)
}

/// Read and validate the file the picker returned.
pub fn read(path: &Path) -> Result<Skin> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > MAX_BYTES {
        bail!(
            "that file is {} MB; a skin is a few KB",
            meta.len() / (1024 * 1024)
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (width, height) = size(&bytes)
        .with_context(|| format!("{} is not a PNG", path.display()))?;
    if !is_skin(width, height) {
        bail!("a skin is 64x64 (or a multiple, like 128x128); that image is {width}x{height}");
    }
    Ok(Skin {
        width,
        height,
        bytes,
    })
}

/// Where the pack sits inside one installation's game folder.
fn pack_dir(instance_dir: &Path) -> std::path::PathBuf {
    instance_dir.join("resourcepacks").join(PACK)
}

/// True when the pack is on disk *and* enabled: a pack the game was never told
/// about is not applied, however correct its files are.
pub fn is_live(instance_dir: &Path) -> bool {
    if !pack_dir(instance_dir).join("pack.mcmeta").is_file() {
        return false;
    }
    std::fs::read_to_string(instance_dir.join("options.txt"))
        .map(|text| {
            text.lines()
                .any(|line| line.starts_with("resourcePacks:") && line.contains("file/mcl-skin"))
        })
        .unwrap_or(false)
}

/// Write the pack into one installation and switch it on. Replaces any earlier
/// copy outright, so a stale texture from the last skin can never survive.
pub fn write_pack(instance_dir: &Path, png: &[u8]) -> Result<()> {
    let dir = pack_dir(instance_dir);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("replacing {}", dir.display()))?;
    }
    let entity = dir
        .join("assets")
        .join("minecraft")
        .join("textures")
        .join("entity");
    // 1.20 moved the player textures into wide/ and slim/; before that they sat
    // one level up. Writing both costs two small files and covers every version
    // this launcher can run.
    let wide = entity.join("player").join("wide");
    let slim = entity.join("player").join("slim");
    std::fs::create_dir_all(&wide)?;
    std::fs::create_dir_all(&slim)?;
    // Every name, because the game picks one of the nine from the offline UUID
    // and never says which. One texture each way is a coin flip on seven sides.
    for name in DEFAULT_SKINS {
        std::fs::write(wide.join(format!("{name}.png")), png)?;
        std::fs::write(slim.join(format!("{name}.png")), png)?;
    }
    // The pre-1.20 layout, for installations old enough to ask for it.
    std::fs::write(entity.join("steve.png"), png)?;
    std::fs::write(entity.join("alex.png"), png)?;
    std::fs::write(dir.join("pack.mcmeta"), MCMETA)?;
    enable(&instance_dir.join("options.txt"), true)
}

/// Take the pack away again: files out, entry out of `options.txt`.
pub fn remove_pack(instance_dir: &Path) -> Result<()> {
    let dir = pack_dir(instance_dir);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    enable(&instance_dir.join("options.txt"), false)
}

/// Add or drop this pack in the game's own options, leaving every other line
/// byte for byte as it was.
pub fn enable(options: &Path, on: bool) -> Result<()> {
    let text = std::fs::read_to_string(options).unwrap_or_default();
    let next = rewrite(&text, on);
    if next != text {
        std::fs::write(options, next).with_context(|| format!("writing {}", options.display()))?;
    }
    Ok(())
}

fn rewrite(text: &str, on: bool) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut seen_packs = false;
    for line in lines.iter_mut() {
        let (body, cr) = match line.strip_suffix('\r') {
            Some(body) => (body.to_string(), true),
            None => (line.clone(), false),
        };
        let replace = if let Some(rest) = body.strip_prefix("resourcePacks:") {
            seen_packs = true;
            let mut items = parse_list(rest);
            items.retain(|item| item != "file/mcl-skin");
            if on {
                if !items.iter().any(|item| item == "vanilla") {
                    items.insert(0, "vanilla".to_string());
                }
                items.push("file/mcl-skin".to_string());
            }
            Some(format!("resourcePacks:{}", format_list(&items)))
        } else if let Some(rest) = body.strip_prefix("incompatibleResourcePacks:") {
            // A pack listed here is the one the game refuses to load.
            let items: Vec<String> = parse_list(rest)
                .into_iter()
                .filter(|item| item != "file/mcl-skin")
                .collect();
            Some(format!("incompatibleResourcePacks:{}", format_list(&items)))
        } else {
            None
        };
        if let Some(new) = replace {
            *line = if cr { new + "\r" } else { new };
        }
    }
    // A fresh installation has no options.txt yet; the game fills in the rest
    // on first run, and an options file with one line is valid.
    if on && !seen_packs {
        lines.insert(0, "resourcePacks:[\"vanilla\",\"file/mcl-skin\"]".to_string());
    }
    lines.join("\n")
}

fn parse_list(rest: &str) -> Vec<String> {
    let text = rest.trim();
    if let Ok(items) = serde_json::from_str::<Vec<String>>(text) {
        return items;
    }
    text.trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn format_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|item| serde_json::to_string(item).unwrap_or_else(|_| format!("\"{item}\"")))
        .collect();
    format!("[{}]", quoted.join(","))
}

/// Base64 for the preview. The page cannot read files, and a skin is small
/// enough to ride in the boot payload.
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enough of a PNG for the header check; the pack copies bytes, it never
    /// decodes them.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(PNG_SIG);
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0]);
        out
    }

    #[test]
    fn only_skin_shaped_pngs_are_accepted() {
        assert!(is_skin(64, 64));
        assert!(is_skin(64, 32));
        assert!(is_skin(128, 128));
        assert!(is_skin(256, 128));
        for (w, h) in [(63, 63), (64, 48), (100, 100), (32, 32), (0, 0)] {
            assert!(!is_skin(w, h), "{w}x{h} was accepted");
        }
        assert_eq!(size(&png(64, 64)), Some((64, 64)));
        assert_eq!(size(b"not a png"), None);
    }

    #[test]
    fn the_pack_is_written_enabled_and_taken_away_again() {
        let dir = std::env::temp_dir().join(format!("mcl-skin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // An options file the user already edited, CRLF and all.
        let options = dir.join("options.txt");
        std::fs::write(
            &options,
            "fov:0.0\r\nresourcePacks:[\"vanilla\",\"file/other.zip\"]\r\nincompatibleResourcePacks:[\"file/mcl-skin\"]\r\n",
        )
        .unwrap();

        let art = png(64, 64);
        write_pack(&dir, &art).unwrap();

        assert!(pack_dir(&dir).join("pack.mcmeta").is_file());
        for name in DEFAULT_SKINS {
            for kind in ["wide", "slim"] {
                let path = pack_dir(&dir)
                    .join(format!("assets/minecraft/textures/entity/player/{kind}/{name}.png"));
                assert_eq!(std::fs::read(&path).unwrap(), art, "{}", path.display());
            }
        }
        let text = std::fs::read_to_string(&options).unwrap();
        assert!(text.contains("resourcePacks:[\"vanilla\",\"file/other.zip\",\"file/mcl-skin\"]"), "{text}");
        assert!(text.contains("incompatibleResourcePacks:[]"), "{text}");
        assert!(text.starts_with("fov:0.0\r\n"), "other lines untouched");
        assert!(is_live(&dir), "written but not enabled");

        remove_pack(&dir).unwrap();
        assert!(!pack_dir(&dir).exists());
        let text = std::fs::read_to_string(&options).unwrap();
        assert!(text.contains("resourcePacks:[\"vanilla\",\"file/other.zip\"]"), "{text}");
        assert!(!is_live(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_options_file_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("mcl-skin-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_pack(&dir, &png(64, 64)).unwrap();
        let text = std::fs::read_to_string(dir.join("options.txt")).unwrap();
        assert_eq!(text, "resourcePacks:[\"vanilla\",\"file/mcl-skin\"]\n");
        assert!(is_live(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
