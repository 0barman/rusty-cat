use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, RANGE};

use crate::download_trait::{BreakpointDownload, DownloadHeadCtx, DownloadRangeGetCtx};
use crate::error::{InnerErrorCode, MeowError};
use crate::TransferTask;

use super::time::now_unix_secs;
use super::{PresignedDownloadUrlRefresher, PresignedRangeDownloadPlan};

thread_local! {
    // A stack, rather than one current key, also detects indirect recursion
    // such as A refresher -> B refresher -> A refresher.
    static ACTIVE_RANGE_REFRESH: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    // `BreakpointDownload` intentionally keeps its existing public API, where
    // range headers and URL are obtained by two consecutive calls. The default
    // backend performs both calls in one blocking closure. Remember the URL
    // paired with the just-built headers on that thread so a concurrent plan
    // refresh cannot mix two credential generations.
    static PENDING_RANGE_URL: RefCell<Option<(Weak<StdMutex<()>>, String)>> = const { RefCell::new(None) };
}

#[derive(Debug)]
struct ActiveRefreshGuard {
    key: usize,
}

impl ActiveRefreshGuard {
    fn enter(key: usize) -> Result<Self, MeowError> {
        ACTIVE_RANGE_REFRESH.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&key) {
                return Err(MeowError::from_code_str(
                    InnerErrorCode::InvalidTaskState,
                    "presigned range URL refresher re-entered the same download",
                ));
            }
            active.push(key);
            Ok(Self { key })
        })
    }
}

impl Drop for ActiveRefreshGuard {
    fn drop(&mut self) {
        ACTIVE_RANGE_REFRESH.with(|active| {
            let mut active = active.borrow_mut();
            if let Some(index) = active.iter().rposition(|key| *key == self.key) {
                active.remove(index);
            }
        });
    }
}

/// Provider-neutral presigned range-download implementation.
#[derive(Clone)]
pub struct PresignedRangeDownload {
    plan: Arc<RwLock<Arc<PresignedRangeDownloadPlan>>>,
    refresh_gate: Arc<StdMutex<()>>,
    url_refresher: Option<Arc<dyn PresignedDownloadUrlRefresher>>,
}

