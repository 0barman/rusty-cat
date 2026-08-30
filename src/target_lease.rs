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
/// contend with the canonical path key. Other platforms rely on the actual
/// target-file lock below for aliases instead of unstable file-identity APIs.
pub(crate) struct TargetLease {
    files: Vec<LeaseFile>,
}

impl TargetLease {
    pub(crate) fn acquire(target: &Path) -> io::Result<Self> {
        crate::dflt::download_progress::ensure_target_outside_sidecar_namespace(target)?;
        let directory = std::env::temp_dir().join("rusty-cat-target-leases-v1");
        std::fs::create_dir_all(&directory)?;
        let mut keys = lease_keys(target)?;
        // A target and its private resume sidecar share one ownership domain.
        // This matters when another task deliberately chooses the first task's
        // hashed sidecar path as its visible target: neither task may write
        // that path while the other owns it.
        keys.extend(lease_keys(&crate::dflt::download_progress::sidecar_path(
            target,
        ))?);
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
            file.try_lock_exclusive().map_err(|error| {
                let contended = fs2::lock_contended_error();
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.raw_os_error() == contended.raw_os_error()
                {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "download target is already owned by another task: {} ({error})",
                            target.display()
                        ),
                    )
                } else {
                    error
                }
            })?;
            files.push(LeaseFile { file, path });
        }
        // Recheck after acquiring the union of target + generated-sidecar keys
        // as defense in depth for a path whose existing symlink prefix changed
        // during lease setup. The `.rusty-cat` component itself is reserved
        // unconditionally, so namespace ownership-marker timing is irrelevant.
        crate::dflt::download_progress::ensure_target_outside_sidecar_namespace(target)?;
        Ok(Self { files })
    }
}

/// Opens and exclusively locks the actual download target.
///
/// The returned handle owns the OS lock and must be the handle used for all
/// transfer I/O. This matters on Windows, where a byte-range lock held through
/// one handle also blocks the locking process from accessing the range through
/// a second handle. Closing the returned file releases the lock after every
/// terminal task path, including process termination.
pub(crate) fn open_locked_target(target: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // Stable std API, expressed as the documented Win32 share flags. Keep
        // read/write sharing so another cooperative opener can observe the
        // advisory lock, but omit FILE_SHARE_DELETE so the visible path cannot
        // be renamed or deleted while completion still depends on this handle.
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options.open(target)?;
    file.try_lock_exclusive().map_err(|error| {
        let contended = fs2::lock_contended_error();
        if error.kind() == io::ErrorKind::WouldBlock
            || error.raw_os_error() == contended.raw_os_error()
        {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "download target is locked through another path or process: {} ({error})",
                    target.display()
                ),
            )
        } else {
            error
        }
    })?;
    Ok(file)
}

impl Drop for TargetLease {
    fn drop(&mut self) {
        for lease in &self.files {
            let _ = FileExt::unlock(&lease.file);
        }
    }
}

fn lease_keys(target: &Path) -> io::Result<Vec<Vec<u8>>> {
    let canonical = normalize_lease_path(target)?;

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
        #[cfg(not(unix))]
        let _ = metadata;
    }
    Ok(keys)
}

