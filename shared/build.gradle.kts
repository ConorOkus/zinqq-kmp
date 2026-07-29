import gobley.gradle.GobleyHost
import gobley.gradle.cargo.dsl.CargoJvmBuild
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.kotlin.multiplatform)
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.atomicfu)
    alias(libs.plugins.gobley.cargo)
    alias(libs.plugins.gobley.uniffi)
}

cargo {
    // The wallet-core crate lives at the repository root, not inside this module.
    packageDirectory = layout.projectDirectory.dir("../rust")
    // The JVM target exists only for host-side binding tests. Without this,
    // Gobley builds cargo for every publishable JVM triple (including Linux)
    // and needs cross-compilers this repo does not ship.
    publishJvmArtifacts = false
    builds.withType<CargoJvmBuild<*>>().configureEach {
        embedRustLibrary = rustTarget == GobleyHost.current.rustTarget
    }
}

uniffi {
    // Library-mode generation from the built cdylib (KTD-2): proc-macro
    // scaffolding via uniffi::setup_scaffolding!(), no UDL.
    generateFromLibrary {
        packageName = "uniffi.wallet_core"
    }
}

kotlin {
    androidTarget {
        compilerOptions {
            jvmTarget.set(JvmTarget.JVM_17)
        }
    }
    jvm {
        compilerOptions {
            jvmTarget.set(JvmTarget.JVM_17)
        }
    }
    listOf(
        iosArm64(),
        iosSimulatorArm64(),
    ).forEach { iosTarget ->
        iosTarget.binaries.framework {
            baseName = "Shared"
            isStatic = true
        }
    }

    sourceSets {
        commonMain.dependencies {
            implementation(libs.kotlinx.coroutines.core)
        }
        jvmTest.dependencies {
            implementation(libs.kotlin.test)
            implementation(libs.kotlinx.coroutines.test)
        }
    }
}

android {
    namespace = "zinqq.main.shared"
    compileSdk = 35
    // NDK r28+: 16 KB page-size alignment is the default for packaged .so files.
    ndkVersion = "28.1.13356709"
    defaultConfig {
        minSdk = 26
        ndk.abiFilters += setOf("arm64-v8a", "x86_64")
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