impl PresignedRangeDownload {
    /// Creates a download protocol from a plan.
    pub fn new(plan: PresignedRangeDownloadPlan) -> Self {
        Self {
            plan: Arc::new(RwLock::new(Arc::new(plan))),
            refresh_gate: Arc::new(StdMutex::new(())),
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
        self.plan.read().map(|g| g.as_ref().clone()).map_err(|_| {
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

    fn plan_snapshot(&self) -> Result<Arc<PresignedRangeDownloadPlan>, MeowError> {
        self.plan.read().map(|plan| Arc::clone(&plan)).map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "presigned range download plan lock poisoned",
            )
        })
    }

    fn ensure_fresh_snapshot(&self) -> Result<Arc<PresignedRangeDownloadPlan>, MeowError> {
        let plan = self.plan_snapshot()?;
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

        let refresh_key = Arc::as_ptr(&self.refresh_gate) as usize;
        if ACTIVE_RANGE_REFRESH.with(|active| active.borrow().contains(&refresh_key)) {
            return Err(MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "presigned range URL refresher re-entered the same download",
            ));
        }

        // Only one caller refreshes a stale generation. Re-check after taking
        // the gate because another range part may have already published a new
        // immutable snapshot while this caller waited.
        let _refresh = self.refresh_gate.lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "presigned range refresh lock poisoned",
            )
        })?;
        let plan = self.plan_snapshot()?;
        if !Self::should_refresh_plan(&plan)? {
            return Ok(plan);
        }

        let _active_refresh = ActiveRefreshGuard::enter(refresh_key)?;
        let refresh_result =
            catch_unwind(AssertUnwindSafe(|| refresher.refresh_range_download(&plan))).map_err(
                |_| {
                    MeowError::from_code_str(
                        InnerErrorCode::InvalidTaskState,
                        "presigned range URL refresher panicked",
                    )
                },
            )?;
        let mut refreshed = refresh_result.inspect_err(|e| {
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
        let refreshed = Arc::new(refreshed);
        let mut guard = self.plan.write().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::InvalidTaskState,
                "presigned range download plan lock poisoned",
            )
        })?;
        *guard = Arc::clone(&refreshed);
        Ok(refreshed)
    }

    fn request_from_fresh_snapshot(
        &self,
        range_value: &str,
        mut base: HeaderMap,
    ) -> Result<(String, HeaderMap), MeowError> {
        let plan = self.ensure_fresh_snapshot()?;
        base.insert(
            RANGE,
            HeaderValue::from_str(range_value).map_err(|e| {
                let detail = format!("invalid range header value '{range_value}': {e}");
                crate::log::emit_lazy({
                    let detail = detail.clone();
                    move || crate::log::Log::warn("range_get", detail)
                });
                MeowError::from_code(InnerErrorCode::ParameterEmpty, detail)
            })?,
        );
        if !base.contains_key(ACCEPT) {
            base.insert(
                ACCEPT,
                HeaderValue::from_static(crate::http_breakpoint::DEFAULT_RANGE_ACCEPT),
            );
        }
        Self::merge_headers(&mut base, &plan.range_headers);
        Ok((plan.range_url.clone(), base))
    }

    fn remember_range_url(&self, range_url: String) {
        let key = Arc::downgrade(&self.refresh_gate);
        PENDING_RANGE_URL.with(|pending| {
            *pending.borrow_mut() = Some((key, range_url));
        });
    }

    fn take_remembered_range_url(&self) -> Option<String> {
        let key = Arc::downgrade(&self.refresh_gate);
        PENDING_RANGE_URL.with(|pending| {
            let mut pending = pending.borrow_mut();
            match pending.as_ref() {
                Some((pending_key, _)) if Weak::ptr_eq(pending_key, &key) => {
                    pending.take().map(|(_, url)| url)
                }
                _ => None,
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn ensure_fresh_plan(&self) -> Result<PresignedRangeDownloadPlan, MeowError> {
        self.ensure_fresh_snapshot()
            .map(|plan| plan.as_ref().clone())
    }
}

impl BreakpointDownload for PresignedRangeDownload {
    fn total_size_hint(&self, _task: &TransferTask) -> Option<u64> {
        self.plan_snapshot().ok().and_then(|plan| plan.total_size)
    }

    fn head_url(&self, task: &TransferTask) -> String {
        self.plan_snapshot()
            .ok()
            .and_then(|plan| plan.head_url.clone())
            .unwrap_or_else(|| task.url().to_string())
    }

    fn range_url(&self, _task: &TransferTask) -> String {
        self.take_remembered_range_url().unwrap_or_else(|| {
            self.plan_snapshot()
                .map(|plan| plan.range_url.clone())
                .unwrap_or_default()
        })
    }

    fn merge_head_headers(&self, ctx: DownloadHeadCtx<'_>) -> Result<(), MeowError> {
        let plan = self.plan()?;
        Self::merge_headers(ctx.base, &plan.head_headers);
        Ok(())
    }

    fn merge_range_get_headers(&self, ctx: DownloadRangeGetCtx<'_>) -> Result<(), MeowError> {
        let (range_url, headers) =
            self.request_from_fresh_snapshot(ctx.range_value, ctx.base.clone())?;
        *ctx.base = headers;
        self.remember_range_url(range_url);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    use super::*;

    struct CountingRefresher {
        calls: AtomicUsize,
    }

    impl PresignedDownloadUrlRefresher for CountingRefresher {
        fn refresh_range_download(
            &self,
            plan: &PresignedRangeDownloadPlan,
        ) -> Result<PresignedRangeDownloadPlan, MeowError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            Ok(plan
                .clone()
                .with_range_expires_at_unix_secs(now_unix_secs()? + 3600))
        }
    }

    #[test]
    fn concurrent_expired_readers_refresh_only_once_and_see_complete_snapshot() {
        let refresher = Arc::new(CountingRefresher {
            calls: AtomicUsize::new(0),
        });
        let download = Arc::new(
            PresignedRangeDownload::new(
                PresignedRangeDownloadPlan::new("https://example.com/object")
                    .with_total_size(42)
                    .with_range_expires_at_unix_secs(1)
                    .with_refresh_before_secs(60),
            )
            .with_url_refresher(refresher.clone()),
        );
        let barrier = Arc::new(Barrier::new(16));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let download = Arc::clone(&download);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let plan = download.ensure_fresh_plan().expect("fresh plan");
                assert_eq!(plan.total_size, Some(42));
                assert_eq!(plan.range_url, "https://example.com/object");
                assert!(plan.range_expires_at_unix_secs.unwrap_or(0) > 1);
            }));
        }
        for thread in threads {
            thread.join().expect("reader");
        }
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    struct SnapshotRefresher;

    impl PresignedDownloadUrlRefresher for SnapshotRefresher {
        fn refresh_range_download(
            &self,
            plan: &PresignedRangeDownloadPlan,
        ) -> Result<PresignedRangeDownloadPlan, MeowError> {
            let mut refreshed = plan.clone();
            refreshed.range_url = "https://new.example.com/object".to_owned();
            refreshed
                .range_headers
                .insert("x-plan-generation", HeaderValue::from_static("new"));
            refreshed.range_expires_at_unix_secs = Some(now_unix_secs()? + 3600);
            Ok(refreshed)
        }
    }

    #[test]
    fn range_url_and_headers_are_built_from_one_refreshed_snapshot() {
        let download = PresignedRangeDownload::new(
            PresignedRangeDownloadPlan::new("https://old.example.com/object")
                .with_range_expires_at_unix_secs(1),
        )
        .with_url_refresher(Arc::new(SnapshotRefresher));

        let (url, headers) = download
            .request_from_fresh_snapshot("bytes=0-9", HeaderMap::new())
            .expect("range request");
        assert_eq!(url, "https://new.example.com/object");
        assert_eq!(headers.get("x-plan-generation").unwrap(), "new");
        assert_eq!(headers.get(RANGE).unwrap(), "bytes=0-9");

        download.remember_range_url(url);
        *download.plan.write().expect("plan write") = Arc::new(
            PresignedRangeDownloadPlan::new("https://later.example.com/object")
                .with_range_expires_at_unix_secs(now_unix_secs().unwrap() + 7200),
        );
        assert_eq!(
            download.take_remembered_range_url().as_deref(),
            Some("https://new.example.com/object"),
            "the URL paired with the headers must survive a concurrent plan publication"
        );
        assert!(download.take_remembered_range_url().is_none());
    }

    struct ReentrantRefresher {
        download: Mutex<Option<PresignedRangeDownload>>,
    }

    impl PresignedDownloadUrlRefresher for ReentrantRefresher {
        fn refresh_range_download(
            &self,
            _plan: &PresignedRangeDownloadPlan,
        ) -> Result<PresignedRangeDownloadPlan, MeowError> {
            self.download
                .lock()
                .map_err(|_| {
                    MeowError::from_code_str(
                        InnerErrorCode::InvalidTaskState,
                        "reentrant test lock poisoned",
                    )
                })?
                .as_ref()
                .ok_or_else(|| {
                    MeowError::from_code_str(
                        InnerErrorCode::InvalidTaskState,
                        "reentrant test download missing",
                    )
                })?
                .ensure_fresh_plan()
        }
    }

    #[test]
    fn refresher_reentry_returns_error_instead_of_deadlocking() {
        let refresher = Arc::new(ReentrantRefresher {
            download: Mutex::new(None),
        });
        let download = PresignedRangeDownload::new(
            PresignedRangeDownloadPlan::new("https://example.com/object")
                .with_range_expires_at_unix_secs(1),
        )
        .with_url_refresher(refresher.clone());
        *refresher.download.lock().expect("refresher download") = Some(download.clone());

        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = sender.send(download.ensure_fresh_plan());
        });
        let error = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("reentrant refresh must return promptly")
            .expect_err("reentrant refresh must fail");
        assert!(error.to_string().contains("re-entered"));
        thread.join().expect("refresh thread");
    }

    #[test]
    fn indirect_same_thread_refresh_reentry_is_rejected() {
        let first = ActiveRefreshGuard::enter(11).expect("enter first refresher");
        let second = ActiveRefreshGuard::enter(22).expect("enter nested refresher");
        let error = ActiveRefreshGuard::enter(11).expect_err("A -> B -> A must be rejected");
        assert!(error.to_string().contains("re-entered"));
        drop(second);
        drop(first);
        ActiveRefreshGuard::enter(11).expect("guards must clean up their stack on drop");
    }
}
