use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::error::{InnerErrorCode, MeowError};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
/// A fixed upper bound keeps per-file digest metadata predictable even when a
/// caller chooses a pathological one-byte upload chunk. At 32 bytes per entry
/// this caps the digest table at about 32 MiB; ordinary MiB-sized chunks still
/// cover terabyte-scale files.
const MAX_UPLOAD_DIGEST_BLOCKS: u64 = 1_000_000;
type Sha256Digest = [u8; 32];

fn upload_hash_workers() -> Arc<tokio::sync::Semaphore> {
    static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    Arc::clone(WORKERS.get_or_init(|| {
        let permits = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .clamp(1, 4);
        Arc::new(tokio::sync::Semaphore::new(permits))
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileGeneration {
    len: u64,
    readonly: bool,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

impl FileGeneration {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            mtime_nsec: metadata.mtime_nsec(),
            #[cfg(unix)]
            ctime: metadata.ctime(),
            #[cfg(unix)]
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

fn visible_path_requires_hash(
    verify_content: bool,
    path_generation: &FileGeneration,
    handle_generation: &FileGeneration,
    accepted_path: Option<&FileGeneration>,
) -> bool {
    verify_content
        || (path_generation != handle_generation && accepted_path != Some(path_generation))
}

#[derive(Debug)]
struct ContentIdentity {
    sha256: Sha256Digest,
    verification_block_bytes: usize,
    block_sha256: Arc<[Sha256Digest]>,
}

impl ContentIdentity {
    fn matches(&self, scan: &ContentScan) -> bool {
        self.sha256 == scan.sha256 && self.block_sha256.as_ref() == scan.block_sha256.as_slice()
    }
}

struct ContentScan {
    protocol_md5: md5::Digest,
    sha256: Sha256Digest,
    block_sha256: Vec<Sha256Digest>,
}

#[derive(Default)]
struct UploadFileState {
    file: Option<Arc<File>>,
    accepted_handle_generation: Option<FileGeneration>,
    accepted_path_generation: Option<FileGeneration>,
}

/// Hashed generation plus an execution-scoped, lazily opened stable handle.
///
/// Enqueue hashing deliberately closes its handle so a large paused/queued task
/// set cannot exhaust the process file-descriptor limit. The first prepare/read
/// reopens and validates the exact hashed generation; active positioned reads
/// then share that handle without sharing a mutable cursor. The executor
/// releases it again whenever the run (including pause/failure) settles.
#[derive(Clone)]
pub(crate) struct UploadFileSnapshot {
    path: PathBuf,
    state: Arc<Mutex<UploadFileState>>,
    generation: FileGeneration,
    sign: Arc<str>,
    content: Arc<ContentIdentity>,
}

impl std::fmt::Debug for UploadFileSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadFileSnapshot")
            .field("path", &self.path)
            .field("generation", &self.generation)
            .field("sign", &self.sign)
            .field("content_sha256", &self.content.sha256)
            .finish_non_exhaustive()
    }
}

impl UploadFileSnapshot {
    #[cfg(test)]
    pub(crate) async fn open_and_hash(path: PathBuf, expected_len: u64) -> Result<Self, MeowError> {
        Self::open_and_hash_with_verification_block_bytes(
            path,
            expected_len,
            HASH_BUFFER_BYTES as u64,
        )
        .await
    }

    pub(crate) async fn open_and_hash_with_verification_block_bytes(
        path: PathBuf,
        expected_len: u64,
        verification_block_bytes: u64,
    ) -> Result<Self, MeowError> {
        let verification_block_bytes = usize::try_from(
            verification_block_bytes.clamp(1, HASH_BUFFER_BYTES as u64),
        )
        .map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::IoError,
                "upload verification block size does not fit this target architecture",
            )
        })?;
        let permit = upload_hash_workers().acquire_owned().await.map_err(|_| {
            MeowError::from_code_str(InnerErrorCode::IoError, "upload hash worker limiter closed")
        })?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::open_and_hash_blocking(path, expected_len, verification_block_bytes)
        })
        .await
        .map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::IoError,
                format!("upload source hash worker failed: {e}"),
            )
        })?
    }

    fn open_and_hash_blocking(
        path: PathBuf,
        expected_len: u64,
        verification_block_bytes: usize,
    ) -> Result<Self, MeowError> {
        let file = File::open(&path).map_err(|e| {
            let code = if e.kind() == io::ErrorKind::NotFound {
                InnerErrorCode::FileNotFound
            } else {
                InnerErrorCode::IoError
            };
            MeowError::from_source(
                code,
                format!("open upload source failed: {}", path.display()),
                e,
            )
        })?;
        let before = file.metadata().map_err(|e| {
            MeowError::from_io(format!("stat upload source failed: {}", path.display()), e)
        })?;
        if !before.file_type().is_file() {
            return Err(MeowError::from_code(
                InnerErrorCode::IoError,
                format!("upload source is not a regular file: {}", path.display()),
            ));
        }
        // `expected_len` was captured by the public builder. Preserve the
        // existing enqueue-stage behavior when the path changed between build
        // and enqueue: hash the generation that is actually opened here, then
        // let the transfer's exact range read fail before any short part is
        // sent. U0 freezes this opened generation; it does not move legacy
        // failures from worker status callbacks into `try_enqueue`.
        let _builder_len_changed = before.len() != expected_len;
        let generation = FileGeneration::from_metadata(&before);
        // One initial pass produces all three identities needed by the upload:
        // the public/protocol MD5, a collision-resistant whole-file SHA-256,
        // and fixed-block SHA-256 values from which every later positioned
        // read can be verified without knowing the task's chunk size here.
        let scan = scan_file_at(&file, before.len(), verification_block_bytes).map_err(|e| {
            Self::map_source_io_error(
                &path,
                format!(
                    "calculate upload source signature failed: {}",
                    path.display()
                ),
                e,
            )
        })?;
        let after = file.metadata().map_err(|e| {
            MeowError::from_io(
                format!("re-stat upload source failed: {}", path.display()),
                e,
            )
        })?;
        if FileGeneration::from_metadata(&after) != generation {
            return Err(Self::source_changed_error(
                &path,
                "metadata changed while calculating signature",
            ));
        }
        let sign = format!("{:x}", scan.protocol_md5);
        let content = Arc::new(ContentIdentity {
            sha256: scan.sha256,
            verification_block_bytes,
            block_sha256: Arc::from(scan.block_sha256),
        });
        let path_generation =
            FileGeneration::from_metadata(&std::fs::metadata(&path).map_err(|e| {
                MeowError::from_io(
                    format!("stat upload source path failed: {}", path.display()),
                    e,
                )
            })?);
        if path_generation != generation {
            Self::verify_visible_path_content(&path, before.len(), &content, true)?;
        }
        Ok(Self {
            path,
            state: Arc::new(Mutex::new(UploadFileState::default())),
            generation,
            sign: Arc::from(sign),
            content,
        })
    }

    pub(crate) fn sign(&self) -> &str {
        &self.sign
    }

    /// Peak temporary digest buffer used while verifying one positioned part.
    /// The parallel executor charges this in addition to the returned body so
    /// its byte semaphore reflects the actual allocation peak.
    pub(crate) fn verification_scratch_bytes(&self) -> u64 {
        self.content.verification_block_bytes as u64
    }

    pub(crate) fn validate_total_size(&self, expected_len: u64) -> Result<(), MeowError> {
        // Preserve the established short-source failure timing: fully readable
        // leading chunks may be sent, then the first short range fails and the
        // provider is aborted. A longer source is different: every requested
        // range could succeed and complete only a prefix while `file_sign`
        // describes the whole file, so it must be rejected before prepare.
        if self.generation.len > expected_len {
            return Err(Self::source_changed_error(
                &self.path,
                format!(
                    "source grew since task construction: expected={expected_len} actual={}",
                    self.generation.len
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_chunk_size(chunk_size: u64) -> Result<usize, MeowError> {
        usize::try_from(chunk_size).map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::IoError,
                "upload chunk size does not fit this target architecture",
            )
        })
    }

    fn opened_file_blocking(&self) -> Result<Arc<File>, MeowError> {
        let mut state = self.state.lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::LockPoisoned,
                "upload source handle lock poisoned",
            )
        })?;
        if let Some(file) = state.file.as_ref() {
            return Ok(Arc::clone(file));
        }
        let file = File::open(&self.path).map_err(|e| {
            let code = if e.kind() == io::ErrorKind::NotFound {
                InnerErrorCode::LocalFileRemoved
            } else {
                InnerErrorCode::IoError
            };
            MeowError::from_source(
                code,
                format!("reopen upload source failed: {}", self.path.display()),
                e,
            )
        })?;
        let opened_metadata = file.metadata().map_err(|e| {
            MeowError::from_io(
                format!(
                    "stat reopened upload source failed: {}",
                    self.path.display()
                ),
                e,
            )
        })?;
        if !opened_metadata.file_type().is_file() || opened_metadata.len() != self.generation.len {
            return Err(Self::source_changed_error(
                &self.path,
                "reopened source length or type changed",
            ));
        }
        let opened_generation = FileGeneration::from_metadata(&opened_metadata);
        // Metadata alone cannot prove that a later open still names the bytes
        // that produced `file_sign` (timestamps can collide or be restored),
        // while a metadata mismatch may merely be an identical replacement.
        // Content is therefore authoritative before publishing this stable
        // handle to active readers.
        let reopened_scan = scan_file_at(
            &file,
            self.generation.len,
            self.content.verification_block_bytes,
        )
        .map_err(|e| {
            Self::map_source_io_error(
                &self.path,
                format!(
                    "re-hash reopened upload source failed: {}",
                    self.path.display()
                ),
                e,
            )
        })?;
        let after_generation = FileGeneration::from_metadata(&file.metadata().map_err(|e| {
            MeowError::from_io(
                format!(
                    "re-stat reopened upload source failed: {}",
                    self.path.display()
                ),
                e,
            )
        })?);
        if after_generation != opened_generation {
            return Err(Self::source_changed_error(
                &self.path,
                "metadata changed while re-hashing the reopened source",
            ));
        }
        if !self.content.matches(&reopened_scan) {
            return Err(Self::content_changed_error(
                &self.path,
                "reopened source content no longer matches its SHA-256 identity",
            ));
        }
        let file = Arc::new(file);
        state.file = Some(Arc::clone(&file));
        state.accepted_handle_generation = Some(opened_generation.clone());
        state.accepted_path_generation = Some(opened_generation);
        Ok(file)
    }

    pub(crate) fn release_handle(&self) {
        match self.state.lock() {
            Ok(mut state) => {
                *state = UploadFileState::default();
            }
            Err(poisoned) => {
                *poisoned.into_inner() = UploadFileState::default();
            }
        }
    }

    #[cfg(test)]
    fn handle_is_open_for_test(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.file.is_some())
            .unwrap_or(false)
    }

    #[cfg(test)]
    fn verification_read_bytes_for_test(&self, offset: u64, len: u64) -> Result<u64, MeowError> {
        let end = offset.checked_add(len).ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::InvalidRange, "upload read range overflow")
        })?;
        if end > self.generation.len {
            return Err(MeowError::from_code_str(
                InnerErrorCode::InvalidRange,
                "upload read exceeds source snapshot",
            ));
        }
        if len == 0 {
            return Ok(0);
        }
        let block_bytes = self.content.verification_block_bytes as u64;
        let first_block_start = (offset / block_bytes) * block_bytes;
        let last_block = (end - 1) / block_bytes;
        let covered_end = last_block
            .checked_add(1)
            .and_then(|block| block.checked_mul(block_bytes))
            .unwrap_or(self.generation.len)
            .min(self.generation.len);
        Ok(covered_end - first_block_start)
    }

    pub(crate) async fn read_exact_at(&self, offset: u64, len: u64) -> Result<Bytes, MeowError> {
        let end = offset.checked_add(len).ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::InvalidRange, "upload read range overflow")
        })?;
        if end > self.generation.len {
            return Err(Self::source_changed_error(
                &self.path,
                format!(
                    "upload read exceeds hashed source generation: offset={offset} len={len} total={}",
                    self.generation.len
                ),
            ));
        }
        let len = usize::try_from(len).map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::IoError,
                "upload read length does not fit this target architecture",
            )
        })?;
        let snapshot = self.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let file = snapshot.opened_file_blocking()?;
            let buffer = snapshot.read_verified_range_blocking(&file, offset, len, &path)?;
            Ok(Bytes::from(buffer))
        })
        .await
        .map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::IoError,
                format!("upload positioned-read worker failed: {e}"),
            )
        })?
    }

    fn read_verified_range_blocking(
        &self,
        file: &File,
        offset: u64,
        len: usize,
        path: &Path,
    ) -> Result<Vec<u8>, MeowError> {
        let mut output = Vec::new();
        output.try_reserve_exact(len).map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::IoError,
                format!("cannot allocate upload part buffer: {e}"),
            )
        })?;
        if len == 0 {
            return Ok(output);
        }

        let end = offset.checked_add(len as u64).ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::InvalidRange, "upload read range overflow")
        })?;
        let block_bytes = self.content.verification_block_bytes as u64;
        let first_block = offset / block_bytes;
        let last_block = (end - 1) / block_bytes;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(self.content.verification_block_bytes)
            .map_err(|e| {
                MeowError::from_code(
                    InnerErrorCode::IoError,
                    format!("cannot allocate upload digest block buffer: {e}"),
                )
            })?;

        for block_index in first_block..=last_block {
            let block_start = block_index.checked_mul(block_bytes).ok_or_else(|| {
                MeowError::from_code_str(
                    InnerErrorCode::InvalidRange,
                    "upload digest block offset overflow",
                )
            })?;
            let block_len = usize::try_from((self.generation.len - block_start).min(block_bytes))
                .map_err(|_| {
                MeowError::from_code_str(
                    InnerErrorCode::IoError,
                    "upload digest block length does not fit this target architecture",
                )
            })?;
            scratch.resize(block_len, 0);
            read_exact_at(file, block_start, &mut scratch).map_err(|e| {
                Self::map_source_io_error(
                    path,
                    format!(
                        "positioned upload read failed: path={} offset={offset} len={len}",
                        path.display()
                    ),
                    e,
                )
            })?;

            let actual = sha256_bytes(&scratch);
            let expected = usize::try_from(block_index)
                .ok()
                .and_then(|index| self.content.block_sha256.get(index))
                .ok_or_else(|| {
                    MeowError::from_code_str(
                        InnerErrorCode::InvalidTaskState,
                        "upload content identity is missing a digest block",
                    )
                })?;
            if &actual != expected {
                return Err(Self::content_changed_error(
                    path,
                    format!("positioned read no longer matches digest block {block_index}"),
                ));
            }

            let copy_start = offset.max(block_start) - block_start;
            let block_end = block_start + block_len as u64;
            let copy_end = end.min(block_end) - block_start;
            output.extend_from_slice(&scratch[copy_start as usize..copy_end as usize]);
        }
        debug_assert_eq!(output.len(), len);
        Ok(output)
    }

    /// Validates that the stable handle and visible path still represent the
    /// initial content identity. Metadata is only a cheap change detector: an
    /// identical replacement is accepted after SHA-256 verification. On
    /// terminal completion the stable handle is always fully re-hashed.
    pub(crate) async fn validate_generation(&self, verify_content: bool) -> Result<(), MeowError> {
        let snapshot = self.clone();
        tokio::task::spawn_blocking(move || snapshot.validate_generation_blocking(verify_content))
            .await
            .map_err(|e| {
                MeowError::from_code(
                    InnerErrorCode::IoError,
                    format!("upload source validation worker failed: {e}"),
                )
            })?
    }

    fn validate_generation_blocking(&self, verify_content: bool) -> Result<(), MeowError> {
        let file = self.opened_file_blocking()?;
        let handle_generation = FileGeneration::from_metadata(&file.metadata().map_err(|e| {
            MeowError::from_io(
                format!("stat opened upload source failed: {}", self.path.display()),
                e,
            )
        })?);
        let path_metadata = std::fs::metadata(&self.path).map_err(|e| {
            MeowError::from_source(
                InnerErrorCode::LocalFileRemoved,
                format!("upload source path disappeared: {}", self.path.display()),
                e,
            )
        })?;
        let path_generation = FileGeneration::from_metadata(&path_metadata);

        let (accepted_handle, accepted_path) = {
            let state = self.state.lock().map_err(|_| {
                MeowError::from_code_str(
                    InnerErrorCode::LockPoisoned,
                    "upload source handle lock poisoned",
                )
            })?;
            (
                state.accepted_handle_generation.clone(),
                state.accepted_path_generation.clone(),
            )
        };

        if verify_content || accepted_handle.as_ref() != Some(&handle_generation) {
            let current = scan_file_stably(
                &file,
                self.generation.len,
                self.content.verification_block_bytes,
                &self.path,
                "opened",
            )?;
            if !self.content.matches(&current) {
                let detail = if verify_content {
                    "upload source content changed before completion"
                } else {
                    "opened source content changed after its metadata changed"
                };
                return Err(Self::content_changed_error(&self.path, detail));
            }
            self.accept_handle_generation(handle_generation.clone())?;
        }

        // Metadata equality is only a non-terminal fast path. At completion the
        // visible path is always opened and hashed: on platforms without a
        // stable std file identifier, a replacement can otherwise reproduce the
        // same length/timestamps/permissions tuple and bypass content identity.
        if visible_path_requires_hash(
            verify_content,
            &path_generation,
            &handle_generation,
            accepted_path.as_ref(),
        ) {
            Self::verify_visible_path_content(
                &self.path,
                self.generation.len,
                &self.content,
                false,
            )?;
            self.accept_path_generation(path_generation)?;
        } else if path_generation == handle_generation
            && accepted_path.as_ref() != Some(&path_generation)
        {
            self.accept_path_generation(path_generation)?;
        }
        Ok(())
    }

    fn accept_handle_generation(&self, generation: FileGeneration) -> Result<(), MeowError> {
        let mut state = self.state.lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::LockPoisoned,
                "upload source handle lock poisoned",
            )
        })?;
        state.accepted_handle_generation = Some(generation);
        Ok(())
    }

    fn accept_path_generation(&self, generation: FileGeneration) -> Result<(), MeowError> {
        let mut state = self.state.lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::LockPoisoned,
                "upload source handle lock poisoned",
            )
        })?;
        state.accepted_path_generation = Some(generation);
        Ok(())
    }

    fn verify_visible_path_content(
        path: &Path,
        expected_len: u64,
        content: &ContentIdentity,
        initial_scan: bool,
    ) -> Result<(), MeowError> {
        let file = File::open(path).map_err(|e| {
            let code = if initial_scan && e.kind() == io::ErrorKind::NotFound {
                InnerErrorCode::FileNotFound
            } else if e.kind() == io::ErrorKind::NotFound {
                InnerErrorCode::LocalFileRemoved
            } else {
                InnerErrorCode::IoError
            };
            MeowError::from_source(
                code,
                format!("open visible upload source failed: {}", path.display()),
                e,
            )
        })?;
        let metadata = file.metadata().map_err(|e| {
            MeowError::from_io(
                format!("stat visible upload source failed: {}", path.display()),
                e,
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(Self::source_changed_error(
                path,
                "visible upload source is not a regular file",
            ));
        }
        if metadata.len() != expected_len {
            return Err(Self::source_changed_error(
                path,
                format!(
                    "visible upload source length changed: expected={expected_len} actual={}",
                    metadata.len()
                ),
            ));
        }
        let scan = scan_file_stably(
            &file,
            expected_len,
            content.verification_block_bytes,
            path,
            "visible",
        )?;
        if !content.matches(&scan) {
            return Err(Self::content_changed_error(
                path,
                "visible upload source has different content",
            ));
        }
        Ok(())
    }

    fn source_changed_error(path: &Path, detail: impl std::fmt::Display) -> MeowError {
        MeowError::from_code(
            InnerErrorCode::LocalFileRemoved,
            format!("upload source changed: path={} ({detail})", path.display()),
        )
    }

    fn content_changed_error(path: &Path, detail: impl std::fmt::Display) -> MeowError {
        MeowError::from_code(
            InnerErrorCode::ChecksumMismatch,
            format!(
                "upload source content changed: path={} ({detail})",
                path.display()
            ),
        )
    }

    fn map_source_io_error(path: &Path, context: String, error: io::Error) -> MeowError {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            Self::source_changed_error(path, format!("source truncated during I/O: {context}"))
        } else {
            MeowError::from_io(context, error)
        }
    }
}

