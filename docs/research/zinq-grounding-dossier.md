# Grounding Dossier: KMP Native App Port for Zinqq Lightning Wallet

## Tech Stack & LDK Integration

- Framework: React 19.2.0, TypeScript 5.9, Vite 7.3.1, Tailwind CSS 4.1.8 — `package.json:35-36`
- Lightning: `lightningdevkit: 0.2.4-0` (JavaScript/Web WASM bindings) — `package.json:32`
- Bitcoin: `@bitcoindevkit/bdk-wallet-web: ^0.3.0` (JavaScript WASM) — `package.json:24`
- Crypto: @noble libraries (secp256k1, hashes, ciphers, BIP32/39) — `package.json:26-31`
- PWA: vite-plugin-pwa 1.2.0, workbox-window 7.4.0 — `package.json:66, 70`
- WASM runtime: vite-plugin-wasm, vite-plugin-top-level-await — `package.json:68, 67`

## LDK Integration Architecture

- Initialization: LDK node created from seed; imports KeysManager, ChainMonitor, ChannelManager, NetworkGraph, ProbabilisticScorer, PeerManager from `lightningdevkit` — `src/ldk/init.ts:1-35`
- Persistence traits: custom Persist, FeeEstimator, BroadcasterInterface, SignerProvider backed by IndexedDB and BDK — `src/ldk/init.ts:41-70`
- Storage: IndexedDB, 23 object stores (ldk_seed, ldk_channel_monitors, ldk_channel_manager, ldk_network_graph, ldk_scorer, wallet_mnemonic, bdk_changeset, ldk_payment_history, …), version 13 — `src/storage/idb.ts:1-24`
- Seed: BIP39 mnemonic in IndexedDB under key 'primary' (32 bytes); keys via @noble/bip32 — `src/ldk/storage/seed.ts:1-29`

## LSPS2 Channel-Open Client

- Standalone LSPS2Client class: `requestOpeningParams` (lsps2.get_info) and `selectOpeningParams` (lsps2.buy) via JSON-RPC over peer custom messages — `src/ldk/lsps2/client.ts:1-60`
- Message routing: LDK peer message handler routes LSPS2 JSON-RPC to async client (`createLspsMessageHandler`) — `src/ldk/init.ts:64`

## Blockchain & Fee Queries

- Custom Esplora REST client (block hashes, txs, UTXOs, fee rates); raw fetch with timeout, semaphore, LRU cache — `src/ldk/sync/esplora-client.ts:1-60`
- Esplora endpoints via `LDK_CONFIG.esploraUrl` + fallback — `src/ldk/init.ts:237, 295, 297`
- BDK-WASM wallet initialized with Esplora client for UTXO tracking/balance — `src/ldk/init.ts:248`

## Network & Peer Connectivity

- WebSocket→TCP proxy (Cloudflare Worker) for peer connections; browsers lack raw TCP — `proxy/src/index.ts:1-60`
- Proxy validates origin/port, opens TCP to lightning peer, bridges bidirectionally — `proxy/src/index.ts:48-60`
- App-side `connectToPeer` (pubkey + host + port) via proxy — `src/ldk/ldk-context.ts:34`

## PWA & Persistence

- Service worker: precache + runtime NetworkFirst for WASM; manual SW registration — `vite.config.ts:62-104`
- Install prompt: `usePwaInstall()` hook, `beforeinstallprompt`, iOS detection — `src/hooks/use-pwa-install.ts:1-50`
- Update detection: Workbox update banner — `docs/plans/2026-04-02-003-feat-pwa-install-button-and-service-worker-plan.md:34`

## Key Architectural Surfaces for a Native Port

1. WASM → native: lightningdevkit JS bindings would need Kotlin/JVM or native wrapper; no official KMP LDK exists.
2. Storage: IndexedDB (browser-only) → platform-native or shared SQLite via KMP.
3. Peer connectivity: proxy removed; native raw TCP on both platforms.
4. Esplora REST: direct HTTP from KMP (no CORS constraint).
5. LSPS2 RPC: over peer custom messages — protocol unchanged; routing moves to native LDK bindings.
6. Seed & keys: IndexedDB → platform keystore (Keychain / EncryptedSharedPreferences).
7. BDK: JS-only today; BDK Kotlin exists, BDK Swift in dev.

## No Prior Native/KMP Work

- Grep of docs/brainstorms, docs/plans, docs/solutions: zero mentions of Kotlin, KMP, Swift, React Native, Capacitor.
- PWA is the current mobile strategy (`src/hooks/use-pwa-install.ts`).
