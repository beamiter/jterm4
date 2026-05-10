# jterm4 User Guide

## 🚀 Getting Started

### First Launch

```bash
./target/release/jterm4
```

jterm4 will:
- Create config directory at `~/.config/jterm4/`
- Use default configuration
- Open with a single tab

### Configuration

Edit `~/.config/jterm4/config.toml`:

```toml
terminal_mode = "block"  # or "vte"
opacity = 0.95
font = "SauceCodePro Nerd Font Regular 14"
theme = "default"
```

See [config.toml.example](../config.toml.example) for all options.

---

## 📑 Tab Management

### Creating Tabs

| Action | Shortcut | Description |
|--------|----------|-------------|
| New tab | `Ctrl+Shift+T` | Opens in current directory |
| New tab (button) | Click `+` in top bar | Same as shortcut |

### Switching Tabs

| Action | Shortcut |
|--------|----------|
| Next tab | `Ctrl+Tab` |
| Previous tab | `Ctrl+Shift+Tab` |
| Quick switch | `Alt+1` to `Alt+9` |
| Mouse | Click tab in sidebar |

### Closing Tabs

| Action | Shortcut |
|--------|----------|
| Close current | `Ctrl+Shift+W` |
| Close (mouse) | Click `×` on tab |

**Protection**: jterm4 will warn you before closing tabs with running processes (ssh, docker, nix develop, etc.)

### Organizing Tabs

- **Drag-and-drop**: Click and drag tabs in sidebar to reorder
- **Rename**: Double-click tab label to rename
- **Auto-naming**: Tabs auto-update their names based on current directory

---

## 🪟 Split Panes

### Creating Splits

| Action | Shortcut | Result |
|--------|----------|--------|
| Split horizontally | `Ctrl+Shift+\` | Left/Right panes |
| Split vertically | `Ctrl+Shift+\|` | Top/Bottom panes |

### Navigating Splits

| Action | Shortcut |
|--------|----------|
| Cycle panes | `Alt+Tab` / `Alt+Shift+Tab` |
| Focus left | `Alt+←` |
| Focus right | `Alt+→` |
| Focus up | `Alt+↑` |
| Focus down | `Alt+↓` |

### Resizing Panes

| Action | Shortcut |
|--------|----------|
| Resize left | `Alt+Shift+←` |
| Resize right | `Alt+Shift+→` |
| Resize up | `Alt+Shift+↑` |
| Resize down | `Alt+Shift+↓` |

### Managing Panes

| Action | Shortcut | Description |
|--------|----------|-------------|
| Zoom pane | `Ctrl+Shift+Z` | Maximize current pane |
| Unzoom | `Ctrl+Shift+Z` | Restore split layout |
| Close pane | `Ctrl+Shift+W` | Close focused pane |
| Move to new tab | `Ctrl+Shift+!` | Extract pane as new tab |

**Persistence**: Split layouts are automatically saved and restored on restart!

---

## 🔍 Search

### Basic Search

1. Press `Ctrl+Shift+F` to open search
2. Type your search term
3. Press `Enter` to find next
4. Press `Shift+Enter` to find previous
5. Press `Esc` to close search

### Search Features

- **Block Mode**: Searches across all command blocks
- **VTE Mode**: Uses VTE's built-in regex search
- **Case sensitive** by default
- **Regex support** (in VTE mode)

---

## 💾 Session Persistence

### What Gets Saved

jterm4 automatically saves:
- ✅ All open tabs
- ✅ Working directory per tab/pane
- ✅ Split pane layouts (complete structure)
- ✅ Running environments (nix develop, ssh, docker)
- ✅ Session history (with rsh)

### When State is Saved

State is automatically saved when:
- ✅ You close jterm4 (Ctrl+W on last tab)
- ✅ You add/remove tabs
- ✅ Window is closed

### State Location

```bash
~/.config/jterm4/tabs.state
```

### Viewing State

```bash
# Pretty-print state
./scripts/show-state.sh

# Raw state file
cat ~/.config/jterm4/tabs.state
```

### Resetting State

```bash
# Clear saved state
./scripts/debug.sh clean-state

