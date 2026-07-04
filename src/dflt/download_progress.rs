//! Crash-resumable progress for the concurrent download path.
//!
//! A `<target>.rcdl` sidecar records which fixed-size parts of a pre-sized
//! download file are durably written, so an interrupted concurrent download
//! resumes by re-fetching only the missing parts. The sidecar is bound to the
//! target by `(identity, total, chunk, max_parts)` and by the target's on-disk
//! length; any mismatch is treated as a fresh download. This module performs
//! only tiny synchronous file I/O and never panics.
//!
//! `DownloadProgress` is stored on `TransferTask` and driven by the concurrent
//! download path in `DefaultHttpTransfer` (prepare pre-sizes + loads the
//! sidecar, each part marks its bit, and complete validates + deletes it).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: u32 = 0x5243_444C; // "RCDL"
const VERSION: u16 = 1;

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
    max_parts: usize,
    identity: String,
    part_count: usize,
    /// One bit per part; `bits[i >> 3] & (1 << (i & 7))`.
    bits: Vec<u8>,
}

/// Returns the sidecar path for a download target (`<target>.rcdl`).
pub(crate) fn sidecar_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(".rcdl");
    PathBuf::from(s)
}

fn part_count_for(total: u64, chunk: u64) -> usize {
    let chunk = chunk.max(1);
    // ceil(total / chunk), saturating into usize.
    let parts = total.div_ceil(chunk);
    usize::try_from(parts).unwrap_or(usize::MAX)
}

impl DownloadProgress {
    /// Part index for a chunk-aligned start offset.
    fn index_of(&self, offset: u64) -> Option<usize> {
        let chunk = self.chunk.max(1);
        let idx = offset / chunk;
        usize::try_from(idx).ok().filter(|i| *i < self.part_count)
    }

    fn fresh(target: &Path, total: u64, chunk: u64, max_parts: usize, identity: &str) -> Self {
        let part_count = part_count_for(total, chunk);
        let byte_len = part_count.div_ceil(8);
        Self {
            target: target.to_path_buf(),
            sidecar: sidecar_path(target),
            total,
            chunk: chunk.max(1),
            max_parts,
            identity: identity.to_string(),
            part_count,
            bits: vec![0u8; byte_len],
        }
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
        let mut fresh = Self::fresh(target, total, chunk, max_parts, identity);
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
        let raw = match std::fs::read(&fresh.sidecar) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(fresh),
            Err(e) => return Err(e),
        };
        match Self::decode(&raw, total, chunk, max_parts, identity) {
            Some(bits) => {
                fresh.bits = bits;
                Ok(fresh)
            }
            None => Ok(fresh), // corrupt / mismatched header => start over
        }
    }

    /// header: MAGIC u32 | VERSION u16 | total u64 | chunk u64 | max_parts u32
    ///         | id_len u16 | id bytes | bitmap bytes
    fn decode(
        raw: &[u8],
        total: u64,
        chunk: u64,
        max_parts: usize,
        identity: &str,
    ) -> Option<Vec<u8>> {
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
        if u64::from_le_bytes(u64b) != chunk.max(1) {
            return None;
        }
        cur.read_exact(&mut u32b).ok()?;
        if u32::from_le_bytes(u32b) as usize != max_parts {
            return None;
        }
        cur.read_exact(&mut u16b).ok()?;
        let id_len = u16::from_le_bytes(u16b) as usize;
        let mut id = vec![0u8; id_len];
        cur.read_exact(&mut id).ok()?;
        if id.as_slice() != identity.as_bytes() {
            return None;
        }
        let part_count = part_count_for(total, chunk);
        let byte_len = part_count.div_ceil(8);
        let mut bits = vec![0u8; byte_len];
        // A short bitmap is tolerated (treated as trailing-missing) but a header
        // that decodes to a different byte length is a mismatch.
        let read = cur.read(&mut bits).ok()?;
        if read == 0 && byte_len > 0 {
            return None;
        }
        Some(bits)
    }

    fn encode(&self) -> Vec<u8> {
        let id = self.identity.as_bytes();
        let mut out = Vec::with_capacity(28 + id.len() + self.bits.len());
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.total.to_le_bytes());
        out.extend_from_slice(&self.chunk.to_le_bytes());
        out.extend_from_slice(&(self.max_parts as u32).to_le_bytes());
        out.extend_from_slice(&(id.len() as u16).to_le_bytes());
        out.extend_from_slice(id);
        out.extend_from_slice(&self.bits);
        out
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

    /// Sets the bit for `offset` and atomically rewrites the sidecar (temp file,
    /// fsync, then rename). Caller MUST have durably written and `sync_data`'d
    /// the part's bytes BEFORE calling this, so a set bit always implies good
    /// bytes.
    pub(crate) fn mark_done_and_persist(&mut self, offset: u64) -> io::Result<()> {
        if let Some(i) = self.index_of(offset) {
            if let Some(byte) = self.bits.get_mut(i >> 3) {
                *byte |= 1u8 << (i & 7);
            }
        }
        self.persist_atomic()
    }

    fn persist_atomic(&self) -> io::Result<()> {
        let bytes = self.encode();
        let mut tmp = self.sidecar.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.sidecar)?;
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
            wm = (wm + self.chunk).min(self.total);
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
        assert!(!p2.is_done(0), "short target must invalidate remembered bits");
    }
}
