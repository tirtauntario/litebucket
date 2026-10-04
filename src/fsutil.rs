//! Durable local filesystem primitives: the store lock, sharded paths,
//! exclusive creation, synchronization, and no-clobber publication.
//!
//! All functions are blocking; async callers run them on bounded blocking work.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags, RenameFlags};

use crate::error::{Error, Result};
use crate::failpoint;
use crate::ids::StorageId;

pub const LOCK_FILE: &str = "store.lock";
pub const DB_FILE: &str = "metadata.sqlite3";

/// Physical file areas. Every area uses two-level ID sharding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    Objects,
    Staging,
    Multipart,
}

impl Area {
    pub const ALL: [Area; 3] = [Area::Objects, Area::Staging, Area::Multipart];

    pub fn dir_name(self) -> &'static str {
        match self {
            Area::Objects => "objects",
            Area::Staging => "staging",
            Area::Multipart => "multipart",
        }
    }
}

/// File name for an ID within an area; staging files carry `.tmp`.
pub fn file_name(area: Area, id: &StorageId) -> String {
    match area {
        Area::Staging => format!("{}.tmp", id.to_hex()),
        _ => id.to_hex(),
    }
}

/// `area/id[0:2]/id[2:4]/filename(id)` relative to the data root.
pub fn relative_path(area: Area, id: &StorageId) -> PathBuf {
    let hex = id.to_hex();
    PathBuf::from(area.dir_name())
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(file_name(area, id))
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

pub fn open_dir_at(dir: impl AsFd, name: &str) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(dir, name, dir_flags(), Mode::empty())?)
}

pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(path, dir_flags(), Mode::empty())?)
}

/// Make a file's contents durable. macOS needs F_FULLFSYNC for a real barrier.
pub fn sync_fd(fd: impl AsFd) -> io::Result<()> {
    failpoint::io("sync")?;
    #[cfg(target_vendor = "apple")]
    {
        match rustix::fs::fcntl_fullfsync(&fd) {
            Ok(()) => return Ok(()),
            // Some filesystems do not support F_FULLFSYNC; fall back to fsync.
            Err(rustix::io::Errno::INVAL) | Err(rustix::io::Errno::NOTSUP) => {}
            Err(e) => return Err(e.into()),
        }
    }
    rustix::fs::fsync(fd)?;
    Ok(())
}

/// Make directory entries durable.
pub fn sync_dir(fd: impl AsFd) -> io::Result<()> {
    failpoint::io("sync_dir")?;
    sync_fd(fd)
}

/// Create a directory if missing. Returns true when newly created.
fn mkdir_at(dir: impl AsFd, name: &str, mode: u32) -> io::Result<bool> {
    match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(mode as _)) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::EXIST) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Owns the data root, its process lock, and the per-process set of shard
/// directories whose entries are known durable.
pub struct DataDir {
    root: PathBuf,
    root_fd: OwnedFd,
    area_fds: Vec<(Area, OwnedFd)>,
    durable_shards: Mutex<HashSet<(Area, u16)>>,
    _lock: StoreLock,
}

impl std::fmt::Debug for DataDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataDir").field("root", &self.root).finish()
    }
}

/// Exclusive lifetime lock on `store.lock`. The lock file is never unlinked.
pub struct StoreLock {
    _fd: OwnedFd,
}

impl StoreLock {
    pub fn acquire(root_fd: impl AsFd) -> Result<Self> {
        let fd = rustix::fs::openat(
            root_fd,
            LOCK_FILE,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(io::Error::from)?;
        match rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { _fd: fd }),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(Error::Locked),
            Err(e) => Err(io::Error::from(e).into()),
        }
    }
}

impl DataDir {
    /// Open an initialized data directory, acquire its lock, and validate it.
    pub fn open(root: &Path) -> Result<Self> {
        let root_fd = open_dir(root).map_err(|e| {
            Error::config(format!(
                "cannot open data directory {}: {e}",
                root.display()
            ))
        })?;
        validate_private_dir(&root_fd, "data directory")?;
        let lock = StoreLock::acquire(&root_fd)?;
        Self::open_locked(root, root_fd, lock)
    }

    fn open_locked(root: &Path, root_fd: OwnedFd, lock: StoreLock) -> Result<Self> {
        let root_dev = rustix::fs::fstat(&root_fd).map_err(io::Error::from)?.st_dev;
        let mut area_fds = Vec::new();
        for area in Area::ALL {
            let fd = open_dir_at(&root_fd, area.dir_name()).map_err(|e| {
                Error::config(format!(
                    "data directory is missing the {} area (run `storlite init`?): {e}",
                    area.dir_name()
                ))
            })?;
            let st = rustix::fs::fstat(&fd).map_err(io::Error::from)?;
            if st.st_dev != root_dev {
                return Err(Error::config(format!(
                    "{} is on a different filesystem; all areas must share one device",
                    area.dir_name()
                )));
            }
            validate_private_dir(&fd, area.dir_name())?;
            area_fds.push((area, fd));
        }
        Ok(Self {
            root: root.to_path_buf(),
            root_fd,
            area_fds,
            durable_shards: Mutex::new(HashSet::new()),
            _lock: lock,
        })
    }

