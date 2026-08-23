use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use sha2::{Digest, Sha256};

struct LeaseFile {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

/// Cross-client and cross-process exclusive ownership for a download target.
///
/// The lock files may remain after a crash, but the OS advisory lock is tied to
/// the open descriptor and is therefore released automatically. Existing files
/// also acquire a device/inode key on Unix, making symlink and hardlink aliases
/// contend with the canonical path key.
pub(crate) struct TargetLease {
    files: Vec<LeaseFile>,
}

impl TargetLease {
    pub(crate) fn acquire(target: &Path) -> io::Result<Self> {
        let directory = std::env::temp_dir().join("rusty-cat-target-leases-v1");
        std::fs::create_dir_all(&directory)?;
        let mut keys = lease_keys(target)?;
        keys.sort();
        keys.dedup();

        let mut files = Vec::new();
        files
            .try_reserve_exact(keys.len())
            .map_err(|e| io::Error::other(format!("cannot allocate target lease handles: {e}")))?;
        for key in keys {
            let digest = Sha256::digest(&key);
            let mut name = String::with_capacity(digest.len() * 2 + 5);
            for byte in digest {
                use std::fmt::Write as _;
                write!(&mut name, "{byte:02x}")
                    .map_err(|_| io::Error::other("format target lease digest failed"))?;
            }
            name.push_str(".lock");
            let path = directory.join(name);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            file.try_lock_exclusive().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "download target is already owned by another task: {} ({e})",
                        target.display()
                    ),
                )
            })?;
            files.push(LeaseFile { file, path });
        }
        Ok(Self { files })
    }
}

impl Drop for TargetLease {
    fn drop(&mut self) {
        for lease in &self.files {
            let _ = FileExt::unlock(&lease.file);
        }
    }
}

fn lease_keys(target: &Path) -> io::Result<Vec<Vec<u8>>> {
    let canonical = match std::fs::canonicalize(target) {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let parent = target.parent().unwrap_or_else(|| Path::new("."));
            let canonical_parent = std::fs::canonicalize(parent)?;
            match target.file_name() {
                Some(name) => canonical_parent.join(name),
                None => canonical_parent,
            }
        }
        Err(e) => return Err(e),
    };

    let mut keys = Vec::new();
    keys.push(path_key(&canonical));
    #[cfg(target_os = "macos")]
    {
        // Most macOS volumes are case-insensitive, while their POSIX path bytes
        // preserve case. Lock a conservative Unicode-lowercased alias in
        // addition to the exact key so two not-yet-created targets such as
        // `Report.bin` and `report.bin` cannot be written concurrently. On a
        // case-sensitive macOS volume this only reduces concurrency; it cannot
        // merge or corrupt files.
        keys.push(
            format!(
                "path-casefold:{}",
                canonical.to_string_lossy().to_lowercase()
            )
            .into_bytes(),
        );
    }
    if let Ok(metadata) = std::fs::metadata(target) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            keys.push(format!("inode:{}:{}", metadata.dev(), metadata.ino()).into_bytes());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if let (Some(volume), Some(index)) =
                (metadata.volume_serial_number(), metadata.file_index())
            {
                keys.push(format!("file-id:{volume}:{index}").into_bytes());
            }
        }
    }
    Ok(keys)
}

#[cfg(unix)]
fn path_key(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut key = b"path:".to_vec();
    key.extend_from_slice(path.as_os_str().as_bytes());
    key
}

#[cfg(windows)]
fn path_key(path: &Path) -> Vec<u8> {
    format!("path:{}", path.to_string_lossy().to_lowercase()).into_bytes()
}

