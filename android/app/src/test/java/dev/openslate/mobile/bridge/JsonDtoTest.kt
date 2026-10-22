package dev.openslate.mobile.bridge

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * JsonDto 解析层单测（纯 JVM）。样例形态取自 call-chains.md §3.2 与
 * 04-tools/ask-user.md 的 wire 契约（Rust 源：askuser.rs / protocol lib.rs）。
 */
class JsonDtoTest {

    // ── ask_user：questions[] 解析（六种形态 + 缺省容错）────────────────

    @Test
    fun parseQuestions_sixTypes() {
        // choice / text / confirm / secret(env) / secret(file) / 全缺省题
        val json = """
            [
              {"id":"db","header":"数据库","question":"选哪个数据库？","type":"choice",
               "options":[{"label":"SQLite","description":"轻量内嵌","recommended":true},
                          {"label":"Postgres","description":"功能全"},
                          {"label":""}],
               "multi_select":true,"optional":false},
              {"id":"name","type":"text","question":"项目名？","placeholder":"my-app","optional":true},
              {"id":"go","type":"confirm","question":"继续吗？",
               "options":[{"label":"是"},{"label":"否"}]},
              {"id":"tok","type":"secret","question":"GitHub token？",
               "storage":{"kind":"env","name":"GITHUB_TOKEN"},"placeholder":"ghp_…"},
              {"id":"net","type":"secret","question":"netrc 路径？",
               "storage":{"kind":"file","path":"~/.netrc"}},
              {"question":"全缺省题？"}
            ]
        """.trimIndent()
        val qs = decodeQuestionDtos(json).mapIndexed { i, dto -> dto.toQuestionUi(i) }
        assertEquals(6, qs.size)

        val choice = qs[0]
        assertEquals("db", choice.id)
        assertEquals("数据库", choice.header)
        assertEquals("choice", choice.type)
        assertTrue(choice.multiSelect)
        assertEquals(false, choice.optional)
        assertEquals(2, choice.options.size) // 空 label 行被过滤
        assertEquals(QuestionOptionUi("SQLite", "轻量内嵌", true), choice.options[0])
        assertEquals(QuestionOptionUi("Postgres", "功能全", false), choice.options[1])
        assertNull(choice.placeholder)
        assertNull(choice.storageKind)
        assertNull(choice.storageName)

        val text = qs[1]
        assertEquals("text", text.type)
        assertEquals("my-app", text.placeholder)
        assertTrue(text.optional)
        assertEquals(emptyList<QuestionOptionUi>(), text.options)

        val confirm = qs[2]
        assertEquals("confirm", confirm.type)
        assertEquals(listOf(QuestionOptionUi("是"), QuestionOptionUi("否")), confirm.options)
        assertEquals(false, confirm.multiSelect)

        val secretEnv = qs[3]
        assertEquals("env", secretEnv.storageKind)
        assertEquals("GITHUB_TOKEN", secretEnv.storageName)

        val secretFile = qs[4]
        assertEquals("file", secretFile.storageKind)
        assertEquals("~/.netrc", secretFile.storageName)

        // 全缺省：id → q6、header → 问题 6、type → choice（对齐旧 opt* 缺省链）
        val blank = qs[5]
        assertEquals("q6", blank.id)
        assertEquals("问题 6", blank.header)
        assertEquals("choice", blank.type)
    }

    @Test
    fun parseQuestions_defensiveFallbacks() {
        // file storage 的 path 空 → 回退 name；非对象行跳过；垃圾输入 → 空表
        assertEquals("fallback", decodeQuestionDtos("""[{"type":"secret","storage":{"kind":"file","path":"","name":"fallback"}}]""")
            .mapIndexed { i, dto -> dto.toQuestionUi(i) }.single().storageName)
        assertEquals(0, decodeQuestionDtos("""["not-an-object", 42]""").size)
        assertEquals(0, decodeQuestionDtos("garbage").size)
        assertEquals(0, decodeQuestionDtos(null).size)
        // 显式 null placeholder / 非 env·file 的 storage kind → 均按缺省
        val q = decodeQuestionDtos("""[{"id":"a","placeholder":null,"storage":{"kind":"usb"}}]""")
            .mapIndexed { i, dto -> dto.toQuestionUi(i) }.single()
        assertNull(q.placeholder)
        assertNull(q.storageKind)
        assertNull(q.storageName)
    }

