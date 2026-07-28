# Residual Review Findings

Source: ce-code-review run 20260727-043422-b54ac024 on branch `feat/pwa-feature-parity` (base 7b50e8a, verdict "Ready with fixes"). Seven of eight actionable findings were applied and committed in `fix(review): apply review findings`; the item below was deferred, and the report-only observations are recorded here because the run artifacts live in temp storage.

## Residual Review Findings

- P1 — `rust/src/node.rs:318` — Split node.rs (3,100 lines) into sibling impl files — [#4](https://github.com/ConorOkus/zinqq-kmp/issues/4). Deferred: the restructure would churn the entire reviewed diff post-review; isolated follow-up PR.

## Report-only observations (no ticket)

- P2 advisory — `rust/src/api.rs:985` — the flat 1,724-line `impl Wallet` block should split into api/payments|onchain|channels when the node.rs split (#4) happens.
- Known Pattern (debated, validator-dropped) — `rust/src/vss/store.rs:519` — the manifest 409-merge retry is uncapped; the sibling PWA repo's dual-write learning caps conflict retries at 5. Validator: the monotone merge converges and the indefinite gate is the documented KTD-3 design. Revisit if racing-writer telemetry appears.
- Validator-dropped — `iosApp/.../SendController.swift:212` — claimed missing task cancellation cannot occur (strong self-capture precludes deallocation mid-job; entry points already cancel).
- Testing gaps recorded in the review: CM bounded-attempt timeout branch (`store.rs:730`) needs a hang-capable mock; `apply_config_overrides` sibling branch untested; capture-protection has no automated tests on either platform.
- Outstanding by design: U23 manual mainnet acceptance protocol (AE1–AE6, force-close drill, collision drill) documented in `README.md` with an empty results table — requires human devices/funds.
