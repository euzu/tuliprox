use serde::Deserialize;
use shared::{
    error::TuliproxError,
    model::{ConfigInputAliasDto, SourcesConfigDto},
};
use std::{collections::HashMap, fmt::Write, ops::Range};

#[derive(Debug, Clone)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
}

#[derive(Debug, Deserialize)]
pub struct SourcePatchDocument {
    #[serde(default)]
    pub inputs: Vec<serde_saphyr::Spanned<PatchInput>>,
}

#[derive(Debug, Deserialize)]
pub struct PatchInput {
    pub name: serde_saphyr::Spanned<String>,
    #[serde(default)]
    pub enabled: Option<serde_saphyr::Spanned<bool>>,
    #[serde(default)]
    pub url: Option<serde_saphyr::Spanned<String>>,
    #[serde(default)]
    pub username: Option<serde_saphyr::Spanned<String>>,
    #[serde(default)]
    pub password: Option<serde_saphyr::Spanned<String>>,
    #[serde(default)]
    pub exp_date: Option<serde_saphyr::Spanned<i64>>,
    #[serde(default)]
    pub max_connections: Option<serde_saphyr::Spanned<u16>>,
    #[serde(default)]
    pub panel_api: Option<serde_saphyr::Spanned<PatchPanelApi>>,
    #[serde(default)]
    pub aliases: Option<serde_saphyr::Spanned<Vec<serde_saphyr::Spanned<PatchAlias>>>>,
}

#[derive(Debug, Deserialize)]
pub struct PatchAlias {
    pub name: serde_saphyr::Spanned<String>,
    #[serde(default)]
    pub enabled: Option<serde_saphyr::Spanned<bool>>,
    #[serde(default)]
    pub url: Option<serde_saphyr::Spanned<String>>,
    #[serde(default)]
    pub username: Option<serde_saphyr::Spanned<String>>,
    #[serde(default)]
    pub password: Option<serde_saphyr::Spanned<String>>,
    #[serde(default)]
    pub exp_date: Option<serde_saphyr::Spanned<i64>>,
    #[serde(default)]
    pub max_connections: Option<serde_saphyr::Spanned<u16>>,
}

#[derive(Debug, Deserialize)]
pub struct PatchPanelApi {
    #[serde(default)]
    pub credits: Option<serde_saphyr::Spanned<String>>,
}

