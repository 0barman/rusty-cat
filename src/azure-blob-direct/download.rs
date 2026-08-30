use reqwest::header::{HeaderValue, ACCEPT, RANGE};

use super::constants::DEFAULT_RANGE_ACCEPT;
use super::signing::{apply_signed_headers, header_value};
use crate::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx, MeowError, TransferTask};

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
    fn resume_identity(&self, task: &TransferTask) -> Result<Option<Vec<u8>>, MeowError> {
        let mut headers = task.headers().clone();
        if !headers.contains_key(ACCEPT) {
            headers.insert(ACCEPT, HeaderValue::from_static(DEFAULT_RANGE_ACCEPT));
        }
        headers.insert(
            super::constants::HEADER_MS_VERSION,
            HeaderValue::from_static(super::constants::MS_VERSION),
        );
        let mut context = crate::http_breakpoint::canonical_resume_headers(headers);
        context.extend_from_slice(b"rusty-cat/azure-blob-direct/v1\0");
        crate::http_breakpoint::append_resume_identity_field(
            &mut context,
            self.account_name.as_bytes(),
        );
        Ok(Some(context))
    }

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
