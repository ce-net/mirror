//! mirror — push a local folder to a remote machine over the CE mesh.
//!
//! This is an application built *on top of* CE, not part of it. CE is the infrastructure: it owns
//! the file-transport primitive ([`ce_rs::CeClient::mesh_sync_file`] → `PUT /mesh-sync`), which
//! signs the write and routes it over libp2p so it traverses the target's NAT. `mirror` owns the
//! policy a folder-sync tool needs: walking the tree, ignoring build/VCS noise, mapping local
//! paths onto remote ones, and watching for changes.
//!
//! Workflow it exists for: your laptop's disk is full, so you edit source on the laptop and build
//! and test on a beefier desktop. `mirror watch` keeps the desktop's copy of your source current;
//! you run the actual compile there with `ce exec <desktop> -- cargo build` (or an SSH session).
//!
//! Direction is one-way (laptop -> desktop) and additive: created/modified files are pushed;
//! local deletions are *not* propagated (the mesh transport has no remote-delete primitive yet).
//! Stale files on the remote rarely break a build; clean them by hand if needed.
//!
//! Prerequisites (one-time):
//!   - A CE node running locally (`ce start`) — mirror talks to its HTTP API.
//!   - The target machine's node running and reachable through the relay.
//!   - The target must trust this node for sync. On the target: `ce devices add <name> <this-node-id>`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use ce_rs::CeClient;
use notify::{RecursiveMode, Watcher};
use serde::Deserialize;
use walkdir::WalkDir;

/// Directory/file name segments never synced. `target` (Rust build output) and `.git` are the big
/// ones — they're huge, machine-specific, and regenerated remotely. Pruned as whole directories.
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".DS_Store"];

#[derive(Parser)]
#[command(name = "mirror", version, about = "Mirror a local folder to a remote machine over the CE mesh")]
struct Cli {
    /// Local CE node API URL. Overrides config; defaults to config's `node.url`, else http://127.0.0.1:8844.
    #[arg(long, global = true)]
    node: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot: push every (non-ignored) file under <dir> to the target, then exit.
    Push {
        /// Local directory to push.
        dir: PathBuf,
        /// Destination as `<node-id-or-alias>:<remote-dir>` (remote-dir is relative to the target's home).
        target: String,
        /// Relay circuit multiaddr dial hint (overrides the alias's configured hint).
        #[arg(long)]
        hint: Option<String>,
    },
    /// Continuous: full sync once, then watch <dir> and push each change as it happens.
    Watch {
        /// Local directory to watch.
        dir: PathBuf,
        /// Destination as `<node-id-or-alias>:<remote-dir>`.
        target: String,
        /// Relay circuit multiaddr dial hint (overrides the alias's configured hint).
        #[arg(long)]
        hint: Option<String>,
    },
    /// Write an example config (aliases + node URL) to the config path.
    Init,
}

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    node: NodeCfg,
    #[serde(default)]
    alias: std::collections::HashMap<String, Alias>,
}

#[derive(Deserialize)]
struct NodeCfg {
    url: String,
}

impl Default for NodeCfg {
    fn default() -> Self {
        NodeCfg { url: ce_rs::DEFAULT_BASE_URL.to_string() }
    }
}

#[derive(Deserialize, Clone)]
struct Alias {
    node_id: String,
    #[serde(default)]
    hint: Option<String>,
}

/// A fully resolved destination.
struct Target {
    node_id: String,
    hint: Option<String>,
    /// Remote directory, normalised to be relative to the target's home (no leading `~/` or `/`).
    remote_root: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Cmd::Init = cli.cmd {
        return write_example_config();
    }

    let cfg = load_config()?;
    let node_url = cli.node.clone().unwrap_or_else(|| cfg.node.url.clone());
    let client = CeClient::new(node_url.clone());

    // Fail fast with a clear message if the local node isn't up — every push goes through it.
    if !client.health().await.unwrap_or(false) {
        return Err(anyhow!(
            "local CE node not reachable at {node_url} — is `ce start` running? \
             (set the right port with --node http://127.0.0.1:<port> or in the config)"
        ));
    }

    match cli.cmd {
        Cmd::Push { dir, target, hint } => {
            let t = resolve_target(&target, hint, &cfg)?;
            let root = canonical(&dir)?;
            let sent = full_sync(&client, &t, &root).await?;
            println!("pushed {sent} files to {}:{}", short(&t.node_id), t.remote_root);
        }
        Cmd::Watch { dir, target, hint } => {
            let t = resolve_target(&target, hint, &cfg)?;
            let root = canonical(&dir)?;
            let sent = full_sync(&client, &t, &root).await?;
            println!("initial sync: {sent} files");
            watch(&client, &t, &root).await?;
        }
        Cmd::Init => unreachable!("handled above"),
    }
    Ok(())
}

/// Walk `root` and push every non-ignored file. Build-output and VCS directories are pruned by
/// name before we descend, so `target/` is never even traversed. Returns the count of files sent.
async fn full_sync(client: &CeClient, t: &Target, root: &Path) -> Result<usize> {
    let mut sent = 0usize;

    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        // depth 0 is `root` itself (keep it regardless of its name); prune ignored entries otherwise.
        .filter_entry(|e| e.depth() == 0 || !skip_name(e.file_name()));

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let bytes = std::fs::read(entry.path()).with_context(|| format!("read {}", rel.display()))?;
        let remote = remote_path(&t.remote_root, rel);
        client
            .mesh_sync_file(&t.node_id, &remote, bytes, t.hint.as_deref())
            .await
            .with_context(|| format!("push {} to {}", rel.display(), short(&t.node_id)))?;
        sent += 1;
    }

    Ok(sent)
}

