//! `c0mpute storage mount DIR`: an encrypted volume as a FUSE filesystem
//! (Linux). Apps and backup tools read and write plain files in DIR; each
//! file is stored on the network as a private object, sealed on this
//! machine when it is closed, and the directory tree is one more private
//! object. Nodes hold only ciphertext and cannot tell the objects belong
//! together.
//!
//! Whole-file semantics: a file is fetched and decrypted into memory when
//! opened and sealed and published when closed (or fsynced). Good for
//! documents, photos and backup archives; not a database volume.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use c0mpute_core::sealed_store::{BlobStore, NetworkStore, Volume};
use c0mpute_sealed::StorageSecret;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use libc::{EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOTDIR, ENOTEMPTY};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;

struct Open {
    path: String,
    data: Zeroizing<Vec<u8>>,
    dirty: bool,
}

/// The FUSE side of a [`Volume`].
pub struct VolumeFs {
    rt: Handle,
    vol: Volume,
    inodes: HashMap<u64, String>,
    paths: HashMap<String, u64>,
    next_ino: u64,
    handles: HashMap<u64, Open>,
    next_fh: u64,
    uid: u32,
    gid: u32,
}

fn join(parent: &str, name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return None;
    }
    Some(if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") })
}

fn secs(t: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(t)
}

