//! Crash-resumable progress for the concurrent download path.
//!
//! A hashed sidecar in the target's adjacent `.rusty-cat` namespace records
//! which fixed-size parts of a pre-sized download file are durably written, so
//! an interrupted concurrent download resumes by re-fetching only the missing
//! parts. The sidecar is bound to the target by `(identity, total, chunk)` and
//! by the target's on-disk length; any mismatch is treated as a fresh download.
//! Recovery verifies each completed part from disk before trusting its bit, and
//! all allocation/offset calculations return controlled errors instead of
//! panicking or saturating. The former `<target>.rcdl` location is deliberately
//! ignored: it may be an ordinary user file and is never read, replaced, or
//! deleted by this module.
//!
//! `DownloadProgress` is stored on `TransferTask` and driven by the concurrent
//! download path in `DefaultHttpTransfer` (prepare pre-sizes + loads the
//! sidecar, each part marks its bit, and complete validates + deletes it).

use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const MAGIC: u32 = 0x5243_444C; // "RCDL"
const VERSION: u16 = 2;
const SIDECAR_NAMESPACE: &str = ".rusty-cat";
const SIDECAR_NAMESPACE_MARKER: &str = ".download-state-v1";
const SIDECAR_HEADER_LEN: usize = 28;
const PART_DIGEST_LEN: usize = 32;
/// Preserve the existing million-part sidecar contract on 64-bit targets.
/// 32-bit targets use a lower, memory-derived limit below.
const MAX_PART_COUNT: u64 = 1_000_000;
/// Match the client-wide 32-bit parallel-body budget. Checkpoint state is not
/// held by that semaphore, so its own worst-case snapshot must fit this bound
/// independently instead of assuming a 64-bit address space.
const CHECKPOINT_MEMORY_LIMIT_32_BIT: u64 = 64 * 1024 * 1024;
/// Leave room for maximum-length identity/header data, Vec bookkeeping,
/// allocator size classes, and small concurrent control allocations.
const CHECKPOINT_FIXED_HEADROOM_32_BIT: u64 = 8 * 1024 * 1024;
/// Conservative logical peak per part while loading or committing a snapshot:
/// three 32-byte digest representations (resident, clone/decode, encoded/raw),
/// a pending entry, bitmap copies, and rounding headroom.
const CHECKPOINT_PEAK_BYTES_PER_PART_UPPER_BOUND: u64 = 144;
const MAX_PART_COUNT_32_BIT: u64 = (CHECKPOINT_MEMORY_LIMIT_32_BIT
    - CHECKPOINT_FIXED_HEADROOM_32_BIT)
    / CHECKPOINT_PEAK_BYTES_PER_PART_UPPER_BOUND;
const DEFAULT_CHECKPOINT_BATCH_PARTS: usize = 8;
const MAX_BATCH_CHECKPOINTS: usize = 16;
const CHECKPOINT_TIMER_MAX_PART_COUNT: usize = 4_096;
pub(crate) const DEFAULT_CHECKPOINT_INTERVAL: Duration = Duration::from_millis(250);
static CHECKPOINT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct PendingPart {
    index: usize,
    digest: [u8; PART_DIGEST_LEN],
}

pub(crate) struct StageDoneOutcome {
    #[cfg(test)]
    pub(crate) checkpointed: bool,
    /// The caller must run a data barrier and commit the pending checkpoint.
    pub(crate) checkpoint_due: bool,
    pub(crate) arm_timer: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointFailurePoint {
    BeforeDataBarrier,
    AfterDataBarrier,
    AfterSidecarFsync,
    AfterRename,
}

/// In-memory part bitmap plus its sidecar location and binding metadata.
pub(crate) struct DownloadProgress {
    /// Original download target path. Retained for provenance/debugging; the
    /// sidecar path derived from it (below) is what all I/O uses, so the field
    /// itself is currently only written, not read.
    #[allow(dead_code)]
    target: PathBuf,
    sidecar: PathBuf,
    total: u64,
    chunk: u64,
    /// Retained in the v2 on-disk header inherited from v1. It is not a
    /// content-identity field: changing the in-flight scheduler window must not
    /// discard durable parts that use the same total/chunk grid.
    max_parts: usize,
    identity: String,
    part_count: usize,
    /// One bit per part; `bits[i >> 3] & (1 << (i & 7))`.
    bits: Vec<u8>,
    /// SHA-256 for every part index. Only entries whose bit is set are trusted;
    /// keeping a fixed-width table makes a bit and its digest part of one
    /// atomically replaced sidecar snapshot.
    digests: Vec<[u8; PART_DIGEST_LEN]>,
    /// Strong validator observed during the current prepare. This is not read
    /// from disk; every process must obtain it again from the remote before any
    /// remembered bit may be reused.
    expected_validator: Option<String>,
    /// Completed writes in the current open epoch. Their bits are deliberately
    /// absent from `bits` until one data barrier and one atomic sidecar snapshot
    /// commit the whole frozen epoch.
    pending: Vec<PendingPart>,
    /// O(1) duplicate detection keeps large pending batches from becoming a
    /// quadratic scan.
    pending_bits: Vec<u8>,
    checkpoint_threshold: usize,
    last_checkpoint: Instant,
    checkpoint_timer_armed: bool,
    #[cfg(test)]
    checkpoint_count: usize,
    #[cfg(test)]
    fail_next_checkpoint: Option<CheckpointFailurePoint>,
}

/// Returns the private sidecar path for a download target.
///
/// Only the lossless platform representation of the target file name is
/// needed in the digest: the namespace is adjacent to the target, so the
/// parent directory already scopes the name. This keeps the generated
/// component short and avoids UTF-8 conversion on Unix and Windows.
pub(crate) fn sidecar_path(target: &Path) -> PathBuf {
    let component = target.file_name().unwrap_or(target.as_os_str());
    let mut hasher = Sha256::new();
    hasher.update(b"rusty-cat/download-sidecar-path/v1\0");
    update_hasher_with_os_str(&mut hasher, component);
    let digest = hasher.finalize();
    let mut name = String::with_capacity(digest.len() * 2 + ".rcdl".len());
    for byte in digest {
        use std::fmt::Write as _;
        // Formatting into a String is infallible; retaining the result keeps
        // this path helper usable from the lease layer without an I/O error.
        let _ = write!(&mut name, "{byte:02x}");
    }
    name.push_str(".rcdl");

    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.join(SIDECAR_NAMESPACE).join(name)
}

/// Rejects a visible download target that resolves through the reserved sidecar
/// namespace component, whether or not it has been claimed yet. Without this
/// guard, a later ordinary task could overwrite a checkpoint (or its atomic
/// temp file), and checkpoint cleanup could delete that visible target.
pub(crate) fn ensure_target_outside_sidecar_namespace(target: &Path) -> io::Result<()> {
    ensure_path_outside_sidecar_namespace(target)?;
    let normalized = normalize_longest_existing_prefix(target)?;
    if normalized != target {
        ensure_path_outside_sidecar_namespace(&normalized)?;
    }
    Ok(())
}

fn ensure_path_outside_sidecar_namespace(target: &Path) -> io::Result<()> {
    for ancestor in target.ancestors() {
        let Some(name) = ancestor.file_name() else {
            continue;
        };
        if !is_sidecar_namespace_component(name) {
            continue;
        }
        // Reserve the component before an ownership marker exists. Checking
        // only an already-owned marker leaves a cross-process transition race:
        // a visible target can be created after another task observes an empty
        // namespace but before that task creates the marker, after which stale
        // checkpoint cleanup may delete the visible target. Unconditional
        // reservation makes the rule independent of marker-claim timing.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "download target is inside the reserved rusty-cat checkpoint namespace: {}",
                target.display()
            ),
        ));
    }
    Ok(())
}

fn is_sidecar_namespace_component(name: &std::ffi::OsStr) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        name.to_string_lossy()
            .eq_ignore_ascii_case(SIDECAR_NAMESPACE)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        name == std::ffi::OsStr::new(SIDECAR_NAMESPACE)
    }
}

fn normalize_longest_existing_prefix(path: &Path) -> io::Result<PathBuf> {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(cursor) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // `canonicalize` reports `NotFound` both for a genuinely
                // missing component and for a dangling symbolic link. Treating
                // the latter as an ordinary missing component would let the
                // eventual create operation follow that link into the private
                // checkpoint namespace after this validation has completed.
                // Fail closed for every existing component that cannot be
                // resolved; only peel components that truly do not exist.
                match std::fs::symlink_metadata(cursor) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "download target contains a dangling or unresolvable symbolic link: {}",
                                cursor.display()
                            ),
                        ));
                    }
                    Ok(_) => return Err(error),
                    Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {}
                    Err(metadata_error) => return Err(metadata_error),
                }
                if let Some(name) = cursor.file_name() {
                    missing.push(name.to_os_string());
                    cursor = cursor
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."));
                } else {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn update_hasher_with_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    use std::os::unix::ffi::OsStrExt;
    hasher.update(value.as_bytes());
}

#[cfg(windows)]
fn update_hasher_with_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    for unit in value.encode_wide() {
        hasher.update(unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn update_hasher_with_os_str(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    hasher.update(value.as_encoded_bytes());
}

const fn part_count_limit_for_pointer_width(pointer_width: u32) -> u64 {
    if pointer_width <= 32 {
        MAX_PART_COUNT_32_BIT
    } else {
        MAX_PART_COUNT
    }
}

fn checkpoint_peak_upper_bound_bytes(part_count: u64) -> io::Result<u64> {
    part_count
        .checked_mul(CHECKPOINT_PEAK_BYTES_PER_PART_UPPER_BOUND)
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_FIXED_HEADROOM_32_BIT))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "download checkpoint peak memory estimate overflow",
            )
        })
}

fn part_count_for_pointer_width(total: u64, chunk: u64, pointer_width: u32) -> io::Result<usize> {
    if chunk == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "download chunk size must be greater than zero",
        ));
    }
    let parts = total.div_ceil(chunk);
    let part_limit = part_count_limit_for_pointer_width(pointer_width);
    if parts > part_limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "download requires {parts} parts, exceeding sidecar limit {part_limit} for {pointer_width}-bit targets"
            ),
        ));
    }
    if pointer_width <= 32
        && checkpoint_peak_upper_bound_bytes(parts)? > CHECKPOINT_MEMORY_LIMIT_32_BIT
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("download checkpoint peak exceeds the 32-bit memory budget for {parts} parts"),
        ));
    }
    usize::try_from(parts).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download part count does not fit this target architecture",
        )
    })
}

fn part_count_for(total: u64, chunk: u64) -> io::Result<usize> {
    part_count_for_pointer_width(total, chunk, usize::BITS)
}

