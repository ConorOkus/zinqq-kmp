---
title: "First App Store upload validation: icons, device family, export compliance"
date: 2026-07-29
category: integration-issues
module: iosApp
problem_type: integration_issue
component: tooling
symptoms:
  - "Upload rejected: missing 120x120 iPhone icon (90022) and 152x152 iPad icon (90023)"
  - "Missing Info.plist value CFBundleIconName (90713)"
  - "Invalid Export Compliance Code — ITSAppUsesNonExemptEncryption=true with no filed documentation (90592)"
  - "Invalid bundle: iPad multitasking requires all four UISupportedInterfaceOrientations (90474)"
root_cause: config_error
resolution_type: config_change
severity: high
tags: [ios, testflight, app-store-validation, xcodegen, app-icon, export-compliance, device-family]
---

# First App Store upload validation: icons, device family, export compliance

## Problem

The first-ever TestFlight upload of the zinqq iOS app (KMP shared core, XcodeGen-generated Xcode project, `GENERATE_INFOPLIST_FILE: YES` with an `info:` block in `iosApp/project.yml` writing a generated, gitignored `Info.plist`) was rejected by App Store Connect's server-side validation on five counts. The app had been developed and CI-tested exclusively against the simulator; nothing in that loop ever required an icon, exercised device-family scoping, or asked about export compliance, so all three gaps shipped invisibly until the first real upload.

## Symptoms

Validation returned five errors on the same build (1.0, build 1):

1. **90022** — missing 120x120 iPhone app icon.
2. **90023** — missing 152x152 iPad app icon.
3. **90713** — missing `CFBundleIconName`.
4. **90592** — "Invalid Export Compliance Code. The export compliance key value [] in the app's Info.plist doesn't match the key value of the app's export compliance documentation" (`ITSAppUsesNonExemptEncryption` was `true`).
5. **90474** — iPad multitasking requires all four `UISupportedInterfaceOrientations`; the app declared portrait only.

These observed validation responses collapse into three root causes, not five:

- **(a) No icon asset catalog existed at all.** Simulator runs never require one.
- **(b) The default `TARGETED_DEVICE_FAMILY` was effectively "1,2"** (iPad support was never explicitly scoped away), silently declaring iPad support a phone-designed app never intended — which is what drags in the 152x152 iPad icon and the four-orientation multitasking requirement.
- **(c) `ITSAppUsesNonExemptEncryption: true`** is not the "honest, we-use-encryption" answer. `true` means the app uses NON-exempt encryption and demands filed export-compliance documentation plus a matching compliance code in the plist. The app's crypto (ChaCha20-Poly1305 for VSS blobs, LDK protocol crypto, TLS) is all standard, published-algorithm crypto eligible for the EAR mass-market exempt self-classification — the correct value is `false`.

## What Didn't Work

Three dead ends along the way to the fix:

1. **Reusing the PWA's existing brand PNGs (180px/512px) as the App Store icon.** They were pre-rounded with alpha (transparent corners) for web/PWA use. App Store icons must be full-bleed and opaque — iOS applies its own corner mask at render time — so an icon with baked-in rounding and alpha is wrong twice over (double-masked corners, and alpha isn't accepted for the App Store slot).
2. **Rendering the icon with `swift -interpret` plus AppKit's `NSGraphicsContext`.** This crashed headless with a signal inside AppKit — AppKit assumes a windowing/display context that isn't present in a headless interpreted-Swift invocation. A compiled pure CoreGraphics/CoreText tool (no AppKit) rendering the mark from the PWA brand SVG's proportions, using the repo-bundled `space_grotesk_700.ttf`, worked without crashing.
3. **Verifying `CFBundleIconName` with a top-level `PlistBuddy -c "Print :CFBundleIconName" Info.plist`.** This is a false negative: `actool` injects the key nested under `CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconName` (and mirrored under `CFBundleIcons~ipad` where relevant), not at the plist root. Validation reads the nested key, so a top-level check reports "missing" on a bundle that is actually correct.

## Solution

Landed in PR #10 (merged), fixing all three root causes:

**1. Icon asset catalog.** Added `iosApp/iosApp/Assets.xcassets/AppIcon.appiconset/` with a single 1024x1024 full-bleed, opaque PNG (`hasAlpha: no`), rendered from the PWA brand mark's proportions with the bundled Space Grotesk Bold font via the compiled CoreGraphics tool. `Contents.json` uses the single-size universal appiconset shape:

```json
{
  "images" : [
    {
      "filename" : "AppIcon.png",
      "idiom" : "universal",
      "platform" : "ios",
      "size" : "1024x1024"
    }
  ],
  "info" : {
    "author" : "xcode",
    "version" : 1
  }
}
```

`iosApp/project.yml` wires the catalog in under `targets.iosApp.settings.base`:

```yaml
ASSETCATALOG_COMPILER_APPICON_NAME: AppIcon
```

**2. Device-family scoping.** Same `settings.base` block declares iPhone-only, with the reasoning captured inline:

```yaml
# iPhone-only. The 16 screens are phone-designed (PWA parity), and
# declaring iPad support drags in App Store requirements the app
# doesn't meet (152x152 iPad icon, all-four multitasking orientations
# — upload validation errors 90023/90474).
TARGETED_DEVICE_FAMILY: 1
```

**3. Export compliance.** In the `info.properties` block of the same target:

