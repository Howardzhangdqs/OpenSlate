# ask_user 移动端 UI 设计规范（design-only）

> 状态：设计稿，供评审；不含实现代码。
> 依据：`docs/proposals/ask-user-tool-design.md`（r5）+ 现有 UI 基线源码（`ChatScreen.kt`、`AppDialog.kt`、`SoftButton.kt`、`SettingsScreen.kt`、`ThemePrefs.kt`、`StatusColors.kt`、`MainActivity.kt`）。
> 交互骨架已拍板（本稿不更改）：对话流内联问卷卡片；卡片顶部一排分段线段（每段一题，区分当前/已答/未答），点分段切题、题在卡片内作答；卡片提供全屏按钮，全屏内作答，退出回到内联卡，进度保留。
> 硬规则：图标一律 Material Design icons（`androidx.compose.material.icons`，项目已引 `material-icons-extended`），**禁止 Unicode 字符当图标**。

---

## 1. 现有视觉语言摘要（延续什么）

从代码里读到的基线，问卷 UI 全部沿用，不新造语言：

| 维度 | 现状 | 问卷 UI 的用法 |
|---|---|---|
| 配色 | M3 冷蓝灰中性面（浅 `surface #F6FAFE` / 深 `#0F1417`，container 阶梯齐全）+ 7 套可切换强调色（仅 primary 族跟随） | 卡片、输入框、选项全走 `colorScheme` 角色，不写死颜色；「选中/主操作」用 primary 族 → 自动跟随主题色 |
| 语义色 | 成功绿 `StatusColors.successGreen/successGreenContainer`（深浅两套）；失败 `error`；执行中 `secondary`；元信息/思考 `tertiary` | 已答 = 成功绿；校验错误 = error；等待中 = primary；终态摘要 = outline 中性 |
| 形状 | 气泡 16dp（尾角 4dp）、工具组 12dp、AppDialog 28dp、SoftButton 10dp、chip 全圆角 50 | 内联问卷卡 16dp（气泡同族）；内部选项行/按钮 10dp（SoftButton 同族）；徽章/chip 全圆角 |
| 边框 | 发丝线 0.5dp + 语义色 40% alpha（工具组 `secondary@40%`、玻璃面板 `outline@40%`） | 待答卡 0.5dp primary 边框（区别于工具组的 secondary），已答后转 `secondary@40%` 归入转写语言 |
| 按钮 | `SoftButton`：语义色 12% 底 + 同色文字、10dp 圆角、最小高 34dp、labelLarge；破坏性传 `error` | 所有次要动作（跳过/不答/不回答）= SoftButton 或纯文字按钮；提交 = SoftButton 放大变体（主操作、下半屏） |
| chip/pill 语言 | `tertiaryContainer@35%` 底 + 0.5dp `tertiary@40%` 边、13dp 图标 + labelSmall（「思考中」chip、思维链 pill）；工具 chip 行高紧凑 | 题目标签 chip、答案摘要 chip、推荐徽章、终态摘要行全走这套比例（13dp 图标 + labelSmall/labelMedium） |
| 玻璃 | `GlassPanel` 磨砂（API 31+ 真模糊，12dp 半径）用于顶栏/输入胶囊/悬浮横幅 | 待答提醒 pill（悬浮在输入区上方）复用 TermuxStallBanner 的玻璃语言；问卷卡本体不玻璃（内容卡片要实底，可读性优先） |
| 排版 | M3 默认字阶；`MonoFont`（JetBrains Mono 子集）只用于标识类短文本 | 环境变量名、文件路径、工具名用 MonoFont；题干 bodyLarge、选项主文 bodyMedium、说明 bodySmall |
| 图标 | 全 Material icons，行内小图标 11–16dp | 见附录 B 清单 |
| 布局 | LazyColumn 全宽、条目横向 4dp padding、`spacedBy(6dp)`；气泡全宽非左右分栏 | 问卷卡作为列表条目全宽展开，与气泡对齐 |
| 动效 | 280ms `FastOutSlowInEasing` 家族（页面转场、sharedBounds）；`expandVertically+fadeIn` 入场（StallBanner）；`animateContentSize`（思维链） | 全部动效沿用这三件套，时长不超出 280ms 家族 |

