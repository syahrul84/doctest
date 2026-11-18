//! sha256 as lowercase hex.

use sha2::{Digest, Sha256};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_value() {
        assert_eq!(
            super::sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
