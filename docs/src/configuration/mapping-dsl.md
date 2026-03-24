# 🗺️ Pillar 4: `mapping.yml` (Mapper DSL & Logic)

While the filter syntax in `source.yml` only determines *whether* a channel is let through, the Mapping Engine allows you to
perform deep, structural manipulations on the stream object *before* it is written to the final playlist.

Tuliprox utilizes a blazingly fast, embedded **DSL (Domain Specific Language)** specifically built for this purpose.

## Context: Why use the DSL?

Without the DSL, your provider dictates how your channels are named and categorized. The DSL gives you the power to completely
restructure chaotic upstream provider lists into a perfectly clean, personalized format. For example, you can extract the year
out of a messy title, rename the category based on that year, fix resolutions, or filter out specific terms globally.

## Top-level entries

```yaml
mappings:
  templates:
  mapping:
```

* **`templates`**: *(Legacy)* Inline templates for filter macros. Prefer [template.yml](configuration/template.md).
* **`mapping`**: A list of mapping rule objects.

## Mapping Rule Structure (`mapping`)

```yaml
mappings:
  mapping:
    - id: map_sports_clean
      match_as_ascii: true
      mapper:
        - filter: 'Group ~ ".*Sports.*"'
          script: |
            @Group = "Live Sports HD"
      counter:
        - filter: 'Group ~ "Live Sports HD"'
          value: 100
          field: chno
          modifier: assign
```

| Parameter | Type | Description |
| :--- | :--- | :--- |
| **`id`** | String | The identifier of the mapping. It is referenced in `source.yml` under the respective Target (`mapping_ids:[map_sports_clean]`). |
| **`match_as_ascii`** | Bool | **Background:** If `true`, Tuliprox normalizes (de-unicodes) values on-the-fly during regex evaluation. `Cinéma` is treated as `Cinema`. The actual assignment in the DSL, however, retains the original accents! |
| **`mapper`** | List | A list of scripts (executed sequentially) containing the DSL logic. Can optionally be gated by a `filter`. |
| **`counter`** | List | Logic for assigning channel numbers sequentially (see below). |

---

## Filter & Operator Basics

Before writing advanced scripts, you must understand the logical operators available for evaluating `filter` strings (Applies to
`source.yml` & `mapping.yml`):

* **Logical Operators:** `AND`, `OR`, `NOT`
* **Regex Match:** `~` (Tilde executes a Regex match against the specified field)
* **Type Match:** `=` (E.g., `Type = live`, `Type = vod`, `Type = series`, `Type = movie`)

**Evaluatable Fields:** `Group`, `Title`, `Name`, `Caption`, `Url`, `Genre`, `Input`, `Type`

*Example:* `((Group ~ "^DE.*") AND (NOT Title ~ ".*Shopping.*")) OR (Group ~ "^AU.*")`

---

## The Mapper DSL (`mapper`)

The language supports logical constructs, regex evaluations, and assignments. The fields of the currently processed playlist item
are always accessed using the `@` prefix.

**Readable & Writable `@Fields`:**
`@name`, `@title`, `@caption`, `@group`, `@id`, `@chno`, `@logo`, `@logo_small`, `@parent_code`, `@audio_track`, `@time_shift`,
`@rec`, `@url`, `@epg_channel_id`, `@epg_id`, `@genre`.

*(Special note on `@Caption`: Acts as an alias for Title/Name. If you write to `@Caption`, Tuliprox updates both the `Title` AND
the `Name` to the same value).*

### 1. Built-in Functions