    // ── ask_user：已答回看（args + output 双源合并）─────────────────────

    @Test
    fun reviewedQuestion_answerMergeByid() {
        val args = """
            {"questions":[
              {"id":"db","header":"数据库","question":"?","type":"choice",
               "options":[{"label":"SQLite"},{"label":"Postgres"}]},
              {"id":"name","type":"text","question":"?"},
              {"id":"go","type":"confirm","question":"?",
               "options":[{"label":"是"},{"label":"否"}]},
              {"id":"tok","type":"secret","question":"?","storage":{"kind":"env","name":"T"}},
              {"id":"opt","type":"text","question":"?","optional":true},
              {"id":"x","type":"text","question":"?"}
            ]}
        """.trimIndent()
        val output = """
            {"outcome":"answered","answers":[
              {"id":"db","selected":["SQLite","自定义其他说明"]},
              {"id":"name","value":"hello"},
              {"id":"go","confirm":true,"selected":["是"]},
              {"id":"tok","secret":{"name":"T","kind":"env","remember":true}},
              {"id":"opt","skipped":true}
            ]}
        """.trimIndent()
        val review = decodeReviewedQuestion(args, output)
        assertNotNull(review)
        val d = review!!.drafts
        // 「其他」说明未命中选项 → otherChecked/otherText（提交时才并入 selected）
        assertEquals(listOf("SQLite"), d[0].selected)
        assertTrue(d[0].otherChecked)
        assertEquals("自定义其他说明", d[0].otherText)
        assertEquals("hello", d[1].text)
        assertEquals(true, d[2].confirm)
        // secret 只留 remember，明文不回填
        assertEquals(true, d[3].remember)
        assertEquals("", d[3].text)
        assertEquals(true, d[4].skipped)
        // args 有题但 answers 无该 id → 空草稿（不吞题）
        assertEquals(QuestionDraft(), d[5])
    }

    @Test
    fun reviewedQuestion_declinedAndFallbacks() {
        val args = """{"questions":[{"id":"a","type":"text","question":"?"}]}"""
        assertNull(decodeReviewedQuestion(null, """{"outcome":"answered","answers":[{"id":"a","value":"x"}]}"""))
        assertNull(decodeReviewedQuestion(args, null))
        assertNull(decodeReviewedQuestion(args, """{"outcome":"declined"}"""))
        assertNull(decodeReviewedQuestion(args, """{"outcome":"answered","answers":[]}"""))
        val r = decodeReviewedQuestion(args, """{"outcome":"answered","answers":[{"id":"a","declined":true}]}""")
        assertNotNull(r)
        assertTrue(r!!.drafts.single().declined)
    }

    // ── ask_user：提交拼装（resolve / display 双 payload）──────────────

    private fun reviewQuestions() = decodeQuestionDtos(
        """
        [{"id":"db","header":"数据库","question":"?","type":"choice",
          "options":[{"label":"SQLite"},{"label":"Postgres"}]},
         {"id":"tok","header":"密钥","question":"?","type":"secret",
          "storage":{"kind":"env","name":"GITHUB_TOKEN"}},
         {"id":"go","header":"确认","question":"?","type":"confirm",
          "options":[{"label":"是"},{"label":"否"}]},
         {"id":"name","header":"名称","question":"?","type":"text"},
         {"id":"opt","header":"可选","question":"?","type":"text","optional":true}]
        """
    ).mapIndexed { i, dto -> dto.toQuestionUi(i) }

