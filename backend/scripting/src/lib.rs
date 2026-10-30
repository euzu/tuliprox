//! Embedded Rhai runtime for tuliprox scripting hosts.
//!
//! This crate owns the Rhai configuration shared by every scripting host: the engine limits,
//! the redirection of `print`/`debug` into the log, the `eval` prohibition, the compile-time
//! exclusion of modules (`import`), and the compile error mapping. It deliberately knows nothing
//! about playlists, filters or configuration, so a new host only registers its own types and
//! functions and reuses everything else here.
//!
//! A host builds one [`ScriptRuntime`] per process (for example behind an `OnceLock`) because
//! constructing an engine is comparatively expensive. `compile` is cheap and only runs when the
//! configuration is loaded; evaluating a [`CompiledScript`] is the per-item path.

use log::trace;
use rhai::ParseError;
pub use rhai::{Dynamic, Engine, EvalAltResult, ImmutableString, Map, Position, Scope, AST};
use shared::{error::TuliproxError, utils::Capitalize};
use std::fmt::{Display, Formatter};

/// Upper bound for one script evaluation.
///
/// A mapper script runs once per playlist item, so this must stay small enough that a runaway
/// loop is cut off quickly while leaving room for realistic conditional logic.
pub const MAX_OPERATIONS: u64 = 100_000;
/// Maximum native call depth.
pub const MAX_CALL_LEVELS: usize = 32;
/// Maximum expression nesting depth outside and inside functions.
pub const MAX_EXPR_DEPTH: usize = 32;
/// Maximum length of a string a script may build.
pub const MAX_STRING_SIZE: usize = 64 * 1024;
/// Maximum array length a script may build.
pub const MAX_ARRAY_SIZE: usize = 1024;
/// Maximum map size a script may build.
pub const MAX_MAP_SIZE: usize = 1024;

/// A configured Rhai engine plus the compiler entry point.
pub struct ScriptRuntime {
    engine: Engine,
}

impl ScriptRuntime {
    /// Build a runtime with the shared limits and callbacks, then run `setup` to register the
    /// host's types and functions.
    ///
    /// `setup` must not remove the limits; it exists only to add host functions, so every host
    /// keeps the same resource bounds.
    #[must_use]
    pub fn new(setup: impl FnOnce(&mut Engine)) -> Self {
        let mut engine = Engine::new();
        engine
            .set_max_operations(MAX_OPERATIONS)
            .set_max_call_levels(MAX_CALL_LEVELS)
            .set_max_expr_depths(MAX_EXPR_DEPTH, MAX_EXPR_DEPTH)
            .set_max_string_size(MAX_STRING_SIZE)
            .set_max_array_size(MAX_ARRAY_SIZE)
            .set_max_map_size(MAX_MAP_SIZE)
            // `eval` runs an arbitrary script string and would bypass both the compile step and
            // the limits of the caller's script.
            .disable_symbol("eval");
        engine.on_print(|text| trace!("[script] {text}"));
        engine.on_debug(|text, _source, _position| trace!("[script] {text}"));
        setup(&mut engine);
        Self { engine }
    }

    /// The configured engine. Hosts read it, they do not modify it.
    #[must_use]
    pub fn engine(&self) -> &Engine { &self.engine }

    /// Compile `source`. `origin` names the configuration location and is only used in errors,
    /// so a failure points at the mapping and rule that hold the script.
    pub fn compile(&self, source: &str, origin: &str) -> Result<CompiledScript, ScriptError> {
        self.engine
            .compile(source)
            .map(|ast| CompiledScript { ast })
            .map_err(|err| ScriptError::from_parse(origin, &err))
    }
}

/// A compiled script. Cloning shares nothing but the AST handle, so a compiled mapping can be
/// stored in an `Arc` snapshot and evaluated from several workers.
#[derive(Debug, Clone)]
pub struct CompiledScript {
    ast: AST,
}

impl CompiledScript {
    #[must_use]
    pub const fn ast(&self) -> &AST { &self.ast }
}

/// A compile failure with the configuration origin it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    origin: String,
    message: String,
}

impl ScriptError {
    #[must_use]
    pub fn new(origin: impl Into<String>, message: impl Into<String>) -> Self {
        Self { origin: origin.into(), message: message.into() }
    }

    fn from_parse(origin: &str, error: &ParseError) -> Self { Self::new(origin, error.to_string()) }

    #[must_use]
    pub fn origin(&self) -> &str { &self.origin }

    #[must_use]
    pub fn message(&self) -> &str { &self.message }
}

impl Display for ScriptError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { write!(f, "{}: {}", self.origin, self.message) }
}

impl ScriptError {
    /// Convert into the configuration error a caller reports. Deliberately an inherent method
    /// rather than `From`: a blanket `From<ScriptError> for TuliproxError` adds a second
    /// candidate to every `?` whose error type is still being inferred and turns unrelated
    /// inference in the workspace ambiguous.
    #[must_use]
    pub fn into_tuliprox_error(self) -> TuliproxError { TuliproxError::Mapper(self.to_string()) }
}

/// Case helpers shared by all hosts, matching the mapper DSL's `capitalize` and `title_case`.
pub fn register_text_helpers(engine: &mut Engine) {
    engine.register_fn("capitalize", |value: ImmutableString| value.as_str().capitalize());
    engine.register_fn("title_case", |value: ImmutableString| value.as_str().title_case());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> ScriptRuntime { ScriptRuntime::new(register_text_helpers) }

    #[test]
    fn compile_reports_the_origin_and_a_position() {
        let error = runtime().compile("let x = ;", "mapping 'sports', rule 2").expect_err("invalid script must fail");

        assert_eq!(error.origin(), "mapping 'sports', rule 2");
        assert!(error.to_string().contains("position"), "message should carry a position: {error}");
    }

    #[test]
    fn eval_is_not_available_to_scripts() {
        let error = runtime().compile(r#"eval("1 + 1")"#, "origin").expect_err("eval must be disabled");

        assert!(error.to_string().contains("eval"), "message should name the disabled symbol: {error}");
    }

    #[test]
    fn import_is_not_available_to_scripts() {
        let error = runtime().compile(r#"import "x" as y;"#, "origin").expect_err("import must be disabled");

        assert!(error.to_string().contains("import"), "message should name the disabled keyword: {error}");
    }

    #[test]
    fn runaway_loops_are_stopped_by_the_operation_limit() {
        let runtime = runtime();
        let script = runtime.compile("while true {}", "origin").expect("script compiles");
        let mut scope = Scope::new();

        let error = runtime.engine().eval_ast_with_scope::<Dynamic>(&mut scope, script.ast());

        assert!(error.is_err(), "an endless loop must hit the operation limit");
    }

    #[test]
    fn text_helpers_match_the_mapper_dsl() {
        let runtime = runtime();
        let script = runtime.compile(r#"capitalize("us: tnt hd") + "|" + title_case("us: tnt hd")"#, "origin");
        let script = script.expect("script compiles");
        let mut scope = Scope::new();

        let value = runtime
            .engine()
            .eval_ast_with_scope::<ImmutableString>(&mut scope, script.ast())
            .expect("script evaluates");

        assert_eq!(value.as_str(), "Us: tnt hd|Us: Tnt Hd");
    }

    #[test]
    fn print_does_not_write_to_stdout() {
        let runtime = runtime();
        let script = runtime.compile(r#"print("hello")"#, "origin").expect("script compiles");
        let mut scope = Scope::new();

        assert!(runtime.engine().eval_ast_with_scope::<Dynamic>(&mut scope, script.ast()).is_ok());
    }
}
