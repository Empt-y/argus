//! The one ZIP entry a feed ships, without a ZIP crate.
//!
//! GDELT publishes each fifteen-minute batch as a ZIP archive holding
//! exactly one CSV, and that is the whole of what this reads: the local
//! file header at the front of the archive, followed by the entry's bytes,
//! stored or deflated. The general format has a central directory at the
//! end that is the authority on sizes and offsets, and an entry may defer
//! its sizes to a descriptor after the data; a writer that does either is
//! read through the central directory instead. Encryption, spanning and
//! ZIP64 are refused by name — none of the feeds here use them, and a
//! dependency that handles everything is more code than this.

use std::io::Read;

const LOCAL_HEADER: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const CENTRAL_HEADER: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const END_OF_CENTRAL: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const STORED: u16 = 0;
const DEFLATED: u16 = 8;
const FLAG_ENCRYPTED: u16 = 1;
const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// The first entry's name and decompressed bytes.
pub fn first_entry(bytes: &[u8]) -> Result<(String, Vec<u8>), String> {
    if bytes.get(..4) != Some(&LOCAL_HEADER) {
        return Err("not a ZIP archive: no local file header at the front".into());
    }
    let flags = u16_at(bytes, 6).ok_or("truncated header")?;
    let method = u16_at(bytes, 8).ok_or("truncated header")?;
    let mut compressed = u32_at(bytes, 18).ok_or("truncated header")? as usize;
    let mut uncompressed = u32_at(bytes, 22).ok_or("truncated header")? as usize;
    let name_len = u16_at(bytes, 26).ok_or("truncated header")? as usize;
    let extra_len = u16_at(bytes, 28).ok_or("truncated header")? as usize;
    if flags & FLAG_ENCRYPTED != 0 {
        return Err("encrypted ZIP entries are not read".into());
    }
    let name = String::from_utf8_lossy(bytes.get(30..30 + name_len).ok_or("truncated name")?).into_owned();
    let data_start = 30 + name_len + extra_len;

    // A writer that streamed the entry put zeros here and the truth in a
    // descriptor after the data; the central directory has it too and is
    // easier to find.
    if flags & FLAG_DATA_DESCRIPTOR != 0 || (compressed == 0 && uncompressed == 0 && method != STORED) {
        let (c, u) = sizes_from_central_directory(bytes)?;
        compressed = c;
        uncompressed = u;
    }
    if compressed == u32::MAX as usize || uncompressed == u32::MAX as usize {
        return Err("ZIP64 entries are not read".into());
    }
    let data = bytes.get(data_start..data_start + compressed).ok_or("entry data runs past the end of the archive")?;
    let out = match method {
        STORED => data.to_vec(),
        DEFLATED => {
            let mut out = Vec::with_capacity(uncompressed);
            flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).map_err(|e| format!("deflate: {e}"))?;
            out
        }
        other => return Err(format!("ZIP compression method {other} is not read")),
    };
    if uncompressed != 0 && out.len() != uncompressed {
        return Err(format!("entry decompressed to {} bytes, header says {uncompressed}", out.len()));
    }
    Ok((name, out))
}

/// The first central-directory record's sizes, found from the end record.
fn sizes_from_central_directory(bytes: &[u8]) -> Result<(usize, usize), String> {
    // The end record is the last 22 bytes plus a comment of up to 64 KiB;
    // scan back for its signature.
    let floor = bytes.len().saturating_sub(22 + 65_536);
    let end = (floor..bytes.len().saturating_sub(21)).rev().find(|&i| bytes.get(i..i + 4) == Some(&END_OF_CENTRAL)).ok_or("no end-of-central-directory record")?;
    let central = u32_at(bytes, end + 16).ok_or("truncated end record")? as usize;
    if bytes.get(central..central + 4) != Some(&CENTRAL_HEADER) {
        return Err("central directory is not where the end record says".into());
    }
    let compressed = u32_at(bytes, central + 20).ok_or("truncated central record")? as usize;
    let uncompressed = u32_at(bytes, central + 24).ok_or("truncated central record")? as usize;
    Ok((compressed, uncompressed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal single-entry archive, the way a normal writer lays it out.
    fn archive(name: &str, content: &[u8], deflate: bool, descriptor: bool) -> Vec<u8> {
        let data = if deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(content).unwrap();
            e.finish().unwrap()
        } else {
            content.to_vec()
        };
        let method: u16 = if deflate { 8 } else { 0 };
        let flags: u16 = if descriptor { 1 << 3 } else { 0 };
        let (hc, hu) = if descriptor { (0u32, 0u32) } else { (data.len() as u32, content.len() as u32) };
        let mut z = Vec::new();
        z.extend_from_slice(&LOCAL_HEADER);
        z.extend_from_slice(&20u16.to_le_bytes());
        z.extend_from_slice(&flags.to_le_bytes());
        z.extend_from_slice(&method.to_le_bytes());
        z.extend_from_slice(&[0; 4]); // time, date
        z.extend_from_slice(&0u32.to_le_bytes()); // crc, not checked
        z.extend_from_slice(&hc.to_le_bytes());
        z.extend_from_slice(&hu.to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(name.as_bytes());
        z.extend_from_slice(&data);
        if descriptor {
            z.extend_from_slice(&[0x50, 0x4b, 0x07, 0x08]);
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&(data.len() as u32).to_le_bytes());
            z.extend_from_slice(&(content.len() as u32).to_le_bytes());
        }
        let central_at = z.len();
        z.extend_from_slice(&CENTRAL_HEADER);
        z.extend_from_slice(&[0; 4]); // versions
        z.extend_from_slice(&flags.to_le_bytes());
        z.extend_from_slice(&method.to_le_bytes());
        z.extend_from_slice(&[0; 4]); // time, date
        z.extend_from_slice(&0u32.to_le_bytes()); // crc
        z.extend_from_slice(&(data.len() as u32).to_le_bytes());
        z.extend_from_slice(&(content.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&[0; 2 + 2 + 2 + 2 + 4]); // extra, comment, disk, attrs
        z.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        z.extend_from_slice(name.as_bytes());
        let central_len = z.len() - central_at;
        z.extend_from_slice(&END_OF_CENTRAL);
        z.extend_from_slice(&[0; 4]); // disks
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&(central_len as u32).to_le_bytes());
        z.extend_from_slice(&(central_at as u32).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z
    }

    #[test]
    fn stored_and_deflated_entries_are_read_by_the_local_header() {
        let content = b"1323535512\t20250917\t202509\n1323535513\t20260917\t202609\n".repeat(50);
        let (name, out) = first_entry(&archive("20260917150000.export.CSV", &content, false, false)).unwrap();
        assert_eq!(name, "20260917150000.export.CSV");
        assert_eq!(out, content);
        let (_, out) = first_entry(&archive("x.csv", &content, true, false)).unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn a_streamed_entry_with_a_data_descriptor_is_read_through_the_central_directory() {
        let content = b"hello, descriptor".repeat(20);
        let (_, out) = first_entry(&archive("x.csv", &content, true, true)).unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn what_is_not_a_zip_or_not_supported_is_refused_by_name() {
        assert!(first_entry(b"<html>404").unwrap_err().contains("not a ZIP"));
        let mut z = archive("x", b"abc", false, false);
        z[8] = 12; // bzip2
        assert!(first_entry(&z).unwrap_err().contains("method 12"));
        let mut z = archive("x", b"abc", false, false);
        z[6] |= 1; // encrypted
        assert!(first_entry(&z).unwrap_err().contains("encrypted"));
    }
}
