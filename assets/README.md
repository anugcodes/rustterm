# Bundled assets

## `JetBrainsMono-Regular.ttf`

JetBrains Mono Regular, used as the terminal font. It is embedded in the
binary with `include_bytes!` (`src/main.rs`), so installed builds do not need
the file next to the executable.

- Source: <https://github.com/JetBrains/JetBrainsMono>
- License: SIL Open Font License 1.1 — see `OFL.txt`.
- Weight: Regular (the terminal is not bold).