#[cfg(not(any(unix, windows)))]
fn path_key(path: &Path) -> Vec<u8> {
    format!("path:{}", path.to_string_lossy()).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::TargetLease;

    const CHILD_MODE: &str = "RUSTY_CAT_TARGET_LEASE_CHILD_MODE";
    const CHILD_TARGET: &str = "RUSTY_CAT_TARGET_LEASE_CHILD_TARGET";
    const CHILD_READY: &str = "RUSTY_CAT_TARGET_LEASE_CHILD_READY";

    fn temp_path(case: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rusty_cat_target_lease_{case}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        path
    }

    #[test]
    fn same_target_has_one_owner_and_drop_releases_it() {
        let target = temp_path("same");
        std::fs::write(&target, b"fixture").expect("fixture");
        let first = TargetLease::acquire(&target).expect("first owner");
        assert!(TargetLease::acquire(&target).is_err());
        drop(first);
        TargetLease::acquire(&target).expect("stale OS lock released on drop");
        let _ = std::fs::remove_file(target);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_aliases_share_the_same_inode_lease() {
        let target = temp_path("aliases");
        let symlink = target.with_extension("symlink");
        let hardlink = target.with_extension("hardlink");
        std::fs::write(&target, b"fixture").expect("fixture");
        std::os::unix::fs::symlink(&target, &symlink).expect("symlink");
        std::fs::hard_link(&target, &hardlink).expect("hardlink");

        let owner = TargetLease::acquire(&target).expect("owner");
        assert!(TargetLease::acquire(&symlink).is_err());
        assert!(TargetLease::acquire(&hardlink).is_err());
        drop(owner);

        let _ = std::fs::remove_file(symlink);
        let _ = std::fs::remove_file(hardlink);
        let _ = std::fs::remove_file(target);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn differently_cased_nonexistent_targets_share_a_conservative_lease() {
        let mixed = temp_path("CaseAlias");
        let folded = mixed.with_file_name(
            mixed
                .file_name()
                .expect("target file name")
                .to_string_lossy()
                .to_lowercase(),
        );
        assert_ne!(mixed, folded);

        let owner = TargetLease::acquire(&mixed).expect("mixed-case owner");
        assert!(
            TargetLease::acquire(&folded).is_err(),
            "macOS case aliases must not acquire two download owners"
        );
        drop(owner);
    }

    #[test]
    fn cross_process_owner_excludes_contender_and_killed_owner_is_not_stale() {
        if let Ok(mode) = std::env::var(CHILD_MODE) {
            let target = std::path::PathBuf::from(
                std::env::var_os(CHILD_TARGET).expect("child target path"),
            );
            match mode.as_str() {
                "contender" => {
                    assert!(TargetLease::acquire(&target).is_err());
                }
                "owner" => {
                    let _lease = TargetLease::acquire(&target).expect("child owner");
                    let ready = std::path::PathBuf::from(
                        std::env::var_os(CHILD_READY).expect("child ready path"),
                    );
                    std::fs::write(ready, b"ready").expect("publish child readiness");
                    std::thread::sleep(std::time::Duration::from_secs(30));
                }
                unexpected => panic!("unexpected child mode: {unexpected}"),
            }
            return;
        }

        let target = temp_path("cross_process");
        let ready = target.with_extension("ready");
        std::fs::write(&target, b"fixture").expect("fixture");

        let parent_owner = TargetLease::acquire(&target).expect("parent owner");
        let contender = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                "target_lease::tests::cross_process_owner_excludes_contender_and_killed_owner_is_not_stale",
            ])
            .env(CHILD_MODE, "contender")
            .env(CHILD_TARGET, &target)
            .status()
            .expect("run child contender");
        assert!(contender.success(), "child must observe the parent's lock");
        drop(parent_owner);

        let mut owner = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                "target_lease::tests::cross_process_owner_excludes_contender_and_killed_owner_is_not_stale",
            ])
            .env(CHILD_MODE, "owner")
            .env(CHILD_TARGET, &target)
            .env(CHILD_READY, &ready)
            .spawn()
            .expect("spawn child owner");
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready.exists(), "child owner did not acquire the lease");
        assert!(TargetLease::acquire(&target).is_err());
        owner.kill().expect("kill child owner");
        owner.wait().expect("reap child owner");

        TargetLease::acquire(&target).expect("OS releases a killed owner's advisory lock");
        let _ = std::fs::remove_file(ready);
        let _ = std::fs::remove_file(target);
    }
}