**明确的反例（不照搬）**：`SettingsScreen.Toggle`（L1788）用 `● / ○` 字符表达选中态，违反 AGENTS.md 图标规范，属遗留代码——问卷 UI 的多选一律用 `Checkbox`（项目 `CheckboxRow` 已注明方形角以区分单选语义）、单选用 `RadioButton`、选中标记用 `Icons.Filled.Check`。

---

## 2. 组件分解

新文件 `ui/QuestionCard.kt`（内联卡 + 各题型控件 + 分段条），全屏界面新文件 `ui/QuestionFullScreen.kt`。逻辑复用层不新建，答案草稿挂 `RuntimeBridge` 的 UiState（见 §7 草稿保留）。

```
QuestionCard（内联，聊天流条目）
├─ SegmentBar ............................ 分段指示器（1 题时隐藏）
├─ QuestionHeader ........................ 标签 chip + 题干 + 等待指示
├─ QuestionBody（每题型一节，装在横滑 Pager 里）
│  ├─ ChoiceGroup ........................ 单选/多选选项行 + 「其他」兜底行
│  │   └─ OptionRow ...................... RadioButton/Checkbox + label + description + 推荐徽章
│  ├─ TextInputSection ................... 多行输入框 + placeholder
│  ├─ ConfirmPair ........................ 两枚正反按钮
│  ├─ SecretInputSection ................. 掩码输入 + 可见切换 + 记住勾选 + 落地形态说明
│  └─ QuestionMinorActions ............... 跳过（optional）/ 此题不答
├─ CardFooter ............................ 「不回答」（整卡 decline）+ 提交按钮 + 必填校验提示
└─ SummaryCard（answered 折叠态，同一容器变形）
   └─ AnswerChip × N ..................... 每题一枚答案 chip
TerminalLine ............................. declined/cancelled/superseded/expired 轻摘要（非卡片）
PendingNudgePill ......................... 待答提醒 pill（输入区上方悬浮，GlassPanel 语言）
QuestionFullScreen（全屏问卷）
├─ 顶栏（返回 + 标题 + 「第 n/N 题」进度）
├─ SegmentBar（加高版）
├─ QuestionBody（复用内联同款控件）
└─ 底部操作栏（同 CardFooter，imePadding 上浮）
```

要点：

- **QuestionCard 是 `ask_user` 工具调用的特化渲染**：`groupEntries` 对 `name == "ask_user"` 的 ToolCall 条目单独成条、由 QuestionCard 吸收，避免列表里同时出现通用工具 chip 和问卷卡两张皮。转写（argsPreview/output）仍按工具设计稿 §3.5 双写，摘要态与转写内容一致。
- **SegmentBar 用自绘线段，不用 M3 SegmentedButton**——后者是互斥按钮组语义（带选中容器、文字），这里是进度指示 + 快速跳转，视觉上应该是「线」不是「按钮」。
- **选项行用 foundation 组合（clip + background + clickable）自控行高**，理由同 `ToolChipRow` 注释：M3 组件强制最小触摸尺寸会把行撑高、组内出现大空隙。问卷选项行刻意定 48dp 最小高（工具 chip 是紧凑只读行，这里是可点输入行，标准不同）。

---

## 3. 关键屏与状态线框图

> 左开右开式框线，避开中英混排宽度对齐问题；（括注）为图标名，实机用 Material icons 渲染。

### 3.1 内联待答卡（多题，当前题 = choice 单选）

