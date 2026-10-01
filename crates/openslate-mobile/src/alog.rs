//! Android logcat 桥：Rust 侧阶段日志直打 logcat（无需 tracing subscriber）。
//! 非 Android 平台为空操作，desktop 测试零影响。

#[cfg(target_os = "android")]
pub fn log(level: Level, tag: &str, msg: &str) {
    use std::ffi::CString;
    let prio = match level {
        Level::Verbose => 2,
        Level::Debug => 3,
        Level::Info => 4,
        Level::Warn => 5,
        Level::Error => 6,
    };
    let tag = CString::new(tag.replace('\0', "")).unwrap_or_default();
    let msg = CString::new(msg.replace('\0', "")).unwrap_or_default();
    unsafe { __android_log_print(prio, tag.as_ptr(), b"%s\0".as_ptr().cast(), msg.as_ptr()) };
}

#[cfg(target_os = "android")]
#[allow(non_snake_case)]
extern "C" {
    #[link_name = "__android_log_print"]
    fn __android_log_print(prio: i32, tag: *const std::os::raw::c_char, fmt: *const std::os::raw::c_char, ...) -> i32;
}

#[cfg(not(target_os = "android"))]
pub fn log(_level: Level, _tag: &str, _msg: &str) {}

#[derive(Clone, Copy)]
pub enum Level {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
}

/// 统一 tag（logcat -s OpenSlateRust 过滤）。
pub const TAG: &str = "OpenSlateRust";

/// 阶段打点（Info 级）。
#[macro_export]
macro_rules! alog {
    ($($arg:tt)*) => {
        $crate::alog::log($crate::alog::Level::Info, $crate::alog::TAG, &format!($($arg)*))
    };
}

/// 安装 panic → logcat 桥（Android；tokio spawn 会吞 panic，必须经 hook
/// 外显，否则表现为无声的「空回合」）。
#[cfg(target_os = "android")]
pub fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_default();
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_owned()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "non-string panic".to_owned()
            };
            log(Level::Error, TAG, &format!("PANIC: {msg} at {loc}"));
            default(info);
        }));
    });
}

#[cfg(not(target_os = "android"))]
pub fn install_panic_hook() {}

/// 把 vendor/genai 的原始 usage 诊断流接到 logcat（诊断网关 usage 字段的
/// 预留接口——网关字段变化看 logcat 即可，免抓包）。
#[cfg(target_os = "android")]
pub fn install_raw_usage_logger() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        genai::chat::set_raw_usage_logger(|adapter, json| {
            log(Level::Info, TAG, &format!("{adapter} raw usage: {json}"));
        });
    });
}

#[cfg(not(target_os = "android"))]
pub fn install_raw_usage_logger() {}