    /// Initialize a new store layout in an empty (or absent) directory.
    /// Returns the opened, locked data directory.
    pub fn create(root: &Path) -> Result<Self> {
        match std::fs::symlink_metadata(root) {
            Ok(m) => {
                if !m.is_dir() {
                    return Err(Error::config(format!(
                        "{} exists and is not a directory",
                        root.display()
                    )));
                }
                if std::fs::read_dir(root)?.next().is_some() {
                    return Err(Error::config(format!(
                        "refusing to initialize non-empty directory {}",
                        root.display()
                    )));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let parent = root
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                std::fs::create_dir_all(parent)?;
                let name = root
                    .file_name()
                    .ok_or_else(|| Error::config("invalid data directory path"))?;
                let pfd = open_dir(parent)?;
                mkdir_at(&pfd, &name.to_string_lossy(), 0o700)?;
                sync_dir(&pfd)?;
            }
            Err(e) => return Err(e.into()),
        }
        let root_fd = open_dir(root)?;
        rustix::fs::fchmod(&root_fd, Mode::from_raw_mode(0o700)).map_err(io::Error::from)?;
        let lock = StoreLock::acquire(&root_fd)?;
        for area in Area::ALL {
            mkdir_at(&root_fd, area.dir_name(), 0o700)?;
        }
        sync_dir(&root_fd)?;
        Self::open_locked(root, root_fd, lock)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_fd(&self) -> &OwnedFd {
        &self.root_fd
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join(DB_FILE)
    }

    fn area_fd(&self, area: Area) -> &OwnedFd {
        &self
            .area_fds
            .iter()
            .find(|(a, _)| *a == area)
            .expect("all areas are opened")
            .1
    }

    /// Open (creating as needed) the leaf shard directory for an ID and make
    /// every newly created or not-yet-verified ancestor entry durable.
    pub fn shard_dir(&self, area: Area, id: &StorageId) -> io::Result<OwnedFd> {
        let hex = id.to_hex();
        let (a, b) = (&hex[0..2], &hex[2..4]);
        let key = (
            area,
            u16::from_be_bytes([id.as_bytes()[0], id.as_bytes()[1]]),
        );
        let known = self
            .durable_shards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key);
        let area_fd = self.area_fd(area);
        if known {
            let first = open_dir_at(area_fd, a)?;
            return open_dir_at(&first, b);
        }
        mkdir_at(area_fd, a, 0o700)?;
        let first = open_dir_at(area_fd, a)?;
        mkdir_at(&first, b, 0o700)?;
        let leaf = open_dir_at(&first, b)?;
        // Durability of the chain: entry `b` lives in `first`; entry `a` in the area dir.
        sync_dir(&first)?;
        sync_dir(area_fd)?;
        self.durable_shards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
        Ok(leaf)
    }

    /// Open the leaf shard directory if it exists (no creation).
    pub fn existing_shard_dir(&self, area: Area, id: &StorageId) -> io::Result<Option<OwnedFd>> {
        let hex = id.to_hex();
        let first = match open_dir_at(self.area_fd(area), &hex[0..2]) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        match open_dir_at(&first, &hex[2..4]) {
            Ok(fd) => Ok(Some(fd)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether any entry (file, link, or other) exists at the ID's path,
    /// without following symlinks.
    pub fn path_exists(&self, area: Area, id: &StorageId) -> io::Result<bool> {
        let Some(dir) = self.existing_shard_dir(area, id)? else {
            return Ok(false);
        };
        match rustix::fs::statat(
            &dir,
            file_name(area, id).as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Exclusively create a staging file for writing.
    pub fn create_staging(&self, id: &StorageId) -> io::Result<File> {
        failpoint::io("create_staging")?;
        let dir = self.shard_dir(Area::Staging, id)?;
        let fd = rustix::fs::openat(
            &dir,
            file_name(Area::Staging, id).as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        // No directory sync here: the WRITING row is already durable, and if
        // this entry is lost in a crash, recovery reclaims a row whose file is
        // simply absent. Durability is established at publication.
        Ok(File::from(fd))
    }

    /// Atomically move a synchronized staging file to its final path without
    /// ever replacing an existing file, then make both directory entries durable.
    pub fn publish(&self, area: Area, id: &StorageId) -> io::Result<()> {
        debug_assert!(area != Area::Staging);
        let src_dir = self
            .existing_shard_dir(Area::Staging, id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "staging shard missing"))?;
        let dst_dir = self.shard_dir(area, id)?;
        failpoint::hit("before_publish");
        rustix::fs::renameat_with(
            &src_dir,
            file_name(Area::Staging, id).as_str(),
            &dst_dir,
            file_name(area, id).as_str(),
            RenameFlags::NOREPLACE,
        )?;
        failpoint::hit("after_publish_before_dir_sync");
        sync_dir(&dst_dir)?;
        sync_dir(&src_dir)?;
        failpoint::hit("after_dir_sync");
        Ok(())
    }

    /// Open an immutable published file for reading without following symlinks.
    pub fn open_read(&self, area: Area, id: &StorageId) -> io::Result<File> {
        let dir = self
            .existing_shard_dir(area, id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "shard missing"))?;
        let fd = rustix::fs::openat(
            &dir,
            file_name(area, id).as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let file = File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a regular file",
            ));
        }
        Ok(file)
    }

    /// Remove a tracked file if present. Returns the shard dir fd for a later
    /// batched directory sync, or None if nothing was removed.
    pub fn remove(&self, area: Area, id: &StorageId) -> io::Result<Option<OwnedFd>> {
        let Some(dir) = self.existing_shard_dir(area, id)? else {
            return Ok(None);
        };
        match rustix::fs::unlinkat(&dir, file_name(area, id).as_str(), AtFlags::empty()) {
            Ok(()) => Ok(Some(dir)),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Filesystem availability for capacity admission.
    pub fn fs_stats(&self) -> io::Result<FsStats> {
        let st = rustix::fs::fstatvfs(&self.root_fd)?;
        let frsize = if st.f_frsize > 0 {
            st.f_frsize
        } else {
            st.f_bsize
        };
        Ok(FsStats {
            total_bytes: st.f_blocks.saturating_mul(frsize),
            avail_bytes: st.f_bavail.saturating_mul(frsize),
            total_inodes: st.f_files,
            avail_inodes: st.f_favail,
        })
    }

    /// Verify that the filesystem supports exclusive creation, no-clobber
    /// rename, and file/directory synchronization, using throwaway names in
    /// the staging root (never in shard directories). Stale probe files from
    /// an interrupted run are removed first.
    pub fn probe_capabilities(&self) -> Result<()> {
        let staging = self.area_fd(Area::Staging);
        let root = self.root.join(Area::Staging.dir_name());
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(".probe-") {
                    let _ = rustix::fs::unlinkat(staging, name.as_str(), AtFlags::empty());
                }
            }
        }
        let base = format!(".probe-{}", hex::encode(crate::ids::random_bytes::<8>()));
        let (a, b, c) = (
            base.clone(),
            format!("{base}-renamed"),
            format!("{base}-other"),
        );
        let unsupported = |what: &str, e: &dyn std::fmt::Display| {
            Error::config(format!(
                "data directory filesystem does not support {what}: {e}"
            ))
        };
        let create = |name: &str| {
            rustix::fs::openat(
                staging,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
        };
        let fa = create(&a).map_err(|e| unsupported("exclusive file creation", &e))?;
        if create(&a).is_ok() {
            return Err(Error::config("data directory filesystem ignores O_EXCL"));
        }
        sync_fd(&fa).map_err(|e| unsupported("file synchronization", &e))?;
        drop(fa);
        drop(create(&c).map_err(|e| unsupported("exclusive file creation", &e))?);
        rustix::fs::renameat_with(
            staging,
            a.as_str(),
            staging,
            b.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(|e| unsupported("no-clobber rename (renameat2 RENAME_NOREPLACE)", &e))?;
        match rustix::fs::renameat_with(
            staging,
            c.as_str(),
            staging,
            b.as_str(),
            RenameFlags::NOREPLACE,
        ) {
            Err(rustix::io::Errno::EXIST) => {}
            Ok(()) => {
                return Err(Error::config(
                    "data directory filesystem replaced a file despite RENAME_NOREPLACE",
                ));
            }
            Err(e) => return Err(unsupported("no-clobber rename", &e)),
        }
        sync_dir(staging).map_err(|e| unsupported("directory synchronization", &e))?;
        for n in [&b, &c] {
            let _ = rustix::fs::unlinkat(staging, n.as_str(), AtFlags::empty());
        }
        sync_dir(staging).map_err(|e| unsupported("directory synchronization", &e))?;
        Ok(())
    }

    /// Sync the root directory (after database file creation etc.).
    pub fn sync_root(&self) -> io::Result<()> {
        sync_dir(&self.root_fd)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FsStats {
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub total_inodes: u64,
    pub avail_inodes: u64,
}

/// The root must be a real directory owned by this user and not writable by others.
fn validate_private_dir(fd: &OwnedFd, what: &str) -> Result<()> {
    let st = rustix::fs::fstat(fd).map_err(io::Error::from)?;
    let uid = rustix::process::geteuid().as_raw();
    if st.st_uid != uid {
        return Err(Error::config(format!(
            "{what} is not owned by the service user"
        )));
    }
    if st.st_mode & 0o022 != 0 {
        return Err(Error::config(format!(
            "{what} is writable by group or others (mode {:o})",
            st.st_mode & 0o777
        )));
    }
    Ok(())
}

/// Size of an open file.
pub fn file_len(f: &File) -> io::Result<u64> {
    Ok(f.metadata()?.size())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn new_store() -> (tempfile::TempDir, DataDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dd = DataDir::create(&tmp.path().join("data")).unwrap();
        (tmp, dd)
    }

    #[test]
    fn paths_are_two_level_sharded() {
        let id = StorageId::parse_hex("a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap();
        assert_eq!(
            relative_path(Area::Objects, &id),
            PathBuf::from("objects/a1/b2/a1b2c3d4e5f60718293a4b5c6d7e8f90")
        );
        assert_eq!(
            relative_path(Area::Staging, &id),
            PathBuf::from("staging/a1/b2/a1b2c3d4e5f60718293a4b5c6d7e8f90.tmp")
        );
        assert_eq!(
            relative_path(Area::Multipart, &id),
            PathBuf::from("multipart/a1/b2/a1b2c3d4e5f60718293a4b5c6d7e8f90")
        );
    }

    #[test]
    fn second_owner_is_refused() {
        let (tmp, _dd) = new_store();
        let err = DataDir::open(&tmp.path().join("data")).unwrap_err();
        assert!(matches!(err, Error::Locked), "{err}");
    }

    #[test]
    fn init_refuses_non_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("x"), b"x").unwrap();
        assert!(DataDir::create(tmp.path()).is_err());
    }

    #[test]
    fn exclusive_create_and_noclobber_publish() {
        let (tmp, dd) = new_store();
        let id = StorageId::random();
        let mut f = dd.create_staging(&id).unwrap();
        assert!(dd.create_staging(&id).is_err(), "exclusive creation");
        f.write_all(b"hello").unwrap();
        sync_fd(&f).unwrap();
        drop(f);
        dd.publish(Area::Objects, &id).unwrap();
        let on_disk = tmp
            .path()
            .join("data")
            .join(relative_path(Area::Objects, &id));
        assert_eq!(std::fs::read(&on_disk).unwrap(), b"hello");
        assert!(!dd.path_exists(Area::Staging, &id).unwrap());

        // Injected collision: a second staging file with the same ID must not
        // replace the published file.
        let mut f = dd.create_staging(&id).unwrap();
        f.write_all(b"other").unwrap();
        drop(f);
        let err = dd.publish(Area::Objects, &id).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&on_disk).unwrap(), b"hello");
    }

    #[test]
    fn open_read_refuses_symlinks() {
        let (tmp, dd) = new_store();
        let id = StorageId::random();
        let dir = dd.shard_dir(Area::Objects, &id).unwrap();
        drop(dir);
        let target = tmp.path().join("secret");
        std::fs::write(&target, b"secret").unwrap();
        let link = tmp
            .path()
            .join("data")
            .join(relative_path(Area::Objects, &id));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(dd.open_read(Area::Objects, &id).is_err());
        assert!(dd.path_exists(Area::Objects, &id).unwrap());
    }

    #[test]
    fn capability_probe_passes_and_leaves_nothing() {
        let (tmp, dd) = new_store();
        dd.probe_capabilities().unwrap();
        std::fs::write(tmp.path().join("data/staging/.probe-stale"), b"x").unwrap();
        dd.probe_capabilities().unwrap();
        let left: Vec<_> = std::fs::read_dir(tmp.path().join("data/staging"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn remove_is_idempotent() {
        let (_tmp, dd) = new_store();
        let id = StorageId::random();
        drop(dd.create_staging(&id).unwrap());
        assert!(dd.remove(Area::Staging, &id).unwrap().is_some());
        assert!(dd.remove(Area::Staging, &id).unwrap().is_none());
        assert!(dd.remove(Area::Objects, &id).unwrap().is_none());
    }

    #[test]
    fn rejects_group_writable_root() {
        let (tmp, dd) = new_store();
        drop(dd);
        let root = tmp.path().join("data");
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o770))
            .unwrap();
        assert!(DataDir::open(&root).is_err());
    }
}
