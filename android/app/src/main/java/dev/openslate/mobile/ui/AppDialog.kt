package dev.openslate.mobile.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.BasicAlertDialog
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * 自定义对话框骨架：与 M3 [androidx.compose.material3.AlertDialog] 同款
 * 外观（28dp 圆角、surfaceContainerHigh 底、headlineSmall 标题），但
 * 布局完全自控——原生 AlertDialog 的内容槽/操作槽分层内边距会让
 * "内容区里"的按钮行距卡片底部过远（text 底距 + 空操作区 + 卡片底
 * 距三层叠加），本骨架统一收紧为底部 12dp。
 *
 * 适用：按钮行在内容区末尾的弹窗（如 Provider 编辑、模型管理）。
 * 按钮在原生 confirmButton 槽位的弹窗继续用 AlertDialog 即可。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AppDialog(
    onDismiss: () -> Unit,
    title: String,
    modifier: Modifier = Modifier,
    content: @Composable ColumnScope.() -> Unit,
) {
    BasicAlertDialog(onDismissRequest = onDismiss, modifier = modifier) {
        Surface(
            shape = RoundedCornerShape(28.dp),
            color = MaterialTheme.colorScheme.surfaceContainerHigh,
        ) {
            Column(
                Modifier.padding(start = 24.dp, top = 24.dp, end = 24.dp, bottom = 12.dp),
                verticalArrangement = androidx.compose.foundation.layout.Arrangement.spacedBy(12.dp),
            ) {
                Text(title, style = MaterialTheme.typography.headlineSmall)
                content()
            }
        }
    }
}