| Function | Explanation | Example |
| :--- | :--- | :--- |
| `concat(a, b, ...)` | Concatenates multiple strings. | `concat("US \| ", @Title)` |
| `uppercase(a)` | Converts text to UPPERCASE. | `uppercase(@Group)` |
| `lowercase(a)` | Converts text to lowercase. | `lowercase(@Genre)` |
| `capitalize(a)` | Title Case (Capitalizes the first letter of words). | `capitalize(@Title)` |
| `split(a, delim)` | Splits a string and returns a Named list (iterable). | `split(@Genre, ",")` |
| `trim(a)` | Removes whitespace from the edges. | `trim(@Title)` |
| `replace(a, b, c)` | Simple text Search (b) & Replace (c). | `replace(@Title, "FHD", "")` |
| `pad(val, len, char, align)` | Pads strings/numbers. `>` (Pad left), `<` (Pad right), `^` (Center). | `pad(1, 3, "0", ">")` |
| `format(fmt, ...)` | Rust-style string formatting substituting `{}`. | `format("S{}E{}", season, ep)` |
| `template(name)` | Retrieves a macro value from `template.yml`. | `template("MY_MACRO")` |
| `number(val)` | Casts a string to a float/integer. | `number("2024")` |
| `first(list)` | Returns the first element of a Named list/Regex match. | `first(@Caption ~ "(\d+)")` |
| `print(a, b, ...)` | Logs the values to the console (Requires `trace` log level). | `print("Matched:", @Title)` |
| `add_favourite(grp)` | **Background:** Clone function! Takes the currently processed stream, changes its group to `grp`, generates a clean alias UUID, and adds the stream additionally (as a "Favorite") to the playlist. | `add_favourite("Top 10")` |

### 2. RegEx Captures & Extraction

Regular expressions are executed using the tilde `~` operator. The results are placed in a capture object. You can access them
via index (`.1`, `.2`) or via "Named Captures".

```dsl
# Extract the year from the title using named captures
info = @Title ~ "(?P<Movie>.*?)\s-\s(?P<Year>\d{4})"

# Store the matches in variables
movie_title = info.Movie
movie_year = info.Year

@Title = movie_title
```

### 3. Match Blocks (Switch-Case Logic)

A `match` block allows conditional assignments. **Crucial:** Order matters. The first block whose condition evaluates to true is
executed, and the `match` block exits.

```dsl
# Check if a regex found a specific station (e.g., FOX)
station = @Caption ~ "FOX"

result = match {
    (var1, var2) => "Both variables exist",
    station => "Only the station exists",
    _ => "Fallback (Default)", # The underscore matches anything
}
```

### 4. Map Blocks (Dictionaries & Ranges)

Map blocks are ideal for translating hundreds of cryptic provider categories or resolutions into your own clean design.

**Mapping Texts (with Multi-Keys `|`):**

```dsl
quality = uppercase(@Caption ~ "\b(HD|FHD|4K|UHD)\b")

quality = map quality {
  "SHD" | "SD" => "SD",
  "1080p" | "FHD" => "FHD",
  "4K" | "3840p" => "UHD",
  _ => quality, # If nothing matches, keep the original
}
```

**Mapping Numbers (Ranges `..`):**

```dsl
year_text = @Caption ~ "(\d{4})\)?$"
year = number(year_text) # Cast String to Number

year_group = map year {
   ..2019 => "Classics (< 2020)",
   2020..2025 => "New Releases",
   _ => year_text,
}
```

### 5. For-Each Loops (Iterating Lists)

`for_each` iterates over Named-Results (like Regex captures or output from `split()`). Perfect for distributing movies into
multiple virtual folders based on their genres!

```dsl
genres = split(@Genre, "[,/&]")

genres.for_each((ignored_index, single_genre) => {
  # For each genre in the string "Action, Drama", an alias stream is created!
  add_favourite(concat("Genre - ", trim(single_genre)))
})
```

---

## Counters (Sequential Numbering) (`counter`)

Many IPTV players sort by channel number (`tvg-chno`). Counters allow sequential numbering of channels *after* they have passed
through the DSL.

```yaml
mapping:
  - id: add_channel_numbers
    counter:
      - filter: 'Group ~ "DE Channels"'
        value: 100
        field: chno
        modifier: assign
      - filter: 'Group ~ "DE Channels"'
        value: 1
        padding: 3
        field: title
        modifier: prefix
        concat: ". "
```

**Field Details:**

* `filter`: A string filter determining which streams this counter applies to.
* `value`: The starting value (e.g., start counting from `100`).
* `field`: The target field to write to (`chno`, `title`, `name`, `caption`).
* `padding`: Zero-padding (e.g., `padding: 3` turns `1` into `001`).
* `modifier`:
  * `assign`: Hard overwrites the `field` with the number.
  * `prefix`: Prepends the number to the field (e.g., `001. ARD HD`).
  * `suffix`: Appends the number to the field.
* `concat`: The separator between the number and the original field for Prefix/Suffix (e.g., `". "`).
