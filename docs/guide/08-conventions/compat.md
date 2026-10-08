# 08-conventions · Android API 兼容（API 26～37）

> 来自 `AGENTS.md`（仓库根，权威来源）的硬性规定 + 源码既有范例。

## 1. 目标区间（硬性）

- 测试设备 API 区间 **26（Android 8.0，华为 P10 lite）～ 37（Android 17，小米 15 Pro）**——设备权威信息见根 `AGENTS.md`。
- 代码需兼容整个区间；**涉及新 API 时必须做版本判断**：`Build.VERSION.SDK_INT >= Build.VERSION_CODES.X`。

## 2. 构建层面（`android/app/build.gradle.kts`）

| 项 | 值 | 语义 |
|----|-----|------|
| `minSdk` | **26** | 下限即最低支持线 |
| `compileSdk` | **37** | 用最新 API 编译（lint 会拦高于 minSdk 的调用） |
| `targetSdk` | **36** | 运行时行为按此对齐 |

设备均为 arm64：`abiFilters += listOf("arm64-v8a")`；构建/安装优先 arm64；多设备时 `adb -s <序列号> install -r <apk>`。

## 3. 现有版本判断范例（新代码对齐它们）

### 3.1 `AgentService.kt`（service/AgentService.kt:72-80）

`startForeground` 在 API 34+（`UPSIDE_DOWN_CAKE`）必须显式传 `FOREGROUND_SERVICE_TYPE_SPECIAL_USE`，低版本走无类型重载：

```kotlin
if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
    startForeground(NOTIFICATION_ID, notification,
        ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
} else {
    startForeground(NOTIFICATION_ID, notification)
}
```

### 3.2 `SecretStore.kt`（bridge/SecretStore.kt）

Keystore 密钥：API ≥ 28（`P`）优先 StrongBox，失败回退 TEE；API 26/27 直接 TEE：

```kotlin
val specs = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
    listOf(KeyGenParameterSpec.Builder(..., STRONGBOX_BACKED).build(),
          KeyGenParameterSpec.Builder(...).build())   // StrongBox 优先，TEE 兜底
} else {
    listOf(KeyGenParameterSpec.Builder(...).build())  // 26/27 只有 TEE
}
```

### 3.3 Manifest / Gradle 层兼容项

- `<queries>`（Android 11+ 包可见性）：`com.termux`、`moe.shizuku.privileged.api`——低版本不受影响，直接声明即可。
- `POST_NOTIFICATIONS` 运行时权限（API 33+）按需请求；前台服务通知在低版本照常。

## 4. 检查清单

1. 新 API 先查「API level ≥ 26 是否可用」；不满足则写 `SDK_INT` 分支，高版本走新路径、低版本走回退路径（参考 §3 范例的「先试高规格再回退」结构）。
2. Kotlin 标注 `@RequiresApi(N)` 的调用必须被版本分支包住（lint 会提醒）。
3. 真机验证双端：低版本行为在华为（API 26）上过一遍，新 API 路径在小米（API 37）上过一遍。
