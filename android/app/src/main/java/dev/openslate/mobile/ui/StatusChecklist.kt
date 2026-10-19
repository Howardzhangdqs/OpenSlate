package dev.openslate.mobile.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Cancel
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.outlined.Circle
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * 状态检查清单（可复用组件）：设置页 MCP 控制区阶梯行的提炼——每行 =
 * 状态图标 + 标题（+可选摘要副文本 + 可选操作按钮），整列按行状态呈现
 * 「检查记录」的观感（完成绿勾 / 进行中 spinner / 失败红叉 / 未开始空点）。
 *
 * 视觉规格照搬提炼源（SettingsScreen McpStatusLine + 「当前步骤」行）：
 * - Done/Failed：14×14dp `CheckCircle`（[successGreen]）/ `Cancel`（error
 *   色）+ 「␣␣标题」（labelMedium，两空格前缀与源逐字一致）；
 * - InProgress：16×16dp spinner（stroke 2dp）+ primary 色标题（对齐
 *   McpSection「当前步骤」行的进行中语言）；
 * - Pending：14×14dp 空心圆（outline 色，未开始不抢注意力）；
 * - 行间距 8dp = 设置页 [Section] 内容列的 spacedBy——把整列作为 Section
 *   直接子项放入，行距/顺序与原逐行渲染像素级一致；
 * - 空清单**不组合**（零尺寸子项也会在父 spacedBy 列里引入额外间距，
 *   设置页的渐进显示依赖「未完成的项不占位」语义）。
 *
 * 消费方：设置页 McpSection（五灯）、聊天页 Termux 停滞连接面板。
 */

/** 清单行状态。 */
enum class StatusCheckState {
    /** 完成：绿色实心对勾圆（[successGreen]）。 */
    Done,
    /** 进行中：spinner + primary 色标题。 */
    InProgress,
    /** 失败：error 色实心红叉圆。 */
    Failed,
    /** 未开始：outline 空心圆（不抢注意力）。 */
    Pending,
}

/** 清单行数据：状态 + 标题 + 可选摘要副文本 + 可选操作按钮。 */
data class StatusCheckRow(
    val state: StatusCheckState,
    val title: String,
    /** 标题下的灰色小字说明（labelSmall + outline，同设置页说明行）。 */
    val summary: String? = null,
    /** 操作按钮文字（null = 无按钮）。 */
    val actionLabel: String? = null,
    /** 操作回调（与 [actionLabel] 成对出现）。 */
    val onAction: (() -> Unit)? = null,
)

/** 布尔结果 → Done/Failed 的便捷映射（五灯型阶梯行）。 */
fun statusCheckOf(ok: Boolean): StatusCheckState =
    if (ok) StatusCheckState.Done else StatusCheckState.Failed

/**
 * 渲染状态检查清单：行自上而下、间距 8dp；操作按钮（SoftButton）靠行尾，
 * 摘要副文本在标题下（与标题同缩进）。空清单不组合（见文件文档）。
 */
@Composable
fun StatusChecklist(
    rows: List<StatusCheckRow>,
    modifier: Modifier = Modifier,
) {
    if (rows.isEmpty()) return
    Column(modifier = modifier, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        for (row in rows) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                when (row.state) {
                    StatusCheckState.Done -> Icon(
                        imageVector = Icons.Filled.CheckCircle,
                        contentDescription = "已就绪",
                        tint = successGreen(),
                        modifier = Modifier.height(14.dp).width(14.dp),
                    )
                    StatusCheckState.Failed -> Icon(
                        imageVector = Icons.Filled.Cancel,
                        contentDescription = "未就绪",
                        tint = MaterialTheme.colorScheme.error,
                        modifier = Modifier.height(14.dp).width(14.dp),
                    )
                    StatusCheckState.InProgress -> CircularProgressIndicator(
                        modifier = Modifier.height(16.dp).width(16.dp),
                        strokeWidth = 2.dp,
                    )
                    StatusCheckState.Pending -> Icon(
                        imageVector = Icons.Outlined.Circle,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.height(14.dp).width(14.dp),
                    )
                }
                // 标题色：进行中 = primary（对齐 McpSection「当前步骤」行）；
                // 其余 = 默认内容色（与提炼源 McpStatusLine 一致）。
                val titleColor = if (row.state == StatusCheckState.InProgress) {
                    MaterialTheme.colorScheme.primary
                } else {
                    LocalContentColor.current
                }
                if (row.summary == null) {
                    // 两空格前缀与 McpStatusLine 逐字一致（图标与文字的间距
                    // 在源实现里就是字符串前导空格，保持像素级一致）。
                    Text(
                        "  ${row.title}",
                        style = MaterialTheme.typography.labelMedium,
                        color = titleColor,
                    )
                } else {
                    Column {
                        Text(
                            "  ${row.title}",
                            style = MaterialTheme.typography.labelMedium,
                            color = titleColor,
                        )
                        Text(
                            "  ${row.summary}",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                }
                if (row.actionLabel != null && row.onAction != null) {
                    Spacer(Modifier.width(8.dp))
                    SoftButton(onClick = row.onAction) {
                        Text(row.actionLabel, style = MaterialTheme.typography.labelMedium)
                    }
                }
            }
        }
    }
}
