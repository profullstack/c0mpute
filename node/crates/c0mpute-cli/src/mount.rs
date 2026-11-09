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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use c0mpute_core::sealed_store::{BlobStore, NetworkStore, Volume};
use c0mpute_sealed::StorageSecret;
use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;

struct Open {
    path: String,
    data: Zeroizing<Vec<u8>>,
    dirty: bool,
}

/// State behind the FUSE callbacks (fuser 0.18 calls `&self`).
struct Inner {
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

impl Inner {
    fn new(rt: Handle, vol: Volume) -> Self {
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
            ino: INodeNo(ino),
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

    fn save(&mut self, fh: u64) -> Result<(), Errno> {
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
                Err(Errno::EIO)
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

/// The FUSE side of a [`Volume`].
pub struct VolumeFs(Mutex<Inner>);

impl VolumeFs {
    pub fn new(rt: Handle, vol: Volume) -> Self {
        Self(Mutex::new(Inner::new(rt, vol)))
    }
}

impl Filesystem for VolumeFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let parent = parent.0;
        let Some(p) = s.path(parent) else { return reply.error(Errno::ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(Errno::ENOENT) };
        match s.attr(&path) {
            Some(a) => reply.entry(&TTL, &a, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let ino = ino.0;
        let Some(p) = s.path(ino) else { return reply.error(Errno::ENOENT) };
        match s.attr(&p) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let ino = ino.0;
        let fh = fh.map(|f| f.0);
        let Some(p) = s.path(ino) else { return reply.error(Errno::ENOENT) };
        if let Some(size) = size {
            let handle = fh.filter(|f| s.handles.contains_key(f)).or_else(|| s.handles.iter().find(|(_, h)| h.path == p).map(|(k, _)| *k));
            match handle {
                Some(f) => {
                    let h = s.handles.get_mut(&f).expect("present");
                    h.data.resize(size as usize, 0);
                    h.dirty = true;
                }
                None => {
                    // Truncate a closed file: load, resize, store.
                    let mut data = if s.vol.index.files.contains_key(&p) {
                        match s.rt.block_on(s.vol.read(&p)) {
                            Ok(d) => d,
                            Err(_) => return reply.error(Errno::EIO),
                        }
                    } else {
                        Zeroizing::new(Vec::new())
                    };
                    data.resize(size as usize, 0);
                    if s.rt.block_on(s.vol.write(&p, &data)).is_err() {
                        return reply.error(Errno::EIO);
                    }
                }
            }
        }
        match s.attr(&p) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let parent = parent.0;
        let Some(p) = s.path(parent) else { return reply.error(Errno::ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(Errno::EINVAL) };
        if s.attr(&path).is_some() {
            return reply.error(Errno::EEXIST);
        }
        if s.rt.block_on(s.vol.mkdir(&path)).is_err() {
            return reply.error(Errno::EIO);
        }
        match s.attr(&path) {
            Some(a) => reply.entry(&TTL, &a, Generation(0)),
            None => reply.error(Errno::EIO),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let parent = parent.0;
        let Some(p) = s.path(parent) else { return reply.error(Errno::ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(Errno::ENOENT) };
        if !s.vol.index.files.contains_key(&path) {
            // Created but never stored (still open): just forget it.
            let pending: Vec<u64> = s.handles.iter().filter(|(_, h)| h.path == path).map(|(k, _)| *k).collect();
            if pending.is_empty() {
                return reply.error(Errno::ENOENT);
            }
            for fh in pending {
                if let Some(h) = s.handles.get_mut(&fh) {
                    h.dirty = false;
                    h.path = String::new();
                }
            }
            return reply.ok();
        }
        match s.rt.block_on(s.vol.remove(&path)) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let parent = parent.0;
        let Some(p) = s.path(parent) else { return reply.error(Errno::ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(Errno::ENOENT) };
        if !s.vol.is_dir(&path) {
            return reply.error(Errno::ENOTDIR);
        }
        if s.vol.children(&path).next().is_some() {
            return reply.error(Errno::ENOTEMPTY);
        }
        match s.rt.block_on(s.vol.rmdir(&path)) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, _flags: RenameFlags, reply: ReplyEmpty) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let (parent, newparent) = (parent.0, newparent.0);
        let (Some(p), Some(np)) = (s.path(parent), s.path(newparent)) else { return reply.error(Errno::ENOENT) };
        let (Some(from), Some(to)) = (join(&p, name), join(&np, newname)) else { return reply.error(Errno::EINVAL) };
        // Flush unsaved writes first so the data moves with the name.
        let open: Vec<u64> = s.handles.iter().filter(|(_, h)| h.path == from).map(|(k, _)| *k).collect();
        for fh in &open {
            if s.save(*fh).is_err() {
                return reply.error(Errno::EIO);
            }
        }
        match s.rt.block_on(s.vol.rename(&from, &to)) {
            Ok(()) => {
                for fh in open {
                    if let Some(h) = s.handles.get_mut(&fh) {
                        h.path = to.clone();
                    }
                }
                if let Some(ino) = s.paths.remove(&from) {
                    s.inodes.insert(ino, to.clone());
                    s.paths.insert(to, ino);
                }
                reply.ok()
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let ino = ino.0;
        let flags = flags.0;
        let Some(p) = s.path(ino) else { return reply.error(Errno::ENOENT) };
        if s.vol.is_dir(&p) && !s.vol.index.files.contains_key(&p) {
            return reply.error(Errno::EISDIR);
        }
        let truncate = flags & libc::O_TRUNC != 0;
        let data = if truncate || !s.vol.index.files.contains_key(&p) {
            s.pending(&p).map(|h| h.data.clone()).unwrap_or_default()
        } else {
            match s.rt.block_on(s.vol.read(&p)) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("c0mpute mount: could not fetch {p}: {e:#}");
                    return reply.error(Errno::EIO);
                }
            }
        };
        let fh = s.open_handle(p, data, truncate);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let parent = parent.0;
        let Some(p) = s.path(parent) else { return reply.error(Errno::ENOENT) };
        let Some(path) = join(&p, name) else { return reply.error(Errno::EINVAL) };
        let fh = s.open_handle(path.clone(), Zeroizing::new(Vec::new()), true);
        match s.attr(&path) {
            Some(a) => reply.created(&TTL, &a, Generation(0), FileHandle(fh), FopenFlags::empty()),
            None => reply.error(Errno::EIO),
        }
    }

    fn read(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock: Option<LockOwner>, reply: ReplyData) {
        let g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &*g;
        let fh = fh.0;
        let Some(h) = s.handles.get(&fh) else { return reply.error(Errno::EIO) };
        let start = (offset as usize).min(h.data.len());
        let end = (start + size as usize).min(h.data.len());
        reply.data(&h.data[start..end]);
    }

    fn write(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, data: &[u8], _wf: WriteFlags, _flags: OpenFlags, _lock: Option<LockOwner>, reply: ReplyWrite) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let fh = fh.0;
        let Some(h) = s.handles.get_mut(&fh) else { return reply.error(Errno::EIO) };
        let start = offset as usize;
        if h.data.len() < start + data.len() {
            h.data.resize(start + data.len(), 0);
        }
        h.data[start..start + data.len()].copy_from_slice(data);
        h.dirty = true;
        reply.written(data.len() as u32);
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let fh = fh.0;
        match s.save(fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let fh = fh.0;
        match s.save(fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let fh = fh.0;
        let res = s.save(fh);
        s.handles.remove(&fh);
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &*g;
        let ino = ino.0;
        match s.path(ino) {
            Some(p) if s.vol.is_dir(&p) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Some(_) => reply.error(Errno::ENOTDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let s = &mut *g;
        let ino = ino.0;
        let Some(p) = s.path(ino) else { return reply.error(Errno::ENOENT) };
        let mut entries: Vec<(u64, FileType, String)> = vec![(ino, FileType::Directory, ".".into()), (ROOT, FileType::Directory, "..".into())];
        let mut kids: Vec<(String, bool)> = s.vol.children(&p).collect();
        // Files created but not yet stored show up too.
        let prefix = if p.is_empty() { String::new() } else { format!("{p}/") };
        for h in s.handles.values() {
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
            let ino = s.ino(&child);
            entries.push((ino, if is_dir { FileType::Directory } else { FileType::RegularFile }, name));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
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
    let mut config = Config::default();
    config.mount_options = options();
    fuser::spawn_mount(VolumeFs::new(rt, vol), dir, &config)
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
    if let Err(e) = session.umount_and_join() {
        eprintln!("unmount: {e}");
    }
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
        session.umount_and_join().unwrap();

        // Fresh mount, state comes only from the (ciphertext) store.
        let vol = rt.block_on(Volume::open(store.clone(), secret.clone(), "test")).unwrap();
        let session = mount_volume(rt.handle().clone(), vol, &mnt).unwrap();
        assert_eq!(std::fs::read(mnt.join("taxes/return-2026.txt")).unwrap(), b"SECRET-INCOME-4242");
        assert_eq!(std::fs::read(mnt.join("taxes/archive.bin")).unwrap(), big);
        std::fs::remove_file(mnt.join("taxes/archive.bin")).unwrap();
        assert!(!mnt.join("taxes/archive.bin").exists());
        session.umount_and_join().unwrap();

        for e in std::fs::read_dir(&blobs).unwrap() {
            let b = std::fs::read(e.unwrap().path()).unwrap();
            for needle in [&b"SECRET-INCOME"[..], b"return-2026", b"taxes"] {
                assert!(!b.windows(needle.len()).any(|w| w == needle), "plaintext in the store");
            }
        }
    }
}
