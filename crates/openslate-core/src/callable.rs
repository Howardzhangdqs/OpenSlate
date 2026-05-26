//! Callable abstraction — base trait for tools and child agents.

/// Discriminator for the kind of callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallableKind {
    Tool,
    ChildAgent,
}

/// Base trait shared by tools and child agents.
///
/// The actual async execution method will be added in a future task
/// (Task 15 — Tool trait + registry). For now this captures the
/// metadata every callable must expose.
pub trait Callable: Send + Sync {
    /// The name used to invoke this callable (e.g. `"bash"`, `"child-agent-1"`).
    fn name(&self) -> &str;

    /// Human-readable description of what this callable does.
    fn description(&self) -> &str;

    /// JSON Schema describing the parameters this callable accepts.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Whether this callable is a tool or a child agent.
    fn kind(&self) -> CallableKind;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callable_kind_equality() {
        assert_eq!(CallableKind::Tool, CallableKind::Tool);
        assert_ne!(CallableKind::Tool, CallableKind::ChildAgent);
    }

    #[test]
    fn callable_kind_copy() {
        let a = CallableKind::Tool;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn callable_kind_debug() {
        assert_eq!(format!("{:?}", CallableKind::Tool), "Tool");
        assert_eq!(format!("{:?}", CallableKind::ChildAgent), "ChildAgent");
    }

    /// Minimal stub to verify the trait can be implemented.
    struct StubCallable {
        n: &'static str,
        desc: &'static str,
        k: CallableKind,
    }

    impl Callable for StubCallable {
        fn name(&self) -> &str {
            self.n
        }

        fn description(&self) -> &str {
            self.desc
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn kind(&self) -> CallableKind {
            self.k
        }
    }

    #[test]
    fn stub_callable_implements_trait() {
        let s = StubCallable {
            n: "stub",
            desc: "a stub",
            k: CallableKind::Tool,
        };
        assert_eq!(s.name(), "stub");
        assert_eq!(s.description(), "a stub");
        assert_eq!(s.kind(), CallableKind::Tool);
        // Verify the schema is valid JSON
        let schema = s.parameters_schema();
        assert_eq!(schema["type"], "object");
    }
}
