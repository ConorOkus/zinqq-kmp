# Async payments: standing up a static invoice server

How to exercise zinqq's async payments **receive** half end-to-end. This is a
development and integration-testing procedure — the receive path ships inert,
and nothing here applies to a shipped build.

Plan: `docs/plans/2026-07-30-001-feat-async-payments-plan.md`.
Protocol: [Async Payments: Receiving While Offline](https://lightningdevkit.org/blog/async-payments-receiving-while-offline).

## What the protocol does

An often-offline recipient cannot wake up to claim an incoming HTLC. Async
payments route around that with two always-online helpers:

- A **static invoice server** holds BOLT12 `StaticInvoice`s the recipient
  pre-signed, and serves them to payers while the recipient is offline. The
  static invoice deliberately omits the payment hash, so the server cannot
  hand out the same invoice twice and claim the second payment itself.
- The **payer's own LSP** holds the outbound HTLC with a long CLTV until the
  recipient comes online and sends a release secret over an onion message.

No party custodies funds, and only the payer's liquidity is encumbered while
the recipient sleeps.

## What zinqq ships

| Half | State | Notes |
|------|-------|-------|
| Payer (send to an offline recipient) | **On** | `hold_outbound_htlcs_at_next_hop = true` in `rust/src/config.rs` |
| Recipient (receive while offline) | **Off unless configured** | needs `static_invoice_server_paths`; empty in every shipped build |

The receive half ships off because there is nothing for it to talk to. LDK
calls the flow pre-production and LDK-to-LDK only; there is no in-band way to
fetch a server's blinded paths (`lightning-liquidity 0.2.3` has LSPS0/1/2/5 and
no path-fetch message); and Megalith, zinqq's LSP, does not run a static
invoice server.

### Payer-side caveat

`hold_outbound_htlcs_at_next_hop` only engages against a live channel
counterparty that advertises the `htlc_hold` feature. **Megalith does not
advertise it today** — its `Init` message reports `HtlcHold: not supported`.
When no counterparty supports it, LDK falls back to `enqueue_held_htlc_available`
and the wallet stays online waiting for `ReleaseHeldHtlc`, which is exactly the
pre-existing behavior. Nothing breaks; the capability simply lies dormant until
an LSP that supports holding is in the path.

To re-check, watch a peer's `Init` line in the node log for `HtlcHold`.

## Standing up a server

The server is a separate always-online LDK node — **not** this wallet. It needs
`enable_htlc_hold = false` (that flag is about holding HTLCs for *others*, a
different role) and it must handle two events:

- `Event::PersistStaticInvoice` — a recipient asked you to store a static
  invoice. Persist it keyed by `recipient_id` + `invoice_slot`, then confirm.
  A repeat of the same slot replaces the stored invoice.
- `Event::StaticInvoiceRequested` — a payer asked for that recipient's
  invoice. Serve the persisted one back over the stored invoice request path.

### 1. Mint the recipient's paths

On the server, once per recipient:

```rust
let paths = channel_manager.blinded_paths_for_async_recipient(
    recipient_id,      // must uniquely identify this recipient
    relative_expiry,   // None = never expires
)?;
```

`recipient_id` is what comes back on `Event::PersistStaticInvoice`, so pick
something you can map to an account.

### 2. Hand the paths over, hex-encoded

Serialize each path with LDK's own `Writeable` and hex it. zinqq decodes with
LDK's `Readable`, so the two sides cannot drift:

```rust
use bitcoin::hex::DisplayHex as _;
use lightning::util::ser::Writeable as _;

for path in &paths {
    println!("{}", path.encode().to_lower_hex_string());
}
```

Transport is out-of-band and up to you — LDK defines no protocol for it.

> **These paths are a trust boundary.** They name who serves invoices on the
> user's behalf and who receives their `ServeStaticInvoice` messages. A hostile
> path is a denial and a privacy leak (it cannot steal funds — the invoice is
> signed by the recipient and the payment terminates at their node). This is
> why the field is settable only at wallet construction from the app's own
> build config, with no user-facing or remotely-fetched surface. **Any future
> in-band acquisition must authenticate the server first.**

### 3. Configure the wallet

Pass the hex strings when constructing the wallet:

```kotlin
WalletConfig(
    storageDir = dir,
    staticInvoiceServerPaths = listOf("0002a1b2…", "0002c3d4…"),
)
```

Malformed input fails construction with `InvalidConfig` naming the offending
entry — it is never silently dropped.

### 4. Watch it converge

`Wallet.asyncReceive()` returns the status and the offer together:

| Status | `offer` | Meaning |
|--------|---------|---------|
| `DISABLED` | `null` | No paths configured. Every shipped build sits here, and the core returns without touching LDK. |
| `AWAITING_SERVER` | `null` | Paths configured; LDK has not finished the offer/invoice handshake (or the node is stopped). |
| `READY` | the offer | An offer exists and is payable while the wallet is offline. |

> **Call it once per visit.** `ChannelManager::get_async_receive_offer` is a
> *mutating* read: it marks the freshest unused offer `Used` and requests a
> `ChannelManager` persist. LDK keeps ten cached offers and hands out an unused
> one each time specifically to limit reuse, so an extra call per screen visit
> halves that rotation pool and doubles the `ServeStaticInvoice` churn against
> your server. That is why status and offer come back from one call rather than
> two getters — if you see offers rotating about twice as fast as expected,
> something is reading twice.

`AWAITING_SERVER` is the normal state for a while after start, and the normal
*terminal* state when the server is unreachable. LDK drives the handshake
(`OfferPathsRequest` → `OfferPaths` → `ServeStaticInvoice` →
`StaticInvoicePersisted`) from the background processor's timer ticks, so:

- Nothing polls or retries in zinqq — do not add a retry loop.
- The wallet needs a connected onion-message-capable peer before blinded paths
  can be built. A start with no peers converges once one connects.
- The resulting offers are cached inside the `ChannelManager` (TLV 21), so they
  survive restart and ride the existing VSS backup with no extra work.

Once `READY`, the receive screen grows a third QR page labelled
*"Experimental — payable while you're offline"*.

### 5. Prove the receive

Pay the async offer from a second LDK node while the wallet is **closed**. The
payer's HTLC locks in at their next hop. Reopen the wallet; the
`ChannelManager` — already wired as the `OnionMessenger`'s async-payments
handler in `rust/src/builder.rs` — answers `HeldHtlcAvailable` with
`ReleaseHeldHtlc`, and the payment lands as an ordinary `PaymentReceived`
event.

## Known rough edge

A payer holding HTLCs for an offline recipient sees an ordinary pending
payment, possibly for a long time — LDK deliberately declines to force-close
over an unresolved async payment for four weeks. LDK surfaces no payer-side
event distinguishing a held payment from any other pending one, so zinqq
cannot label it yet. See RK1 in the plan.
