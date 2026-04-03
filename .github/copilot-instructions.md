# GitHub Copilot Instructions – cyphal_service

## Repository overview

`cyphal_service` is a Rust binary that runs as a **systemd-based daemon** on
Ubuntu 24.04 LTS.  It can also be started in **foreground / REPL mode** with
the `--foreground` flag, making it portable to any host.

### Key responsibilities
- Output `hello` every 60 seconds (background timer tick).
- Expose a pluggable **command registry** so additional modules can register
  their own REPL commands without changing existing code.
- Integrate with **systemd** via `sd_notify` (Type=notify) for clean
  start/stop lifecycle management.

---

## Project structure

```
cyphal_service/
├── src/
│   ├── main.rs           – Entry point, CLI argument parsing (clap)
│   ├── daemon.rs         – Async daemon loop (tokio) + systemd sd_notify
│   ├── cli.rs            – Foreground REPL (rustyline) + background tick thread
│   └── commands/
│       ├── mod.rs        – Command trait & CommandRegistry
│       ├── hello.rs      – HelloCommand ("hello" REPL command)
│       └── help.rs       – HelpCommand + format_help() utility
├── config/
│   └── cyphal_service.service  – systemd unit file
├── .github/
│   ├── copilot-instructions.md  – this file
│   └── workflows/
│       ├── ci.yml        – Build, clippy, fmt, test on every push/PR
│       └── release.yml   – Build release artefact and attach to GitHub Release
└── README.md
```

---

## Adding a new command

1. Create `src/commands/<name>.rs`.
2. Implement the `Command` trait:
   ```rust
   use crate::commands::Command;

   pub struct MyCommand;

   impl Command for MyCommand {
       fn name(&self) -> &'static str { "my-cmd" }
       fn description(&self) -> &'static str { "Does something useful" }
       fn execute(&self, args: &[&str]) -> String {
           format!("args: {:?}", args)
       }
   }
   ```
3. Add `pub mod <name>;` to `src/commands/mod.rs`.
4. Register the command in `cli::register_defaults` in `src/cli.rs`:
   ```rust
   registry.register(Box::new(MyCommand));
   ```
5. Write unit tests inside a `#[cfg(test)] mod tests` block in the new file.

---

## Development workflow

### Building
```bash
cargo build            # debug build
cargo build --release  # optimised release build
```

### Linting & formatting (required before finishing any session)
```bash
cargo fmt --all                                 # auto-format
cargo fmt --all -- --check                      # check only (CI)
cargo clippy --all-targets --all-features -- -D warnings
```

### Testing
```bash
cargo test --all-targets
```

> **A session is not considered finished until all three of the following
> pass without errors or warnings:**
> 1. `cargo fmt --all -- --check`
> 2. `cargo clippy --all-targets --all-features -- -D warnings`
> 3. `cargo test --all-targets`

### Running locally
```bash
# Foreground REPL mode
cargo run -- --foreground

# Daemon mode (blocks until SIGTERM / Ctrl-C)
cargo run
```

---

## Code conventions

- Rust edition **2024**.
- Async runtime: **tokio** (full feature set).
- CLI parsing: **clap** with the `derive` feature.
- Logging: **tracing** + **tracing-subscriber** (env-filter).
- REPL line editing: **rustyline**.
- systemd notify: **sd-notify** (degrades gracefully when not under systemd).
- Use `tracing::{info, warn, error}` macros instead of `println!` in
  production paths (except the deliberate `hello` output).
- Follow standard Rust naming conventions (snake_case for functions and
  variables, CamelCase for types).
- Keep modules small and focused; new features go in their own files under
  `src/commands/`.

---

## CI / CD

| Workflow | Trigger | What it does |
|----------|---------|--------------|
| `ci.yml` | Every push & PR | fmt check → clippy → build → test |
| `release.yml` | GitHub Release published | Release build → tarball → uploaded to release assets |

The release artefact is a `.tar.gz` containing the binary, the systemd unit
file, and an `install.sh` helper script.  It is built on `ubuntu-24.04` and
targets `x86_64-unknown-linux-gnu`.
