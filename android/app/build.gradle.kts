import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "org.dishus.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "org.dishus.app"
        minSdk = 28
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    // Release signing: the key lives outside the repo. Point DISHUS_KEYSTORE_PROPERTIES at a file
    // with storeFile / storePassword / keyAlias / keyPassword. Without it, release is unsigned.
    val signing = System.getenv("DISHUS_KEYSTORE_PROPERTIES")?.let { path ->
        Properties().apply { file(path).inputStream().use { load(it) } }
    }
    signingConfigs {
        if (signing != null) create("release") {
            storeFile = file(signing.getProperty("storeFile"))
            storePassword = signing.getProperty("storePassword")
            keyAlias = signing.getProperty("keyAlias")
            keyPassword = signing.getProperty("keyPassword")
        }
    }

    buildTypes {
        release {
            if (signing != null) signingConfig = signingConfigs.getByName("release")
            // -Pdishus.idSuffix=.test installs a release build next to an existing install
            (project.findProperty("dishus.idSuffix") as String?)?.let { applicationIdSuffix = it }
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { compose = true }
    packaging {
        jniLibs { useLegacyPackaging = false }
    }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.12.01")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.fragment:fragment-ktx:1.8.5")
    implementation("androidx.lifecycle:lifecycle-process:2.8.7")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.biometric:biometric:1.1.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")
    // UniFFI's Kotlin bindings call into the Rust core through JNA.
    implementation("net.java.dev.jna:jna:5.15.0@aar")
    // QR scanning (camera permission is requested only when the user taps "Scan").
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")
}
