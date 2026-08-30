# rusty-cat test scenarios and verification matrix

[简体中文](test-scenarios.zh-CN.md)

This document maps `rusty-cat` tests to public behavior, critical safety boundaries, and the latest verification result. Deterministic automation and runnable end-to-end scenarios are listed separately so that local fixtures, live-provider evidence, and unexecuted cases are not conflated.

The matrix intentionally omits credentials, account and storage-space identifiers, service endpoints, object keys, signed URLs, tokens, raw validators, and content digests. Live-provider results are a verification snapshot dated 2026-08-27. They may depend on external service availability, temporary authorization, and network conditions; they are not a guarantee that a third-party service will remain available.

The **Latest verification** column records the last actual run of each scenario in the 2026-08-27 verification session; it does not mean that every live service was rerun after every later code change. The final revision reran both workspaces' default automated suites and the live `azure-direct` acceptance scenario.

## Verification snapshot

Run these commands from the repository root:

- `cargo test --manifest-path test-app/Cargo.toml --workspace`: 176 passed, 0 failed, and 4 explicitly ignored.
- `cargo test --workspace --all-features`: every non-ignored unit, integration, and documentation test in the root workspace passed, including 270 library unit tests and 116 documentation tests.
- The normally ignored 100-round randomized process-kill recovery soak was run separately and passed earlier; it was not rerun after the later download-request construction change.
- The four ignored entries are not hidden failures: two are live-backend contracts, one is the 100-round soak, and one is a subprocess entry point used only by its parent test.

Counts will change as cases are added. The commands above and the CI configuration remain the authoritative source for the current suite.
Source-evidence links in the tables assume a full repository checkout; a standalone published crate archive does not include `test-app`.

## Automated public-behavior matrix

The default automated suite focuses on public-behavior contracts and is supplemented by crate-internal pure-function unit tests. Networked cases use only random loopback HTTP(S) services, fictional accounts, and deterministic fixtures. They require neither internet access nor real cloud credentials.

