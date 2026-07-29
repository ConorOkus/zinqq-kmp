# Residual Review Findings — feat/testflight-distribution

Source: `ce-code-review` run `20260728-143002-73024673` (agent mode, base `main`), reviewing the TestFlight distribution change set (plan: `docs/plans/2026-07-28-001-feat-testflight-distribution-plan.md`). Four actionable findings were returned; the documentation fixes were applied in-branch (`fix(review): apply review findings`). One residual remains.

## Residual Review Findings

- P1 — `.github/workflows/ci.yml:151` — Harden Xcode pre-build script against missing JAVA_HOME (remainder of review finding #3, "CI green, but GUI archive cannot start Gradle") — filed: https://github.com/ConorOkus/zinqq-kmp/issues/8. The documentation half was applied in-branch; the deferred half is the `iosApp/project.yml` pre-build-script hardening (resolve/export a JDK before invoking `./gradlew`), which changes build behavior and needs a rebuild to verify.
