use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;

use crate::error::{InnerErrorCode, MeowError};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;

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
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
}

impl FileGeneration {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(windows)]
        use std::os::windows::fs::MetadataExt;

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
            #[cfg(windows)]
            volume_serial_number: metadata.volume_serial_number(),
            #[cfg(windows)]
            file_index: metadata.file_index(),
        }
    }

    fn same_file(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.dev == other.dev && self.ino == other.ino
        }
        #[cfg(windows)]
        {
            match (
                (self.volume_serial_number, self.file_index),
                (other.volume_serial_number, other.file_index),
            ) {
                (
                    (Some(left_volume), Some(left_index)),
                    (Some(right_volume), Some(right_index)),
                ) => left_volume == right_volume && left_index == right_index,
                // Some filesystems do not expose a stable Windows file ID.
                // Retain the conservative full metadata comparison there.
                _ => self == other,
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            self == other
        }
    }
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
    file: Arc<Mutex<Option<Arc<File>>>>,
    generation: FileGeneration,
    sign: Arc<str>,
}

impl std::fmt::Debug for UploadFileSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadFileSnapshot")
            .field("path", &self.path)
            .field("generation", &self.generation)
            .field("sign", &self.sign)
            .finish_non_exhaustive()
    }
}

impl UploadFileSnapshot {
    pub(crate) async fn open_and_hash(path: PathBuf, expected_len: u64) -> Result<Self, MeowError> {
        let permit = upload_hash_workers().acquire_owned().await.map_err(|_| {
            MeowError::from_code_str(InnerErrorCode::IoError, "upload hash worker limiter closed")
        })?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::open_and_hash_blocking(path, expected_len)
        })
        .await
        .map_err(|e| {
            MeowError::from_code(
                InnerErrorCode::IoError,
                format!("upload source hash worker failed: {e}"),
            )
        })?
    }

    fn open_and_hash_blocking(path: PathBuf, expected_len: u64) -> Result<Self, MeowError> {
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
        let sign = hash_file_at(&file, before.len()).map_err(|e| {
            MeowError::from_io(
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
        let path_generation =
            FileGeneration::from_metadata(&std::fs::metadata(&path).map_err(|e| {
                MeowError::from_io(
                    format!("stat upload source path failed: {}", path.display()),
                    e,
                )
            })?);
        if !generation.same_file(&path_generation) {
            return Err(Self::source_changed_error(
                &path,
                "path was replaced while calculating signature",
            ));
        }
        Ok(Self {
            path,
            file: Arc::new(Mutex::new(None)),
            generation,
            sign: Arc::from(sign),
        })
    }

    pub(crate) fn sign(&self) -> &str {
        &self.sign
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
        let mut slot = self.file.lock().map_err(|_| {
            MeowError::from_code_str(
                InnerErrorCode::LockPoisoned,
                "upload source handle lock poisoned",
            )
        })?;
        if let Some(file) = slot.as_ref() {
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
        let opened_generation = FileGeneration::from_metadata(&file.metadata().map_err(|e| {
            MeowError::from_io(
                format!(
                    "stat reopened upload source failed: {}",
                    self.path.display()
                ),
                e,
            )
        })?);
        if opened_generation != self.generation {
            return Err(Self::source_changed_error(
                &self.path,
                "reopened source no longer matches the hashed generation",
            ));
        }
        // Metadata alone cannot prove that a later open still names the bytes
        // that produced `file_sign` (timestamps can collide or be restored).
        // Revalidate content before publishing this handle to active readers.
        let reopened_sign = hash_file_at(&file, self.generation.len).map_err(|e| {
            MeowError::from_io(
                format!(
                    "re-hash reopened upload source failed: {}",
                    self.path.display()
                ),
                e,
            )
        })?;
        if reopened_sign != self.sign.as_ref() {
            return Err(MeowError::from_code(
                InnerErrorCode::ChecksumMismatch,
                format!(
                    "reopened upload source content no longer matches its file sign: {}",
                    self.path.display()
                ),
            ));
        }
        let file = Arc::new(file);
        *slot = Some(Arc::clone(&file));
        Ok(file)
    }

    pub(crate) fn release_handle(&self) {
        match self.file.lock() {
            Ok(mut slot) => {
                slot.take();
            }
            Err(poisoned) => {
                poisoned.into_inner().take();
            }
        }
    }

    #[cfg(test)]
    fn handle_is_open_for_test(&self) -> bool {
        self.file.lock().map(|slot| slot.is_some()).unwrap_or(false)
    }

    pub(crate) async fn read_exact_at(&self, offset: u64, len: u64) -> Result<Bytes, MeowError> {
        let end = offset.checked_add(len).ok_or_else(|| {
            MeowError::from_code_str(InnerErrorCode::InvalidRange, "upload read range overflow")
        })?;
        if end > self.generation.len {
            return Err(MeowError::from_code(
                InnerErrorCode::InvalidRange,
                format!(
                    "upload read exceeds source snapshot: offset={offset} len={len} total={}",
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
            let mut buffer = Vec::new();
            buffer.try_reserve(len).map_err(|e| {
                MeowError::from_code(
                    InnerErrorCode::IoError,
                    format!("cannot allocate upload part buffer: {e}"),
                )
            })?;
            buffer.resize(len, 0);
            read_exact_at(&file, offset, &mut buffer).map_err(|e| {
                MeowError::from_io(
                    format!(
                        "positioned upload read failed: path={} offset={offset} len={len}",
                        path.display()
                    ),
                    e,
                )
            })?;
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

    /// Validates that the path still names the same opened file. On terminal
    /// completion, also re-hashes the stable handle so in-place changes cannot
    /// result in a remote object whose bytes disagree with the public sign.
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
        if verify_content {
            let current = hash_file_at(&file, self.generation.len).map_err(|e| {
                MeowError::from_io(
                    format!("re-hash upload source failed: {}", self.path.display()),
                    e,
                )
            })?;
            if current != self.sign.as_ref() {
                return Err(MeowError::from_code(
                    InnerErrorCode::ChecksumMismatch,
                    format!(
                        "upload source content changed before completion: {}",
                        self.path.display()
                    ),
                ));
            }
        }
        let handle_generation = FileGeneration::from_metadata(&file.metadata().map_err(|e| {
            MeowError::from_io(
                format!("stat opened upload source failed: {}", self.path.display()),
                e,
            )
        })?);
        if handle_generation != self.generation {
            return Err(Self::source_changed_error(
                &self.path,
                "opened source metadata no longer matches hashed generation",
            ));
        }
        let path_metadata = std::fs::metadata(&self.path).map_err(|e| {
            MeowError::from_source(
                InnerErrorCode::LocalFileRemoved,
                format!("upload source path disappeared: {}", self.path.display()),
                e,
            )
        })?;
        let path_generation = FileGeneration::from_metadata(&path_metadata);
        if !self.generation.same_file(&path_generation) {
            return Err(Self::source_changed_error(
                &self.path,
                "upload source path now identifies a different file",
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
}

fn hash_file_at(file: &File, len: u64) -> io::Result<String> {
    let mut hasher = md5::Context::new();
    let mut buffer = vec![0; HASH_BUFFER_BYTES];
    let mut offset = 0_u64;
    while offset < len {
        let remaining = len - offset;
        let want = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "upload hash range overflow")
        })?;
        let read = read_at(file, offset, &mut buffer[..want])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "upload source ended while hashing",
            ));
        }
        hasher.consume(&buffer[..read]);
        offset = offset.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "upload hash offset overflow")
        })?;
    }
    Ok(format!("{:x}", hasher.compute()))
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
    use super::{FileGeneration, UploadFileSnapshot};

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
    async fn same_length_path_replacement_is_detected_but_open_handle_stays_original() {
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
        std::fs::rename(&replacement, &path).expect("replace path");

        let err = snapshot
            .validate_generation(false)
            .await
            .expect_err("replacement must fail validation");
        assert_eq!(
            err.code(),
            crate::error::InnerErrorCode::LocalFileRemoved as i32
        );
        assert_eq!(
            &snapshot.read_exact_at(0, 32).await.expect("stable handle")[..],
            &original[..32]
        );

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
    async fn chunk_allocation_check_has_no_arbitrary_parallel_window_cap() {
        assert!(UploadFileSnapshot::validate_chunk_size(1024 * 1024).is_ok());
        if usize::BITS < 64 {
            assert!(UploadFileSnapshot::validate_chunk_size(u64::MAX).is_err());
        }
    }
}