    @Test
    fun submitPayloads_fieldExactness() {
        val qs = reviewQuestions()
        val drafts = listOf(
            QuestionDraft(selected = listOf("SQLite"), otherChecked = true, otherText = " 自定义说明 "),
            QuestionDraft(text = "s3cret", remember = true),
            QuestionDraft(confirm = false),
            QuestionDraft(text = "  hello  "),
            QuestionDraft(skipped = true),
        )
        val p = buildAskUserSubmitPayloads(qs, drafts)
        // resolve（明文仅 FFI 通道）：字段集与旧 org.json 版逐键一致
        //（字段顺序 = DTO 声明序 id,selected,value,confirm,remember,…）
        assertEquals(
            """{"outcome":"answered","answers":[""" +
                """{"id":"db","selected":["SQLite","自定义说明"]},""" +
                """{"id":"tok","value":"s3cret","remember":true},""" +
                """{"id":"go","selected":["否"],"confirm":false},""" +
                """{"id":"name","value":"hello"},""" +
                """{"id":"opt","skipped":true}]}""",
            p.resolveJson,
        )
        // display（脱敏转写）：header 恒有；secret 只留 {name,kind,remember} 引用
        assertEquals(
            """{"outcome":"answered","answers":[""" +
                """{"id":"db","header":"数据库","selected":["SQLite","自定义说明"]},""" +
                """{"id":"tok","header":"密钥","secret":{"name":"GITHUB_TOKEN","kind":"env","remember":true}},""" +
                """{"id":"go","header":"确认","selected":["否"],"confirm":false},""" +
                """{"id":"name","header":"名称","value":"hello"},""" +
                """{"id":"opt","header":"可选","skipped":true}]}""",
            p.displayJson,
        )
    }

    @Test
    fun submitPayloads_secretRememberFalseAndDisplayFallback() {
        // remember=false 也必须出现（旧实现恒 put）；storageName/Kind 缺失时
        // display 引用兜底 "" / "env"
        val qs = decodeQuestionDtos("""[{"id":"s","type":"secret","question":"?"}]""")
            .mapIndexed { i, dto -> dto.toQuestionUi(i) }
        val p = buildAskUserSubmitPayloads(qs, listOf(QuestionDraft(text = "v", remember = false)))
        assertEquals(
            """{"outcome":"answered","answers":[{"id":"s","value":"v","remember":false}]}""",
            p.resolveJson,
        )
        assertEquals(
            """{"outcome":"answered","answers":[{"id":"s","header":"问题 1","secret":{"name":"","kind":"env","remember":false}}]}""",
            p.displayJson,
        )
    }

    // ── config：ConfigViewDto 投影（Option 双形态 / 缺省语义）──────────

