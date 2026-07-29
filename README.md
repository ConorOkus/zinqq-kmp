# zinqq-kmp

The native Kotlin Multiplatform client for the Zinqq Lightning wallet: a Rust core built directly on the LDK crates (`lightning 0.2.4`, `lightning-liquidity`, `lightning-transaction-sync`, `bdk_wallet`, `vss-client-ng`), exposed via UniFFI/Gobley into shared `commonMain` Kotlin, with native Compose (Android) and SwiftUI (iOS) shells.

This is the **production native client** for Zinqq — a mainnet Lightning wallet handling real funds, with 16 screens and VSS encrypted cloud backup so one seed restores the whole wallet. The iOS bundle ID is `zinqq.ios` and shared Kotlin code lives under `zinqq.main.*`.

## What it does

- **Unified send** — one input classifies BIP321 URIs, BOLT11 (including amountless with amount entry), BOLT12 offers, BIP353 names (DNSSEC-verified over DoH), LNURL-pay, and on-chain addresses.
- **Unified receive** — one QR combining an on-chain address and BOLT11 invoice (BIP321), a reusable BOLT12 offer page, and LSPS2 just-in-time inbound channels from Megalith with a live fee floor and quote review.
- **VSS encrypted cloud backup** — channel monitors, channel manager, known peers, close records, and recovery state dual-written VSS-first with client-side ChaCha20-Poly1305 encryption and HMAC key obfuscation. Restore from the 12-word seed alone.
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

## Configuration

Defaults live in `rust/src/config.rs` and are overridable through the FFI `WalletConfig`:

- **Esplora**: `https://zinqq.app/api/esplora` (proxy fronting Blockstream Enterprise; keeps credentials server-side). Fallbacks: `blockstream.info`, `mempool.space`.
- **VSS**: `https://zinqq.app/api/vss-proxy` (pass-through to the VSS origin; the proxy adds no trust). `vss_disabled` runs fully local.
- **LSP**: Megalith (`034066e2…1453b0@64.23.159.177:9735`) — use this address, not the public explorer listings, which name the wrong node.
- **RGS**: `https://rapidsync.lightningdevkit.org/snapshot`. Explorer links: `https://mempool.space`.
- Mainnet only, enforced by a genesis-hash check at startup.
