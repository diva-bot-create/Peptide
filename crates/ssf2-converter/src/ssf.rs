use anyhow::{anyhow, Result};
use flate2::read::ZlibDecoder;
use std::io::Read;

/// Decompress SSF to raw SWF bytes.
/// Handles four cases:
///   1. Already a raw SWF (FWS/CWS/ZWS magic) — pass through as-is
///   2. SSF2 `DAT*.ssf` container: a raw zlib stream wrapping a small index header
///      followed by a single embedded SWF — unwrap to that inner SWF
///   3. SSF-wrapped: 4-byte SWF length + 4-byte garbage header size + zlib payload
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 3 {
        return Err(anyhow!("File too small"));
    }

    // Check if it's already a raw SWF
    match [data[0], data[1], data[2]] {
        [b'F', b'W', b'S'] | [b'C', b'W', b'S'] | [b'Z', b'W', b'S'] => {
            log::debug!("File is already a raw SWF, skipping SSF decompression");
            return Ok(data.to_vec());
        }
        _ => {}
    }

    // SSF2 ships its characters/stages/items as `DAT<n>.ssf` archives. Each one is
    // a *raw* zlib stream (starting at offset 0, no SSF preamble) that inflates to:
    //   u32 BE  inner SWF length
    //   u32 BE  index entry count N
    //   N × u32 BE  exported symbol ids carried by this archive
    //   <inner SWF>  (exactly `inner SWF length` bytes)
    // The embedded SWF is the same pre-decompiled character SWF the rest of the
    // pipeline already understands, so we just hand it back. Detection is by trial:
    // we only treat the file as a DAT archive if the inflate succeeds *and* the
    // index header points at a valid SWF, which keeps the SSF-wrapped format below
    // from ever being misread.
    if let Some(inner_swf) = try_extract_dat(data) {
        log::info!(
            "SSF2 DAT archive detected — extracted embedded SWF ({} bytes)",
            inner_swf.len()
        );
        return Ok(inner_swf);
    }

    if data.len() < 8 {
        return Err(anyhow!("SSF too small (< 8 bytes)"));
    }

    let swf_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let header_size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;

    log::debug!("SSF: expected SWF length = {}, garbage header size = {}", swf_len, header_size);

    // Guard against integer overflow on 32-bit targets where usize == u32:
    // an attacker-controlled header_size near usize::MAX could wrap with
    // `8 + header_size` and land back inside the buffer.
    let compressed_start = match 8usize.checked_add(header_size) {
        Some(s) => s,
        None => return Err(anyhow!(
            "SSF header_size {} overflows when added to fixed 8-byte preamble",
            header_size
        )),
    };
    if compressed_start > data.len() {
        return Err(anyhow!("SSF header_size {} extends past end of file", header_size));
    }

    let compressed_data = &data[compressed_start..];
    log::debug!("Compressed payload: {} bytes starting at offset {}", compressed_data.len(), compressed_start);

    // Try zlib decompression first
    let mut decoder = ZlibDecoder::new(compressed_data);
    let mut swf = Vec::with_capacity(swf_len.min(64 * 1024 * 1024));
    match decoder.read_to_end(&mut swf) {
        Ok(_) => {
            log::debug!("Zlib decompressed to {} bytes (expected {})", swf.len(), swf_len);
        }
        Err(e) => {
            // Already uncompressed? Check if it starts with FWS magic
            if compressed_data.starts_with(b"FWS") {
                log::debug!("Payload already uncompressed (FWS magic)");
                swf = compressed_data.to_vec();
            } else {
                return Err(anyhow!("Failed to decompress SSF payload: {}", e));
            }
        }
    }

    // Validate SWF magic bytes
    if swf.len() < 3 {
        return Err(anyhow!("Decompressed SWF too small ({} bytes)", swf.len()));
    }
    match [swf[0], swf[1], swf[2]] {
        [b'F', b'W', b'S'] => log::debug!("SWF magic: FWS (uncompressed)"),
        [b'C', b'W', b'S'] => log::debug!("SWF magic: CWS (zlib-compressed SWF)"),
        [b'Z', b'W', b'S'] => log::debug!("SWF magic: ZWS (LZMA-compressed SWF)"),
        other => return Err(anyhow!("Invalid SWF magic bytes: {:?}", other)),
    }

    Ok(swf)
}

