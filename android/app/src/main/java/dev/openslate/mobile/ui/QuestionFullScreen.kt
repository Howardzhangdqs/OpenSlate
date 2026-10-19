package dev.openslate.mobile.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.SideEffect
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.launch

/**
 * ask_user 全屏问卷（NavHost 路由 `question-fullscreen`，竖直滑入 280ms
 * FastOutSlowIn——「压在聊天上的任务」，与二级页横推区分）。
 *
 * 题型控件与内联卡共用（QuestionCard.kt）；草稿挂 UiState，退出即回内联卡
 * 原样。pending 态消失（提交完成 / superseded / expired / cancel 清态）时
 * 自动退出回到流内。横屏内容列限宽 480dp 水平居中。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun QuestionFullScreen(onBack: () -> Unit) {
    val state by RuntimeBridge.state.collectAsState()
    // 渲染用快照：pending 失效（提交完成/superseded/cancel 清态）后保留最后
    // 一帧内容走出场滑出动画，避免退出瞬间白屏。
    var lastActive by remember {
        mutableStateOf<dev.openslate.mobile.bridge.PendingQuestionUi?>(null)
    }
    val live = state.pendingQuestion
    val active = live != null && (live.phase == "pending" || live.phase == "submitting")
    // 快照更新走 SideEffect：随每次成功组合执行，但**不随草稿更新重启协程**
    // （此前 LaunchedEffect(active, live) 以含 drafts 的对象为键，每次勾选/
    // 输入都拆掉重启）；写入经数据类 equals 判重，同值 no-op 不触发重组。
    SideEffect {
        if (active && live != null) lastActive = live
    }
    // 全屏打开期间问卷失效 → 自动退出，回到流内（轻摘要行 / 摘要卡）。
    LaunchedEffect(active) { if (!active) onBack() }

    val p = (if (active) live else lastActive) ?: return

    // 问卷身份键：一切 remember/rememberSaveable/动画状态以 requestId 为键。
    // 草稿/当前页更新只重组（p 参数变化），子树身份稳定——Pager、输入框、
    // 滚动状态跨草稿更新保留；requestId 变化（superseded 顶替等罕见路径）
    // 才整棵重建。
    key(p.requestId) {
        val n = p.questions.size
        val keyboard = LocalSoftwareKeyboardController.current
        val focusManager = LocalFocusManager.current
        val scope = rememberCoroutineScope()
        val pagerState = rememberPagerState(initialPage = p.current.coerceIn(0, n - 1)) { n }

        LaunchedEffect(pagerState) {
            snapshotFlow { pagerState.currentPage }
                .collect { RuntimeBridge.setCurrentQuestion(p.requestId, it) }
        }
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

        val processed = p.questions.indices.map { isQuestionProcessed(p.questions[it], p.drafts[it]) }

        Scaffold(
            topBar = {
                TopAppBar(
                    title = { Text("待回答问题", maxLines = 1, overflow = TextOverflow.Ellipsis) },
                    navigationIcon = {
                        // 顶栏返回与系统返回键/手势同路（popBackStack）。
                        IconButton(onClick = onBack) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "返回")
                        }
                    },
                    actions = {
                        Text(
                            "第 ${p.current + 1} 题，共 $n 题",
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
                    .padding(padding)
                    .imePadding(),
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
                            current = p.current,
                            processed = processed,
                            onSelect = { i ->
                                scope.launch { pagerState.scrollToPage(i) }
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
                    // 题区（全屏不限高；输入框放宽到 minLines 5）。Pager 传
                    // fillMaxSize 吃满 weight 高度：页视口约束确定（有界），
                    // 首帧即组合当前页，不依赖内容反推的包裹高度。
                    AskQuestionPager(
                        p = p,
                        pagerState = pagerState,
                        enabled = !submittingOf(p),
                        minLines = 5,
                        onDraftChange = { idx, d ->
                            RuntimeBridge.updateQuestionDraft(p.requestId, idx) { d }
                        },
                        modifier = Modifier.weight(1f).fillMaxWidth(),
                    )
                    Spacer(Modifier.height(4.dp))
                    // 底部操作栏（同内联 CardFooter，imePadding 随键盘上浮）。
                    AskCardFooter(
                        p = p,
                        enabled = !submittingOf(p),
                        onSubmit = { RuntimeBridge.submitQuestionAnswers(p.requestId) },
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                    )
                }
            }
        }
    }
}

private fun submittingOf(p: dev.openslate.mobile.bridge.PendingQuestionUi): Boolean =
    p.phase == "submitting"
