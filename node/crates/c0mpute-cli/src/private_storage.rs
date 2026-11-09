//! The private (end-to-end encrypted) side of `c0mpute storage` and
//! `c0mpute hosting publish --private` (DIP-0012 `private` tier).
//!
//! Everything here encrypts or decrypts on this machine. What goes to the
//! network, a gateway or a log is ciphertext, object ids and copy counts:
//! never a key, never plaintext, never a file name.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_core::sealed_store::{self, BlobStore, GatewayStore, IndexEntry, LocalIndex, NetworkStore, ObjectRef, PutOptions};
use c0mpute_sealed::{Kind, StoragePublic, StorageSecret};
use c0mpute_secure_chat::{self as chat, ChatKey};

/// Password for the c0mpute identity keyfile: `C0MPUTE_DM_PASSWORD` for
/// scripts, otherwise a prompt.
fn password(prompt: &str) -> Result<String> {
    if let Ok(p) = std::env::var("C0MPUTE_DM_PASSWORD")
        && !p.is_empty()
    {
        return Ok(p);
    }
    Ok(rpassword::prompt_password(prompt)?)
}

const LOST_KEY_WARNING: &str = "\
  Your private objects are encrypted on this machine with keys only you hold.
  Nobody (not c0mpute, not node operators, not support) can recover them.
  LOSE THIS IDENTITY AND ITS PASSWORD AND YOUR PRIVATE DATA IS GONE FOR GOOD.";

/// Load this machine's c0mpute identity (the same one `c0mpute dm` uses) and
/// derive the storage secret from it. On first use, with `create`, make one,
/// and refuse to continue until its encrypted backup has been saved.
pub fn load_identity(create: bool, backup_to: Option<&Path>) -> Result<StorageSecret> {
    let path = chat::key_file_path()?;
    if path.exists() {
        let kf = chat::load_keyfile(&path)?;
        let pw = zeroize::Zeroizing::new(password("c0mpute identity password: ")?);
        let key = chat::decrypt_keyfile(&kf, &pw)?;
        let secret = sealed_store::secret_for(&key);
        sealed_store::Freshness::install_for(&secret);
        return Ok(secret);
    }
    if !create {
        bail!("no c0mpute identity on this machine yet: run `c0mpute storage key init --backup-to <file>`");
    }
    init_identity(backup_to)
}