| Scenario | Detailed coverage | Assertions and safety boundaries | Latest verification |
|---|---|---|---|
| [Public API and configuration](../../test-app/tests/public_api_surface.rs) | Builders, getters, upload and download options, Binary configuration, provider-plan helpers, statuses, error codes, and structured log fields | Valid values round-trip exactly; zero, out-of-range, and invalid combinations return stable errors before task execution | Passed in the default offline suite |
| [Invalid input and upload sources](../../test-app/tests/invalid_input_api.rs) | Required upload and download fields, file and in-memory sources, source overrides, zero-byte uploads, missing files, and deletion after task construction | Invalid input causes no network I/O or success callback; error types and codes remain diagnosable | Passed in the default offline suite |
| [Logging and secret protection](../../test-app/tests/logging_api.rs) | Listener registration, replacement, clearing, lazy construction, duplicate registration, URL and free-text redaction, and callback-panic isolation | Logs and error chains never expose raw signature parameters, credentials, or tokens; a listener panic cannot stop a transfer | Passed in the default offline suite |
| [Binary downloads](../../test-app/tests/binary_api.rs) | In-memory responses, content type, custom headers, response-size bounds, redirects, timeouts, disconnect retries, cancellation, shutdown, and concurrency limits | Cross-origin redirects remove sensitive headers; oversized responses fail closed; bytes from failed attempts cannot contaminate a successful result | Passed in the default offline suite |
| [Default upload and Range download](../../test-app/tests/transfer_protocol_api.rs) | In-memory upload, fast and independently retried prepare, JSON plans, methods and headers, custom HTTP clients, known-size HEAD bypass, serial and parallel byte ranges, and the HTTP error matrix | Part offsets, lengths, and reassembly are byte-exact; `Content-Range` must match; hard 4xx errors fail fast while transient failures honor the retry budget | Passed in the default offline suite |
| [Concurrent downloads and object generations](../tests/concurrent_download_test.rs) | Serial and parallel ranges, out-of-order completion, strong ETags, unquoted Azure ETags, `If-Match`, response-generation changes, target leases, and checkpoint recovery | Bytes from different remote generations can never mix; unprovable validators cause fail-closed behavior or a safe refetch; providers cannot remove, replace, or append the executor's prepared `If-Match` | Passed: 39 focused regressions |
| [Custom transfer protocols](../../test-app/tests/custom_protocol_api.rs) | Every download hook, independent URL/header/size parsing/resume identity, upload prepare/chunk/complete, context getters, serial fallback, and parallel opt-in | Every hook receives the exact task and part context; undeclared parallelism stays serial; completion payloads are forwarded unchanged | Passed in the default offline suite |
| [Client lifecycle and control](../../test-app/tests/client_control_api.rs) | Active and queued snapshots, pause, resume, cancel, close, global listeners, duplicate tasks, command-queue backpressure, and concurrent shutdown | Task IDs remain stable; each task has exactly one terminal state; full queues fail fast; close cannot deadlock and rejects new work afterward | Passed in the default offline suite |
| [Presigned lifecycle](../../test-app/tests/presigned_lifecycle_api.rs) | Multipart parts and ETags, built-in or custom completion bodies, non-2xx completion, abort, upload and download URL refresh, and resumed parts | Expired URLs refresh only through the protocol contract; confirmed parts are not uploaded again; cancellation calls abort and never complete | Passed in the default offline suite |
| [Normal checkpoint recovery](../../test-app/tests/checkpoint_safety_api.rs) | Strong-ETag binding, missing-part fetches, deleted or truncated targets, remote generation changes, and successful sidecar cleanup | A part is reused only when both its digest and object identity are provable; an untrusted target or generation causes a safe full refetch | Passed in the default offline suite |
| [Real process-crash recovery](../../test-app/tests/process_crash_resume_api.rs) | A subprocess is killed without calling `close()`; serial prefixes, sparse parallel parts, crashes before sidecar publication, remote generation changes, and file-lock release are covered | A new process reuses only persisted and proven parts; pre-publication crashes restart from zero; successful completion removes the sidecar; locks can be reacquired immediately | Current deterministic cases passed; the explicit 100-round soak passed an earlier focused run and was not rerun on the final revision |
| [Adversarial checkpoint corruption](../../test-app/tests/checkpoint_adversarial_api.rs) | Corrupt headers, lengths, bitmaps, digests, tail bytes, and target data; oversized sparse snapshots, grid changes, temporary files, private namespaces, directories, and symlink conflicts | Forged completion bits cannot skip missing parts; only digest-proven parts are reused; conflicting paths fail closed without modifying user files or sentinels | Passed in the default offline suite |
| [Upload-source generation consistency](../../test-app/tests/active_upload_source_mutation_api.rs) | Overwrite, truncate, extend, replace, or rename after build/queue and during prepare, serial and parallel parts, pause/resume, finalize, and cancel | Bytes from different source generations can never mix; a physical replacement with identical content may continue; failures drain in-flight futures before aborting and never complete | Passed in the default offline suite |
| [Upload failure diagnostics](../../test-app/tests/upload_source_safety_api.rs) | Full and partial truncation, same-length replacement, and combinations of a primary error with an abort-cleanup error | Only complete parts from the original generation may leave the process; cleanup failures cannot replace the primary error or expose sensitive diagnostics | Passed in the default offline suite |
| [Direct-provider contracts](../../test-app/tests/provider_direct_api.rs) | Aliyun OSS V4 and Azure Shared Key requests, parallel part upload, complete, abort, Range download, cancellation cleanup, and invalid Azure keys | Part offsets and request bodies are exact; cleanup requests are signed too; invalid keys fail before network I/O; upload-and-download round trips are byte-exact | Passed: 7 focused regressions |
| Azure conditional Range signing ([fixed vectors](../src/azure-blob-direct/signing.rs), [serial/parallel contract](../tests/concurrent_download_test.rs), [independent wire verifier](../../test-app/tests/provider_direct_api.rs)) | Fixed signing vectors, ETag latching from HEAD or the first range, final-request signing of `Range` and `If-Match`, and the known-size path without HEAD | An independent verifier rejects any post-signing mutation of `Range` or `If-Match`; provider hooks on both serial and parallel executor paths see final conditional headers before signing; conflicts fail before network I/O | Passed with independent offline verification and live Azure evidence |
| [TLS, DNS, proxy, and network faults](../../test-app/tests/network_fault_api.rs) | Private-CA acceptance and rejection, hostname mismatch, DNS stubs, HTTPS CONNECT, proxy rejection, TCP RST, half-open sockets, delay, throttling, and wire-body truncation | Untrusted CAs and hostname errors fail during the TLS handshake, before application-level HTTP handling; RST and truncation retry safely within budget; failed-response bytes cannot contaminate the target | Passed in the default offline suite |
| [Local HTTP(S) fault server](../../test-app/test-server/src/lib.rs) | Random-port HTTP/HTTPS, concurrency, delay, disconnect, RST, half-open sockets, throttling, arbitrary status/header/body responses, CONNECT proxying, and request capture | HEAD emits no body; connections remain concurrent; dropping a fixture interrupts delayed responses, closes active connections, and releases the port | Passed in the default offline suite |
| [Backend HTTPS contract](../../test-app/tests/loonadm_https_backend_contract.rs) | Fictional-account login, multipart init, parallel PUT, complete, abort, confirmed-part recovery, the control-plane error matrix, and data-plane retry classification | Both planes use HTTPS secured by the private test CA; readback is byte-for-byte equal; work is rejected after abort; recovery skips confirmed parts; status and error semantics are preserved | Local contract passed; live-deployment contracts are listed below |

