package dev.openslate.mobile.ui

import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import dev.openslate.mobile.R

/**
 * 内嵌等宽字体（JetBrains Mono 子集；latin/数字/常用符号，中文等缺失
 * 字形自动回退系统字体）。**仅用于标识类短文本**：工具名、模型名 /
 * registry 键、命令 / 路径 / 配置片段等。任意代码块与正文（Markdown
 * 代码块、工具输出、日志体）需全字形覆盖，请用系统
 * [FontFamily.Monospace]，不得改用本子集。
 */
internal val MonoFont = FontFamily(Font(R.font.jetbrains_mono_subset))
