---
module: rust/wallet-core
date: 2026-07-31
problem_type: architecture_pattern
component: config
severity: high
applies_when:
  - Adding a second Bitcoin network to a wallet that was built for exactly one
  - Deriving a per-wallet identifier (store id, namespace, account key) from BIP32 material
  - Isolating persisted state between environments that share a seed derivation
  - Reviewing a change that claims two networks cannot reach each other's data
related_components:
  - keys
  - vss
  - builder
  - config
tags:
  - bitcoin
  - bip32
  - ldk
  - vss
  - network-isolation
  - signet
  - mutinynet
  - build-time-config
---

# Per-network state isolation: what is not network-scoped by default

## Context

Adding Mutinynet (a custom signet) alongside mainnet (#14) looked like a
config change: add a sibling constants module, thread a network through, done.
`rust/src/config.rs` even documents that extension point in its own module
doc — *"adding a second network means adding a sibling module, not hunting
hardcoded values across call sites."*

The constants were the easy part. Two mechanisms in the wallet were **silently
network-blind**, and neither announced itself. Both were funds-adjacent: this
is a mainnet wallet holding real money, and a signet build reaching mainnet
state is the failure that matters. One was found by reading source during
planning; the other survived into the implementation and was caught by code
review.

Neither would have produced a compile error, and neither would have failed a
test that did not specifically look for it.

## Guidance

When adding a second network, audit these three classes before writing the
constants module.

### 1. Identifiers derived from BIP32 material do not vary by network

This is the non-obvious one. `Xpriv::new_master(network, &seed)` takes a
network argument, so it reads as though the derived material is
network-specific. It is not: the network only sets the **serialization
prefix** (`xprv` vs `tprv`). The key bytes come from HMAC-SHA512 over the
seed and are identical on every network.

In this wallet that meant `vss_store_id = hex(SHA-256(ldk_seed))`, where
`ldk_seed` is derived at a hardened path, produced the **same store id on
mainnet and signet from the same mnemonic** (`rust/src/keys.rs`,
`derive_wallet_keys`). A signet build with cloud backup enabled would have
written signet channel monitors into the mainnet wallet's remote store.

Namespace at the **identifier** layer, never in the derivation path:

```rust
// Mainnet stays byte-identical — existing wallets' cloud identity depends on it.
let vss_store_id = match network {
    Network::Bitcoin => sha256::Hash::hash(&ldk_seed).to_string(),
    other => {
        let mut namespaced = ldk_seed.to_vec();
        namespaced.extend_from_slice(other.to_string().as_bytes());
        let id = sha256::Hash::hash(&namespaced).to_string();
        namespaced.zeroize(); // the copy holds seed material
        id
    }
};
```

The store id is a lookup key; the seed is the wallet. Changing the derivation
path would alter every existing mainnet wallet's identity — including its node
id and its on-chain descriptors.

### 2. Scope network-dependent paths at construction, not at call sites

The first implementation resolved the per-network data directory in the
builder and added a `network_storage_dir()` accessor. That looked complete and
was not: **six other production sites read the raw `config.storage_dir`** —
the KV store in two places, the mnemonic file behind reveal-mnemonic, the
instance lock, the fenced-flag check, and restore.

The result was a split brain — the builder writing channel state under the
scoped path while the event queue read the unscoped one — plus a path where
the mainnet seed could surface in a signet build.

Patching six call sites would have left the trap for the seventh. The fix was
the shape: `Config::for_network` resolves the scoped path once, so
`storage_dir` is network-scoped **by construction** and every existing reader
is correct without knowing networks exist. The accessor that invited the
divergence was deleted.

The general rule: when a config value must differ per environment, resolve it
at the config boundary. An accessor beside the raw field is a second source of
truth, and the raw field is what call sites reach for.

### 3. Values you set but nobody reads

`explorer_url` was threaded through the new network's constants and looked
network-aware in the diff. It was never exposed over the FFI, and both shells
hardcoded their own explorer constant — so transaction links opened a mainnet
explorer with a signet txid.

Dead config is worse than absent config: the diff *reads* as though the
concern is handled, so the next reader has no reason to check. After adding a
network, grep each new constant for an actual consumer.

## Why This Matters

The startup genesis probe is a real guard — point a build at the wrong chain's
backend and it refuses to start — but it only covers the **chain backend**. It
says nothing about cloud storage, local storage, or UI links. Treating it as
"the network check" is how the other three classes slip through.

The failure modes here are quiet. A shared VSS store id does not error; it
writes. A split-brain storage path does not crash; it reads an empty store and
looks like a fresh wallet. Dead config does not warn; it renders a dead link.
None of these surface until someone is testing on the new network with real
state — which is exactly when trust in the isolation matters most.

## When to Apply

- Adding any network beyond the first (testnet, regtest, another signet)
- Adding any second *environment* that shares seed derivation — a staging
  backend, a parallel account namespace
- Reviewing a change that asserts two environments are isolated: check the
  identifier derivation, the config-boundary resolution, and consumer presence
  for each new value, rather than accepting the constants module as evidence

The mainnet-unchanged invariant is the thing to pin. This repo already had
cross-implementation key vectors (`rust/src/keys.rs` tests, generated from the
PWA's own code paths); they became the regression floor for free. If your
project lacks them, capture the derived values **before** the change — reading
them back from the new code proves nothing.

## Examples

**Isolation asserted through the readers, not a helper.** The helper was what
hid the split brain, so the regression test goes through the node's own path
resolution — two nodes over one base path must mint their own seeds:

```rust
let mainnet = Config::for_network(WalletNetwork::Mainnet, base.clone());
let mutiny = Config::for_network(WalletNetwork::Mutinynet, base.clone());
assert_eq!(mainnet.storage_dir, base, "mainnet keeps the base path");
assert_ne!(mainnet.storage_dir, mutiny.storage_dir);
// ...then start each node and assert the mnemonics differ.
```

**Mainnet's path must not move.** A network segment applied to mainnet would
present to an existing install as a wiped wallet, so only non-mainnet networks
get one:

```rust
pub fn storage_segment(self) -> Option<&'static str> {
    match self {
        Self::Mainnet => None,               // bare path, as it has always been
        Self::Mutinynet => Some("mutinynet"),
    }
}
```

**Runtime verification found what neither review pass did.** Launching the
Debug build on a simulator confirmed the chain end-to-end — bundle id,
network, and an actual peer handshake with the signet LSP rather than the
mainnet one. The app container also held two generations of state side by
side: bare-path files from the pre-fix build, and correctly scoped ones after.
The defect and its repair were visible on disk. When isolation is the claim,
run it and look at the filesystem — three review passes over the same diff
found strictly less than one launch did.
