# 06-mobile · android/ 工程（Gradle / Manifest / Kotlin）

> 面向接手本项目的 AI agent。本章覆盖 `android/` 宿主工程。构建/装包/日志操作见 [../07-build-dev/android-build.md](../07-build-dev/android-build.md)；网络前置（镜像 / 本地应答器）见 [../07-build-dev/troubleshooting.md](../07-build-dev/troubleshooting.md) 与 `android/README.md`。

## 1. 总览

单模块 Compose 应用：`MainActivity`（NavHost 五路由）+ `AgentService`（前台服务持有 Rust runtime 生命周期）+ `RuntimeBridge`（object 单例，事件管线与全部公开操作）+ `bridge/` 其余宿主能力（Keystore / Termux / MCP host / Shizuku / 日志导出）+ `ui/` Compose 页面。Rust 侧经 UniFFI/JNA 加载 `libopenslate_mobile.so`（jniLibs 打包，见 [mobile-ffi.md](mobile-ffi.md)）。

## 2. Gradle 三件套

### 2.1 `android/settings.gradle.kts`

- `rootProject.name = "OpenSlateMobile"`，仅 `include(":app")`。
- 仓库全部走 **aliyun 镜像**（`maven.aliyun.com/repository/gradle-plugin|google|public`）：dl.google.com / repo.maven.org 直连不可达，且 **Gradle JVM 不认 http_proxy 环境变量**，直连仓库会挂死。
- `repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)`——模块里不许再私加仓库。
- 网络前置：本机有 systemd 服务 `openslate-responder` 拦截 AGP 对 dl.google.com addons 列表的拉取（`/etc/hosts` 指向 127.0.0.1 + 本地 HTTPS 空仓库），详见 `android/README.md`「网络前置」。

### 2.2 `android/gradle/libs.versions.toml`（版本目录）

| 键 | 版本 |
|----|------|
| AGP | **9.4.0** |
| Kotlin | **2.4.20** |
| coreKtx | 1.17.0 |
| lifecycle | 2.9.4 |
| activityCompose | 1.11.0 |
| **Compose BOM** | **2026.09.00** |
| navigation | 2.9.5 |
| coroutines | 1.10.2 |

注意：**AGP 9 内置 Kotlin**（`app/build.gradle.kts` 不应用 kotlin-android 插件，只应用 `org.jetbrains.kotlin.plugin.compose`）。

### 2.3 `android/app/build.gradle.kts`

- `namespace = "dev.openslate.mobile"`、`applicationId = "dev.openslate.mobile"`。
- **compileSdk 37 / minSdk 26 / targetSdk 36**（版本区间语义见 [../08-conventions/compat.md](../08-conventions/compat.md)）；`versionCode 1` / `versionName "0.1.0"`。
- `ndkVersion = "21.4.7075529"`；`abiFilters += listOf("arm64-v8a")`（Phase 1 只带 arm64，真机即 arm64）。
- Java/JVM 17（compileOptions + kotlin compilerOptions）。
- `buildConfig = true`：AGP 8+ 默认关 BuildConfig 生成，显式开启——`BuildConfig.DEBUG` 门控内容型日志（release 不剥离 logcat，防对话内容泄漏）。
- `packaging { jniLibs { useLegacyPackaging = false } }`：.so 走未压缩对齐装载（配合 16KB 页对齐，见 [../07-build-dev/toolchain.md](../07-build-dev/toolchain.md)）。
- release：`isMinifyEnabled = false`，**用 debug keystore 签名**（本地迭代免卸载、与 debug 包互相覆盖安装不丢数据；**正式发布前必须换**，注释在文件里）。
- 关键依赖：Compose material3 + **material-icons-extended**（UI 图标唯一来源，见 [../08-conventions/ui.md](../08-conventions/ui.md)）、navigation-compose、mikepenz multiplatform-markdown-renderer(-m3) 0.45.0（模型回复渲染）、**JNA 5.17.0@aar**（UniFFI 生成代码依赖 `com.sun.jna.*`）、Shizuku api/provider 13.1.5（一键授权 Termux RUN_COMMAND）、lifecycle-service（AgentService 用 LifecycleService）。

## 3. Manifest（`android/app/src/main/AndroidManifest.xml`）

