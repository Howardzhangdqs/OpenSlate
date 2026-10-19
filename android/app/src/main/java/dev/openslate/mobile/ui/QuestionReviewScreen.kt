package dev.openslate.mobile.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.outlined.Swipe
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.PendingQuestionUi
import dev.openslate.mobile.bridge.ReviewedQuestionUi
import dev.openslate.mobile.bridge.RuntimeBridge
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.launch

/**
 * ask_user 已答问卷只读回看（NavHost 路由 `question-review`，转场与
 * `question-fullscreen` 同族：竖直滑入 280ms FastOutSlowIn）。
 *
 * 摘要卡点击 → [RuntimeBridge.openQuestionReview] 装配 [ReviewedQuestionUi]
 * （args 出题目结构 + output 答案快照回填只读草稿）→ 本页以与待答全屏页
 * 相同的视觉语言整卷还原：顶栏（返回 + 计数）+ SegmentBar + 题区横滑
 * Pager（复用 [AskQuestionPager]，`enabled=false` 全控件禁用、`readOnly=true`
 * secret 题出「已存入凭据」引用行）。
 *
 * 只读语义：答案按提交时的状态展示（跳过/未答保持对应提示），底部无
 * 提交/跳过类动作——只有向导式「上一题/下一题」导航（复用 [AskPrimaryButton]
 * 几何）+ 末题/单题位出「关闭问卷」按钮（成功绿 + Check，保留已提交语义、点击关闭）。
 * 投影被清（session_reset）时页内自动 pop；退出经 DisposableEffect 清态，
 * 不留陈旧投影。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun QuestionReviewScreen(onBack: () -> Unit) {
    val state by RuntimeBridge.state.collectAsState()
    val review = state.questionReview
    // 投影被清（session_reset 等清态路径）→ 自动退出，与待答全屏页同款守卫。
    LaunchedEffect(review) { if (review == null) onBack() }
    if (review == null) return
    // 退出即清态：投影是「当前正在查看」的瞬态，不跨场景残留
    //（popExit 出场动画期间组合仍存活、state 未动，动画画面稳定）。
    DisposableEffect(Unit) {
        onDispose { RuntimeBridge.closeQuestionReview() }
    }

    // 问卷身份键：Pager/滚动/hint 等 saveable 状态按投影内容隔离——同问卷
    // 返回再进恢复浏览位置（rememberSaveableStateHolder 跨返回栈保留），
    // 换一份问卷（不同 args 解析结果）整树重建，避免页码越界恢复。
    key(review) {
        val n = review.questions.size
        val keyboard = LocalSoftwareKeyboardController.current
        val focusManager = LocalFocusManager.current
        val scope = rememberCoroutineScope()
        val pagerState = rememberPagerState { n }

        // 切题收起键盘、清焦点（与待答全屏页同款；无 UiState 回写——只读页
        // 当前题以 pagerState 为准，顶栏计数/分段条/页脚直接读它）。
        LaunchedEffect(pagerState) {
            snapshotFlow { pagerState.currentPage }
                .drop(1)
                .collect {
                    keyboard?.hide()
                    focusManager.clearFocus()
                }
        }
        // 滑动提示（一次性）：完成首次滑动即消失。
        var hintDismissed by rememberSaveable { mutableStateOf(n <= 1) }
        LaunchedEffect(pagerState) {
            snapshotFlow { pagerState.currentPage }
                .drop(1)
                .collect { hintDismissed = true }
        }

        val cur = pagerState.currentPage.coerceIn(0, n - 1)
        // 已处理态照实计算：正常提交卷恒全绿（校验在提交口把关），老条目
        // 答案缺题（id 对不上）时对应段回落 outline，不谎报。
        val processed = review.questions.mapIndexed { i, q ->
            isQuestionProcessed(q, review.drafts[i])
        }
        // 只读投影：复用 AskQuestionPager 的全部题区渲染（页高插值/滚动/
        // 渐隐条原样）；requestId 恒 -1 → Pager 的 UiState 反向同步天然 no-op。
        val p = remember(review) {
            PendingQuestionUi(
                requestId = -1L,
                questions = review.questions,
                drafts = review.drafts,
            )
        }

        Scaffold(
            topBar = {
                TopAppBar(
                    title = { Text("已回答的问题", maxLines = 1, overflow = TextOverflow.Ellipsis) },
                    navigationIcon = {
                        // 顶栏返回与系统返回键/手势同路（popBackStack）。
                        IconButton(onClick = onBack) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "返回")
                        }
                    },
                    actions = {
                        Text(
                            "第 ${cur + 1} 题，共 $n 题",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                            modifier = Modifier.padding(end = 16.dp),
                        )
                    },
                    colors = TopAppBarDefaults.topAppBarColors(
                        containerColor = Color.Transparent,
                        scrolledContainerColor = Color.Transparent,
                    ),
                )
            },
            containerColor = MaterialTheme.colorScheme.surface,
        ) { padding ->
            Column(
                Modifier
                    .fillMaxSize()
                    .padding(padding),
            ) {
                Column(
                    Modifier
                        .fillMaxSize()
                        .widthIn(max = 480.dp)
                        .align(Alignment.CenterHorizontally),
                ) {
                    if (n > 1) {
                        SegmentBar(
                            count = n,
                            current = cur,
                            processed = processed,
                            onSelect = { i ->
                                // tween(250ms, FastOutSlowInEasing)：与待答页同曲线。
                                scope.launch {
                                    pagerState.animateScrollToPage(
                                        i,
                                        animationSpec = tween(250, easing = FastOutSlowInEasing),
                                    )
                                }
                            },
                            tall = true,
                            modifier = Modifier.padding(horizontal = 16.dp),
                        )
                    }
                    AnimatedVisibility(
                        visible = !hintDismissed,
                        enter = fadeIn() + expandVertically(),
                        exit = fadeOut() + shrinkVertically(),
                    ) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier.padding(horizontal = 20.dp, vertical = 2.dp),
                        ) {
                            Icon(
                                Icons.Outlined.Swipe,
                                contentDescription = null,
                                modifier = Modifier.size(13.dp),
                                tint = MaterialTheme.colorScheme.outline,
                            )
                            Spacer(Modifier.width(6.dp))
                            Text(
                                "左右滑动切换题目",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                    }
                    // 题区（只读：全控件禁用；secret 出凭据引用行）。布局与
                    // 待答全屏页同款：weight(1f) 吃满剩余高、横向 16dp 缩进。
                    AskQuestionPager(
                        p = p,
                        pagerState = pagerState,
                        enabled = false,
                        readOnly = true,
                        minLines = 5,
                        onDraftChange = { _, _ -> },
                        modifier = Modifier
                            .weight(1f)
                            .fillMaxWidth()
                            .padding(horizontal = 16.dp),
                    )
                    Spacer(Modifier.height(4.dp))
                    // 只读页脚：向导式导航（与待答页主操作行同几何，无提交/
                    // 跳过语义）；末题/单题位出「关闭问卷」按钮替代「提交」位。
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(horizontal = 16.dp, vertical = 8.dp),
                    ) {
                        if (n > 1 && cur > 0) {
                            AskPrimaryButton(
                                text = "上一题",
                                tint = MaterialTheme.colorScheme.primary,
                                enabled = true,
                                onClick = {
                                    scope.launch {
                                        pagerState.animateScrollToPage(
                                            cur - 1,
                                            animationSpec = tween(250, easing = FastOutSlowInEasing),
                                        )
                                    }
                                },
                                modifier = Modifier.weight(1f),
                            )
                        }
                        if (n == 1 || cur == n - 1) {
                            // 末题/单题位出「关闭问卷」（成功绿保留已提交语义，
                            // Check 前导图标；点击与返回键同路 pop 回聊天流）。
                            AskPrimaryButton(
                                text = "关闭问卷",
                                tint = successGreen(),
                                enabled = true,
                                onClick = onBack,
                                modifier = if (n > 1) Modifier.weight(2f) else Modifier.fillMaxWidth(),
                                leading = {
                                    Icon(
                                        Icons.Filled.Check,
                                        contentDescription = null,
                                        modifier = Modifier.size(16.dp),
                                        tint = successGreen(),
                                    )
                                },
                            )
                        } else {
                            AskPrimaryButton(
                                text = "下一题",
                                tint = MaterialTheme.colorScheme.primary,
                                enabled = true,
                                onClick = {
                                    scope.launch {
                                        pagerState.animateScrollToPage(
                                            cur + 1,
                                            animationSpec = tween(250, easing = FastOutSlowInEasing),
                                        )
                                    }
                                },
                                modifier = Modifier.weight(2f),
                            )
                        }
                    }
                }
            }
        }
    }
}

