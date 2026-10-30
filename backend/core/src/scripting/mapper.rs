//! Rhai host for mapping rules.
//!
//! The host exposes exactly two names to a script:
//!
//! * `item` — the playlist item under mapping. Fields are read and written with the same
//!   `HeaderField` semantics as the mapper DSL, so `item.caption = ..` updates both `name` and
//!   `title`, `item.chno` is a number, and `item.input`/`item.type` are read-only.
//! * `ctx` — the mapping environment. Templates, regex evaluation and favourite aliases belong to
//!   the mapping, not to the item, so they are explicit context methods:
//!   `ctx.template(name)`, `ctx.regex(text, pattern)` and `ctx.favourite(item, group)`.
//!
//! The item is moved into the evaluation scope and moved back out, so a script never copies the
//! playlist item on the hot path.

use shared::{
    error::TuliproxError,
    model::{
        FieldGet, FieldRef, FieldSet, HeaderField, PatternTemplate, PlaylistItem, PlaylistItemType, TemplateValue,
        MAPPER_FIELDS, REGEX_CACHE,
    },
    utils::{create_alias_uuid, deunicode_string, Internable},
};
use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
};
use tuliprox_scripting::{CompiledScript, Dynamic, Engine, EvalAltResult, ImmutableString, Map, Scope, ScriptRuntime};

/// Header fields a script may address.
///
/// `MAPPER_FIELDS` is the allow-list of the legacy `MapperOperation` config type, not the mapper
/// DSL's vocabulary: the DSL grammar accepts `@genre` while `MAPPER_FIELDS` omits it. The two
/// read-only fields and `genre` are therefore added here, so a script sees exactly what the DSL
/// sees.
fn script_fields() -> impl Iterator<Item = HeaderField> {
    MAPPER_FIELDS.iter().copied().chain([HeaderField::Input, HeaderField::Type, HeaderField::Genre])
}

/// Resolve a field name a script used, accepting only the fields in [`script_fields`].
///
/// `HeaderField::parse` also knows fields the DSL does not expose, such as `provider_id`; the
/// indexer and the property fallback must not reach them.
fn script_field(name: &str) -> Option<HeaderField> {
    HeaderField::parse(name).filter(|field| script_fields().any(|allowed| allowed == *field))
}

const fn is_writable(field: HeaderField) -> bool {
    !matches!(field, HeaderField::Input | HeaderField::Type | HeaderField::ProviderId)
}

/// The item as a script sees it: the playlist item plus the fields it assigned.
///
/// Tracking assignments here, instead of comparing every field afterwards, keeps the mapping
/// statistics and the `changed_fields` diagnostics identical to the DSL path.
#[derive(Debug, Clone)]
pub struct MapperItem {
    pli: PlaylistItem,
    changed: Vec<&'static str>,
    /// Assignments the item refused although the field itself is valid, for example `genre` on a
    /// live channel, a non-numeric `chno` or a value that is neither a string nor an integer. The
    /// DSL logs and continues in the same situation, so
    /// these do not abort the script; they surface as mapping diagnostics afterwards.
    rejected: Vec<String>,
}

impl MapperItem {
    #[must_use]
    pub const fn new(pli: PlaylistItem) -> Self { Self { pli, changed: Vec::new(), rejected: Vec::new() } }

    #[must_use]
    pub fn into_parts(self) -> (PlaylistItem, Vec<String>, Vec<String>) {
        (self.pli, self.changed.into_iter().map(str::to_string).collect(), self.rejected)
    }

    fn read(&self, field: HeaderField) -> Dynamic {
        match self.pli.header.get(field) {
            // `chno` stays numeric so scripts can compare and add without parsing a string.
            Some(FieldRef::Num(value)) => Dynamic::from(i64::from(value)),
            Some(value) => Dynamic::from(value.as_cow().into_owned()),
            None => Dynamic::UNIT,
        }
    }

