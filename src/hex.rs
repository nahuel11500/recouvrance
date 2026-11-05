//! Bytes as lowercase hexadecimal, for hashes in names, file names and tokens.

use std::fmt::Write as _;

/// `[0xab, 0x01]` → `"ab01"`.
pub fn lower(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_digits_a_byte() {
        assert_eq!(super::lower(&[0xab, 0x01, 0x00]), "ab0100");
        assert_eq!(super::lower(&[]), "");
    }
}
