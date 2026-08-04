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

  **Re-confirmed and deliberately deferred again** by a second review pass
  (`ce-code-review 20260803-171441-e7169db0`), which reached it independently
  from the api-contract lens and had an independent validator confirm the
  mechanism: `MONITOR_MANIFEST_KEY` is in `FIXED_REMOTE_KEYS`, so an ordinary
  boot seeds its real remote version from `listKeyVersions` while `monitor_keys`
  is built only from LOCAL monitors — a matching-version DELETE therefore
  succeeds against a body this client never read. So the earlier "no independent
  corroboration" caveat no longer applies; the deferral now rests only on the
  scope call that a fund-safety delete path deserves its own change. Nothing
  else in this branch depends on it.

## Applied in this branch, recorded for traceability

Two findings from the same review were test-only and were applied in
`fix(review): apply review findings`:

- P2 — `rust/src/vss/store.rs:917` — update-put promotion to publishable had no
  test. Now covered by `an_update_under_a_derived_key_heals_it_into_publishability`.
- P2 — `rust/src/vss/store.rs:861` — delete-conflict abandonment had no test. Now
  covered by `a_conflicting_manifest_retire_is_abandoned_not_forced`.

From the second review pass (`ce-code-review 20260803-171441-e7169db0`):

- P2 — `rust/src/restore.rs:2755` — the regression test's route-2 waits used the
  silent-timeout `settle` helper with no follow-up assertion, so a break in the
  legitimate new-channel manifest publish would have made the unverifiable-key
  assertion iterate an empty payload list and pass vacuously (both restore doors
  still succeed via orphan adoption, so nothing else caught it). `settle` now
  returns whether the condition held and is `#[must_use]`; the two route-2 waits
  are `assert!`-wrapped, and the route-1 wait is explicitly `let _ =` with a
  comment stating its timeout is the expected outcome.

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

### From the second review pass

- **R6 is only partially met, and the surviving instance is pre-existing.** R6 asks
  that the archive path leave "neither an empty array nor a listing of a blob that
  was just deleted." The empty-array half is closed. `archive_monitor` still
  discards `write_manifest_once_best_effort`'s outcome and deletes the blob
  regardless (`store.rs:1469` then `:1477`), so a retire that errors or is
  abandoned on a 409 leaves the manifest naming a key whose blob is gone. An
  independent validator confirmed the unconditional delete and three of its four
  failure exits are byte-identical to base, and that the new retire branch strictly
  improves on base (which published a corrupt `[]` unconditionally on the
  single-channel path) — so this is not a regression, but it is the same brick
  class still live in the multi-channel case. Fixing it means having the manifest
  write report whether the key is gone and skipping the blob delete when it is not;
  that also closes the pre-existing instance.
- `extend_publishable` treats the server's manifest as proof of publishability, so a
  manifest written by a pre-fix client or the PWA that already names an unreachable
  key is re-promoted and republished on the conflict path. Not a regression (base's
  plain `extend` behaved the same for publication), but the fix does not heal it.
- The observable failure mode changed direction: for a monitor whose blob is
  genuinely absent remotely, base published the key and a later restore failed
  loudly with `BackupInconsistent`; now the key is silently omitted and the restore
  succeeds with a wallet missing that channel's monitor.
- `mark_publishable` uses `BTreeMap::insert`, so an in-flight monitor put landing
  after `archive_persisted_channel` removed the key re-inserts it as publishable,
  and the archive task's delete then removes the blob. Narrow (needs a pending job
  spanning `ARCHIVAL_DELAY_BLOCKS`); a non-inserting `promote_if_tracked` would
  close it. Base could not hit this — removal used to be final.
- `retire_manifest` has no fence re-check, unlike the sibling delete in the same
  spawned task; the only `is_fenced()` test in `write_manifest_once_best_effort`
  precedes an `await` on `manifest_lock`.
- **PWA absence semantics are unverified.** `fetch_manifest` already read `Ok(None)`
  as a zero-channel backup, but this branch is the first to make the native client
  actively produce that absent state; the PWA never deletes the manifest, it leaves
  it stale. If the PWA's onboarding-vs-restore flow keys off manifest absence, a
  retired manifest could make a used wallet look brand-new to a PWA session on the
  same backup. Not checkable from this repo.
- `vss/store.rs` is now 2,980 lines (from 2,458). `MonitorKeySet`, `KeyProvenance`,
  `is_valid_monitor_key`, and `parse_monitor_manifest` would extract cleanly into
  `vss/monitor_keys.rs`, mirroring `known_peers.rs`/`startup.rs` in the same
  directory.

## Remaining testing gaps

- No test asserts `retire_manifest` leaves the manifest alone when the remote body
  lists keys the store does not track — the coverage that would have caught the P1.
- Branch 2 migration-success seeding uses `MonitorKeySet::all_publishable` but no
  test asserts publishability for that branch; the plan listed it as a U2 scenario.
- No test exercises `archive_monitor`'s blob delete running after the manifest write
  failed or was abandoned (the R6 gap above).
- No test covers an in-flight put landing after `archive_persisted_channel` removed
  the key, or `extend_publishable` re-promoting a key tracked as unverified.
- `write_manifest_once_best_effort`'s own 409-merge branch has no direct test (its
  sibling `write_manifest_with_retry_locked`'s does).
- Startup branch 3's positive `observed_remotely` case is not isolated from silent
  recovery's separate `publishable_monitor_keys()` mechanism.
