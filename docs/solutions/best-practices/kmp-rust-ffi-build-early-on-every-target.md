---
module: shared
date: 2026-07-26
problem_type: best_practice
component: tooling
severity: high
applies_when:
  - Standing up a Kotlin Multiplatform module over a Rust core via UniFFI/Gobley
  - A plan or CI gate is marked "environment-blocked" because a toolchain is missing locally
  - Reviewing code whose only verification is a host-target unit-test suite
related_components:
  - tooling
  - documentation
tags:
  - kmp
  - gobley
  - uniffi
  - rust
  - kotlin-native
  - swift
  - android
  - ldk
  - toolchain
---

# Build on every target before trusting a KMP + Rust FFI surface

## Context

The `wallet-core` spike reached what looked like a healthy state on host tooling alone: 68 Rust tests green, a code review with seven reviewer personas plus independent validation, and a simplification pass. The Gradle, Android, and Xcode gates were all recorded as "environment-blocked" because the machine had no JDK, no Android SDK, and only CommandLineTools instead of Xcode.

Then the toolchains were installed and the same code was built and run for real. **Eight defects surfaced in sequence** — none of which the Rust suite or the review could have caught, because every one lived in generated bindings, platform manifests, build specs, or live network behavior. Two of them were fatal to the app's core purpose.

The durable lesson is not any single bug. It is that a KMP + Rust FFI project's defect surface is concentrated precisely in the layers a host-only test suite never touches, so "environment-blocked" on a build gate is not a deferrable annotation — it is an unmeasured risk that grows with every commit.

## Guidance

**Install every target toolchain and complete one build-and-run per platform before writing the second feature unit.** A walking skeleton that compiles on the host is not a walking skeleton; it must link and launch on each real target.

Specific traps found, worth checking directly:

**1. Field names that collide with the target language's supertype.** A Rust error variant with a `message` field:

```rust
// Before — generated Kotlin fails to compile
pub enum WalletError {
    Startup { message: String },
    InvalidInvoice { message: String },
}
```

UniFFI generates a Kotlin exception class per variant, and `message` collides with `Throwable.message`: *"Conflicting declarations"* plus *"'message' hides member of supertype 'Throwable' and needs an 'override' modifier."* Rename the field (`detail`), and avoid `message`, `cause`, and `stackTrace` in any exported error type.

**2. `Dispatchers.IO` does not exist in `commonMain` on Kotlin/Native.** It is JVM/Android-only, so a shared coroutine that uses it compiles for the JVM target and fails for iOS: *"Cannot access 'val IO: CoroutineDispatcher': it is internal in 'kotlinx/coroutines/Dispatchers'."* Use `expect`/`actual`:

```kotlin
// commonMain
internal expect val ioDispatcher: CoroutineDispatcher
// jvmMain / androidMain -> Dispatchers.IO
// iosMain -> Dispatchers.Default
```

**3. Rust `Option<T>` exports as a boxed nullable, not a primitive.** `skimmed_fee_msat: Option<u64>` reaches Swift as `KotlinULong?`, so a Swift adapter declaring `UInt64` fails to compile. Map with `?.uint64Value` and keep the payload optional end to end. Notably this had been filed during code review as a low-confidence residual risk and was **confirmed as a real compile error** by the first build — an argument for treating binding-shape residuals as build-blocking rather than advisory.

**4. Gobley's `jvm()` target asks cargo for every publishable JVM triple**, including Linux, which needs a cross-compiler most machines lack: *"failed to find tool `aarch64-linux-gnu-gcc`."* Excluding the task does not work — the task graph depends on it. Scope the build instead:

```kotlin
cargo {
    publishJvmArtifacts = false
    builds.withType<CargoJvmBuild<*>>().configureEach {
        embedRustLibrary = rustTarget == GobleyHost.current.rustTarget
    }
}
```

**5. An XcodeGen spec needs `GENERATE_INFOPLIST_FILE`.** Without it: *"Cannot code sign because the target does not have an Info.plist file."*

**6. An Android manifest with no `uses-permission` entries has no network access.** Every HTTPS call and outbound TCP connection fails at the OS level, surfacing as an application-level "sync failed" with nothing in logcat naming permissions. iOS requires no equivalent declaration, so this is invisible until an Android run:

```xml
<uses-permission android:name="android.permission.INTERNET" />
<uses-permission android:name="android.permission.ACCESS_NETWORK_STATE" />
```