- 权限：`INTERNET`、`POST_NOTIFICATIONS`、`FOREGROUND_SERVICE`、`FOREGROUND_SERVICE_SPECIAL_USE`、`com.termux.permission.RUN_COMMAND`（dangerous 运行时权限，系统设置或经 Shizuku 授予；Termux 未装时无副作用）。
- `<queries>`：`com.termux`、`moe.shizuku.privileged.api`（Android 11+ 包可见性，探测 Termux 是否安装做前置提示）。
- `application android:name=".OpenSlateApp"`、`android:allowBackup="false"`。
- `MainActivity`：launcher，`windowSoftInputMode="adjustResize"`（键盘弹起压缩布局）。
- `AgentService`：`foregroundServiceType="specialUse"` + `PROPERTY_SPECIAL_USE_FGS_SUBTYPE = "AI agent runtime host"`；`exported=false`。
- `TermuxResultReceiver`（bridge/）：RUN_COMMAND 结果回传目标，`exported=false`（PendingIntent 临时授权使 Termux 可达）。
- `ShizukuProvider`：authorities `${applicationId}.shizuku`，`INTERACT_ACROSS_USERS_FULL` 保护。

### debug 源集（`src/debug/AndroidManifest.xml`）

额外注册 `DebugReceiver`（**exported=true，仅 debug 构建合并，release 不携带**——四个 action 允许任意发送方触发，必须限定 debug 变体）。action：

- `dev.openslate.mobile.SET_KEY`（extras：provider/key）
- `dev.openslate.mobile.SET_PROXY`
- `dev.openslate.mobile.SEND`
- `dev.openslate.mobile.TERMUX`

广播注入命令示例见 [../07-build-dev/android-build.md](../07-build-dev/android-build.md) §4。

## 4. Kotlin 源码文件角色表（`android/app/src/main/java/dev/openslate/mobile/`）