    @Test
    fun configDecode_optionNullAndAbsent() {
        val json = """
            {"providers":{
               "zhipu":{"base_url":"https://open.bigmodel.cn/api/paas","api_key_env":"ZAI_KEY",
                        "adapter":"openai","title":"智谱","max_attempts":3},
               "bare":{"base_url":"u","api_key_env":"K","adapter":null}
             },
             "models":{
               "zhipu/glm-4.7":{"provider":"zhipu","model":"glm-4.7",
                 "max_context_tokens":200000,"max_output_tokens":128000,
                 "supports_tool_call":true,"supports_vision":false,"supports_reasoning":true,
                 "input_price_per_mtok":0.6,"output_price_per_mtok":2.2,"reasoning_effort":"high"},
               "p/m":{"provider":"p","model":"m","max_context_tokens":null,
                 "supports_tool_call":false}
             },
             "levels":{"main":"zhipu/glm-4.7"},
             "capabilities":{"main":"zhipu/glm-4.7","fast":"p/m"},
             "approval":{"default_level":"auto","tool_levels":{"bash":"allow"},"server_levels":{}},
             "active_config":"/data/openslate.toml",
             "limits":{"max_steps":200},"agents":{"id":"root"},"skills":[]}
        """.trimIndent()
        val c = decodeConfigJson(json)
        assertEquals(2, c.providers.size)
        val zhipu = c.providers[0]
        assertEquals("zhipu", zhipu.name)
        assertEquals("https://open.bigmodel.cn/api/paas", zhipu.baseUrl)
        assertEquals("ZAI_KEY", zhipu.apiKeyEnv)
        assertEquals("openai", zhipu.adapter)
        assertEquals("智谱", zhipu.title)
        val bare = c.providers[1]
        assertNull(bare.adapter) // 显式 null 与缺键同义
        assertNull(bare.title)

        val glm = c.models[0]
        assertEquals("zhipu/glm-4.7", glm.entry)
        assertEquals(200000L, glm.maxContextTokens)
        assertEquals(128000L, glm.maxOutputTokens)
        assertTrue(glm.supportsToolCall)
        assertEquals(false, glm.supportsVision)
        assertTrue(glm.supportsReasoning)
        assertEquals(0.6, glm.inputPricePerMtok!!, 1e-9)
        assertEquals(2.2, glm.outputPricePerMtok!!, 1e-9)
        assertEquals("high", glm.reasoningEffort)
        val m2 = c.models[1]
        assertNull(m2.maxContextTokens) // null 形态
        assertNull(m2.maxOutputTokens)  // 缺键形态
        assertEquals(false, m2.supportsToolCall) // 显式 false
        assertNull(m2.reasoningEffort)  // Rust None 跳键

        assertEquals(mapOf("main" to "zhipu/glm-4.7"), c.levels)
        assertEquals(mapOf("main" to "zhipu/glm-4.7", "fast" to "p/m"), c.capabilities)
        assertEquals("auto", c.approval?.defaultLevel)
        assertEquals(mapOf("bash" to "allow"), c.approval?.toolLevels)
        assertEquals(emptyMap<String, String>(), c.approval?.serverLevels)
        assertEquals("/data/openslate.toml", c.activeConfig)
    }

    @Test
    fun configDecode_defaults() {
        // supports_tool_call 缺键 → true（旧 optBoolean 默认）；approval 缺 → null
        val c = decodeConfigJson("""{"models":{"a/b":{"provider":"a","model":"b"}}}""")
        assertTrue(c.models.single().supportsToolCall)
        assertEquals(false, c.models.single().supportsVision) // 非 nullable（旧 optBoolean 语义）
        assertNull(c.approval)
        assertEquals("", c.activeConfig)
        assertEquals(emptyList<ProviderUi>(), c.providers)
        // 旧 runtime 无 approval 键 / 垃圾输入 → 空投影不抛
        assertEquals(ConfigUi(), decodeConfigJson("garbage"))
        // default_level 空串 → auto（旧 ifBlank 语义）
        val c2 = decodeConfigJson("""{"approval":{"default_level":""}}""")
        assertEquals("auto", c2.approval?.defaultLevel)
    }

    // ── 本地转写镜像：8 种 UiEntry 短键往返（字节级兼容旧格式）────────

