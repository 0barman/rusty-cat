//! Crash-resumable progress for the concurrent download path.
//!
//! A `<target>.rcdl` sidecar records which fixed-size parts of a pre-sized
//! download file are durably written, so an interrupted concurrent download
//! resumes by re-fetching only the missing parts. The sidecar is bound to the
//! target by `(identity, total, chunk)` and by the target's on-disk
//! length; any mismatch is treated as a fresh download. Recovery verifies each
//! completed part from disk before trusting its bit, and all allocation/offset
//! calculations return controlled errors instead of panicking or saturating.
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
const PART_DIGEST_LEN: usize = 32;
/// Bound sidecar bitmap work independently of address width. A million parts
/// already permits multi-terabyte downloads at ordinary chunk sizes while
/// keeping decode, scans and future per-part verification metadata bounded.
const MAX_PART_COUNT: u64 = 1_000_000;
const DEFAULT_CHECKPOINT_BATCH_PARTS: usize = 8;
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
    checkpoint_threshold: usize,
    last_checkpoint: Instant,
    checkpoint_timer_armed: bool,
    #[cfg(test)]
    checkpoint_count: usize,
    #[cfg(test)]
    fail_next_checkpoint: Option<CheckpointFailurePoint>,
}

/// Returns the sidecar path for a download target (`<target>.rcdl`).
pub(crate) fn sidecar_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(".rcdl");
    PathBuf::from(s)
}

fn part_count_for(total: u64, chunk: u64) -> io::Result<usize> {
    if chunk == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "download chunk size must be greater than zero",
        ));
    }
    let parts = total.div_ceil(chunk);
    if parts > MAX_PART_COUNT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("download requires {parts} parts, exceeding sidecar limit {MAX_PART_COUNT}"),
        ));
    }
    usize::try_from(parts).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download part count does not fit this target architecture",
        )
    })
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

fn digest_file_range(target: &Path, offset: u64, len: u64) -> io::Result<[u8; PART_DIGEST_LEN]> {
    let mut file = std::fs::File::open(target)?;
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
            checkpoint_threshold: DEFAULT_CHECKPOINT_BATCH_PARTS,
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
        cleanup_stale_checkpoint_temps(&fresh.sidecar);
        let raw = match std::fs::read(&fresh.sidecar) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(fresh),
            Err(e) => return Err(e),
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
        let mut id = vec![0u8; id_len];
        cur.read_exact(&mut id).ok()?;
        if id.as_slice() != identity.as_bytes() {
            return None;
        }
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
        let capacity = 28usize
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

    fn verify_completed_parts(&mut self) -> io::Result<()> {
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
            let actual = digest_file_range(&self.target, offset, len)?;
            if self.digests.get(index).copied() != Some(actual) {
                if let Some(byte) = self.bits.get_mut(index >> 3) {
                    *byte &= !(1u8 << (index & 7));
                }
                if let Some(digest) = self.digests.get_mut(index) {
                    *digest = [0; PART_DIGEST_LEN];
                }
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

    pub(crate) fn stage_done_with_digest(
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
                arm_timer: false,
            });
        }
        if self.pending.iter().any(|part| part.index == i) {
            // The previous attempt staged this part but its checkpoint failed.
            // A transfer retry must retry the durability step as well; silently
            // accepting the duplicate would turn a transient fsync error into
            // an uncommitted success.
            self.force_checkpoint()?;
            return Ok(StageDoneOutcome {
                #[cfg(test)]
                checkpointed: true,
                arm_timer: false,
            });
        }
        self.pending.try_reserve(1).map_err(|e| {
            io::Error::other(format!("cannot allocate pending checkpoint entry: {e}"))
        })?;
        self.pending.push(PendingPart { index: i, digest });
        let should_checkpoint = self.pending.len() >= self.checkpoint_threshold.max(1)
            || self.last_checkpoint.elapsed() >= DEFAULT_CHECKPOINT_INTERVAL;
        if should_checkpoint {
            self.force_checkpoint()?;
            Ok(StageDoneOutcome {
                #[cfg(test)]
                checkpointed: true,
                arm_timer: false,
            })
        } else {
            let arm_timer = !self.checkpoint_timer_armed;
            self.checkpoint_timer_armed = true;
            Ok(StageDoneOutcome {
                #[cfg(test)]
                checkpointed: false,
                arm_timer,
            })
        }
    }

    /// Called by the one-shot task timer. A threshold/final checkpoint may
    /// already have disarmed it; in that case the stale wake-up is a no-op.
    pub(crate) fn checkpoint_timer_fired(&mut self) -> io::Result<()> {
        if !self.checkpoint_timer_armed {
            return Ok(());
        }
        self.checkpoint_timer_armed = false;
        self.force_checkpoint()
    }

    /// Freezes and commits the current epoch. No sidecar bit changes until the
    /// target data barrier and the atomically replaced sidecar are both durable.
    pub(crate) fn force_checkpoint(&mut self) -> io::Result<()> {
        self.checkpoint_timer_armed = false;
        if self.pending.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        self.fail_if_requested(CheckpointFailurePoint::BeforeDataBarrier)?;

        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.target)?
            .sync_data()?;

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
        self.last_checkpoint = Instant::now();
        #[cfg(test)]
        {
            self.checkpoint_count += 1;
        }
        Ok(())
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
        let mut tmp = self.sidecar.as_os_str().to_os_string();
        let sequence = CHECKPOINT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        tmp.push(format!(".tmp.{}.{}", std::process::id(), sequence));
        let tmp = PathBuf::from(tmp);
        let result = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            #[cfg(test)]
            self.fail_if_requested(CheckpointFailurePoint::AfterSidecarFsync)?;
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
            let actual = digest_file_range(&self.target, offset, len)?;
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
        match std::fs::remove_file(&self.sidecar) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether a (possibly stale) sidecar exists next to `target`.
    pub(crate) fn sidecar_exists(target: &Path) -> bool {
        sidecar_path(target).exists()
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

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let mut source_wide: Vec<u16> = source.as_os_str().encode_wide().collect();
    source_wide.push(0);
    let mut destination_wide: Vec<u16> = destination.as_os_str().encode_wide().collect();
    destination_wide.push(0);
    let ok = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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
        let mut p = std::env::temp_dir();
        p.push(format!("rcdl_{name}_{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(sidecar_path(&p));
        p
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