| 文件 | 行数 | 角色 |
|------|------|------|
| `MainActivity.kt` | 227 | 入口 + `AppNavHost`（路由：`chat`（起点）/ `settings` / `registry` / `keys` / `history`，二级页右侧滑入动画）+ `WarmUpSecondaryScreens()`（二级页 0 尺寸预热一帧，消除 debug 包首导航卡顿）+ Cyan 主题（`LightCyanColors`/`DarkCyanColors`、`OpenSlateTheme`、`ThemeMode`） |
| `OpenSlateApp.kt` | 15 | Application |
| `service/AgentService.kt` | 95 | 前台服务：`onCreate` 起协程 `RuntimeBridge.start(ctx)`（IO）；`onStartCommand` 返回 `START_STICKY`；`onDestroy` `runBlocking` + **2.5s 超时**同步等 shutdown（避免 DESTROYED 竞态截断 Rust 收尾）；`startForeground` 的 `UPSIDE_DOWN_CAKE`(34) 分支选 `FOREGROUND_SERVICE_TYPE_SPECIAL_USE` 重载；通知 channel IMPORTANCE_LOW、`NOTIFICATION_ID=1001` |
| `bridge/RuntimeBridge.kt` | 1665 | **核心**。object 单例：事件管线（UniFFI 回调 → `Channel(UNLIMITED)` trySend → 单消费协程 pump → `StateFlow<RuntimeUiState>`，全程保序）；UI 投影类型（`UiEntry` sealed 8 种 + Config/Provider/Model/SessionSummary/registry 系列）；公开 `submit/cancel/newSession/…/upsertMcpServer/reloadMcpServers/mcpRegistryToolsByPrefix/get·setMcpToolNameTemplate/get·setMcpBuiltinToolNameTemplate/get·setMcpServerName`；**模板正则提取**（`buildTemplateRegex`：字面 escape、`{server}` lazy 组、`{tool}` greedy 组，对大模板与 builtin 小模板（`DEFAULT_MCP_BUILTIN_NAME_TEMPLATE` 字面镜像，两占位符）各自编译；`mcpServerSlugOf`/`mcpToolBareName`/`mcpAgentPrefixNote` 大模板优先、builtin 模板兜底（host 裸名注册形态 → slug = 前缀本身；HostTool 兜底形态同名同样命中，纯展示级可接受），`legacySlugOf` 旧 `__` 启发式仅兜底）；`MCP_SERVER_NAMES` 动态计算属性（`@Volatile` 前缀缓存 + `refreshMcpNaming()` 三刷：服务名 + 两模板）；`parseToolParams` 递归展开嵌套 JSON Schema；host call 分发（termux 家族三分支：`termux.run`/`termux_bash`/`bash`——bash 为仅选 termux 时 HostTool 顶替名，输出按实际注册名配对） |
| `bridge/SecretStore.kt` | 118 | Keystore AES-256-GCM 持久化 API key；API≥28 优先 StrongBox 回退 TEE，API 26/27 直接 TEE；三态 `KeyLoadResult`（Missing/Ok/Failed——必须区分「未存储」与「解密失败」） |
| `bridge/TermuxExec.kt` | 194 | Termux RUN_COMMAND 执行器：PendingIntent（`FLAG_MUTABLE or FLAG_ONE_SHOT`）→ `TermuxResultReceiver` → `CompletableDeferred` 回填 → `resolveHostCall`；结果 Bundle 健壮解析（依次试 `result`/`com.termux.execute.result` 两 key，都不中则按形状扫描 extras，命中 = 含 stdout/stderr/errmsg/err 任一）+ `errCode` 整型错误码；**结果超时 90s**（`RESULT_TIMEOUT_MS`）；错误文案精确化（权限缺失含 Shizuku/先开 Termux 指引，超时含 stopped state/wakelock/电池优化指引） |
| `bridge/TermuxDiagnostics.kt` | 165 | RUN_COMMAND 通道五态诊断（`ChannelResult`：Ok/PolicyRejected/Rejected/DispatchError/NoResponse(stopped)）——termux bash 降级阶梯的**兜底路径**判定（host MCP 在线时 bash 走 MCP 主路径，不经本类）；`isTermuxStopped`（FLAG_STOPPED）；`ensureTermuxUp` 阶梯：1s 首探 → 非 stopped 静默重试 6s → stopped 直接返回诊断结果；**不做自动拉起**，停止态由 UI 引导用户手动打开 Termux（「打开 Termux」按钮内嵌在 McpSection 无应答指引块；返回 ON_RESUME 自动全量重检——非全绿快照不再廉价比对，从 Termux 返回即自动确认恢复并顺带拉起 host）；McpSection 检测中 host 已装未运行时自动 `restartHost` 拉起（无需手动重装）；健康态变化（门控翻转）自动触发重检 + 差异自愈（reloadMcpServers）= 「重载 MCP」入口；`launchTermuxAndReturn` 仅用于 installHost 首步 |
| `bridge/McpHostManager.kt` | 675 | MCP host 管理：`McpServerEntry(name, slug?, command)`（名称=显示名；代号可空=自动从名称派生，**用户指定也经同一 sanitize**）；`entrySlug`/`serversSlugMap` 统一派生（白名单/压连字符/禁 `__`/`srv` 回退/去重）；`suggestServerName(command)`（npx/uvx/pnpm dlx 包名识别，剥 `@scope/`，识别不出回退命令首 token——名称留空时的显示名来源）；assets 内嵌 host 经本机回环 TCP 安装；base64 推送 toml 清单（`[servers.<slug>]`；**`manifestToml` 永不写 `[builtin]` 段——版本偏斜防线：新 App + 旧 host 二进制（HostConfig `deny_unknown_fields`）会把不认识的键当 fatal，host 起不来**）；nohup/setsid 拉起（**注入 `LD_PRELOAD=libtermux-exec.so`**）；探活 `HOST_PORT=8765`；token 经 SecretStore；`probeCommandAvailable`；`hostToolsByPrefix`（readTimeout 15s） |
| `bridge/ShizukuGrant.kt` | 173 | Shizuku 一键 `pm grant`（仅首次配置用，日常执行不经它） |
| `bridge/LogExporter.kt` | 70 | logcat 导出 |
| `bridge/AppPrefs.kt` | 12 | 简单偏好（exec backends / http proxy，启动恢复） |
| `ui/ChatScreen.kt` | 1285 | 主聊天页：`EntryRow` / `ToolGroupCard` / `ToolChipRow` / `ToolDetailDialog` / `ReasoningBlock`（折叠思维链 + usage meta）/ `MarkdownBubble` / `GlassPanel`（磨砂玻璃面板，`Glass`/`rememberGlass`/`GlassPanel` internal，聚焦面板共用）/ `Composer`（BasicTextField 聚焦状态经 `onInputFocusChange` 上抛；发送后 `clearFocus` 退出编辑态）/ `ApprovalBanner`；**输入框聚焦面板接线**：`composerFocused` 状态 + `LaunchedEffect(focused, ready)` 按需读取快照（无轮询，失焦保留上次快照、再聚焦静默刷新）+ 聚焦瞬间末条贴近底部时轻推滚动（避免面板盖住尾部） |
| `ui/ComposerPanel.kt` | 400 | 输入框聚焦面板（`ComposerFocusPanel` + `loadComposerPanelData`）：点击聚焦输入框时从 composer 上方展开（TermuxStallBanner 同列同语言：GlassPanel + expandVertically 自底部），列整体在 imePadding 之上 → 面板向上生长、composer 相对键盘位置不变，未聚焦不参与组合零布局变化；内容 = MCP server 数 + 可见工具数统计行、server 名单行（slug 反查显示名，0 个给配置指引）、0.5dp 分隔线、「本次对话模型」选择器（`ModelPickerRow`：整行点击弹 DropdownMenu，选项 = levels 键 + capabilities 值 + models 条目键，副标题按 resolve_model 同序解析「Provider · modelId」，选中带 Check；切换走 `setModelAlias`（set_model，会话级不落盘）→ model_changed 回填 `state.modelAlias`，与顶栏/发消息实际生效同一状态源）；数据口径：MCP 数 = `mcpRegistryToolsByPrefix` 注册表 slug 分组计数（真实加载，连接失败的 server 不出现，内部自带 refreshMcpNaming）、工具数 = `listTools()` + `getAgentToolVisibility()`（可见性判定与工具管理页逐字同式）；空态/异常态：加载中小 spinner 行、runtime 未就绪降级文案、0 MCP 提示、0 可见工具 error 色 |
| `ui/SettingsScreen.kt` | 3011 | 设置页：Provider/Model/Level 编辑（`EditProviderDialog`/`EditModelDialog`/`ProviderModelsDialog`/`AliasBindingDialog`）、`ModelCapsRow`+`CapsFlag`、`DropdownChoice`、`ExecBackendSection`（bash 后端多选；`CheckboxRow` 支持 `subtitle` 可选第二行——标题正常字号 + 灰色 `labelSmall` 说明，Native/Termux 选项双行展示；状态文案对齐降级阶梯：MCP 在线走 MCP、离线 RUN_COMMAND 兜底）、`McpSection`（`Section` 支持 `subtitle` 副标题双行标题与右侧主题色文字按钮「刷新/重启」左右布局；upsertMcpServer 调用点动态取当前前缀；服务器卡片=名称/命令/六态状态行+底部主题色文字按钮「编辑/重启/删除」——重启=智能修复；添加/编辑对话框=名称（可留空自动识别）+代号+命令+前缀预览；状态源=core 注册表；差异自愈；快照缓存+智能门控（非全绿快照进页/ON_RESUME 直接全量重检，全绿快照仅廉价翻转探测）。两个「工具名模板」设置行已迁往工具管理页的「工具命名模板」小节，McpSection 回归纯服务器管理）、`LogSection`、`Section`/`SlidingSegmentedControl` 基础件；`CompactButton` 为 internal（工具管理页模板小节共用同一实现） |
| `ui/HistoryScreen.kt` | 245 | 历史会话（分页 50/页；`titlesVersion` 增量刷新——run_title 事件到达时原位替换占位标题） |
| `ui/KeysScreen.kt` | 228 | API key 管理（SecretStore；解密失败态显式展示「需重新录入」） |
| `ui/ModelRegistryScreen.kt` | 370 | 模型数据源页面（源状态 / 手动更新 / 条目浏览 / 跨源搜索） |
| `ui/ToolManagementSection.kt` | 1214 | 工具可见性管理（listTools + get/setAgentToolVisibility）：**「工具命名模板」小节**（`ToolNameTemplateSection`，自 SettingsScreen McpSection 迁入、行内交互原样；**默认收起、按需展开的折叠形态**——选折叠而非弹窗：保存后下方分组立即按新模板重聚合，展开态让这条反馈环留在原语境；标题行点击展开/收起（`KeyboardArrowDown` 箭头旋转 + `AnimatedVisibility` 纵向展开，对齐 ToolRow/McpToolGroupCard 范式），收起态标题下灰字等宽单行截断显示「MCP … · builtin …」两个生效模板字面（生效值未读到不渲染防闪烁，展开态不渲染防冗余），模板输入/生效值与进页 IO 预填提升到折叠区外（收起再展开不丢编辑中内容）；置于「MCP 工具」分组正上方——模板即下方分组显示名的来源：「工具名模板」+「builtin 工具名模板」两设置行，光标感知 chip 插入 + 实时预览/非法提示 + 保存 = setter FFI 落盘 + 显式 reloadMcpServers 热生效 + 读回 effective + 重读工具清单（`reloadTools` 与进页首载同路径）；chip 三占位符 `{prefix}`/`{server}`/`{tool}` vs 两占位符 `{prefix}`/`{tool}`，Kotlin 预校验仅提示、以 Rust 校验为准；过程日志镜像 logcat（本页无诊断面板），保存按钮复用 SettingsScreen 的 internal `CompactButton`）；主机/命令行/内置三组平铺行；**「MCP 工具」组按 server slug 聚合成卡片**（归类与聚合同源经 `mcpServerSlugOf`——大模板正则优先、builtin 小模板兜底（host 裸名 → 前缀组），模板无字面 `__` 时也正确分组；头部=显示名（反查 `serversSlugMap`，缺省回退 slug+「未配置」）+ 统计「N 个工具 · M 个可见」+ 组级总开关（**单次全量覆写**批量开关，混合态 checked=false 由文字承载语义）+ 展开箭头；展开区复用 `ToolRow`，行间分隔线，**行内显示裸工具名**（`mcpToolBareName` 双模板提取，`__` 启发式兜底），列表**上方**灰字注明 Agent 前缀（`mcpAgentPrefixNote(slug)`，无前缀段则隐藏））；`ToolRow` 头部行=名称+Checkbox+箭头，描述按态分流（收起态名称下方单行截断、展开态移独立全宽行全显）；`ToolParamRow` 按嵌套深度缩进；Termux 依赖工具可用性判定 = **RUN_COMMAND 可用 ∨ MCP host 健康**（`detectTermuxStatus` 先探活 host——在线即 bash 走 MCP 主路径；离线才走 RUN_COMMAND 五态阶梯），不可用警示（进页 + ON_RESUME） |
| `ui/` 基础件 | — | `SoftButton`（66）/ `StatusColors`（21）/ `ThemePrefs`（33）/ `AppDialog`（48） |
| `uniffi/openslate/mobile/openslate_mobile.kt` | 2592 | **UniFFI 生成代码，禁止手改**（重生成见 [../07-build-dev/android-build.md](../07-build-dev/android-build.md)） |
| `src/debug/.../bridge/DebugReceiver.kt` | 69 | debug 专属广播注入通道（见 §3） |

