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

### Subsections (Object Keys)

| Block | Description | Link |
| :--- | :--- | :--- |
| `templates` | *(Legacy)* Inline templates for filter macros. Prefer `template.yml`. | [See section](#1-templates-templates) |
| `mapping` | The core list of mapping rule objects and their respective DSL scripts. | [See section](#2-mapping-rules-mapping) |

---

## 1. Templates (`templates`)

*(Legacy feature - for new setups, prefer centralized templates in `template.yml` or `template.d` directory).*

If you have a lot of repeats in your regular expressions, you can define inline templates to make your scripts cleaner. You can reference templates inside other templates or scripts by wrapping their name in exclamation marks: `!name!`.

```yaml
mappings:
  templates:
    - name: delimiter
      value: '[\s_-]*'
    - name: quality
      value: '(?i)(?P<quality>HD|LQ|4K|UHD)?'
```
You can then use them in your regex: `^.*TF1!delimiter!Series(!delimiter!!quality!)\s*$`. Tuliprox resolves and replaces these placeholders at startup.

---

## 2. Mapping Rules (`mapping`)

This is the main block where you define your transformation logic. A mapping is referenced by its `id` in your `source.yml` under the respective Target (`mapping_ids:[your_mapping_id]`).

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
| **`id`** | String | **Mandatory.** The unique identifier of the mapping. |
| **`match_as_ascii`** | Bool | If `true`, Tuliprox normalizes (de-unicodes) values on-the-fly during regex evaluation, e.g., `Cinéma` is treated as `Cinema`. The actual assignment in the DSL, however, retains the original accents! Default is `false`. |
| **`mapper`** | List | A list of scripts (executed sequentially) containing the DSL logic. Can optionally be gated by a `filter`. [See Mapper DSL](#the-mapper-dsl-mapper). |
| **`counter`** | List | Logic for assigning channel numbers sequentially. [See Counters](#counters-sequential-numbering-counter). |

---

## Filter & Operator Basics

Before writing advanced scripts, you must understand the logical operators available for evaluating `filter` strings (Applies to `source.yml` & `mapping.yml`):

* **Logical Operators:** `AND`, `OR`, `NOT`
* **Regex Match:** `~` (Tilde executes a Regex match against the specified field)
* **Type Match:** `=` (E.g., `Type = live`, `Type = vod`, `Type = series`, `Type = movie`)

**Evaluatable Fields:** `Group`, `Title`, `Name`, `Caption`, `Url`, `Genre`, `Input`, `Type`

*Example:* `((Group ~ "^DE.*") AND (NOT Title ~ ".*Shopping.*")) OR (Group ~ "^AU.*")`

---

## 2.1 The Mapper DSL (`mapper`)

The embedded language supports logical constructs, regex evaluations, and variable assignments. It is whitespace-tolerant and uses familiar programming concepts. The fields of the currently processed playlist item are always accessed using the `@` prefix.

**Readable & Writable `@Fields`:**
`@name`, `@title`, `@caption`, `@group`, `@id`, `@chno`, `@logo`, `@logo_small`, `@parent_code`, `@audio_track`, `@time_shift`, `@rec`, `@url`, `@epg_channel_id`, `@epg_id`, `@genre`.

*(Special note on `@Caption`: Acts as an alias for Title/Name. If you write to `@Caption`, Tuliprox updates both the `Title` AND the `Name` to the same value).*

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

Regular expressions are executed using the tilde `~` operator. The results are placed in a capture object. You can access them via index (`.1`, `.2`) or via "Named Captures".

```dsl
# Extract the year from the title using named captures
info = @Title ~ "(?P<Movie>.*?)\s-\s(?P<Year>\d{4})"

# Store the matches in variables
movie_title = info.Movie
movie_year = info.Year

@Title = movie_title
```

### 3. Match Blocks (Switch-Case & If-Then-Else)

A `match` block allows conditional assignments. **Crucial:** The order of the cases is important! The first block whose condition evaluates to true is executed, and the `match` block exits. `_ => default` matches anything.

**Standard Switch-Case:**
```dsl
# Check if a regex found a specific station (e.g., FOX)
station = @Caption ~ "FOX"

result = match {
    (var1, var2) => "Both variables exist",
    station => "Only the station exists",
    _ => "Fallback (Default)", # The underscore matches anything
}
```

**Simulating If-Then-Else:**
```dsl
# Maybe there is no station found
station = @Caption ~ "ABC"

match {
   station => {
      # IF block: Executes only if 'station' is set/matched
      @Group = "ABC Networks"
   }
   _ => {
       # ELSE block: 'station' does not exist
       @Group = "Other Networks"
   }
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
   2026.. => "Future Releases",
   _ => year_text,
}
```

### 5. For-Each Loops (Iterating Lists)

`for_each` iterates over Named-Results (like Regex captures or output from `split()`). Perfect for distributing movies into multiple virtual folders based on their genres!

`Named` variables are created by:
1. **`split()` function**: keys are indices ("0", "1", ...), values are the split parts.
2. **Regex with capture groups**: keys are group names (or indices), values are the captured matches.

You can use `_` for parameters you want to ignore (e.g., `(_, value)` or `(key, _)`). However, at least one parameter must be named (you cannot use `(_, _)`).

```dsl
# 1. Using split()
# Split the genre string into a Named result (index as key, genre as value)
genres = split(@Genre, "[,/&]")

genres.for_each((ignored_index, single_genre) => {
  # For each genre in the string "Action, Drama", an alias stream is created!
  add_favourite(concat("Genre - ", trim(single_genre)))
})

# 2. Using Regex with named capture groups
info = @Title ~ "(?P<Movie>.*?)\s-\s(?P<Year>\d{4})"

info.for_each((k, v) => {
    # k will be "Movie" then "Year"
    # v will be "Inception" then "2010"
    print(concat("Found ", k, ": ", v))
})
```

## 2.2 Counters (Sequential Numbering) (`counter`)

Many IPTV players sort by channel number (`tvg-chno`). Counters allow sequential numbering of channels *after* they have passed through the DSL logic.

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

| Parameter | Description |
| :--- | :--- |
| `filter` | A string filter statement determining which streams this counter applies to. |
| `value` | The starting integer value (e.g., start counting from `100`). |
| `field` | The target field to write to (Allowed: `title`, `name`, `caption`, `chno`). |
| `modifier` | `assign`: Hard overwrites the field with the number.<br>`prefix`: Prepends the number to the field.<br>`suffix`: Appends the number to the field. |
| `concat` | *(Optional)* The separator string between the number and the original field for Prefix/Suffix (e.g., `". "`). |
| `padding` | *(Optional)* Zero-padding length (e.g., `padding: 3` turns `1` into `001`). |

---

## Advanced Examples

### Grouping and Cleaning Categories

We assume we have some groups containing keywords like EU, SATELLITE, NATIONAL, NEWS, MUSIC, SPORT, RELIGION, FILM, KIDS, DOCU in the group name.
We want to group the channels inside NEWS, NATIONAL, SATELLITE by their resolution/quality.
The other groups should get a numerical prefix for ordering.

```yaml
- filter: 'Group ~ ".*"'
  script: |
    group = @Group ~ "(EU|SATELLITE|NATIONAL|NEWS|MUSIC|SPORT|RELIGION|FILM|KIDS|DOCU)"
    quality = @Caption ~ "\b([F]?HD[i]?)\b"
    
    title_match = @Caption ~ "(.*?)\:\s*(.*)"
    title_prefix = title_match.1
    title_name = title_match.2

    # Add a suffix '*' to the channel name if it came from the SATELLITE group
    title_name = map title_prefix {
      "SATELLITE" =>  concat(title_name, "*"),
      _ => title_name,
    }

    # Only extract quality for specific groups
    quality = map group {
        "NEWS" | "NATIONAL" | "SATELLITE" => quality,
        _ => null,
    }

    # Assign a sequential prefix based on quality, fallback to group type
    prefix = map quality {
    "HD" => "01.",
    "FHD" => "02.",
    "HDi" => "03.",
    _ => map group {
        "NEWS" => "04.",
        "DOCU" => "05.",
        "SPORT" => "06.",
        "NATIONAL" => "07.",
        "RELIGION" => "08.",
        "KIDS" => "09.",
        "FILM" => "10.",
        "MUSIC" => "11.",
        "EU" => "12.",
        "SATELLITE" => "13.",
        _ => group
      },
    }

    # Build the final Group name
    name = match {
      quality => concat(prefix, " FR [", quality, "]"),
      group => concat(prefix, " FR [", group, "]"),
      _ => prefix
    }

    # Assign back to the stream object
    @Group = name
    @Caption = title_name
```

#### Explanation

1. **Initial Group Detection**  
   The script first checks whether `@Group` contains one of the known category keywords such as `NEWS`, `SPORT`, or `FILM`.  
   The matched keyword is stored in the variable `group` and becomes the logical classification base for the rest of the script.

2. **Quality Extraction from the Caption**  
   The expression `@Caption ~ "\b([F]?HD[i]?)\b"` tries to extract quality markers such as `HD`, `FHD`, or `HDi` from the caption.  
   This allows Tuliprox to build more meaningful target groups for selected categories.

3. **Splitting Prefix and Display Title**  
   The caption is split into two logical parts:
   - `title_prefix`: the segment before `:`
   - `title_name`: the segment after `:`
   
   This is useful when the provider encodes structural metadata directly into the title string, such as:
   - `SATELLITE: Eurosport HD`
   - `NEWS: France 24`

4. **Special Handling for SATELLITE Entries**  
   If the extracted prefix is `SATELLITE`, the visible title gets a trailing `*`.  
   This is a lightweight visual marker that lets users immediately recognize channels that originated from the SATELLITE branch.

5. **Quality is Only Kept for Selected Groups**  
   Not every category should be grouped by resolution.  
   Therefore, the script keeps the extracted `quality` only for:
   - `NEWS`
   - `NATIONAL`
   - `SATELLITE`
   
   For all other categories, `quality` is set to `null`, which forces fallback logic later.

6. **Prefix Assignment for Stable Ordering**  
   The variable `prefix` is generated in two stages:
   - first by detected `quality`
   - if no relevant quality exists, by detected `group`
   
   This gives deterministic ordering such as:
   - `01.` for `HD`
   - `02.` for `FHD`
   - `06.` for `SPORT`
   - `10.` for `FILM`

   The result is a client-friendly ordering even in players that sort alphabetically rather than by provider order.

7. **Building the Final Group Label**  
   The `match` block decides how the final target group is constructed:
   - if `quality` exists, the new group becomes something like `01. FR [HD]`
   - otherwise, if `group` exists, it becomes something like `06. FR [SPORT]`
   - if neither exists, only the prefix is used

8. **Final Assignment Back to the Playlist Item**  
   At the end:
   - `@Group` receives the normalized, ordered category label
   - `@Caption` receives the cleaned channel title without the original structural prefix

### Grouping by release year

We want to automatically group movie channels by their release year, using the following logic:

- All movies released before `2020` should be grouped together under one label.
- Movies from `2020` onward should each be grouped by their specific year.

Example title: `"Master Movie (2020)"`

The result should look like:

- `FR | Movies < 2020`
- `FR | Movies 2020`
- `FR | Movies 2021`


```yaml
- filter: 'Group ~ "^FR" AND Caption ~ "\(?\d{4}\)?$"'
  script: |
    year_text = @Caption ~ "(\d{4})\)?$"
    year = number(year_text)
    
    year_group = map year {
     ..2019 => "< 2020",
     _ =>  year_text,
    }
    
    @Group = concat("FR | MOVIES ", year_group)
```


#### Explanation

1. **Filter Scope**  
   The filter restricts the mapper step to entries where:
   - the group starts with `FR`
   - the caption ends with a 4-digit year, optionally wrapped in parentheses

   This prevents unrelated channels from being processed by the release-year grouping logic.

2. **Year Extraction**  
   The regular expression `(\d{4})\)?$` extracts the final year value from the caption and stores it as `year_text`.

3. **Numeric Conversion**  
   `number(year_text)` converts the extracted text into a number so it can be compared using numeric range logic in a `map` block.

4. **Range-Based Grouping**  
   The `map year` block creates two output classes:
   - any year up to `2019` becomes `< 2020`
   - any later year keeps its exact value

5. **Final Group Assignment**  
   The result is appended to the fixed prefix `FR | MOVIES ` and written back into `@Group`.

### URL Rewriting

If you want to proxy streams but your provider uses dynamic token query parameters that must be preserved, or if you want to alter the domain structure inside the playlist while keeping the original path/query part unchanged, URL rewriting is a useful technique.

**Why This is Useful**  
This pattern is especially valuable when:
   - you want to route streams through your own reverse proxy
   - provider URLs contain dynamic query tokens that must not be lost
   - you want to hide the original upstream host from clients
   - you want to normalize mixed provider domains into one controlled entry point

```yaml
mapping:
  - id: France_URL_Rewrite
    match_as_ascii: true
    mapper:
      - filter: 'Name ~ "^TF.*"'
        script: |
          # Extract everything after the domain
          query_match = @Url ~ "https?:\/\/(.*?)\/(?P<query>.*)$"
          
          # Rebuild the URL pointing to a different domain but keeping the query
          @Url = concat("http://my.iptv.proxy.com/", query_match.query)
```

#### Explanation

1. **Targeted Filtering**  
   The mapper rule is only applied to entries whose `Name` starts with `TF`.  
   This ensures that only the intended provider subset is rewritten, rather than rewriting every URL globally.

2. **Splitting the URL into Host and Remainder**  
   The regex:

   ```dsl
   https?:\/\/(.*?)\/(?P<query>.*)$
   ```

   captures:
   - the original host/domain
   - everything after the first `/` into the named capture `query`

   In practical terms, this means the script preserves the provider-specific path and tokenized suffix while discarding the original domain.

3. **Rebuilding the URL with a New Base Domain**  
   The script then constructs a new URL using:

   ```dsl
   concat("http://my.iptv.proxy.com/", query_match.query)
   ```

   This keeps the original request path intact while forcing the stream to be served through a different frontend domain.

4. **Final Assignment**  
   The rewritten URL is written back into `@Url`, so all downstream outputs use the proxied version rather than the original provider domain.

After this mapper runs, clients will still access the same stream path and tokenized request parameters, but through your custom proxy domain.  
This helps centralize traffic, simplify DNS exposure, and keep client-facing playlists independent from upstream host changes.

---

## 📂 Mapping File Resolution

By default, the mapping file is `mapping.yml` in the config directory of Tuliprox. This can be changed by setting `mapping_path` in `config.yml`. 

For complex IPTV setups, it is highly recommended to set the `mapping_path` to a directory rather than a single file.
If `mapping_path` points to a directory, Tuliprox loads all `*.yml` files in **alphanumeric** order and merges them.

```yaml
mapping_path: ./config/mappings.d
```

### File Loading & Processing Order

When providing a directory path for `mapping_path` or `template_path`, Tuliprox automatically loads all `.yml` files within that folder. However, it is critical to understand the **Lexicographical (String-based) Sort** used by the system.

Without leading zeros, a file starting with `10` will be prioritized over a file starting with `2`, because `1` comes before `2` in the ASCII character set.

**Incorrect Naming (Unexpected Order):**
1.  `mappings_1.yml`
2.  `mappings_10.yml`  <-- *Loaded second*
3.  `mappings_2.yml`   <-- *Loaded third*

**Correct Naming (Sequential Order):**
To ensure your mappings and templates are merged in the intended sequence, always use **padded numbering**:
1.  `01_base_mappings.yml`
2.  `02_additional_mappings.yml`
3.  `10_custom_overrides.yml`

> **Why this matters:** Tuliprox processes these files sequentially. If a later file contains a mapping ID that was already defined in an earlier file, the later definition may overwrite or merge with the previous one, depending on the specific logic. Proper numbering ensures your global "Base" rules are established before your specific "Overrides" are applied.

#### CLI Overrides

If you run Tuliprox from the command line, you can override mapping and template loading paths via CLI flags:

```bash
tuliprox -m /custom/path/mapping.yml -T /custom/path/template.yml
```
Arguments:
- `-m` / `--mapping` overrides the mapping file or mapping directory path.
- `-T` / `--template` overrides the template file or template directory path.