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
            // 本地测试用：debug keystore 签 release，可与 debug 包互相覆盖安装
            // （免卸载、不丢本地数据）。正式发布前换成正式签名。
            signingConfig = signingConfigs.getByName("debug")
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
    // Markdown 渲染（模型回复）：code 模块提供语法高亮代码块，coil3 模块提供
    // 图片加载（配 coil3 + SvgDecoder 支持 SVG）。
    implementation(libs.markdown.renderer)
    implementation(libs.markdown.renderer.core)
    implementation(libs.markdown.renderer.code)
    implementation(libs.markdown.renderer.coil3)
    // 代码高亮引擎（SyntaxMP tokenizer 层：XML/HTML/SVG 等标记语言支持）。
    implementation(libs.syntaxmp.tokenizer)
    implementation(libs.coil.compose)
    implementation(libs.coil.network.okhttp)
    implementation(libs.coil.svg)

    // Haze：弹层局部真实背景模糊（hazeSource / hazeBlur），Apache-2.0。
    implementation(libs.haze)
    implementation(libs.haze.blur)

    // Telephoto：附件全屏查看器的缩放/平移手势 + 大图子采样（按解码区域
    // 分块解码，2048px 级 JPEG 不再整张进内存）；okio 经 sub-sampling-image
    // 传递引入，无需显式声明。
    implementation(libs.telephoto.zoomable)
    implementation(libs.telephoto.subsampling)
    // UniFFI 生成的 Kotlin 绑定依赖 JNA（com.sun.jna.*）。
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    // Shizuku（设置页一键授权 Termux RUN_COMMAND；仅一次性 pm grant 用）。
    implementation("dev.rikka.shizuku:api:13.1.5")
    implementation("dev.rikka.shizuku:provider:13.1.5")
    debugImplementation(libs.androidx.ui.tooling)
}
