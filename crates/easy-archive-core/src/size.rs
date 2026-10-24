use crate::{ErrorCode, Result, VolumeError};

/// Exact decimal parsing: no floating-point rounding, no fractional bytes.
pub fn parse_size(input: &str) -> Result<u64> {
    let invalid = || VolumeError::new(ErrorCode::InvalidSize);
    let input = input.trim();
    if input.is_empty() || input.len() > 64 {
        return Err(invalid());
    }
    let unit_start = input
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(input.len());
    let number = input[..unit_start].trim_end();
    let multiplier: u128 = match &input[unit_start..] {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        _ => return Err(invalid()),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
        || fraction.len() > 9
        || (number.contains('.') && fraction.is_empty())
    {
        return Err(invalid());
    }
    let denominator = 10_u128.pow(fraction.len() as u32);
    let whole: u128 = whole.parse().map_err(|_| invalid())?;
    let fraction: u128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().map_err(|_| invalid())?
    };
    let scaled = whole
        .checked_mul(denominator)
        .and_then(|v| v.checked_add(fraction))
        .and_then(|v| v.checked_mul(multiplier))
        .ok_or_else(invalid)?;
    if scaled % denominator != 0 {
        return Err(invalid());
    }
    let bytes = u64::try_from(scaled / denominator).map_err(|_| invalid())?;
    if bytes == 0 {
        Err(invalid())
    } else {
        Ok(bytes)
    }
}
