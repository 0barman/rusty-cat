use std::sync::Arc;

use serde_json::json;

use super::{
    headers_from_iter, headers_from_pairs, PresignedMultipartUpload, PresignedMultipartUploadPlan,
    PresignedRangeDownload, PresignedRangeDownloadPlan, PresignedUploadPart, PresignedUploadedPart,
};

struct TestRefresher;

#[async_trait::async_trait]
impl super::PresignedUploadUrlRefresher for TestRefresher {
    async fn refresh_upload_part(
        &self,
        part: &super::PresignedUploadPart,
    ) -> Result<super::PresignedUploadPart, crate::MeowError> {
        Ok(part
            .clone()
            .with_expires_at_unix_secs(super::PresignedMultipartUpload::now_unix_secs()? + 3600))
    }
}

struct TestDownloadRefresher;

impl super::PresignedDownloadUrlRefresher for TestDownloadRefresher {
    fn refresh_range_download(
        &self,
        plan: &super::PresignedRangeDownloadPlan,
    ) -> Result<super::PresignedRangeDownloadPlan, crate::MeowError> {
        Ok(plan
            .clone()
            .with_range_expires_at_unix_secs(
                super::PresignedMultipartUpload::now_unix_secs()? + 3600,
            )
            .with_range_headers(headers_from_pairs(&[("x-refreshed", "yes")])?))
    }
}

#[test]
fn test_plan_finds_part_by_offset() {
    let plan = PresignedMultipartUploadPlan::new(
        10,
        5,
        vec![
            PresignedUploadPart::put(1, 0, 5, "https://example.com/1"),
            PresignedUploadPart::put(2, 5, 5, "https://example.com/2"),
        ],
    );
    assert_eq!(plan.part_for_offset(5).unwrap().part_number, 2);
    assert!(plan.part_for_offset(10).is_none());
}

#[test]
fn presigned_multipart_advertises_parallel_parts() {
    use crate::upload_trait::BreakpointUpload;
    // Presigned multipart accounts parts by offset under a Mutex and reorders
    // the manifest by part_number at completion, so it is out-of-order safe.
    let upload = PresignedMultipartUpload::new(PresignedMultipartUploadPlan::new(
        10,
        5,
        vec![
            PresignedUploadPart::put(1, 0, 5, "https://example.com/1"),
            PresignedUploadPart::put(2, 5, 5, "https://example.com/2"),
        ],
    ));
    assert!(upload.supports_parallel_parts());
}

#[test]
fn presigned_accounting_is_order_independent() {
    // Parts completing out of order (the whole point of parallel parts) must
    // still reduce to the correct contiguous prefix and an ascending,
    // part_number-ordered completion manifest.
    let plan = PresignedMultipartUploadPlan::new(
        20,
        5,
        vec![
            PresignedUploadPart::put(1, 0, 5, "https://example.com/1"),
            PresignedUploadPart::put(2, 5, 5, "https://example.com/2"),
            PresignedUploadPart::put(3, 10, 5, "https://example.com/3"),
            PresignedUploadPart::put(4, 15, 5, "https://example.com/4"),
        ],
    );
    let upload = PresignedMultipartUpload::new(plan);

    let mk = |part_number, offset, etag: &str| PresignedUploadedPart {
        part_number,
        provider_part_id: None,
        offset,
        size: 5,
        etag: Some(etag.to_string()),
    };
    // Recorded in scrambled completion order.
    let scrambled = vec![
        mk(3, 10, "e3"),
        mk(1, 0, "e1"),
        mk(4, 15, "e4"),
        mk(2, 5, "e2"),
    ];

    // Contiguous prefix is order-independent: full file -> 20.
    assert_eq!(upload.resumed_offset_from(&scrambled), 20);

    // Completion manifest is always ascending by part_number.
    let mut to_complete = scrambled.clone();
    PresignedMultipartUpload::sort_dedup_completion_parts(&mut to_complete);
    let part_numbers: Vec<_> = to_complete.iter().map(|p| p.part_number).collect();
    assert_eq!(part_numbers, vec![1, 2, 3, 4]);
}

#[test]
fn test_plan_validate_rejects_duplicate_offsets() {
    let plan = PresignedMultipartUploadPlan::new(
        10,
        5,
        vec![
            PresignedUploadPart::put(1, 0, 5, "https://example.com/1"),
            PresignedUploadPart::put(2, 0, 5, "https://example.com/2"),
        ],
    );
    let err = plan.validate().expect_err("duplicate offset must fail");
    assert!(err.msg().contains("duplicate presigned part offset"));
}

