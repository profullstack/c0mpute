//! Read git objects straight out of packfiles, with no `git` binary: what a
//! gateway needs to show a repo in a browser (`/git/<owner>/<repo>`).
//!
//! Supports pack index v2 and pack v2/v3 with every object kind git writes:
//! commits, trees, blobs, tags, and both delta encodings (`OFS_DELTA` and
//! `REF_DELTA`, resolved across every pack of the repo). SHA-1 only, like
//! the rest of git hosting.

use std::collections::HashMap;
use std::io::Read;

use anyhow::{Context, Result, bail};

/// Largest single object we inflate (a guard against zip bombs in a pack).
pub const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
/// Longest delta chain we follow (git's own default depth is 50).
const MAX_DELTA_DEPTH: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }
}

/// One pack and its index, both whole in memory.
struct Pack {
    idx: Vec<u8>,
    pack: Vec<u8>,
    count: usize,
}

impl Pack {
    fn new(pack: Vec<u8>, idx: Vec<u8>) -> Result<Self> {
        if idx.len() < 8 + 256 * 4 || &idx[..4] != b"\xfftOc" || be32(&idx, 4)? != 2 {
            bail!("not a version 2 pack index");
        }
        if pack.len() < 12 || &pack[..4] != b"PACK" {
            bail!("not a packfile");
        }
        let count = be32(&idx, 8 + 255 * 4)? as usize;
        let need = 8 + 256 * 4 + count * (20 + 4 + 4);
        if idx.len() < need {
            bail!("pack index is truncated");
        }
        Ok(Self { idx, pack, count })
    }

    fn sha_at(&self, i: usize) -> &[u8] {
        let base = 8 + 256 * 4 + i * 20;
        &self.idx[base..base + 20]
    }

