# 🧩 Pillar 5: `template.yml` (Macros & DRY)

In a large IPTV setup, you will quickly realize that you are repeating the same regular expressions (Regex) or complex filters  
(like blocking Adult content) across dozens of targets and mappings.

This leads to unreadable and highly unmaintainable configurations. Tuliprox solves this elegantly using **Templates**  
(applying the DRY principle: Don't Repeat Yourself).

You define complex strings or regex patterns exactly once. Afterward, you can invoke them in all other configuration files  
(like `source.yml` or `mapping.yml`) by wrapping the template name in exclamation marks: `!MACRO_NAME!`.

## Global Path (Recommended Setup)

It is highly recommended to set the `template_path` in `config.yml` to a directory rather than a single file:

```yaml
template_path: ./config/templates.d
```

Upon startup, Tuliprox reads all `.yml` files in this directory in alphanumeric order (e.g., `01-regex.yml`, `02-filters.yml`)  
and merges them into one massive global macro catalog.

*Important: The names of the templates (`name`) must be globally unique across all files!*

---

## Top-level entries

```yaml
templates:
  - name: DELIMITER
    value: '[\s_-]*'
```

## Structure & Variable Resolution

```yaml
templates:
  # A simple regex snippet for delimiters (spaces, underscores)
  - name: DELIMITER
    value: '[\s_-]*'
    
  # A capture-group regex for common TV qualities
  - name: QUALITY
    value: '(?i)(?P<quality>HD|LQ|4K|UHD)?'
    
  # A nested logical filter condition
  - name: FILTER_NO_TRASH
    value: 'NOT (Group ~ "(?i).*Shopping.*" OR Group ~ "(?i).*Commercials.*")'
    
  # The Magic: Macros can call other Macros!
  - name: FILTER_DE_CLEAN
    value: 'Group ~ "^DE.*" AND !FILTER_NO_TRASH!'
    
  # Lists for Sequence-Sorting
  - name: CHAN_SEQ
    value:
      - '(?i)\bUHD\b'
      - '(?i)\bFHD\b'
```

Tuliprox recursively resolves the entire template tree during system startup.
*(Security Feature: The system detects cyclic dependencies—Macro A calls Macro B, which calls Macro A—and aborts the startup  
with a log error to prevent infinite loops).*

---

## Practical Application

### 1. In `source.yml` (As a Target Filter)

Instead of writing a monstrous 500-character line into your target, you build it out of logical template blocks.

```yaml
targets:
  - name: clean_german_tv
    filter: "!FILTER_DE_CLEAN! AND Type = live"
```

### 2. In `source.yml` (As a Sequence Sort)

For the "Sort Sequence" feature (sorting by the occurrence of tags in the name), templates defined as lists (`value:` as an array)  
can be injected directly into the sequence array.

```yaml
sort:
  rules:
    - target: channel
      field: caption
      order: asc
      sequence:
        - "!CHAN_SEQ!"
        - '(?i)\bHD\b'
```

### 3. In `mapping.yml` (As a Regex Component)

In the Mapper DSL, Tuliprox injects the resolved regex pattern exactly where the exclamation mark macro is placed. This prevents complex regex typos.

```dsl
# Extracts "UHD" from "Sky Sport UHD" and writes it to the variable 'quality'
quality = uppercase(@Caption ~ "!QUALITY!")

# Replaces all arbitrary spaces and underscores with a clean separator
@Title = replace(@Title, "!DELIMITER!", " - ")
```