fn init_identity(backup_to: Option<&Path>) -> Result<StorageSecret> {
    let path = chat::key_file_path()?;
    if path.exists() {
        bail!("an identity already exists at {}", path.display());
    }
    eprintln!("Creating your c0mpute identity (first use of private storage).\n");
    eprintln!("{LOST_KEY_WARNING}\n");
    let interactive = std::io::stdin().is_terminal();
    if backup_to.is_none() && !interactive {
        bail!(
            "first use needs somewhere to put the encrypted key backup: pass\n\
             --backup-to <file> (ideally on another disk or a USB stick)"
        );
    }
    let pw = zeroize::Zeroizing::new(match std::env::var("C0MPUTE_DM_PASSWORD") {
        Ok(p) if p.len() >= 10 => p,
        Ok(p) if !p.is_empty() => bail!("C0MPUTE_DM_PASSWORD must be at least 10 characters"),
        _ => crate::prompt_new_password()?,
    });
    let key = ChatKey::generate();
    let kf = key.encrypt_to_keyfile(&pw, None)?;
    let backup = serde_json::to_string_pretty(&kf)?;

    // The backup is mandatory: written and read back, or confirmed by hand,
    // before the identity is saved. No identity exists without one.
    match backup_to {
        Some(dest) => {
            write_private_file(dest, backup.as_bytes())?;
            let back: chat::KeyFile = serde_json::from_slice(&std::fs::read(dest)?)?;
            chat::decrypt_keyfile(&back, &pw).context("backup did not read back correctly")?;
            eprintln!("encrypted backup written to {} (and verified)", dest.display());
        }
        None => {
            eprintln!("── encrypted backup: save this text somewhere safe, off this machine ──");
            eprintln!("{backup}");
            eprintln!("────────────────────────────────────────────────────────────────────────");
            eprintln!("Restore it on any machine with: c0mpute dm restore --from-backup <file>");
            loop {
                eprint!("Type SAVED once the backup is stored somewhere safe: ");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if line.trim() == "SAVED" {
                    break;
                }
            }
        }
    }
    chat::save_keyfile(&path, &kf)?;
    let secret = sealed_store::secret_for(&key);
    sealed_store::Freshness::install_for(&secret);
    eprintln!("identity saved to {}", path.display());
    eprintln!("storage identity: {}", secret.public().to_share_string());
    eprintln!("  (give this to people who want to share private objects with you)\n");
    Ok(secret)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create {} (it must not exist yet)", path.display()))?;
        f.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

fn parse_recipients(raw: &[String]) -> Result<Vec<StoragePublic>> {
    raw.iter()
        .map(|r| StoragePublic::from_share_string(r).with_context(|| format!("`{r}` is not a storage identity (c0ps1...)")))
        .collect()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

fn remember(secret: &StorageSecret, site_id: &str, entry: IndexEntry) {
    let path = LocalIndex::default_path();
    let mut idx = LocalIndex::load(&path, secret).unwrap_or_default();
    idx.objects.insert(site_id.to_string(), entry);
    if let Err(e) = idx.save(&path, secret) {
        eprintln!("warning: could not update the local object list: {e:#}");
    }
}

fn forget(secret: &StorageSecret, site_id: &str) {
    let path = LocalIndex::default_path();
    if let Ok(mut idx) = LocalIndex::load(&path, secret) {
        idx.objects.remove(site_id);
        let _ = idx.save(&path, secret);
    }
}

async fn wait_copies(store: &NetworkStore, wait: Duration) -> usize {
    let counts = store
        .wait_for_copies(wait, |id, n| eprintln!("  {id}: {n}/{} network copies", c0mpute_core::hosting::TARGET_REPLICAS))
        .await;
    counts.values().copied().min().unwrap_or(0)
}

fn copies_note(copies: usize) {
    if copies < c0mpute_core::hosting::TARGET_REPLICAS {
        eprintln!(
            "note: fewer than {} hosting nodes took a copy before the wait ran out; the\n\
             network keeps replicating, but re-run the command if no copies appear",
            c0mpute_core::hosting::TARGET_REPLICAS
        );
    }
}

/// `c0mpute storage put FILE` (the default, private).
pub async fn put(file: &Path, share_with: &[String], gateway: &str, wait: Duration, backup_to: Option<&Path>) -> Result<()> {
    let rcpts = parse_recipients(share_with)?;
    let secret = load_identity(true, backup_to)?;
    let bytes = tokio::fs::read(file).await.with_context(|| format!("read {}", file.display()))?;
    let file_name = file.file_name().map(|n| n.to_string_lossy().to_string());
    let content_type = file_name.as_deref().map(|n| c0mpute_core::hosting::site::content_type_for(n).to_string());
    eprintln!("  encrypting {} on this machine...", human(bytes.len() as u64));
    let store = NetworkStore::start(gateway).await?;
    let res = sealed_store::put_bytes(
        &store,
        &secret,
        &bytes,
        PutOptions {
            file_name: file_name.clone(),
            content_type,
            share_with: rcpts.clone(),
        },
    )
    .await?;
    remember(
        &secret,
        &res.site_id,
        IndexEntry {
            file_name,
            size: bytes.len() as u64,
            kind: if res.published.len() > 1 { Kind::Multipart } else { Kind::File },
            created: c0mpute_core::payments::now_secs(),
            shared_with: rcpts.len(),
        },
    );
    eprintln!(
        "  published {} object(s), {} of ciphertext; waiting for network copies...",
        res.published.len(),
        human(res.stored_bytes)
    );
    let copies = wait_copies(&store, wait).await;
    println!("{}", sealed_store::plain_ref(&res.site_id));
    eprintln!("  private: only you{} can read it", if rcpts.is_empty() { String::new() } else { format!(" and {} other identit(ies)", rcpts.len()) });
    eprintln!("  get it back:   c0mpute storage get {} -o <file>", sealed_store::plain_ref(&res.site_id));
    eprintln!("  share by link: c0mpute storage link {}", sealed_store::plain_ref(&res.site_id));
    copies_note(copies);
    Ok(())
}

/// Seal a file for `storage put --local`: the ciphertext goes into this
/// node's own store. Returns the blob.
pub fn seal_local(bytes: &[u8], file: &Path, backup_to: Option<&Path>) -> Result<Vec<u8>> {
    if bytes.len() > sealed_store::PART_BYTES {
        bail!("--local private objects are limited to {} MiB; drop --local to store bigger files on the network", sealed_store::PART_BYTES >> 20);
    }
    let secret = load_identity(true, backup_to)?;
    let name = c0mpute_sealed::random_name();
    let file_name = file.file_name().map(|n| n.to_string_lossy().to_string());
    let sealed = c0mpute_sealed::seal(
        bytes,
        &c0mpute_sealed::SealParams {
            kind: Kind::File,
            name: &name,
            owner: &secret,
            content_type: file_name.as_deref().map(|n| c0mpute_core::hosting::site::content_type_for(n).to_string()),
            file_name,
            files: Default::default(),
            parts: Vec::new(),
            recipients: &[secret.public()],
            created: c0mpute_core::payments::now_secs(),
            version: 0,
        },
    )?;
    Ok(sealed.blob)
}

/// True when `bytes` is a sealed object (so `get` decrypts it).
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..8] == c0mpute_sealed::format::MAGIC
}