impl VolumeFs {
    pub fn new(rt: Handle, vol: Volume) -> Self {
        let mut fs = Self {
            rt,
            vol,
            inodes: HashMap::new(),
            paths: HashMap::new(),
            next_ino: ROOT + 1,
            handles: HashMap::new(),
            next_fh: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        fs.inodes.insert(ROOT, String::new());
        fs.paths.insert(String::new(), ROOT);
        fs
    }

    fn ino(&mut self, path: &str) -> u64 {
        if let Some(i) = self.paths.get(path) {
            return *i;
        }
        let i = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(i, path.to_string());
        self.paths.insert(path.to_string(), i);
        i
    }

    fn path(&self, ino: u64) -> Option<String> {
        self.inodes.get(&ino).cloned()
    }

    /// Size of a file, counting unsaved writes in open handles.
    fn pending(&self, path: &str) -> Option<&Open> {
        self.handles.values().find(|h| h.path == path)
    }

    fn attr(&mut self, path: &str) -> Option<FileAttr> {
        let ino = self.ino(path);
        let (kind, size, mtime, perm) = if let Some(h) = self.pending(path) {
            (FileType::RegularFile, h.data.len() as u64, SystemTime::now(), 0o600)
        } else if let Some(f) = self.vol.index.files.get(path) {
            (FileType::RegularFile, f.size, secs(f.mtime), 0o600)
        } else if self.vol.is_dir(path) {
            (FileType::Directory, 0, SystemTime::now(), 0o700)
        } else {
            return None;
        };
        Some(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    fn save(&mut self, fh: u64) -> Result<(), i32> {
        let Some(h) = self.handles.get_mut(&fh) else { return Ok(()) };
        if !h.dirty {
            return Ok(());
        }
        let path = h.path.clone();
        let data = h.data.clone();
        let res = self.rt.block_on(self.vol.write(&path, &data));
        match res {
            Ok(()) => {
                if let Some(h) = self.handles.get_mut(&fh) {
                    h.dirty = false;
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("c0mpute mount: could not store {path}: {e:#}");
                Err(EIO)
            }
        }
    }

    fn open_handle(&mut self, path: String, data: Zeroizing<Vec<u8>>, dirty: bool) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, Open { path, data, dirty });
        fh
    }
}

impl Filesystem for VolumeFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(p) = self.path(parent) else { return reply.error(ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(ENOENT) };
        match self.attr(&path) {
            Some(a) => reply.entry(&TTL, &a, 0),
            None => reply.error(ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(p) = self.path(ino) else { return reply.error(ENOENT) };
        match self.attr(&p) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(p) = self.path(ino) else { return reply.error(ENOENT) };
        if let Some(size) = size {
            let handle = fh.filter(|f| self.handles.contains_key(f)).or_else(|| self.handles.iter().find(|(_, h)| h.path == p).map(|(k, _)| *k));
            match handle {
                Some(f) => {
                    let h = self.handles.get_mut(&f).expect("present");
                    h.data.resize(size as usize, 0);
                    h.dirty = true;
                }
                None => {
                    // Truncate a closed file: load, resize, store.
                    let mut data = if self.vol.index.files.contains_key(&p) {
                        match self.rt.block_on(self.vol.read(&p)) {
                            Ok(d) => d,
                            Err(_) => return reply.error(EIO),
                        }
                    } else {
                        Zeroizing::new(Vec::new())
                    };
                    data.resize(size as usize, 0);
                    if self.rt.block_on(self.vol.write(&p, &data)).is_err() {
                        return reply.error(EIO);
                    }
                }
            }
        }
        match self.attr(&p) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(ENOENT),
        }
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(p) = self.path(parent) else { return reply.error(ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(EINVAL) };
        if self.attr(&path).is_some() {
            return reply.error(EEXIST);
        }
        if self.rt.block_on(self.vol.mkdir(&path)).is_err() {
            return reply.error(EIO);
        }
        match self.attr(&path) {
            Some(a) => reply.entry(&TTL, &a, 0),
            None => reply.error(EIO),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(p) = self.path(parent) else { return reply.error(ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(ENOENT) };
        if !self.vol.index.files.contains_key(&path) {
            // Created but never stored (still open): just forget it.
            let pending: Vec<u64> = self.handles.iter().filter(|(_, h)| h.path == path).map(|(k, _)| *k).collect();
            if pending.is_empty() {
                return reply.error(ENOENT);
            }
            for fh in pending {
                if let Some(h) = self.handles.get_mut(&fh) {
                    h.dirty = false;
                    h.path = String::new();
                }
            }
            return reply.ok();
        }
        match self.rt.block_on(self.vol.remove(&path)) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(EIO),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(p) = self.path(parent) else { return reply.error(ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(ENOENT) };
        if !self.vol.is_dir(&path) {
            return reply.error(ENOTDIR);
        }
        if self.vol.children(&path).next().is_some() {
            return reply.error(ENOTEMPTY);
        }
        match self.rt.block_on(self.vol.rmdir(&path)) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(EIO),
        }
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
        let (Some(p), Some(np)) = (self.path(parent), self.path(newparent)) else { return reply.error(ENOENT) };
        let (Some(from), Some(to)) = (join(&p, name), join(&np, newname)) else { return reply.error(EINVAL) };
        // Flush unsaved writes first so the data moves with the name.
        let open: Vec<u64> = self.handles.iter().filter(|(_, h)| h.path == from).map(|(k, _)| *k).collect();
        for fh in &open {
            if self.save(*fh).is_err() {
                return reply.error(EIO);
            }
        }
        match self.rt.block_on(self.vol.rename(&from, &to)) {
            Ok(()) => {
                for fh in open {
                    if let Some(h) = self.handles.get_mut(&fh) {
                        h.path = to.clone();
                    }
                }
                if let Some(ino) = self.paths.remove(&from) {
                    self.inodes.insert(ino, to.clone());
                    self.paths.insert(to, ino);
                }
                reply.ok()
            }
            Err(_) => reply.error(EIO),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(p) = self.path(ino) else { return reply.error(ENOENT) };
        if self.vol.is_dir(&p) && !self.vol.index.files.contains_key(&p) {
            return reply.error(EISDIR);
        }
        let truncate = flags & libc::O_TRUNC != 0;
        let data = if truncate || !self.vol.index.files.contains_key(&p) {
            self.pending(&p).map(|h| h.data.clone()).unwrap_or_default()
        } else {
            match self.rt.block_on(self.vol.read(&p)) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("c0mpute mount: could not fetch {p}: {e:#}");
                    return reply.error(EIO);
                }
            }
        };
        let fh = self.open_handle(p, data, truncate);
        reply.opened(fh, 0);
    }

    fn create(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some(p) = self.path(parent) else { return reply.error(ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(EINVAL) };
        let fh = self.open_handle(path.clone(), Zeroizing::new(Vec::new()), true);
        match self.attr(&path) {
            Some(a) => reply.created(&TTL, &a, 0, fh, 0),
            None => reply.error(EIO),
        }
    }

    fn read(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        let Some(h) = self.handles.get(&fh) else { return reply.error(EIO) };
        let start = (offset.max(0) as usize).min(h.data.len());
        let end = (start + size as usize).min(h.data.len());
        reply.data(&h.data[start..end]);
    }

    fn write(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, data: &[u8], _wf: u32, _flags: i32, _lock: Option<u64>, reply: ReplyWrite) {
        let Some(h) = self.handles.get_mut(&fh) else { return reply.error(EIO) };
        let start = offset.max(0) as usize;
        if h.data.len() < start + data.len() {
            h.data.resize(start + data.len(), 0);
        }
        h.data[start..start + data.len()].copy_from_slice(data);
        h.dirty = true;
        reply.written(data.len() as u32);
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _lock: u64, reply: ReplyEmpty) {
        match self.save(fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        match self.save(fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, _lock: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        let res = self.save(fh);
        self.handles.remove(&fh);
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.path(ino) {
            Some(p) if self.vol.is_dir(&p) => reply.opened(0, 0),
            Some(_) => reply.error(ENOTDIR),
            None => reply.error(ENOENT),
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(p) = self.path(ino) else { return reply.error(ENOENT) };
        let mut entries: Vec<(u64, FileType, String)> = vec![(ino, FileType::Directory, ".".into()), (ROOT, FileType::Directory, "..".into())];
        let mut kids: Vec<(String, bool)> = self.vol.children(&p).collect();
        // Files created but not yet stored show up too.
        let prefix = if p.is_empty() { String::new() } else { format!("{p}/") };
        for h in self.handles.values() {
            if let Some(rest) = h.path.strip_prefix(&prefix)
                && !rest.is_empty()
                && !rest.contains('/')
                && !kids.iter().any(|(n, _)| n == rest)
            {
                kids.push((rest.to_string(), false));
            }
        }
        for (name, is_dir) in kids {
            let child = if p.is_empty() { name.clone() } else { format!("{p}/{name}") };
            let ino = self.ino(&child);
            entries.push((ino, if is_dir { FileType::Directory } else { FileType::RegularFile }, name));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

fn options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("c0mpute".into()),
        MountOption::Subtype("c0mpute-private".into()),
        MountOption::NoAtime,
        MountOption::DefaultPermissions,
    ]
}

/// Mount `vol` at `dir` until the returned session is dropped.
pub fn mount_volume(rt: Handle, vol: Volume, dir: &Path) -> Result<fuser::BackgroundSession> {
    fuser::spawn_mount2(VolumeFs::new(rt, vol), dir, &options())
        .with_context(|| format!("mount at {} (is FUSE installed? `fusermount3` must be on PATH)", dir.display()))
}

/// `c0mpute storage mount DIR`.
pub async fn run(dir: &Path, label: &str, gateway: &str) -> Result<()> {
    let secret = Arc::new(crate::private_storage::load_identity(true, None)?);
    let net = Arc::new(NetworkStore::start(gateway).await?);
    let store: Arc<dyn BlobStore> = net.clone();
    let vol = Volume::open(store, secret.clone(), label).await?;
    eprintln!(
        "volume {label}: {} file(s); mounting at {} (Ctrl-C to unmount)",
        vol.index.files.len(),
        dir.display()
    );
    eprintln!("  files are encrypted on this machine when closed; the network holds only ciphertext");
    let session = mount_volume(Handle::current(), vol, dir)?;
    tokio::signal::ctrl_c().await?;
    drop(session);
    eprintln!("unmounted; making sure the network has copies of what you wrote...");
    let counts = net
        .wait_for_copies(Duration::from_secs(300), |_, _| {})
        .await;
    let short = counts.values().filter(|n| **n < c0mpute_core::hosting::TARGET_REPLICAS).count();
    if short > 0 {
        eprintln!("note: {short} object(s) have fewer than 3 copies yet; the network keeps replicating");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use c0mpute_core::sealed_store::DirStore;

    fn fuse_available() -> bool {
        std::path::Path::new("/dev/fuse").exists()
            && ["fusermount3", "fusermount"].iter().any(|b| {
                std::env::var_os("PATH")
                    .map(|p| std::env::split_paths(&p).any(|d| d.join(b).is_file()))
                    .unwrap_or(false)
            })
    }

    /// Write and read files through a real FUSE mount, unmount, mount again
    /// from the store alone, and check the store holds only ciphertext.
    #[test]
    fn fuse_write_read_round_trip() {
        if !fuse_available() {
            if std::env::var_os("C0MPUTE_REQUIRE_FUSE").is_some() {
                panic!("C0MPUTE_REQUIRE_FUSE is set but /dev/fuse or fusermount is missing");
            }
            eprintln!("skipping: no /dev/fuse or fusermount on this machine");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let blobs = tmp.path().join("blobs");
        let mnt = tmp.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        let secret = Arc::new(StorageSecret::generate());
        let store: Arc<dyn BlobStore> = Arc::new(DirStore(blobs.clone()));

        let vol = rt.block_on(Volume::open(store.clone(), secret.clone(), "test")).unwrap();
        let session = mount_volume(rt.handle().clone(), vol, &mnt).unwrap();
        std::fs::create_dir(mnt.join("taxes")).unwrap();
        std::fs::write(mnt.join("taxes/return-2026.txt"), b"SECRET-INCOME-4242").unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
        std::fs::write(mnt.join("archive.bin"), &big).unwrap();
        assert_eq!(std::fs::read(mnt.join("taxes/return-2026.txt")).unwrap(), b"SECRET-INCOME-4242");
        std::fs::rename(mnt.join("archive.bin"), mnt.join("taxes/archive.bin")).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(mnt.join("taxes")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["archive.bin", "return-2026.txt"]);
        drop(session);

        // Fresh mount, state comes only from the (ciphertext) store.
        let vol = rt.block_on(Volume::open(store.clone(), secret.clone(), "test")).unwrap();
        let session = mount_volume(rt.handle().clone(), vol, &mnt).unwrap();
        assert_eq!(std::fs::read(mnt.join("taxes/return-2026.txt")).unwrap(), b"SECRET-INCOME-4242");
        assert_eq!(std::fs::read(mnt.join("taxes/archive.bin")).unwrap(), big);
        std::fs::remove_file(mnt.join("taxes/archive.bin")).unwrap();
        assert!(!mnt.join("taxes/archive.bin").exists());
        drop(session);

        for e in std::fs::read_dir(&blobs).unwrap() {
            let b = std::fs::read(e.unwrap().path()).unwrap();
            for needle in [&b"SECRET-INCOME"[..], b"return-2026", b"taxes"] {
                assert!(!b.windows(needle.len()).any(|w| w == needle), "plaintext in the store");
            }
        }
    }
}