# Or manually
rm ~/.config/jterm4/tabs.state
```

---

## 🎨 Customization

### Themes

Built-in themes:
- `default` - Dark theme
- `light` - Light theme

```toml
theme = "default"
```

### Fonts

Recommended: Use Nerd Fonts for best icon support

```toml
font = "SauceCodePro Nerd Font Regular 14"
```

### Opacity

```toml
opacity = 0.95  # 0.0 = transparent, 1.0 = opaque
```

Runtime adjustment:
- Increase: `Ctrl+Shift+O`
- Decrease: `Ctrl+Alt+O`

### Font Size

Runtime adjustment:
- Increase: `Ctrl+Plus` or `Ctrl+=`
- Decrease: `Ctrl+Minus` or `Ctrl+-`
- Reset: (set in config)

---

## 🔧 Advanced Features

### Startup Commands

Auto-run commands in new tabs:

```toml
startup_commands = "cd ~/projects, nix develop"
```

Commands are comma-separated.

### Environment Detection

jterm4 automatically detects and restores:

- **nix develop** - Nix development shells
- **nix-shell** - Legacy nix shells
- **ssh / mosh** - Remote connections
- **docker exec** - Container sessions
- **docker compose exec** - Compose services
- **podman exec** - Podman containers

### Shell Integration

Best with **rsh** (Rust shell):
- Session history persistence
- Better prompt detection
- Faster startup

Fallback to `bash -l` if rsh not available.

---

## ⌨️ Complete Keyboard Reference

### Tab Management

| Shortcut | Action |
|----------|--------|
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Shift+W` | Close tab/pane |
| `Ctrl+Tab` | Next tab |
| `Ctrl+Shift+Tab` | Previous tab |
| `Alt+1` through `Alt+9` | Switch to tab 1-9 |

### Pane Management

| Shortcut | Action |
|----------|--------|
| `Ctrl+Shift+\` | Split horizontally |
| `Ctrl+Shift+\|` | Split vertically |
| `Alt+←/→/↑/↓` | Focus pane (directional) |
| `Alt+Shift+←/→/↑/↓` | Resize pane |
| `Alt+Tab` | Cycle pane focus forward |
| `Alt+Shift+Tab` | Cycle pane focus backward |
| `Ctrl+Shift+Z` | Toggle pane zoom |
| `Ctrl+Shift+!` | Move pane to new tab |

### Editing

| Shortcut | Action |
|----------|--------|
| `Ctrl+Shift+C` | Copy |
| `Ctrl+Shift+V` | Paste |
| `Ctrl+Shift+F` | Find/Search |

### View

| Shortcut | Action |
|----------|--------|
| `Ctrl+Plus` | Increase font size |
| `Ctrl+Minus` | Decrease font size |
| `Ctrl+Shift+O` | Increase opacity |
| `Ctrl+Alt+O` | Decrease opacity |
| `Ctrl+\` | Toggle sidebar |

### System

| Shortcut | Action |
|----------|--------|
| `Ctrl+Shift+K` | Show keybindings |
| `Ctrl+Shift+R` | Reload config |
| `Ctrl+Q` | Quit |

---

## 🐛 Troubleshooting

### Tabs Not Restoring

**Problem**: Tabs don't restore on startup

**Solutions**:
```bash
# Check state file exists
ls -la ~/.config/jterm4/tabs.state

# View state file
./scripts/show-state.sh

# Check logs
JTERM4_LOG=debug ./target/release/jterm4
```

### Split Layouts Not Restoring

**Problem**: Split pane layout is lost

**Check**:
- State file format (should be JSON)
- Logs for parsing errors
- Manually verify JSON structure

### Running Process Not Detected

**Problem**: No confirmation when closing tab with ssh/docker

**Check**:
- Process must be foreground (not background)
- Supported: ssh, mosh, docker, nix develop
- View detected processes in logs

### Performance Issues

**Problem**: Slow rendering or high CPU

**Solutions**:
```toml
# Reduce cache size
ansi_cache_capacity = 500

# Tighter batching
output_batch_max_ms = 16

# Limit visible blocks
max_visible_blocks = 50
```

### Configuration Not Loading

**Problem**: Config changes don't apply

**Solutions**:
```bash
# Check config file location
ls -la ~/.config/jterm4/config.toml

# Validate TOML syntax
cat ~/.config/jterm4/config.toml

# Reset to defaults
./scripts/debug.sh reset-config

# Reload config
# Press Ctrl+Shift+R in jterm4
```

---

## 📚 Additional Resources

- [Performance Guide](PERFORMANCE.md)
- [Changelog](../CHANGELOG.md)
- [Optimization Summary](../OPTIMIZATION_SUMMARY.md)
- [Configuration Example](../config.toml.example)

---

## 💬 Getting Help

1. Check this guide
2. Review CHANGELOG for recent changes
3. Enable debug logging: `JTERM4_LOG=debug`
4. Run diagnostics: `./scripts/debug.sh info`

---

**Happy terminal-ing!** 🎉
