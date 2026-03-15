# Mapping And Templates

Tuliprox supports two layers of editorial logic:

- reusable templates
- mapping rules written in a small DSL

## Template loading

Templates can live:

- inline in `source.yml`
- inline in `mapping.yml`
- centrally in files or directories configured via `template_path`

If a directory is used, all `*.yml` files are loaded in alphanumeric order.

Example:

```yaml
templates:
  - name: delimiter
    value: '[\s_-]*'
  - name: quality
    value: '(?i)(?P<quality>HD|LQ|4K|UHD)?'
```

Referenced as:

```text
^.*TF1!delimiter!Series?!delimiter!Films?(!delimiter!!quality!)\s*$
```

## `mapping.yml`

Top-level structure:

- `templates`
- `mapping`

Each mapping entry defines:

- `id`
- `match_as_ascii`
- `mapper`
- `counter`

## `match_as_ascii`

If enabled, Tuliprox deunicodes values during matching.
That is useful when filters should match `é`, `ö` or `ß` with simpler ASCII patterns.

## Mapper DSL basics

The DSL supports:

- variable assignment
- string manipulation
- regex matching
- match blocks
- map blocks
- access to playlist fields with `@Field`
- a few builtin functions

Common builtins:

- `concat`
- `uppercase`
- `lowercase`
- `capitalize`
- `trim`
- `print`
- `number`
- `first`
- `template`
- `replace`
- `pad`
- `format`
- `add_favourite`

## Simple example

```yaml
mappings:
  mapping:
    - id: favourites_news
      match_as_ascii: true
      mapper:
        - filter: 'Group ~ "(?i)news"'
          script: |
            add_favourite("Favourites")
```

## Match and map blocks

Example `match` block:

```dsl
result = match {
  (var1, var2) => result1,
  var2 => result2,
  _ => default
}
```

Example `map` block:

```dsl
quality = map quality {
  "720p" => "HD",
  "1080p" => "FHD",
  "4K" => "UHD",
  _ => quality,
}
```

## Regex captures

Regex matches can expose numbered or named captures:

```dsl
title_match = @Caption ~ "(.*?)\\:\\s*(.*)"
title_prefix = title_match.1
title_name = title_match.2
```

## `for_each`

`for_each` iterates over named results such as split values or regex captures:

```dsl
genres = split(@Genre, "[,/&]")
genres.for_each((_, genre) => {
  add_favourite(concat("Genre - ", genre))
})
```

## Counters

Mappings can also define counters:

```yaml
mapping:
  - id: simple
    counter:
      - filter: 'Group ~ ".*FR.*"'
        value: 9000
        field: title
        padding: 2
        modifier: suffix
        concat: " - "
```

Counter fields:

- `filter`
- `value`
- `field`
- `modifier`
- `concat`
- `padding`

## Grouping example

This groups channels into quality-oriented or category-oriented buckets:

```dsl
group = @Group ~ "(EU|SATELLITE|NATIONAL|NEWS|MUSIC|SPORT|RELIGION|FILM|KIDS|DOCU)"
quality = @Caption ~ "\\b([F]?HD[i]?)\\b"
title_match = @Caption ~ "(.*?)\\:\\s*(.*)"
title_name = title_match.2

quality = map group {
  "NEWS" | "NATIONAL" | "SATELLITE" => quality,
  _ => null,
}

prefix = map quality {
  "HD" => "01.",
  "FHD" => "02.",
  "HDi" => "03.",
  _ => map group {
    "NEWS" => "04.",
    "DOCU" => "05.",
    "SPORT" => "06.",
    _ => group
  },
}

name = match {
  quality => concat(prefix, " FR [", quality, "]"),
  group => concat(prefix, " FR [", group, "]"),
  _ => prefix
}

@Group = name
@Caption = title_name
```

## Grouping by release year

```dsl
year_text = @Caption ~ "(\\d{4})\\)?$"
year = number(year_text)
year_group = map year {
  ..2019 => "< 2020",
  _ => year_text,
}
@Group = concat("FR | MOVIES ", year_group)
```

## Example `mapping.yml`

```yaml
mappings:
  templates:
    - name: QUALITY
      value: '(?i)\b([FUSL]?HD|SD|4K|1080p|720p|3840p)\b'
  mapping:
    - id: all_channels
      match_as_ascii: true
      mapper:
        - filter: 'Caption ~ "(?i)^(US|USA|United States).*?TNT"'
          script: |
            quality = uppercase(@Caption ~ "!QUALITY!")
            quality = map quality {
              "720p" => "HD",
              "1080p" => "FHD",
              "4K" => "UHD",
              "3840p" => "UHD",
              _ => quality,
            }
            @Group = "United States - Entertainment"
```

## Filter hints

Filters support:

- `NOT`
- `AND`
- `OR`
- regex matches with `~`
- `Type = live|vod|series`

Fields:

- `Group`
- `Title`
- `Name`
- `Caption`
- `Url`
- `Genre`
- `Input`
- `Type`

For Rust regex testing, `regex101.com` works well if the Rust flavor is selected.