    /// Pack offset of `oid`, by binary search inside its fan-out bucket.
    fn find(&self, oid: &[u8; 20]) -> Option<u64> {
        let b = oid[0] as usize;
        let lo = if b == 0 {
            0
        } else {
            be32(&self.idx, 8 + (b - 1) * 4).ok()? as usize
        };
        let hi = be32(&self.idx, 8 + b * 4).ok()? as usize;
        let (mut lo, mut hi) = (lo, hi.min(self.count));
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.sha_at(mid).cmp(&oid[..]) {
                std::cmp::Ordering::Equal => return self.offset_at(mid),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    fn offset_at(&self, i: usize) -> Option<u64> {
        let offs = 8 + 256 * 4 + self.count * 24;
        let o = be32(&self.idx, offs + i * 4).ok()?;
        if o & 0x8000_0000 == 0 {
            return Some(o as u64);
        }
        let large = offs + self.count * 4 + (o & 0x7fff_ffff) as usize * 8;
        let b = self.idx.get(large..large + 8)?;
        Some(u64::from_be_bytes(b.try_into().ok()?))
    }
}

/// What is stored at one pack offset, before delta resolution.
enum Entry {
    Full(Kind, Vec<u8>),
    OfsDelta(u64, Vec<u8>),
    RefDelta([u8; 20], Vec<u8>),
}

/// Every pack of one repo version.
pub struct PackSet {
    packs: Vec<Pack>,
}

impl PackSet {
    /// `(pack bytes, idx bytes)` per pack, any order.
    pub fn new(packs: Vec<(Vec<u8>, Vec<u8>)>) -> Result<Self> {
        let packs = packs
            .into_iter()
            .map(|(p, i)| Pack::new(p, i))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { packs })
    }

    /// Bytes held in memory (for cache accounting).
    pub fn bytes(&self) -> usize {
        self.packs.iter().map(|p| p.pack.len() + p.idx.len()).sum()
    }

    fn locate(&self, oid: &[u8; 20]) -> Option<(usize, u64)> {
        self.packs
            .iter()
            .enumerate()
            .find_map(|(i, p)| p.find(oid).map(|o| (i, o)))
    }

    pub fn has(&self, oid: &str) -> bool {
        parse_oid(oid).is_ok_and(|o| self.locate(&o).is_some())
    }

    /// The object `oid` (hex): its kind and full content.
    pub fn object(&self, oid: &str) -> Result<(Kind, Vec<u8>)> {
        let o = parse_oid(oid)?;
        let (pi, off) = self
            .locate(&o)
            .with_context(|| format!("object {oid} is not in this repo"))?;
        self.read(pi, off)
    }

    /// Resolve the object at `(pack, offset)`, following its delta chain.
    fn read(&self, pi: usize, off: u64) -> Result<(Kind, Vec<u8>)> {
        let mut deltas: Vec<Vec<u8>> = Vec::new();
        let (mut pi, mut off) = (pi, off);
        let (kind, mut data) = loop {
            if deltas.len() > MAX_DELTA_DEPTH {
                bail!("delta chain too deep");
            }
            match entry_at(&self.packs[pi].pack, off)? {
                Entry::Full(k, d) => break (k, d),
                Entry::OfsDelta(base, d) => {
                    deltas.push(d);
                    off = base;
                }
                Entry::RefDelta(base, d) => {
                    deltas.push(d);
                    let (bp, bo) = self
                        .locate(&base)
                        .context("delta base is missing from the repo")?;
                    pi = bp;
                    off = bo;
                }
            }
        };
        for d in deltas.iter().rev() {
            data = apply_delta(&data, d)?;
        }
        Ok((kind, data))
    }

    /// Follow tags to the commit (or tree, blob) they point at.
    pub fn peel(&self, oid: &str) -> Result<(String, Kind, Vec<u8>)> {
        let mut oid = oid.to_string();
        for _ in 0..16 {
            let (k, d) = self.object(&oid)?;
            if k != Kind::Tag {
                return Ok((oid, k, d));
            }
            oid = Tag::parse(&d)?.object;
        }
        bail!("tag chain too long")
    }
}

/// The entry header at `off`: type, size, delta base, then zlib data.
fn entry_at(pack: &[u8], off: u64) -> Result<Entry> {
    let mut p = usize::try_from(off).context("offset")?;
    let mut c = *pack.get(p).context("offset past the end of the pack")?;
    p += 1;
    let ty = (c >> 4) & 7;
    let mut size = (c & 0x0f) as u64;
    let mut shift = 4;
    while c & 0x80 != 0 {
        c = *pack.get(p).context("truncated object header")?;
        p += 1;
        if shift > 56 {
            bail!("object size overflows");
        }
        size |= ((c & 0x7f) as u64) << shift;
        shift += 7;
    }
    let size = usize::try_from(size).context("size")?;
    if size > MAX_OBJECT_BYTES {
        bail!("object of {size} bytes is too large to show");
    }
    match ty {
        1..=4 => {
            let kind = [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag][ty as usize - 1];
            Ok(Entry::Full(kind, inflate(&pack[p..], size)?))
        }
        6 => {
            let mut c = *pack.get(p).context("truncated delta offset")?;
            p += 1;
            let mut rel = (c & 0x7f) as u64;
            while c & 0x80 != 0 {
                c = *pack.get(p).context("truncated delta offset")?;
                p += 1;
                rel = ((rel + 1) << 7) | (c & 0x7f) as u64;
            }
            let base = off
                .checked_sub(rel)
                .context("delta base before the pack start")?;
            if base >= off {
                bail!("delta base does not precede its delta");
            }
            Ok(Entry::OfsDelta(base, inflate(&pack[p..], size)?))
        }
        7 => {
            let b: [u8; 20] = pack
                .get(p..p + 20)
                .context("truncated delta base")?
                .try_into()?;
            Ok(Entry::RefDelta(b, inflate(&pack[p + 20..], size)?))
        }
        _ => bail!("unknown pack object type {ty}"),
    }
}

fn inflate(data: &[u8], size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(size);
    flate2::read::ZlibDecoder::new(data)
        .take(size as u64 + 1)
        .read_to_end(&mut out)
        .context("inflate pack object")?;
    if out.len() != size {
        bail!(
            "pack object inflates to {} bytes, header says {size}",
            out.len()
        );
    }
    Ok(out)
}

fn delta_varint(d: &[u8], p: &mut usize) -> Result<usize> {
    let (mut v, mut shift) = (0usize, 0);
    loop {
        let c = *d.get(*p).context("truncated delta")?;
        *p += 1;
        if shift > 56 {
            bail!("delta size overflows");
        }
        v |= ((c & 0x7f) as usize) << shift;
        shift += 7;
        if c & 0x80 == 0 {
            return Ok(v);
        }
    }
}

/// git's delta format: source size, target size, then copy/insert ops.
fn apply_delta(base: &[u8], d: &[u8]) -> Result<Vec<u8>> {
    let mut p = 0;
    let src = delta_varint(d, &mut p)?;
    let dst = delta_varint(d, &mut p)?;
    if src != base.len() {
        bail!("delta expects a {src}-byte base, got {}", base.len());
    }
    if dst > MAX_OBJECT_BYTES {
        bail!("delta target too large");
    }
    let mut out = Vec::with_capacity(dst);
    while p < d.len() {
        let op = d[p];
        p += 1;
        if op & 0x80 != 0 {
            let mut off = 0usize;
            let mut len = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    off |= (*d.get(p).context("truncated copy")? as usize) << (8 * i);
                    p += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len |= (*d.get(p).context("truncated copy")? as usize) << (8 * i);
                    p += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let end = off.checked_add(len).context("copy overflows")?;
            out.extend_from_slice(base.get(off..end).context("copy outside the base")?);
        } else if op != 0 {
            let n = op as usize;
            out.extend_from_slice(d.get(p..p + n).context("truncated insert")?);
            p += n;
        } else {
            bail!("delta opcode 0 is reserved");
        }
        if out.len() > dst {
            bail!("delta overruns its target size");
        }
    }
    if out.len() != dst {
        bail!("delta produced {} bytes, expected {dst}", out.len());
    }
    Ok(out)
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    let s = b.get(at..at + 4).context("truncated")?;
    Ok(u32::from_be_bytes(s.try_into()?))
}

pub fn parse_oid(hex_oid: &str) -> Result<[u8; 20]> {
    let v = hex::decode(hex_oid).context("object id is not hex")?;
    v.try_into()
        .map_err(|_| anyhow::anyhow!("only SHA-1 object ids are supported"))
}

// ── parsed objects ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub oid: String,
}