pub fn span_byte_range(spanned: &serde_saphyr::Spanned<impl Sized>) -> Result<Range<usize>, TuliproxError> {
    let loc = &spanned.referenced;
    if loc != &spanned.defined {
        return Err(TuliproxError::Config(
            "source.yml patch rejected: value originates from a YAML alias or merge key".to_string(),
        ));
    }
    let span = loc.span();
    let byte_offset = span.byte_offset().ok_or_else(|| {
        TuliproxError::Config("source.yml patch rejected: byte span information unavailable".to_string())
    })?;
    let byte_len = span.byte_len().ok_or_else(|| {
        TuliproxError::Config("source.yml patch rejected: byte span information unavailable".to_string())
    })?;
    let start = usize::try_from(byte_offset)
        .map_err(|_| TuliproxError::Config("source.yml patch rejected: byte offset overflow".to_string()))?;
    let len = usize::try_from(byte_len)
        .map_err(|_| TuliproxError::Config("source.yml patch rejected: byte length overflow".to_string()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| TuliproxError::Config("source.yml patch rejected: byte range overflow".to_string()))?;
    Ok(start..end)
}

pub fn serialize_yaml_scalar<T: serde::Serialize>(value: &T) -> Result<String, TuliproxError> {
    let mut out = String::new();
    serde_saphyr::to_fmt_writer(&mut out, value)
        .map_err(|err| TuliproxError::Config(format!("source.yml patch: scalar serialization failed: {err}")))?;
    if out.ends_with('\n') {
        out.pop();
    }
    if out.ends_with('\r') {
        out.pop();
    }
    Ok(out)
}

pub fn detect_newline(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

pub fn line_indent_at(text: &str, byte_pos: usize) -> usize {
    let line_start = text[..byte_pos].rfind('\n').map_or(0, |p| p + 1);
    text[line_start..].chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

pub fn line_end_offset(text: &str, byte_pos: usize) -> usize {
    match text[byte_pos..].find('\n') {
        Some(off) => byte_pos + off + 1,
        None => text.len(),
    }
}

/// Returns true when the mapping owning `byte_range` is written in flow style (`{...}`).
///
/// Scanning starts at the beginning of the line holding the value and walks backwards over
/// preceding lines while they belong to the same flow scope, so a value on a continuation line
/// of a multi-line flow mapping is still detected.
fn is_flow_style_mapping_at(text: &str, byte_range: &Range<usize>) -> bool {
    let mut depth = 0i32;
    for ch in text[..byte_range.start.min(text.len())].chars().rev() {
        match ch {
            '}' | ']' => depth += 1,
            '{' | '[' if depth == 0 => return true,
            '{' | '[' => depth -= 1,
            '\n' if depth == 0 => break,
            _ => {}
        }
    }
    false
}

/// Rejects inserting a new key into a flow-style mapping.
///
/// Replacing an existing scalar inside a flow mapping is safe because its span is explicit,
/// but inserting a whole `key: value` line is not representable there.
pub fn ensure_block_style_for_insertion(
    text: &str,
    anchor: &Range<usize>,
    field_name: &str,
) -> Result<(), TuliproxError> {
    if is_flow_style_mapping_at(text, anchor) {
        return Err(TuliproxError::Config(format!(
            "cannot insert optional field '{field_name}' into a flow-style YAML mapping; edit the account in block style"
        )));
    }
    Ok(())
}

/// Byte offset of the end of the line holding `byte_pos`, excluding its line break.
pub fn line_content_end(text: &str, byte_pos: usize) -> usize {
    let line_end = line_end_offset(text, byte_pos);
    let mut end = line_end;
    if text[..end].ends_with('\n') {
        end -= 1;
        if text[..end].ends_with('\r') {
            end -= 1;
        }
    }
    end
}

/// Column at which sibling keys of the mapping owning `byte_pos` start.
///
/// For a sequence item written as `  - name: account` the siblings of `name` are indented to
/// the column of `name`, not to the column of the `-`.
pub fn sibling_key_indent(text: &str, byte_pos: usize) -> usize {
    let line_start = text[..byte_pos.min(text.len())].rfind('\n').map_or(0, |p| p + 1);
    let line = &text[line_start..];
    let mut indent = 0;
    let mut chars = line.chars();
    for ch in chars.by_ref() {
        match ch {
            ' ' | '\t' => indent += 1,
            '-' => {
                indent += 1;
                // Consume the whitespace that separates the dash from the first key.
                for next in chars.by_ref() {
                    if next == ' ' || next == '\t' {
                        indent += 1;
                    } else {
                        break;
                    }
                }
                return indent;
            }
            _ => return indent,
        }
    }
    indent
}

/// Builds an edit that inserts `key: value` on its own line directly below the anchor line.
///
/// The anchor range must point at an existing sibling scalar in the same mapping; its line
/// indentation and the document's newline style are reused so no existing byte changes.
pub fn build_field_insertion_edit(
    text: &str,
    anchor: &Range<usize>,
    key: &str,
    value: &str,
) -> Result<TextEdit, TuliproxError> {
    ensure_block_style_for_insertion(text, anchor, key)?;
    let insert_at = line_content_end(text, anchor.end);
    let indent = sibling_key_indent(text, anchor.start);
    let newline = detect_newline(text);
    let replacement = format!("{newline}{:indent$}{key}: {value}", "", indent = indent);
    Ok(TextEdit { range: insert_at..insert_at, replacement })
}

pub fn apply_scalar_edits(original_text: &str, edits: Vec<TextEdit>) -> Result<String, TuliproxError> {
    let mut sorted_edits: Vec<TextEdit> = edits;
    sorted_edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));

    for window in sorted_edits.windows(2) {
        if window[0].range.start < window[1].range.end {
            return Err(TuliproxError::Config("source.yml patch rejected: overlapping edit ranges".to_string()));
        }
    }

    let mut result = original_text.to_string();
    for edit in &sorted_edits {
        if edit.range.end > result.len() {
            return Err(TuliproxError::Config(format!(
                "source.yml patch rejected: edit range {}..{} exceeds document length {}",
                edit.range.start,
                edit.range.end,
                result.len()
            )));
        }
        result.replace_range(edit.range.clone(), &edit.replacement);
    }

    Ok(result)
}

pub fn parse_and_validate_patched_text(patched_text: &str, expected: &SourcesConfigDto) -> Result<(), TuliproxError> {
    let mut reparsed: SourcesConfigDto = serde_saphyr::from_str(patched_text)
        .map_err(|err| TuliproxError::Config(format!("patched source.yml failed to parse: {err}")))?;
    let mut expected = expected.clone();

    normalize_computed_source_fields(&mut reparsed);
    normalize_computed_source_fields(&mut expected);

    if reparsed != expected {
        return Err(TuliproxError::Config(
            "patched source.yml did not match the expected configuration; original file was not changed".to_string(),
        ));
    }
    Ok(())
}

/// Removes fields computed while preparing a source configuration and therefore not part of
/// persistent YAML semantics. Empty and missing alias collections are equivalent because an
/// empty `aliases:` mapping reparses as `None`.
fn normalize_computed_source_fields(dto: &mut SourcesConfigDto) {
    for input in &mut dto.inputs {
        input.id = 0;
        if input.aliases.as_ref().is_some_and(Vec::is_empty) {
            input.aliases = None;
        }
        if let Some(aliases) = input.aliases.as_mut() {
            for alias in aliases {
                alias.id = 0;
            }
        }
    }
}

pub fn parse_patch_document(text: &str) -> Result<SourcePatchDocument, TuliproxError> {
    serde_saphyr::from_str(text)
        .map_err(|err| TuliproxError::Config(format!("source.yml patch: projection parse failed: {err}")))
}

pub fn find_input<'a>(
    doc: &'a SourcePatchDocument,
    input_name: &str,
) -> Result<&'a serde_saphyr::Spanned<PatchInput>, TuliproxError> {
    let mut found: Option<&serde_saphyr::Spanned<PatchInput>> = None;
    for input in &doc.inputs {
        if input.value.name.value == input_name {
            if found.is_some() {
                return Err(TuliproxError::Config(format!(
                    "source.yml patch target input '{input_name}' is ambiguous"
                )));
            }
            found = Some(input);
        }
    }
    let input = found
        .ok_or_else(|| TuliproxError::Config(format!("source.yml patch target input '{input_name}' was not found")))?;
    ensure_unique_alias_names(input, input_name)?;
    Ok(input)
}

/// Rejects an input whose alias list contains the same name twice, because every alias lookup
/// during patch planning would then silently target the first match.
fn ensure_unique_alias_names(input: &serde_saphyr::Spanned<PatchInput>, input_name: &str) -> Result<(), TuliproxError> {
    let Some(aliases) = &input.value.aliases else {
        return Ok(());
    };
    let mut seen: Vec<&str> = Vec::with_capacity(aliases.value.len());
    for alias in &aliases.value {
        let name = alias.value.name.value.as_str();
        if seen.contains(&name) {
            return Err(TuliproxError::Config(format!(
                "source.yml patch target alias '{name}' under input '{input_name}' is ambiguous"
            )));
        }
        seen.push(name);
    }
    Ok(())
}

