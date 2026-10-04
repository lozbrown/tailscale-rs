plugins {
    id("com.android.library")
    kotlin("android")
}

android {
    namespace = "com.tailscale.rs.android"
    compileSdk = 35

    defaultConfig {
        minSdk = 24
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDir(providers.gradleProperty("rustJniLibsDir").orElse("src/main/jniLibs").get())
        }
    }
}

kotlin {
    jvmToolchain(17)
}
