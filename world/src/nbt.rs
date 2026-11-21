//! A small NBT reader.
//!
//! Only what a Minecraft save actually contains is supported: the twelve tags
//! that appear in `level.dat` and in chunk payloads. Nothing here is generic or
//! clever — it walks a byte slice and builds a tree.
//!
//! Two facts about modern saves drive the design:
//!
//! 1. A 26.3 chunk has **no `Level` wrapper**; `sections`, `Status` and the rest
//!    sit at the root compound. Code written from older documentation reads
//!    `Level` and silently finds nothing.
//! 2. Some compound keys are the empty string (`{"": "minecraft:deepslate"}` is
//!    a real palette entry in this world), so a key lookup must accept `""` and
//!    must not treat it as "missing".

use anyhow::{bail, Result};
use std::fmt;

/// A compound keeps its entries in order; they are small and lookups are rare
/// enough that a linear scan beats a hash map per section.
#[derive(Debug, Clone, Default)]
pub struct Compound(Vec<(String, Nbt)>);

impl From<Vec<(String, Nbt)>> for Compound {
    fn from(entries: Vec<(String, Nbt)>) -> Self {
        Compound(entries)
    }
}

impl Compound {
    pub fn get(&self, key: &str) -> Option<&Nbt> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[derive(Debug, Clone)]
pub enum Nbt {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(Vec<Nbt>),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Nbt {
    pub fn tag_name(&self) -> &'static str {
        match self {
            Nbt::Byte(_) => "byte",
            Nbt::Short(_) => "short",
            Nbt::Int(_) => "int",
            Nbt::Long(_) => "long",
            Nbt::Float(_) => "float",
            Nbt::Double(_) => "double",
            Nbt::ByteArray(_) => "byte[]",
            Nbt::String(_) => "string",
            Nbt::List(_) => "list",
            Nbt::Compound(_) => "compound",
            Nbt::IntArray(_) => "int[]",
            Nbt::LongArray(_) => "long[]",
        }
    }

    pub fn as_compound(&self) -> Result<&Compound> {
        match self {
            Nbt::Compound(c) => Ok(c),
            other => bail!("expected compound, found {}", other.tag_name()),
        }
    }

    pub fn as_list(&self) -> Result<&[Nbt]> {
        match self {
            Nbt::List(items) => Ok(items),
            other => bail!("expected list, found {}", other.tag_name()),
        }
    }

    pub fn as_str(&self) -> Result<&str> {
        match self {
            Nbt::String(s) => Ok(s),
            other => bail!("expected string, found {}", other.tag_name()),
        }
    }

    pub fn as_longs(&self) -> Result<&[i64]> {
        match self {
            Nbt::LongArray(v) => Ok(v),
            other => bail!("expected long[], found {}", other.tag_name()),
        }
    }

    /// Integer-ish tags widen to `i32`. `Y` in a section is a byte, `yPos` is an
    /// int; both are wanted as plain numbers.
    pub fn as_i32(&self) -> Result<i32> {
        match self {
            Nbt::Byte(v) => Ok(*v as i32),
            Nbt::Short(v) => Ok(*v as i32),
            Nbt::Int(v) => Ok(*v),
            Nbt::Long(v) => Ok(*v as i32),
            other => bail!("expected an integer, found {}", other.tag_name()),
        }
    }
}

impl fmt::Display for Nbt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Nbt::Byte(v) => write!(f, "{v}b"),
            Nbt::Short(v) => write!(f, "{v}s"),
            Nbt::Int(v) => write!(f, "{v}"),
            Nbt::Long(v) => write!(f, "{v}L"),
            Nbt::Float(v) => write!(f, "{v}f"),
            Nbt::Double(v) => write!(f, "{v}d"),
            Nbt::String(s) => write!(f, "\"{s}\""),
            Nbt::ByteArray(v) => write!(f, "[{} bytes]", v.len()),
            Nbt::IntArray(v) => write!(f, "[{} ints]", v.len()),
            Nbt::LongArray(v) => write!(f, "[{} longs]", v.len()),
            Nbt::List(v) => write!(f, "[{} items]", v.len()),
            Nbt::Compound(c) => write!(f, "{{{}}} keys", c.len()),
        }
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| anyhow::anyhow!("NBT offset overflow"))?;
        if end > self.buf.len() {
            bail!(
                "NBT truncated: wanted {n} bytes at {}, only {} left",
                self.pos,
                self.buf.len() - self.pos
            );
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }

    fn i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(self.i16()? as u16)
    }

    fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.i32()? as u32))
    }

    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.i64()? as u64))
    }

    /// NBT strings are length-prefixed with a *signed* 16-bit count in older
    /// writers and unsigned in practice; both are the same bytes for the lengths
    /// that occur here (all well under 32 KiB).
    fn string(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn value(&mut self, tag: u8) -> Result<Nbt> {
        Ok(match tag {
            1 => Nbt::Byte(self.i8()?),
            2 => Nbt::Short(self.i16()?),
            3 => Nbt::Int(self.i32()?),
            4 => Nbt::Long(self.i64()?),
            5 => Nbt::Float(self.f32()?),
            6 => Nbt::Double(self.f64()?),
            7 => {
                let n = self.i32()?;
                if n < 0 {
                    bail!("negative byte[] length {n}");
                }
                Nbt::ByteArray(self.take(n as usize)?.to_vec())
            }
            8 => Nbt::String(self.string()?),
            9 => {
                let elem = self.u8()?;
                let n = self.i32()?;
                if n < 0 {
                    bail!("negative list length {n}");
                }
                // A list of TAG_End is how an empty list is written; it has a
                // length of 0 and no elements either way.
                let mut items = Vec::with_capacity(n.min(4096) as usize);
                for _ in 0..n {
                    items.push(self.value(elem)?);
                }
                Nbt::List(items)
            }
            10 => Nbt::Compound(self.compound()?),
            11 => {
                let n = self.i32()?;
                if n < 0 {
                    bail!("negative int[] length {n}");
                }
                let mut v = Vec::with_capacity(n.min(65536) as usize);
                for _ in 0..n {
                    v.push(self.i32()?);
                }
                Nbt::IntArray(v)
            }
            12 => {
                let n = self.i32()?;
                if n < 0 {
                    bail!("negative long[] length {n}");
                }
                let mut v = Vec::with_capacity(n.min(65536) as usize);
                for _ in 0..n {
                    v.push(self.i64()?);
                }
                Nbt::LongArray(v)
            }
            other => bail!("unknown NBT tag {other} at offset {}", self.pos - 1),
        })
    }

    fn compound(&mut self) -> Result<Compound> {
        let mut entries = Vec::new();
        loop {
            let tag = self.u8()?;
            if tag == 0 {
                break;
            }
            let key = self.string()?;
            let value = self.value(tag)?;
            entries.push((key, value));
        }
        Ok(Compound(entries))
    }
}