fn scan_file_stably(
    file: &File,
    len: u64,
    verification_block_bytes: usize,
    path: &Path,
    description: &str,
) -> Result<ContentScan, MeowError> {
    let before = FileGeneration::from_metadata(&file.metadata().map_err(|e| {
        MeowError::from_io(
            format!(
                "stat {description} upload source failed: {}",
                path.display()
            ),
            e,
        )
    })?);
    if before.len != len {
        return Err(UploadFileSnapshot::source_changed_error(
            path,
            format!("{description} source length changed before content validation"),
        ));
    }
    let scan = scan_file_at(file, len, verification_block_bytes).map_err(|e| {
        UploadFileSnapshot::map_source_io_error(
            path,
            format!(
                "hash {description} upload source failed: {}",
                path.display()
            ),
            e,
        )
    })?;
    let after = FileGeneration::from_metadata(&file.metadata().map_err(|e| {
        MeowError::from_io(
            format!(
                "re-stat {description} upload source failed: {}",
                path.display()
            ),
            e,
        )
    })?);
    if after != before {
        return Err(UploadFileSnapshot::source_changed_error(
            path,
            format!("{description} source metadata changed while validating content"),
        ));
    }
    Ok(scan)
}

fn scan_file_at(file: &File, len: u64, verification_block_bytes: usize) -> io::Result<ContentScan> {
    if verification_block_bytes == 0 || verification_block_bytes > HASH_BUFFER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload verification block size is outside the supported range",
        ));
    }
    let mut protocol_md5 = md5::Context::new();
    let mut full_sha256 = Sha256::new();
    let mut block_sha256_hasher = Sha256::new();
    let mut block_filled = 0_usize;
    let block_count = len.div_ceil(verification_block_bytes as u64);
    if block_count > MAX_UPLOAD_DIGEST_BLOCKS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "upload requires {block_count} digest blocks, exceeding limit {MAX_UPLOAD_DIGEST_BLOCKS}"
            ),
        ));
    }
    let block_capacity = usize::try_from(block_count)
        .map_err(|_| io::Error::other("upload digest block count does not fit in memory"))?;
    let mut block_sha256 = Vec::new();
    block_sha256
        .try_reserve_exact(block_capacity)
        .map_err(|e| io::Error::other(format!("cannot allocate upload digest blocks: {e}")))?;
    let mut buffer = vec![0; HASH_BUFFER_BYTES];
    let mut offset = 0_u64;
    while offset < len {
        let remaining = len - offset;
        let want = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "upload hash range overflow")
        })?;
        read_exact_at(file, offset, &mut buffer[..want])?;
        protocol_md5.consume(&buffer[..want]);
        full_sha256.update(&buffer[..want]);
        let mut cursor = 0_usize;
        while cursor < want {
            let take = (verification_block_bytes - block_filled).min(want - cursor);
            block_sha256_hasher.update(&buffer[cursor..cursor + take]);
            block_filled += take;
            cursor += take;
            if block_filled == verification_block_bytes {
                block_sha256.push(block_sha256_hasher.finalize_reset().into());
                block_filled = 0;
            }
        }
        offset = offset.checked_add(want as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "upload hash offset overflow")
        })?;
    }
    if block_filled != 0 {
        block_sha256.push(block_sha256_hasher.finalize().into());
    }
    debug_assert_eq!(block_sha256.len(), block_capacity);
    Ok(ContentScan {
        protocol_md5: protocol_md5.compute(),
        sha256: full_sha256.finalize().into(),
        block_sha256,
    })
}