    fn write(&mut self, field: HeaderField, value: &Dynamic) -> Result<(), Box<EvalAltResult>> {
        if !is_writable(field) {
            return Err(format!("field '{}' is read-only", field.as_str()).into());
        }
        // A value of the wrong type, typically `()` from a regex capture that did not match, is
        // refused like any other value the item cannot carry: recorded, and the script continues.
        let Some(text) = field_text(value) else {
            self.rejected.push(format!(
                "could not assign {}: expected a string or integer, got {}",
                field.as_str(),
                value.type_name()
            ));
            return Ok(());
        };
        if self.pli.header.set(field, &text) {
            let name = field.as_str();
            if !self.changed.iter().any(|changed| changed.eq_ignore_ascii_case(name)) {
                self.changed.push(name);
            }
            Ok(())
        } else {
            // The field name is valid and writable, so the item itself refused the value: a live
            // channel cannot carry a genre, and `chno` only accepts numbers. Record it and let the
            // script continue, matching the DSL's log-and-continue behaviour.
            self.rejected.push(format!("could not assign {} = {text:?} on this item", field.as_str()));
            Ok(())
        }
    }
}

/// Convert a script value into the string a header field stores.
///
/// Numbers are accepted for numeric fields such as `chno`; the field setter rejects values that
/// do not parse. Everything else yields `None` instead of being silently stringified, so a script
/// that assigns a map, an array or `()` gets a diagnostic rather than a field holding `"()"`.
fn field_text(value: &Dynamic) -> Option<String> {
    if let Some(text) = value.read_lock::<ImmutableString>() {
        return Some(text.to_string());
    }
    value.as_int().ok().map(|number| number.to_string())
}

/// The mapping environment a script receives as `ctx`.
#[derive(Debug, Clone, Default)]
pub struct MappingScriptContext {
    templates: Option<Arc<[PatternTemplate]>>,
    match_as_ascii: bool,
    virtual_items: Vec<PlaylistItem>,
}

impl MappingScriptContext {
    #[must_use]
    pub fn new(templates: Option<Arc<[PatternTemplate]>>, match_as_ascii: bool) -> Self {
        Self { templates, match_as_ascii, virtual_items: Vec::new() }
    }

    #[must_use]
    pub fn virtual_items(&self) -> &[PlaylistItem] { &self.virtual_items }

    fn template(&self, name: &str) -> Dynamic {
        self.templates
            .as_deref()
            .and_then(|templates| {
                templates.iter().rev().find(|template| template.name == name).and_then(|template| {
                    match &template.value {
                        TemplateValue::Single(value) => Some(value.clone()),
                        TemplateValue::Multi(_) => None,
                    }
                })
            })
            .map_or(Dynamic::UNIT, Dynamic::from)
    }

    fn regex(&self, text: &str, pattern: &str) -> Result<Map, String> {
        let regex =
            REGEX_CACHE.get_or_compile(pattern).map_err(|error| format!("invalid regex '{pattern}': {error}"))?;
        let source = if self.match_as_ascii { deunicode_string(text) } else { Cow::Borrowed(text) };
        let mut captures_by_name = Map::new();
        let Some(captures) = regex.captures(&source) else {
            return Ok(captures_by_name);
        };
        for index in 1..captures.len() {
            if let Some(value) = captures.get(index) {
                captures_by_name.insert(index.to_string().into(), Dynamic::from(value.as_str().to_string()));
            }
        }
        for name in regex.capture_names().flatten() {
            if let Some(value) = captures.name(name) {
                captures_by_name.insert(name.into(), Dynamic::from(value.as_str().to_string()));
            }
        }
        Ok(captures_by_name)
    }

    /// Add the current item as an extra row in `group`, mirroring the DSL's `add_favourite`.
    ///
    /// The argument is a copy of the item on purpose: the alias row *is* a copy, and the caller's
    /// item keeps its own group and UUID.
    fn favourite(&mut self, item: MapperItem, group: &str) {
        if matches!(item.pli.header.item_type, PlaylistItemType::Series | PlaylistItemType::LocalSeries) {
            return;
        }
        let mut alias = item.pli;
        alias.header.group = group.intern();
        alias.header.uuid = create_alias_uuid(&alias.header.uuid, group);
        self.virtual_items.push(alias);
    }
}

/// A compiled mapper script.
#[derive(Debug, Clone)]
pub struct MappingScript {
    script: CompiledScript,
}

impl MappingScript {
    /// Compile a script. The caller adds the mapping and rule context to the error.
    pub fn compile(source: &str) -> Result<Self, TuliproxError> {
        let script =
            runtime().compile(source, "mapper script").map_err(tuliprox_scripting::ScriptError::into_tuliprox_error)?;
        Ok(Self { script })
    }