    @Test
    fun mirror_shortKeyExactness() {
        assertEquals(
            """{"t":"user","x":"你好"}""",
            encodeEntryMirror(UiEntry.User("你好")),
        )
        assertEquals(
            """{"t":"user","x":"看图","atts":[""" +
                """{"id":"att-1","mime":"image/jpeg","filename":"a.jpg","tokens":123},""" +
                """{"id":"att-2","mime":"video/mp4"}]}""",
            encodeEntryMirror(
                UiEntry.User(
                    "看图",
                    listOf(
                        AttachmentRefUi("att-1", "image/jpeg", "a.jpg", 123L),
                        AttachmentRefUi("att-2", "video/mp4"), // filename/tokens null → 跳键
                    ),
                ),
            ),
        )
        assertEquals("""{"t":"assistant","x":"回复"}""", encodeEntryMirror(UiEntry.Assistant("回复")))
        assertEquals("""{"t":"reasoning","x":"思考"}""", encodeEntryMirror(UiEntry.Reasoning("思考")))
        assertEquals(
            """{"t":"reasoning","x":"思考","m":"12.3s · ↑1.2k ↓3.4k"}""",
            encodeEntryMirror(UiEntry.Reasoning("思考", "12.3s · ↑1.2k ↓3.4k")),
        )
        assertEquals("""{"t":"meta","x":"⚡ 生成中"}""", encodeEntryMirror(UiEntry.Meta("⚡ 生成中")))
        // 空值键省略（output/callId null → o/c 不写；旧读侧 opt* 缺键同落 null）
        assertEquals(
            """{"t":"tool","n":"bash","a":"ls -la","s":"running","f":"{\"command\":\"ls\"}"}""",
            encodeEntryMirror(UiEntry.ToolCall(name = "bash", argsPreview = "ls -la", status = "running", fullArgs = "{\"command\":\"ls\"}")),
        )
        assertEquals(
            // status=done 为 DTO 默认值 → s 省略（读侧缺键回退 done，值等价）
            """{"t":"tool","n":"read","a":"…","f":"{}","o":"内容","c":"call-9"}""",
            encodeEntryMirror(UiEntry.ToolCall(name = "read", argsPreview = "…", status = "done", fullArgs = "{}", output = "内容", callId = "call-9")),
        )
        assertEquals("""{"t":"delegate","n":"researcher","d":true}""", encodeEntryMirror(UiEntry.Delegate("researcher", true)))
        assertEquals("""{"t":"approval","n":"bash","d":"approved"}""", encodeEntryMirror(UiEntry.Approval("bash", "approved")))
        assertEquals("""{"t":"step"}""", encodeEntryMirror(UiEntry.StepBreak))
    }

    @Test
    fun mirror_roundTrip() {
        val entries = listOf(
            UiEntry.User("hi", listOf(AttachmentRefUi("att-1", "image/png", "p.png", 7))),
            UiEntry.Assistant("yo"),
            UiEntry.Reasoning("think", "meta 行"),
            UiEntry.Meta("⚡ note"),
            UiEntry.ToolCall(name = "bash", argsPreview = "ls", status = "failed", fullArgs = "{}", output = "err", callId = "c1"),
            UiEntry.Delegate("sub", false),
            UiEntry.Approval("bash", "denied"),
            UiEntry.StepBreak,
        )
        for (e in entries) {
            val back = decodeEntryMirror(encodeEntryMirror(e))
            assertEquals(e, back)
        }
        // o/c 空串 → null；expanded 不持久化（true → 恢复 false）
        val tool = UiEntry.ToolCall(name = "t", argsPreview = "p", status = "done", fullArgs = "", output = null, callId = null)
        val restoredTool = decodeEntryMirror(encodeEntryMirror(tool)) as UiEntry.ToolCall
        assertNull(restoredTool.output)
        assertNull(restoredTool.callId)
        assertEquals(false, restoredTool.expanded)
    }

    @Test
    fun mirror_legacyCompat() {
        // 旧镜像：user 无 atts 键 → 空表；tool 无 c 键 → callId null；未知 t → StepBreak
        assertEquals(UiEntry.User("旧条目", emptyList()), decodeEntryMirror("""{"t":"user","x":"旧条目"}"""))
        assertEquals(
            UiEntry.ToolCall(name = "bash", argsPreview = "a", status = "done", fullArgs = "f"),
            decodeEntryMirror("""{"t":"tool","n":"bash","a":"a","s":"done","f":"f","o":""}"""),
        )
        // 未知 t / 垃圾输入 → null（RuntimeBridge.entryFromJson 再兜底 StepBreak）
        assertNull(decodeEntryMirror("""{"t":"future-kind","x":"?"}"""))
        assertNull(decodeEntryMirror("garbage"))
        // reasoning m 空白 → null
        val r = decodeEntryMirror("""{"t":"reasoning","x":"t","m":"  "}""") as UiEntry.Reasoning
        assertNull(r.meta)
    }
}
