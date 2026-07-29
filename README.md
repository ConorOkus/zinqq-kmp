# zinqq-kmp

The native Kotlin Multiplatform client for the Zinqq Lightning wallet: a Rust core built directly on the LDK crates (`lightning 0.2.4`, `lightning-liquidity`, `lightning-transaction-sync`, `bdk_wallet`, `vss-client-ng`), exposed via UniFFI/Gobley into shared `commonMain` Kotlin, with native Compose (Android) and SwiftUI (iOS) shells.

This is the **production native client** for Zinqq — a mainnet Lightning wallet handling real funds, on a path to real distribution (internal TestFlight today; App Store pending organization enrollment). It has **full feature parity with the Zinqq web PWA** (the sibling `zinq` repo): the same 16 screens, every shipped capability, and the same architecture — including VSS encrypted cloud backup that is wire-compatible with the PWA, so one seed restores on either client. The iOS bundle ID is `zinqq.ios` and shared Kotlin code lives under `zinqq.main.*`; the original payment spike this grew from survives only in the plan history.

- Plans: `docs/plans/2026-07-26-001-feat-pwa-feature-parity-plan.md` (parity), `docs/plans/2026-07-28-001-feat-testflight-distribution-plan.md` (distribution), `docs/plans/2026-07-25-001-feat-kmp-native-payment-spike-plan.md` (original spike)
- The Zinqq web PWA remains a maintained client; the two clients share protocols, formats, and infrastructure — never code.

## What it does

- **Unified send** — one input classifies BIP321 URIs, BOLT11 (including amountless with amount entry), BOLT12 offers, BIP353 names (DNSSEC-verified over DoH), LNURL-pay, and on-chain addresses.
- **Unified receive** — one QR combining an on-chain address and BOLT11 invoice (BIP321), a reusable BOLT12 offer page, and LSPS2 just-in-time inbound channels from Megalith with a live fee floor and quote review.
- **VSS encrypted cloud backup** — channel monitors, channel manager, known peers, close records, and recovery state dual-written VSS-first with client-side ChaCha20-Poly1305 encryption and HMAC key obfuscation, byte-compatible with the PWA's scheme. Restore from the 12-word seed alone, on either client.
- **On-chain wallet** — send with a 10,000-sat anchor reserve while channels exist, send-max, fee guards, and a review-to-broadcast drift guard.
- **Force-close pipeline** — close records with chain-truth reconciliation, a recovery flow with deposit calculation, anchor CPFP fee-bumping, and a sweep engine with a subsidized near-dust rescue.
- **Channel management** — connect/forget peers, open (20k–16.77M sats) and close (cooperative or force) channels with informational estimates.
- **Payment history** — persisted rows merged with on-chain transactions and channel closes into one activity feed.
- **QR scanning** — CameraX/MLKit on Android, VisionKit (with AVCapture fallback) on iOS.

Single-active-client rule: **never run two clients on one seed at the same time.** The VSS layer detects a concurrent writer via versioned-write conflicts and fences the losing client (it halts and offers wipe-and-restore). This is collision detection, not prevention — stop the other client before restoring a shared seed.

## Layout

```text
rust/        wallet-core crate: LDK node, VSS store, engines (send/receive/
             onchain/channels/close-records/recovery/sweep), UniFFI exports
shared/      KMP module (Gobley generates uniffi.wallet_core bindings) +
             pure helpers (BIP177 formatting, numpad reducer)
androidApp/  Compose shell: 16 screens, three appearance modes
iosApp/      SwiftUI shell: the same 16 screens (XcodeGen project.yml)
```

## Prerequisites

