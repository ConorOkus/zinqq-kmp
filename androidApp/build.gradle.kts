import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

/**
 * The network a debug build targets: Mutinynet unless `-Pzinqq.network=mainnet`
 * says otherwise (KTD-4).
 *
 * An unrecognized value falls back to the debug default rather than failing the
 * build — a typo should not silently produce some third behaviour, and the only
 * value worth honouring here is the deliberate mainnet opt-in.
 */
fun Project.debugWalletNetwork(): String =
    when (providers.gradleProperty("zinqq.network").orNull?.lowercase()) {
        "mainnet" -> "mainnet"
        else -> "mutinynet"
    }

android {
    // U13: the spike package graduated to the product id. Safe because spike
    // installs are disposable (plan Key Decisions: no migration is built).
    namespace = "zinqq.app"
    compileSdk = 35
    defaultConfig {
        applicationId = "zinqq.app"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        ndk.abiFilters += setOf("arm64-v8a", "x86_64")
    }
    buildTypes {
        // Debug targets Mutinynet so local testing never touches real funds
        // (KTD-1: build-time selection, no runtime switch). `-Pzinqq.network=
        // mainnet` overrides it when a production bug needs reproducing with a
        // debugger attached (KTD-4) — an escape hatch Release deliberately
        // does not get.
        getByName("debug") {
            applicationIdSuffix = ".debug"
            buildConfigField("String", "WALLET_NETWORK", "\"${debugWalletNetwork()}\"")
        }
        // Release — and therefore TestFlight — is hard-wired to mainnet. It
        // reads no property, so no build invocation can put a shipped binary
        // on a test network.
        getByName("release") {
            buildConfigField("String", "WALLET_NETWORK", "\"mainnet\"")
        }
    }
    buildFeatures {
        compose = true
        buildConfig = true
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

dependencies {
    implementation(project(":shared"))
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.lifecycle.process)
    // U13: 16-destination NavHost with declarative destination-based back (KTD-11).
    implementation(libs.androidx.navigation.compose)
    // U13: appearance mode + balance visibility persisted with the PWA's keys (R12).
    implementation(libs.androidx.datastore.preferences)
    implementation(libs.kotlinx.coroutines.android)
    // QR rendering only: the BOLT11 string goes in, pixels come out (R4).
    implementation(libs.zxing.core)
    // U15 Scan: CameraX preview + MLKit QR-only analyzer (R13).
    implementation(libs.androidx.camera.camera2)
    implementation(libs.androidx.camera.lifecycle)
    implementation(libs.androidx.camera.view)
    implementation(libs.androidx.camera.mlkit.vision)
    implementation(libs.mlkit.barcode.scanning)

    testImplementation(libs.junit)
    testImplementation(libs.kotlin.test)
    testImplementation(libs.kotlinx.coroutines.test)
}