/// Decrypt a sealed object read from this node's own store.
pub fn open_local(bytes: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    let secret = load_identity(false, None)?;
    let (opened, _) = c0mpute_sealed::open_with_secret(bytes, &secret, None)?;
    Ok(opened.content)
}

/// `c0mpute storage get c0p1:...`.
pub async fn get(reference: &str, out: Option<&Path>, raw: bool, gateway: &str) -> Result<()> {
    let r = ObjectRef::parse(reference)?;
    let store = GatewayStore::new(gateway);
    if raw {
        // The ciphertext exactly as operators hold it: for audits.
        let blob = store.get(&r.site_id).await?;
        return write_out(out, &blob);
    }
    let secret = if r.key.is_none() { Some(load_identity(false, None)?) } else { None };
    let fetched = sealed_store::get(&store, &r, secret.as_ref()).await?;
    let meta = &fetched.opened.meta;
    if meta.kind == Kind::Site {
        let Some(dir) = out else {
            bail!("this is a private site ({} files): pass -o <directory> to unpack it", meta.files.len());
        };
        let n = sealed_store::unpack_site(&fetched.opened, dir)?;
        eprintln!("unpacked {n} file(s) into {}", dir.display());
        return Ok(());
    }
    if let Some(name) = &meta.file_name {
        eprintln!("  {name}, {}", human(meta.size));
    }
    write_out(out, &fetched.opened.content)
}

fn write_out(out: Option<&Path>, bytes: &[u8]) -> Result<()> {
    match out {
        Some(p) => {
            std::fs::write(p, bytes).with_context(|| format!("write {}", p.display()))?;
            eprintln!("wrote {} to {}", human(bytes.len() as u64), p.display());
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(bytes)?;
        }
    }
    Ok(())
}

/// `c0mpute storage share REF --with c0ps1...`.
pub async fn share(reference: &str, with: &[String], gateway: &str, wait: Duration) -> Result<()> {
    let add = parse_recipients(with)?;
    if add.is_empty() {
        bail!("say who to share with: --with c0ps1... (they get it from `c0mpute storage key show`)");
    }
    let r = ObjectRef::parse(reference)?;
    let secret = load_identity(false, None)?;
    let store = NetworkStore::start(gateway).await?;
    let roster = sealed_store::share(&store, &secret, &r.site_id, &add).await?;
    let copies = wait_copies(&store, wait).await;
    println!("{} is now readable by {} identit(ies) (you included)", sealed_store::plain_ref(&r.site_id), roster.len());
    eprintln!("  they read it with: c0mpute storage get {} -o <file>", sealed_store::plain_ref(&r.site_id));
    copies_note(copies);
    Ok(())
}

/// `c0mpute storage revoke REF --from c0ps1...`.
pub async fn revoke(reference: &str, from: &[String], gateway: &str, wait: Duration) -> Result<()> {
    let remove = parse_recipients(from)?;
    let r = ObjectRef::parse(reference)?;
    let secret = load_identity(false, None)?;
    let store = NetworkStore::start(gateway).await?;
    let (roster, res) = sealed_store::revoke(&store, &secret, &r.site_id, &remove).await?;
    let copies = wait_copies(&store, wait).await;
    println!(
        "{} re-encrypted under a new key; {} identit(ies) can read the new version",
        sealed_store::plain_ref(&res.site_id),
        roster.len()
    );
    eprintln!(
        "  every earlier link and key is dead. Anyone you removed may still have a copy of\n  \
         what they could read before; revocation only protects the data from now on."
    );
    copies_note(copies);
    Ok(())
}

/// `c0mpute storage link REF`: print a capability link.
pub async fn link(reference: &str, gateway: &str) -> Result<()> {
    let r = ObjectRef::parse(reference)?;
    let secret = if r.key.is_none() { Some(load_identity(false, None)?) } else { None };
    let fetched = sealed_store::get(&GatewayStore::new(gateway), &r, secret.as_ref()).await?;
    println!("{}", sealed_store::browser_link(gateway, &r.site_id, &fetched.dek).as_str());
    println!("{}", sealed_store::capability_ref(&r.site_id, &fetched.dek).as_str());
    eprintln!(
        "  anyone with either link can read this object. The key is after #, which browsers\n  \
         never send to the gateway. To kill every link: c0mpute storage revoke {}",
        sealed_store::plain_ref(&r.site_id)
    );
    Ok(())
}

