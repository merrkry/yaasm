# Use XDG layout on all platforms

YAASM uses the XDG configuration and data layout on Linux, macOS, and Windows, falling back to `.config` and `.local/share` under the user's home directory. Callers may instead provide both `--config-dir` and `--data-dir`, but never only one. This deliberately favors one predictable cross-platform layout over each operating system's native application directories.
