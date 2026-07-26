# zinqq-kmp

Native Kotlin Multiplatform spike for the Zinqq Lightning wallet: a Rust core built directly on the LDK crates, exposed via UniFFI/Gobley into shared `commonMain` Kotlin, with thin native Compose (Android) and SwiftUI (iOS) shells.

Success criterion: one real mainnet payment received through a Megalith LSPS2 JIT channel and one sent, driven by the same shared core on both platforms.

- Requirements plan: `docs/plans/2026-07-25-001-feat-kmp-native-payment-spike-plan.md`
- Extraction of the Zinqq web implementation this spike references: `docs/research/zinq-grounding-dossier.md`

The Zinqq web PWA remains the production client; this repo is an exploration, not a migration.