/// Upper bound on the DAT index-entry count. Real archives top out around a
/// dozen entries; this only guards a misread `count` from forcing a huge
/// allocation / out-of-bounds slice before we can reject a non-DAT file.
const DAT_MAX_INDEX_ENTRIES: u32 = 4096;

/// Try to read `data` as an SSF2 `DAT*.ssf` archive and return the embedded SWF.
///
/// Returns `None` (never an error) for anything that isn't a DAT archive, so the
/// caller can cleanly fall through to the other SSF layouts. The whole detection
/// is validation-gated: we require a clean zlib inflate, a self-consistent index
/// header, and a real SWF magic at the computed offset before committing — so the
/// SSF-wrapped format (whose leading bytes are a little-endian length, not a zlib
/// stream) is never mistaken for a DAT archive.
fn try_extract_dat(data: &[u8]) -> Option<Vec<u8>> {
    // zlib stream header: CM=8 (deflate) and the 16-bit big-endian (CMF,FLG) is a
    // multiple of 31. This is a cheap pre-filter; the inflate below is the real test.
    if data.len() < 2 || data[0] & 0x0f != 0x08 {
        return None;
    }
    if !((data[0] as u16) << 8 | data[1] as u16).is_multiple_of(31) {
        return None;
    }

    let mut decoder = ZlibDecoder::new(data);
    let mut buf = Vec::new();
    if decoder.read_to_end(&mut buf).is_err() {
        return None;
    }

    // Need at least the two leading u32s (inner length + entry count).
    if buf.len() < 8 {
        return None;
    }
    let inner_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let count = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if count > DAT_MAX_INDEX_ENTRIES {
        return None;
    }

    // Header = the two u32s + `count` u32 index entries. The remainder must be
    // exactly the embedded SWF.
    let header_size = 8usize.checked_add((count as usize).checked_mul(4)?)?;
    let end = header_size.checked_add(inner_len)?;
    if end != buf.len() {
        return None;
    }

    let inner = &buf[header_size..end];
    if inner.len() < 3 {
        return None;
    }
    match [inner[0], inner[1], inner[2]] {
        [b'F', b'W', b'S'] | [b'C', b'W', b'S'] | [b'Z', b'W', b'S'] => Some(inner.to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// Build a fake DAT archive: zlib( [be inner_len][be count][count entries][swf] ).
    fn make_dat(inner_swf: &[u8], entries: &[u32]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(inner_swf.len() as u32).to_be_bytes());
        payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for e in entries {
            payload.extend_from_slice(&e.to_be_bytes());
        }
        payload.extend_from_slice(inner_swf);

        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&payload).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn extracts_inner_swf_from_dat() {
        // A minimal but valid SWF header: FWS, version, file length, then padding.
        let mut swf = b"FWS\x0f".to_vec();
        swf.extend_from_slice(&[0u8; 64]);
        let dat = make_dat(&swf, &[0x194f, 0xb4e, 0xebc, 0x6a9]);

        let out = decompress(&dat).expect("DAT should decompress");
        assert_eq!(out, swf, "extracted SWF must be byte-identical to the embedded one");
    }

    #[test]
    fn dat_with_no_index_entries() {
        let mut swf = b"FWS\x06".to_vec();
        swf.extend_from_slice(&[0u8; 16]);
        let dat = make_dat(&swf, &[]);
        assert_eq!(decompress(&dat).unwrap(), swf);
    }

    #[test]
    fn raw_swf_passes_through_untouched() {
        let mut swf = b"FWS\x0f".to_vec();
        swf.extend_from_slice(&[1u8; 32]);
        assert_eq!(decompress(&swf).unwrap(), swf);
    }

    #[test]
    fn zlib_stream_that_isnt_a_dat_is_rejected() {
        // Valid zlib, but the inflated bytes are not a DAT (no SWF magic at offset).
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"not a dat archive, just some compressed text padding...")
            .unwrap();
        let blob = enc.finish().unwrap();
        assert!(try_extract_dat(&blob).is_none());
    }

    #[test]
    fn truncated_dat_is_rejected() {
        let mut swf = b"FWS\x0f".to_vec();
        swf.extend_from_slice(&[0u8; 64]);
        let dat = make_dat(&swf, &[1, 2]);
        // Lopping bytes off the compressed stream must not panic, just decline.
        assert!(try_extract_dat(&dat[..dat.len() / 2]).is_none());
    }
}
