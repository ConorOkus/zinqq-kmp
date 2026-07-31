# Concepts

Shared domain vocabulary for this project — entities, named processes, and status concepts with project-specific meaning. Seeded with core domain vocabulary, then accretes as ce-compound and ce-compound-refresh process learnings; direct edits are fine. Glossary only, not a spec or catch-all.

## Wallet identity and state

### VSS Store
The remote, client-encrypted namespace holding one wallet's Lightning state — channel monitors, the channel manager, known peers, close records, recovery state — so a single seed phrase restores the whole wallet.

Addressed by a store id derived from the seed, which means the wallet finds its own backup without any account or login. That derivation is network-independent by default: BIP32 key material does not vary by chain, so a store id must be explicitly namespaced per network or two networks will share one store. Writes are versioned, which is what makes the Fence possible.

### Fence
The self-halt a client performs when it detects that another client has written this seed's VSS Store — a versioned-write conflict, meaning two clients are live on one seed.

Collision *detection*, not prevention: the losing client stops and offers a wipe-and-restore rather than trying to merge divergent channel state. Two live clients on one seed diverge on channel state, which risks a penalty transaction, so halting is the safe outcome. The fenced state is durable — it survives restart until the user resolves it.

### Build-Time Network
The Bitcoin network a build targets, fixed when the binary is produced rather than chosen at runtime.

Deliberately not a user-facing setting: a wallet holding real funds must not let anyone switch networks with live channels open. Release builds resolve to mainnet unconditionally; development builds may target a test network and carry their own storage, backup namespace, and application identity so the two never share state.

## Liquidity

### Trusted LSP Set
The node ids permitted to open zero-confirmation inbound channels to this wallet.

A set plus a predicate rather than a single hardcoded node, so an operator override does not have to be repeated in two places. Accepting a channel before confirmation is a trust decision — the funds are spendable before the funding transaction is mined — so membership is deliberately narrow and is scoped to the network the build targets.
