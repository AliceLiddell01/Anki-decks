//! SHA-256 и стабильное lowercase-hex представление hash-значений.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// Вычисляет SHA-256 и возвращает его каноническое lowercase-hex представление.
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    encode_lower_hex(Sha256::digest(bytes.as_ref()))
}

/// Кодирует байты в lowercase hex независимо от formatter API digest-типа.
pub(crate) fn encode_lower_hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("запись в String не может завершиться ошибкой");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_hex_preserves_leading_zeroes() {
        assert_eq!(encode_lower_hex([0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