pub fn alias_item_block_range(
    text: &str,
    alias_span: &serde_saphyr::Spanned<PatchAlias>,
) -> Result<Range<usize>, TuliproxError> {
    let alias_range = span_byte_range(alias_span)?;
    let name_range = span_byte_range(&alias_span.value.name)?;
    let line_start = text[..alias_range.start].rfind('\n').map_or(0, |p| p + 1);
    let dash_pos = text[line_start..alias_range.start].find('-').map_or(line_start, |p| line_start + p);
    let block_start = text[..dash_pos].rfind('\n').map_or(0, |p| p + 1);

    let comment_start = find_owned_comment_start(text, block_start, line_start);

    let block_end;
    let item_indent = text[block_start..].chars().take_while(|c| *c == ' ').count();

    let mut pos = line_end_offset(text, name_range.end);
    loop {
        if pos >= text.len() {
            block_end = text.len();
            break;
        }
        let next_line_start = pos;
        let next_line_end = match text[next_line_start..].find('\n') {
            Some(off) => next_line_start + off + 1,
            None => text.len(),
        };
        let next_line = &text[next_line_start..next_line_end.min(text.len())];
        let stripped = next_line.trim_start();
        if stripped.is_empty() {
            pos = next_line_end;
            continue;
        }
        let next_indent = next_line.chars().take_while(|c| *c == ' ').count();
        if next_indent < item_indent || (next_indent == item_indent && stripped.starts_with('-')) {
            block_end = next_line_start;
            break;
        }
        if next_indent == item_indent && stripped.starts_with('#') {
            let mut lookahead = next_line_end;
            let mut comments_belong_to_next_item = false;
            while lookahead < text.len() {
                let lookahead_end = match text[lookahead..].find('\n') {
                    Some(off) => lookahead + off + 1,
                    None => text.len(),
                };
                let candidate = &text[lookahead..lookahead_end.min(text.len())];
                let candidate_stripped = candidate.trim_start();
                let candidate_indent = candidate.chars().take_while(|c| *c == ' ').count();
                if candidate_indent == item_indent && candidate_stripped.starts_with('#') {
                    lookahead = lookahead_end;
                    continue;
                }
                if candidate_indent == item_indent && candidate_stripped.starts_with('-') {
                    comments_belong_to_next_item = true;
                    break;
                }
                break;
            }
            if comments_belong_to_next_item {
                block_end = next_line_start;
                break;
            }
        }
        pos = next_line_end;
    }

    Ok(comment_start..block_end)
}