- Rust (stable) with mobile targets: `rustup target add aarch64-linux-android x86_64-linux-android aarch64-apple-ios aarch64-apple-ios-sim`
- JDK **21** (newer JDKs break the Android Gradle Plugin)
- Android SDK 35 with **NDK r28+** (16 KB page alignment) and an emulator image; point `local.properties` at it via `sdk.dir=`
- Xcode 16+ (full install) and [XcodeGen](https://github.com/yonaskolb/XcodeGen) for `iosApp`
- The Gradle wrapper is committed, so `./gradlew` needs no separate Gradle install

## Build and test

Rust core (host, no mobile toolchain needed):

```bash
cd rust
cargo test                    # full offline suite
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

Live-network tests (`#[ignore]`d; talk to mainnet services):

```bash
cargo test --lib -- --ignored live_vss_roundtrip           # VSS wire compat
cargo test --lib -- --ignored live_megalith_get_info       # LSPS2 fee menu
cargo test --lib -- --ignored live_megalith_receive_jit    # full JIT quote+buy
cargo test --lib -- --ignored live_lightning_address_resolution
```

Android:

```bash
./gradlew :shared:jvmTest                   # bindings smoke test across the FFI
./gradlew :androidApp:testDebugUnitTest     # screen logic/presentation tests
./gradlew :androidApp:assembleDebug
./gradlew :androidApp:installDebug          # device or emulator
```

iOS:

```bash
cd iosApp && xcodegen generate
xcodebuild -project iosApp.xcodeproj -scheme iosApp \
  -destination 'platform=iOS Simulator,name=iPhone 17' \
  ARCHS=arm64 CODE_SIGNING_ALLOWED=NO test   # build + XCTest suites
```

## Distribution

iOS ships to **internal TestFlight testers** via manual Xcode Organizer uploads — the full procedure (first-time setup, routine uploads, pre-upload sanity check, and the real-funds constraints like the 90-day build expiry) lives in `docs/runbooks/testflight-upload.md`. The `ios-release-device` CI job compiles the same Rust-release + Kotlin/Native-release device chain an archive uses, so archive-toolchain breakage surfaces in CI, not mid-upload. External beta and App Store release wait on organization enrollment (Apple requires it for wallet apps). Android distribution is not set up yet.

## Configuration

Defaults live in `rust/src/config.rs` and are overridable through the FFI `WalletConfig`:

- **Esplora**: `https://zinqq.app/api/esplora` (the PWA's proxy fronting Blockstream Enterprise; keeps credentials server-side). Fallbacks: `blockstream.info`, `mempool.space`.
- **VSS**: `https://zinqq.app/api/vss-proxy` (pass-through to the VSS origin; the proxy adds no trust). `vss_disabled` runs fully local.
- **LSP**: Megalith (`034066e2…1453b0@64.23.159.177:9735`), sourced from the PWA's working configuration — public explorer listings name the wrong node.
- **RGS**: `https://rapidsync.lightningdevkit.org/snapshot`. Explorer links: `https://mempool.space`.
- Mainnet only, enforced by a genesis-hash check at startup.

## Cross-client acceptance protocol (manual — U23 in the plan)

Amounts stay small (< $20 total). Record payment hashes only — never seeds or preimages. The PWA side runs the **pinned 2026-07-26 commit built locally**.

1. **AE1 — node identity**: initialize the same test mnemonic in the PWA (dev) and this app; the node IDs must be identical.
2. **AE2 — cross-client restore**: create + fund a small wallet on the PWA, let it back up to VSS, **stop the PWA**, restore on native from the seed; balances and channel state must match, then send a payment.
3. **AE3 — native restore**: wipe and reinstall the native app, restore from seed; monitors, manager, and peers rebuild from VSS.
4. **JIT receive + send** on both platforms through the full UI (payer on a separate device; the node is foreground-only).
5. **Force-close drill**: force-close a small channel, verify CPFP/recovery/sweep behavior and fee sanity.
6. **Collision drill** (throwaway wallet): run both clients on one seed deliberately; the losing writer must fence (durable flag, halt, zero further puts) and recover via restore-take-over.

### Results

_Not yet run._

| Date | Step | Platform | Outcome | Payment hashes | Notes |
|---|---|---|---|---|---|
