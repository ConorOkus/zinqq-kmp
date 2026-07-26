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
- JDK **21** (JDK 26 is too new for the Android Gradle Plugin in use)
- Android SDK 35 with **NDK r28+** (16 KB page alignment) and an emulator system image; point `local.properties` at it via `sdk.dir=`
- Xcode 16+ (full install, not CommandLineTools) and [XcodeGen](https://github.com/yonaskolb/XcodeGen) for `iosApp`
- The Gradle wrapper is committed, so `./gradlew` needs no separate Gradle install

## Build and run

Rust core (host, no mobile toolchain needed):

```bash
cd rust
cargo test                                  # 68 offline tests
cargo test --lib -- --ignored live_megalith_receive_jit  # live LSPS2 flow (network)
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

Configuration (Esplora URL, Megalith pubkey/address, RGS URL) lives in `rust/src/config.rs`. Esplora defaults to the Zinqq PWA's own proxy (`https://zinqq.app/api/esplora`), which fronts Blockstream Enterprise staging and keeps credentials server-side; `blockstream.info` and `mempool.space` remain configurable fallbacks. Public mempool.space throttled a single request to 75s under this repo's test volume, which stalls every sync pass — prefer the proxy.

## Mainnet acceptance protocol (manual — U8 in the plan)

Amounts stay under $10 equivalent. The payer must be a **separate device** — the node is foreground-only and stops when the app backgrounds. Keep the app foregrounded from invoice display until `PaymentReceived`.

1. Fresh install on Android. First launch generates a new seed (there is no import path).
2. Request an invoice for at least **6,000 sats**. Megalith's observed floor is a flat 2,500 sat opening fee with a 2,501 sat minimum payment (1.4% proportional only matters above ~180k sats), so 6,000 sats received leaves ~3,500. The app reads the live menu and rejects amounts at or below the fee.
3. Pay from an external wallet on another device. Expect: JIT channel opens (0-conf), balance shows amount minus the skimmed opening fee. Megalith advertises `client_trusts_lsp: true`, so the preimage is released before the funding transaction is visible — the received amount is the trust ceiling.
4. Send a small payment out to an external invoice from the JIT channel balance.
5. Repeat 1–4 on iOS with zero platform Lightning-code changes.
6. Record results below (payment hashes only — never preimages or the seed; no secrets in screenshots).

### Results

_Not yet run._

| Date | Platform | Received (msat) | Skimmed fee (msat) | Sent (msat) | Payment hashes | Notes |
|---|---|---|---|---|---|---|

### Verified live (2026-07-26)

The client half of the JIT receive flow is proven against mainnet Megalith on the
Android emulator through the real UI — typing an amount and tapping **Invoice**
produced a payable `lnbc60u...` invoice with a QR and expiry countdown. Only the
external payment itself remains manual. Headless equivalents:

```bash
cd rust
cargo test --lib -- --ignored live_megalith_get_info      # real fee menu
cargo test --lib -- --ignored live_megalith_receive_jit   # get_info + buy + invoice
```

The LSP identity in `rust/src/config.rs` comes from the Zinqq PWA's own working
configuration, not from public explorer listings — the explorer-listed node
completes a handshake but never answers `lsps2.get_info`.

Also verified on the emulator run: every packaged `.so` (including
`libwallet_core.so`) has 16 KB-aligned `LOAD` segments, satisfying the Android 15+
page-size requirement that NDK r28 provides by default:

```bash
unzip -o androidApp/build/outputs/apk/debug/androidApp-debug.apk 'lib/*' -d /tmp/apk
"$ANDROID_HOME"/ndk/*/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-readelf \
  -l /tmp/apk/lib/arm64-v8a/libwallet_core.so | grep LOAD   # expect 0x4000
```

The debug APK is large (~650 MB) because the Rust staticlib carries full debug
symbols; a release build strips them.
