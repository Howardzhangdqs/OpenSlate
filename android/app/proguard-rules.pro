# UniFFI 生成代码不裁剪（Rust 侧对象经 JNI 持有）。
-keep class uniffi.** { *; }
-dontwarn uniffi.**
