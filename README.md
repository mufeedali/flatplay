# Flatplay

Flatplay is a CLI tool to build and run Flatpak applications. It's designed to be easy to integrate into your workflows with editors like Zed or neovim.

Note that this is a **work in progress** and you might encounter issues along the way.

## External Dependencies

Flatplay uses the **`flatpak` CLI** for sandboxing and lifecycle (`build-init`, `build`, `build-finish`, export). It does **not** invoke **`flatpak-builder`** — modules and sources are handled in-process.

| Need | How |
|------|-----|
| Sandbox / lifecycle | **`flatpak`** on PATH |
| SDK / runtime | Installed via Flatpak (e.g. `flatpak install org.gnome.Sdk//…`) |
| Git sources | **libgit2** (linked), not host `git` |
| A11y bus | **zbus** (optional `gdbus` fallback) |
| Toolchains (meson, ninja, gcc, …) | Inside the Flatpak **SDK** image |

Architecture: typed sources + download cache, **one** in-process module builder for deps and app, `FlatpakRunner` (`flatpak build`).

### Notes / limitations

- Manifest discovery scans the project tree (skips heavy dirs like `node_modules` / `target`) with a cheap text prefilter before full parse.
- `sdk-extensions` and `cleanup` are parsed but not fully applied yet.
- Host font remapping uses the usual FHS / XDG locations Flatpak expects.

## Installation & Usage

```bash
# Install flatplay using cargo
cargo install flatplay
# or if you want to install the latest version from git:
# cargo install --git https://github.com/mufeedali/flatplay/

# Optionally, enable completions (replace 'fish' with your shell)
flatplay completions fish > ~/.config/fish/completions/flatplay.fish

# Run the help command to see available features.
flatplay --help

# cd into a project directory
# then simply run:
flatplay

# This will attempt to build and run the project.
```

## Integrate into editors

### Zed

To integrate flatplay in Zed you can define custom tasks and key bindings.

Add the following tasks to your `tasks.json` file (you can open it with the `zed: open tasks` action):

```json
{
  "label": "build & run flatpak",
  "command": "flatplay build-and-run"
},
{
  "label": "build flatpak",
  "command": "flatplay build"
},
{
  "label": "run flatpak",
  "command": "flatplay run"
},
{
  "label": "stop flatpak",
  "command": "flatplay stop"
},
{
  "label": "clean flatpak build",
  "command": "flatplay clean"
},
{
  "label": "update flatpak dependencies",
  "command": "flatplay update-dependencies"
},
{
  "label": "export flatpak bundle",
  "command": "flatplay export-bundle"
},
{
  "label": "select flatpak manifest",
  "command": "flatplay select-manifest"
}
```

Next, add new bindings to your `keymaps.json` file (you can open it with the `zed: open keymap` action):

```json
{
  "context": "Workspace",
  "bindings": {
    "ctrl-alt-b": ["task::Spawn", { "task_name": "build & run flatpak" }],
    "ctrl-alt-r": ["task::Spawn", { "task_name": "run flatpak" }],
    "ctrl-alt-c": ["task::Spawn", { "task_name": "stop flatpak" }]
  }
},
```

Then you can run any of the tasks by pressing `Alt-Shift-T`, or use `Ctrl-Alt-B`, `Ctrl-Alt-R` or `Ctrl-Alt-C` to build & run, run or stop flatpaks.

# Some notes

- A lot of the logic is borrowed from [`flatpak-vscode`](https://github.com/bilelmoussaoui/flatpak-vscode).
- There will be bugs and missing features. Please report them, or better yet, send a PR.
- I'm not a Rust programmer.
- I hope to improve it over time and I hope Bilal forgives me for the sins I've committed here.