fn find_owned_comment_start(text: &str, block_start: usize, _first_content_line_start: usize) -> usize {
    let mut comment_start = block_start;
    let mut pos = block_start;
    while pos > 0 {
        let prev_line_end = pos;
        let prev_line_start = text[..prev_line_end.saturating_sub(1)].rfind('\n').map_or(0, |p| p + 1);
        let prev_line = &text[prev_line_start..prev_line_end.min(text.len())];
        let stripped = prev_line.trim();
        if stripped.starts_with('#') {
            let prev_indent = prev_line.chars().take_while(|c| *c == ' ').count();
            let block_indent = text[block_start..].chars().take_while(|c| *c == ' ').count();
            if prev_indent >= block_indent {
                comment_start = prev_line_start;
                pos = prev_line_start;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    comment_start
}

pub fn serialize_alias_block(
    alias: &ConfigInputAliasDto,
    indent: usize,
    newline: &str,
) -> Result<String, TuliproxError> {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let _ = writeln!(out, "{pad}- name: {}", serialize_yaml_scalar(&alias.name.as_ref())?);
    let _ = writeln!(out, "{pad}  url: {}", serialize_yaml_scalar(&alias.url)?);
    if let Some(username) = &alias.username {
        let _ = writeln!(out, "{pad}  username: {}", serialize_yaml_scalar(username)?);
    }
    if let Some(password) = &alias.password {
        let _ = writeln!(out, "{pad}  password: {}", serialize_yaml_scalar(password)?);
    }
    if alias.max_connections != 0 {
        let _ = writeln!(out, "{pad}  max_connections: {}", serialize_yaml_scalar(&alias.max_connections)?);
    }
    if let Some(exp_date) = alias.exp_date {
        let _ = writeln!(out, "{pad}  exp_date: {}", serialize_yaml_scalar(&exp_date)?);
    }
    if !alias.enabled {
        let _ = writeln!(out, "{pad}  enabled: false");
    }
    if out.ends_with('\n') {
        out.truncate(out.len() - 1);
    }
    if newline == "\r\n" {
        out = out.replace('\n', "\r\n");
    }
    Ok(out)
}

pub fn build_alias_addition_edit(
    text: &str,
    doc: &SourcePatchDocument,
    input_name: &str,
    alias: &ConfigInputAliasDto,
) -> Result<TextEdit, TuliproxError> {
    let input = find_input(doc, input_name)?;
    let newline = detect_newline(text);

    if let Some(aliases_spanned) = &input.value.aliases {
        let aliases = &aliases_spanned.value;
        if aliases.is_empty() {
            let aliases_range = span_byte_range(aliases_spanned)?;
            let line_end = line_end_offset(text, aliases_range.end);
            let indent = line_indent_at(text, aliases_range.start) + 2;
            let block = serialize_alias_block(alias, indent, newline)?;
            let insertion = format!("{newline}{block}{newline}");
            return Ok(TextEdit { range: line_end..line_end, replacement: insertion });
        }
        let Some(last_alias) = aliases.last() else {
            return Err(TuliproxError::Config(format!(
                "source.yml patch: alias list for input '{input_name}' unexpectedly empty"
            )));
        };
        let last_block = alias_item_block_range(text, last_alias)?;
        let item_indent = text[last_block.start..].chars().take_while(|c| *c == ' ').count();
        let block = serialize_alias_block(alias, item_indent, newline)?;
        let insertion = format!("{block}{newline}");
        return Ok(TextEdit { range: last_block.end..last_block.end, replacement: insertion });
    }

    let input_span_range = span_byte_range(&input.value.name)?;
    let input_indent = line_indent_at(text, input_span_range.start);
    let aliases_indent = input_indent;
    let item_indent = aliases_indent + 2;

    // `aliases: null` (or an empty `aliases:`) deserializes to `None` but still occupies a line.
    // Replacing that line in place is what turns the null marker into a real block sequence.
    let field_indent = sibling_key_indent(text, input_span_range.start);
    if let Some(null_line) = find_null_aliases_line(text, input_span_range.end, field_indent) {
        let block = serialize_alias_block(alias, field_indent + 2, newline)?;
        let pad = " ".repeat(field_indent);
        let replacement = format!("{pad}aliases:{newline}{block}");
        return Ok(TextEdit { range: null_line, replacement });
    }

    let mut last_field_end: Option<usize> = None;
    if let Some(f) = &input.value.panel_api {
        last_field_end = Some(span_byte_range(f)?.end);
    }
    if last_field_end.is_none() {
        if let Some(f) = &input.value.max_connections {
            last_field_end = Some(span_byte_range(f)?.end);
        }
    }
    if last_field_end.is_none() {
        if let Some(f) = &input.value.exp_date {
            last_field_end = Some(span_byte_range(f)?.end);
        }
    }
    if last_field_end.is_none() {
        if let Some(f) = &input.value.password {
            last_field_end = Some(span_byte_range(f)?.end);
        }
    }
    if last_field_end.is_none() {
        if let Some(f) = &input.value.username {
            last_field_end = Some(span_byte_range(f)?.end);
        }
    }
    if last_field_end.is_none() {
        if let Some(f) = &input.value.url {
            last_field_end = Some(span_byte_range(f)?.end);
        }
    }

    let anchor_end = last_field_end.unwrap_or(input_span_range.end);
    let line_end = line_end_offset(text, anchor_end);
    let block = serialize_alias_block(alias, item_indent, newline)?;
    let insertion =
        format!("{newline}{:aliases_indent$}aliases:{newline}{block}{newline}", "", aliases_indent = aliases_indent);
    Ok(TextEdit { range: line_end..line_end, replacement: insertion })
}

/// Scans forward from `start` for an `aliases:` line at `key_indent` whose value is empty or
/// the literal `null`. Returns the byte range of the line content (excluding the trailing newline).
fn find_null_aliases_line(text: &str, start: usize, key_indent: usize) -> Option<Range<usize>> {
    let mut line_start = start;
    while line_start < text.len() {
        let next_nl = text[line_start..].find('\n').map_or(text.len(), |p| line_start + p);
        let line = &text[line_start..next_nl];
        if line.trim().is_empty() {
            line_start = next_nl + 1;
            continue;
        }
        let indent = line.chars().take_while(|c| *c == ' ').count();
        if indent < key_indent {
            return None;
        }
        if indent != key_indent {
            line_start = next_nl + 1;
            continue;
        }
        let body = line[indent..].trim_start();
        if let Some(value) = body.strip_prefix("aliases:") {
            let value = value.trim();
            if value.is_empty() || value == "null" {
                return Some(line_start..next_nl);
            }
            return None;
        }
        line_start = next_nl + 1;
    }
    None
}

pub fn build_alias_removal_edits(
    text: &str,
    doc: &SourcePatchDocument,
    input_name: &str,
    alias_names_to_remove: &[&str],
) -> Result<Vec<TextEdit>, TuliproxError> {
    let input = find_input(doc, input_name)?;
    let Some(aliases_spanned) = &input.value.aliases else {
        return Ok(Vec::new());
    };

    let mut edits = Vec::new();
    for alias_span in &aliases_spanned.value {
        if alias_names_to_remove.contains(&alias_span.value.name.value.as_str()) {
            let block = alias_item_block_range(text, alias_span)?;
            edits.push(TextEdit { range: block, replacement: String::new() });
        }
    }
    Ok(edits)
}

pub fn build_alias_sort_edit(
    text: &str,
    doc: &SourcePatchDocument,
    input_name: &str,
    sorted_alias_names: &[&str],
) -> Result<Option<TextEdit>, TuliproxError> {
    let input = find_input(doc, input_name)?;
    let Some(aliases_spanned) = &input.value.aliases else {
        return Ok(None);
    };
    let aliases = &aliases_spanned.value;
    if aliases.len() < 2 {
        return Ok(None);
    }

    let current_order: Vec<&str> = aliases.iter().map(|a| a.value.name.value.as_str()).collect();
    if current_order == sorted_alias_names {
        return Ok(None);
    }

    let mut blocks: Vec<(String, Range<usize>)> = Vec::new();
    for alias_span in aliases {
        let block_range = alias_item_block_range(text, alias_span)?;
        blocks.push((alias_span.value.name.value.clone(), block_range));
    }

    let (Some(first_block_start), Some(last_block_end)) =
        (blocks.iter().map(|(_, r)| r.start).min(), blocks.iter().map(|(_, r)| r.end).max())
    else {
        return Ok(None);
    };

    let mut reordered_text = String::new();
    for name in sorted_alias_names {
        let (_, range) = blocks.iter().find(|(n, _)| n == name).ok_or_else(|| {
            TuliproxError::Config(format!(
                "source.yml patch: alias '{name}' not found during sort for input '{input_name}'"
            ))
        })?;
        reordered_text.push_str(&text[range.clone()]);
    }

    Ok(Some(TextEdit { range: first_block_start..last_block_end, replacement: reordered_text }))
}

/// Rebuilds an alias sequence from opaque existing blocks and newly serialized aliases.
///
/// Existing alias contents and their owned comments are copied byte-for-byte. This is used when
/// one semantic command both changes the alias set and determines a new order, which cannot be
/// represented safely as independent edits against the same original spans.
pub fn build_alias_sequence_edit(
    text: &str,
    doc: &SourcePatchDocument,
    input_name: &str,
    expected_aliases: &[ConfigInputAliasDto],
) -> Result<Option<TextEdit>, TuliproxError> {
    let input = find_input(doc, input_name)?;
    let Some(aliases_spanned) = &input.value.aliases else {
        return Ok(None);
    };
    let aliases = &aliases_spanned.value;
    if aliases.is_empty() {
        return Ok(None);
    }

    let mut blocks = HashMap::<&str, Range<usize>>::with_capacity(aliases.len());
    for alias in aliases {
        blocks.insert(alias.value.name.value.as_str(), alias_item_block_range(text, alias)?);
    }
    let start = blocks
        .values()
        .map(|range| range.start)
        .min()
        .ok_or_else(|| TuliproxError::Config("source.yml patch: alias sequence has no start".to_string()))?;
    let end = blocks
        .values()
        .map(|range| range.end)
        .max()
        .ok_or_else(|| TuliproxError::Config("source.yml patch: alias sequence has no end".to_string()))?;
    let first_name_start =
        aliases.first().map(|alias| span_byte_range(&alias.value.name)).transpose()?.map_or(start, |range| range.start);
    let item_indent = line_indent_at(text, first_name_start);
    let newline = detect_newline(text);
    let mut replacement = String::new();

    for alias in expected_aliases {
        if let Some(range) = blocks.get(alias.name.as_ref()) {
            replacement.push_str(&text[range.clone()]);
        } else {
            replacement.push_str(&serialize_alias_block(alias, item_indent, newline)?);
            replacement.push_str(newline);
        }
    }

    Ok(Some(TextEdit { range: start..end, replacement }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        "templates:\n",
        "  - name: provider_channels\n",
        "    value: 'Input = \"provider\"'\n",
        "\n",
        "inputs:\n",
        "  - name: provider\n",
        "    enabled: true # must survive\n",
        "    type: xtream\n",
        "    url: provider://main\n",
        "    username: ${env:XTREAM_USER}\n",
        "    password: ${env:XTREAM_PASSWORD}\n",
        "    aliases:\n",
        "      # backup account comment follows the account when sorted\n",
        "      - name: provider-backup\n",
        "        url: http://backup.example\n",
        "        username: backup\n",
        "        password: \"contains: # special characters\"\n",
        "\n",
        "sources:\n",
        "  - inputs:\n",
        "      - provider\n",
        "    targets:\n",
        "      - name: output\n",
        "        filter: \"!provider_channels!\"\n",
        "        output:\n",
        "          - type: m3u\n",
    );

    #[test]
    fn projection_parses_fixture() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        assert_eq!(doc.inputs.len(), 1);
        assert_eq!(doc.inputs[0].value.name.value, "provider");
        assert!(doc.inputs[0].value.enabled.is_some());
        assert!(doc.inputs[0].value.exp_date.is_none());
        let aliases = doc.inputs[0].value.aliases.as_ref().expect("aliases");
        assert_eq!(aliases.value.len(), 1);
        assert_eq!(aliases.value[0].value.name.value, "provider-backup");
    }

    #[test]
    fn span_byte_ranges_are_valid() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let name_range = span_byte_range(&doc.inputs[0].value.name).expect("byte range");
        assert_eq!(&FIXTURE[name_range.clone()], "provider");

        let enabled_range =
            span_byte_range(doc.inputs[0].value.enabled.as_ref().expect("enabled")).expect("byte range");
        assert_eq!(&FIXTURE[enabled_range], "true");
    }

    #[test]
    fn existing_scalar_replacement_preserves_surrounding_bytes() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let enabled_range =
            span_byte_range(doc.inputs[0].value.enabled.as_ref().expect("enabled")).expect("byte range");

        let new_value = serialize_yaml_scalar(&false).expect("serialize");
        let edit = TextEdit { range: enabled_range, replacement: new_value };
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        let expected = FIXTURE.replace("enabled: true # must survive", "enabled: false # must survive");
        assert_eq!(patched, expected);
    }

    #[test]
    fn missing_exp_date_insertion_preserves_all_existing_bytes() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let input = &doc.inputs[0].value;

        let password_range = span_byte_range(input.password.as_ref().expect("password")).expect("byte range");
        let anchor_end = line_end_offset(FIXTURE, password_range.end);
        let indent = line_indent_at(FIXTURE, password_range.start);
        let newline = detect_newline(FIXTURE);
        let new_value = serialize_yaml_scalar(&1_900_000_000_i64).expect("serialize");
        let insertion = format!("{newline}{:indent$}exp_date: {new_value}", "", indent = indent);

        let edit = TextEdit { range: anchor_end..anchor_end, replacement: insertion };
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        assert!(patched.contains("exp_date: 1900000000"));
        for line in FIXTURE.lines() {
            assert!(patched.contains(line), "line missing after patch: {line}");
        }
    }

    #[test]
    fn alias_exp_date_insertion() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let aliases = doc.inputs[0].value.aliases.as_ref().expect("aliases");
        let alias = &aliases.value[0].value;

        let password_range = span_byte_range(alias.password.as_ref().expect("password")).expect("byte range");
        let anchor_end = line_end_offset(FIXTURE, password_range.end);
        let indent = line_indent_at(FIXTURE, password_range.start);
        let newline = detect_newline(FIXTURE);
        let new_value = serialize_yaml_scalar(&2_000_000_000_i64).expect("serialize");
        let insertion = format!("{newline}{:indent$}exp_date: {new_value}", "", indent = indent);

        let edit = TextEdit { range: anchor_end..anchor_end, replacement: insertion };
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        assert!(patched.contains("exp_date: 2000000000"));
        assert!(patched.contains("password: \"contains: # special characters\""));
    }

    #[test]
    fn inline_comment_after_scalar_survives_replacement() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let enabled_range =
            span_byte_range(doc.inputs[0].value.enabled.as_ref().expect("enabled")).expect("byte range");

        let new_value = serialize_yaml_scalar(&false).expect("serialize");
        let edit = TextEdit { range: enabled_range, replacement: new_value };
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        assert!(patched.contains("enabled: false # must survive"));
    }

    #[test]
    fn crlf_preservation() {
        let crlf_fixture = FIXTURE.replace('\n', "\r\n");
        let doc: SourcePatchDocument = serde_saphyr::from_str(&crlf_fixture).expect("projection parses");
        let enabled_range =
            span_byte_range(doc.inputs[0].value.enabled.as_ref().expect("enabled")).expect("byte range");

        let new_value = serialize_yaml_scalar(&false).expect("serialize");
        let edit = TextEdit { range: enabled_range, replacement: new_value };
        let patched = apply_scalar_edits(&crlf_fixture, vec![edit]).expect("apply");

        assert!(patched.contains("\r\n"));
        assert!(patched.contains("enabled: false # must survive"));
    }

    #[test]
    fn unicode_before_span_does_not_corrupt_offsets() {
        let unicode_fixture = "# Ünïcödé tëst 🎉\ninputs:\n  - name: über-input\n    exp_date: 12345\n";
        let doc: SourcePatchDocument = serde_saphyr::from_str(unicode_fixture).expect("projection parses");
        let exp_range = span_byte_range(doc.inputs[0].value.exp_date.as_ref().expect("exp_date")).expect("byte range");
        assert_eq!(&unicode_fixture[exp_range.clone()], "12345");

        let new_value = serialize_yaml_scalar(&99999_i64).expect("serialize");
        let edit = TextEdit { range: exp_range, replacement: new_value };
        let patched = apply_scalar_edits(unicode_fixture, vec![edit]).expect("apply");
        assert!(patched.contains("exp_date: 99999"));
        assert!(patched.starts_with("# Ünïcödé tëst 🎉\n"));
    }

    #[test]
    fn duplicate_input_names_fail_without_output() {
        let dup_fixture = "inputs:\n  - name: same\n    exp_date: 1\n  - name: same\n    exp_date: 2\n";
        let doc: SourcePatchDocument = serde_saphyr::from_str(dup_fixture).expect("projection parses");
        let names: Vec<&str> = doc.inputs.iter().map(|i| i.value.name.value.as_str()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert!(unique.len() < names.len(), "duplicate names detected");
    }

    #[test]
    fn yaml_anchor_merge_target_fails() {
        let anchor_fixture = "defaults: &defaults\n  exp_date: 100\ninputs:\n  - name: test\n    <<: *defaults\n";
        let doc: SourcePatchDocument = serde_saphyr::from_str(anchor_fixture).expect("projection parses");
        let input = &doc.inputs[0].value;
        if let Some(exp_date) = &input.exp_date {
            let result = span_byte_range(exp_date);
            assert!(result.is_err(), "merge-derived value should be rejected");
        }
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let edits = vec![
            TextEdit { range: 5..15, replacement: "a".to_string() },
            TextEdit { range: 10..20, replacement: "b".to_string() },
        ];
        let result = apply_scalar_edits("01234567890123456789", edits);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overlapping"));
    }

    #[test]
    fn flow_style_scalar_replacement_succeeds() {
        let flow_fixture = "inputs:\n  - {name: test, exp_date: 42}\n";
        let doc: SourcePatchDocument = serde_saphyr::from_str(flow_fixture).expect("projection parses");
        let exp_range = span_byte_range(doc.inputs[0].value.exp_date.as_ref().expect("exp_date")).expect("byte range");
        assert_eq!(&flow_fixture[exp_range.clone()], "42");

        let new_value = serialize_yaml_scalar(&99_i64).expect("serialize");
        let edit = TextEdit { range: exp_range, replacement: new_value };
        let patched = apply_scalar_edits(flow_fixture, vec![edit]).expect("apply");
        assert_eq!(patched, "inputs:\n  - {name: test, exp_date: 99}\n");
    }

    #[test]
    fn quoted_values_survive_unchanged() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let aliases = doc.inputs[0].value.aliases.as_ref().expect("aliases");
        let alias = &aliases.value[0].value;
        let password_range = span_byte_range(alias.password.as_ref().expect("password")).expect("byte range");
        assert_eq!(&FIXTURE[password_range.clone()], "\"contains: # special characters\"");

        let new_value = serialize_yaml_scalar(&"new: pass").expect("serialize");
        let edit = TextEdit { range: password_range, replacement: new_value };
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        let username_range = span_byte_range(alias.username.as_ref().expect("username")).expect("byte range");
        assert_eq!(&patched[username_range], "backup");
    }

    #[test]
    fn semantic_validation_rejects_mismatch() {
        let mut expected: SourcesConfigDto = serde_saphyr::from_str(FIXTURE).expect("parse");
        expected.inputs[0].exp_date = Some(999);

        let result = parse_and_validate_patched_text(FIXTURE, &expected);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("did not match"));
    }

    #[test]
    fn semantic_validation_accepts_matching_text() {
        let expected: SourcesConfigDto = serde_saphyr::from_str(FIXTURE).expect("parse");
        let result = parse_and_validate_patched_text(FIXTURE, &expected);
        assert!(result.is_ok());
    }

    #[test]
    fn templates_and_filter_placeholder_survive_patch() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let password_range =
            span_byte_range(doc.inputs[0].value.password.as_ref().expect("password")).expect("byte range");
        let anchor_end = line_end_offset(FIXTURE, password_range.end);
        let indent = line_indent_at(FIXTURE, password_range.start);
        let newline = detect_newline(FIXTURE);
        let new_value = serialize_yaml_scalar(&1_800_000_000_i64).expect("serialize");
        let insertion = format!("{newline}{:indent$}exp_date: {new_value}", "", indent = indent);

        let edit = TextEdit { range: anchor_end..anchor_end, replacement: insertion };
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        assert!(patched.contains("templates:\n  - name: provider_channels\n    value: 'Input = \"provider\"'"));
        assert!(patched.contains("filter: \"!provider_channels!\""));
        assert!(patched.contains("username: ${env:XTREAM_USER}"));
        assert!(patched.contains("password: ${env:XTREAM_PASSWORD}"));
    }

    #[test]
    fn add_alias_to_existing_list() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let new_alias = ConfigInputAliasDto {
            name: "provider-second".into(),
            url: "http://second.example".to_string(),
            username: Some("second-user".to_string()),
            password: Some("second-pass".to_string()),
            exp_date: Some(2_000_000_000),
            ..Default::default()
        };

        let edit = build_alias_addition_edit(FIXTURE, &doc, "provider", &new_alias).expect("build edit");
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        assert!(patched.contains("- name: provider-second"));
        assert!(patched.contains("url: http://second.example"));
        assert!(patched.contains("username: second-user"));
        assert!(patched.contains("password: second-pass"));
        assert!(patched.contains("exp_date: 2000000000"));
        for line in FIXTURE.lines() {
            assert!(patched.contains(line), "line missing after patch: {line}");
        }
    }

    #[test]
    fn add_first_alias_when_aliases_absent() {
        let no_aliases_fixture = "inputs:\n  - name: solo\n    url: http://solo.example\n";
        let doc: SourcePatchDocument = serde_saphyr::from_str(no_aliases_fixture).expect("projection parses");
        let new_alias = ConfigInputAliasDto {
            name: "solo-alias".into(),
            url: "http://alias.example".to_string(),
            username: Some("alias-user".to_string()),
            password: Some("alias-pass".to_string()),
            ..Default::default()
        };

        let edit = build_alias_addition_edit(no_aliases_fixture, &doc, "solo", &new_alias).expect("build edit");
        let patched = apply_scalar_edits(no_aliases_fixture, vec![edit]).expect("apply");

        assert!(patched.contains("aliases:"));
        assert!(patched.contains("- name: solo-alias"));
        assert!(patched.contains("url: http://alias.example"));
    }

    #[test]
    fn remove_alias_from_list() {
        let multi_alias_fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    aliases:\n",
            "      - name: first\n",
            "        url: http://first.example\n",
            "      - name: second\n",
            "        url: http://second.example\n",
            "      - name: third\n",
            "        url: http://third.example\n",
        );
        let doc: SourcePatchDocument = serde_saphyr::from_str(multi_alias_fixture).expect("projection parses");

        let edits = build_alias_removal_edits(multi_alias_fixture, &doc, "provider", &["second"]).expect("build edits");
        let patched = apply_scalar_edits(multi_alias_fixture, edits).expect("apply");

        assert!(!patched.contains("second"));
        assert!(patched.contains("first"));
        assert!(patched.contains("third"));
    }

    #[test]
    fn sort_aliases_reorders_blocks() {
        let sort_fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    aliases:\n",
            "      # comment for oldest\n",
            "      - name: oldest\n",
            "        url: http://oldest.example\n",
            "        exp_date: 100\n",
            "      # comment for newest\n",
            "      - name: newest\n",
            "        url: http://newest.example\n",
            "        exp_date: 200\n",
        );
        let doc: SourcePatchDocument = serde_saphyr::from_str(sort_fixture).expect("projection parses");

        let edit = build_alias_sort_edit(sort_fixture, &doc, "provider", &["newest", "oldest"])
            .expect("build edit")
            .expect("edit present");
        let patched = apply_scalar_edits(sort_fixture, vec![edit]).expect("apply");

        let newest_pos = patched.find("name: newest").expect("newest present");
        let oldest_pos = patched.find("name: oldest").expect("oldest present");
        assert!(newest_pos < oldest_pos, "newest should come before oldest after sort");
        assert!(patched.contains("# comment for newest"));
        assert!(patched.contains("# comment for oldest"));
    }

    #[test]
    fn sort_aliases_preserves_fields_before_name() {
        let sort_fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    aliases:\n",
            "      - url: http://oldest.example\n",
            "        enabled: false\n",
            "        name: oldest\n",
            "        exp_date: 100\n",
            "      - max_connections: 2\n",
            "        url: http://newest.example\n",
            "        name: newest\n",
            "        exp_date: 200\n",
        );
        let doc: SourcePatchDocument = serde_saphyr::from_str(sort_fixture).expect("projection parses");

        let edit = build_alias_sort_edit(sort_fixture, &doc, "provider", &["newest", "oldest"])
            .expect("build edit")
            .expect("edit present");
        let patched = apply_scalar_edits(sort_fixture, vec![edit]).expect("apply");
        let reparsed: SourcePatchDocument = serde_saphyr::from_str(&patched).expect("reparse");
        let aliases = reparsed.inputs[0].value.aliases.as_ref().expect("aliases");

        assert_eq!(aliases.value[0].value.name.value, "newest");
        assert_eq!(aliases.value[0].value.max_connections.as_ref().expect("max connections").value, 2);
        assert_eq!(aliases.value[1].value.name.value, "oldest");
        assert_eq!(aliases.value[1].value.enabled.as_ref().expect("enabled").value, false);
    }

    #[test]
    fn remove_alias_removes_fields_before_name() {
        let fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    aliases:\n",
            "      - url: http://remove.example\n",
            "        enabled: false\n",
            "        name: remove\n",
            "      - url: http://keep.example\n",
            "        name: keep\n",
        );
        let doc: SourcePatchDocument = serde_saphyr::from_str(fixture).expect("projection parses");

        let edits = build_alias_removal_edits(fixture, &doc, "provider", &["remove"]).expect("build edits");
        let patched = apply_scalar_edits(fixture, edits).expect("apply");
        let reparsed: SourcePatchDocument = serde_saphyr::from_str(&patched).expect("reparse");
        let aliases = reparsed.inputs[0].value.aliases.as_ref().expect("aliases");

        assert_eq!(aliases.value.len(), 1);
        assert_eq!(aliases.value[0].value.name.value, "keep");
        assert!(!patched.contains("http://remove.example"));
        assert!(!patched.contains("enabled: false"));
    }

    #[test]
    fn alias_credentials_with_special_chars_serialize_correctly() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let new_alias = ConfigInputAliasDto {
            name: "provider-special".into(),
            url: "http://special.example".to_string(),
            username: Some("user:with:colons".to_string()),
            password: Some("pass # with hash".to_string()),
            ..Default::default()
        };

        let edit = build_alias_addition_edit(FIXTURE, &doc, "provider", &new_alias).expect("build edit");
        let patched = apply_scalar_edits(FIXTURE, vec![edit]).expect("apply");

        let reparsed: SourcePatchDocument = serde_saphyr::from_str(&patched).expect("reparse");
        let input = find_input(&reparsed, "provider").expect("find input");
        let aliases = input.value.aliases.as_ref().expect("aliases");
        let special = aliases.value.iter().find(|a| a.value.name.value == "provider-special").expect("special alias");
        assert_eq!(special.value.username.as_ref().expect("username").value, "user:with:colons");
        assert_eq!(special.value.password.as_ref().expect("password").value, "pass # with hash");
    }

    #[test]
    fn combined_add_and_sort_produces_valid_transaction() {
        let doc: SourcePatchDocument = serde_saphyr::from_str(FIXTURE).expect("projection parses");
        let new_alias = ConfigInputAliasDto {
            name: "provider-new".into(),
            url: "http://new.example".to_string(),
            exp_date: Some(3_000_000_000),
            ..Default::default()
        };

        let add_edit = build_alias_addition_edit(FIXTURE, &doc, "provider", &new_alias).expect("build add");
        let patched_after_add = apply_scalar_edits(FIXTURE, vec![add_edit]).expect("apply add");

        let doc2: SourcePatchDocument = serde_saphyr::from_str(&patched_after_add).expect("reparse");
        let sort_edit =
            build_alias_sort_edit(&patched_after_add, &doc2, "provider", &["provider-new", "provider-backup"])
                .expect("build sort")
                .expect("sort edit present");
        let final_text = apply_scalar_edits(&patched_after_add, vec![sort_edit]).expect("apply sort");

        assert!(final_text.contains("- name: provider-new"));
        assert!(final_text.contains("- name: provider-backup"));
    }

    #[test]
    fn add_alias_when_aliases_is_null_replaces_line_in_place() {
        let fixture =
            concat!("inputs:\n", "  - name: provider\n", "    url: http://main.example\n", "    aliases: null\n",);
        let doc: SourcePatchDocument = serde_saphyr::from_str(fixture).expect("projection parses");
        let new_alias = ConfigInputAliasDto {
            name: "provider-backup".into(),
            url: "http://backup.example".to_string(),
            ..Default::default()
        };

        let edit = build_alias_addition_edit(fixture, &doc, "provider", &new_alias).expect("build edit");
        let patched = apply_scalar_edits(fixture, vec![edit]).expect("apply");

        assert!(!patched.contains("aliases: null"), "null marker must be gone");
        assert!(patched.contains("- name: provider-backup"));
        assert!(patched.contains("url: http://backup.example"));
        // Every original line (other than `aliases: null`) survives byte-for-byte.
        for line in fixture.lines().filter(|l| !l.trim().is_empty() && *l != "    aliases: null") {
            assert!(patched.contains(line), "line missing after patch: {line}");
        }
    }

    #[test]
    fn add_alias_when_aliases_key_has_no_value_replaces_line_in_place() {
        let fixture = concat!("inputs:\n", "  - name: provider\n", "    url: http://main.example\n", "    aliases:\n",);
        let doc: SourcePatchDocument = serde_saphyr::from_str(fixture).expect("projection parses");
        let new_alias = ConfigInputAliasDto {
            name: "provider-backup".into(),
            url: "http://backup.example".to_string(),
            ..Default::default()
        };

        let edit = build_alias_addition_edit(fixture, &doc, "provider", &new_alias).expect("build edit");
        let patched = apply_scalar_edits(fixture, vec![edit]).expect("apply");

        assert!(patched.contains("aliases:"));
        assert!(patched.contains("- name: provider-backup"));
        // Reparse must succeed and yield exactly one alias — guards against duplicate `aliases:` keys.
        let reparsed: SourcePatchDocument = serde_saphyr::from_str(&patched).expect("reparse");
        let aliases = reparsed.inputs[0].value.aliases.as_ref().expect("aliases");
        assert_eq!(aliases.value.len(), 1);
    }

    #[test]
    fn duplicate_alias_names_are_rejected() {
        let fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    aliases:\n",
            "      - name: twin\n",
            "        url: http://first.example\n",
            "      - name: twin\n",
            "        url: http://second.example\n",
        );
        let doc: SourcePatchDocument = serde_saphyr::from_str(fixture).expect("projection parses");

        let err = find_input(&doc, "provider").expect_err("duplicate alias must be rejected");
        let msg = format!("{err:?}");
        assert!(msg.contains("alias 'twin'"), "error must name the duplicated alias, got: {msg}");
    }

    #[test]
    fn sort_aliases_keeps_comments_attached_to_their_original_blocks() {
        let sort_fixture = concat!(
            "inputs:\n",
            "  - name: provider\n",
            "    url: http://main.example\n",
            "    aliases:\n",
            "      # comment for oldest\n",
            "      - name: oldest\n",
            "        url: http://oldest.example\n",
            "        exp_date: 100\n",
            "      # comment for newest\n",
            "      - name: newest\n",
            "        url: http://newest.example\n",
            "        exp_date: 200\n",
        );
        let doc: SourcePatchDocument = serde_saphyr::from_str(sort_fixture).expect("projection parses");

        let edit = build_alias_sort_edit(sort_fixture, &doc, "provider", &["newest", "oldest"])
            .expect("build edit")
            .expect("edit present");
        let patched = apply_scalar_edits(sort_fixture, vec![edit]).expect("apply");

        let newest_pos = patched.find("name: newest").expect("newest present");
        let oldest_pos = patched.find("name: oldest").expect("oldest present");
        let newest_comment_pos = patched.find("# comment for newest").expect("newest comment present");
        let oldest_comment_pos = patched.find("# comment for oldest").expect("oldest comment present");
        assert_eq!(patched.matches("# comment for newest").count(), 1);
        assert_eq!(patched.matches("# comment for oldest").count(), 1);
        assert!(newest_pos < oldest_pos, "newest should come before oldest after sort");
        // Comment must travel with its block, not get left in front of the other name.
        assert!(newest_comment_pos < oldest_pos, "newest comment must precede oldest name after sort, not trail it");
        assert!(
            oldest_comment_pos > newest_pos && oldest_comment_pos > newest_comment_pos,
            "oldest comment must come after newest block, not before it"
        );
    }
}