```
╭───────────────────────────────────────────────
│  ▂▂▂▂▂   ▂▂▂▂▂   ▁▁▁▁▁   ▁▁▁▁▁        (OpenInFull)
│  └当前·主色   └已答·成功绿      └未答·outline 25%
│
│  (数据库)                        ◌ 等待回答
│  └标签 chip：secondaryContainer@40%   └13dp spinner + labelSmall primary
│  本地存储用 SQLite 还是 sled？
│  └题干 bodyLarge
│
│  ◉  SQLite                [推荐]
│  │                        └徽章：Recommend 图标 12dp + labelSmall
│  │                           primaryContainer 底 + onPrimaryContainer 字
│  │  零配置，移动端生态默认
│  │  └bodySmall outline
│  ○  sled
│  │  纯 Rust，写性能好
│  ○  其他
│  ┌─────────────────────────────────────
│  │ 选其他时请说明                ← 选中「其他」才展开的输入框
│  └─────────────────────────────────────
│
│  此题不答                                 不回答
│  └逐题 decline，labelMedium          └整卡 decline，labelMedium
│                                       outline 色，点后有二次确认（见 §4）
│  ┌─────────────────────────────────────
│  │               提交                  ← 主按钮：SoftButton 放大变体
│  └─────────────────────────────────────   全宽、最小高 48dp、primary
│                                           必填未答 → 禁用态 + 右侧
│                                           「还有 1 题未作答」labelSmall
╰───────────────────────────────────────────────
```

- 选项行：最小高 48dp、10dp 圆角、横向 10dp 内边距；选中行底色 `primaryContainer@35%`（chip 同比例），未选中透明。
- 卡片整体：16dp 圆角、`surfaceContainerLowest` 底、0.5dp primary 边框带呼吸动画（§4）。

### 3.2 分段指示器（SegmentBar）解剖

```
   题1(已答)    题2(当前·未答)   题3(未答)    题4(未答)
  ━━━━━━━      ━━━━━━━         ─────        ─────
  成功绿        主色·加高        outline      outline
  高4dp         高6dp           25% alpha    25% alpha

  规则：颜色表达「是否已处理」，高度表达「当前在哪题」。
  每段宽度 = 剩余宽等分（weight），视觉线 4–6dp，
  命中区整行 ≥44dp 高（线段上下各留透明内边距）。
```

### 3.3 各题型（题区特写）

text：
```
  (命名)
  服务名用什么？
  ┌─────────────────────────────────────
  │ my-agent-service              ← 多行输入框 minLines 3，placeholder
  │
  └─────────────────────────────────────
```

confirm（二值 → 两枚按钮，非选项行）：
```
  (测试)
  要顺带跑一遍测试吗？
  ┌──────────────┐   ┌──────────────┐
  │ (Check) 是    │   │ (Close) 否    │   ← 各占一半，最小高 48dp
  └──────────────┘   └──────────────┘     正面 = primary tint
     选中：tint 12% 底 + 1dp 同色边        反面 = secondary tint
     未选中：中性底 + 0.5dp outline 边
```

secret：
```
  (凭据)
  请提供 GitHub 访问令牌：
  ┌─────────────────────────────────────
  │ •••••••••••••••         (Visibility) ← 掩码输入，单行
  └─────────────────────────────────────     密码键盘；图标切换明文
  ☑ 记住此凭据                                （VisibilityOff）
  │ 勾选后以后会话也能用；默认仅本会话有效
  │ └CheckboxRow 基线：Checkbox + bodyMedium + bodySmall outline 副文本
  ┌─────────────────────────────────────
  │ (Terminal) GITHUB_TOKEN · 环境变量     ← 落地形态行（来自工具参数
  └─────────────────────────────────────     storage），变量名/路径用
                                             MonoFont；file 落地则用
  明文不会出现在对话和日志里                    (Description) + 路径
  └bodySmall outline
```

多选（`multi_select: true`）：
```
  (范围)
  要包含哪些模块？（可多选）
  ☑ tests        回归测试脚本      ← Checkbox 方形角，与单选区分
  ☐ docs         使用文档
  ☐ 示例工程      examples/ 目录
  ☐ 其他
  ┌─────────────────────────────────────
  │ 补充说明                        ← 选中「其他」展开
  └─────────────────────────────────────
```

