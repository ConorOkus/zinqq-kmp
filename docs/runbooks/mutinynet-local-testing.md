# Local testing on Mutinynet

How to run zinqq against [Mutinynet](https://www.nobsbitcoin.com/mutinynet/) — a
custom signet with 30-second blocks — so wallet changes can be exercised without
risking real funds.

Plan: `docs/plans/2026-07-31-001-feat-mutinynet-network-support-plan.md`.

## What you get

| | Debug build | Release / TestFlight |
|---|---|---|
| Network | Mutinynet (signet) | mainnet, always |
| Android app id | `zinqq.app.debug` | `zinqq.app` |
| iOS bundle id | `zinqq.ios.debug` | `zinqq.ios` |
| Override | `-Pzinqq.network=mainnet` | none — the property is not read |

The network is chosen **at build time**. There is deliberately no runtime
switcher: a wallet holding real funds must not let anyone flip networks with
live channels open.

Because debug and release use different app ids, **both install side by side**.
Your real wallet stays where it is while the test wallet runs next to it.

## Isolation

Three independent mechanisms keep signet state away from mainnet state, plus a
fourth that was already there:

1. **App id** — separate installs, separate OS-level data containers.
2. **Storage directory** — Mutinynet uses a `mutinynet/` subtree. Mainnet keeps
   the bare path it has always used, so existing installs still find their data.
3. **VSS store id** — non-mainnet networks mix their name into the store-id
   hash. Without this the id would be *identical* across networks: BIP32
   master-key bytes do not vary by network, so the same mnemonic would otherwise
   point both wallets at the same cloud store.
4. **Genesis probe** — a backend serving the wrong chain fails the start hard.

Key *derivation* is untouched: the same mnemonic yields the same `ldk_seed` on
every network. The store id is a lookup key; the seed is the wallet. Changing
the seed would alter every existing mainnet wallet's identity.

In practice each install generates its own mnemonic on first launch, so the two
wallets never share a seed anyway — the above is defence in depth.

## Running it

### Android

```bash
./gradlew :androidApp:installDebug          # Mutinynet
./gradlew :androidApp:installDebug -Pzinqq.network=mainnet   # mainnet, for
                                            # reproducing a production bug
```

Confirm which network a build got:

```bash
grep WALLET_NETWORK androidApp/build/generated/source/buildConfig/debug/zinqq/app/BuildConfig.java
```

### iOS

Debug is Mutinynet; run the `iosApp` scheme as usual. To check what a build
resolved, read `WalletNetwork` from the built app's `Info.plist`.

There is no iOS equivalent of the Gradle property — to run Debug against
mainnet, change `WALLET_NETWORK` under `configs: Debug` in
`iosApp/project.yml`, regenerate with `xcodegen generate`, and revert when done.

## Getting funds

1. Launch the app; it generates a fresh wallet on first start.
2. Copy the on-chain receive address (it will be a signet `tb1…` address).
3. Fund it from <https://faucet.mutinynet.com/>.
4. Blocks are 30 seconds, so confirmation is quick.

## Getting inbound liquidity

The faucet also opens channels. Alternatively, connect and open manually from
Settings → Channels using Megalith's Mutinynet node:

```
03e30fda71887a916ef5548a4d02b06fe04aaa1a8de9e24134ce7f139cf79d7579@64.23.192.68:9736
```

> **LSPS2 on Mutinynet is unconfirmed.** zinqq's *receive* flow uses LSPS2 JIT
> channels, and Megalith documents LSPS1 for Mutinynet while its LSPS2 page 404s.
> If JIT quoting fails there, that is why — open a channel manually instead. The
> app's manual connect/open path works regardless. If you confirm it either way,
> record the answer here.

## Testing async payments

This is what the network support was built for: proving the async payments
receive path from
`docs/plans/2026-07-30-001-feat-async-payments-plan.md`, which cannot be
exercised safely on mainnet.

1. Stand up an ldk-node static invoice server on Mutinynet and mint the
   recipient paths — see
   [the static invoice server runbook](async-payments-static-invoice-server.md).
2. Configure the wallet with those paths.
3. Pay the resulting offer from a second ldk-node while the wallet is closed.

Note the two halves are independently provable: two ldk-node instances on
Mutinynet validate the protocol itself with no zinqq involved, which is worth
doing first.

## Services

| Service | Endpoint |
|---|---|
| Esplora | `https://mutinynet.com/api` |
| Rapid Gossip Sync | `https://rgs.mutinynet.com/snapshot` |
| Explorer | `https://mutinynet.com` |
| Faucet | `https://faucet.mutinynet.com/` |
| Genesis | `00000008819873e925422c1ff0f99f7cc9bbb232af63a077a480a3633bee1ef6` |

All of these live in `rust/src/config.rs` under `mod mutinynet`. Adding a third
network means adding a sibling module there, not editing call sites.

VSS is **not** separated by endpoint — Mutinynet uses the same proxy with a
namespaced store id. If that ever becomes undesirable, point non-mainnet at a
different `vss_url`.
