use reqwest::header::{HeaderValue, ACCEPT, RANGE};

use super::constants::DEFAULT_RANGE_ACCEPT;
use super::signing::{apply_signed_headers, header_value};
use crate::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx, MeowError};

/// Azure Blob direct range download protocol using SharedKey authentication.
#[derive(Clone)]
pub struct AzureBlobDirectDownload {
    account_name: String,
    account_key_b64: String,
}

impl AzureBlobDirectDownload {
    pub fn new(account_name: impl Into<String>, account_key_b64: impl Into<String>) -> Self {
        Self {
            account_name: account_name.into(),
            account_key_b64: account_key_b64.into(),
        }
    }
}

impl BreakpointDownload for AzureBlobDirectDownload {
    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        apply_signed_headers(
            ctx.task.url(),
            "HEAD",
            ctx.base,
            self.account_name.as_str(),
            self.account_key_b64.as_str(),
        )
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        ctx.base.insert(RANGE, header_value(ctx.range_value)?);
        if !ctx.base.contains_key(ACCEPT) {
            ctx.base
                .insert(ACCEPT, HeaderValue::from_static(DEFAULT_RANGE_ACCEPT));
        }
        apply_signed_headers(
            ctx.task.url(),
            "GET",
            ctx.base,
            self.account_name.as_str(),
            self.account_key_b64.as_str(),
        )
    }
}
