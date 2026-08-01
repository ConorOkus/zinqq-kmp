# Residual Review Findings

Branch: `fix/monitor-key-publication-and-node-split`
Plan: `docs/plans/2026-07-31-002-fix-monitor-key-publication-and-node-split-plan.md`
Review run: `ce-code-review 20260801-092650-9ea7b257`
Verdict: Ready with fixes

Findings from the branch review that were **not** applied in the pipeline's
mechanical-apply step, recorded here so they stay durable independently of the
tracker.

## Filed

- **P1 — `rust/src/vss/store.rs:850` — retire_manifest deletes a manifest it never read.**
  The new empty-publishable branch deletes the remote `_monitor_keys` manifest at
  the cached version without reading its contents first, while every other write
  path in the file merges the server's keys before writing. On an ordinary boot
  the store caches a manifest version from the `listKeyVersions` listing but never
  reads the body (`fetch_manifest` is only called from `silent_recovery`), so
  archiving the last local channel can delete a manifest that still lists another
  device's live channel. Recoverable via orphan adoption, so not fund loss — but it
  destroys manifest state this client never read, and it breaks the spirit of R4
  ("a manifest write never drops a key another device tracks") even though R4's
  wording says *write* rather than *delete*.
  Still strictly better than the pre-fix behavior, which published `[]` and bricked
  restore outright.
  Filed: https://github.com/ConorOkus/zinqq-kmp/issues/17

  Not applied here because the fix changes behavior in a fund-safety delete path,
  which is outside the pipeline's mechanical-apply bar, and because the review's
  own coverage recorded no independent cross-persona corroboration (all reviewer
  lenses ran inline on one model).

## Applied in this branch, recorded for traceability

Two findings from the same review were test-only and were applied in
`fix(review): apply review findings`:

- P2 — `rust/src/vss/store.rs:917` — update-put promotion to publishable had no
  test. Now covered by `an_update_under_a_derived_key_heals_it_into_publishability`.
- P2 — `rust/src/vss/store.rs:861` — delete-conflict abandonment had no test. Now
  covered by `a_conflicting_manifest_retire_is_abandoned_not_forced`.

## Advisory (no action taken)

- **P3 — `rust/src/node/payments.rs`** — at 1,039 lines it is the largest of the
  five new sibling modules, within a rounding error of the size that motivated
  splitting `node.rs` in the first place. Not worth churning this diff; the next
  feature in this area should split receive from send rather than growing the file.

## Residual risks (advisory, from the same review)

- `run_monitor_job`'s `mark_publishable` is only correct because
  `queue_monitor_write` early-returns for `remote.is_none()` before enqueueing;
  `put_fund_critical_with_retry` itself returns `Ok(())` in local-only mode. A
  future caller reaching `run_monitor_job` with no remote configured would mark
  keys publishable with nothing written remotely. Latent coupling, not a current
  defect.
- The gating path's empty-publishable early return (`store.rs:697`) completes a new
  channel without publishing the manifest, which would weaken KTD-3's gate if it
  were reachable. It is unreachable by construction today because promotion
  precedes the gating write, and returning `Ok` is the fund-safe direction versus
  wedging channel operations on an impossible state.
- `restore::backfill_manifest` skips on an empty publishable set while
  `store::write_manifest_once_best_effort` deletes. The asymmetry is defensible
  (backfill may have no manifest to speak of; archive has just emptied one that
  exists) but undocumented, and the P1 above is where the difference bites.

## Remaining testing gaps

- No test asserts `retire_manifest` leaves the manifest alone when the remote body
  lists keys the store does not track — the coverage that would have caught the P1.
- Branch 2 migration-success seeding uses `MonitorKeySet::all_publishable` but no
  test asserts publishability for that branch; the plan listed it as a U2 scenario.