/// Full sync already done by the caller; here we watch for changes and push them as they land.
/// Events are debounced into a short window so one save (which fires several FS events) is one push.
async fn watch(client: &CeClient, t: &Target, root: &Path) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive).with_context(|| format!("watch {}", root.display()))?;

    println!(
        "watching {} -> {}:{}  (Ctrl-C to stop; deletions are not propagated)",
        root.display(),
        short(&t.node_id),
        t.remote_root
    );

    loop {
        let first = tokio::select! {
            ev = rx.recv() => match ev { Some(e) => e, None => break },
            _ = tokio::signal::ctrl_c() => { println!("\nstopped"); break; }
        };

        // Coalesce a burst of events (editor writes touch a file several times) into one set.
        let mut paths: HashSet<PathBuf> = first.paths.into_iter().collect();
        loop {
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Some(ev)) => paths.extend(ev.paths),
                _ => break,
            }
        }

        for p in paths {
            let rel = match p.strip_prefix(root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if rel.as_os_str().is_empty() || rel_is_ignored(rel) {
                continue;
            }
            if p.is_file() {
                match std::fs::read(&p) {
                    Ok(bytes) => {
                        let remote = remote_path(&t.remote_root, rel);
                        match client.mesh_sync_file(&t.node_id, &remote, bytes, t.hint.as_deref()).await {
                            Ok(()) => println!("  synced {}", rel.display()),
                            Err(e) => eprintln!("  WARN {}: {e}", rel.display()),
                        }
                    }
                    // File vanished between the event and the read — nothing to push.
                    Err(_) => {}
                }
            } else if !p.exists() {
                eprintln!("  note: {} removed locally (not propagated)", rel.display());
            }
        }
    }
    Ok(())
}

/// Resolve `<node-id-or-alias>:<remote-dir>` against the config. A 64-hex left side is used
/// verbatim; otherwise it's looked up as an alias.
fn resolve_target(spec: &str, cli_hint: Option<String>, cfg: &Config) -> Result<Target> {
    let (left, right) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("target must be <node-id-or-alias>:<remote-dir>, e.g. desktop:ce-net/ce"))?;

    let (node_id, hint) = if is_hex64(left) {
        (left.to_string(), cli_hint)
    } else if let Some(a) = cfg.alias.get(left) {
        (a.node_id.clone(), cli_hint.or_else(|| a.hint.clone()))
    } else {
        return Err(anyhow!(
            "unknown alias '{left}'. Add it under [alias.{left}] in {}, or pass a 64-hex node id.",
            config_path()?.display()
        ));
    };

    Ok(Target { node_id, hint, remote_root: normalize_remote(right) })
}

/// Build the remote path for `rel`, joined under the (home-relative) remote root, using `/` always.
fn remote_path(remote_root: &str, rel: &Path) -> String {
    let rel_str = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if remote_root.is_empty() {
        rel_str
    } else {
        format!("{remote_root}/{rel_str}")
    }
}

/// Normalise a remote dir to be relative to the target's home (the mesh-sync endpoint joins it
/// under `~/`): strip a leading `~/`, `~`, or `/`, and any trailing `/`.
fn normalize_remote(s: &str) -> String {
    s.trim_start_matches("~/")
        .trim_start_matches('~')
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
}

/// True if any component of `rel` is a skipped dir, or the file name is editor/OS noise.
fn rel_is_ignored(rel: &Path) -> bool {
    rel.components().any(|c| skip_name(c.as_os_str()))
}

/// True if a single path segment should be skipped: a build/VCS dir, or an editor/OS temp file.
fn skip_name(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    if SKIP_DIRS.contains(&n.as_ref()) {
        return true;
    }
    n.ends_with('~')
        || n.ends_with(".swp")
        || n.ends_with(".swo")
        || n.ends_with(".tmp")
        || n.starts_with(".#")
        || n == ".DS_Store"
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn short(node_id: &str) -> &str {
    &node_id[..node_id.len().min(8)]
}

fn canonical(dir: &Path) -> Result<PathBuf> {
    dir.canonicalize().with_context(|| format!("no such directory: {}", dir.display()))
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("cannot locate a config directory"))?;
    Ok(dir.join("mirror").join("config.toml"))
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parse {}", path.display())),
        Err(_) => Ok(Config::default()),
    }
}

fn write_example_config() -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        println!("config already exists: {}", path.display());
        return Ok(());
    }
    std::fs::write(&path, EXAMPLE_CONFIG)?;
    println!("wrote example config: {}", path.display());
    println!("edit the [alias.*] node ids, then: mirror watch <dir> desktop:<remote-dir>");
    Ok(())
}

/// Example config. `node.url` points at the local CE node API; edit the aliases for your machines.
const EXAMPLE_CONFIG: &str = r#"# mirror config

[node]
# Local CE node HTTP API. The default node port is 8844; this laptop's node currently runs on 8080.
url = "http://127.0.0.1:8080"

# Aliases let you write `mirror watch ./ce desktop:ce-net/ce` instead of pasting a 64-hex node id.
# `hint` is an optional relay circuit multiaddr that speeds up the first dial to a NAT'd peer.
[alias.desktop]
node_id = "25df8f15853855c4cd2c5769cbc9789bf156534356ffead3b67c2c395f6d8ac1"
hint = "/ip4/178.105.145.170/tcp/4001/p2p/12D3KooWC6vyMMrtmdWEdpcMx7JZ4Ze5scUhA6BbMdYqnUDC7nr7/p2p-circuit/p2p/12D3KooWCNCyEFHAGE2z4ZhpP6ApeqFXY7cRLxJqVTWvYCBfrWmn"
"#;