    /// Run the script for one item.
    ///
    /// `item` is `None` only when the script destroyed the reserved `item` binding. The channel
    /// cannot be recovered in that case and the caller discards it.
    ///
    /// A script that merely *shadows* `item` (`let item = item.name;`) keeps working: the host
    /// binding is popped from the scope by value, so the shadowed value is skipped and the real
    /// item is recovered instead of being thrown away.
    #[must_use]
    pub fn eval(&self, item: PlaylistItem, context: MappingScriptContext) -> MappingScriptOutcome {
        let mut scope = Scope::new();
        scope.push("item", MapperItem::new(item));
        scope.push("ctx", context);

        let mut diagnostics = Vec::new();
        if let Err(error) = runtime().engine().eval_ast_with_scope::<Dynamic>(&mut scope, self.script.ast()) {
            diagnostics.push(format!("script error: {error}"));
        }

        let context = take_binding::<MappingScriptContext>(&mut scope, "ctx").unwrap_or_else(|| {
            diagnostics.push("script error: the script replaced the reserved variable 'ctx'".to_string());
            MappingScriptContext::default()
        });
        let mapped = take_binding::<MapperItem>(&mut scope, "item");
        let (item, changed_fields) = if let Some(mapped) = mapped {
            let (item, changed, rejected) = mapped.into_parts();
            diagnostics.extend(rejected);
            (Some(item), changed)
        } else {
            diagnostics.push(
                "script error: the script replaced the reserved variable 'item'; the channel was discarded".to_string(),
            );
            (None, Vec::new())
        };
        MappingScriptOutcome { item, virtual_items: context.virtual_items, changed_fields, diagnostics }
    }
}

/// Remove the host binding `name` from `scope`, skipping values a script pushed under the same
/// name.
///
/// Rhai resolves a name to the innermost binding and `Scope::remove` deletes only that one, so a
/// script that writes `let item = item.name;` would otherwise hide the real item. Shadowing is
/// harmless and must not cost the channel; only a value that no longer exists at all is lost.
fn take_binding<T: 'static>(scope: &mut Scope, name: &str) -> Option<T> {
    while let Some(value) = scope.remove::<Dynamic>(name) {
        if let Some(found) = value.try_cast::<T>() {
            return Some(found);
        }
    }
    None
}

/// What one script run produced. `item` is `None` when the script destroyed the item.
#[derive(Debug, Default)]
pub struct MappingScriptOutcome {
    pub item: Option<PlaylistItem>,
    pub virtual_items: Vec<PlaylistItem>,
    pub changed_fields: Vec<String>,
    pub diagnostics: Vec<String>,
}

fn runtime() -> &'static ScriptRuntime {
    static RUNTIME: OnceLock<ScriptRuntime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        ScriptRuntime::new(|engine| {
            tuliprox_scripting::register_text_helpers(engine);
            register_mapper_api(engine);
        })
    })
}

fn register_mapper_api(engine: &mut Engine) {
    for field in script_fields() {
        engine.register_get(field.as_str(), move |item: &mut MapperItem| item.read(field));
        if is_writable(field) {
            engine.register_set(field.as_str(), move |item: &mut MapperItem, value: Dynamic| item.write(field, &value));
        }
    }

    // `item["caption"]` resolves through the same table as `item.caption`, so a field spelling
    // that works in the DSL keeps working here.
    engine.register_indexer_get(|item: &mut MapperItem, name: ImmutableString| -> Dynamic {
        script_field(&name).map_or(Dynamic::UNIT, |field| item.read(field))
    });
    engine.register_indexer_set(
        |item: &mut MapperItem, name: ImmutableString, value: Dynamic| -> Result<(), Box<EvalAltResult>> {
            let field = script_field(&name).ok_or_else(|| format!("unknown field '{name}'"))?;
            item.write(field, &value)
        },
    );

    engine.register_fn("template", |context: &mut MappingScriptContext, name: ImmutableString| context.template(&name));
    engine.register_fn(
        "regex",
        |context: &mut MappingScriptContext,
         text: ImmutableString,
         pattern: ImmutableString|
         -> Result<Map, Box<EvalAltResult>> { context.regex(&text, &pattern).map_err(Into::into) },
    );
    engine.register_fn("favourite", |context: &mut MappingScriptContext, item: MapperItem, group: ImmutableString| {
        context.favourite(item, &group);
    });
}

#[cfg(test)]
mod tests;