**7. A free public API's rate limiting can look like a code bug.** `mempool.space` returned HTTP 200 for a single request in **75 seconds** after throttling the repo's own test volume. Since an LDK chain sync issues many sequential calls, every pass blew its timeout and the UI showed "Chain sync failed." Blockstream measured ~250ms for the same call. When a network-dependent gate fails, time a single raw request before touching the code — and prefer an endpoint you control.

**8. Public explorer listings are not authoritative for a service endpoint.** The LSPS2 node id and host were taken from Amboss/1ML listings for the LSP's name. That node completes a BOLT8 handshake and answers Ping/Pong, but never replies to `lsps2.get_info` — it is not the LSPS2 service. The result read convincingly as "the LSP requires an access token," and two client-side causes were ruled out (a 9x timeout increase, and a genuine double-dial race) before the real cause was found: a sibling project's working configuration named a different node id and host entirely. **When a sibling repo already talks to the service, its config is the source of truth; explorer metadata is a guess.**

Toolchain constraints worth recording alongside the code ones:

- **A too-new JDK fails opaquely.** JDK 26 aborted the Android Gradle Plugin with a bare `* What went wrong: 26.0.1` and no explanation. JDK 21 worked. Pin the JDK in the README.
- **Xcode ships with no simulator runtime.** `xcodebuild -showsdks` lists the iOS SDK while `xcrun simctl list runtimes` is empty; `xcodebuild -downloadPlatform iOS` must be run with `sudo`, and non-interactively it stalls silently at 0% CPU with no network sockets rather than reporting that it needs authorization.
- **An emulator/simulator is a hard prerequisite, not a detail.** Budget the multi-GB downloads before promising a run.

## Why This Matters

Every one of these eight defects sat in a layer the host test suite cannot reach: four in generated bindings, two in platform build/manifest configuration, two in live external services. A project can therefore accumulate a green Rust suite, a clean multi-persona code review, and a simplification pass while remaining **unable to launch on either target** — which was precisely the state this spike was in before the toolchains were installed.

The cost asymmetry is stark. Each bug took minutes to fix once a compiler or a device pointed at it. Diagnosing them from source alone ranged from hard to genuinely impossible: no reading of `AndroidManifest.xml` reveals a missing permission that no code references, and no amount of review would have revealed that the LSP's published node id is the wrong one.

The second-order cost is worse: findings get mis-attributed. The wrong LSP identity was recorded in the plan as an external blocker ("Megalith requires a token — request access") that would have sat waiting on someone else's reply. It was our configuration, fixable in one line.

## When to Apply

- **Immediately**, at the walking-skeleton stage of any KMP + Rust/UniFFI project. The skeleton's job is proving the toolchain, so it must link and launch on every target, exercise one async export (the JNA/Kotlin-Native suspend paths are where Gobley regressions live), and route through at least one real dependency rather than a trivial function.
- **Whenever a plan or PR marks a build gate "environment-blocked."** Treat that as an open risk with an owner, not documentation. State plainly which gates are unmeasured.
- **Before believing an external service is at fault.** Rule out your own identity/config against a working sibling client, and time one raw request, before recording a third-party blocker.
- **Not** as an argument against host-target tests. The 68 Rust tests caught real defects cheaply and remain the fast inner loop. The point is that they and the platform builds cover disjoint surfaces; neither substitutes for the other.

## Examples

The sequence that produced this learning, in the order the compiler and the devices forced it:

| Gate first run | What it exposed |
|---|---|
| `:shared:jvmTest` | `message` field colliding with `Throwable.message`; Gobley demanding a Linux cross-compiler |
| `:shared:linkDebugFrameworkIosSimulatorArm64` | `Dispatchers.IO` unavailable in `commonMain` on Kotlin/Native |
| `swiftc -typecheck` against the real framework | `Option<u64>` exporting as nullable `KotlinULong?` |
| `xcodebuild` for the simulator | XcodeGen spec missing `GENERATE_INFOPLIST_FILE` |
| App launched on the iOS simulator | mempool.space throttling stalling every chain sync |
| Live LSPS2 request | Wrong LSP identity sourced from explorer listings |
| App launched on the Android emulator | Manifest missing `INTERNET` permission |

Two gates the plan had assumed and never verified also came good only on a real run: the packaged `.so` files' 16 KB `LOAD` alignment (Android 15+ requirement, provided by NDK r28 by default), and the claim that one `commonMain` core could drive both shells with no platform-specific logic.
