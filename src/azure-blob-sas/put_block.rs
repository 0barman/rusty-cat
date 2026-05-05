use reqwest::Url;

use super::block_id::block_id_by_index;
use super::constants::{QUERY_BLOCK_ID, QUERY_COMP, QUERY_VALUE_BLOCK};
use crate::presigned::PresignedUploadPart;
use crate::{InnerErrorCode, MeowError};

/// Creates one Azure SAS `Put Block` entry.
pub fn put_block(
    index: usize,
    offset: u64,
    size: u64,
    url: impl Into<String>,
) -> PresignedUploadPart {
    PresignedUploadPart::put(index as u64, offset, size, url)
        .with_provider_part_id(block_id_by_index(index))
}

/// Creates one Azure SAS `Put Block` entry from a blob SAS/base URL.
pub fn put_block_from_blob_url(
    blob_sas_url: impl AsRef<str>,
    index: usize,
    offset: u64,
    size: u64,
) -> Result<PresignedUploadPart, MeowError> {
    let block_id = block_id_by_index(index);
    let mut url = Url::parse(blob_sas_url.as_ref()).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid azure blob sas url: {e}"),
        )
    })?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair(QUERY_COMP, QUERY_VALUE_BLOCK);
        pairs.append_pair(QUERY_BLOCK_ID, block_id.as_str());
    }
    Ok(
        PresignedUploadPart::put(index as u64, offset, size, url.to_string())
            .with_provider_part_id(block_id),
    )
}