optional 题与逐题 decline（题区底部次要操作行）：
```
  跳过            此题不答
  └仅 optional 显示   └多题时显示；单题卡隐藏（与「不回答」重复）
```

### 3.4 全屏问卷态

```
┌───────────────────────────────────────────────
│ (ArrowBack)  待回答问题                  2 / 4  ← 顶栏：返回 + 标题
│                                                + labelSmall 进度
│  ━━━━━━   ━━━━━━   ──────   ──────             ← 分段条加高版（6/8dp）
│
│  (范围)
│  要包含哪些模块？（可多选）
│
│  ☑ tests        回归测试脚本
│  ☐ docs         使用文档
│  ☐ 示例工程      examples/ 目录
│  ☐ 其他
│  ┌─────────────────────────────────────
│  │ 补充说明                             ← 全屏输入 minLines 5
│  └─────────────────────────────────────
│
│  （首次进入显示一次：「左右滑动切换题目」，完成首次滑动即消失）
│
│  跳过 · 此题不答                          不回答
│ ┌─────────────────────────────────────
│ │               提交                   ← 底部操作栏固定，imePadding
│ └─────────────────────────────────────     随键盘上浮
└───────────────────────────────────────────────
```

- 题与题之间横向滑动切换（与点分段等效）；单题时无滑动。
- 退出全屏 = 顶栏返回 / 系统返回手势，两者同路；草稿不丢。

### 3.5 已答摘要态（answered 折叠）

```
╭───────────────────────────────────────────────
│ (Check) 已回答 · 3 题                          ← 成功绿 14dp 图标 + labelMedium
│
│ (数据库: SQLite) (范围: tests、docs) (命名: my-agent-service)
│ └每题一枚 chip：50% 全圆角、surfaceContainerHigh 底、
│  labelSmall；长答案省略号截断，点卡片弹出与工具详情同款的
│  完整答案弹窗（复用 ToolDetailDialog 的容器语言）
│
│ (Check) 已提交，Agent 正在继续                  ← 提交成功后 4s 内显示，
╰───────────────────────────────────────────────    随后淡出、卡片收矮
```

secret 题的答案 chip 特例：`(Key) GITHUB_TOKEN · 已存入凭据`（成功绿 container 底），**永不出现明文**。

### 3.6 终态轻摘要（declined / cancelled / superseded / expired）

不再是卡片，一行轻摘要（与审批转写行 `Approval` 同款排版：labelMedium + 13dp 图标 + 横向 8dp）：

```
  (Block)      未作答 —— 已告知 Agent 自行判断继续
  (Stop)       提问已取消
  (SwapHoriz)  提问已被新消息取代
  (Schedule)   提问已过期
```

---

## 4. 交互与动效规范

### 4.1 出入场与状态变形

| 时机 | 动效 | 参数 |
|---|---|---|
| 卡片入场（pending 出现） | `expandVertically(expandFrom = Bottom)` + `fadeIn`，与 TermuxStallBanner 同款 | 220ms FastOutSlowIn；入场后列表自动滚到卡片（沿用现有贴底滚动逻辑） |
| pending → submitting → answered | 容器 `animateContentSize` + 内容 `AnimatedContent` 交叉淡替 | 各 200ms；卡片高度平滑收矮，不跳变 |
| 提交成功反馈 | 「已提交，Agent 正在继续」行淡入，4s 后淡出并收矮 | 4s 计时从 resolve 成功起算；期间用户滚走不打断 |
| 卡片消失（cancelled 等终态） | 卡片收矮为终态摘要行（同上 animateContentSize 路径） | 200ms |

### 4.2 等待动画（pending 专属）

