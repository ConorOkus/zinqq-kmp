# TestFlight Upload Runbook

Manual distribution of the zinqq iOS app to **internal TestFlight testers** under an individual Apple developer account. Internal testing is review-free (no Beta App Review); external beta and App Store release are out of scope — Apple guideline 3.1.5 requires organization enrollment for a wallet app to pass any review.

Uploads go through **Xcode Organizer** (or the Transporter app). Never `altool` — it is deprecated and its upload path breaks on current toolchains.

## Prerequisites

- Apple Developer Program **individual** enrollment ($99/yr), active.
- Xcode with the iOS SDK, `xcodegen` (`brew install xcodegen`), and a JDK 21 for Gradle.
- The Rust device target: `rustup target add aarch64-apple-ios` (the Gobley plugin also auto-installs it on first build).

**Java note (known hazard):** Gradle runs inside Xcode's build-phase shell, which does not inherit your interactive shell's Java setup — and the Gradle *wrapper* must find a `java` executable before Gradle reads any properties, so `org.gradle.java.home` alone cannot rescue a machine where `java` is not discoverable. If the archive's "Build Shared KMP Framework" phase dies immediately with a Java-location error, make a JDK 21 discoverable system-wide. With Homebrew:

```bash
brew install openjdk@21
sudo ln -sfn /opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk \
  /Library/Java/JavaVirtualMachines/openjdk-21.jdk
```

After that, `/usr/libexec/java_home -v 21` resolves and GUI-launched Xcode finds Java. Optionally add `org.gradle.java.home=<jdk-home>` to `~/.gradle/gradle.properties` — that setting only selects the JVM Gradle's daemon runs on; it is not a substitute for a discoverable `java`.

## First-time setup

1. **Verify the Team ID.** `DEVELOPMENT_TEAM: 2LD486V4AU` is committed in `iosApp/project.yml`; confirm it matches App Store Connect → Membership details (replace it if the app ever moves to a different team). Then regenerate:

   ```bash
   (cd iosApp && xcodegen generate)
   ```

2. **Pre-warm the Release device chain** (catches toolchain/config errors before the slow first archive — this is the first half of what an archive does). From the repository root:

   ```bash
   ./gradlew :shared:linkReleaseFrameworkIosArm64
   ```

   Do not try to run `embedAndSignAppleFrameworkForXcode` from a terminal — it requires the full Xcode build-phase environment (five env vars including a versioned `SDK_NAME`) and refuses to run outside it.

3. **Archive.** Open `iosApp/iosApp.xcodeproj` in Xcode, select the `iosApp` scheme with destination **Any iOS Device (arm64)**, then Product → Archive. Automatic signing creates the distribution certificate and auto-registers the `zinqq.ios` App ID on first signed build. (If you create the App Store Connect record *before* the first archive, register `zinqq.ios` manually under Certificates, Identifiers & Profiles first.)

4. **Create the App Store Connect record.** App Store Connect → Apps → **+** → New App: platform iOS, bundle ID `zinqq.ios`, a name and primary language, SKU (e.g. `zinqq-ios`).

5. **Upload.** In Xcode Organizer: select the archive → Distribute App → App Store Connect / TestFlight & App Store → Upload.

6. **Export compliance.** The build declares `ITSAppUsesNonExemptEncryption: false` — the exempt self-classification: the wallet uses only standard, published algorithms (ChaCha20-Poly1305, LDK protocol crypto, TLS), which qualify under the EAR mass-market exemptions. With `false` committed, App Store Connect asks no per-build compliance questions. Do not flip it to `true` — that value demands filed export-compliance documentation plus a compliance code in the plist, and uploads without them are rejected (validation error 90592). The **French App Store declaration** applies only if the app is made available in France — decide availability when configuring territories.

7. **Add internal testers.** App Store Connect → Users and Access: add each tester with **Marketing** — the least-privileged of the roles eligible to be internal testers (Account Holder, Admin, App Manager, Developer, Marketing); Marketing cannot upload or manage builds. Only the account owner should hold upload capability. Then App → TestFlight → Internal Testing: create a group (leave **Enable automatic distribution** on) and add them. Builds are installable the moment processing finishes — no review.

## Routine upload

1. Bump the build number: in `iosApp/project.yml`, increment `CURRENT_PROJECT_VERSION` (bump `MARKETING_VERSION` only for a user-visible version change). Regenerate: `(cd iosApp && xcodegen generate)`.
2. Run the pre-upload sanity check (below).
3. Product → Archive in Xcode, then Organizer → Distribute App → Upload.
4. Internal testers are notified automatically once processing completes.

## Pre-upload sanity check

Internal TestFlight has no review step, so this checklist is the only gate between an archive and testers' devices:

- [ ] The archive is built from the intended, reviewed commit (`git log -1`; working tree clean).
- [ ] Configuration is **Release**, destination a device archive (not simulator).
- [ ] Bundle ID is `zinqq.ios`, Team ID is the expected one (Organizer shows both).
- [ ] `CURRENT_PROJECT_VERSION` is higher than the last uploaded build.

## Operational constraints (real-funds wallet)

- **TestFlight builds expire 90 days after upload** and refuse to launch afterward. Upload a replacement before expiry while testers hold funds — an expired build locks testers out of the app (funds remain recoverable via VSS backup/seed, but not in-app until a fresh build installs).
- **Before a tester funds a wallet on a TestFlight install**, verify their recovery path once end-to-end (VSS-encrypted backup restore or seed restore), and keep balances small — testers are running mainnet Lightning with real money.

## CI guard

The `ios-release-device` CI job compiles the same Rust-release + Kotlin/Native-release + unsigned-app chain an archive uses. If it is red, fix it before attempting an upload — an archive will hit the same failure with worse error messages.
