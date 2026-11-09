//! `c0mpute dm`: the secure-chat client (DIP-0018).
//!
//! Network commands boot a short-lived libp2p client node (its own identity
//! under `<data_dir>/dm-client`), find mailbox nodes through the peers it
//! connects to, and talk to them over `/c0mpute/rpc/1.0.0`. When this
//! machine also runs a worker, the client signs a home record naming the
//! worker's peer id, so senders can deliver straight to that worker; the
//! client then reads the worker's spool on `dm pull`.
//!
//! The key password comes from `C0MPUTE_DM_PASSWORD` when set (scripts),
//! otherwise from a prompt.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_core::Config;
use c0mpute_core::config;
use c0mpute_core::dm::{self, Dm, Paths};
use c0mpute_secure_chat::identity::{fingerprint_of, normalize_query};
use c0mpute_secure_chat::mailbox::{Limits, Mailbox};
use c0mpute_secure_chat::sealed::{MAX_TTL_SECS, pull_tags, seal};
use c0mpute_secure_chat::state::{LocalState, Received, state_file_path};
use c0mpute_secure_chat::{self as chat, ChatKey, HomeRecord, KeyFile, KeyRecord, PublicIdentity, unix_now};

use super::{ChatCmd, ContactsCmd, KeyCmd, prompt_new_password};

/// How long home and key records stay valid; refreshed on every network command.
const RECORD_SECS: u64 = 30 * 86_400;

fn password(prompt: &str) -> Result<String> {
    if let Ok(p) = std::env::var("C0MPUTE_DM_PASSWORD")
        && !p.is_empty()
    {
        return Ok(p);
    }
    Ok(rpassword::prompt_password(prompt)?)
}

struct Session {
    key: ChatKey,
    state: LocalState,
    state_path: std::path::PathBuf,
    cfg: Config,
}

impl Session {
    fn open() -> Result<Self> {
        let path = chat::key_file_path()?;
        let kf = chat::load_keyfile(&path).with_context(|| "no chat key yet: run `c0mpute dm keygen`".to_string())?;
        let key = chat::decrypt_keyfile(&kf, &password("Chat key password: ")?)?;
        let state_path = state_file_path()?;
        let state = LocalState::load(&state_path, &key)?;
        let cfg = Config::load_or_default(&config::default_config_path())?;
        Ok(Self {
            key,
            state,
            state_path,
            cfg,
        })
    }

    fn save(&self) -> Result<()> {
        self.state.save(&self.state_path, &self.key)
    }

    fn paths(&self) -> Paths {
        Paths::under(&self.cfg.storage.root)
    }

    /// If this machine has a worker identity, make that worker this user's
    /// home: it accepts direct deliveries and gossips the record while up.
    fn register_home(&self) -> Option<String> {
        let dir = config::config_dir()?;
        let peer = dm::worker_peer_id(&dir)?;
        let rec = HomeRecord::sign(&self.key, &peer, unix_now(), RECORD_SECS).ok()?;
        dm::write_home_record(&self.paths().homes, &rec).ok()?;
        Some(peer)
    }

    /// Resolve TO from local knowledge, then the network.
    async fn resolve(&mut self, who: &str, net: Option<&Dm>) -> Result<PublicIdentity> {
        if let Some(id) = self.state.resolve(who) {
            return Ok(id);
        }
        let Some(net) = net else {
            bail!("unknown recipient {who}: use an address, fingerprint, contact name, or c0dm1. identity");
        };
        if normalize_query(who).is_none() {
            bail!("unknown contact {who}");
        }
        match net.lookup(who).await {
            Some(id) => {
                self.state.remember(&id);
                Ok(id)
            }
            None => bail!(
                "no key record found for {who}. Ask them for their identity string (`c0mpute dm key show --full`)"
            ),
        }
    }
}

/// Boot the client node and wait briefly for mailboxes.
async fn connect(s: &Session) -> Result<Dm> {
    let id_dir = config::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("dm-client");
    let net = dm::boot_client(&id_dir).await?;
    let d = Dm::start(net, dm::Options::default()).await?;
    let n = d.wait_for_mailboxes(1, Duration::from_secs(20)).await;
    if n == 0 {
        eprintln!("warning: no mailbox nodes reachable yet (is a c0mpute worker or gateway reachable?)");
    }
    // Publish our key record so others can find us by fingerprint.
    if let Ok(rec) = KeyRecord::sign(&s.key, unix_now(), RECORD_SECS) {
        let _ = d.publish_key(&rec).await;
    }
    Ok(d)
}

