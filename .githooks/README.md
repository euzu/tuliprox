# Pre-push privacy scan

## 1. Install Gitleaks

Use Git 2.36+ and Gitleaks 8.25+ (8.x). Download the archive matching your OS and
processor from the [official releases](https://github.com/gitleaks/gitleaks/releases).

| OS | Installation |
| --- | --- |
| Windows | Install [Git for Windows](https://gitforwindows.org/), which includes Git Bash. Extract `gitleaks.exe` into a directory on your `PATH`. |
| Linux | Extract `gitleaks` into a directory on your `PATH`, such as `$HOME/.local/bin`, and run `chmod +x` on the binary. |
| macOS | Run `brew install gitleaks`, or install the release binary into a directory on your `PATH`. |

Verify the installation in your terminal (Windows: Git Bash):

```bash
gitleaks version
```

## 2. Add rules

`.gitleaks.default.toml` contains the shared rules and enables Gitleaks' built-in
secret detection. Save `.gitleaks.personal.toml` in the **main repository root**
(the first path shown by `git worktree list`). All linked worktrees load it from
there automatically. Create the file with rules such as:

```toml
[[rules]]
id = "private-marker"
description = "Private text must not be published"
regex = '''PRIVATE_MARKER_DO_NOT_PUBLISH'''
```

Replace the marker with your private pattern, using Go-compatible regex syntax.
Private configs stay Git-ignored and must not be committed or pushed.
Additional root-level `.gitleaks.*.toml` files can provide worktree-specific rules;
only `.gitleaks.default.toml` is versioned. Every worktree
needs the updated hook and default config in its checked-out branch.
The hook scans the default and every local config independently; all must pass.
Custom allowlists affect only their own scan. No `[extend]` is needed locally.

## 3. Enable the Git hook

Run these commands from the repository root (Windows: Git Bash). First check
whether you already use a custom hooks directory:

```bash
git config --show-origin --get core.hooksPath
```

If hooks already exist, integrate them before changing the hooks directory.
Then enable the hook for this clone:

```bash
chmod +x .githooks/pre-push
git config --local core.hooksPath .githooks
```

Repeat this setup in each clone. The hook scans every pushed branch from the main
repository or any linked worktree, even when another branch is checked out. The
current checkout must contain an executable `.githooks/pre-push` and the shared config.

Every push fetches the current `develop` from `https://github.com/euzu/tuliprox.git`
and scans all branch commits absent from that upstream history. This applies to new
and existing branches. A failed fetch or an unrelated history blocks the push.
The baseline is stored in the hidden ref `refs/privacy-scan/develop`; `FETCH_HEAD` is not changed.

Every commit in that range is scanned, including findings deleted later. Local
output shows complete findings with file, line, commit, and matched text. All
configurations and pushed refs are scanned before findings or errors block the push.
