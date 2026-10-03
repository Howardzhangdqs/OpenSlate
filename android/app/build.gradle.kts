import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "dev.openslate.mobile"
    compileSdk = 37
    ndkVersion = "21.4.7075529"

    defaultConfig {
        applicationId = "dev.openslate.mobile"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        // Phase 1 只带 arm64（真机即 arm64）。
        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    // AGP 9 内置 Kotlin（无需 kotlin-android 插件）。
    kotlin {
        compilerOptions {
            jvmTarget.set(JvmTarget.JVM_17)
        }
    }
    buildFeatures {
        compose = true
        // BuildConfig.DEBUG 门控内容型日志（release 不剥离 logcat）需要
        // BuildConfig 生成；AGP 8+ 默认关闭，需显式开启。
        buildConfig = true
    }
    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.lifecycle.service)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.androidx.material.icons)
    implementation(libs.androidx.navigation.compose)
    implementation(libs.kotlinx.coroutines.android)
    // Markdown 渲染（模型回复）。
    implementation(libs.markdown.renderer)
    implementation(libs.markdown.renderer.core)
    // UniFFI 生成的 Kotlin 绑定依赖 JNA（com.sun.jna.*）。
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    // Shizuku（设置页一键授权 Termux RUN_COMMAND；仅一次性 pm grant 用）。
    implementation("dev.rikka.shizuku:api:13.1.5")
    implementation("dev.rikka.shizuku:provider:13.1.5")
    debugImplementation(libs.androidx.ui.tooling)
}
