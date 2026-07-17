# Herdr Recent Navigator

A recent workspaces/tabs/panes switcher for [Herdr](https://herdr.dev/) **≥0.7.4**. Opens an popup listing
recently focused workspaces, tabs, panes, and AI agents — fuzzy-searchable and
navigable by keyboard.

![Screenshot](https://github.com/beyondlex/images/blob/main/recent-navigator.jpg)

<p align="center">
  <img alt="Herdr 0.7.4+" src="https://img.shields.io/badge/Herdr-0.7.4%2B-6693ff" />
  <img alt="Linux and macOS" src="https://img.shields.io/badge/Platform-Linux%20%7C%20macOS-2eb14f" />
  <img alt="Release" src="https://img.shields.io/github/v/release/beyondlex/herdr-recent-navigator" />
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/License-MIT-cd933e" /></a>
</p>

## Demo

<p align="center">
  <img alt="demo" src="https://github.com/beyondlex/images/blob/main/recent-navigator.gif" width="559px" />
</p>

## Features

- **Four category tabs**: Workspaces, Tabs, Agents, Panes — switch with `Tab`
- **MRU ordering**: most recently focused items float to the top
- **Fuzzy search**: type to filter any category
- **Live agent status**: Working agents show a braille spinner; status updates
  in real time without reopening
- **Herdr-native colors**: TokyoNight palette, consistent with the Herdr UI
- **Automatic tracking**: hooks into `workspace.focused`, `pane.focused`,
  `tab.focused` events to build `MRU` history

## Install

### Quick install (curl | bash)

```bash
curl -fsSL https://raw.githubusercontent.com/beyondlex/herdr-recent-navigator/main/install.sh | bash
```

This downloads the prebuilt binary for your platform, places it in
`~/.local/bin/`, and links it into Herdr.

### Build from source

```bash
git clone https://github.com/beyondlex/herdr-recent-navigator
cd herdr-recent-navigator
cargo build --release
herdr plugin link "$PWD"
```

Verify the plugin is registered:

```bash
herdr plugin action list --plugin beyondlex.herdr-recent-navigator
```

## Bind a shortcut

Add to your Herdr config:

```toml
[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "beyondlex.herdr-recent-navigator.focus-workspaces"
description = "Open Navigator: Workspace"


# Optional: Focus Tab/Agent when open navigator
[[keys.command]]
key = "cmd+i"
type = "plugin_action"
command = "beyondlex.herdr-recent-navigator.focus-tabs"
description = "Open Navigator: Tab"

[[keys.command]]
key = "cmd+e"
type = "plugin_action"
command = "beyondlex.herdr-recent-navigator.focus-panes"
description = "Open Navigator: Agent"
```

Reload:

```bash
herdr server reload-config
```

Press the shortcut to open the navigator popup.

## Configuration

Open the plugin manifest at the path shown by:

```bash
herdr plugin list --json | grep manifest_path
```

Add or edit the `theme` field:

```toml
# ~/.local/share/herdr-recent-navigator/herdr-plugin.toml
theme = "light"        # "dark" (default) or "light"
```

The navigator uses a dark TokyoNight palette by default. Set `theme = "light"` for a light palette. Full per-theme auto-detection will be added once Herdr sends the theme name via `HERDR_PLUGIN_CONTEXT_JSON`.

## Usage

| Key | Action |
|---|---|
| `↑` / `↓` | Navigate list |
| `Tab` / `Shift+Tab` | Cycle category tabs |
| `Enter` | Focus selected item |
| `Esc` | Clear search / close |
| `Ctrl+C` | Close without focusing |
| Type any text | Fuzzy-search the list |

### Category tabs

- **Workspaces**: MRU workspaces with dot indicators for agent status
- **Tabs**: MRU tabs within those workspaces
- **Agents**: AI agents sorted by last activity
- **Panes**: Individual terminal panes


## License

MIT

