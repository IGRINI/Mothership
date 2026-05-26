use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};

use crate::{MothershipError, Result};

const PKCE_VERIFIER_CHARS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce_pair() -> Result<PkcePair> {
    let verifier = generate_pkce_verifier(43)?;
    let challenge = pkce_challenge_s256(&verifier);
    Ok(PkcePair {
        verifier,
        challenge,
    })
}

pub fn generate_state() -> Result<String> {
    let mut bytes = [0_u8; 32];
    fill_random(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn pkce_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn decode_base64_url_json(value: &str) -> Option<serde_json::Value> {
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn generate_pkce_verifier(length: usize) -> Result<String> {
    let mut bytes = vec![0_u8; length];
    fill_random(&mut bytes)?;

    Ok(bytes
        .into_iter()
        .map(|byte| PKCE_VERIFIER_CHARS[byte as usize % PKCE_VERIFIER_CHARS.len()] as char)
        .collect())
}

fn fill_random(bytes: &mut [u8]) -> Result<()> {
    getrandom::getrandom(bytes).map_err(|error| {
        MothershipError::InvalidRequest(format!("failed to generate secure random bytes: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        let challenge = pkce_challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_state_is_url_safe() {
        let state = generate_state().expect("state");
        assert!(!state.contains('+'));
        assert!(!state.contains('/'));
        assert!(!state.contains('='));
    }
}
