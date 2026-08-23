# Presigned multipart lifecycle

Provider-neutral presigned primitives are enabled by the `presigned` feature and
are also enabled by `aliyun-oss-presigned` and `azure-blob-sas`. A trusted backend
creates the remote session and short-lived URLs; the client uploads parts and
optionally asks the backend or provider to complete or abort the session.

## Upload plan

`PresignedMultipartUploadPlan` contains:

| Field | Contract |
|---|---|
| `total_size`, `chunk_size` | Must match the local file and server plan |
| `parts` | Unique offsets with part number, size, URL, headers, and optional expiry |
| `upload_id` | Optional provider/session identifier |
| `metadata` | Application values supplied to a custom completion body builder |
| `complete_request` | Optional final commit/callback HTTP request |
| `abort_request` | Optional cleanup request sent on user cancellation |
| `complete_body_builder` | Optional application-specific completion body |
| `refresh_before_secs` | Refresh threshold; default 60 seconds |

Call `validate()` as early as possible on backend-provided plans. It rejects zero
chunk size, missing parts for non-empty uploads, zero-size parts, duplicate
offsets, overflow, and ranges beyond the declared total.

## Completion and abort are explicit

Uploading every part is not always the same as committing an object.

- With `complete_request`, the SDK sends the final request after all parts join.
- `CompletionRequest::with_uploaded_parts_json_body()` generates JSON containing
  upload/session data and uploaded part metadata such as part number, provider
  part ID, byte range, and ETag.
- `with_complete_body_builder` replaces that generated shape for an
  application-specific API.
- Without `complete_request`, the SDK reports local task success after all part
  requests succeed; it does not prove that a provider-side multipart object was
  merged.
- Without `abort_request`, cancellation performs no remote presigned-session
  cleanup. Your backend should expire or reap orphaned sessions.

Treat completion and abort endpoints as authenticated, idempotent operations.
The backend must verify that the user, upload ID, object key, sizes, part numbers,
and ETags belong together rather than trusting client JSON.

## URL expiry and refresh

Each `PresignedUploadPart` can carry expiry metadata. When a URL is expired or
within `refresh_before_secs`, a protocol configured with
`with_url_refresher(Arc<dyn PresignedUploadUrlRefresher>)` asks the backend for a
replacement. The replacement must retain the same part number, offset, and size.

`PresignedDownloadUrlRefresher` is synchronous because download URL/header hooks
are synchronous. It should use a local cache populated by application code and
must not block on a remote refresh call.

## Restart persistence

Persist the logical file identity, upload/session ID, total and chunk sizes,
completed part metadata, and enough backend identity to request a fresh plan.
Do not rely on an expired signed URL as durable identity and do not log its query
string. On restart, obtain fresh URLs for unfinished parts and reconstruct the
protocol with the already completed part metadata supported by the provider
helper.

The backend remains the source of truth. Reconcile persisted client parts with
the remote session before skipping work, because a local write can outlive a
failed network response or a remote session may have expired.

For provider-specific construction and completion behavior, see
[Aliyun OSS presigned](aliyun-oss-presigned.md) and
[Azure Blob SAS](azure-blob-sas.md). For the complete recovery matrix, see
[Resume after restart](resume-after-restart.md).