/// Canonicalizes the longest existing prefix and appends every missing path
/// component losslessly. Sidecar namespaces are intentionally created only at
/// the first checkpoint, so lease acquisition must also normalize paths whose
/// immediate parent does not exist yet.
fn normalize_lease_path(path: &Path) -> io::Result<PathBuf> {
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
    use super::{open_locked_target, TargetLease};
    use std::io::{Read, Seek, Write};

    const CHILD_MODE: &str = "RUSTY_CAT_TARGET_LEASE_CHILD_MODE";
    const CHILD_TARGET: &str = "RUSTY_CAT_TARGET_LEASE_CHILD_TARGET";
    const CHILD_READY: &str = "RUSTY_CAT_TARGET_LEASE_CHILD_READY";
    const ACTUAL_LOCK_TEST: &str =
        "target_lease::tests::cross_process_actual_target_lock_covers_hardlink_alias_and_process_exit";

    fn temp_path(case: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rusty_cat_target_lease_tests_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("target lease test root");
        root.join(format!(
            "rusty_cat_target_lease_{case}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn reserved_sidecar_namespace_cannot_be_used_as_a_download_target() {
        let root = temp_path("reserved_namespace_target");
        let namespace = root.join(".rusty-cat");
        std::fs::create_dir_all(&namespace).expect("namespace fixture");
        std::fs::write(namespace.join(".download-state-v1"), []).expect("ownership marker");
        let target = namespace.join("user-visible.bin");
        std::fs::write(&target, b"checkpoint sentinel").expect("checkpoint fixture");

        let error = TargetLease::acquire(&target)
            .err()
            .expect("reserved sidecar namespace must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("reserved"));
        assert_eq!(
            std::fs::read(&target).expect("sentinel must remain"),
            b"checkpoint sentinel",
            "rejection must not overwrite an existing checkpoint"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unowned_directory_named_rusty_cat_is_still_reserved_for_checkpoint_claims() {
        let root = temp_path("unowned_namespace_target");
        let namespace = root.join(".rusty-cat");
        std::fs::create_dir_all(&namespace).expect("user directory fixture");
        let target = namespace.join("user-visible.bin");

        let error = TargetLease::acquire(&target)
            .err()
            .expect("every reserved namespace component must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_into_owned_namespace_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = temp_path("owned_namespace_symlink_alias");
        let namespace = root.join("real").join(".rusty-cat");
        std::fs::create_dir_all(&namespace).expect("namespace fixture");
        std::fs::write(namespace.join(".download-state-v1"), []).expect("ownership marker");
        std::fs::create_dir_all(root.join("visible")).expect("alias parent");
        let alias = root.join("visible").join("checkpoint-alias");
        symlink(&namespace, &alias).expect("namespace alias");

        let error = TargetLease::acquire(&alias.join("part.rcdl"))
            .err()
            .expect("resolved owned namespace alias must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_final_symlink_into_reserved_namespace_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = temp_path("dangling_final_reserved_alias");
        let namespace = root.join("real").join(".rusty-cat");
        std::fs::create_dir_all(&namespace).expect("namespace fixture");
        let destination = namespace.join("future.rcdl");
        let alias = root.join("visible-link");
        symlink(&destination, &alias).expect("dangling target alias");

        let error = TargetLease::acquire(&alias)
            .err()
            .expect("dangling alias into reserved namespace must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_intermediate_symlink_into_reserved_namespace_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = temp_path("dangling_intermediate_reserved_alias");
        let namespace = root.join("real").join(".rusty-cat");
        std::fs::create_dir_all(&namespace).expect("namespace fixture");
        let missing_directory = namespace.join("future-directory");
        let alias = root.join("visible-directory-link");
        symlink(&missing_directory, &alias).expect("dangling directory alias");
        let destination = missing_directory.join("visible.bin");

        let error = TargetLease::acquire(&alias.join("visible.bin"))
            .err()
            .expect("dangling intermediate alias must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(root);
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

    #[test]
    fn generated_sidecar_path_is_never_a_valid_visible_target() {
        let target = temp_path("target_sidecar_collision");
        std::fs::write(&target, b"fixture").expect("fixture");
        let sidecar = crate::dflt::download_progress::sidecar_path(&target);
        std::fs::create_dir_all(sidecar.parent().expect("sidecar parent"))
            .expect("sidecar namespace");

        let owner = TargetLease::acquire(&target).expect("target owner");
        let error = match TargetLease::acquire(&sidecar) {
            Ok(_) => panic!("a generated sidecar path must remain reserved"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        drop(owner);

        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn nonexistent_relative_target_with_empty_parent_can_acquire_a_lease() {
        let target = std::path::PathBuf::from(format!(
            "rusty_cat_relative_target_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        assert_eq!(target.parent(), Some(std::path::Path::new("")));
        assert!(
            !target.exists(),
            "fixture must remain a not-yet-created path"
        );

        let owner = TargetLease::acquire(&target)
            .expect("an empty relative parent must resolve to the current directory");
        assert!(
            TargetLease::acquire(&target).is_err(),
            "the normalized relative path must still enforce one owner"
        );
        drop(owner);
        TargetLease::acquire(&target).expect("dropping the owner releases the relative lease");
    }

    #[test]
    fn locked_target_handle_can_write_and_hardlink_alias_contends() {
        let target = temp_path("locked_target");
        let hardlink = target.with_extension("hardlink");
        std::fs::write(&target, b"fixture").expect("fixture");
        std::fs::hard_link(&target, &hardlink).expect("hardlink");

        let mut owner = open_locked_target(&target, false).expect("lock target");
        owner.set_len(0).expect("truncate through locked handle");
        owner
            .write_all(b"written while locked")
            .expect("write through locked handle");
        owner.sync_all().expect("sync through locked handle");
        owner.rewind().expect("rewind locked handle");
        let mut bytes = Vec::new();
        owner
            .read_to_end(&mut bytes)
            .expect("read through locked handle");
        assert_eq!(bytes, b"written while locked");

        let error = open_locked_target(&hardlink, false)
            .expect_err("hardlink alias must contend on the actual file lock");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

        drop(owner);
        open_locked_target(&hardlink, false).expect("close releases target lock");
        let _ = std::fs::remove_file(hardlink);
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

    #[cfg(any(windows, target_os = "macos"))]
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
    fn partial_multi_key_acquisition_releases_every_prior_lock() {
        use fs2::FileExt as _;
        use sha2::{Digest as _, Sha256};

        fn lock_path(key: &[u8]) -> std::path::PathBuf {
            let directory = std::env::temp_dir().join("rusty-cat-target-leases-v1");
            std::fs::create_dir_all(&directory).expect("lease directory");
            let digest = Sha256::digest(key);
            let name = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            directory.join(format!("{name}.lock"))
        }

        let target = temp_path("partial_multi_key_release");
        std::fs::write(&target, b"fixture").expect("target fixture");
        let mut keys = super::lease_keys(&target).expect("target keys");
        keys.extend(
            super::lease_keys(&crate::dflt::download_progress::sidecar_path(&target))
                .expect("sidecar keys"),
        );
        keys.sort();
        keys.dedup();
        assert!(keys.len() >= 2, "fixture must exercise multiple lease keys");

        let contended_path = lock_path(keys.last().expect("last key"));
        let contended = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&contended_path)
            .expect("open contended lock");
        contended.try_lock_exclusive().expect("hold last lease key");

        let error = TargetLease::acquire(&target)
            .err()
            .expect("last-key contention must fail acquisition");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

        let first_path = lock_path(keys.first().expect("first key"));
        let first = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(first_path)
            .expect("open first lock");
        first
            .try_lock_exclusive()
            .expect("failed acquisition must release its earlier keys");

        let _ = fs2::FileExt::unlock(&first);
        let _ = fs2::FileExt::unlock(&contended);
        let _ = std::fs::remove_file(target);
    }

    #[cfg(windows)]
    #[test]
    fn windows_locked_target_denies_delete_and_rename_until_handle_release() {
        let target = temp_path("windows_delete_sharing");
        let renamed = target.with_extension("renamed");
        std::fs::write(&target, b"fixture").expect("target fixture");

        let owner = open_locked_target(&target, false).expect("lock visible target");
        assert!(
            std::fs::rename(&target, &renamed).is_err(),
            "active target handle must deny rename sharing"
        );
        assert!(
            std::fs::remove_file(&target).is_err(),
            "active target handle must deny delete sharing"
        );

        drop(owner);
        std::fs::rename(&target, &renamed)
            .expect("releasing the target handle must restore rename");
        std::fs::remove_file(renamed).expect("cleanup renamed target");
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

    #[test]
    fn cross_process_actual_target_lock_covers_hardlink_alias_and_process_exit() {
        if let Ok(mode) = std::env::var(CHILD_MODE) {
            let target = std::path::PathBuf::from(
                std::env::var_os(CHILD_TARGET).expect("child target path"),
            );
            match mode.as_str() {
                "actual-lock-contender" => {
                    let error = open_locked_target(&target, false)
                        .expect_err("hardlink alias must observe the parent's target lock");
                    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                }
                "actual-lock-owner" => {
                    let _owner = open_locked_target(&target, false).expect("child target owner");
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

        let target = temp_path("actual_lock_cross_process");
        let hardlink = target.with_extension("hardlink");
        let ready = target.with_extension("ready");
        std::fs::write(&target, b"fixture").expect("fixture");
        std::fs::hard_link(&target, &hardlink).expect("hardlink");

        let parent_owner = open_locked_target(&target, false).expect("parent target owner");
        let contender = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args(["--exact", ACTUAL_LOCK_TEST])
            .env(CHILD_MODE, "actual-lock-contender")
            .env(CHILD_TARGET, &hardlink)
            .status()
            .expect("run hardlink contender");
        assert!(
            contender.success(),
            "child must observe the parent's lock through a hardlink alias"
        );
        drop(parent_owner);
        drop(
            open_locked_target(&hardlink, false)
                .expect("closing parent owner must release the hardlink lock"),
        );

        let mut child_owner =
            std::process::Command::new(std::env::current_exe().expect("test exe"))
                .args(["--exact", ACTUAL_LOCK_TEST])
                .env(CHILD_MODE, "actual-lock-owner")
                .env(CHILD_TARGET, &target)
                .env(CHILD_READY, &ready)
                .spawn()
                .expect("spawn child target owner");
        for _ in 0..1_000 {
            if ready.exists() {
                break;
            }
            if let Some(status) = child_owner.try_wait().expect("poll child owner") {
                panic!("child target owner exited before publishing readiness: {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "child target owner did not acquire the lock"
        );

        let error = open_locked_target(&hardlink, false)
            .expect_err("hardlink alias must contend with the child target owner");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        child_owner.kill().expect("kill child target owner");
        child_owner.wait().expect("reap child target owner");

        drop(
            open_locked_target(&hardlink, false)
                .expect("process exit must immediately release the actual target lock"),
        );
        let _ = std::fs::remove_file(ready);
        let _ = std::fs::remove_file(hardlink);
        let _ = std::fs::remove_file(target);
    }
}
