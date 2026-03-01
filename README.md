# ai-jail

A sandbox wrapper for AI coding agents (Linux: `bwrap`, macOS: `sandbox-exec`). Isolates tools like Claude Code, GPT Codex, OpenCode, and Crush so they can only access what you explicitly allow.

## Install

### From source

```bash
cargo build --release
cp target/release/ai-jail ~/.local/bin/
```

### Dependencies

- Linux: [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`) must be installed:
  - Arch: `pacman -S bubblewrap`
  - Debian/Ubuntu: `apt install bubblewrap`
  - Fedora: `dnf install bubblewrap`
- macOS: `/usr/bin/sandbox-exec` is used (legacy/deprecated Apple interface).

## Quick Start

```bash
cd ~/Projects/my-app

# Run Claude Code in a sandbox
ai-jail claude

# Run bash inside the sandbox (for debugging)
ai-jail bash

# See what the sandbox would do without running it
ai-jail --dry-run claude
```

On first run, `ai-jail` creates a `.ai-jail` config file in the current directory. Subsequent runs reuse that config. Commit `.ai-jail` to your repo so the sandbox settings follow the project.

## Security Notes (Important)

The default mode is a usability-oriented sandbox, not a maximal lockbox. The following are intentionally still open in default mode:

1. Docker socket passthrough can be auto-enabled when `/var/run/docker.sock` exists (`--no-docker` disables it).
2. Display passthrough mounts `XDG_RUNTIME_DIR` on Linux, which can expose extra host IPC sockets.
3. Environment variables are inherited by default (tokens/secrets in your shell env are visible in-jail).

If you want a suspicious command / malware-analysis posture, use `--lockdown` (see below).

## Sandbox Model Differences

`ai-jail` is a thin wrapper around native OS sandboxing, so security properties depend on the backend:

- `bwrap` (Linux): namespace + mount sandboxing in userspace.
- `sandbox-exec` / seatbelt profile (macOS): legacy policy interface to Apple sandbox rules.
- `AppContainer` (Windows): token/capability-based app sandbox model (not currently implemented by `ai-jail`).

Important differences and inherent limits (cannot be fully solved in this project):

- Kernel trust boundary:
  `bwrap`, seatbelt, and AppContainer all depend on host kernel correctness. Kernel escapes are out of scope for wrapper-level hardening.
- Shared-kernel model:
  They are process sandboxes, not hardware isolation. A guest VM gives a stronger boundary because it runs a separate kernel.
- Side channels and host resource sharing:
  Timing/cache side channels, scheduler interference, and other shared-resource effects still exist in process sandboxes.
- Backend maturity and semantics vary by OS:
  Linux/macOS/Windows primitives are not equivalent; policy parity is approximate, not identical.
- macOS seatbelt CLI status:
  `sandbox-exec` is a deprecated interface; long-term behavior is less future-proof than current VM/container stacks.

If your expectation is "full isolation against unknown malware", use a dedicated VM (or microVM) with disposable disks/snapshots and treat `ai-jail` as defense-in-depth, not as a complete replacement.

## Lockdown Mode

`--lockdown` enables strict read-only, ephemeral behavior aimed at hostile workloads.

```bash
ai-jail --lockdown claude
```

What `--lockdown` does:

- Forces read-only project mount.
- Disables GPU, Docker, display passthrough, and mise integration.
- Ignores extra map flags from config/CLI (`--rw-map`, `--map`).
- Mounts `$HOME` as tmpfs only (no host dotfiles layered in).
- Linux: adds `--clearenv` + minimal env allowlist, `--unshare-net`, `--new-session`.
- macOS: clears environment to minimal allowlist and removes network/file-write allowances from generated SBPL profile.

Persistence behavior:

- Runtime `--lockdown` sessions do not auto-write `.ai-jail` (to keep runs non-persistent).
- Persist lockdown explicitly with `ai-jail --init --lockdown ...`.
- Disable persisted lockdown with `--no-lockdown` (and `--init` if you want to write config).

## What Gets Sandboxed

### Default behavior (no flags needed)

| Resource | Access | Notes |
|----------|--------|-------|
| `/usr`, `/etc`, `/opt`, `/sys` | read-only | System binaries and config |
| `/dev`, `/proc` | device/proc | Standard device and process access |
| `/tmp`, `/run` | tmpfs | Fresh temp dirs per session |
| `$HOME` | tmpfs | Empty home, then dotfiles layered on top |
| Project directory (pwd) | **read-write** | The whole point |
| GPU devices (`/dev/nvidia*`, `/dev/dri`) | device | For GPU-accelerated tools |
| Docker socket | read-write | If `/var/run/docker.sock` exists |
| X11/Wayland | passthrough | Display server access |
| `/dev/shm` | device | Shared memory (Chromium needs this) |

In `--lockdown`, project is mounted read-only and host write mounts are removed.

### Home directory handling

Your real `$HOME` is replaced with a tmpfs. Dotfiles and dotdirs are selectively mounted on top:

**Never mounted (sensitive data):**
- `.gnupg`, `.aws`, `.ssh`, `.mozilla`, `.basilisk-dev`, `.sparrow`

**Mounted read-write (AI tools and build caches):**
- `.claude`, `.crush`, `.codex`, `.aider`, `.config`, `.cargo`, `.cache`, `.docker`

**Everything else:** mounted read-only.

**Additionally hidden (tmpfs over):**
- `~/.config/BraveSoftware`, `~/.config/Bitwarden`
- `~/.cache/BraveSoftware`, `~/.cache/chromium`, `~/.cache/spotify`, `~/.cache/nvidia`, `~/.cache/mesa_shader_cache`, `~/.cache/basilisk-dev`

**Explicit file mounts:**
- `~/.gitconfig` (read-only)
- `~/.claude.json` (read-write)

**Local overrides (read-write):**
- `~/.local/state`
- `~/.local/share/{zoxide,crush,opencode,atuin,mise,yarn,flutter,kotlin,NuGet,pipx,ruby-advisory-db,uv}`

### Namespace isolation

The sandbox uses PID, UTS, and IPC namespace isolation. The hostname inside is `ai-sandbox`. The process dies when the parent exits (`--die-with-parent`).
Linux enables `--new-session` for non-interactive runs and always in `--lockdown`. In `--lockdown`, Linux also unshares network.

### mise integration

If [mise](https://mise.jdx.dev/) is found on `$PATH`, the sandbox automatically runs `mise trust && mise activate bash && mise env` before your command. This gives AI tools access to project-specific language versions. Disable with `--no-mise`.

## Usage

```
ai-jail [OPTIONS] [--] [COMMAND [ARGS...]]
```

### Commands

| Command | What it does |
|---------|-------------|
| `claude` | Run Claude Code |
| `codex` | Run GPT Codex |
| `opencode` | Run OpenCode |
| `crush` | Run Crush |
| `bash` | Drop into a bash shell |
| `status` | Show current `.ai-jail` config |
| Any other | Passed through as the command |

If no command is given and no `.ai-jail` config exists, defaults to `bash`.

### Options

| Flag | Description |
|------|-------------|
| `--rw-map <PATH>` | Mount PATH read-write (repeatable) |
| `--map <PATH>` | Mount PATH read-only (repeatable) |
| `--lockdown` / `--no-lockdown` | Enable/disable strict read-only lockdown mode |
| `--gpu` / `--no-gpu` | Enable/disable GPU passthrough |
| `--docker` / `--no-docker` | Enable/disable Docker socket |
| `--display` / `--no-display` | Enable/disable X11/Wayland |
| `--mise` / `--no-mise` | Enable/disable mise integration |
| `--clean` | Ignore existing config, start fresh |
| `--dry-run` | Print the bwrap command without executing |
| `--init` | Create/update config and exit (don't run) |
| `--bootstrap` | Generate smart permission configs for AI tools |
| `-v`, `--verbose` | Show detailed mount decisions |
| `-h`, `--help` | Show help |
| `-V`, `--version` | Show version |

### Examples

```bash
# Share an extra library directory read-write
ai-jail --rw-map ~/Projects/shared-lib claude

# Read-only access to reference data
ai-jail --map /opt/datasets claude

# No GPU, no Docker, just the basics
ai-jail --no-gpu --no-docker claude

# Suspicious/untrusted workload mode
ai-jail --lockdown bash

# See exactly what mounts are being set up
ai-jail --dry-run --verbose claude

# Create config without running
ai-jail --init --no-docker claude

# Regenerate config from scratch
ai-jail --clean --init claude

# Pass flags through to the sub-command (after --)
ai-jail -- claude --model opus
```

## Config File (`.ai-jail`)

Created automatically in the project directory on first run. Example:

```toml
# ai-jail sandbox configuration
# Edit freely. Regenerate with: ai-jail --clean --init

command = ["claude"]
rw_maps = ["/home/user/Projects/shared-lib"]
ro_maps = []
no_gpu = true
lockdown = true
```

### Merge behavior

When CLI flags are provided alongside an existing config:

- **command**: CLI replaces config
- **rw_maps / ro_maps**: CLI values are appended (duplicates removed)
- **Boolean flags**: CLI overrides config (`--no-gpu` sets `no_gpu = true`)
- The config file is updated after merge in normal mode; lockdown runtime skips auto-save

### Available fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string array | `["bash"]` | Command to run inside sandbox |
| `rw_maps` | path array | `[]` | Extra read-write mounts |
| `ro_maps` | path array | `[]` | Extra read-only mounts |
| `no_gpu` | bool | not set (auto) | `true` disables GPU passthrough |
| `no_docker` | bool | not set (auto) | `true` disables Docker socket |
| `no_display` | bool | not set (auto) | `true` disables X11/Wayland |
| `no_mise` | bool | not set (auto) | `true` disables mise integration |
| `lockdown` | bool | not set (disabled) | `true` enables strict read-only lockdown mode |

When a boolean field is not set, the feature is enabled if the resource exists on the host.

## License

GPL-3.0. See [LICENSE](LICENSE).