- 卡片边框呼吸：0.5dp primary 边框，alpha 在 0.25 ↔ 0.65 间往复，2000ms `tween` + `RepeatMode.Reverse`。克制、不抢内容；提交后立即停。
- 头部右侧：13dp / 1.5dp 描边 spinner（与工具 chip 行「执行中」同款）+ 「等待回答」labelSmall primary。
- 二者都是 pending 态标记；实现时跟随系统动画时长缩放（`Settings.Global.ANIMATOR_DURATION_SCALE`），归零即静止显示终帧。

### 4.3 切题交互

- **点分段**：直接跳到该题，线段加高指示随 `animateDpAsState` 平移（150ms）。
- **横向滑动**：题区用横滑 Pager（题与题水平排列），手势与聊天列表的纵向滚动正交、不冲突；滑动切换与点分段完全等效，分段高亮跟随 Pager 页。滑到边界有正常 overscroll，不循环。
- 切题时：收起键盘、清输入焦点（避免键盘横跨两题）；草稿即时保留（见 §7）。
- 切题内容过渡：Pager 自带位移，不再叠加额外动画。

### 4.4 全屏转场

- 进入：路由级转场 `slideInVertically`（自底部整屏滑入）+ `fadeIn`，280ms FastOutSlowIn——沿用现有页面转场时长家族，但用竖直方向与二级页的横推区分开（全屏问卷是「压在聊天上的任务」，不是「推入的层级页」）。
- 退出（顶栏返回 / 系统返回）：反向滑出 + 淡出，280ms。
- 内联卡在全屏期间保持组合（仅在屏幕外），返回即见原样——草稿天然不丢。
- 逐题布局与内联共用同一套题型控件（全屏只放宽输入框行数）；进度指示 = 顶栏「第 n / N 题」+ 加高分段条。

### 4.5 提交与必填校验

- 有效作答定义：choice ≥1 选中（选「其他」时说明文字非空）；text 非空白；confirm 已选定一侧；secret 非空白；跳过 / 逐题不答视为「已处理」。
- 全部题目「已处理」→ 提交可用；否则提交禁用（SoftButton 禁用态自带 5% 底 / 45% 字）+ 按钮上方右侧 labelSmall outline：「还有 N 题未作答」。
- 提交中：按钮进入禁用 + 13dp spinner + 文案「提交中」，防重复点击（host call 幂等首答赢，UI 层仍要防抖）。
- 「不回答」（整卡 decline）点击后弹轻确认：复用 `AppDialog` 骨架，标题「不回答这组问题？」正文「Agent 会自行判断并继续，这组问题不再等待你作答。」按钮「继续回答 / 不回答」（error tint 用于确认键）。防误触，因为该动作整卡不可逆。

### 4.6 键盘与单手

- 内联卡在聊天列表内，键盘弹起时列表视口经根布局既有 `imePadding` 收缩；文本框聚焦时把当前卡片滚进视口（复用现有 `animateScrollToItem` 贴底逻辑）。
- 全屏页根布局 `imePadding`，底部操作栏随键盘上浮，输入框永不被遮。
- 主操作（提交）全宽置于卡片/页面最底部——拇指热区；次要操作（跳过/此题不答/不回答）在其上一行。
- 所有可点行最小高 48dp；分段条命中区 ≥44dp；「不回答」等文字按钮的实际命中区经内边距补足到 44dp。

---

## 5. 状态机

### 5.1 卡片级（问卷整体）

```
                ┌──「不回答」（确认后）──────▶ declined（终态轻摘要行）
                │
 pending ───────┼── 提交 ──▶ submitting ──成功──▶ answered（折叠摘要卡）
（呼吸边框+spinner）           （按钮 spinner）        │ 4s 反馈后收矮
                │
                ├── 用户点停止（现有 Cancel 通道）──▶ cancelled（轻摘要行）
                ├── 新 turn 开始前仍未答（合成）────▶ superseded（轻摘要行）
                └── TTL 24h 过期 ─────────────────▶ expired（轻摘要行）
```

