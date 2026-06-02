# mirror

Mirror a local folder to a remote machine **over the CE mesh**. Edit on your laptop, build and
test on a remote box — even when both are behind NAT.

`mirror` is an application built **on top of CE**, not part of it. CE is the infrastructure: it
provides the file-transport primitive (`PUT /mesh-sync/:node_id/*path`, exposed in the `ce-rs` SDK
as `mesh_sync_file`) that signs each write and routes it over libp2p, traversing the target's NAT
via the relay. `mirror` owns the application policy: which files to send, when, what to ignore, and
how local paths map onto the remote machine.

## Why

Your laptop's disk is full and compiling Rust locally is painful. Keep editing on the laptop, but
run the heavy `cargo build` / tests on a desktop with room and a GPU. `mirror watch` keeps the
desktop's copy of your source current; you run the build there (`ce exec desktop -- cargo build`,
or an SSH session).

## How it relates to CE

- **CE provides:** the mesh, NAT traversal (relay + DCUtR), node identity/auth, and the
  `mesh_sync_file` transport. None of that lives here.
- **mirror provides:** directory walking, ignore rules (`target/`, `.git`, editor temp files),
  path mapping, an initial full sync, and a debounced filesystem watcher.

This is the same split as `swarm`: CE is the substrate, apps build on it through `ce-rs`.

## Install

```bash
cargo install --path .
# or
cargo build --release   # -> target/release/mirror
```

Build it on a machine with disk to spare. The binary runs wherever the files you edit live (i.e.
the laptop — that's where the watcher runs and pushes from).

## Prerequisites (one-time)

1. A CE node running locally: `ce start`. `mirror` talks to its HTTP API.
2. The target machine's node running and reachable through the relay.
3. The target must **trust this node** for sync. On the target machine:
   ```bash
   ce devices add laptop <this-node-id>
   ```
   `<this-node-id>` is your local node's id (`ce id` / `GET /status`).

## Configure

```bash
mirror init        # writes an example config with a `desktop` alias prefilled
```

Config lives at `~/.config/mirror/config.toml` (or your platform's config dir):

```toml
[node]
# Local CE node HTTP API. Default node port is 8844; this laptop's node currently runs on 8080.
url = "http://127.0.0.1:8080"

[alias.desktop]
node_id = "25df8f15853855c4cd2c5769cbc9789bf156534356ffead3b67c2c395f6d8ac1"
hint = "/ip4/178.105.145.170/tcp/4001/p2p/.../p2p-circuit/p2p/..."   # optional relay dial hint
```

## Use

```bash
# One-shot push
mirror push ./ce desktop:ce-net/ce

# Continuous: full sync, then watch and push changes as you save
mirror watch ./ce desktop:ce-net/ce

# Without an alias, pass a 64-hex node id directly
mirror watch ./ce 25df...ac1:ce-net/ce --hint /ip4/.../p2p-circuit/p2p/...

# Point at a node on a non-default port
mirror --node http://127.0.0.1:8080 watch ./ce desktop:ce-net/ce
```

The remote directory is **relative to the target's home** (`ce-net/ce` → `~/ce-net/ce`).

## Limitations (v0)

- **One-way** (local → remote) and **additive**: created/modified files are pushed; local
  **deletions are not propagated** (the mesh transport has no remote-delete primitive yet). Clean
  stale remote files by hand if a build needs it.
- Ignored, by name, at every level: `target`, `.git`, `node_modules`, `.DS_Store`, and editor temp
  files (`*~`, `*.swp`, `*.swo`, `*.tmp`, `.#*`). `.gitignore` is not parsed yet.
- The local CE node must be running; remote pushes fail if the target is offline or doesn't trust
  this node.