## Runnable end-to-end and live-service scenarios

One `test-app` dispatcher manages the first 14 command entries; the final entry is an explicitly enabled integration test. Statuses record the latest actual run. They do not replace the deterministic suite above and do not guarantee that an external service will retain the same state.

| Scenario | Detailed coverage | Assertions and safety boundaries | Latest verification |
|---|---|---|---|
| [`loonadm`](../../test-app/src/lib.rs) | Backend login, multiple download sources, multipart init, parallel upload, complete/abort, metrics, and post-upload readback | A failure in one download source does not skip later sources or stop the upload flow; failed uploads abort; post-upload readback passes only when the backend provides a readable object address or read permission | **Partially passed; overall scenario failed**: TLS/login, a live download, 20-part upload, and complete passed; write-only backend authorization made post-upload readback return 403 |
| [`direct-download`](../../test-app/src/download/direct.rs) | Probes a caller-supplied Range resource and compares serial with multi-lane download | The origin must return a valid 206 and `Content-Range`; configured expected size or digest mismatches fail the scenario | **Passed**: both serial and four-lane downloads completed with matching size and MD5 |
| [`otacdn-x86-64`](../../test-app/src/scenarios/otacdn_x86_64.rs) | Downloads a real release artifact, pauses after non-zero progress, proves file growth stops, and resumes the same task | The task must genuinely enter `Paused`; resume must complete; final size and release digest must match; the downloaded artifact is never executed | **Passed**: pause/resume trace, size, and MD5 all matched |
| [`azure-download`](../../test-app/src/download/oss_azure.rs) | Uses read-only temporary authorization for a four-lane Range download and report generation | Validates 206, `Content-Range`, final size, and complete assembly of 300 ranges; reports cannot retain signature parameters | **Passed**: all 300 ranges completed and final size matched |
| [`azure-sas-roundtrip`](../../test-app/src/scenarios/azure_sas.rs) | Put Block, one ordered Put Block List, parallel Range readback, and SHA-256 verification | Resource scope and read/write permission are checked before I/O; existing objects cannot be overwritten; upload and readback size and digest must match | **Blocked by configuration**: the available authorization was read-only and lacked upload permission, so no upload was attempted |
| [`aliyun-presigned`](../../test-app/src/download/aliyun_presigned.rs) | Aliyun OSS V4 presigned Range download, pre-request expiry checking, and object-size validation | A known-expired URL is rejected before network I/O; probe size, configured size, and on-disk size must agree | **Passed**: 31 Range requests completed and final size matched |
| [`aliyun-direct`](../../test-app/src/scenarios/cloud_direct.rs) | Official Aliyun direct provider multipart upload, signed HEAD/Range download, and content readback | Uses an isolated object; post-upload readback must be byte-exact; direct credentials are suitable only for a trusted test environment | **Passed**: 5 MiB payload, 1 MiB parts, four-lane upload, and byte-exact readback |
| [`aliyun-prepare-files`](../../test-app/src/scenarios/aliyun_upload_matrix.rs) | Generates deterministic 0 B, 1 B, 1 KiB, around-part-boundary, 5 MiB-boundary, and multi-part-tail files | Same-sized files may be reused; it touches only the local file system and is a fixture-generation step, not an independent transfer proof | **Passed**: nine boundary files were generated or reused |
| [`aliyun-upload-matrix`](../../test-app/src/scenarios/aliyun_upload_matrix.rs) | Runs direct and presigned multipart upload and readback for every boundary file, then checks size and SHA-256 | Every non-empty file must be byte-exact; 0 B is an expected pre-I/O rejection under the current public contract; any unexpected result fails the matrix | **Passed**: 16 valid transfers passed, two 0 B cases were rejected as expected, and there were no unexpected failures |
| [`azure-direct`](../../test-app/src/scenarios/cloud_direct.rs) | Official Azure Shared Key provider Put Block, Block List, HEAD, and conditional Range readback | The final signature covers both `Range` and trusted `If-Match`; requests after first-range ETag latching are re-signed; upload and readback are byte-exact | **Passed**: live 5 MiB upload with 1 MiB parts and byte-exact readback; the former 403 did not recur |
| [`local-http`](../../test-app/src/scenarios/local.rs) | Local custom upload and standard Range download, with real pause/resume on both tasks | Uploaded bytes equal server-received bytes; the download is byte-exact; pause and resume reach the correct states | **Passed** |
| [`resume-restart`](../../test-app/src/scenarios/local.rs) | Injects a first-run Range failure, leaves a trusted checkpoint, and resumes with a new client | The first run must fail; recovery fetches only the missing range and never repeats proven parts; the final file is byte-exact | **Passed**: fetched only the missing part |
| [`restore-paused`](../../test-app/src/scenarios/local.rs) | Imports multiple paused tasks and resumes only the caller-selected tasks | Import causes zero HTTP I/O and creates no target file; selected tasks complete while unselected tasks stay paused without files | **Passed** |
| [`local-all`](../../test-app/src/scenarios/local.rs) | Runs `local-http`, `resume-restart`, and `restore-paused` in sequence | This aggregate entry is not counted as a fourth independent coverage area; any child failure fails the aggregate | **Passed**: all three child scenarios passed |
| [`loonadm_live_contract`](../../test-app/tests/loonadm_live_contract.rs) | Live-deployment login/init/abort/auth rejection, plus rebuilding after the first confirmed part and completing the remaining upload | Requires explicit opt-in with a dedicated test account; confirmed parts cannot be uploaded twice; verifiable receipts are checked by size or byte-for-byte equality | **Not run**: two live-deployment contracts require dedicated configuration and explicit enablement |

`full` is only a compatibility alias for `loonadm`, and `help` is not a test scenario. Neither is counted again.

## Reproducing the suite

Start with the deterministic regressions in both the root workspace and the
independent `test-app` workspace:

```bash
cargo test --workspace --all-features
cargo test --manifest-path test-app/Cargo.toml --workspace
```

Before a release, the normally ignored 100-round process-crash recovery soak can
be run explicitly:

```bash
cargo test --manifest-path test-app/Cargo.toml \
  --test process_crash_resume_api \
  randomized_process_crash_soak_100_rounds \
  -- --ignored --nocapture --test-threads=1
```

Then run an explicit end-to-end scenario when needed:

```bash
cargo run --manifest-path test-app/Cargo.toml -- <scenario>
```

Live-service entries may create isolated test objects. Use only dedicated test resources and least-privilege configuration. The default automated suite never contacts those services and never reports an unexecuted live scenario as passed.
