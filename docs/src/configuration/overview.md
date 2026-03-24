# ⚙️ Configuration & Setup (Overview)

Tuliprox intentionally avoids a gigantic, monolithic configuration file. Instead, it follows the principle of **Separation of
Concerns**. The setup consists of "5 Pillars" (files) that are logically separated.

## The Home-Directory Logic (Path Resolution)

Before diving into the files, it is essential to understand how Tuliprox resolves file paths. Upon startup, Tuliprox determines
a central **Home Directory**. All relative paths in your config files (e.g., `storage_dir: ./data` or `web_root: ./web`) are
resolved strictly relative to this Home Directory.

The resolution order for the Home Directory is:

1. **CLI Argument:** `--home` or `-H` (Highest Priority)
2. **Environment Variable:** `TULIPROX_HOME`
3. **Fallback:** The physical directory where the executed `tuliprox` binary is located.

### Default Directory Structure

By default, if you just run Tuliprox in an empty folder, it will create the following structure:

```text
tuliprox_home/
 ├─ config/         # Contains config.yml, source.yml, mapping.yml, user.txt
 ├─ data/           # Primary storage_dir for B+Tree databases (*.db)
 ├─ data/backup/    # Backups initiated by the Web UI
 ├─ data/user/      # User-specific configurations (like favorites)
 ├─ downloads/      # Downloaded VODs
 └─ web/            # Frontend assets for the Web UI
 └─ cache/          # Cached resources
```

*Example:* If your home is resolved to `/opt/tuliprox` and you define `backup_dir: ./backup` in `config.yml`, Tuliprox will
securely store backups exactly under `/opt/tuliprox/backup`.

---

## The 5 Pillars of Configuration

To utilize Tuliprox fully, you must understand these 5 files and place them in your `config` directory:

| File | Responsibility | Architecture Level |
| :--- | :--- | :--- |
| **`config.yml`** | The Core System. Defines *how* Tuliprox physically runs. Sets ports, reverse proxy buffers, paths, TMDB API keys, metadata worker limits, Web UI settings, and global logging. | **Infrastructure & Engine** |
| **`source.yml`** | The Data Flow. Defines *what* goes in (Provider URLs, Xtream credentials, Panel API limits) and *what* goes out (Targets, Filter assignments, Formats like STRM or M3U). | **Data Sources & Targets** |
| **`api-proxy.yml`** | The Gateway. Defines virtual server endpoints exposed to clients (VLC, TiviMate) and handles **Access Management** (Which user can access which target? Which proxy mode and user priority is used?). | **Network & Auth** |
| **`mapping.yml`** | The Transformation. Contains a powerful, embedded DSL (Domain Specific Language) to dynamically rename streams, reassign groups, or map IDs (including counters) based on regex filters. | **Data Enrichment** |
| **`template.yml`** | The DRY Principle (Don't Repeat Yourself). Contains globally reusable Regular Expressions (Regex) and logic macros that can be invoked in `source.yml` and `mapping.yml` via `!MACRO_NAME!`. | **Structuring** |
