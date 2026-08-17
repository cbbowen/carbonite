//! Unsigned LEB128 varints, used for lengths and tags.

use crate::error::{Error, Result};

/// Appends `value` to `out` as an unsigned LEB128 varint (1–10 bytes).
pub(crate) fn write(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads a varint from the front of `buf`, returning the value and bytes consumed.
pub(crate) fn read(buf: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        // The 10th byte may only contribute the final bit of a u64.
        if shift > 63 || (shift == 63 && byte & 0x7f > 1) {
            return Err(Error::InvalidVarint);
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
    }
    Err(Error::UnexpectedEof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_representative_values() {
        for value in [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            write(&mut buf, value);
            let (back, used) = read(&buf).unwrap();
            assert_eq!(back, value);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn single_byte_encoding_for_small_values() {
        let mut buf = Vec::new();
        write(&mut buf, 5);
        assert_eq!(buf, [5]);
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(matches!(read(&[0x80]), Err(Error::UnexpectedEof)));
        assert!(matches!(read(&[]), Err(Error::UnexpectedEof)));
    }

    #[test]
    fn rejects_overlong_input() {
        // 11 continuation bytes can never encode a u64.
        let buf = [0x80u8; 11];
        assert!(matches!(read(&buf), Err(Error::InvalidVarint)));
        // 10th byte with more than the final bit set overflows.
        let mut buf = vec![0xffu8; 9];
        buf.push(0x02);
        assert!(matches!(read(&buf), Err(Error::InvalidVarint)));
    }
}