pub async fn run_chat(cmd: ChatCmd) -> Result<()> {
    match cmd {
        ChatCmd::Keygen => keygen(),
        ChatCmd::Key { cmd } => key_cmd(cmd),
        ChatCmd::Restore { from_backup } => {
            let dest = chat::key_file_path()?;
            if dest.exists() {
                eprintln!("A keyfile already exists at {}. Remove it first.", dest.display());
                bail!("keyfile already exists");
            }
            let raw = std::fs::read_to_string(&from_backup)?;
            let kf: KeyFile = serde_json::from_str(&raw)?;
            // Verify the password decrypts correctly before saving.
            chat::decrypt_keyfile(&kf, &password("Password: ")?)?;
            chat::save_keyfile(&dest, &kf)?;
            println!("keyfile restored to {}", dest.display());
            Ok(())
        }
        ChatCmd::Send {
            to,
            message,
            ttl_days,
            sealed: _,
        } => {
            let mut s = Session::open()?;
            s.register_home();
            let net = connect(&s).await?;
            let rcpt = s.resolve(&to, Some(&net)).await?;
            let ttl = (ttl_days * 86_400).clamp(60, MAX_TTL_SECS);
            let sealed = seal(&s.key, &rcpt, &message, ttl, unix_now())?;
            let report = net.send(&sealed, &rcpt.address(), ttl).await?;
            s.save()?;
            let name = s.state.name_of(&rcpt.address());
            match report.direct {
                Some(peer) => println!("delivered directly to {name}'s node ({peer})"),
                None => println!(
                    "{name} is offline: held on {} mailbox node(s) for up to {} days",
                    report.stored_on.len(),
                    ttl / 86_400
                ),
            }
            Ok(())
        }
        ChatCmd::Pull => {
            let mut s = Session::open()?;
            s.register_home();
            let now = unix_now();
            let me = s.key.address();
            let mut new = 0usize;
            let mut dropped = 0usize;
            let mut bad = 0usize;
            // 1. Direct deliveries on this machine's node.
            let spool_dir = s.paths().spool;
            if spool_dir.exists() {
                let mut spool = Mailbox::open(&spool_dir, Limits::default(), now);
                for tag in pull_tags(&me, now) {
                    for (id, bytes) in spool.fetch(&tag, now) {
                        let res = chat::SealedMessage::parse(&bytes)
                            .and_then(|m| s.state.receive(&s.key, &m, "direct", now));
                        match res {
                            Ok(r) => {
                                tally(&r, &mut new, &mut dropped);
                                spool.ack(&tag, &id, r.ack());
                            }
                            Err(e) => {
                                bad += 1;
                                eprintln!("skipped a direct message: {e}");
                            }
                        }
                    }
                }
            }
            // 2. Mailboxes.
            let net = connect(&s).await?;
            for f in net.fetch(&me).await {
                match s.state.receive(&s.key, &f.msg, "mailbox", unix_now()) {
                    Ok(r) => {
                        tally(&r, &mut new, &mut dropped);
                        net.ack(&f, r.ack()).await;
                    }
                    Err(e) => {
                        bad += 1;
                        eprintln!("skipped a mailbox message: {e}");
                    }
                }
            }
            s.save()?;
            println!(
                "{new} new message(s){}{}",
                if dropped > 0 { format!(", {dropped} from blocked senders dropped") } else { String::new() },
                if bad > 0 { format!(", {bad} rejected") } else { String::new() }
            );
            if new > 0 {
                print_inbox(&s.state, new);
            }
            Ok(())
        }
        ChatCmd::Inbox { limit } => {
            let s = Session::open()?;
            if s.state.inbox.is_empty() {
                println!("inbox empty (run `c0mpute dm pull`)");
            } else {
                print_inbox(&s.state, limit);
            }
            Ok(())
        }
        ChatCmd::Lookup { query } => {
            let mut s = Session::open()?;
            let id = match s.state.resolve(&query) {
                Some(id) => id,
                None => {
                    let net = connect(&s).await?;
                    s.resolve(&query, Some(&net)).await?
                }
            };
            s.save()?;
            println!("address     : {}", id.address());
            println!("fingerprint : {}", id.fingerprint());
            println!("identity    : {}", id.to_share_string());
            Ok(())
        }
        ChatCmd::Contacts { cmd } => match cmd.unwrap_or(ContactsCmd::Ls) {
            ContactsCmd::Ls => {
                let s = Session::open()?;
                if s.state.contacts.is_empty() {
                    println!("no contacts (add one: c0mpute dm contacts add <address|fingerprint> --name alice)");
                }
                for (addr, c) in &s.state.contacts {
                    let blocked = if s.state.blocked.contains(addr) { "  [blocked]" } else { "" };
                    println!("{:<16} {}  {addr}{blocked}", c.name.as_deref().unwrap_or("-"), fingerprint_of(addr));
                }
                Ok(())
            }
            ContactsCmd::Add { who, name } => {
                let mut s = Session::open()?;
                let id = match s.state.resolve(&who) {
                    Some(id) => id,
                    None => {
                        let net = connect(&s).await?;
                        s.resolve(&who, Some(&net)).await?
                    }
                };
                let addr = s.state.add_contact(id, name, unix_now());
                s.save()?;
                println!("added {} ({})", s.state.name_of(&addr), fingerprint_of(&addr));
                Ok(())
            }
            ContactsCmd::Rm { who } => {
                let mut s = Session::open()?;
                let id = s.state.resolve(&who).context("no such contact")?;
                s.state.contacts.remove(&id.address());
                s.save()?;
                println!("removed {}", fingerprint_of(&id.address()));
                Ok(())
            }
        },
        ChatCmd::Block { who } => {
            let mut s = Session::open()?;
            let addr = match s.state.resolve(&who) {
                Some(id) => id.address(),
                None => match normalize_query(&who) {
                    Some(q) if q.len() == 64 => q,
                    _ => bail!("block needs a full address, or a known contact/fingerprint"),
                },
            };
            s.state.blocked.insert(addr.clone());
            s.save()?;
            println!("blocked {}", fingerprint_of(&addr));
            Ok(())
        }
        ChatCmd::Unblock { who } => {
            let mut s = Session::open()?;
            let addr = s
                .state
                .resolve(&who)
                .map(|id| id.address())
                .or_else(|| normalize_query(&who).filter(|q| q.len() == 64))
                .context("unknown sender")?;
            let was = s.state.blocked.remove(&addr);
            s.save()?;
            println!("{} {}", if was { "unblocked" } else { "was not blocked:" }, fingerprint_of(&addr));
            Ok(())
        }
    }
}