/// `c0mpute storage rm c0p1:...`: replace with a tombstone.
pub async fn rm(reference: &str, yes: bool, gateway: &str) -> Result<()> {
    let r = ObjectRef::parse(reference)?;
    if !yes {
        eprintln!("about to delete {} from the network; re-run with --yes to confirm", sealed_store::plain_ref(&r.site_id));
        return Ok(());
    }
    let secret = load_identity(false, None)?;
    let store = NetworkStore::start(gateway).await?;
    let gone = sealed_store::delete(&store, &secret, &r.site_id).await?;
    wait_copies(&store, Duration::from_secs(60)).await;
    forget(&secret, &r.site_id);
    println!("deleted {} ({} object(s) tombstoned; holders drop the ciphertext)", sealed_store::plain_ref(&r.site_id), gone.len());
    Ok(())
}

/// `c0mpute storage ls --private`.
pub fn ls() -> Result<()> {
    let secret = load_identity(false, None)?;
    let idx = LocalIndex::load(&LocalIndex::default_path(), &secret)?;
    if idx.objects.is_empty() {
        println!("no private objects recorded on this machine");
        return Ok(());
    }
    println!("{:<28} {:>10}  {:<9} NAME", "OBJECT", "SIZE", "KIND");
    for (id, e) in &idx.objects {
        println!(
            "{:<28} {:>10}  {:<9} {}",
            sealed_store::plain_ref(id),
            human(e.size),
            format!("{:?}", e.kind).to_lowercase(),
            e.file_name.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

/// `c0mpute storage key ...`.
pub fn key(cmd: KeyCmd) -> Result<()> {
    match cmd {
        KeyCmd::Init { backup_to } => {
            init_identity(backup_to.as_deref())?;
            Ok(())
        }
        KeyCmd::Show => {
            let secret = load_identity(false, None)?;
            let p = secret.public();
            println!("storage identity: {}", p.to_share_string());
            println!("fingerprint:      {}", p.fingerprint());
            Ok(())
        }
        KeyCmd::Backup { out } => {
            let kf = chat::load_keyfile(&chat::key_file_path()?)?;
            let json = serde_json::to_string_pretty(&kf)?;
            match out {
                Some(p) => {
                    write_private_file(&p, json.as_bytes())?;
                    eprintln!("encrypted backup written to {}", p.display());
                }
                None => println!("{json}"),
            }
            eprintln!("{LOST_KEY_WARNING}");
            Ok(())
        }
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum KeyCmd {
    /// Create your c0mpute identity and its mandatory encrypted backup.
    Init {
        /// Write the encrypted backup here (it must not exist yet). Without
        /// it the backup is printed and you confirm you saved it.
        #[arg(long)]
        backup_to: Option<PathBuf>,
    },
    /// Print your storage identity (`c0ps1...`), which others use to share
    /// private objects with you. Public; safe to give out.
    Show,
    /// Print (or write) the password-encrypted identity backup again.
    Backup {
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// `c0mpute hosting publish DIR --name N --private`.
pub async fn publish_site(dir: &Path, name: &str, rekey: bool, gateway: &str, wait: Duration) -> Result<()> {
    if !c0mpute_core::hosting::site::valid_name(name) {
        bail!("site name must be 1-63 of a-z, 0-9 and '-'");
    }
    let secret = load_identity(true, None)?;
    let store = NetworkStore::start(gateway).await?;
    let res = sealed_store::put_site(&store, &secret, dir, name, rekey).await?;
    remember(
        &secret,
        &res.site_id,
        IndexEntry {
            file_name: Some(format!("{name} (private site)")),
            size: res.stored_bytes,
            kind: Kind::Site,
            created: c0mpute_core::payments::now_secs(),
            shared_with: 0,
        },
    );
    eprintln!("  published private site {name} ({} of ciphertext); waiting for network copies...", human(res.stored_bytes));
    let copies = wait_copies(&store, wait).await;
    println!("site:    {}", sealed_store::plain_ref(&res.site_id));
    println!("open:    {}", sealed_store::browser_link(gateway, &res.site_id, &res.dek).as_str());
    println!("copies:  {copies}/{}", c0mpute_core::hosting::TARGET_REPLICAS);
    eprintln!(
        "  only people with the link can see the site; gateways serve ciphertext and the\n  \
         browser decrypts it. Republishing keeps the link; --rekey kills every old link."
    );
    copies_note(copies);
    Ok(())
}
