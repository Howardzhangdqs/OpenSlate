package dev.openslate.mobile.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 工具详情弹窗 JSON 自动格式化纯函数单测（[looksLikeJson] /
 * [prettyJsonOrNull]，ChatScreen.kt 顶层 internal 函数；纯 JVM——实现用
 * kotlinx.serialization，不涉及 org.json 的 JVM stub 限制）。
 *
 * 场景对齐弹窗实际输入：工具参数 fullArgs（合法紧凑 JSON / 截断残骸）与
 * 工具输出段（stdout/stderr 任意文本）。
 */
class ToolJsonFormatTest {

    // ── looksLikeJson：合法形态 ──────────────────────────────────────────

    @Test
    fun looksLikeJson_objectAndArray() {
        assertTrue(looksLikeJson("""{"command":"ls -la"}"""))
        assertTrue(looksLikeJson("""[1,2,3]"""))
        // 前后空白容忍（输出段经 parseToolOut trim，防御性双保险）。
        assertTrue(looksLikeJson("""  { "a" : 1 }  """))
        // 已是 pretty 的输入同样判定为 JSON。
        assertTrue(looksLikeJson("{\n  \"a\": 1\n}\n"))
    }

    @Test
    fun looksLikeJson_nestedAndChineseAndUnicode() {
        assertTrue(looksLikeJson("""{"外":{"中":["值"]}}"""))
        assertTrue(looksLikeJson("""{"s":"héllo 中"}"""))
        assertTrue(looksLikeJson("""{"k":"\u4e2d\u6587"}"""))
        assertTrue(looksLikeJson("""[{"a":1},{"b":null}]"""))
    }

    // ── looksLikeJson：非 JSON / 边界 ────────────────────────────────────

    @Test
    fun looksLikeJson_scalarsAreFalse() {
        // 裸标量格式化与否视觉无差，明确排除（按钮不误现）。
        assertFalse(looksLikeJson("123"))
        assertFalse(looksLikeJson("-0.5"))
        assertFalse(looksLikeJson("1e10"))
        assertFalse(looksLikeJson("\"plain string\""))
        assertFalse(looksLikeJson("true"))
        assertFalse(looksLikeJson("false"))
        assertFalse(looksLikeJson("null"))
    }

    @Test
    fun looksLikeJson_blankAndPlainFalse() {
        assertFalse(looksLikeJson(""))
        assertFalse(looksLikeJson("   \n\t "))
        assertFalse(looksLikeJson("not json"))
        assertFalse(looksLikeJson("plain tool output line"))
    }

    @Test
    fun looksLikeJson_truncatedIsFalse() {
        // 截断残骸（传输层 120 字符截断等）残缺形态不得误判为 JSON。
        assertFalse(looksLikeJson("""{"command":"cargo build --release"""))
        assertFalse(looksLikeJson("[1, 2"))
        assertFalse(looksLikeJson("""[{"a":1},{"b"""))
    }

    // ── prettyJsonOrNull：pretty 输出形态 ────────────────────────────────

    @Test
    fun prettyJson_twoSpaceIndent() {
        assertEquals(
            "{\n  \"a\": 1\n}",
            prettyJsonOrNull("""{"a":1}"""),
        )
        assertEquals(
            "[\n  1,\n  2\n]",
            prettyJsonOrNull("[1,2]"),
        )
    }

    @Test
    fun prettyJson_nestedStructure() {
        assertEquals(
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {\n    \"c\": true\n  }\n}",
            prettyJsonOrNull("""{"a":[1,2],"b":{"c":true}}"""),
        )
    }

    @Test
    fun prettyJson_scalarsAndContainerKinds() {
        // 布尔/字符串值原样保留类型与字面量。
        assertEquals(
            "{\n  \"on\": true,\n  \"off\": false,\n  \"s\": \"x\"\n}",
            prettyJsonOrNull("""{"on":true,"off":false,"s":"x"}"""),
        )
    }

    @Test
    fun prettyJson_chineseKeptLiteral() {
        assertEquals(
            "{\n  \"名\": \"值中\"\n}",
            prettyJsonOrNull("""{"名":"值中"}"""),
        )
    }

    @Test
    fun prettyJson_unicodeEscapeDecoded() {
        // 转义在解析期还原：pretty 输出写原字符，不再带 \u 序列。
        assertEquals(
            "{\n  \"k\": \"中文\"\n}",
            prettyJsonOrNull("""{"k":"\u4e2d\u6587"}"""),
        )
    }

    @Test
    fun prettyJson_preservesKeyInsertionOrder() {
        // JsonObject 保键插入序：键序不重排（工具参数键序有语义观感）。
        assertEquals(
            "{\n  \"z\": 1,\n  \"a\": 2,\n  \"m\": 3\n}",
            prettyJsonOrNull("""{"z":1,"a":2,"m":3}"""),
        )
    }

    @Test
    fun prettyJson_bigIntegerPrecisionKept() {
        // 2^53+1 超 double 精度、20 位超 long 范围：JsonElement 以原字符串
        // 存数字，round-trip 不失真（这正是选 kotlinx 的能力前提之一）。
        assertEquals(
            "{\n  \"big\": 9007199254740993\n}",
            prettyJsonOrNull("""{"big":9007199254740993}"""),
        )
        assertEquals(
            "[\n  12345678901234567890\n]",
            prettyJsonOrNull("[12345678901234567890]"),
        )
    }

    @Test
    fun prettyJson_invalidReturnsNull() {
        assertNull(prettyJsonOrNull("not json"))
        assertNull(prettyJsonOrNull("{ broken"))
        assertNull(prettyJsonOrNull("""{"command":"cargo build --release"""))
    }

    @Test
    fun prettyJson_scalarAndBlankReturnNull() {
        // 非容器标量与空串：null（调用方回退原文显示，与 looksLikeJson 一致）。
        assertNull(prettyJsonOrNull("123"))
        assertNull(prettyJsonOrNull("\"str\""))
        assertNull(prettyJsonOrNull("true"))
        assertNull(prettyJsonOrNull(""))
        assertNull(prettyJsonOrNull("   "))
    }

    // ── 两函数判定一致（弹窗切换逻辑依赖该不变量）───────────────────────

    @Test
    fun looksLikeJson_and_prettyJson_agreeOnSameInputs() {
        val inputs = listOf(
            """{"a":1}""",
            """[1,2]""",
            "123",
            "\"str\"",
            "true",
            "",
            "   ",
            "{ broken",
            """{"command":"x""""",
            "not json",
        )
        for (input in inputs) {
            assertEquals(
                "looksLikeJson 与 prettyJsonOrNull 判定不一致：$input",
                looksLikeJson(input),
                prettyJsonOrNull(input) != null,
            )
        }
    }
}
