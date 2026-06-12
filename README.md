# neomake

A progressively enhanced build orchestrator in Rust. `neomake` reads a
TOML (or TOMLX) configuration file, builds a dependency DAG, runs tasks
concurrently on a `tokio` executor, and skips unchanged work via a
SHA-256 content-addressable cache.

## Highlights

- **TOML in, TOML out.** Your build file starts as plain TOML. When you
  need more, opt in to TOMLX — a strict superset adding variables,
  value-level conditionals, and a small library of built-ins
  (`env`, `glob`, `exec`).
- **Parallel by default.** Tasks with no dependency run concurrently.
  Concurrency is capped with `--concurrency N` (default: number of
  CPUs).
- **Content-addressable cache.** Each task's cache key is a SHA-256 of
  its command, env, sorted input file hashes, and the cache keys of its
  upstream dependencies, so a change anywhere in the transitive input
  closure invalidates every dependent.
- **Clear diagnostics.** Cycle detection reports the concrete cycle
  path; every config error carries a file name and line number.

## Workspace layout

```
neomake/             CLI binary
neomake-core/        task model, DAG builder, cache, tokio executor
neomake-tomlx/       TOMLX lexer, parser, evaluator
examples/            basic.toml, advanced.tomlx
docs/tomlx-spec.md   formal TOMLX spec
```

## Installation

Requires a recent stable Rust toolchain (1.75+).

```bash
cargo install --path neomake
```

A `cargo build --release` at the workspace root produces
`target/release/neomake`.

### Arch Linux (AUR)

`neomake` is available from the [AUR](https://wiki.archlinux.org/title/Arch_User_Repository). With [yay](https://github.com/Jguer/yay) or [paru](https://github.com/Morganamilo/paru):

```bash
yay -S neomake
# or
paru -S neomake
```

### GitHub Actions (reusable workflow)

Other repositories can call the reusable workflow to install `neomake`
from this repo and run it against the caller’s checkout:

```yaml
jobs:
  ci:
    uses: sinisterMage/neomake/.github/workflows/reusable-neomake.yml@main
    with:
      neomake-ref: main
      config-path: neomake.toml
```

See [`.github/workflows/reusable-neomake.yml`](.github/workflows/reusable-neomake.yml)
for all inputs (`working-directory`, `tasks`, `concurrency`, `extra-args`, …).

### Platform support

`neomake` runs on Linux, macOS, and Windows. Task commands are passed
to a shell whose choice depends on the host:

- **Unix-likes** — `sh -c <command>`.
- **Windows** — `pwsh -NoProfile -NoLogo -Command <command>` if
  PowerShell 7+ is on `PATH`, otherwise `powershell.exe` (Windows
  PowerShell 5.1), otherwise `cmd.exe /S /C` as a last-resort fallback.

Set `NEOMAKE_SHELL` to override; its value is whitespace-tokenized as
argv and the user's command is appended as the final argument:

```bash
NEOMAKE_SHELL="bash -c"          neomake run     # force bash
NEOMAKE_SHELL="cmd /S /C"        neomake run     # force cmd.exe on Windows
```

> **Cache compatibility.** This release bumps the cache format to
> `neomake-cache-v2` so input paths are hashed with portable `/`
> separators. Existing entries written by an older neomake will miss
> on first run after upgrading; use `neomake cache clean` if you want
> to reclaim the disk space immediately.

> **Windows PowerShell 5.1 redirection.** `>` writes UTF-16 + BOM by
> default in 5.1 (this is fixed in pwsh / PowerShell 7+). For
> deterministic byte-level output, prefer `pwsh` or use
> `Set-Content -Encoding ascii` / `-Encoding utf8NoBOM` instead.

## Quickstart

Create `neomake.toml`:

```toml
[tasks.build]
command = "cargo build --release"
inputs  = ["src/**/*.rs", "Cargo.toml"]
outputs = ["target/release/myapp"]

[tasks.test]
command = "cargo test"
deps    = ["build"]
```

Then:

```bash
neomake run               # builds all leaves (test → build)
neomake run build         # builds just `build` and its deps
neomake list              # shows the DAG in topological order
neomake cache status      # prints cache entry count and size
neomake cache clean       # empties the cache directory
```

### CLI reference

```
neomake [OPTIONS] <SUBCOMMAND>

Subcommands:
  run [TASK]...            Run tasks (defaults to all tasks)
  list                     Print tasks in topological order
  cache clean              Remove every cache entry
  cache status             Print cache size and entry count

Options:
  -c, --config <FILE>      Path to config; auto-detects neomake.toml or neomake.tomlx
      --concurrency <N>    Max parallel shell commands (default: num_cpus)
      --no-cache           Treat every task as a cache miss
  -q, --quiet              Suppress per-task stdout/stderr forwarding
```

Set `NEOMAKE_LOG=debug` to turn on `tracing` diagnostics.

## TOMLX at a glance

| Feature               | Syntax                                                      |
|-----------------------|-------------------------------------------------------------|
| Variable declaration  | `$profile = "release"`                                      |
| Variable reference    | `${profile}` (in strings) / `$profile` (in expressions)     |
| String interpolation  | `"target/${profile}/app"`                                   |
| String concatenation  | `"cargo build" + " --release"`                              |
| Conditional value     | `if ${ci} { "make ci" } else { "make" }`                    |
| Environment access    | `env("PROFILE", "debug")`                                   |
| File globbing         | `glob("src/**/*.rs")`                                       |
| Command execution     | `exec("git rev-parse HEAD")`                                |
| Git clone             | `git("github:owner/repo", "v1.0.0", "dest/%")`               |
| File download         | `fetch("https://url.com/file", "dest/%")`                    |

See [`docs/tomlx-spec.md`](docs/tomlx-spec.md) for the full grammar and
semantics, and [`examples/advanced.tomlx`](examples/advanced.tomlx) for
a complete example.

## Architecture

```
┌───────────────────┐        ┌──────────────────────────┐
│  *.toml / *.tomlx │──────▶│   neomake-tomlx          │
└───────────────────┘        │  (lexer / parser / eval) │
                             └───────────┬──────────────┘
                                         │ toml::Value
                                         ▼
                             ┌──────────────────────────┐
                             │   neomake-core           │
                             │   TaskSet → DAG          │
                             │         │                │
                             │         ▼                │
                             │   Executor (tokio)       │
                             │         │                │
                             │         ▼                │
                             │   Cache (.neomake/cache) │
                             └──────────────────────────┘
```

- **DAG construction** uses Kahn's algorithm for topological sorting; if
  a cycle is detected, an iterative DFS recovers the concrete cycle
  path (e.g. `a -> b -> c -> a`) and reports it.
- **Cache keys** are SHA-256 over a versioned canonical byte layout
  (`neomake-cache-v1`) built from the command, env, sorted input file
  hashes, and upstream dep keys. See `neomake-core/src/cache.rs` for the
  exact format.
- **Parallel execution** uses one tokio task per DAG node, a per-task
  `watch` channel to propagate outcomes to dependents, and a
  `Semaphore` to cap concurrent shell commands. Cache hits bypass the
  semaphore entirely.


## License

MIT
