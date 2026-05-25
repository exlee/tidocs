# clidoc

Browse Rust documentation in your terminal. Search across all indexed crates
with fuzzy matching, instant results, and full rendered doc pages.

## Features

- **Auto-discovery** of rustup toolchain docs (std, core, alloc, etc.)
- **Manual source paths** for any `cargo doc` output directory
- **SQLite caching** for fast startup after first index
- **Background search thread** for responsive typing
- **Kind filtering**: prefix queries with `fn`, `st`, `tr`, `en`, `md`, `ma`, `ty`, `co`, `pr`, `kw`
- **In-doc search** (`/`) with highlight and navigation (`n`/`N`)
- **Non-interactive mode** (`--query`) for scripting and CI

## Installation

```bash
cargo install clidoc
```

## Usage

### Interactive mode

```bash
# Browse rustup standard library docs (default)
clidoc

# Add custom crate docs
clidoc ./my-project/target/doc

# Multiple sources
clidoc ./project-a/target/doc ./project-b/target/doc
```

### Non-interactive mode

```bash
# List matches
clidoc --query "Vec::push"

# Show full doc page for first match
clidoc --query "Vec::push" --details
```

### Kind-filtered search

Prefix the query with a kind badge to filter results:

| Badge | Kinds            |
| ----- | ---------------- |
| `fn`  | fn, method       |
| `st`  | struct           |
| `tr`  | trait            |
| `en`  | enum             |
| `md`  | mod              |
| `ma`  | macro            |
| `ty`  | type, assoc_type |
| `co`  | const, constant  |
| `pr`  | primitive        |
| `kw`  | keyword          |

Examples:

```
fn peek      # only functions/methods named "peek"
st HashMap   # only structs named "HashMap"
tr Iterator  # only traits named "Iterator"
```

### Keyboard shortcuts

#### Search mode

| Key         | Action               |
| ----------- | -------------------- |
| Type        | Search               |
| Enter       | Open detail view     |
| Up/Down     | Navigate results     |
| C-n/C-p     | Next/Prev result     |
| C-f/C-b     | Page forward/back    |
| PgUp/PgDn   | Page up/down         |
| Home/End    | First/Last result    |
| C-u         | Clear query          |
| C-w         | Delete last word     |
| Esc         | Clear or quit        |
| Backspace   | Delete char          |
| Delete      | Delete to word end   |
| C-g/C-c     | Quit                 |

#### Detail mode

| Key                | Action                   |
| ------------------ | ------------------------ |
| Esc/q              | Back to search           |
| j/k/Up/Down        | Scroll line              |
| Space/Backspace    | Page scroll              |
| PgUp/PgDn          | Page up/down             |
| C-f/C-b/C-u        | Page forward/back/up     |
| Home/End           | Top/bottom               |
| `/`                | Search within doc        |
| n/N                | Next/prev search match   |

## Adding Documentation Sources

Generate docs for your crate and point clidoc at the output:

```bash
cd my-crate
cargo doc --no-deps
clidoc target/doc
```

Or use the multi-crate root directly:

```bash
clidoc target/doc/my_crate
```

The directory must contain `all.html` or `sidebar-items*.js` files
(standard output of `cargo doc`).

## Cache

Indexed items and compressed HTML pages are stored in
`$XDG_CACHE_HOME/clidoc/index.db` (or `~/.cache/clidoc/index.db`).
On subsequent runs, unchanged sources load instantly from the cache.

## License

MIT
