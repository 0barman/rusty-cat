use reqwest::header::{HeaderMap, ACCEPT, RANGE};
use reqwest::Url;

use super::constants::DEFAULT_RANGE_ACCEPT;
use super::signing::{header_value, param_error, signed_headers};
use crate::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx, MeowError, TransferTask};

/// Aliyun OSS direct range download protocol using AccessKey signing.
#[derive(Clone)]
pub struct AliOssDirectDownload {
    bucket: String,
    access_key_id: String,
    access_key_secret: String,
    region: String,
}

impl AliOssDirectDownload {
    pub fn new(
        bucket: impl Into<String>,
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        Self {
            bucket: bucket.into(),
            access_key_id: access_key_id.into(),
            access_key_secret: access_key_secret.into(),
            region: region.into(),
        }
    }

    fn object_canonical_uri_from_task_url(&self, task: &TransferTask) -> Result<String, MeowError> {
        let url = Url::parse(task.url()).map_err(param_error)?;
        Ok(format!("/{}{}", self.bucket, url.path()))
    }

    fn apply_signed_headers(
        &self,
        task: &TransferTask,
        method: &str,
        base: &mut HeaderMap,
    ) -> Result<(), MeowError> {
        let headers = signed_headers(
            method,
            self.object_canonical_uri_from_task_url(task)?.as_str(),
            None,
            &[],
            None,
            self.access_key_id.as_str(),
            self.access_key_secret.as_str(),
            self.region.as_str(),
        )?;
        for (k, v) in headers {
            if let Some(k) = k {
                base.insert(k, v);
            }
        }
        Ok(())
    }
}

impl BreakpointDownload for AliOssDirectDownload {
    fn resume_identity(&self, task: &TransferTask) -> Result<Option<Vec<u8>>, MeowError> {
        let mut headers = task.headers().clone();
        if !headers.contains_key(ACCEPT) {
            headers.insert(ACCEPT, header_value(DEFAULT_RANGE_ACCEPT)?);
        }
        let mut context = crate::http_breakpoint::canonical_resume_headers(headers);
        context.extend_from_slice(b"rusty-cat/aliyun-oss-direct/v1\0");
        crate::http_breakpoint::append_resume_identity_field(&mut context, self.bucket.as_bytes());
        crate::http_breakpoint::append_resume_identity_field(
            &mut context,
            self.access_key_id.as_bytes(),
        );
        crate::http_breakpoint::append_resume_identity_field(&mut context, self.region.as_bytes());
        Ok(Some(context))
    }

    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        self.apply_signed_headers(ctx.task, "HEAD", ctx.base)
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        ctx.base.insert(RANGE, header_value(ctx.range_value)?);
        if !ctx.base.contains_key(ACCEPT) {
            ctx.base.insert(ACCEPT, header_value(DEFAULT_RANGE_ACCEPT)?);
        }
        self.apply_signed_headers(ctx.task, "GET", ctx.base)
    }
}