- 所有终态不可逆；迟到答案由协议层保证只对 pending 问卷有效（首答赢）。
- 全屏打开期间发生 superseded/expired：全屏自动退出，回到流内轻摘要行。

### 5.2 题级（草稿子状态，存在于 pending 期间）

```
 未答 ──选择/输入──▶ 已草答（可反复修改，分段变绿）
   ├─「跳过」（仅 optional）──▶ 已跳过（分段变绿）
   ├─「此题不答」（仅多题）──▶ 逐题 decline（分段变绿）
   └─ 提交时必填未答 ──▶ 提交禁用 + 「还有 N 题未作答」
```

### 5.3 卡片外观随状态速查

| 状态 | 容器 | 边框 | 头部指示 | 内容 |
|---|---|---|---|---|
| pending | 16dp 卡，`surfaceContainerLowest` 底 | 0.5dp primary 呼吸 | spinner + 「等待回答」 | 完整问卷 |
| submitting | 同上，边框停帧 | primary 静态 | 「提交中」+ spinner | 控件禁用 |
| answered | 同容器收矮 | 0.5dp `secondary@40%`（归入工具组语言） | `Check` + 「已回答 · N 题」 | 答案 chips |
| declined/cancelled/superseded/expired | 无卡 | 无 | 13dp 图标 + labelMedium 一行 | — |

---

## 6. 边界情况与实现提示

| 情况 | 设计决定 | 实现提示（映射现有基线，非代码） |
|---|---|---|
| 只有 1 题 | 隐藏分段条（头部右侧保留全屏按钮）；无滑动；「此题不答」隐藏（与「不回答」重复） | SegmentBar 以题数 >1 为显示条件 |
| 4 题满配 | 分段每段约 70dp 宽（360dp 屏），仍可点；题区横滑；内联卡题区最大高约可用高度的 60%，超出则题区内部滚动 + 底缘渐隐 + 头部全屏按钮描边加重（引导全屏） | Pager + 高度约束；渐隐用 `drawWithContent` 渐变（ChatScreen 已有同类离屏渐隐手法） |
| 选项 description 很长 | 不限行数、完整换行显示；内联由题区滚动兜底，全屏无限制；不折叠（问答回归「读完再选」，折叠伤决策） | OptionRow 用 Column 自然撑高，行最小高 48dp 只当下限 |
| 深浅色 / 7 主题色 | 全部走 `colorScheme` 角色 + `StatusColors`，零写死色；选中态用 primary 族自动跟随强调色 | 深色适配已被主题承担；绿已由 StatusColors 双套 |
| 横屏 | 内联卡随列表全宽（变宽可读性不劣化）；全屏页内容列限宽 480dp 水平居中，避免选项行被拉过长 | 全屏内容容器 `widthIn(max = 480.dp)` 居中 |
| 字体缩放（fontScale >1） | 行高用「最小 48dp + 自适应撑高」，不锁死高度（Composer 输入条的锚高手法不适用于内容卡） | `heightIn(min = 48.dp)` 而非 `height` |
| 键盘遮挡 | §4.6 已定；内联靠列表滚动 + 根 `imePadding`，全屏靠页内 `imePadding` | 不新增链路，复用 ChatScreen 现有机制 |
| 草稿保留 | 以 `requestId` 为键存于 UiState 层（非 composable 局部状态）：切题、进出全屏、卡片重组、列表滚动回收都不丢；进程被杀后已提交答案走持久化恢复（r4/r5 语义），**未提交草稿不持久化**（可接受，重启后空草稿重答） | UiState 加 pending 草稿表；QuestionCard 无状态渲染 |
| pending 时用户滚走 | 输入胶囊上方浮出提醒 pill：「等待你的回答 · N 题」，点击滚动回卡片；卡片回到可视区即收 | 复用 TermuxStallBanner 槽位与 GlassPanel；可见性由 LazyListState 可视范围判断 |
| 未知题型（payload `v` 前向兼容） | 未知 `type` 按 text 渲染兜底（题干 + 输入框），不崩、不吞题 | 渲染层 `else` 分支 |
| 与转写的一致性 | ask_user 的 ToolCall 条目被 QuestionCard 吸收，不另渲染工具 chip；summary 内容与 output 转写同源 | `groupEntries` 特判 + UiEntry 不变 |
| App 重启恢复 | pending 回放后卡片即 pending 态原样；waiting 呼吸动画重启 | 走 `SnapshotDto.pending_questions`，UI 无额外逻辑 |
| 超时（S2，本稿仅预留） | 默认永不超时；配置阈值后超时**不关卡**：头部指示换「已超时，仍可补答」（outline 色），状态仍 pending | 头部指示位预留第三种文案 |

