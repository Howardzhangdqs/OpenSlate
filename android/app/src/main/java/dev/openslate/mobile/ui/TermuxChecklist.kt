package dev.openslate.mobile.ui

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.TermuxExec

/**
 * Termux 连接检测阶梯清单（共享组件）：[TermuxStatus] → 四行
 * [StatusCheckRow]（Termux 已安装 → RUN_COMMAND 权限 → Termux 已运行 →
 * 命令通道应答），经 [StatusChecklist] 渲染——设置页 MCP 五灯清单同款
 * 视觉（通过绿勾 / 失败红叉 / 进行中 spinner / 未轮到空心圆）。
 *
 * 提炼自聊天页 TermuxStallBanner 的私有映射实现（状态/文案/操作行为
 * 逐字保留），消费方 = TermuxStallBanner（stall 悬浮面板——命令卡住时
 * 查 RUN_COMMAND 兜底路径的口径）；composer 聚焦面板子面板已换轨五灯
 * （ui/McpLights.kt，与设置页 MCP 区同口径），不经本组件。
 *
 * 阶梯短路语义 = 前置项通过才轮到当前卡点；未轮到的行保持 Pending
 * 不抢注意力。**标题文案随状态变化**（检测前/中/后可区分，避免 spinner
 * 配成功式标题的歧义）：Pending＝「X · 待检测」、InProgress＝「正在…」、
 * Done＝「已…」、Failed＝「未…」。卡点行随状态出现引导摘要与操作按钮：
 * - 未安装 →「去安装」：Termux 不在 Play 上架，开 F-Droid 详情页
 *   （浏览器兜底；无浏览器静默失败不崩溃）；
 * - 无权限 →「去授权」：优先系统权限弹窗（Activity 上下文可达），非
 *   Activity 上下文兜底直达本应用详情页；
 * - 通道无应答 →「打开 Termux」：拉起 Termux 应用，点击成功后按钮换
 *   「已拉起，等待应答…」防重复点击（pulled remember 无 key：状态翻离
 *   ChannelDown 再回来仍记住已拉起，与提炼源一致；宿主离开组合即重置）。
 *
 * 纯展示组件：探测/轮询由调用方驱动（TermuxStallTracker.refresh +
 * state 订阅），本组件只消费快照里的 [TermuxStatus]。
 */
@Composable
internal fun TermuxStatusChecklist(
    status: TermuxStatus,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    // 「打开 Termux」点击后的轻量反馈：按钮文字切换 + 禁用重复点击。
    var pulled by remember { mutableStateOf(false) }
    // 检查记录行集合：探测阶梯 → 四行状态映射（阶梯短路的语义 = 前置
    // 项通过才轮到当前卡点；未轮到的行保持 Pending 不抢注意力）。
    val rows = buildList {
        add(
            StatusCheckRow(
                state = when (status) {
                    is TermuxStatus.Checking -> StatusCheckState.InProgress
                    is TermuxStatus.NotInstalled -> StatusCheckState.Failed
                    else -> StatusCheckState.Done
                },
                title = when (status) {
                    is TermuxStatus.Checking -> "正在检测 Termux 安装…"
                    is TermuxStatus.NotInstalled -> "Termux 未安装"
                    else -> "Termux 已安装"
                },
                summary = if (status is TermuxStatus.NotInstalled) {
                    "bash 依赖 Termux 执行命令；安装后打开一次再回到本页"
                } else {
                    null
                },
                actionLabel = if (status is TermuxStatus.NotInstalled) "去安装" else null,
                onAction = if (status is TermuxStatus.NotInstalled) {
                    {
                        // Termux 不在 Play 上架：开 F-Droid 详情页（浏览器
                        // 兜底；无浏览器静默失败不崩溃）。
                        runCatching {
                            context.startActivity(
                                Intent(
                                    Intent.ACTION_VIEW,
                                    Uri.parse("https://f-droid.org/en/packages/com.termux/"),
                                ),
                            )
                        }
                    }
                } else {
                    null
                },
            ),
        )
        add(
            StatusCheckRow(
                state = when (status) {
                    is TermuxStatus.Checking, is TermuxStatus.NotInstalled -> StatusCheckState.Pending
                    is TermuxStatus.NoPermission -> StatusCheckState.Failed
                    else -> StatusCheckState.Done
                },
                title = when (status) {
                    is TermuxStatus.NoPermission -> "RUN_COMMAND 权限未授予"
                    is TermuxStatus.Checking, is TermuxStatus.NotInstalled ->
                        "RUN_COMMAND 权限 · 待检测"
                    else -> "RUN_COMMAND 权限已授予"
                },
                summary = if (status is TermuxStatus.NoPermission) {
                    "系统设置 → 应用 → OpenSlate → 权限，或经 Shizuku 一键授予"
                } else {
                    null
                },
                actionLabel = if (status is TermuxStatus.NoPermission) "去授权" else null,
                onAction = if (status is TermuxStatus.NoPermission) {
                    {
                        // 优先系统权限弹窗（Activity 上下文可达，与设置页
                        // McpSection 同款）；非 Activity 上下文兜底直达本
                        // 应用详情页。
                        val activity = context as? Activity
                        if (activity != null) {
                            activity.requestPermissions(
                                arrayOf(TermuxExec.RUN_COMMAND_PERMISSION),
                                1002,
                            )
                        } else {
                            runCatching {
                                context.startActivity(
                                    Intent(
                                        Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                                        Uri.parse("package:${context.packageName}"),
                                    ),
                                )
                            }
                        }
                    }
                } else {
                    null
                },
            ),
        )
        add(
            StatusCheckRow(
                state = when (status) {
                    is TermuxStatus.Checking, is TermuxStatus.NotInstalled,
                    is TermuxStatus.NoPermission,
                    -> StatusCheckState.Pending
                    is TermuxStatus.ChannelDown -> StatusCheckState.InProgress
                    is TermuxStatus.Ready -> StatusCheckState.Done
                },
                title = when (status) {
                    is TermuxStatus.ChannelDown -> "正在等待 Termux 运行…"
                    is TermuxStatus.Ready -> "Termux 已运行"
                    else -> "Termux 运行 · 待检测"
                },
                summary = if (status is TermuxStatus.ChannelDown) {
                    "${status.summary}；打开后回到本页即自动继续"
                } else {
                    null
                },
                actionLabel = if (status is TermuxStatus.ChannelDown) {
                    if (pulled) "已拉起，等待应答…" else "打开 Termux"
                } else {
                    null
                },
                onAction = if (status is TermuxStatus.ChannelDown) {
                    {
                        if (McpHostManager.startTermuxApp(context.applicationContext)) {
                            pulled = true
                        }
                    }
                } else {
                    null
                },
            ),
        )
        add(
            StatusCheckRow(
                state = when (status) {
                    is TermuxStatus.Ready -> StatusCheckState.Done
                    is TermuxStatus.ChannelDown -> StatusCheckState.InProgress
                    else -> StatusCheckState.Pending
                },
                title = when (status) {
                    is TermuxStatus.Ready -> "命令通道应答正常"
                    is TermuxStatus.ChannelDown -> "正在等待命令通道应答…"
                    else -> "命令通道 · 待检测"
                },
            ),
        )
    }
    StatusChecklist(rows = rows, modifier = modifier)
}
