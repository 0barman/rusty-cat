use std::sync::{Arc, Mutex as StdMutex};

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, RANGE};

use crate::download_trait::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx};
use crate::error::{InnerErrorCode, MeowError};
use crate::TransferTask;

use super::time::now_unix_secs;
use super::{PresignedDownloadUrlRefresher, PresignedRangeDownloadPlan};

/// Provider-neutral presigned range-download implementation.
#[derive(Clone)]
pub struct PresignedRangeDownload {
    plan: Arc<StdMutex<PresignedRangeDownloadPlan>>,
    url_refresher: Option<Arc<dyn PresignedDownloadUrlRefresher>>,
}

impl PresignedRangeDownload {
    /// Creates a download protocol from a plan.
    pub fn new(plan: PresignedRangeDownloadPlan) -> Self {
        Self {
            plan: Arc::new(StdMutex::new(plan)),
            url_refresher: None,
        }
    }

    /// Adds a synchronous URL refresher used when range URL is expired or close
    /// to expiry.
    pub fn with_url_refresher(mut self, refresher: Arc<dyn PresignedDownloadUrlRefresher>) -> Self {
        self.url_refresher = Some(refresher);
        self
    }

    /// Returns a snapshot of the current download plan.
    pub fn plan(&self) -> Result<PresignedRangeDownloadPlan, MeowError> {
        self.plan.lock().map(|g| g.clone()).map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "presigned range download plan lock poisoned",
            )
        })
    }

    fn merge_headers(target: &mut HeaderMap, extra: &HeaderMap) {
        for (k, v) in extra {
            target.insert(k.clone(), v.clone());
        }
    }

    fn should_refresh_plan(plan: &PresignedRangeDownloadPlan) -> Result<bool, MeowError> {
        let Some(expires_at) = plan.range_expires_at_unix_secs else {
            return Ok(false);
        };
        Ok(now_unix_secs()?.saturating_add(plan.refresh_before_secs) >= expires_at)
    }

    fn is_plan_expired(plan: &PresignedRangeDownloadPlan) -> Result<bool, MeowError> {
        let Some(expires_at) = plan.range_expires_at_unix_secs else {
            return Ok(false);
        };
        Ok(now_unix_secs()? >= expires_at)
    }

    pub(crate) fn ensure_fresh_plan(&self) -> Result<PresignedRangeDownloadPlan, MeowError> {
        let plan = self.plan()?;
        if !Self::should_refresh_plan(&plan)? {
            return Ok(plan);
        }

        let Some(refresher) = &self.url_refresher else {
            if Self::is_plan_expired(&plan)? {
                crate::log::emit_lazy(|| {
                    crate::log::Log::error(
                        "range_get",
                        "presigned range URL expired and no refresher is configured",
                    )
                    .with_url(plan.range_url.as_str())
                });
                return Err(MeowError::from_code_str(
                    InnerErrorCode::InvalidTaskState,
                    "presigned range URL expired and no refresher is configured",
                ));
            }
            return Ok(plan);
        };

        let mut refreshed = refresher.refresh_range_download(&plan).map_err(|e| {
            crate::log::emit_lazy(|| {
                crate::log::Log::error(
                    "range_get",
                    format!(
                        "presigned range URL refresh/re-sign failed: {}",
                        crate::log::redact_secrets(&e.to_string())
                    ),
                )
                .with_url(plan.range_url.as_str())
            });
            e
        })?;
        if let (Some(old), Some(new)) = (plan.total_size, refreshed.total_size) {
            if old != new {
                crate::log::emit_lazy(|| {
                    crate::log::Log::error(
                        "range_get",
                        format!("refreshed range total_size mismatch: old={old} new={new}"),
                    )
                    .with_url(plan.range_url.as_str())
                });
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidTaskState,
                    format!("refreshed range total_size mismatch: old={old} new={new}"),
                ));
            }
        }
        if refreshed.total_size.is_none() {
            refreshed.total_size = plan.total_size;
        }
        let mut guard = self.plan.lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "presigned range download plan lock poisoned",
            )
        })?;
        *guard = refreshed.clone();
        Ok(refreshed)
    }
}

impl BreakpointDownload for PresignedRangeDownload {
    fn total_size_hint(&self, _task: &TransferTask) -> Option<u64> {
        self.plan.lock().ok().and_then(|g| g.total_size)
    }

    fn head_url(&self, task: &TransferTask) -> String {
        self.plan
            .lock()
            .ok()
            .and_then(|g| g.head_url.clone())
            .unwrap_or_else(|| task.url().to_string())
    }

    fn range_url(&self, _task: &TransferTask) -> String {
        self.plan
            .lock()
            .map(|g| g.range_url.clone())
            .unwrap_or_default()
    }

    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        let plan = self.plan()?;
        Self::merge_headers(ctx.base, &plan.head_headers);
        Ok(())
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        let plan = self.ensure_fresh_plan()?;
        ctx.base.insert(
            RANGE,
            HeaderValue::from_str(ctx.range_value).map_err(|e| {
                let detail = format!("invalid range header value '{}': {e}", ctx.range_value);
                crate::log::emit_lazy({
                    let detail = detail.clone();
                    move || crate::log::Log::warn("range_get", detail)
                });
                MeowError::from_code(InnerErrorCode::ParameterEmpty, detail)
            })?,
        );
        if !ctx.base.contains_key(ACCEPT) {
            ctx.base.insert(
                ACCEPT,
                HeaderValue::from_static(crate::http_breakpoint::DEFAULT_RANGE_ACCEPT),
            );
        }
        Self::merge_headers(ctx.base, &plan.range_headers);
        Ok(())
    }
}
