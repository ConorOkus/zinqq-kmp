import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
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
    buildFeatures {
        compose = true
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

    testImplementation(libs.junit)
    testImplementation(libs.kotlin.test)
    testImplementation(libs.kotlinx.coroutines.test)
}
