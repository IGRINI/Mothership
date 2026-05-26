use std::time::{SystemTime, UNIX_EPOCH};

use crate::{MothershipError, Result};

pub(crate) fn generate_id(prefix: &str) -> Result<String> {
    let mut random = [0_u8; 16];
    getrandom::getrandom(&mut random).map_err(|error| {
        MothershipError::InvalidRequest(format!("failed to generate secure id: {error}"))
    })?;

    Ok(format!(
        "{prefix}_{}_{}",
        current_timestamp(),
        hex_encode(&random)
    ))
}

fn current_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    seconds.to_string()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
