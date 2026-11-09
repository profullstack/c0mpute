//! Chunks a publisher serves straight from its source files.
//!
//! `c0mpute hosting publish` scans a folder in place ([`super::site::scan_dir`])
//! and registers where every chunk is. The node's chunk server
//! (`supervisor::StoreSource`) falls back to this index when a chunk is not
//! in the chunk store, so the publisher seeds a 50 GiB folder without copying
//! it. Every read is checked against the chunk's hash: a file edited while it
//! is being published answers "not found", never wrong bytes.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{OnceLock, RwLock};

use c0mpute_proto::Hash;

use super::site::ChunkLoc;

fn index() -> &'static RwLock<HashMap<Hash, ChunkLoc>> {
    static INDEX: OnceLock<RwLock<HashMap<Hash, ChunkLoc>>> = OnceLock::new();
    INDEX.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Serve these chunks from their files from now on.
pub fn register(locations: &HashMap<Hash, ChunkLoc>) {
    if let Ok(mut idx) = index().write() {
        for (h, loc) in locations {
            idx.entry(*h).or_insert_with(|| loc.clone());
        }
    }
}

/// True when this process seeds `hash` from a file.
pub fn has(hash: &Hash) -> bool {
    index()
        .read()
        .map(|i| i.contains_key(hash))
        .unwrap_or(false)
}

/// Read a seeded chunk, checked against its hash.
pub fn read(hash: &Hash) -> Option<Vec<u8>> {
    let loc = index().read().ok()?.get(hash).cloned()?;
    let mut f = std::fs::File::open(&loc.path).ok()?;
    f.seek(SeekFrom::Start(loc.offset)).ok()?;
    let mut buf = vec![0u8; loc.len as usize];
    f.read_exact(&mut buf).ok()?;
    (Hash::of(&buf) == *hash).then_some(buf)
}

/// [`read`] off the async runtime.
pub async fn read_async(hash: &Hash) -> Option<Vec<u8>> {
    if !has(hash) {
        return None;
    }
    let h = *hash;
    tokio::task::spawn_blocking(move || read(&h))
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_registered_chunks_and_refuses_edited_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let h = Hash::of(&data[1000..2000]);
        let mut m = HashMap::new();
        m.insert(
            h,
            ChunkLoc {
                path: p.clone(),
                offset: 1000,
                len: 1000,
            },
        );
        register(&m);
        assert!(has(&h));
        assert_eq!(read(&h).unwrap(), data[1000..2000].to_vec());
        std::fs::write(&p, vec![0u8; 3000]).unwrap();
        assert!(read(&h).is_none(), "changed bytes are not served");
        assert!(read(&Hash::of(b"other")).is_none());
    }
}