---

## 附录 A · 文案清单（简体中文，待 orchestrator 复审微调）

| 位置 | 文案 |
|---|---|
| 头部等待指示 | 等待回答 |
| 提交按钮 / 提交中 | 提交 / 提交中 |
| 提交成功反馈 | 已提交，Agent 正在继续 |
| 摘要头部 | 已回答 · N 题 |
| 次要操作 | 跳过 / 此题不答 / 不回答 |
| decline 确认弹窗 | 不回答这组问题？／ Agent 会自行判断并继续，这组问题不再等待你作答。／ 继续回答 ／ 不回答 |
| 必填校验 | 还有 N 题未作答 |
| 「其他」兜底 | 其他 ／（placeholder，取工具参数或默认）请补充说明 |
| 其他选中但未填 | 请填写说明，或取消选择「其他」 |
| 全屏标题 / 进度 | 待回答问题 ／ 第 n 题，共 N 题 |
| 滑动提示（一次性） | 左右滑动切换题目 |
| secret：记住勾选 | 记住此凭据 ／ 勾选后以后会话也能用；默认仅本会话有效 |
| secret：落地形态 | 环境变量 · 本会话的命令自动生效 ／ 写入文件 |
| secret：安全说明 | 明文不会出现在对话和日志里 |
| secret：答案 chip | 已存入凭据 |
| 终态行 | 未作答 —— 已告知 Agent 自行判断继续 ／ 提问已取消 ／ 提问已被新消息取代 ／ 提问已过期 |
| 提醒 pill | 等待你的回答 · N 题 |
| 超时预留（S2） | 已超时，仍可补答 |

## 附录 B · 图标清单（全部 Material icons，禁 Unicode）

| 语义 | 图标 |
|---|---|
| 问卷标识 / 提醒 pill / 全屏标题旁 | `Icons.Outlined.HelpOutline` |
| 全屏进入 / （无需退出图标，退出走返回） | `Icons.Filled.OpenInFull` |
| 全屏返回 | `Icons.AutoMirrored.Filled.ArrowBack` |
| 滑动提示（全屏一次性提示） | `Icons.Outlined.Swipe` |
| 已答 / 已提交 / confirm 正面 | `Icons.Filled.Check` |
| confirm 反面 | `Icons.Filled.Close` |
| 推荐徽章 | `Icons.Outlined.Recommend` |
| secret 可见切换 | `Icons.Outlined.Visibility` / `Icons.Outlined.VisibilityOff` |
| 凭据 chip | `Icons.Outlined.Key` |
| storage=env 落地形态 | `Icons.Outlined.Terminal`（项目既有基线图标） |
| storage=file 落地形态 | `Icons.Outlined.Description` |
| 校验错误（如有图标位） | `Icons.Filled.ErrorOutline` |
| declined 终态 | `Icons.Outlined.Block` |
| cancelled 终态 | `Icons.Filled.Stop`（与输入区停止键同源） |
| superseded 终态 | `Icons.Outlined.SwapHoriz` |
| expired 终态 | `Icons.Outlined.Schedule` |
| 超时预留（S2） | `Icons.Outlined.TimerOff` |
| 等待 / 提交中 | `CircularProgressIndicator`（13dp、1.5dp 描边，非图标字体） |