/// Parse a whole NBT document, returning the payload of the root compound.
///
/// The root tag name is discarded: nothing in a save uses it.
pub fn parse(bytes: &[u8]) -> Result<Nbt> {
    let mut cursor = Cursor { buf: bytes, pos: 0 };
    let tag = cursor.u8()?;
    if tag == 0 {
        bail!("NBT document is empty (root tag 0)");
    }
    let _root_name = cursor.string()?;
    let value = cursor.value(tag)?;
    Ok(value)
}

/// Parse a gzip-compressed NBT document, which is what `level.dat` is.
pub fn parse_gzip(bytes: &[u8]) -> Result<Nbt> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| anyhow::anyhow!("level.dat is not valid gzip: {e}"))?;
    parse(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built compound: `{ "a": int 7, "b": "hi", "": string "empty-key" }`.
    fn sample() -> Vec<u8> {
        let mut v = vec![10u8, 0, 1, b'r']; // root compound, name "r"
        v.extend_from_slice(&[3, 0, 1, b'a']); // int a
        v.extend_from_slice(&7i32.to_be_bytes());
        v.extend_from_slice(&[8, 0, 1, b'b']); // string b
        v.extend_from_slice(&[0, 2, b'h', b'i']);
        v.extend_from_slice(&[8, 0, 0]); // string with empty key
        v.extend_from_slice(&[0, 9]);
        v.extend_from_slice(b"empty-key");
        v.push(0); // end of root
        v
    }

    #[test]
    fn reads_scalars_strings_and_the_empty_key() {
        let doc = parse(&sample()).expect("parses");
        let c = doc.as_compound().expect("compound");
        assert_eq!(c.get("a").unwrap().as_i32().unwrap(), 7);
        assert_eq!(c.get("b").unwrap().as_str().unwrap(), "hi");
        assert_eq!(c.get("").unwrap().as_str().unwrap(), "empty-key");
        assert!(c.get("missing").is_none());
    }

    #[test]
    fn reports_truncation_instead_of_panicking() {
        let mut bytes = sample();
        bytes.truncate(bytes.len() - 4);
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn reads_a_long_array() {
        let mut v = vec![10u8, 0, 0];
        v.extend_from_slice(&[12, 0, 2, b'l', b'a']); // long[] "la"
        v.extend_from_slice(&2i32.to_be_bytes());
        v.extend_from_slice(&(-1i64).to_be_bytes());
        v.extend_from_slice(&5i64.to_be_bytes());
        v.push(0);
        let doc = parse(&v).expect("parses");
        let longs = doc
            .as_compound()
            .unwrap()
            .get("la")
            .unwrap()
            .as_longs()
            .unwrap();
        assert_eq!(longs, &[-1, 5]);
    }

    #[test]
    fn empty_list_of_tag_end_is_allowed() {
        let mut v = vec![10u8, 0, 0];
        v.extend_from_slice(&[9, 0, 1, b'l']); // list "l"
        v.push(0); // element type TAG_End
        v.extend_from_slice(&0i32.to_be_bytes());
        v.push(0);
        let doc = parse(&v).expect("parses");
        let list = doc.as_compound().unwrap().get("l").unwrap().as_list().unwrap();
        assert!(list.is_empty());
    }
}
