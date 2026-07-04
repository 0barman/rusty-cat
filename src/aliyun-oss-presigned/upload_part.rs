use super::constants::PART_NUMBER_PROVIDER_ID_RADIX;
use crate::presigned::PresignedUploadPart;

/// Creates one Aliyun `UploadPart` entry.
pub fn upload_part(
    part_number: u64,
    offset: u64,
    size: u64,
    url: impl Into<String>,
) -> PresignedUploadPart {
    PresignedUploadPart::put(part_number, offset, size, url)
        .with_provider_part_id(provider_part_id(part_number))
}

/// Creates one Aliyun `UploadPart` entry with URL expiration metadata.
pub fn upload_part_with_expiry(
    part_number: u64,
    offset: u64,
    size: u64,
    url: impl Into<String>,
    expires_at_unix_secs: u64,
) -> PresignedUploadPart {
    upload_part(part_number, offset, size, url).with_expires_at_unix_secs(expires_at_unix_secs)
}

fn provider_part_id(part_number: u64) -> String {
    match PART_NUMBER_PROVIDER_ID_RADIX {
        10 => part_number.to_string(),
        radix if (2..=36).contains(&radix) => format_radix(part_number, radix),
        // A radix outside 2..=36 has no valid digit alphabet and would otherwise
        // index `DIGITS` out of bounds (a panic in both debug and release). Fall
        // back to base-10 so a misconfigured constant degrades instead of crashing.
        _ => part_number.to_string(),
    }
}

/// Formats `value` in `radix`. The caller guarantees `2 <= radix <= 36`, which
/// keeps the `DIGITS` index in bounds.
fn format_radix(mut value: u64, radix: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let radix = u64::from(radix);
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % radix) as usize] as char);
        value /= radix;
    }
    out.iter().rev().collect()
}