## 5. 状态管理约定

- **无 ViewModel 层**：`RuntimeBridge` object 单例持有全部状态，Compose 经 `collectAsState` 收 `StateFlow<RuntimeUiState>`。
- **transcript 完整镜像在 Rust 侧（数据库）**；Kotlin 只保留渲染所需精简投影（`UiEntry` 列表），模型回复以 delta 增量并入最后一条 Assistant 条目（对齐 Rust 侧 StreamBuffers 语义）。
- 用户消息**乐观本地回显**（Rust 推 transcript 但不广播 User 事件——与桌面 TUI 同语义）；发送失败保留气泡可重发。
- 持久化偏好启动恢复：`AppPrefs`（exec backends、http proxy → `RuntimeBridge.restoreExecBackend()/restoreHttpProxy()`）、API keys（SecretStore → 逐 provider 重注入，解密失败显式告警不静默）、MCP token（`setMcpHostToken` 必须在 create 前调）。**模型选择不本地重放**：`[capabilities].main`（openslate.toml）是唯一真相源。
- 事件消费协程对内容型日志（事件可含对话/工具参数）加 `BuildConfig.DEBUG` 门控，且仅前 20 条 + 每 100 条采样。

## 6. 接手检查清单

1. 新增页面 → `MainActivity.AppNavHost` 加路由 + `ui/` 新文件；图标只用 Material icons（[../08-conventions/ui.md](../08-conventions/ui.md)）。
2. 新增 FFI 方法调用 → 先确认绑定已重生成，且阻塞方法包在 `Dispatchers.IO`。
3. 新增宿主能力（host call 目标）→ `RuntimeBridge.handleHostCall` 补分发分支 + Rust 侧注册 `HostTool`。
4. 涉及新 Android API → 先确认 API level ≥ 26 可用，否则 `Build.VERSION.SDK_INT` 分支（[../08-conventions/compat.md](../08-conventions/compat.md)）。
5. debug 专属能力一律放 `src/debug/` 源集，release 不得携带。
