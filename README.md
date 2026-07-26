# zinqq-kmp

Native Kotlin Multiplatform spike for the Zinqq Lightning wallet: a Rust core built directly on the LDK crates (`lightning 0.2.4`, `lightning-liquidity`, `lightning-transaction-sync`, `bdk_wallet`), exposed via UniFFI/Gobley into shared `commonMain` Kotlin, with thin native Compose (Android) and SwiftUI (iOS) shells.

Success criterion: one real mainnet payment received through a Megalith LSPS2 JIT channel and one sent, driven by the same shared core on both platforms.

- Plan: `docs/plans/2026-07-25-001-feat-kmp-native-payment-spike-plan.md`
- Zinqq web extraction this spike references: `docs/research/zinq-grounding-dossier.md`

The Zinqq web PWA remains the production client; this repo is an exploration, not a migration.

## Layout

```text
rust/        wallet-core crate: LDK node, LSPS2 client, event queue, UniFFI exports
shared/      KMP module (Gobley generates uniffi.wallet_core bindings into commonMain)
androidApp/  Compose shell (single screen: balance, receive QR, send)
iosApp/      SwiftUI shell (same shape; XcodeGen project.yml)
```

## Prerequisites

- Rust (stable) with mobile targets: `rustup target add aarch64-linux-android x86_64-linux-android aarch64-apple-ios aarch64-apple-ios-sim`
- JDK 17+, Android SDK with **NDK r28+** (16 KB page alignment), `ANDROID_HOME` set
- Xcode 16+ (full install, not CommandLineTools) and [XcodeGen](https://github.com/yonaskolb/XcodeGen) for `iosApp`
- A Gradle install to generate the wrapper once: `gradle wrapper`

## Build and run

Rust core (host, no mobile toolchain needed):

```bash
cd rust
cargo test                                  # 67 offline tests
cargo test -- --ignored live_megalith_get_info   # one live LSPS2 get_info against Megalith (network)
```

Android:

```bash
./gradlew :shared:jvmTest                   # bindings smoke test across the FFI
./gradlew :androidApp:assembleDebug
./gradlew :androidApp:installDebug          # device or emulator
```

iOS:

```bash
cd iosApp && xcodegen generate
open iosApp.xcodeproj                       # build/run the iosApp scheme on simulator or device
```

Configuration (Esplora URL, Megalith pubkey/address, RGS URL) lives in `rust/src/config.rs`. Default Esplora is `https://mempool.space/api`; the PWA's credentialed endpoint swaps in via the same config value.

## Mainnet acceptance protocol (manual — U8 in the plan)

Amounts stay under $10 equivalent. The payer must be a **separate device** — the node is foreground-only and stops when the app backgrounds. Keep the app foregrounded from invoice display until `PaymentReceived`.

1. Fresh install on Android. First launch generates a new seed (there is no import path).
2. Request an invoice for an amount comfortably above Megalith's minimum fee (read from the live fee menu; the app rejects amounts at or below it).
3. Pay from an external wallet on another device. Expect: JIT channel opens (0-conf), balance shows amount minus the skimmed opening fee.
4. Send a small payment out to an external invoice from the JIT channel balance.
5. Repeat 1–4 on iOS with zero platform Lightning-code changes.
6. Record results below (payment hashes only — never preimages or the seed; no secrets in screenshots).

### Results

_Not yet run._

| Date | Platform | Received (msat) | Skimmed fee (msat) | Sent (msat) | Payment hashes | Notes |
|---|---|---|---|---|---|---|
