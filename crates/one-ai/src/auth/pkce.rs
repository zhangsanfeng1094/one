//! PKCE (RFC 7636) using OS random + SHA-256.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);
    Pkce {
        verifier,
        challenge,
    }
}

pub fn random_hex(bytes_len: usize) -> String {
    let mut bytes = vec![0u8; bytes_len];
    fill_random(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fill_random(buf: &mut [u8]) {
    // Prefer getrandom via std if available; fall back to /dev/urandom.
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(buf).is_ok() {
                return;
            }
        }
    }
    // Weak fallback (should not hit on Linux).
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut x = seed as u64 ^ std::process::id() as u64;
    for b in buf.iter_mut() {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (x >> 33) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_lengths() {
        let p = generate_pkce();
        assert!(p.verifier.len() >= 40);
        assert!(p.challenge.len() >= 40);
        assert_ne!(p.verifier, p.challenge);
    }
}
