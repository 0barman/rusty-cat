use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;

use super::constants::BLOCK_ID_WIDTH;

/// Builds a stable Azure block id from a zero-based block index.
pub fn block_id_by_index(index: usize) -> String {
    BASE64_STANDARD.encode(format!("{index:0BLOCK_ID_WIDTH$}"))
}