#[test]
fn test_completion_json_contains_uploaded_parts() {
    let upload = PresignedMultipartUpload::new(
        PresignedMultipartUploadPlan::new(
            5,
            5,
            vec![PresignedUploadPart::put(1, 0, 5, "https://example.com/1")],
        )
        .with_upload_id("u-1"),
    );
    let body = upload
        .completion_json_body(&[PresignedUploadedPart {
            part_number: 1,
            provider_part_id: Some("block-1".to_string()),
            offset: 0,
            size: 5,
            etag: Some("etag-1".to_string()),
        }])
        .expect("json body");
    let json = String::from_utf8(body).expect("utf8 json");
    assert!(json.contains("u-1"));
    assert!(json.contains("etag-1"));
    assert!(json.contains("block-1"));
}

#[test]
fn test_custom_completion_body_builder_can_match_backend_contract() {
    let upload = PresignedMultipartUpload::new(
        PresignedMultipartUploadPlan::new(
            5,
            5,
            vec![PresignedUploadPart::put(1, 0, 5, "https://example.com/1")
                .with_provider_part_id("azure-block-1")],
        )
        .with_upload_id("u-1")
        .with_metadata("key", "prefix/file.bin")
        .with_complete_body_builder(Arc::new(|plan: &PresignedMultipartUploadPlan, parts: &[PresignedUploadedPart]| {
            let completed = parts
                .iter()
                .map(|part| {
                    json!({
                        "part_number": part.part_number,
                        "etag": part.provider_part_id.as_deref().or_else(|| part.etag_unquoted()).unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&json!({
                "key": plan.metadata.get("key"),
                "upload_id": plan.upload_id,
                "parts": completed,
            }))
            .map_err(|e| crate::MeowError::from_code(
                crate::InnerErrorCode::ResponseParseError,
                format!("serialize test body failed: {e}"),
            ))
        })),
    );

    let builder = upload
        .plan()
        .complete_body_builder
        .as_ref()
        .expect("custom body builder");
    let body = builder
        .build_body(
            upload.plan(),
            &[PresignedUploadedPart {
                part_number: 1,
                provider_part_id: Some("azure-block-1".to_string()),
                offset: 0,
                size: 5,
                etag: Some("\"etag-1\"".to_string()),
            }],
        )
        .expect("completion body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(value["key"], "prefix/file.bin");
    assert_eq!(value["upload_id"], "u-1");
    assert_eq!(value["parts"][0]["part_number"], 1);
    assert_eq!(value["parts"][0]["etag"], "azure-block-1");
}

#[test]
fn test_headers_from_iter_accepts_dynamic_headers() {
    let headers = headers_from_iter(vec![(
        "x-ms-blob-type".to_string(),
        "BlockBlob".to_string(),
    )])
    .expect("headers");
    assert_eq!(headers.get("x-ms-blob-type").unwrap(), "BlockBlob");
}

#[test]
fn test_uploaded_part_etag_unquoted() {
    let part = PresignedUploadedPart {
        part_number: 1,
        provider_part_id: None,
        offset: 0,
        size: 5,
        etag: Some("\"etag-1\"".to_string()),
    };
    assert_eq!(part.etag_unquoted(), Some("etag-1"));
}

#[tokio::test]
async fn test_part_for_upload_uses_refresher_near_expiry() {
    let expires = PresignedMultipartUpload::now_unix_secs().expect("now") + 1;
    let upload = PresignedMultipartUpload::new(
        PresignedMultipartUploadPlan::new(
            5,
            5,
            vec![
                PresignedUploadPart::put(1, 0, 5, "https://old.example.com/1")
                    .with_expires_at_unix_secs(expires),
            ],
        )
        .with_refresh_before_secs(60),
    )
    .with_url_refresher(Arc::new(TestRefresher));

    let part = upload.part_for_upload(0).await.expect("refreshed part");
    assert!(part.expires_at_unix_secs.expect("expires") > expires);
}

#[test]
fn test_range_download_uses_refresher_near_expiry() {
    let expires = PresignedMultipartUpload::now_unix_secs().expect("now") + 1;
    let download = PresignedRangeDownload::new(
        PresignedRangeDownloadPlan::new("https://old.example.com/file")
            .with_total_size(10)
            .with_range_expires_at_unix_secs(expires)
            .with_refresh_before_secs(60),
    )
    .with_url_refresher(Arc::new(TestDownloadRefresher));

    let plan = download.ensure_fresh_plan().expect("fresh plan");
    assert_eq!(plan.range_headers.get("x-refreshed").unwrap(), "yes");
    assert!(plan.range_expires_at_unix_secs.expect("expires") > expires);
}

// ---- phase 2: presigned multipart cross-restart resume (with_resumed_parts) ----

fn plan_three_5b_parts() -> PresignedMultipartUploadPlan {
    PresignedMultipartUploadPlan::new(
        15,
        5,
        vec![
            PresignedUploadPart::put(1, 0, 5, "https://example.com/1"),
            PresignedUploadPart::put(2, 5, 5, "https://example.com/2"),
            PresignedUploadPart::put(3, 10, 5, "https://example.com/3"),
        ],
    )
}

fn uploaded(part_number: u64, offset: u64, size: u64) -> PresignedUploadedPart {
    PresignedUploadedPart {
        part_number,
        provider_part_id: None,
        offset,
        size,
        etag: Some(format!("etag-{part_number}")),
    }
}

#[test]
fn test_resumed_offset_covers_full_contiguous_prefix() {
    let up = PresignedMultipartUpload::new(plan_three_5b_parts());
    let resumed = up.resumed_offset_from(&[uploaded(1, 0, 5), uploaded(2, 5, 5)]);
    assert_eq!(resumed, 10, "two contiguous 5-byte parts resume at offset 10");
}

#[test]
fn test_resumed_offset_stops_at_gap() {
    // Parts at 0 and 10 leave a gap at 5; resume must stop at the gap, not skip it.
    let up = PresignedMultipartUpload::new(plan_three_5b_parts());
    let resumed = up.resumed_offset_from(&[uploaded(1, 0, 5), uploaded(3, 10, 5)]);
    assert_eq!(resumed, 5);
}

#[test]
fn test_resumed_offset_stops_on_plan_size_mismatch() {
    // A persisted part whose size disagrees with the plan is not trusted.
    let up = PresignedMultipartUpload::new(plan_three_5b_parts());
    let resumed = up.resumed_offset_from(&[uploaded(1, 0, 4)]);
    assert_eq!(resumed, 0);
}

#[test]
fn test_resumed_offset_ignores_part_not_in_plan() {
    // An offset the plan does not contain cannot start the contiguous prefix.
    let up = PresignedMultipartUpload::new(plan_three_5b_parts());
    let resumed = up.resumed_offset_from(&[uploaded(9, 3, 5)]);
    assert_eq!(resumed, 0);
}

#[test]
fn test_resumed_offset_empty_is_zero() {
    let up = PresignedMultipartUpload::new(plan_three_5b_parts());
    assert_eq!(up.resumed_offset_from(&[]), 0);
}

#[tokio::test]
async fn test_with_resumed_parts_dedups_by_offset() {
    // Injecting two parts for the same offset keeps only one.
    let up = PresignedMultipartUpload::new(plan_three_5b_parts())
        .with_resumed_parts(vec![uploaded(1, 0, 5), uploaded(1, 0, 5), uploaded(2, 5, 5)]);
    let parts = up.uploaded_parts().await;
    assert_eq!(parts.len(), 2);
    let mut offsets: Vec<u64> = parts.iter().map(|p| p.offset).collect();
    offsets.sort_unstable();
    assert_eq!(offsets, vec![0, 5]);
}

#[tokio::test]
async fn test_with_resumed_parts_empty_is_fresh_upload() {
    let up = PresignedMultipartUpload::new(plan_three_5b_parts()).with_resumed_parts(vec![]);
    assert!(up.uploaded_parts().await.is_empty());
    assert_eq!(up.resumed_offset_from(&[]), 0);
}

#[test]
fn test_sort_dedup_completion_parts_orders_by_part_number_and_drops_dup_offset() {
    let mut parts = vec![
        uploaded(3, 10, 5),
        uploaded(1, 0, 5),
        uploaded(2, 5, 5),
        uploaded(2, 5, 5), // duplicate offset 5
    ];
    PresignedMultipartUpload::sort_dedup_completion_parts(&mut parts);
    let pns: Vec<u64> = parts.iter().map(|p| p.part_number).collect();
    let offsets: Vec<u64> = parts.iter().map(|p| p.offset).collect();
    assert_eq!(pns, vec![1, 2, 3], "ascending part numbers");
    assert_eq!(offsets, vec![0, 5, 10], "duplicate offset dropped");
}