/// Computes the only valid byte length for a v2 sidecar bound to these
/// parameters. All values derived from the sidecar itself are deliberately
/// excluded so an untrusted length cannot influence allocation.
fn expected_v2_encoded_len(total: u64, chunk: u64, identity: &str) -> io::Result<usize> {
    if identity.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "download identity is too long for the sidecar format",
        ));
    }
    let part_count = part_count_for(total, chunk)?;
    let bitmap_len = part_count.div_ceil(8);
    let digest_len = part_count.checked_mul(PART_DIGEST_LEN).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download sidecar digest length overflow",
        )
    })?;
    SIDECAR_HEADER_LEN
        .checked_add(identity.len())
        .and_then(|len| len.checked_add(bitmap_len))
        .and_then(|len| len.checked_add(digest_len))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "download sidecar encoded length overflow",
            )
        })
}

fn sidecar_namespace(sidecar: &Path) -> io::Result<&Path> {
    sidecar.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download sidecar has no namespace directory",
        )
    })
}

fn validate_namespace_directory(namespace: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(namespace)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "reserved rusty-cat download namespace is not a real directory: {}",
                namespace.display()
            ),
        ));
    }
    Ok(())
}

fn validate_namespace_marker(marker: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(marker)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "rusty-cat download namespace marker is not owned by this format: {}",
                marker.display()
            ),
        ));
    }
    Ok(())
}

/// Creates or validates the adjacent private namespace without following a
/// namespace or marker symlink. `create_dir` and `create_new` make ordinary
/// pre-existing path types fail closed instead of being replaced.
fn ensure_sidecar_namespace(sidecar: &Path) -> io::Result<()> {
    let namespace = sidecar_namespace(sidecar)?;
    match std::fs::create_dir(namespace) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_namespace_directory(namespace)?;
        }
        Err(error) => return Err(error),
    }

    let marker = namespace.join(SIDECAR_NAMESPACE_MARKER);
    match std::fs::symlink_metadata(&marker) {
        Ok(_) => return validate_namespace_marker(&marker),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Never claim a pre-existing non-empty directory. This protects unrelated
    // `.rusty-cat` directories and every ordinary file they already contain.
    // An empty directory is safe to initialize, and `create_new` below lets
    // concurrent downloads race without replacing each other's marker.
    if std::fs::read_dir(namespace)?.next().transpose()?.is_some() {
        // Another cooperative initializer may have created the marker between
        // our first lookup and the directory scan, and may already be writing a
        // different target's sidecar. Recheck ownership before classifying the
        // non-empty directory as an unrelated namespace.
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => return validate_namespace_marker(&marker),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "reserved rusty-cat download namespace is non-empty and has no ownership marker: {}",
                namespace.display()
            ),
        ));
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(file) => {
            file.sync_all()?;
            sync_parent_directory(&marker)?;
            sync_parent_directory(namespace)?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_namespace_marker(&marker)?;
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn sidecar_namespace_is_owned(sidecar: &Path) -> io::Result<bool> {
    let namespace = sidecar_namespace(sidecar)?;
    match std::fs::symlink_metadata(namespace) {
        Ok(_) => validate_namespace_directory(namespace)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let marker = namespace.join(SIDECAR_NAMESPACE_MARKER);
    match std::fs::symlink_metadata(&marker) {
        Ok(_) => {
            validate_namespace_marker(&marker)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Returns true for an absent destination and false for an owned namespace's
/// regular sidecar file. Symlinks, directories, and other path types are left
/// untouched. Namespace ownership is validated separately before this helper.
fn sidecar_destination_is_absent(sidecar: &Path) -> io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(sidecar) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "download sidecar destination is not an owned regular file: {}",
                sidecar.display()
            ),
        ));
    }

    Ok(false)
}

/// Opens and reads at most the exact expected v2 payload plus one sentinel
/// byte. The metadata check cheaply rejects oversized/sparse files, while the
/// bounded read detects truncation or growth after that check without trusting
/// the path's reported length for allocation.
fn read_sidecar_bounded(path: &Path, expected_len: usize) -> io::Result<Option<Vec<u8>>> {
    let path_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "download sidecar is not an owned regular file: {}",
                path.display()
            ),
        ));
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let expected_len_u64 = u64::try_from(expected_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download sidecar length does not fit the filesystem API",
        )
    })?;
    if file.metadata()?.len() != expected_len_u64 {
        return Ok(None);
    }
    read_sidecar_bounded_from_open_file(file, expected_len)
}

fn read_sidecar_bounded_from_open_file(
    file: std::fs::File,
    expected_len: usize,
) -> io::Result<Option<Vec<u8>>> {
    let read_limit = expected_len.checked_add(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download sidecar read limit overflow",
        )
    })?;
    let read_limit_u64 = u64::try_from(read_limit).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download sidecar read limit does not fit the filesystem API",
        )
    })?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(read_limit).map_err(|error| {
        io::Error::other(format!(
            "cannot allocate bounded download sidecar buffer: {error}"
        ))
    })?;
    file.take(read_limit_u64).read_to_end(&mut raw)?;
    if raw.len() != expected_len {
        return Ok(None);
    }
    Ok(Some(raw))
}

fn zeroed_bytes(len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|e| io::Error::other(format!("cannot allocate download sidecar bitmap: {e}")))?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn zeroed_digests(len: usize) -> io::Result<Vec<[u8; PART_DIGEST_LEN]>> {
    let mut digests = Vec::new();
    digests
        .try_reserve_exact(len)
        .map_err(|e| io::Error::other(format!("cannot allocate download part digests: {e}")))?;
    digests.resize(len, [0; PART_DIGEST_LEN]);
    Ok(digests)
}

#[cfg(test)]
fn digest_file_range(target: &Path, offset: u64, len: u64) -> io::Result<[u8; PART_DIGEST_LEN]> {
    let mut file = std::fs::File::open(target)?;
    digest_open_file_range(&mut file, offset, len)
}

fn digest_open_file_range(
    file: &mut std::fs::File,
    offset: u64,
    len: u64,
) -> io::Result<[u8; PART_DIGEST_LEN]> {
    file.seek(io::SeekFrom::Start(offset))?;
    let mut remaining = len;
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "part digest read length overflow",
            )
        })?;
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "download target ended while hashing a completed part",
            ));
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher.finalize().into())
}

impl DownloadProgress {
    /// Part index for a chunk-aligned start offset.
    fn index_of(&self, offset: u64) -> Option<usize> {
        if offset % self.chunk != 0 {
            return None;
        }
        let idx = offset / self.chunk;
        usize::try_from(idx).ok().filter(|i| *i < self.part_count)
    }

