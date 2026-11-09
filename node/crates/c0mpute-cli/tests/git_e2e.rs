//! End to end: real `git push` / `git clone` through the real
//! `git-remote-c0mpute` (this crate's binary under that name) against
//! hosting nodes and gateways on loopback libp2p + HTTP.
//!
//! * the owner pushes to gateway A; hosting nodes take three copies;
//! * gateway B, which holds nothing, serves clones over the remote helper
//!   (a reader with a different key) and over git's dumb HTTP protocol;
//! * an incremental push, a pull, a push from the owner's second checkout,
//!   a refused non-fast-forward, a forced push, a refused stranger push and
//!   an annotated tag.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::hosting::{Hosting, Options, Timing};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

async fn node(dir: &Path, bootstrap: Vec<libp2p::Multiaddr>) -> (Arc<Libp2pNetwork>, ChunkStore) {
    let store = ChunkStore::open(dir.join("store")).await.unwrap();
    let net = Libp2pNetwork::spawn(
        NetworkConfig::for_dir(dir.join("id"))
            .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
            .with_local_source(Arc::new(StoreSource(store.clone())))
            .with_bootstrap(bootstrap),
    )
    .await
    .unwrap();
    (Arc::new(net), store)
}

async fn listen_addr(net: &Libp2pNetwork) -> libp2p::Multiaddr {
    for _ in 0..50 {
        if let Ok(addrs) = net.listen_addrs().await
            && let Some(a) = addrs.into_iter().next()
        {
            return a.with(Protocol::P2p(net.peer_id()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen addr");
}

fn fast() -> Timing {
    Timing {
        announce_every: Duration::from_millis(700),
        records_every: Duration::from_millis(1500),
        repair_every: Duration::from_millis(500),
        holder_ttl: Duration::from_secs(4),
        fetch_timeout: Duration::from_secs(20),
    }
}

async fn wait_for<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn serve(h: &Hosting) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = h.router();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// A person at a machine: own data dir (so own key), own HOME (no global
/// git config), the helper on PATH, a gateway.
#[derive(Clone)]
struct User {
    home: PathBuf,
    bin: PathBuf,
    gateway: String,
}

impl User {
    fn new(root: &Path, name: &str, bin: &Path, gateway: &str) -> Self {
        let home = root.join(name);
        std::fs::create_dir_all(&home).unwrap();
        Self {
            home,
            bin: bin.to_path_buf(),
            gateway: gateway.to_string(),
        }
    }

    fn cmd(&self, prog: &str, cwd: &Path) -> Command {
        let mut c = Command::new(prog);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&path));
        c.current_dir(cwd)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "riotcoder")
            .env("GIT_AUTHOR_EMAIL", "riotcoder@example.org")
            .env("GIT_COMMITTER_NAME", "riotcoder")
            .env("GIT_COMMITTER_EMAIL", "riotcoder@example.org")
            .env("C0MPUTE_NO_AUTO_UPDATE", "1")
            .env("C0MPUTE_GIT_TRACE", "1")
            .env("C0MPUTE_GIT_GATEWAY", &self.gateway);
        c
    }

    /// Run git, panicking with its output on failure.
    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let started = std::time::Instant::now();
        let out = self.cmd("git", cwd).args(args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!(
            "$ git {}   ({:.1}s)\n{text}",
            args.join(" "),
            started.elapsed().as_secs_f64()
        );
        assert!(
            out.status.success(),
            "git {} failed:\n{text}",
            args.join(" ")
        );
        text
    }

    /// Run git, expecting failure; returns its output.
    fn git_fails(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.cmd("git", cwd).args(args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!("$ git {}  (expected to fail)\n{text}", args.join(" "));
        assert!(
            !out.status.success(),
            "git {} should have failed:\n{text}",
            args.join(" ")
        );
        text
    }

    fn key(&self) -> String {
        let out = self
            .cmd(env!("CARGO_BIN_EXE_c0mpute"), &self.home)
            .args(["git", "key"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

/// `git push`/`git clone` block; run them off the async test threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_push_and_clone_through_the_network() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let tmp = tempfile::Builder::new()
        .prefix("git-e2e")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = tmp.path().to_path_buf();

    // Hub + three more hosting nodes; gateway A also hosts, gateway B holds nothing.
    let opts = |dir: &Path, hosts: bool| Options {
        hosts,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        ..Default::default()
    };
    let (hub_net, hub_store) = node(&root.join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let mut hosts = vec![
        Hosting::start(hub_net, hub_store, opts(&root.join("h0"), true))
            .await
            .unwrap(),
    ];
    for i in 1..4 {
        let d = root.join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, opts(&d, true)).await.unwrap());
    }
    let (an, as_) = node(&root.join("gwa"), vec![hub_addr.clone()]).await;
    let gw_a = Hosting::start(an, as_, opts(&root.join("gwa"), true))
        .await
        .unwrap();
    let (bn, bs) = node(&root.join("gwb"), vec![hub_addr.clone()]).await;
    let gw_b = Hosting::start(bn, bs, opts(&root.join("gwb"), false))
        .await
        .unwrap();
    let url_a = serve(&gw_a).await;
    let url_b = serve(&gw_b).await;
    tokio::time::sleep(Duration::from_secs(3)).await; // gossipsub mesh warm-up

    // The helper: this crate's binary under the helper's name.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_c0mpute"),
        bin.join("git-remote-c0mpute"),
    )
    .unwrap();

    let owner = User::new(&root, "owner", &bin, &url_a);
    let key = owner.key();
    assert_eq!(key.len(), 64);
    let remote = format!("c0mpute://{key}/demo");
    let site_id = c0mpute_core::git::site_id(&key, "demo");

    // 1. First push.
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    {
        let (o, w, r) = (owner.clone(), work.clone(), remote.clone());
        blocking(move || {
            o.git(&w, &["init", "-q", "-b", "main"]);
            std::fs::write(w.join("README.md"), "hello from the network\n").unwrap();
            o.git(&w, &["add", "."]);
            o.git(&w, &["commit", "-q", "-m", "first"]);
            o.git(&w, &["remote", "add", "origin", &r]);
            let out = o.git(&w, &["push", "origin", "main"]);
            assert!(out.contains("main -> main"), "{out}");
        })
        .await;
    }
    let first_sha = owner.git(&work, &["rev-parse", "HEAD"]).trim().to_string();

    // 2. Three copies on hosting nodes, no coordinator.
    wait_for("3 copies of the repo", 60, || {
        let a = gw_a.clone();
        let id = site_id.clone();
        async move { a.holders(&id).await.len() >= 3 }
    })
    .await;
    wait_for("gateway B to learn the repo", 30, || {
        let b = gw_b.clone();
        let id = site_id.clone();
        async move { b.record_for(&id).await.is_some() }
    })
    .await;

    // 3. A reader with another key clones from gateway B through the helper.
    let reader = User::new(&root, "reader", &bin, &url_b);
    assert_ne!(reader.key(), key);
    let clone1 = root.join("clone1");
    {
        let (r, root2, rem, c1) = (reader.clone(), root.clone(), remote.clone(), clone1.clone());
        blocking(move || {
            r.git(&root2, &["clone", "-q", &rem, c1.to_str().unwrap()]);
        })
        .await;
    }
    assert_eq!(
        std::fs::read_to_string(clone1.join("README.md")).unwrap(),
        "hello from the network\n"
    );
    assert_eq!(
        reader.git(&clone1, &["rev-parse", "HEAD"]).trim(),
        first_sha
    );

    // 4. ...and over plain dumb HTTP from the same gateway (no helper, no key).
    let http_url = format!("{url_b}/git/{key}/demo.git");
    let clone2 = root.join("clone2");
    {
        let (r, root2, u, c2) = (
            reader.clone(),
            root.clone(),
            http_url.clone(),
            clone2.clone(),
        );
        blocking(move || {
            r.git(&root2, &["clone", "-q", &u, c2.to_str().unwrap()]);
        })
        .await;
    }
    assert_eq!(
        std::fs::read_to_string(clone2.join("README.md")).unwrap(),
        "hello from the network\n"
    );

    // 5. Incremental push; the reader pulls it.
    {
        let (o, w) = (owner.clone(), work.clone());
        blocking(move || {
            std::fs::write(w.join("second.txt"), "second commit\n").unwrap();
            o.git(&w, &["add", "."]);
            o.git(&w, &["commit", "-q", "-m", "second"]);
            o.git(&w, &["push", "-q", "origin", "main"]);
        })
        .await;
    }
    let second_sha = owner.git(&work, &["rev-parse", "HEAD"]).trim().to_string();
    wait_for("gateway B to learn version 2", 30, || {
        let b = gw_b.clone();
        let id = site_id.clone();
        let a = gw_a.clone();
        async move {
            match (b.record_for(&id).await, a.record_for(&id).await) {
                (Some(rb), Some(ra)) => rb.seq == ra.seq && !b.holders(&id).await.is_empty(),
                _ => false,
            }
        }
    })
    .await;
    {
        let (r, c1) = (reader.clone(), clone1.clone());
        blocking(move || {
            r.git(&c1, &["pull", "-q", "--ff-only"]);
        })
        .await;
    }
    assert_eq!(
        reader.git(&clone1, &["rev-parse", "HEAD"]).trim(),
        second_sha
    );
    assert_eq!(
        std::fs::read_to_string(clone1.join("second.txt")).unwrap(),
        "second commit\n"
    );
    let state = gw_a.git_state(&key, "demo").await.unwrap();
    let packs = state.manifest.matches(".pack\"").count();
    assert_eq!(packs, 2, "one pack per push: {}", state.manifest);

    // 6. The owner's second checkout (fresh clone) pushes on top.
    let work2 = root.join("work2");
    {
        let (o, root2, rem, w2) = (owner.clone(), root.clone(), remote.clone(), work2.clone());
        blocking(move || {
            o.git(&root2, &["clone", "-q", &rem, w2.to_str().unwrap()]);
            std::fs::write(w2.join("third.txt"), "from the second machine\n").unwrap();
            o.git(&w2, &["add", "."]);
            o.git(&w2, &["commit", "-q", "-m", "third"]);
            o.git(&w2, &["push", "-q", "origin", "main"]);
        })
        .await;
    }

    // 7. The first checkout is now behind: a divergent push is refused...
    {
        let (o, w) = (owner.clone(), work.clone());
        blocking(move || {
            std::fs::write(w.join("diverge.txt"), "diverged\n").unwrap();
            o.git(&w, &["add", "."]);
            o.git(&w, &["commit", "-q", "-m", "diverge"]);
            let out = o.git_fails(&w, &["push", "origin", "main"]);
            assert!(
                out.contains("rejected") || out.contains("non-fast-forward"),
                "{out}"
            );
            // ...unless forced.
            o.git(&w, &["push", "-q", "--force", "origin", "main"]);
        })
        .await;
    }
    let forced_sha = owner.git(&work, &["rev-parse", "HEAD"]).trim().to_string();
    let state = gw_a.git_state(&key, "demo").await.unwrap();
    assert!(state.manifest.contains("c0mpute-git.json"));

    // 8. A stranger cannot push to the owner's repo.
    {
        let (r, c1) = (reader.clone(), clone1.clone());
        let ra = User {
            gateway: url_a.clone(),
            ..r
        };
        blocking(move || {
            std::fs::write(c1.join("evil.txt"), "nope\n").unwrap();
            ra.git(&c1, &["add", "."]);
            ra.git(&c1, &["commit", "-q", "-m", "evil"]);
            let out = ra.git_fails(&c1, &["push", "origin", "HEAD:refs/heads/main"]);
            assert!(out.contains("belongs to key"), "{out}");
        })
        .await;
    }

    // 9. An annotated tag; a fresh dumb-HTTP clone sees it and the forced head.
    {
        let (o, w) = (owner.clone(), work.clone());
        blocking(move || {
            o.git(&w, &["tag", "-a", "v1.0.0", "-m", "release"]);
            o.git(&w, &["push", "-q", "origin", "v1.0.0"]);
        })
        .await;
    }
    wait_for("gateway B to learn the tag", 30, || {
        let b = gw_b.clone();
        let a = gw_a.clone();
        let id = site_id.clone();
        async move {
            match (b.record_for(&id).await, a.record_for(&id).await) {
                (Some(rb), Some(ra)) => rb.seq == ra.seq && !b.holders(&id).await.is_empty(),
                _ => false,
            }
        }
    })
    .await;
    let clone3 = root.join("clone3");
    {
        let (r, root2, u, c3) = (
            reader.clone(),
            root.clone(),
            http_url.clone(),
            clone3.clone(),
        );
        blocking(move || {
            r.git(&root2, &["clone", "-q", &u, c3.to_str().unwrap()]);
        })
        .await;
    }
    assert_eq!(
        reader.git(&clone3, &["rev-parse", "HEAD"]).trim(),
        forced_sha
    );
    assert_eq!(
        reader
            .git(&clone3, &["rev-parse", "v1.0.0^{commit}"])
            .trim(),
        forced_sha
    );
    assert!(clone3.join("diverge.txt").exists());
    assert!(
        !clone3.join("third.txt").exists(),
        "forced push replaced the third commit"
    );

    // The gateway's repo list.
    let list: serde_json::Value = reqwest::get(format!("{url_b}/git/v1/repos"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        list["repos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["repo"] == "demo" && r["owner"] == key)
    );
    drop(hosts);
}

/// A repo in the first helper's layout (`refs.json` + one `repo.bundle`)
/// still clones, by its old `c0mpute://<site id>` URL too, and the owner's
/// next push rewrites it in the pack layout without losing history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_bundle_repo_clones_and_migrates_on_push() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let tmp = tempfile::Builder::new()
        .prefix("git-legacy")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = tmp.path().to_path_buf();
    let opts = |dir: &Path| Options {
        hosts: true,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        ..Default::default()
    };
    let (hub_net, hub_store) = node(&root.join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let mut hosts = vec![
        Hosting::start(hub_net, hub_store, opts(&root.join("h0")))
            .await
            .unwrap(),
    ];
    for i in 1..4 {
        let d = root.join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, opts(&d)).await.unwrap());
    }
    let (an, as_) = node(&root.join("gwa"), vec![hub_addr.clone()]).await;
    let gw = Hosting::start(an, as_, opts(&root.join("gwa")))
        .await
        .unwrap();
    let url = serve(&gw).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_c0mpute"),
        bin.join("git-remote-c0mpute"),
    )
    .unwrap();
    let owner = User::new(&root, "owner", &bin, &url);
    let key = owner.key();
    let kp = c0mpute_net::identity::load_or_create(&owner.home.join("data/c0mpute/buyer")).unwrap();

    // The owner's repo, published the way the first helper did.
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    owner.git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "from the bundle days\n").unwrap();
    owner.git(&work, &["add", "."]);
    owner.git(&work, &["commit", "-q", "-m", "old"]);
    let old_sha = owner.git(&work, &["rev-parse", "HEAD"]).trim().to_string();
    let site_dir = root.join("legacy-site");
    std::fs::create_dir_all(&site_dir).unwrap();
    let bundle = site_dir.join("repo.bundle").to_string_lossy().to_string();
    owner.git(&work, &["update-ref", "refs/c0mpute-push/0", &old_sha]);
    owner.git(&work, &["bundle", "create", &bundle, "refs/c0mpute-push/0"]);
    owner.git(&work, &["update-ref", "-d", "refs/c0mpute-push/0"]);
    std::fs::write(
        site_dir.join("refs.json"),
        serde_json::json!({"head": "refs/heads/main", "refs": {"refs/heads/main": old_sha}})
            .to_string(),
    )
    .unwrap();
    let name = c0mpute_core::git::site_name("legacy");
    let built = c0mpute_core::hosting::site::build_from_dir(&site_dir, &name).unwrap();
    let rec = c0mpute_core::hosting::site::SiteRecord::sign(
        &kp,
        &name,
        &built.manifest_hash(),
        1,
        built.manifest.total_bytes(),
        vec![],
    )
    .unwrap();
    let site_id = rec.site_id.clone();
    assert_eq!(site_id, c0mpute_core::git::site_id(&key, "legacy"));
    gw.publish_site(&built, rec).await.unwrap();
    wait_for("3 copies of the legacy repo", 60, || {
        let g = gw.clone();
        let id = site_id.clone();
        async move { g.holders(&id).await.len() >= 3 }
    })
    .await;

    // A reader clones it by the old site-id URL.
    let reader = User::new(&root, "reader", &bin, &url);
    let c1 = root.join("c1");
    {
        let (r, u, d, at) = (
            reader.clone(),
            format!("c0mpute://{site_id}"),
            c1.to_string_lossy().to_string(),
            root.clone(),
        );
        blocking(move || r.git(&at, &["clone", &u, &d])).await;
    }
    assert_eq!(
        std::fs::read_to_string(c1.join("README.md")).unwrap(),
        "from the bundle days\n"
    );

    // The owner pushes a new commit by the short own-key URL.
    owner.git(&work, &["remote", "add", "origin", "c0mpute://legacy"]);
    std::fs::write(work.join("new.txt"), "packs now\n").unwrap();
    owner.git(&work, &["add", "."]);
    owner.git(&work, &["commit", "-q", "-m", "new"]);
    let new_sha = owner.git(&work, &["rev-parse", "HEAD"]).trim().to_string();
    {
        let (o, w) = (owner.clone(), work.clone());
        let out = blocking(move || o.git(&w, &["push", "origin", "main"])).await;
        assert!(out.contains("main -> main"), "{out}");
    }
    let state = gw.git_state(&key, "legacy").await.expect("repo state");
    assert!(
        state.manifest.contains(c0mpute_core::git::DOC_PATH),
        "{}",
        state.manifest
    );
    assert!(
        !state.manifest.contains("repo.bundle"),
        "bundle dropped: {}",
        state.manifest
    );

    // A fresh clone has the whole history from the pack layout.
    let c2 = root.join("c2");
    {
        let (r, u, d, at) = (
            reader.clone(),
            format!("c0mpute://{key}/legacy"),
            c2.to_string_lossy().to_string(),
            root.clone(),
        );
        blocking(move || r.git(&at, &["clone", &u, &d])).await;
    }
    assert_eq!(reader.git(&c2, &["rev-parse", "HEAD"]).trim(), new_sha);
    assert_eq!(reader.git(&c2, &["rev-parse", "HEAD~1"]).trim(), old_sha);
    drop(hosts);
}