fn tally(r: &Received, new: &mut usize, dropped: &mut usize) {
    match r {
        Received::New(_) => *new += 1,
        Received::Blocked(_) => *dropped += 1,
    }
}

fn print_inbox(state: &LocalState, limit: usize) {
    let start = state.inbox.len().saturating_sub(limit);
    for m in &state.inbox[start..] {
        println!("[{}] {} ({}): {}", fmt_time(m.created_at), state.name_of(&m.from), m.via, m.text);
    }
}

fn fmt_time(t: u64) -> String {
    let ago = unix_now().saturating_sub(t);
    match ago {
        0..=59 => format!("{ago}s ago"),
        60..=3599 => format!("{}m ago", ago / 60),
        3600..=86_399 => format!("{}h ago", ago / 3600),
        _ => format!("{}d ago", ago / 86_400),
    }
}

fn keygen() -> Result<()> {
    let path = chat::key_file_path()?;
    if path.exists() {
        eprintln!(
            "A keyfile already exists at {}.\n\
             Use `c0mpute dm key rotate` for how to replace it.",
            path.display()
        );
        bail!("keyfile already exists");
    }
    let key = ChatKey::generate();
    let password = match std::env::var("C0MPUTE_DM_PASSWORD") {
        Ok(p) if p.len() >= 10 => p,
        _ => prompt_new_password()?,
    };
    let kf = key.encrypt_to_keyfile(&password, None)?;
    chat::save_keyfile(&path, &kf)?;
    let id = key.public_identity();
    println!("keypair generated");
    println!("  address     : {}", id.address());
    println!("  fingerprint : {}", id.fingerprint());
    println!("  keyfile     : {}", path.display());
    println!();
    println!("Share your fingerprint (others look it up) or your full identity:");
    println!("  {}", id.to_share_string());
    println!();
    println!("── encrypted backup (save this somewhere safe) ──");
    println!("{}", serde_json::to_string_pretty(&kf)?);
    println!("─────────────────────────────────────────────────");
    println!("restore with: c0mpute dm restore --from-backup <file>");
    Ok(())
}

fn key_cmd(cmd: KeyCmd) -> Result<()> {
    match cmd {
        KeyCmd::Show { full } => {
            let kf = chat::load_keyfile(&chat::key_file_path()?)?;
            // Public fields only: no password needed.
            let id = PublicIdentity::from_keyfile(&kf);
            println!("address     : {}", id.address());
            println!("fingerprint : {}", id.fingerprint());
            if let Some(did) = &kf.did {
                println!("did         : {did}");
            }
            if full {
                println!("identity    : {}", id.to_share_string());
            }
            Ok(())
        }
        KeyCmd::Export => {
            let kf = chat::load_keyfile(&chat::key_file_path()?)?;
            println!("{}", serde_json::to_string_pretty(&kf)?);
            Ok(())
        }
        KeyCmd::Rotate => {
            println!("To replace your keypair (your address changes; tell your contacts):");
            println!("  1. c0mpute dm pull            (collect anything still queued)");
            println!("  2. move ~/.config/c0mpute/chat.key and chat-state.enc aside");
            println!("  3. c0mpute dm keygen");
            Ok(())
        }
    }
}
