# ccfind

A fuzzy finder for [Claude Code](https://claude.com/claude-code) named sessions.

Scans `~/.claude/projects/` for sessions with custom titles and lets you search, select, and resume them interactively.

## Installation

```bash
cargo install --git https://github.com/hiboma/ccfind
```

Or build from source:

```bash
git clone https://github.com/hiboma/ccfind
cd ccfind
cargo install --path .
```

## Usage

### Interactive mode (default)

```bash
# Select a session and print the resume command to stdout
ccfind

# Select a session and exec into it directly
ccfind -e
ccfind --exec
```

### List mode

```bash
# Print all named sessions as TSV (session_id, title, project_path)
ccfind --list
```

### With eval

```bash
eval $(ccfind)
```

## Key bindings

| Key | Action |
|---|---|
| Type | Filter sessions (substring match) |
| Up / Ctrl-p | Move selection up |
| Down / Ctrl-n | Move selection down |
| Enter | Confirm selection |
| Esc / Ctrl-c | Cancel |

## How it works

1. Scans all `*.jsonl` files under `~/.claude/projects/` in parallel
2. Extracts `custom-title` entries using byte-level search (`memmem`) to skip non-matching lines
3. Resolves encoded project directory names back to filesystem paths (greedy left-to-right matching)
4. Presents an interactive substring-match finder powered by [nucleo](https://github.com/helix-editor/nucleo)

## License

MIT
