//! GPH2 on-disk layout: block codec, trailer, 16-byte end record.

use crate::backbone::ROOT;

pub const GPH2_MAGIC: &[u8; 4] = b"GPH2";
pub const GPH2_VERSION: u16 = 1;
pub const BLOCK_DOCS: usize = 128;
pub const END_RECORD_LEN: usize = 16;
pub const BLOCK_HDR_LEN: usize = 12;
pub const ATTR_NONE: u16 = 0;

/// Parsed trailer (not including the 16-byte end record).
#[derive(Debug, Clone)]
pub struct Gph2Trailer {
    pub uuid: String,
    pub max_doc: u32,
    pub n_blocks: u32,
    pub label_w: u8,
    pub colocated: Vec<String>,
    pub attr_w: Vec<u8>,
    pub block_off: Vec<u32>,
    pub rel_dict: Vec<String>,
    pub attr_dicts: Vec<Vec<String>>,
}

impl Gph2Trailer {
    /// Dictionary-less trailer (default for scratch buffers and tests).
    pub fn empty() -> Self {
        Self {
            uuid: String::new(),
            max_doc: 0,
            n_blocks: 0,
            label_w: 1,
            colocated: Vec::new(),
            attr_w: Vec::new(),
            block_off: Vec::new(),
            rel_dict: Vec::new(),
            attr_dicts: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut uuid = [0u8; 32];
        let bytes = self.uuid.as_bytes();
        let n = bytes.len().min(32);
        uuid[..n].copy_from_slice(&bytes[..n]);
        out.extend_from_slice(&uuid);
        out.extend_from_slice(&self.max_doc.to_le_bytes());
        out.extend_from_slice(&self.n_blocks.to_le_bytes());
        out.push(self.label_w);
        out.push(self.colocated.len() as u8);
        out.extend_from_slice(&[0u8, 0]);
        out.extend_from_slice(&self.attr_w);
        for name in &self.colocated {
            write_str16(&mut out, name);
        }
        for off in &self.block_off {
            out.extend_from_slice(&off.to_le_bytes());
        }
        write_dict(&mut out, &self.rel_dict);
        for d in &self.attr_dicts {
            write_dict(&mut out, d);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 32 + 4 + 4 + 4 {
            return Err("GPH2 trailer too small".into());
        }
        let uuid = {
            let raw = &bytes[0..32];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(32);
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let mut i = 32;
        let max_doc = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        i += 4;
        let n_blocks = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        i += 4;
        let label_w = bytes[i];
        i += 1;
        let n_col = bytes[i] as usize;
        i += 1;
        i += 2; // pad
        if i + n_col > bytes.len() {
            return Err("GPH2 trailer attr_w truncated".into());
        }
        let attr_w = bytes[i..i + n_col].to_vec();
        i += n_col;
        let mut colocated = Vec::with_capacity(n_col);
        for _ in 0..n_col {
            let (s, n) = read_str16(&bytes[i..])?;
            colocated.push(s);
            i += n;
        }
        let mut block_off = Vec::with_capacity(n_blocks as usize + 1);
        for _ in 0..=n_blocks {
            if i + 4 > bytes.len() {
                return Err("GPH2 trailer block_off truncated".into());
            }
            block_off.push(u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()));
            i += 4;
        }
        let (rel_dict, n) = read_dict(&bytes[i..])?;
        i += n;
        let mut attr_dicts = Vec::with_capacity(n_col);
        for _ in 0..n_col {
            let (d, n) = read_dict(&bytes[i..])?;
            attr_dicts.push(d);
            i += n;
        }
        let _ = i;
        Ok(Self {
            uuid,
            max_doc,
            n_blocks,
            label_w,
            colocated,
            attr_w,
            block_off,
            rel_dict,
            attr_dicts,
        })
    }
}

/// 16-byte foot: trailer_len | version | flags | "GPH2" | reserved.
#[derive(Debug, Clone, Copy)]
pub struct EndRecord {
    pub trailer_len: u32,
    pub version: u16,
    pub flags: u16,
}

impl EndRecord {
    pub fn encode(self) -> [u8; END_RECORD_LEN] {
        let mut buf = [0u8; END_RECORD_LEN];
        buf[0..4].copy_from_slice(&self.trailer_len.to_le_bytes());
        buf[4..6].copy_from_slice(&self.version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..12].copy_from_slice(GPH2_MAGIC);
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < END_RECORD_LEN {
            return Err("GPH2 end record truncated".into());
        }
        let trailer_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        let flags = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
        if &bytes[8..12] != GPH2_MAGIC {
            return Err("GPH2 magic mismatch".into());
        }
        Ok(Self {
            trailer_len,
            version,
            flags,
        })
    }

    /// Byte range of the trailer within a file of `file_len` bytes.
    pub fn trailer_range(file_len: u64, rec: Self) -> std::ops::Range<u64> {
        let end = file_len.saturating_sub(END_RECORD_LEN as u64);
        let start = end.saturating_sub(rec.trailer_len as u64);
        start..end
    }
}

#[derive(Debug, Clone)]
pub struct BlockHeader {
    pub n_docs: u16,
    pub head_w: u8,
    pub flags: u8,
    pub n_tok: u32,
    pub n_ov: u32,
}

impl BlockHeader {
    pub fn encode(&self) -> [u8; BLOCK_HDR_LEN] {
        let mut buf = [0u8; BLOCK_HDR_LEN];
        buf[0..2].copy_from_slice(&self.n_docs.to_le_bytes());
        buf[2] = self.head_w;
        buf[3] = self.flags;
        buf[4..8].copy_from_slice(&self.n_tok.to_le_bytes());
        buf[8..12].copy_from_slice(&self.n_ov.to_le_bytes());
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < BLOCK_HDR_LEN {
            return Err("GPH2 block header truncated".into());
        }
        Ok(Self {
            n_docs: u16::from_le_bytes(bytes[0..2].try_into().unwrap()),
            head_w: bytes[2],
            flags: bytes[3],
            n_tok: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            n_ov: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        })
    }
}

pub fn root_value(head_w: u8) -> u32 {
    if head_w == 1 { 0xFF } else { 0xFFFF }
}

pub fn encode_head(head: u32, head_w: u8) -> u32 {
    if head == ROOT {
        root_value(head_w)
    } else {
        head
    }
}

pub fn decode_head(raw: u32, head_w: u8) -> u32 {
    if raw == root_value(head_w) { ROOT } else { raw }
}

pub fn write_width(out: &mut Vec<u8>, value: u32, width: u8) {
    match width {
        1 => out.push(value as u8),
        2 => out.extend_from_slice(&(value as u16).to_le_bytes()),
        _ => out.extend_from_slice(&value.to_le_bytes()),
    }
}

pub fn read_width(bytes: &[u8], off: &mut usize, width: u8) -> Result<u32, String> {
    match width {
        1 => {
            if *off >= bytes.len() {
                return Err("width-1 truncated".into());
            }
            let v = bytes[*off] as u32;
            *off += 1;
            Ok(v)
        }
        2 => {
            if *off + 2 > bytes.len() {
                return Err("width-2 truncated".into());
            }
            let v = u16::from_le_bytes(bytes[*off..*off + 2].try_into().unwrap()) as u32;
            *off += 2;
            Ok(v)
        }
        _ => Err(format!("unsupported width {width}")),
    }
}

fn write_str16(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    out.extend_from_slice(&(b.len() as u16).to_le_bytes());
    out.extend_from_slice(b);
}

fn read_str16(bytes: &[u8]) -> Result<(String, usize), String> {
    if bytes.len() < 2 {
        return Err("str16 truncated".into());
    }
    let len = u16::from_le_bytes(bytes[0..2].try_into().unwrap()) as usize;
    if bytes.len() < 2 + len {
        return Err("str16 body truncated".into());
    }
    let s = String::from_utf8(bytes[2..2 + len].to_vec()).map_err(|e| e.to_string())?;
    Ok((s, 2 + len))
}

fn write_dict(out: &mut Vec<u8>, dict: &[String]) {
    out.extend_from_slice(&(dict.len() as u32).to_le_bytes());
    for s in dict {
        write_str16(out, s);
    }
}

fn read_dict(bytes: &[u8]) -> Result<(Vec<String>, usize), String> {
    if bytes.len() < 4 {
        return Err("dict truncated".into());
    }
    let n = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let mut i = 4;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (s, k) = read_str16(&bytes[i..])?;
        out.push(s);
        i += k;
    }
    Ok((out, i))
}

/// Locate trailer from a complete GPH2 byte buffer.
pub fn trailer_from_file_bytes(bytes: &[u8]) -> Result<(Gph2Trailer, EndRecord), String> {
    if bytes.len() < END_RECORD_LEN {
        return Err("file shorter than end record".into());
    }
    let rec = EndRecord::decode(&bytes[bytes.len() - END_RECORD_LEN..])?;
    if rec.version != GPH2_VERSION {
        return Err(format!("unsupported GPH2 version {}", rec.version));
    }
    let range = EndRecord::trailer_range(bytes.len() as u64, rec);
    let trailer = Gph2Trailer::decode(&bytes[range.start as usize..range.end as usize])?;
    Ok((trailer, rec))
}
