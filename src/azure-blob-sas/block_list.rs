use reqwest::Method;
use reqwest::Url;

use super::constants::{
    CONTENT_TYPE_APPLICATION_XML, HEADER_CONTENT_TYPE, QUERY_COMP, QUERY_VALUE_BLOCK_LIST,
    XML_BLOCK_LIST_CLOSE, XML_DECLARATION_AND_BLOCK_LIST_OPEN, XML_LATEST_CLOSE, XML_LATEST_OPEN,
};
use crate::presigned::{headers_from_pairs, CompletionRequest};
use crate::{InnerErrorCode, MeowError};

/// Builds a stable Azure `Put Block List` URL from a server-issued blob SAS URL.
pub fn put_block_list_url(blob_sas_url: impl AsRef<str>) -> Result<String, MeowError> {
    let mut url = Url::parse(blob_sas_url.as_ref()).map_err(|e| {
        MeowError::from_code(
            InnerErrorCode::ParameterEmpty,
            format!("invalid azure blob sas url: {e}"),
        )
    })?;
    url.query_pairs_mut()
        .append_pair(QUERY_COMP, QUERY_VALUE_BLOCK_LIST);
    Ok(url.to_string())
}

/// Builds Azure `Put Block List` XML body from block ids in final blob order.
pub fn block_list_xml<'a>(block_ids: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let mut xml = String::from(XML_DECLARATION_AND_BLOCK_LIST_OPEN);
    for id in block_ids {
        xml.push_str(XML_LATEST_OPEN);
        xml.push_str(id);
        xml.push_str(XML_LATEST_CLOSE);
    }
    xml.push_str(XML_BLOCK_LIST_CLOSE);
    xml.into_bytes()
}

/// Creates an Azure SAS `Put Block List` completion request.
pub fn put_block_list_request<'a>(
    blob_sas_url: impl AsRef<str>,
    block_ids: impl IntoIterator<Item = &'a str>,
) -> Result<CompletionRequest, MeowError> {
    Ok(
        CompletionRequest::new(Method::PUT, put_block_list_url(blob_sas_url)?)
            .with_headers(headers_from_pairs(&[(
                HEADER_CONTENT_TYPE,
                CONTENT_TYPE_APPLICATION_XML,
            )])?)
            .with_body(block_list_xml(block_ids)),
    )
}