```yaml
# Export compliance (R5/KTD4): the wallet's encryption is standard,
# published algorithms only (ChaCha20-Poly1305 for VSS blobs, LDK
# protocol crypto, TLS) — the exempt self-classification under the
# EAR mass-market provisions, which this key encodes as `false`
# ("uses no NON-exempt encryption"). `true` requires filed export
# compliance documentation and a compliance code in the plist; the
# first upload was rejected without them (validation error 90592).
ITSAppUsesNonExemptEncryption: false
```

`CURRENT_PROJECT_VERSION` was also bumped from 1 to 2 (same `settings.base` block) to re-upload as a new build.

**Verification before re-upload.** Rather than trust the fix blind, the unsigned Release device app was built locally and the produced bundle inspected directly: `Assets.car` and `AppIcon60x60@2x.png` present; `CFBundleIconName=AppIcon` present (nested, correctly, under `CFBundleIcons.CFBundlePrimaryIcon`); `UIDeviceFamily=[1]`; portrait-only orientation accepted because the app is now iPhone-only; `CFBundleVersion=2`; encryption key `false`. Re-upload then returned "Uploaded to Apple" and processed cleanly.

## Why This Works

- **Icon + `CFBundleIconName` (90022/90713):** `actool` (Xcode's asset catalog compiler) derives every required device/scale icon slot from the single 1024x1024 universal source at build time — including the 120x120 iPhone slot as `AppIcon60x60@2x` — and, when `ASSETCATALOG_COMPILER_APPICON_NAME` matches the appiconset name, injects `CFBundleIconName` into the built Info.plist nested under `CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconName`. One correctly-shaped source image plus that build setting satisfies both errors without hand-authoring per-size assets.
- **Device family (90023/90474):** the 152x152 iPad icon requirement and the four-orientation multitasking requirement are only imposed on builds that declare iPad (`TARGETED_DEVICE_FAMILY` including `2`). Setting it to `1` (iPhone only) removes iPad from the app's declared device families entirely, so validation stops requiring iPad-only assets and orientations the app never supported.
- **Export compliance (90592):** `ITSAppUsesNonExemptEncryption` is a declaration key, not a "do you encrypt" flag. `false` asserts the app either uses no encryption or only encryption exempt from EAR export licensing (the standard, published algorithms this app uses qualify under the mass-market exemption). `true` asserts non-exempt encryption is in use, which obligates Apple to check the app against filed export-compliance documentation and a matching compliance code — documentation this app never filed, hence the mismatch error. `false` is the factually correct and unblocking value here, not a workaround.

## Prevention

Because App Store validation runs server-side at upload time, no local build, unit test, or CI step exercises any of these five checks — the repo's `ios-release-device` CI job compiles the Rust-release + Kotlin/Native-release + unsigned-app chain and would stay green through all three root causes; it catches toolchain/config breakage, not App Store policy compliance. The cheap mitigation is a manual bundle inspection before the *first* upload (and worth re-running after any icon/device-family/compliance change):

```bash
# After an unsigned or signed Release-device build, inspect the produced .app:
plutil -p "<path-to>.app/Info.plist" | grep -A3 CFBundleIcons   # nested CFBundleIconName present?
plutil -p "<path-to>.app/Info.plist" | grep UIDeviceFamily      # matches intended TARGETED_DEVICE_FAMILY?
plutil -p "<path-to>.app/Info.plist" | grep ITSAppUsesNonExemptEncryption
ls "<path-to>.app/Assets.car"                                   # compiled asset catalog exists?
sips -g hasAlpha -g pixelWidth -g pixelHeight AppIcon.png        # opaque (no alpha), 1024x1024, before adding to the catalog
```

Checklist to apply before any first TestFlight/App Store upload:

- An `AppIcon.appiconset` exists with a full-bleed, opaque source image (no alpha, no pre-rounded corners) and `ASSETCATALOG_COMPILER_APPICON_NAME` is set on the target.
- `TARGETED_DEVICE_FAMILY` is scoped to only the device families the UI actually supports (`1` for iPhone-only apps) — don't inherit the "1,2" default by omission.
- `ITSAppUsesNonExemptEncryption` is `false` if all cryptography used is standard and published (TLS, well-known ciphers/protocols); only set `true` if filed export-compliance documentation and a compliance code actually exist.
- Verify `CFBundleIconName` with a *nested* lookup (`CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconName`), never a top-level `PlistBuddy` check — the latter is a false negative even on a correct bundle.
- If rendering icon assets programmatically and headless, use compiled CoreGraphics/CoreText, not `swift -interpret` with AppKit (`NSGraphicsContext` requires a display context AppKit can't get headless and crashes).

## Related Issues

- `docs/runbooks/testflight-upload.md` — the manual upload procedure; its export-compliance step was corrected by this learning (PR #10).
- `docs/plans/2026-07-28-001-feat-testflight-distribution-plan.md` — the distribution plan; its KTD4 originally committed `ITSAppUsesNonExemptEncryption: true`, which the first upload proved wrong. The plan stands as the historical decision record; this doc captures the correction.
- `docs/solutions/best-practices/kmp-rust-ffi-build-early-on-every-target.md` — adjacent lesson: real-target builds surface defect classes host/simulator testing cannot. This learning extends that principle past builds to Apple's server-side upload validation, which even a green device build cannot exercise.
- [PR #10](https://github.com/ConorOkus/zinqq-kmp/pull/10) — the fix. [Issue #8](https://github.com/ConorOkus/zinqq-kmp/issues/8) — same distribution effort, different failure mode (Java resolution in the archive's build phase).