fn sha256_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256::digest(bytes).into()
}

fn read_exact_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
    let mut filled = 0_usize;
    while filled < buffer.len() {
        let current = offset.checked_add(filled as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "positioned read offset overflow",
            )
        })?;
        match read_at(file, current, &mut buffer[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "upload source ended during positioned read",
                ));
            }
            Ok(read) => filled += read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    use std::io::{Read, Seek};
    let mut cloned = file.try_clone()?;
    cloned.seek(io::SeekFrom::Start(offset))?;
    cloned.read(buffer)
}

#[cfg(test)]
mod tests {
    use super::{visible_path_requires_hash, FileGeneration, UploadFileSnapshot};

    fn temp_path(case: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rusty_cat_upload_snapshot_{case}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        path
    }

    #[test]
    fn completion_never_treats_equal_metadata_as_visible_content_proof() {
        let path = temp_path("equal_metadata_requires_hash");
        std::fs::write(&path, b"fixture").expect("write fixture");
        let generation =
            FileGeneration::from_metadata(&std::fs::metadata(&path).expect("metadata"));

        assert!(!visible_path_requires_hash(
            false,
            &generation,
            &generation,
            Some(&generation),
        ));
        assert!(visible_path_requires_hash(
            true,
            &generation,
            &generation,
            Some(&generation),
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn upload_digest_table_has_an_explicit_part_count_limit() {
        let path = temp_path("digest_table_limit");
        std::fs::write(&path, b"").expect("write fixture");
        let file = std::fs::File::open(&path).expect("open fixture");

        let error = super::scan_file_at(&file, super::MAX_UPLOAD_DIGEST_BLOCKS + 1, 1)
            .err()
            .expect("pathological digest grids must fail before allocation or file I/O");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("exceeding limit"));

        let _ = std::fs::remove_file(path);
    }

    fn scan_fixture(
        case: &str,
        payload: &[u8],
        verification_block_bytes: usize,
    ) -> super::ContentScan {
        let path = temp_path(case);
        std::fs::write(&path, payload).expect("write scan fixture");
        let file = std::fs::File::open(&path).expect("open scan fixture");
        let scan = super::scan_file_at(&file, payload.len() as u64, verification_block_bytes)
            .expect("scan fixture");
        let _ = std::fs::remove_file(path);
        scan
    }

    fn assert_full_digests(scan: &super::ContentScan, payload: &[u8]) {
        use sha2::Digest as _;

        assert_eq!(
            format!("{:x}", scan.protocol_md5),
            format!("{:x}", md5::compute(payload))
        );
        assert_eq!(scan.sha256, <[u8; 32]>::from(sha2::Sha256::digest(payload)));
    }

    #[test]
    fn empty_upload_scan_has_canonical_digests_and_no_blocks() {
        let scan = scan_fixture("empty_scan", b"", 1);

        assert_full_digests(&scan, b"");
        assert!(scan.block_sha256.is_empty());
    }

    #[test]
    fn single_byte_upload_scan_has_one_exact_block() {
        let payload = b"x";
        let scan = scan_fixture("single_byte_scan", payload, 1);

        assert_full_digests(&scan, payload);
        assert_eq!(scan.block_sha256.as_slice(), &[scan.sha256]);
    }

    #[test]
    fn exact_hash_buffer_upload_scan_has_no_spurious_tail_block() {
        let payload: Vec<u8> = (0..=255).cycle().take(super::HASH_BUFFER_BYTES).collect();
        let scan = scan_fixture("exact_hash_buffer_scan", &payload, super::HASH_BUFFER_BYTES);

        assert_full_digests(&scan, &payload);
        assert_eq!(scan.block_sha256.as_slice(), &[scan.sha256]);
    }

    #[test]
    fn upload_scan_hashes_the_final_partial_block() {
        use sha2::Digest as _;

        let payload: Vec<u8> = (0..=255)
            .cycle()
            .take(super::HASH_BUFFER_BYTES + 17)
            .collect();
        let scan = scan_fixture("partial_tail_scan", &payload, super::HASH_BUFFER_BYTES);

        assert_full_digests(&scan, &payload);
        assert_eq!(scan.block_sha256.len(), 2);
        assert_eq!(
            scan.block_sha256[1],
            <[u8; 32]>::from(sha2::Sha256::digest(&payload[super::HASH_BUFFER_BYTES..]))
        );
    }

    fn replace_visible_path(replacement: &std::path::Path, path: &std::path::Path) {
        #[cfg(windows)]
        std::fs::remove_file(path).expect("remove old visible path");
        std::fs::rename(replacement, path).expect("replace visible path");
    }

    #[tokio::test]
    async fn initial_scan_preserves_protocol_md5_full_sha256_and_block_digest() {
        let path = temp_path("identity_algorithms");
        std::fs::write(&path, b"abc").expect("write fixture");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), 3)
            .await
            .expect("snapshot");

        assert_eq!(snapshot.sign(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            snapshot.content.sha256,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
        assert_eq!(snapshot.content.block_sha256.len(), 1);
        assert_eq!(snapshot.content.block_sha256[0], snapshot.content.sha256);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn small_upload_chunks_bound_digest_verification_read_amplification() {
        let path = temp_path("small_verification_blocks");
        let original: Vec<u8> = (0..=255).cycle().take(4097).collect();
        std::fs::write(&path, &original).expect("write fixture");
        let snapshot = UploadFileSnapshot::open_and_hash_with_verification_block_bytes(
            path.clone(),
            original.len() as u64,
            1024,
        )
        .await
        .expect("snapshot");

        assert_eq!(snapshot.content.verification_block_bytes, 1024);
        assert_eq!(snapshot.content.block_sha256.len(), 5);
        assert_eq!(
            snapshot
                .verification_read_bytes_for_test(1, 1024)
                .expect("verification span"),
            2048
        );
        assert_eq!(
            &snapshot
                .read_exact_at(1, 1024)
                .await
                .expect("unaligned verified read")[..],
            &original[1..1025]
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn positioned_reads_are_offset_independent_and_byte_exact() {
        let path = temp_path("positioned");
        let payload: Vec<u8> = (0..=255).cycle().take(4096).collect();
        std::fs::write(&path, &payload).expect("write fixture");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), payload.len() as u64)
            .await
            .expect("snapshot");
        assert!(
            !snapshot.handle_is_open_for_test(),
            "enqueue hashing must not retain one descriptor per queued task"
        );

        let mut joins = Vec::new();
        for offset in [3072_u64, 0, 2048, 1024] {
            let snapshot = snapshot.clone();
            joins.push(tokio::spawn(async move {
                (offset, snapshot.read_exact_at(offset, 1024).await)
            }));
        }
        for join in joins {
            let (offset, bytes) = join.await.expect("join");
            let bytes = bytes.expect("read");
            assert_eq!(
                &bytes[..],
                &payload[offset as usize..offset as usize + 1024]
            );
        }
        assert!(snapshot.handle_is_open_for_test());
        snapshot.release_handle();
        assert!(!snapshot.handle_is_open_for_test());
        assert_eq!(
            &snapshot.read_exact_at(0, 16).await.unwrap()[..],
            &payload[..16]
        );
        assert!(
            snapshot.handle_is_open_for_test(),
            "a resumed run reopens lazily"
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn same_length_different_content_replacement_is_checksum_mismatch_but_handle_stays_original(
    ) {
        let path = temp_path("replace");
        let replacement = temp_path("replacement");
        let original = vec![b'A'; 4096];
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        // Start the active generation before replacing the visible path. Reads
        // during this run stay on the stable handle, while validation fails.
        assert_eq!(
            &snapshot
                .read_exact_at(0, 32)
                .await
                .expect("open generation")[..],
            &original[..32]
        );
        std::fs::write(&replacement, vec![b'B'; original.len()]).expect("write replacement");
        replace_visible_path(&replacement, &path);

        let err = snapshot
            .validate_generation(false)
            .await
            .expect_err("replacement must fail validation");
        assert_eq!(
            err.code(),
            crate::error::InnerErrorCode::ChecksumMismatch as i32
        );
        assert_eq!(
            &snapshot.read_exact_at(0, 32).await.expect("stable handle")[..],
            &original[..32]
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn resume_reopen_maps_same_length_content_mismatch_to_checksum_mismatch() {
        let path = temp_path("different_content_resume");
        let replacement = temp_path("different_content_resume_replacement");
        let original = vec![b'A'; 4096];
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        assert!(!snapshot.handle_is_open_for_test());

        std::fs::write(&replacement, vec![b'B'; original.len()]).expect("write replacement");
        replace_visible_path(&replacement, &path);

        let error = snapshot
            .read_exact_at(0, 32)
            .await
            .expect_err("a reopened generation with different SHA-256 must be rejected");
        assert_eq!(
            error.code(),
            crate::error::InnerErrorCode::ChecksumMismatch as i32
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn missing_visible_path_remains_local_file_removed() {
        let path = temp_path("missing_visible_path");
        let original = vec![b'A'; 4096];
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        snapshot
            .read_exact_at(0, 32)
            .await
            .expect("open stable handle");
        std::fs::remove_file(&path).expect("remove visible path");

        let error = snapshot
            .validate_generation(true)
            .await
            .expect_err("missing visible path must fail completion");
        assert_eq!(
            error.code(),
            crate::error::InnerErrorCode::LocalFileRemoved as i32
        );
    }

    #[tokio::test]
    async fn visible_path_type_and_length_changes_remain_local_file_removed() {
        for case in ["type", "length"] {
            let path = temp_path(case);
            let replacement = temp_path(&format!("{case}_replacement"));
            let original = vec![b'A'; 4096];
            std::fs::write(&path, &original).expect("write original");
            let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
                .await
                .expect("snapshot");
            snapshot
                .read_exact_at(0, 32)
                .await
                .expect("open stable handle");

            if case == "type" {
                std::fs::remove_file(&path).expect("remove visible file");
                std::fs::create_dir(&path).expect("replace visible path with directory");
            } else {
                std::fs::write(&replacement, vec![b'B'; original.len() + 1])
                    .expect("write longer replacement");
                replace_visible_path(&replacement, &path);
            }

            let error = snapshot
                .validate_generation(true)
                .await
                .expect_err("path metadata change must fail completion");
            assert_eq!(
                error.code(),
                crate::error::InnerErrorCode::LocalFileRemoved as i32,
                "{case}"
            );

            if case == "type" {
                let _ = std::fs::remove_dir(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    #[tokio::test]
    async fn same_content_path_replacement_is_allowed_by_content_identity() {
        let path = temp_path("same_content_replace");
        let replacement = temp_path("same_content_replacement");
        let original: Vec<u8> = (0..=255).cycle().take(4096).collect();
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        assert_eq!(
            &snapshot
                .read_exact_at(0, 32)
                .await
                .expect("open generation")[..],
            &original[..32]
        );

        std::fs::write(&replacement, &original).expect("write identical replacement");
        replace_visible_path(&replacement, &path);

        snapshot
            .validate_generation(false)
            .await
            .expect("identical bytes preserve the upload content identity");
        snapshot
            .validate_generation(true)
            .await
            .expect("completion accepts an identical visible replacement");
        assert_eq!(
            &snapshot.read_exact_at(0, 32).await.expect("stable handle")[..],
            &original[..32]
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn same_content_replacement_is_accepted_when_resume_reopens_the_path() {
        let path = temp_path("same_content_resume");
        let replacement = temp_path("same_content_resume_replacement");
        let original: Vec<u8> = (0..=255).rev().cycle().take(4096).collect();
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        assert!(!snapshot.handle_is_open_for_test());

        std::fs::write(&replacement, &original).expect("write identical replacement");
        replace_visible_path(&replacement, &path);

        assert_eq!(
            &snapshot
                .read_exact_at(100, 257)
                .await
                .expect("content-equivalent generation may be reopened")[..],
            &original[100..357]
        );
        snapshot
            .validate_generation(true)
            .await
            .expect("reopened stable handle still has the initial content identity");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn positioned_read_rejects_transient_change_even_if_bytes_are_restored_before_complete() {
        let path = temp_path("transient_change");
        let original = vec![b'A'; 4096];
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        snapshot
            .validate_generation(false)
            .await
            .expect("open stable handle");

        std::fs::write(&path, vec![b'B'; original.len()]).expect("transient rewrite");
        let error = snapshot
            .read_exact_at(0, original.len() as u64)
            .await
            .expect_err("bytes captured for sending must be checked against the initial scan");
        assert_eq!(
            error.code(),
            crate::error::InnerErrorCode::ChecksumMismatch as i32
        );

        std::fs::write(&path, &original).expect("restore original bytes");
        snapshot
            .validate_generation(true)
            .await
            .expect("a final-only hash would miss the transient bad read");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn positioned_read_verifies_unaligned_and_final_partial_digest_boundaries() {
        let len = super::HASH_BUFFER_BYTES * 2 + 37;
        let path = temp_path("digest_boundaries");
        let original: Vec<u8> = (0..=250).cycle().take(len).collect();
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        snapshot
            .validate_generation(false)
            .await
            .expect("open stable handle");

        for (case, offset, read_len, changed_at) in [
            (
                "cross_block",
                super::HASH_BUFFER_BYTES as u64 - 7,
                19_u64,
                super::HASH_BUFFER_BYTES as u64,
            ),
            ("final_partial", (len - 19) as u64, 19_u64, (len - 1) as u64),
        ] {
            let mut changed = original.clone();
            changed[changed_at as usize] ^= 0xff;
            std::fs::write(&path, &changed).expect("rewrite boundary byte");
            let error = snapshot
                .read_exact_at(offset, read_len)
                .await
                .expect_err(case);
            assert_eq!(
                error.code(),
                crate::error::InnerErrorCode::ChecksumMismatch as i32,
                "{case}"
            );
            std::fs::write(&path, &original).expect("restore fixture");
        }

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn complete_validation_detects_in_place_content_change() {
        let path = temp_path("in_place");
        let original = vec![b'A'; 4096];
        std::fs::write(&path, &original).expect("write original");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");
        snapshot
            .validate_generation(false)
            .await
            .expect("open active generation");
        std::fs::write(&path, vec![b'B'; original.len()]).expect("rewrite");

        snapshot
            .validate_generation(false)
            .await
            .expect_err("metadata change must be detected");
        let err = snapshot
            .validate_generation(true)
            .await
            .expect_err("content change must be detected before complete");
        assert_eq!(
            err.code(),
            crate::error::InnerErrorCode::ChecksumMismatch as i32
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn reopen_rehash_rejects_content_even_if_metadata_snapshot_matches() {
        let path = temp_path("metadata_collision");
        let original = vec![b'A'; 4096];
        std::fs::write(&path, &original).expect("write original");
        let mut snapshot = UploadFileSnapshot::open_and_hash(path.clone(), original.len() as u64)
            .await
            .expect("snapshot");

        std::fs::write(&path, vec![b'B'; original.len()]).expect("rewrite source");
        // Simulate a reused/restored metadata tuple: content, not metadata,
        // remains the decisive proof when the active handle is opened.
        snapshot.generation =
            FileGeneration::from_metadata(&std::fs::metadata(&path).expect("metadata"));
        let error = snapshot
            .validate_generation(false)
            .await
            .expect_err("different bytes must not become the active generation");
        assert_eq!(
            error.code(),
            crate::error::InnerErrorCode::ChecksumMismatch as i32
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn builder_length_mismatch_is_rejected_before_upload_prepare() {
        let path = temp_path("length_mismatch");
        std::fs::write(&path, vec![b'A'; 4097]).expect("write fixture");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), 4096)
            .await
            .expect("snapshot creation preserves legacy enqueue timing");

        let err = snapshot
            .validate_total_size(4096)
            .expect_err("a stale builder length must not upload only a prefix");
        assert_eq!(
            err.code(),
            crate::error::InnerErrorCode::LocalFileRemoved as i32
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn source_shortened_before_enqueue_reports_local_file_removed_on_first_short_range() {
        let path = temp_path("short_before_enqueue");
        std::fs::write(&path, vec![b'A'; 1024]).expect("write shortened fixture");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), 2048)
            .await
            .expect("snapshot keeps legacy worker-stage length failure timing");

        let error = snapshot
            .read_exact_at(1024, 1024)
            .await
            .expect_err("range beyond the hashed generation must fail");
        assert_eq!(
            error.code(),
            crate::error::InnerErrorCode::LocalFileRemoved as i32
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn source_truncated_after_validation_reports_local_file_removed_during_read() {
        let path = temp_path("truncate_after_validation");
        std::fs::write(&path, vec![b'A'; 4096]).expect("write fixture");
        let snapshot = UploadFileSnapshot::open_and_hash(path.clone(), 4096)
            .await
            .expect("snapshot");
        snapshot
            .validate_generation(false)
            .await
            .expect("open and validate active generation");

        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open fixture for truncation")
            .set_len(0)
            .expect("truncate active source");
        let error = snapshot
            .read_exact_at(0, 4096)
            .await
            .expect_err("short positioned read must fail");
        assert_eq!(
            error.code(),
            crate::error::InnerErrorCode::LocalFileRemoved as i32
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn chunk_allocation_check_has_no_arbitrary_parallel_window_cap() {
        assert!(UploadFileSnapshot::validate_chunk_size(1024 * 1024).is_ok());
        if usize::BITS < 64 {
            assert!(UploadFileSnapshot::validate_chunk_size(u64::MAX).is_err());
        }
    }
}
