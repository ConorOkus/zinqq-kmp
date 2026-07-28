---
module: rust/wallet-core
date: 2026-07-27
problem_type: integration_issue
component: chain-access
severity: critical
applies_when:
  - A chain backend is reached through a proxy or serverless function rather than directly
  - An SDK helper is chosen without checking which HTTP endpoint it actually calls
  - Binary-bodied HTTP endpoints are consumed (Esplora `/raw`, block bodies, merkle proofs)
  - A dependency's internal calls share a backend with first-party code
related_components:
  - chain-access
  - vss
  - sweep
  - restore
tags:
  - esplora
  - proxy
  - binary-encoding
  - utf8
  - ldk
  - lightning-transaction-sync
  - bdk
  - vercel
  - fund-safety
---

# A proxy that decodes bodies as text silently destroys every binary chain endpoint

## Context

A wallet restored from a seed created by the sibling PWA recovered its Lightning
state but left 11,500 sats of force-close funds unswept. The pass built to
recover them found its close record correctly, then logged:

```
Replay: tx 4572e68e… skipped: esplora unreachable: BitcoinEncoding(Io(UnexpectedEof))
```

The bug was not in the wallet. The first-party Esplora proxy — a Vercel function
holding OAuth credentials server-side — returned every upstream response through
`await upstream.text()`. That decodes the body as UTF-8, so on a **binary** body
every byte that is not valid UTF-8 becomes U+FFFD (`EF BF BD`) and is then
re-encoded on the way out. The corruption is lossy and irreversible.

Measured against a direct backend as the control, same paths:

| Endpoint | Upstream | Via proxy | |
|---|---|---|---|
| `/tx/:txid/raw` (binary) | 443 B | **773 B** | corrupt |
| `/block/:hash/raw` (binary) | 1,506,616 B | **2,565,784 B** | corrupt |
| `/tx/:txid/hex` (ASCII) | 886 B | 886 B | intact |

The ASCII row is the whole diagnosis: text bodies survive, binary bodies do not.

## Root Cause

Two independent mistakes had to line up.

**1. The proxy assumed every body was text.** One line — `new Response(await
upstream.text(), …)` — with the upstream `Content-Type` faithfully passed
through, so callers were told they were receiving `application/octet-stream`
while the bytes had already been destroyed.

**2. Nothing on the consuming side knew which endpoint it was using.**
`esplora_client::get_tx` reads `/tx/:txid/raw`. That is entirely reasonable and
invisible at the call site: the code says "get me this transaction," not "fetch a
binary body from this specific route." Picking the obvious SDK helper silently
picked the one endpoint this backend breaks.

The sibling PWA was unaffected and had been for months, because its JavaScript
stack reads the JSON and hex endpoints. The corruption was therefore invisible to
the entire existing test suite of the client that owns the proxy.

## Impact

Worse than a failed fetch, because the failures were silent and spread through
code that had nothing to do with HTTP.

- **`lightning-transaction-sync` 0.2.1 calls the same `get_tx` internally**
  (`src/esplora.rs:378`) in its confirmation path. Every sync that needed a
  transaction body aborted wholesale: `Failed during transaction sync, aborting.
  Synced so far: 0 confirmed, 0 unconfirmed.` LDK's chain sync had therefore
  **never worked** against this backend — meaning monitors could miss
  confirmations, closes, and maturing outputs. That is a fund-safety property,
  not a degraded-UX one.
- **Force-close recovery could not read a commitment transaction**, leaving
  claimable funds unswept with no user-visible error.
- **A sweep sentinel read every known transaction as unknown.**
  `tx_known_to_chain` used the same helper, so the subsidized sweep's
  `AlreadyKnown` confirmation turned into a permanent broadcast-ambiguous
  failure. This one survived the first round of fixes and was caught later by
  review — see "Why it slipped through."

## Solution

**Fix the proxy** — pass bytes through instead of decoding them:

```ts
return new Response(await upstream.arrayBuffer(), { … })   // or stream upstream.body
```

Streaming is better still: it also avoids buffering a 1.5 MB block in a
serverless function.

**And stop depending on the binary endpoint.** `/tx/:txid/hex` is the same
standard Esplora API, is pure ASCII, and is immune to text-mangling by any
intermediary. Make it the primary, not a workaround:

```rust
// NOT `esplora_client::get_tx`, which reads `/tx/:txid/raw`.
let url = format!("{}/tx/{txid}/hex", self.esplora_url);
// … fetch, hex-decode, then:
if tx.compute_txid() != *txid {
    return Err(ChainError::EsploraUnreachable(/* … */));
}
```

The `compute_txid()` check is the important half. It costs one hash and it means
a backend returning the wrong or mangled body can never feed a foreign
transaction into monitor replay. Hex costs 2x the bytes of a small body, which is
nothing next to removing a whole class of proxy-transparency assumptions.

## Prevention

- **Prefer text-encoded endpoints across any intermediary you do not control.**
  Binary correctness depends on every hop being byte-transparent, which is an
  assumption you cannot see at the call site and cannot test from the client that
  works.
- **Know which route your SDK helper calls.** "Get me the transaction" is not a
  specification. When a helper wraps an HTTP path, the path is part of your
  dependency surface.
- **Add a byte-identity test to any proxy.** Assert the proxied body length
  equals upstream's, or that hex-encoding `/raw` equals `/hex`. This corruption
  is invisible to every JSON and hex consumer, so a client-side suite will never
  catch a reintroduction. That test is the durable fix; the one-line change is
  not.
- **Verify a fix reaches every call site, not just the one that failed.** Grep
  for the helper, not for the symptom.
- **Treat "a dependency shares my backend" as part of the blast radius.** The
  most serious consequence here was inside `lightning-transaction-sync`, which
  could not be patched from this crate at all — only fixing the proxy restored
  it.

## Why It Slipped Through

The first fix changed `transaction_by_txid` and missed `tx_known_to_chain`, which
used the same helper four hundred lines away. The commit that made the fix even
documented the corruption in a doc comment — and still left a second caller on
the broken endpoint. A later review caught it by grepping for `get_tx` rather
than re-reading the diff.

The general shape: after fixing a call site, search for the *mechanism* you just
declared unsafe. A fix that names a hazard in prose while leaving a live instance
of it is a half-fix that reads as a whole one.

## Verification

```bash
TX=4572e68e6234800e3cd1a2f72a02512090e55e2aa2ad11c7848a656080d101af
[ "$(curl -s https://zinqq.app/api/esplora/tx/$TX/raw | wc -c)" -eq 443 ] && echo PASS || echo FAIL
```

After the proxy fix, LDK's sync went from aborting every pass to completing in
204 ms – 5.6 s, and the monitors resumed processing chain data (`Transaction …
confirmed in block`, `Updating claims view at height 959879`). The recovered
sweep confirmed in block 959,872.

## Related

- `rust/src/chain.rs` — `transaction_by_txid`, `tx_known_to_chain`
- `rust/src/replay.rs` — the close-record-driven recovery pass this blocked
- Proxy issue: https://github.com/ConorOkus/zinqq/issues/185
- Sibling learning on the same subsystem:
  `zinq/docs/solutions/integration-issues/bdk-ldk-force-close-destination-script-interop.md`