    fn part_range(&self, index: usize) -> io::Result<(u64, u64)> {
        let offset = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(self.chunk))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "part offset overflow"))?;
        let end = offset.saturating_add(self.chunk).min(self.total);
        let len = end.checked_sub(offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid download part range")
        })?;
        Ok((offset, len))
    }

    fn fresh(
        target: &Path,
        total: u64,
        chunk: u64,
        max_parts: usize,
        identity: &str,
    ) -> io::Result<Self> {
        if max_parts > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_parts_in_flight does not fit the sidecar format",
            ));
        }
        if identity.len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "download identity is too long for the sidecar format",
            ));
        }
        let part_count = part_count_for(total, chunk)?;
        let byte_len = part_count.div_ceil(8);
        let checkpoint_threshold =
            DEFAULT_CHECKPOINT_BATCH_PARTS.max(part_count.div_ceil(MAX_BATCH_CHECKPOINTS));
        Ok(Self {
            target: target.to_path_buf(),
            sidecar: sidecar_path(target),
            total,
            chunk,
            max_parts,
            identity: identity.to_string(),
            part_count,
            bits: zeroed_bytes(byte_len)?,
            digests: zeroed_digests(part_count)?,
            expected_validator: None,
            pending: Vec::new(),
            pending_bits: zeroed_bytes(byte_len)?,
            checkpoint_threshold,
            last_checkpoint: Instant::now(),
            checkpoint_timer_armed: false,
            #[cfg(test)]
            checkpoint_count: 0,
            #[cfg(test)]
            fail_next_checkpoint: None,
        })
    }

    /// Creates an all-missing bitmap when the remote supplied no strong
    /// validator. Existing sidecars are deliberately not decoded: URL + length
    /// cannot prove that the representation is still the same generation.
    pub(crate) fn create_unverified(
        target: &Path,
        total: u64,
        chunk: u64,
        max_parts: usize,
    ) -> io::Result<Self> {
        Self::fresh(target, total, chunk, max_parts, "unverified")
    }

    pub(crate) fn set_expected_validator(&mut self, validator: String) {
        self.expected_validator = Some(validator);
    }

    pub(crate) fn expected_validator(&self) -> Option<&str> {
        self.expected_validator.as_deref()
    }

    /// Loads a matching sidecar, or returns a fresh (all-missing) bitmap when
    /// none exists or it does not match this target/parameters.
    pub(crate) fn load_or_create(
        target: &Path,
        total: u64,
        chunk: u64,
        max_parts: usize,
        identity: &str,
    ) -> io::Result<Self> {
        let mut fresh = Self::fresh(target, total, chunk, max_parts, identity)?;
        // The target must already be pre-sized to `total`; otherwise remembered
        // bits cannot be trusted (file was removed/replaced/truncated).
        let target_len = match std::fs::metadata(target) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        if target_len != total {
            return Ok(fresh);
        }
        if !sidecar_namespace_is_owned(&fresh.sidecar)? {
            return Ok(fresh);
        }
        cleanup_stale_checkpoint_temps(&fresh.sidecar);
        let expected_len = expected_v2_encoded_len(total, chunk, identity)?;
        let raw = match read_sidecar_bounded(&fresh.sidecar, expected_len)? {
            Some(raw) => raw,
            None => return Ok(fresh),
        };
        match Self::decode(&raw, total, chunk, max_parts, identity) {
            Some((bits, digests)) => {
                fresh.bits = bits;
                fresh.digests = digests;
                fresh.verify_completed_parts()?;
                Ok(fresh)
            }
            None => Ok(fresh), // corrupt / mismatched header => start over
        }
    }

    /// Serial-resume variant of [`Self::load_or_create`].
    ///
    /// A serial target intentionally has `len == contiguous downloaded
    /// prefix`, so a length below `total` is valid. A matching sidecar may only
    /// retain completed parts wholly contained in that visible prefix, and
    /// every retained digest is checked before its bit is trusted. The strict
    /// parallel loader above deliberately keeps its exact-`total` requirement.
    pub(crate) fn load_or_create_serial(
        target: &Path,
        total: u64,
        chunk: u64,
        max_parts: usize,
        identity: &str,
    ) -> io::Result<Self> {
        let mut fresh = Self::fresh(target, total, chunk, max_parts, identity)?;
        let target_len = match std::fs::metadata(target) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        if target_len > total {
            return Ok(fresh);
        }

        if !sidecar_namespace_is_owned(&fresh.sidecar)? {
            return Ok(fresh);
        }
        cleanup_stale_checkpoint_temps(&fresh.sidecar);
        let expected_len = expected_v2_encoded_len(total, chunk, identity)?;
        let raw = match read_sidecar_bounded(&fresh.sidecar, expected_len)? {
            Some(raw) => raw,
            None => return Ok(fresh),
        };
        match Self::decode(&raw, total, chunk, max_parts, identity) {
            Some((bits, digests)) => {
                fresh.bits = bits;
                fresh.digests = digests;
                fresh.verify_completed_parts_through(target_len)?;
                Ok(fresh)
            }
            None => Ok(fresh),
        }
    }

    /// header: MAGIC u32 | VERSION u16 | total u64 | chunk u64 | max_parts u32
    ///         | id_len u16 | id bytes | bitmap bytes | SHA-256[part_count]
    fn decode(
        raw: &[u8],
        total: u64,
        chunk: u64,
        _max_parts: usize,
        identity: &str,
    ) -> Option<(Vec<u8>, Vec<[u8; PART_DIGEST_LEN]>)> {
        let mut cur = io::Cursor::new(raw);
        let mut u32b = [0u8; 4];
        let mut u16b = [0u8; 2];
        let mut u64b = [0u8; 8];
        cur.read_exact(&mut u32b).ok()?;
        if u32::from_le_bytes(u32b) != MAGIC {
            return None;
        }
        cur.read_exact(&mut u16b).ok()?;
        if u16::from_le_bytes(u16b) != VERSION {
            return None;
        }
        cur.read_exact(&mut u64b).ok()?;
        if u64::from_le_bytes(u64b) != total {
            return None;
        }
        cur.read_exact(&mut u64b).ok()?;
        if u64::from_le_bytes(u64b) != chunk {
            return None;
        }
        cur.read_exact(&mut u32b).ok()?;
        let _encoded_max_parts = u32::from_le_bytes(u32b);
        cur.read_exact(&mut u16b).ok()?;
        let id_len = u16::from_le_bytes(u16b) as usize;
        // The caller already supplied the only acceptable identity length.
        // Reject a forged length before allocating or slicing by that field.
        if id_len != identity.len() {
            return None;
        }
        let id_start = usize::try_from(cur.position()).ok()?;
        let id_end = id_start.checked_add(id_len)?;
        if raw.get(id_start..id_end)? != identity.as_bytes() {
            return None;
        }
        cur.set_position(u64::try_from(id_end).ok()?);
        let part_count = part_count_for(total, chunk).ok()?;
        let byte_len = part_count.div_ceil(8);
        let digest_bytes = part_count.checked_mul(PART_DIGEST_LEN)?;
        let body_len = byte_len.checked_add(digest_bytes)?;
        let consumed = usize::try_from(cur.position()).ok()?;
        if raw.len().checked_sub(consumed)? != body_len {
            return None;
        }
        let mut bits = zeroed_bytes(byte_len).ok()?;
        cur.read_exact(&mut bits).ok()?;
        let mut digests = zeroed_digests(part_count).ok()?;
        for digest in &mut digests {
            cur.read_exact(digest).ok()?;
        }
        Some((bits, digests))
    }

    fn encode_snapshot(
        &self,
        bits: &[u8],
        digests: &[[u8; PART_DIGEST_LEN]],
    ) -> io::Result<Vec<u8>> {
        let id = self.identity.as_bytes();
        let capacity = SIDECAR_HEADER_LEN
            .checked_add(id.len())
            .and_then(|len| len.checked_add(bits.len()))
            .and_then(|len| {
                digests
                    .len()
                    .checked_mul(PART_DIGEST_LEN)
                    .and_then(|digest_len| len.checked_add(digest_len))
            })
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "sidecar length overflow")
            })?;
        let mut out = Vec::new();
        out.try_reserve_exact(capacity).map_err(|e| {
            io::Error::other(format!("cannot allocate encoded download sidecar: {e}"))
        })?;
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.total.to_le_bytes());
        out.extend_from_slice(&self.chunk.to_le_bytes());
        out.extend_from_slice(&(self.max_parts as u32).to_le_bytes());
        out.extend_from_slice(&(id.len() as u16).to_le_bytes());
        out.extend_from_slice(id);
        out.extend_from_slice(bits);
        for digest in digests {
            out.extend_from_slice(digest);
        }
        Ok(out)
    }

    fn clear_part(&mut self, index: usize) {
        if let Some(byte) = self.bits.get_mut(index >> 3) {
            *byte &= !(1u8 << (index & 7));
        }
        if let Some(digest) = self.digests.get_mut(index) {
            *digest = [0; PART_DIGEST_LEN];
        }
    }

    fn verify_completed_parts(&mut self) -> io::Result<()> {
        self.verify_completed_parts_through(self.total)
    }

    fn verify_completed_parts_through(&mut self, target_len: u64) -> io::Result<()> {
        let mut target_file = None;
        for index in 0..self.part_count {
            let (offset, len) = self.part_range(index)?;
            let end = offset.checked_add(len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "download part end overflow")
            })?;
            if end > target_len {
                self.clear_part(index);
                continue;
            }
            let is_set = self
                .bits
                .get(index >> 3)
                .map(|byte| byte & (1u8 << (index & 7)) != 0)
                .unwrap_or(false);
            if !is_set {
                continue;
            }
            if target_file.is_none() {
                target_file = Some(std::fs::File::open(&self.target)?);
            }
            let Some(target_file) = target_file.as_mut() else {
                return Err(io::Error::other(
                    "download target handle initialization failed",
                ));
            };
            let actual = digest_open_file_range(target_file, offset, len)?;
            if self.digests.get(index).copied() != Some(actual) {
                self.clear_part(index);
            }
        }
        Ok(())
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    pub(crate) fn is_done(&self, offset: u64) -> bool {
        match self.index_of(offset) {
            Some(i) => self
                .bits
                .get(i >> 3)
                .map(|b| b & (1u8 << (i & 7)) != 0)
                .unwrap_or(false),
            None => false,
        }
    }

    /// Adds a completed write to the current epoch and checkpoints when its
    /// count/time threshold is reached. Returning `false` means the part is
    /// intentionally still uncommitted and will be safely re-downloaded after a
    /// crash; it does not mean the write failed.
    #[cfg(test)]
    pub(crate) fn stage_done(&mut self, offset: u64) -> io::Result<bool> {
        let i = self.index_of(offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unaligned or out-of-range download part offset: {offset}"),
            )
        })?;
        let (part_offset, len) = self.part_range(i)?;
        let digest = digest_file_range(&self.target, part_offset, len)?;
        self.stage_done_with_digest(offset, digest)
            .map(|outcome| outcome.checkpointed)
    }

    #[cfg(test)]
    pub(crate) fn stage_done_with_digest(
        &mut self,
        offset: u64,
        digest: [u8; PART_DIGEST_LEN],
    ) -> io::Result<StageDoneOutcome> {
        let outcome = self.stage_done_with_digest_deferred(offset, digest)?;
        if !outcome.checkpoint_due {
            return Ok(outcome);
        }
        self.force_checkpoint()?;
        Ok(StageDoneOutcome {
            #[cfg(test)]
            checkpointed: true,
            checkpoint_due: false,
            arm_timer: false,
        })
    }

    /// Stages a completed part without opening or syncing the target path.
    ///
    /// Production callers that own an already-locked target handle use this
    /// method, perform [`Self::begin_external_checkpoint`], sync that exact
    /// handle, and then call [`Self::commit_checkpoint_after_data_sync`]. The
    /// compatibility [`Self::stage_done_with_digest`] method retains the legacy
    /// path-opening behavior by composing this method with
    /// [`Self::force_checkpoint`].
    pub(crate) fn stage_done_with_digest_deferred(
        &mut self,
        offset: u64,
        digest: [u8; PART_DIGEST_LEN],
    ) -> io::Result<StageDoneOutcome> {
        let i = self.index_of(offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unaligned or out-of-range download part offset: {offset}"),
            )
        })?;
        if self.is_done(offset) {
            return Ok(StageDoneOutcome {
                #[cfg(test)]
                checkpointed: false,
                checkpoint_due: false,
                arm_timer: false,
            });
        }
        let already_pending = self
            .pending_bits
            .get(i >> 3)
            .map(|byte| byte & (1u8 << (i & 7)) != 0)
            .unwrap_or(false);
        if already_pending {
            // The previous attempt staged this part but its checkpoint failed.
            // A transfer retry must request the durability step again; silently
            // accepting the duplicate would turn a transient sync error into an
            // uncommitted success. Do not run it here because production must
            // sync through the unique already-locked target handle.
            return Ok(StageDoneOutcome {
                #[cfg(test)]
                checkpointed: false,
                checkpoint_due: true,
                arm_timer: false,
            });
        }
        self.pending.try_reserve(1).map_err(|e| {
            io::Error::other(format!("cannot allocate pending checkpoint entry: {e}"))
        })?;
        self.pending.push(PendingPart { index: i, digest });
        let pending_byte = self.pending_bits.get_mut(i >> 3).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "pending part bitmap index overflow",
            )
        })?;
        *pending_byte |= 1u8 << (i & 7);
        let timer_enabled = self.part_count <= CHECKPOINT_TIMER_MAX_PART_COUNT;
        let should_checkpoint = self.pending.len() >= self.checkpoint_threshold.max(1)
            || (timer_enabled && self.last_checkpoint.elapsed() >= DEFAULT_CHECKPOINT_INTERVAL);
        if should_checkpoint {
            Ok(StageDoneOutcome {
                #[cfg(test)]
                checkpointed: false,
                checkpoint_due: true,
                arm_timer: false,
            })
        } else {
            let arm_timer = timer_enabled && !self.checkpoint_timer_armed;
            if timer_enabled {
                self.checkpoint_timer_armed = true;
            }
            Ok(StageDoneOutcome {
                #[cfg(test)]
                checkpointed: false,
                checkpoint_due: false,
                arm_timer,
            })
        }
    }

    /// Called by the one-shot task timer. A threshold/final checkpoint may
    /// already have disarmed it; in that case the stale wake-up is a no-op.
    #[cfg(test)]
    pub(crate) fn checkpoint_timer_fired(&mut self) -> io::Result<()> {
        if !self.begin_timer_checkpoint()? {
            return Ok(());
        }
        self.sync_target_by_path()?;
        self.commit_checkpoint_after_data_sync()
    }

    /// Claims a pending checkpoint for a caller that will sync the unique,
    /// already-locked target handle. This method performs no target path I/O.
    ///
    /// `Ok(true)` means the caller must perform `sync_data` successfully and
    /// then call [`Self::commit_checkpoint_after_data_sync`]. Pending bits stay
    /// unpublished until that commit succeeds.
    pub(crate) fn begin_external_checkpoint(&mut self) -> io::Result<bool> {
        self.checkpoint_timer_armed = false;
        if self.pending.is_empty() {
            return Ok(false);
        }
        #[cfg(test)]
        self.fail_if_requested(CheckpointFailurePoint::BeforeDataBarrier)?;
        Ok(true)
    }

    /// Claims an armed one-shot timer checkpoint without touching the target.
    /// A stale timer returns `Ok(false)` and performs no path I/O.
    pub(crate) fn begin_timer_checkpoint(&mut self) -> io::Result<bool> {
        if !self.checkpoint_timer_armed {
            return Ok(false);
        }
        self.checkpoint_timer_armed = false;
        self.begin_external_checkpoint()
    }

    /// Publishes the pending epoch after the caller has durably synced the exact
    /// locked target handle. This method writes only the sidecar; it never opens
    /// the target path.
    pub(crate) fn commit_checkpoint_after_data_sync(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        #[cfg(test)]
        self.fail_if_requested(CheckpointFailurePoint::AfterDataBarrier)?;

        let mut next_bits = try_clone_slice(&self.bits, "checkpoint bitmap")?;
        let mut next_digests = try_clone_slice(&self.digests, "checkpoint digests")?;
        for part in &self.pending {
            let byte = next_bits.get_mut(part.index >> 3).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pending part bitmap index overflow",
                )
            })?;
            *byte |= 1u8 << (part.index & 7);
            let digest = next_digests.get_mut(part.index).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pending part digest index overflow",
                )
            })?;
            *digest = part.digest;
        }
        self.persist_snapshot_atomic(&next_bits, &next_digests)?;
        self.bits = next_bits;
        self.digests = next_digests;
        self.pending.clear();
        self.pending_bits.fill(0);
        self.last_checkpoint = Instant::now();
        #[cfg(test)]
        {
            self.checkpoint_count += 1;
        }
        Ok(())
    }

    #[cfg(test)]
    fn sync_target_by_path(&self) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.target)?
            .sync_data()
    }

    /// Compatibility checkpoint path for tests and callers that do not own a
    /// locked target handle. Production download code should use the external
    /// begin/sync/commit sequence instead.
    #[cfg(test)]
    pub(crate) fn force_checkpoint(&mut self) -> io::Result<()> {
        if !self.begin_external_checkpoint()? {
            return Ok(());
        }

        self.sync_target_by_path()?;
        self.commit_checkpoint_after_data_sync()
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Compatibility helper used by focused sidecar tests. Production uses the
    /// batched `stage_done` path and only forces on pause/cancel/finalize.
    #[cfg(test)]
    pub(crate) fn mark_done_and_persist(&mut self, offset: u64) -> io::Result<()> {
        self.stage_done(offset)?;
        self.force_checkpoint()
    }

    fn persist_snapshot_atomic(
        &mut self,
        bits: &[u8],
        digests: &[[u8; PART_DIGEST_LEN]],
    ) -> io::Result<()> {
        let bytes = self.encode_snapshot(bits, digests)?;
        ensure_sidecar_namespace(&self.sidecar)?;
        let (tmp, mut f) = create_checkpoint_temp(&self.sidecar)?;
        let result = (|| {
            f.write_all(&bytes)?;
            f.sync_all()?;
            #[cfg(test)]
            self.fail_if_requested(CheckpointFailurePoint::AfterSidecarFsync)?;
            drop(f);
            sidecar_destination_is_absent(&self.sidecar)?;
            atomic_replace(&tmp, &self.sidecar)?;
            #[cfg(test)]
            self.fail_if_requested(CheckpointFailurePoint::AfterRename)?;
            sync_parent_directory(&self.sidecar)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    #[cfg(test)]
    fn set_checkpoint_threshold_for_test(&mut self, threshold: usize) {
        self.checkpoint_threshold = threshold.max(1);
    }

    #[cfg(test)]
    fn checkpoint_count_for_test(&self) -> usize {
        self.checkpoint_count
    }

    #[cfg(test)]
    fn fail_next_checkpoint_for_test(&mut self, point: CheckpointFailurePoint) {
        self.fail_next_checkpoint = Some(point);
    }

    #[cfg(test)]
    fn fail_if_requested(&mut self, point: CheckpointFailurePoint) -> io::Result<()> {
        if self.fail_next_checkpoint == Some(point) {
            self.fail_next_checkpoint = None;
            return Err(io::Error::other(format!(
                "injected checkpoint failure at {point:?}"
            )));
        }
        Ok(())
    }

    /// Replaces the bitmap with one caller-verified contiguous prefix and
    /// publishes exactly one snapshot.
    ///
    /// This is the linear-time migration path for a legacy serial partial file:
    /// the caller hashes the already-locked handle, syncs that exact handle,
    /// then supplies `(part offset, SHA-256)` entries for offsets
    /// `0, chunk, 2*chunk, ...`. This method never opens the target path.
    #[cfg(test)]
    pub(crate) fn seed_verified_prefix_digests_after_data_sync(
        &mut self,
        verified: &[(u64, [u8; PART_DIGEST_LEN])],
    ) -> io::Result<()> {
        if verified.len() > self.part_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "verified prefix contains more parts than the download",
            ));
        }
        for (index, (offset, _)) in verified.iter().enumerate() {
            let expected = u64::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(self.chunk))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "verified prefix offset overflow",
                    )
                })?;
            if *offset != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "verified prefix offset {offset} is not the expected contiguous offset {expected}"
                    ),
                ));
            }
        }

        let mut next_bits = zeroed_bytes(self.bits.len())?;
        let mut next_digests = zeroed_digests(self.part_count)?;
        for (index, (_, digest)) in verified.iter().enumerate() {
            let byte = next_bits.get_mut(index >> 3).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "verified prefix bitmap overflow",
                )
            })?;
            *byte |= 1u8 << (index & 7);
            next_digests[index] = *digest;
        }

        self.persist_snapshot_atomic(&next_bits, &next_digests)?;
        self.bits = next_bits;
        self.digests = next_digests;
        self.pending.clear();
        self.pending_bits.fill(0);
        self.checkpoint_timer_armed = false;
        self.last_checkpoint = Instant::now();
        #[cfg(test)]
        {
            self.checkpoint_count += 1;
        }
        Ok(())
    }

    /// Keeps only the committed run beginning at part zero and atomically
    /// publishes it after the caller has truncated + synced the locked target
    /// handle to [`Self::contiguous_watermark`]. Even an empty prefix is
    /// persisted, so stale high bits cannot return on the next process start.
    /// This method writes only the sidecar and never opens the target path.
    pub(crate) fn retain_contiguous_prefix_after_data_sync(&mut self) -> io::Result<u64> {
        let watermark = self.contiguous_watermark();
        let retained_parts = if watermark == self.total {
            self.part_count
        } else {
            usize::try_from(watermark / self.chunk).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "serial prefix part count does not fit this architecture",
                )
            })?
        };
        let mut next_bits = try_clone_slice(&self.bits, "serial prefix bitmap")?;
        let mut next_digests = try_clone_slice(&self.digests, "serial prefix digests")?;
        for (index, digest) in next_digests.iter_mut().enumerate().skip(retained_parts) {
            let byte = next_bits.get_mut(index >> 3).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "serial prefix bitmap overflow")
            })?;
            *byte &= !(1u8 << (index & 7));
            *digest = [0; PART_DIGEST_LEN];
        }

        self.persist_snapshot_atomic(&next_bits, &next_digests)?;
        self.bits = next_bits;
        self.digests = next_digests;
        self.pending.clear();
        self.pending_bits.fill(0);
        self.checkpoint_timer_armed = false;
        self.last_checkpoint = Instant::now();
        #[cfg(test)]
        {
            self.checkpoint_count += 1;
        }
        Ok(watermark)
    }

    /// End offset (clamped to `total`) of the longest contiguous run of done
    /// parts starting at part 0.
    pub(crate) fn contiguous_watermark(&self) -> u64 {
        let mut wm: u64 = 0;
        for i in 0..self.part_count {
            let set = self
                .bits
                .get(i >> 3)
                .map(|b| b & (1u8 << (i & 7)) != 0)
                .unwrap_or(false);
            if !set {
                break;
            }
            wm = wm.saturating_add(self.chunk).min(self.total);
        }
        wm
    }

    pub(crate) fn all_done(&self) -> bool {
        (0..self.part_count).all(|i| {
            self.bits
                .get(i >> 3)
                .map(|b| b & (1u8 << (i & 7)) != 0)
                .unwrap_or(false)
        })
    }

    /// Re-reads every committed range before a Complete event may be emitted.
    /// This catches a same-length in-place rewrite or visible-path replacement
    /// that happened after the last checkpoint but before finalization.
    pub(crate) fn validate_committed_content(&self) -> io::Result<()> {
        // Open once so all ranges and the exact-length check observe one file
        // handle rather than a mixture of path generations.
        let mut file = std::fs::File::open(&self.target)?;
        self.validate_committed_content_from_open_file(&mut file)
    }

    /// Windows holds a mandatory whole-file lock during finalization, so a
    /// second handle may not read the locked range even in the owning process.
    /// The target handle omits delete sharing, which keeps the visible path
    /// bound to this object; validate through that exact locked handle instead.
    #[cfg(windows)]
    pub(crate) fn validate_committed_content_on_locked_file(
        &self,
        file: &mut std::fs::File,
    ) -> io::Result<()> {
        self.validate_committed_content_from_open_file(file)
    }

    fn validate_committed_content_from_open_file(
        &self,
        file: &mut std::fs::File,
    ) -> io::Result<()> {
        let actual_len = file.metadata()?.len();
        if actual_len != self.total {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "download target length changed: expected {}, found {actual_len}",
                    self.total
                ),
            ));
        }
        for index in 0..self.part_count {
            let is_set = self
                .bits
                .get(index >> 3)
                .map(|byte| byte & (1u8 << (index & 7)) != 0)
                .unwrap_or(false);
            if !is_set {
                continue;
            }
            let (offset, len) = self.part_range(index)?;
            let actual = digest_open_file_range(file, offset, len)?;
            if self.digests.get(index).copied() != Some(actual) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("download target content changed in committed part {index}"),
                ));
            }
        }
        Ok(())
    }

    /// Removes the sidecar (best effort; call after a verified full download).
    pub(crate) fn delete(self) -> io::Result<()> {
        if !sidecar_namespace_is_owned(&self.sidecar)? {
            return Ok(());
        }
        if sidecar_destination_is_absent(&self.sidecar)? {
            return Ok(());
        }
        match std::fs::remove_file(&self.sidecar) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn try_clone_slice<T: Clone>(source: &[T], what: &str) -> io::Result<Vec<T>> {
    let mut cloned = Vec::new();
    cloned
        .try_reserve_exact(source.len())
        .map_err(|e| io::Error::other(format!("cannot allocate {what}: {e}")))?;
    cloned.extend_from_slice(source);
    Ok(cloned)
}

fn create_checkpoint_temp(sidecar: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    loop {
        let mut tmp = sidecar.as_os_str().to_os_string();
        let sequence = CHECKPOINT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        tmp.push(format!(".tmp.{}.{}", std::process::id(), sequence));
        let tmp = PathBuf::from(tmp);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn cleanup_stale_checkpoint_temps(sidecar: &Path) {
    if !matches!(sidecar_namespace_is_owned(sidecar), Ok(true)) {
        return;
    }
    let Some(file_name) = sidecar.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!("{file_name}.tmp");
    let parent = sidecar.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let matches = entry
            .file_name()
            .to_str()
            .map(|name| name == prefix || name.starts_with(&format!("{prefix}.")))
            .unwrap_or(false);
        if matches {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rusty_cat_download_progress_tests_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let p = root.join(format!("rcdl_{name}_{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(sidecar_path(&p));
        p
    }

    fn checkpoint_temp_path(sidecar: &Path, sequence: u64) -> PathBuf {
        let mut path = sidecar.as_os_str().to_os_string();
        path.push(format!(".tmp.{}.{}", std::process::id(), sequence));
        PathBuf::from(path)
    }

    fn checkpoint_temp_files(sidecar: &Path) -> Vec<PathBuf> {
        let Some(file_name) = sidecar.file_name().and_then(|name| name.to_str()) else {
            return Vec::new();
        };
        let prefix = format!("{file_name}.tmp");
        let parent = sidecar.parent().unwrap_or_else(|| Path::new("."));
        std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .filter(|name| *name == prefix || name.starts_with(&format!("{prefix}.")))
                    .map(|_| entry.path())
            })
            .collect()
    }

    #[test]
    fn checkpoint_replaces_an_existing_sidecar_snapshot() {
        let target = tmp("checkpoint_replace_existing");
        std::fs::write(&target, vec![7u8; 20]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        progress.mark_done_and_persist(0).unwrap();
        let sidecar = sidecar_path(&target);
        let first_snapshot = std::fs::read(&sidecar).unwrap();

        progress.mark_done_and_persist(10).unwrap();
        let second_snapshot = std::fs::read(&sidecar).unwrap();
        assert_ne!(first_snapshot, second_snapshot);
        drop(progress);

        let resumed = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        assert!(resumed.is_done(0));
        assert!(resumed.is_done(10));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn checkpoint_never_touches_the_legacy_target_dot_rcdl_path() {
        const SENTINEL: &[u8] = b"ordinary user file, not rusty-cat state";

        let target = tmp("legacy_name_collision");
        std::fs::write(&target, vec![7u8; 20]).unwrap();
        let mut legacy = target.as_os_str().to_os_string();
        legacy.push(".rcdl");
        let legacy = PathBuf::from(legacy);
        std::fs::write(&legacy, SENTINEL).unwrap();

        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        progress.mark_done_and_persist(0).unwrap();

        assert_eq!(std::fs::read(&legacy).unwrap(), SENTINEL);
        assert_ne!(sidecar_path(&target), legacy);
        progress.delete().unwrap();
        assert_eq!(std::fs::read(&legacy).unwrap(), SENTINEL);

        let _ = std::fs::remove_file(legacy);
        let _ = std::fs::remove_file(target);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_target_names_have_distinct_lossless_hashed_sidecars() {
        use std::os::unix::ffi::OsStringExt;

        let root = tmp("non_utf8_sidecar_names").with_extension("dir");
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join(std::ffi::OsString::from_vec(vec![b'f', b'o', 0x80]));
        let second = root.join(std::ffi::OsString::from_vec(vec![b'f', b'o', 0x81]));

        let first_sidecar = sidecar_path(&first);
        let second_sidecar = sidecar_path(&second);
        assert_ne!(first_sidecar, second_sidecar);
        assert_eq!(
            first_sidecar.parent(),
            Some(root.join(".rusty-cat").as_path())
        );
        assert_eq!(
            first_sidecar.extension(),
            Some(std::ffi::OsStr::new("rcdl"))
        );
        assert_eq!(
            first_sidecar.file_name().unwrap().as_encoded_bytes().len(),
            64 + ".rcdl".len()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_utf16_target_names_have_distinct_lossless_hashed_sidecars() {
        use std::os::windows::ffi::OsStringExt;

        let root = tmp("non_unicode_utf16_sidecar_names").with_extension("dir");
        let first = root.join(std::ffi::OsString::from_wide(&[
            b'f' as u16,
            b'o' as u16,
            0xd800,
        ]));
        let second = root.join(std::ffi::OsString::from_wide(&[
            b'f' as u16,
            b'o' as u16,
            0xd801,
        ]));

        let first_sidecar = sidecar_path(&first);
        let second_sidecar = sidecar_path(&second);
        assert_ne!(first_sidecar, second_sidecar);
        assert_eq!(
            first_sidecar.parent(),
            Some(root.join(".rusty-cat").as_path())
        );
        assert_eq!(
            first_sidecar.extension(),
            Some(std::ffi::OsStr::new("rcdl"))
        );
        assert_eq!(
            first_sidecar
                .file_name()
                .expect("hashed sidecar name")
                .to_string_lossy()
                .len(),
            64 + ".rcdl".len()
        );
    }

    #[test]
    fn namespace_type_conflict_fails_closed_without_overwriting_it() {
        const SENTINEL: &[u8] = b"pre-existing file at reserved namespace";

        let root = tmp("namespace_type_conflict").with_extension("dir");
        let target = root.join("payload.bin");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&target, vec![3u8; 10]).unwrap();
        let namespace = root.join(".rusty-cat");
        std::fs::write(&namespace, SENTINEL).unwrap();

        let error = match DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1") {
            Ok(mut progress) => progress.mark_done_and_persist(0).unwrap_err(),
            Err(error) => error,
        };
        assert!(matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory
        ));
        assert_eq!(std::fs::read(&namespace).unwrap(), SENTINEL);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unowned_nonempty_namespace_is_never_claimed_or_modified() {
        const SENTINEL: &[u8] = b"unrelated application state";

        let root = tmp("unowned_namespace").with_extension("dir");
        let target = root.join("payload.bin");
        let namespace = root.join(".rusty-cat");
        let unrelated = namespace.join("settings.json");
        std::fs::create_dir_all(&namespace).unwrap();
        std::fs::write(&target, vec![9u8; 10]).unwrap();
        std::fs::write(&unrelated, SENTINEL).unwrap();

        let mut progress = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
        let error = progress.mark_done_and_persist(0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&unrelated).unwrap(), SENTINEL);
        assert!(!namespace.join(SIDECAR_NAMESPACE_MARKER).exists());
        assert!(!sidecar_path(&target).exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_first_use_initializes_one_shared_namespace_without_spurious_failure() {
        const THREADS: usize = 16;

        let root = tmp("concurrent_namespace_init").with_extension("dir");
        std::fs::create_dir_all(&root).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let mut workers = Vec::new();
        for index in 0..THREADS {
            let barrier = std::sync::Arc::clone(&barrier);
            let sidecar = sidecar_path(&root.join(format!("target-{index}.bin")));
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                ensure_sidecar_namespace(&sidecar)
            }));
        }
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        let marker = root.join(SIDECAR_NAMESPACE).join(SIDECAR_NAMESPACE_MARKER);
        validate_namespace_marker(&marker).unwrap();

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_temp_creation_never_overwrites_an_existing_file() {
        const COLLISION_COUNT: u64 = 128;
        const SENTINEL: &[u8] = b"pre-existing checkpoint temp";

        let target = tmp("checkpoint_create_new");
        std::fs::write(&target, vec![5u8; 10]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
        let sidecar = sidecar_path(&target);
        ensure_sidecar_namespace(&sidecar).unwrap();
        let first_sequence = CHECKPOINT_TEMP_SEQUENCE.load(Ordering::Relaxed);
        let collisions: Vec<_> = (0..COLLISION_COUNT)
            .map(|offset| checkpoint_temp_path(&sidecar, first_sequence.wrapping_add(offset)))
            .collect();
        for collision in &collisions {
            std::fs::write(collision, SENTINEL).unwrap();
        }

        progress.mark_done_and_persist(0).unwrap();

        for collision in &collisions {
            assert_eq!(
                std::fs::read(collision).unwrap(),
                SENTINEL,
                "exclusive creation must skip an occupied checkpoint temp path"
            );
        }
        cleanup_stale_checkpoint_temps(&sidecar);
        let _ = progress.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn failed_checkpoint_keeps_the_previous_snapshot_and_removes_its_temp() {
        let target = tmp("checkpoint_preserve_old");
        std::fs::write(&target, vec![3u8; 20]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(8);
        progress.mark_done_and_persist(0).unwrap();
        let sidecar = sidecar_path(&target);
        let previous_snapshot = std::fs::read(&sidecar).unwrap();

        progress.stage_done(10).unwrap();
        progress.fail_next_checkpoint_for_test(CheckpointFailurePoint::AfterSidecarFsync);
        assert!(progress.force_checkpoint().is_err());

        assert_eq!(std::fs::read(&sidecar).unwrap(), previous_snapshot);
        assert!(
            checkpoint_temp_files(&sidecar).is_empty(),
            "a failed checkpoint must remove its temporary file"
        );
        drop(progress);

        let resumed = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        assert!(resumed.is_done(0));
        assert!(!resumed.is_done(10));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn loading_removes_only_stale_checkpoint_temp_files() {
        let target = tmp("checkpoint_stale_temp_cleanup");
        std::fs::write(&target, vec![1u8; 10]).unwrap();
        let sidecar = sidecar_path(&target);
        ensure_sidecar_namespace(&sidecar).unwrap();
        let mut exact_temp = sidecar.as_os_str().to_os_string();
        exact_temp.push(".tmp");
        let exact_temp = PathBuf::from(exact_temp);
        let stale_temp = checkpoint_temp_path(&sidecar, u64::MAX);
        let mut unrelated = sidecar.as_os_str().to_os_string();
        unrelated.push(".tmp-not-a-checkpoint");
        let unrelated = PathBuf::from(unrelated);
        std::fs::write(&exact_temp, b"stale").unwrap();
        std::fs::write(&stale_temp, b"stale").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let progress = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();

        assert!(!exact_temp.exists());
        assert!(!stale_temp.exists());
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");
        let _ = std::fs::remove_file(unrelated);
        let _ = progress.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn staged_parts_are_not_committed_until_checkpoint_and_batch_syncs_once() {
        let target = tmp("checkpoint_batch");
        std::fs::write(&target, vec![7u8; 40]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 40, 10, 4, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(4);

        for offset in [30, 0, 20] {
            assert!(!progress.stage_done(offset).unwrap());
            assert!(!progress.is_done(offset), "pending bit must stay unset");
        }
        assert_eq!(progress.pending_count(), 3);
        assert_eq!(progress.checkpoint_count_for_test(), 0);

        assert!(progress.stage_done(10).unwrap(), "threshold freezes epoch");
        assert_eq!(progress.pending_count(), 0);
        assert_eq!(progress.checkpoint_count_for_test(), 1);
        assert!(progress.all_done());
        let _ = progress.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn final_partial_batch_is_force_committed() {
        let target = tmp("checkpoint_final");
        std::fs::write(&target, vec![9u8; 30]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(8);
        progress.stage_done(0).unwrap();
        progress.stage_done(20).unwrap();
        assert_eq!(progress.pending_count(), 2);
        progress.force_checkpoint().unwrap();
        assert!(progress.is_done(0));
        assert!(progress.is_done(20));
        assert!(!progress.is_done(10));

        let resumed = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        assert!(resumed.is_done(0));
        assert!(resumed.is_done(20));
        assert!(!resumed.is_done(10));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn external_checkpoint_commits_without_reopening_the_target_path() {
        let target = tmp("external_checkpoint_no_reopen");
        let bytes = vec![9u8; 10];
        std::fs::write(&target, &bytes).unwrap();
        let digest: [u8; PART_DIGEST_LEN] = Sha256::digest(&bytes).into();
        let mut progress = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(1);

        // The production caller has already opened and locked the target. Make
        // any accidental path reopen fail while exercising only the external
        // barrier API.
        std::fs::remove_file(&target).unwrap();
        let outcome = progress.stage_done_with_digest_deferred(0, digest).unwrap();
        assert!(outcome.checkpoint_due);
        assert!(!outcome.arm_timer);
        assert!(!progress.is_done(0), "begin must not publish the bit");
        assert!(progress.begin_external_checkpoint().unwrap());
        assert!(
            !progress.is_done(0),
            "data sync alone must not publish the bit"
        );
        progress.commit_checkpoint_after_data_sync().unwrap();
        assert!(progress.is_done(0));
        assert_eq!(progress.pending_count(), 0);

        // The sidecar snapshot contains the supplied digest and can be trusted
        // once the target is visible again with matching bytes.
        std::fs::write(&target, &bytes).unwrap();
        let resumed = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
        assert!(resumed.is_done(0));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn deferred_duplicate_requests_checkpoint_without_path_io() {
        let target = tmp("external_checkpoint_duplicate");
        let bytes = vec![4u8; 10];
        std::fs::write(&target, &bytes).unwrap();
        let digest: [u8; PART_DIGEST_LEN] = Sha256::digest(&bytes).into();
        let mut progress = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(8);

        let first = progress.stage_done_with_digest_deferred(0, digest).unwrap();
        assert!(!first.checkpoint_due);
        assert!(first.arm_timer);
        std::fs::remove_file(&target).unwrap();

        let duplicate = progress.stage_done_with_digest_deferred(0, digest).unwrap();
        assert!(duplicate.checkpoint_due);
        assert!(!duplicate.arm_timer);
        assert_eq!(progress.pending_count(), 1);

        let _ = std::fs::remove_file(sidecar_path(&target));
    }

    #[test]
    fn external_timer_claim_is_one_shot_and_stale_timer_is_noop() {
        let target = tmp("external_checkpoint_timer");
        let bytes = vec![6u8; 10];
        std::fs::write(&target, &bytes).unwrap();
        let digest: [u8; PART_DIGEST_LEN] = Sha256::digest(&bytes).into();
        let mut progress = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(8);
        let outcome = progress.stage_done_with_digest_deferred(0, digest).unwrap();
        assert!(outcome.arm_timer);
        assert!(!outcome.checkpoint_due);

        std::fs::remove_file(&target).unwrap();
        assert!(progress.begin_timer_checkpoint().unwrap());
        progress.commit_checkpoint_after_data_sync().unwrap();
        assert!(progress.is_done(0));
        assert!(!progress.begin_timer_checkpoint().unwrap());
        progress
            .checkpoint_timer_fired()
            .expect("stale timer must not reopen a missing target");

        let _ = progress.delete();
    }

    #[test]
    fn checkpoint_failure_never_exposes_pending_bits() {
        let target = tmp("checkpoint_failure");
        std::fs::write(&target, vec![3u8; 20]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(8);
        progress.stage_done(0).unwrap();
        progress.fail_next_checkpoint_for_test(CheckpointFailurePoint::AfterDataBarrier);
        assert!(progress.force_checkpoint().is_err());
        assert!(!progress.is_done(0));
        assert_eq!(progress.pending_count(), 1);

        let resumed = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        assert!(
            !resumed.is_done(0),
            "unpublished epoch must be downloaded again"
        );
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn every_checkpoint_crash_boundary_recovers_only_a_durable_snapshot() {
        for (case, point, expect_committed) in [
            (
                "before_data",
                CheckpointFailurePoint::BeforeDataBarrier,
                false,
            ),
            (
                "after_data",
                CheckpointFailurePoint::AfterDataBarrier,
                false,
            ),
            (
                "after_sidecar_fsync",
                CheckpointFailurePoint::AfterSidecarFsync,
                false,
            ),
            ("after_rename", CheckpointFailurePoint::AfterRename, true),
        ] {
            let target = tmp(case);
            std::fs::write(&target, vec![3u8; 20]).unwrap();
            let mut progress =
                DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
            progress.set_checkpoint_threshold_for_test(8);
            progress.stage_done(0).unwrap();
            progress.fail_next_checkpoint_for_test(point);
            assert!(progress.force_checkpoint().is_err());
            drop(progress);

            let resumed = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
            assert_eq!(
                resumed.is_done(0),
                expect_committed,
                "unexpected recovery at {point:?}"
            );
            let _ = resumed.delete();
            let _ = std::fs::remove_file(target);
        }
    }

    #[test]
    fn part_staged_while_previous_epoch_is_frozen_enters_next_epoch() {
        let target = tmp("checkpoint_epoch");
        std::fs::write(&target, vec![5u8; 30]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        progress.set_checkpoint_threshold_for_test(2);
        progress.stage_done(0).unwrap();
        assert!(progress.stage_done(10).unwrap());
        assert_eq!(progress.checkpoint_count_for_test(), 1);
        progress.stage_done(20).unwrap();
        assert_eq!(progress.pending_count(), 1);
        assert!(!progress.is_done(20));
        assert_eq!(progress.checkpoint_count_for_test(), 1);
        progress.force_checkpoint().unwrap();
        assert_eq!(progress.checkpoint_count_for_test(), 2);
        assert!(progress.all_done());
        let _ = progress.delete();
        let _ = std::fs::remove_file(target);
    }

    // total=25, chunk=10 => 3 parts at offsets 0,10,20
    #[test]
    fn fresh_has_nothing_done_and_zero_watermark() {
        let target = tmp("fresh");
        std::fs::write(&target, vec![0u8; 25]).unwrap();
        let p = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-1").unwrap();
        assert!(!p.is_done(0));
        assert_eq!(p.contiguous_watermark(), 0);
        assert!(!p.all_done());
        assert_eq!(p.total(), 25);
    }

    #[test]
    fn mark_persists_and_reloads_only_unfinished() {
        let target = tmp("persist");
        std::fs::write(&target, vec![0u8; 25]).unwrap();
        {
            let mut p = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-1").unwrap();
            p.mark_done_and_persist(0).unwrap();
            p.mark_done_and_persist(20).unwrap(); // out-of-order: 0 and 20 done, 10 missing
        }
        // Reload: 0 and 20 remembered, 10 still missing; watermark stops at the hole.
        let p2 = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-1").unwrap();
        assert!(p2.is_done(0));
        assert!(!p2.is_done(10));
        assert!(p2.is_done(20));
        assert_eq!(p2.contiguous_watermark(), 10); // prefix ends where part@10 is missing
        assert!(!p2.all_done());
    }

    #[test]
    fn contiguous_watermark_and_all_done() {
        let target = tmp("wm");
        std::fs::write(&target, vec![0u8; 25]).unwrap();
        let mut p = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-1").unwrap();
        p.mark_done_and_persist(0).unwrap();
        assert_eq!(p.contiguous_watermark(), 10);
        p.mark_done_and_persist(10).unwrap();
        assert_eq!(p.contiguous_watermark(), 20);
        p.mark_done_and_persist(20).unwrap();
        assert_eq!(p.contiguous_watermark(), 25); // clamps to total, not 30
        assert!(p.all_done());
    }

    #[test]
    fn identity_or_size_mismatch_starts_fresh() {
        let target = tmp("ident");
        std::fs::write(&target, vec![0u8; 25]).unwrap();
        {
            let mut p = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-A").unwrap();
            p.mark_done_and_persist(0).unwrap();
        }
        // Different identity => stale sidecar ignored, fresh bitmap.
        let p2 = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-B").unwrap();
        assert!(!p2.is_done(0));
        // Different total => fresh too.
        std::fs::write(&target, vec![0u8; 40]).unwrap();
        let p3 = DownloadProgress::load_or_create(&target, 40, 10, 4, "id-A").unwrap();
        assert!(!p3.is_done(0));
    }

    #[test]
    fn missing_or_short_target_file_invalidates_sidecar() {
        let target = tmp("short");
        std::fs::write(&target, vec![0u8; 25]).unwrap();
        {
            let mut p = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-1").unwrap();
            p.mark_done_and_persist(0).unwrap();
        }
        // Truncate the target below `total`: a set bit can no longer be trusted.
        std::fs::write(&target, vec![0u8; 5]).unwrap();
        let p2 = DownloadProgress::load_or_create(&target, 25, 10, 4, "id-1").unwrap();
        assert!(
            !p2.is_done(0),
            "short target must invalidate remembered bits"
        );
    }

    #[test]
    fn serial_load_recovers_only_verified_parts_within_current_length() {
        let target = tmp("serial_partial_prefix");
        let bytes: Vec<u8> = [vec![1u8; 10], vec![2u8; 10], vec![3u8; 10]].concat();
        std::fs::write(&target, &bytes).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        for offset in [0, 10, 20] {
            first.mark_done_and_persist(offset).unwrap();
        }
        drop(first);

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        file.set_len(20).unwrap();
        file.sync_data().unwrap();
        drop(file);

        let resumed = DownloadProgress::load_or_create_serial(&target, 30, 10, 2, "id-1").unwrap();
        assert!(resumed.is_done(0));
        assert!(resumed.is_done(10));
        assert!(!resumed.is_done(20), "a part beyond EOF must be cleared");
        assert_eq!(resumed.digests[2], [0; PART_DIGEST_LEN]);

        // The strict parallel loader keeps its old all-or-nothing target-length
        // binding and must not trust even a matching prefix in a short file.
        let strict = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        assert!(!strict.is_done(0));
        assert!(!strict.is_done(10));
        assert!(!strict.is_done(20));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn serial_load_clears_a_digest_mismatch_inside_the_partial_file() {
        let target = tmp("serial_partial_rewrite");
        std::fs::write(&target, vec![4u8; 20]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(0).unwrap();
        first.mark_done_and_persist(10).unwrap();
        drop(first);

        let mut rewritten = std::fs::read(&target).unwrap();
        rewritten[15] ^= 0xff;
        std::fs::write(&target, rewritten).unwrap();
        let resumed = DownloadProgress::load_or_create_serial(&target, 20, 10, 2, "id-1").unwrap();
        assert!(resumed.is_done(0));
        assert!(!resumed.is_done(10));
        assert_eq!(resumed.digests[1], [0; PART_DIGEST_LEN]);
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn serial_load_handles_a_partial_part_boundary_and_the_short_final_part() {
        let target = tmp("serial_partial_boundaries");
        let bytes: Vec<u8> = (0..25u8).collect();
        std::fs::write(&target, &bytes).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 25, 10, 2, "id-1").unwrap();
        for offset in [0, 10, 20] {
            first.mark_done_and_persist(offset).unwrap();
        }
        drop(first);

        let complete = DownloadProgress::load_or_create_serial(&target, 25, 10, 2, "id-1").unwrap();
        assert!(complete.is_done(20), "the five-byte final part is valid");
        drop(complete);

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        file.set_len(15).unwrap();
        file.sync_data().unwrap();
        drop(file);
        let partial = DownloadProgress::load_or_create_serial(&target, 25, 10, 2, "id-1").unwrap();
        assert!(partial.is_done(0));
        assert!(
            !partial.is_done(10),
            "a bit is reusable only when its complete part ends before EOF"
        );
        assert!(!partial.is_done(20));
        let _ = partial.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn serial_load_rejects_legacy_truncated_and_oversized_bindings() {
        for (case, corrupt) in [("legacy", true), ("truncated", false)] {
            let target = tmp(&format!("serial_corrupt_{case}"));
            std::fs::write(&target, vec![5u8; 20]).unwrap();
            let mut first = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
            first.mark_done_and_persist(0).unwrap();
            drop(first);
            let sidecar = sidecar_path(&target);
            let mut raw = std::fs::read(&sidecar).unwrap();
            if corrupt {
                raw[4..6].copy_from_slice(&1u16.to_le_bytes());
            } else {
                raw.pop();
            }
            std::fs::write(&sidecar, raw).unwrap();
            let resumed =
                DownloadProgress::load_or_create_serial(&target, 20, 10, 2, "id-1").unwrap();
            assert!(!resumed.is_done(0), "{case} sidecar must start fresh");
            let _ = resumed.delete();
            let _ = std::fs::remove_file(target);
        }

        let target = tmp("serial_oversized_target");
        std::fs::write(&target, vec![6u8; 20]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(0).unwrap();
        drop(first);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        file.set_len(21).unwrap();
        drop(file);
        let resumed = DownloadProgress::load_or_create_serial(&target, 20, 10, 2, "id-1").unwrap();
        assert!(!resumed.is_done(0));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn retaining_a_serial_prefix_discards_high_bits_and_persists_the_result() {
        let target = tmp("serial_retain_prefix");
        std::fs::write(&target, vec![7u8; 30]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        progress.mark_done_and_persist(0).unwrap();
        progress.mark_done_and_persist(20).unwrap();
        assert!(progress.is_done(20));

        // The caller establishes the serial invariant before publishing the
        // compacted sidecar: visible length equals the contiguous watermark.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        file.set_len(10).unwrap();
        file.sync_data().unwrap();
        drop(file);
        let watermark = progress.retain_contiguous_prefix_after_data_sync().unwrap();
        assert_eq!(watermark, 10);
        assert!(progress.is_done(0));
        assert!(!progress.is_done(10));
        assert!(!progress.is_done(20));
        assert_eq!(progress.digests[1], [0; PART_DIGEST_LEN]);
        assert_eq!(progress.digests[2], [0; PART_DIGEST_LEN]);

        drop(progress);
        let resumed = DownloadProgress::load_or_create_serial(&target, 30, 10, 2, "id-1").unwrap();
        assert!(resumed.is_done(0));
        assert!(!resumed.is_done(10));
        assert!(!resumed.is_done(20));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn retaining_an_empty_serial_prefix_persists_without_opening_target() {
        let target = tmp("serial_retain_empty");
        std::fs::write(&target, []).unwrap();
        let mut progress =
            DownloadProgress::load_or_create_serial(&target, 30, 10, 2, "id-1").unwrap();
        std::fs::remove_file(&target).unwrap();

        assert_eq!(
            progress.retain_contiguous_prefix_after_data_sync().unwrap(),
            0
        );
        assert!(sidecar_path(&target).exists());
        assert_eq!(progress.pending_count(), 0);

        std::fs::write(&target, []).unwrap();
        let resumed = DownloadProgress::load_or_create_serial(&target, 30, 10, 2, "id-1").unwrap();
        assert_eq!(resumed.contiguous_watermark(), 0);
        assert!(!resumed.is_done(0));
        let _ = resumed.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn bulk_prefix_seed_persists_one_snapshot_for_many_tiny_parts() {
        const PARTS: usize = 100_000;
        let target = tmp("bulk_seed_tiny_parts");
        std::fs::write(&target, []).unwrap();
        let mut progress =
            DownloadProgress::fresh(&target, PARTS as u64, 1, 4, "legacy-migration").unwrap();
        std::fs::remove_file(&target).unwrap();
        let digest: [u8; PART_DIGEST_LEN] = Sha256::digest([42u8]).into();
        let verified: Vec<_> = (0..PARTS).map(|offset| (offset as u64, digest)).collect();

        progress
            .seed_verified_prefix_digests_after_data_sync(&verified)
            .unwrap();

        assert!(progress.all_done());
        assert_eq!(progress.contiguous_watermark(), PARTS as u64);
        assert_eq!(progress.checkpoint_count_for_test(), 1);
        assert!(sidecar_path(&target).exists());
        let _ = progress.delete();
    }

    #[test]
    fn bulk_prefix_seed_rejects_a_gap_without_publishing() {
        let target = tmp("bulk_seed_gap");
        let mut progress = DownloadProgress::fresh(&target, 30, 10, 2, "migration").unwrap();
        let digest = [9u8; PART_DIGEST_LEN];
        let error = progress
            .seed_verified_prefix_digests_after_data_sync(&[(0, digest), (20, digest)])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(progress.contiguous_watermark(), 0);
        assert!(!sidecar_path(&target).exists());
    }

    #[test]
    fn large_part_grids_have_a_bounded_snapshot_count_and_no_timer() {
        const PARTS: usize = 100_000;
        let target = tmp("bounded_snapshots");
        let mut progress = DownloadProgress::fresh(&target, PARTS as u64, 1, 8, "id-1").unwrap();
        let digest = [3u8; PART_DIGEST_LEN];
        let mut snapshots = 0usize;

        for offset in 0..PARTS as u64 {
            let outcome = progress
                .stage_done_with_digest_deferred(offset, digest)
                .unwrap();
            assert!(!outcome.arm_timer, "large grids must not arm the timer");
            if outcome.checkpoint_due {
                assert!(progress.begin_external_checkpoint().unwrap());
                progress.commit_checkpoint_after_data_sync().unwrap();
                snapshots += 1;
            }
        }
        if progress.begin_external_checkpoint().unwrap() {
            progress.commit_checkpoint_after_data_sync().unwrap();
            snapshots += 1;
        }

        assert!(progress.all_done());
        assert!(snapshots <= 17, "snapshot count was {snapshots}");
        assert_eq!(progress.checkpoint_count_for_test(), snapshots);
        let _ = progress.delete();
    }

    #[test]
    fn part_count_rejects_zero_chunk_and_unbounded_bitmap() {
        assert_eq!(part_count_for(0, 1).expect("empty download"), 0);
        assert_eq!(part_count_for(25, 10).expect("normal part count"), 3);
        assert_eq!(
            part_count_for(1, 0)
                .expect_err("zero chunk must be rejected")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            part_count_for(u64::MAX, 1)
                .expect_err("an unbounded bitmap must be rejected before allocation")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn thirty_two_bit_checkpoint_part_cap_stays_within_the_memory_budget() {
        let cap_32 = part_count_limit_for_pointer_width(32);

        assert!(cap_32 < MAX_PART_COUNT);
        assert_eq!(part_count_limit_for_pointer_width(64), MAX_PART_COUNT);
        assert!(
            checkpoint_peak_upper_bound_bytes(cap_32).unwrap() <= CHECKPOINT_MEMORY_LIMIT_32_BIT
        );
        assert_eq!(
            part_count_for_pointer_width(cap_32 + 1, 1, 32)
                .expect_err("a 32-bit checkpoint grid must stay within its memory budget")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            part_count_for_pointer_width(MAX_PART_COUNT, 1, 64)
                .expect("64-bit targets retain the existing sidecar limit"),
            MAX_PART_COUNT as usize
        );
    }

    #[test]
    fn checkpoint_peak_estimate_overflow_is_a_controlled_error() {
        assert_eq!(
            checkpoint_peak_upper_bound_bytes(u64::MAX)
                .expect_err("peak estimation must not wrap")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn oversized_identity_is_a_controlled_error() {
        let target = tmp("oversized_identity");
        std::fs::write(&target, [0u8]).unwrap();
        let identity = "x".repeat(u16::MAX as usize + 1);
        let err = DownloadProgress::load_or_create(&target, 1, 1, 4, &identity)
            .err()
            .expect("identity length must not truncate to u16");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn expected_v2_length_matches_the_encoder() {
        let target = tmp("expected_v2_length");
        let progress = DownloadProgress::fresh(&target, 25, 10, 4, "id-一").unwrap();
        let encoded = progress
            .encode_snapshot(&progress.bits, &progress.digests)
            .unwrap();

        assert_eq!(
            expected_v2_encoded_len(25, 10, "id-一").unwrap(),
            encoded.len()
        );
        let current_part_limit = part_count_limit_for_pointer_width(usize::BITS);
        assert!(
            expected_v2_encoded_len(current_part_limit, 1, "id").unwrap() < 33 * 1024 * 1024,
            "the part-count cap must also bound every legitimate read buffer"
        );
    }

    #[test]
    fn bounded_reader_rejects_growth_after_metadata_precheck() {
        let target = tmp("sidecar_growth_after_metadata");
        std::fs::write(&target, vec![4u8; 20]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        progress.mark_done_and_persist(0).unwrap();
        drop(progress);

        let sidecar = sidecar_path(&target);
        let expected_len = expected_v2_encoded_len(20, 10, "id-1").unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&sidecar)
            .unwrap();
        assert_eq!(file.metadata().unwrap().len(), expected_len as u64);

        // Simulate growth after the loader's same-handle metadata precheck.
        file.seek(io::SeekFrom::End(0)).unwrap();
        file.write_all(&[0x7f]).unwrap();
        file.seek(io::SeekFrom::Start(0)).unwrap();
        assert!(
            read_sidecar_bounded_from_open_file(file, expected_len)
                .unwrap()
                .is_none(),
            "the sentinel byte must reject trailing data introduced by a race"
        );

        let _ = std::fs::remove_file(sidecar);
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn both_loaders_reject_a_trailing_byte_and_start_fresh() {
        let target = tmp("sidecar_trailing_byte");
        std::fs::write(&target, vec![6u8; 20]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(0).unwrap();
        drop(first);

        let sidecar = sidecar_path(&target);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&sidecar)
            .unwrap();
        file.write_all(&[0]).unwrap();
        drop(file);

        let parallel = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        assert!(!parallel.is_done(0));
        drop(parallel);
        let serial = DownloadProgress::load_or_create_serial(&target, 20, 10, 2, "id-1").unwrap();
        assert!(!serial.is_done(0));

        let _ = serial.delete();
        let _ = std::fs::remove_file(target);
    }

    #[cfg(unix)]
    #[test]
    fn loader_rejects_a_sidecar_symlink_without_reading_its_target() {
        use std::os::unix::fs::symlink;

        let target = tmp("sidecar_symlink");
        std::fs::write(&target, vec![6u8; 20]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(0).unwrap();
        drop(first);

        let sidecar = sidecar_path(&target);
        let external = target.with_extension("external-sidecar");
        std::fs::rename(&sidecar, &external).unwrap();
        let sentinel = std::fs::read(&external).unwrap();
        symlink(&external, &sidecar).unwrap();

        let error = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1")
            .err()
            .expect("a formal sidecar symlink must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&external).unwrap(), sentinel);
        assert!(std::fs::symlink_metadata(&sidecar)
            .unwrap()
            .file_type()
            .is_symlink());

        let _ = std::fs::remove_file(sidecar);
        let _ = std::fs::remove_file(external);
        let _ = std::fs::remove_file(target);
    }

    #[cfg(unix)]
    #[test]
    fn oversized_sparse_sidecar_is_rejected_without_reading_its_logical_size() {
        let target = tmp("oversized_sparse_sidecar");
        std::fs::write(&target, vec![8u8; 20]).unwrap();
        let sidecar = sidecar_path(&target);
        ensure_sidecar_namespace(&sidecar).unwrap();
        let sparse = std::fs::File::create(&sidecar).unwrap();
        const SPARSE_LOGICAL_LEN: u64 = 1 << 40;
        sparse.set_len(SPARSE_LOGICAL_LEN).unwrap();
        drop(sparse);

        let parallel = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        assert!(!parallel.is_done(0));
        drop(parallel);
        let serial = DownloadProgress::load_or_create_serial(&target, 20, 10, 2, "id-1").unwrap();
        assert!(!serial.is_done(0));
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().len(),
            SPARSE_LOGICAL_LEN,
            "the loader must reject by length instead of consuming the sparse file"
        );

        let _ = serial.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn changing_only_in_flight_window_preserves_durable_parts() {
        let target = tmp("window_not_identity");
        std::fs::write(&target, vec![0u8; 25]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 25, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(0).unwrap();
        drop(first);

        let resumed = DownloadProgress::load_or_create(&target, 25, 10, 8, "id-1").unwrap();
        assert!(
            resumed.is_done(0),
            "max_parts_in_flight is scheduling policy, not content identity"
        );
    }

    #[test]
    fn truncated_bitmap_is_rejected_instead_of_trusting_a_prefix() {
        let target = tmp("truncated_bitmap");
        std::fs::write(&target, vec![0u8; 90]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 90, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(0).unwrap();
        first.mark_done_and_persist(80).unwrap();
        drop(first);

        let sidecar = sidecar_path(&target);
        let mut raw = std::fs::read(&sidecar).unwrap();
        raw.pop();
        std::fs::write(&sidecar, raw).unwrap();

        let resumed = DownloadProgress::load_or_create(&target, 90, 10, 2, "id-1").unwrap();
        assert!(!resumed.is_done(0));
        assert!(!resumed.is_done(80));
    }

    #[test]
    fn same_length_local_rewrite_invalidates_the_changed_completed_part() {
        let target = tmp("same_length_rewrite");
        std::fs::write(&target, vec![1u8; 30]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(0).unwrap();
        first.mark_done_and_persist(20).unwrap();
        drop(first);

        let mut rewritten = vec![1u8; 30];
        rewritten[5] = 9;
        std::fs::write(&target, rewritten).unwrap();

        let resumed = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        assert!(
            !resumed.is_done(0),
            "the digest must detect a same-length rewrite inside part 0"
        );
        assert!(
            resumed.is_done(20),
            "an independently verified unchanged part may still be reused"
        );
    }

    #[test]
    fn final_validation_rejects_rewrite_after_last_checkpoint() {
        let target = tmp("final_rewrite");
        std::fs::write(&target, vec![8u8; 20]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        progress.mark_done_and_persist(0).unwrap();
        progress.mark_done_and_persist(10).unwrap();
        assert!(progress.all_done());
        progress.validate_committed_content().unwrap();

        let mut changed = std::fs::read(&target).unwrap();
        changed[15] ^= 0xff;
        std::fs::write(&target, changed).unwrap();
        let err = progress.validate_committed_content().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let _ = progress.delete();
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn final_validation_accepts_same_content_replacement_and_classifies_length_loss() {
        let target = tmp("final_same_content_replacement");
        let bytes: Vec<u8> = (0..20u8).collect();
        std::fs::write(&target, &bytes).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        progress.mark_done_and_persist(0).unwrap();
        progress.mark_done_and_persist(10).unwrap();

        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, &bytes).unwrap();
        progress
            .validate_committed_content()
            .expect("content identity must allow a same-content replacement");

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        file.set_len(19).unwrap();
        drop(file);
        assert_eq!(
            progress.validate_committed_content().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        std::fs::remove_file(&target).unwrap();
        assert_eq!(
            progress.validate_committed_content().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let _ = progress.delete();
    }

    #[test]
    fn completed_part_digest_checks_first_middle_and_last_byte() {
        for (case, changed_index) in [("first", 0usize), ("middle", 5), ("last", 9)] {
            let target = tmp(case);
            std::fs::write(&target, vec![3u8; 10]).unwrap();
            let mut first = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
            first.mark_done_and_persist(0).unwrap();
            drop(first);

            let mut bytes = std::fs::read(&target).unwrap();
            bytes[changed_index] ^= 0xff;
            std::fs::write(&target, bytes).unwrap();

            let resumed = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
            assert!(
                !resumed.is_done(0),
                "{case} byte corruption must invalidate the completed part"
            );
        }
    }

    #[test]
    fn tampered_digest_and_legacy_or_unknown_versions_start_safe() {
        for (case, mutate) in [("digest", 0u8), ("v1", 1u8), ("unknown", u8::MAX)] {
            let target = tmp(case);
            std::fs::write(&target, vec![4u8; 10]).unwrap();
            let mut first = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
            first.mark_done_and_persist(0).unwrap();
            drop(first);

            let sidecar = sidecar_path(&target);
            let mut raw = std::fs::read(&sidecar).unwrap();
            if case == "digest" {
                let last = raw.last_mut().unwrap();
                *last ^= 0xff;
            } else {
                raw[4..6].copy_from_slice(&(mutate as u16).to_le_bytes());
            }
            std::fs::write(&sidecar, raw).unwrap();

            let resumed = DownloadProgress::load_or_create(&target, 10, 10, 2, "id-1").unwrap();
            assert!(!resumed.is_done(0), "{case} sidecar must not be trusted");
        }
    }

    #[test]
    fn forged_identity_length_is_rejected_before_using_that_length() {
        let target = tmp("forged_identity_length");
        let progress = DownloadProgress::fresh(&target, 10, 10, 2, "id-1").unwrap();
        let mut raw = progress
            .encode_snapshot(&progress.bits, &progress.digests)
            .unwrap();
        raw[26..28].copy_from_slice(&u16::MAX.to_le_bytes());

        assert!(DownloadProgress::decode(&raw, 10, 10, 2, "id-1").is_none());

        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn truncate_then_expand_to_original_length_does_not_restore_old_bits() {
        let target = tmp("truncate_expand");
        std::fs::write(&target, vec![5u8; 30]).unwrap();
        let mut first = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        first.mark_done_and_persist(20).unwrap();
        drop(first);

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        file.set_len(5).unwrap();
        file.set_len(30).unwrap();
        drop(file);

        let resumed = DownloadProgress::load_or_create(&target, 30, 10, 2, "id-1").unwrap();
        assert!(!resumed.is_done(20));
    }

    #[test]
    fn unaligned_part_offset_is_rejected_without_setting_a_neighbor_bit() {
        let target = tmp("unaligned");
        std::fs::write(&target, vec![6u8; 20]).unwrap();
        let mut progress = DownloadProgress::load_or_create(&target, 20, 10, 2, "id-1").unwrap();
        let err = progress.mark_done_and_persist(5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!progress.is_done(0));
        assert!(!progress.is_done(10));
    }
}
