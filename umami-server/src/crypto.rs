//! Ports of upstream's `src/lib/crypto.ts`.

use sha2::{Digest, Sha512};

/// Upstream's `hash(...args)`: SHA-512 (lowercase hex) of the arguments concatenated.
pub fn hash(parts: &[&str]) -> String {
    let mut hasher = Sha512::new();

    for part in parts {
        hasher.update(part.as_bytes());
    }

    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_sha512_of_concatenation() {
        let abc = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                   2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";

        assert_eq!(hash(&["abc"]), abc);
        assert_eq!(hash(&["a", "bc"]), abc);
    }
}