impl TreeEntry {
    pub fn is_dir(&self) -> bool {
        self.mode == "40000"
    }
    pub fn is_submodule(&self) -> bool {
        self.mode == "160000"
    }
    pub fn is_symlink(&self) -> bool {
        self.mode == "120000"
    }
}

/// A tree's entries, directories first, then by name.
pub fn parse_tree(d: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < d.len() {
        let sp = d[p..]
            .iter()
            .position(|&c| c == b' ')
            .context("bad tree entry")?
            + p;
        let nul = d[sp..]
            .iter()
            .position(|&c| c == 0)
            .context("bad tree entry")?
            + sp;
        let oid = d.get(nul + 1..nul + 21).context("truncated tree entry")?;
        out.push(TreeEntry {
            mode: String::from_utf8_lossy(&d[p..sp]).into_owned(),
            name: String::from_utf8_lossy(&d[sp + 1..nul]).into_owned(),
            oid: hex::encode(oid),
        });
        p = nul + 21;
    }
    out.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// `Name <email> <unix time> <tz>` of a commit or tag.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
    pub tz: String,
}

impl Signature {
    fn parse(s: &str) -> Self {
        let (who, when) = match s.rfind('>') {
            Some(i) => (&s[..=i], s[i + 1..].trim()),
            None => (s, ""),
        };
        let (name, email) = match who.find('<') {
            Some(i) => (who[..i].trim(), who[i + 1..].trim_end_matches('>')),
            None => (who.trim(), ""),
        };
        let mut w = when.split_whitespace();
        Signature {
            name: name.to_string(),
            email: email.to_string(),
            time: w.next().and_then(|t| t.parse().ok()).unwrap_or(0),
            tz: w.next().unwrap_or("").to_string(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Commit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

impl Commit {
    pub fn parse(d: &[u8]) -> Result<Self> {
        let text = String::from_utf8_lossy(d);
        let (head, msg) = text.split_once("\n\n").unwrap_or((&text, ""));
        let mut c = Commit {
            message: msg.to_string(),
            ..Default::default()
        };
        for line in head.lines() {
            // Continuation lines (gpgsig) start with a space.
            if line.starts_with(' ') {
                continue;
            }
            match line.split_once(' ') {
                Some(("tree", v)) => c.tree = v.to_string(),
                Some(("parent", v)) => c.parents.push(v.to_string()),
                Some(("author", v)) => c.author = Signature::parse(v),
                Some(("committer", v)) => c.committer = Signature::parse(v),
                _ => {}
            }
        }
        if c.tree.is_empty() {
            bail!("commit has no tree");
        }
        Ok(c)
    }

    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Tag {
    pub object: String,
    pub kind: String,
    pub name: String,
    pub message: String,
}

impl Tag {
    pub fn parse(d: &[u8]) -> Result<Self> {
        let text = String::from_utf8_lossy(d);
        let (head, msg) = text.split_once("\n\n").unwrap_or((&text, ""));
        let mut t = Tag {
            message: msg.to_string(),
            ..Default::default()
        };
        for line in head.lines() {
            match line.split_once(' ') {
                Some(("object", v)) => t.object = v.to_string(),
                Some(("type", v)) => t.kind = v.to_string(),
                Some(("tag", v)) => t.name = v.to_string(),
                _ => {}
            }
        }
        if t.object.is_empty() {
            bail!("tag has no object");
        }
        Ok(t)
    }
}

/// Walk `path` (`a/b/c`, empty = the root) from a root tree. Returns the
/// object id and whether it is a tree.
pub fn walk(packs: &PackSet, root_tree: &str, path: &str) -> Result<(String, Kind)> {
    let mut oid = root_tree.to_string();
    let mut kind = Kind::Tree;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if kind != Kind::Tree {
            bail!("{path}: not a directory");
        }
        let (k, d) = packs.object(&oid)?;
        if k != Kind::Tree {
            bail!("{path}: not a directory");
        }
        let e = parse_tree(&d)?
            .into_iter()
            .find(|e| e.name == part)
            .with_context(|| format!("{path}: no such file or directory"))?;
        if e.is_submodule() {
            bail!("{path} is a submodule");
        }
        kind = if e.is_dir() { Kind::Tree } else { Kind::Blob };
        oid = e.oid;
    }
    Ok((oid, kind))
}

/// Up to `limit` commits from `start`, first parent only (a linear log).
pub fn log(packs: &PackSet, start: &str, limit: usize) -> Result<Vec<(String, Commit)>> {
    let mut out = Vec::new();
    let mut seen = HashMap::new();
    let mut cur = Some(start.to_string());
    while let Some(oid) = cur.take() {
        if out.len() >= limit || seen.insert(oid.clone(), ()).is_some() || !packs.has(&oid) {
            break;
        }
        let (k, d) = packs.object(&oid)?;
        if k != Kind::Commit {
            break;
        }
        let c = Commit::parse(&d)?;
        cur = c.parents.first().cloned();
        out.push((oid, c));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_copy_and_insert() {
        let base = b"hello world";
        // src 11, dst 11: copy 6 bytes from 0 ("hello "), insert "there".
        let d = [
            11,
            11,
            0x80 | 0x01 | 0x10,
            0,
            6,
            5,
            b't',
            b'h',
            b'e',
            b'r',
            b'e',
        ];
        assert_eq!(apply_delta(base, &d).unwrap(), b"hello there");
        // Wrong base size is refused.
        assert!(apply_delta(b"x", &d).is_err());
    }

    #[test]
    fn signatures_and_commits() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nauthor A U Thor <a@example.com> 1700000000 +0100\ncommitter C <c@example.com> 1700000001 -0500\n\nsubject line\n\nbody\n";
        let c = Commit::parse(raw).unwrap();
        assert_eq!(c.tree, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.author.name, "A U Thor");
        assert_eq!(c.author.email, "a@example.com");
        assert_eq!(c.author.time, 1_700_000_000);
        assert_eq!(c.subject(), "subject line");
    }

    #[test]
    fn trees_sort_dirs_first() {
        let mut raw = Vec::new();
        for (mode, name) in [("100644", "b.txt"), ("40000", "src"), ("100644", "a.txt")] {
            raw.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            raw.extend_from_slice(&[7u8; 20]);
        }
        let t = parse_tree(&raw).unwrap();
        let names: Vec<_> = t.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["src", "a.txt", "b.txt"]);
        assert!(t[0].is_dir());
    }

    /// Against real git: build a repo with deltas, pack it, read it back.
    #[test]
    fn reads_a_real_pack() {
        use std::process::Command;
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("c0mpute-pack-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let o = Command::new("git")
                .current_dir(&dir)
                .args([
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@e",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        let big: String = (0..400)
            .map(|i| format!("line {i} of a file that deltas well\n"))
            .collect();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.txt"), &big).unwrap();
        std::fs::write(dir.join("README.md"), "# hi\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("src/lib.txt"), format!("{big}one more line\n")).unwrap();
        git(&["commit", "-q", "-am", "second"]);
        git(&["tag", "-a", "v1", "-m", "release"]);
        git(&["repack", "-adq", "--depth=10", "--window=10"]);
        let packdir = dir.join(".git/objects/pack");
        let mut packs = Vec::new();
        for e in std::fs::read_dir(&packdir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "pack") {
                packs.push((
                    std::fs::read(&p).unwrap(),
                    std::fs::read(p.with_extension("idx")).unwrap(),
                ));
            }
        }
        let set = PackSet::new(packs).unwrap();
        let head = git(&["rev-parse", "HEAD"]);
        let tag = git(&["rev-parse", "v1"]);
        let (peeled, k, _) = set.peel(&tag).unwrap();
        assert_eq!((peeled.as_str(), k), (head.as_str(), Kind::Commit));
        let commits = log(&set, &head, 10).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].1.subject(), "second");
        let (blob, k) = walk(&set, &commits[0].1.tree, "src/lib.txt").unwrap();
        assert_eq!(k, Kind::Blob);
        let (_, data) = set.object(&blob).unwrap();
        assert!(
            String::from_utf8(data)
                .unwrap()
                .ends_with("one more line\n")
        );
        // The older version (likely stored as a delta) reads back too.
        let (old, _) = walk(&set, &commits[1].1.tree, "src/lib.txt").unwrap();
        assert_eq!(set.object(&old).unwrap().1, big.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
