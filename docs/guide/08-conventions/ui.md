# 08-conventions · UI 规范（Material 图标）

> 来自 `AGENTS.md`（仓库根，权威来源）的硬性规定 + 源码既有基线。冲突时以 AGENTS.md 为准。

## 1. 硬性规则

- **UI 中表达状态/能力的图标一律使用 Material Design 图标**（`androidx.compose.material.icons`；工程引的是 `material-icons-extended`，版本随 Compose BOM 走）。
- **禁止用 Unicode 字符当图标**（✓ ✗ ● ▶ 之类）——测试设备横跨 Android 8～17（API 26～37），字体渲染不一致，视觉不可控。
- **文本内嵌选中态也不例外**：选中标记用 `Icon(Icons.Filled.Check, ...)`，不要往 `Text` 里拼 `" ✓"` 字符串。
- 例外：**logcat 日志里的 `✓` 不受此限**（非 UI 渲染）。

## 2. 现有基线（新 UI 对齐它们）

| 基线 | 位置 | 用法 |
|------|------|------|
| `DropdownChoice` 菜单项 | `android/app/src/main/java/dev/openslate/mobile/ui/SettingsScreen.kt` | 下拉选中态用 `Icon(Icons.Filled.Check, …)`，不拼字符串 |
| `ModelCapsRow` + `CapsFlag` | `SettingsScreen.kt` | 模型能力摘要（vision/reasoning/tool_call）的能力图标/标识 |
| `EditModelDialog` 的 Provider 选择器 | `SettingsScreen.kt` | 选中态同 DropdownChoice 风格 |

## 3. 写法示例（对齐基线）

```kotlin
// ✅ 正确：图标用 Material icons 表达选中
DropdownMenuItem(
    text = { Text(option) },
    trailingIcon = {
        if (selected) Icon(Icons.Filled.Check, contentDescription = null)
    },
    onClick = { … },
)

// ❌ 错误：Unicode 字符当图标（禁止）
Text(if (selected) "$option ✓" else option)
```

## 4. 新增 UI 时的检查

1. 任何状态/能力/方向/操作的可视化标记 → `androidx.compose.material.icons`（extended 包已全量引入，无需加依赖）。
2. 评审时全局搜可疑 Unicode（✓ ✗ ● ▲ ▶ ◀ ★ 等）出现在 `Text(...)` 字面量里的情况。
3. 装饰性图标记得 `contentDescription = null`（无障碍语义留给文本）。
