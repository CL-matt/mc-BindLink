# bindsync

Tired of manually rebinding dozens of keys every time you switch modpacks, upgrade versions, or download a friend's client? **BindSync** is a lightweight, standalone desktop utility written in Rust that ports your personal keybindings and mod-specific configurations across instances seamlessly.

### ✨ Key Features
- **One-Click Sync**: Transfer your controls and preferences instantly—no need to touch `options.txt` by hand.
- **Auto-Detection for Launchers**: Automatically detects HMCL and PCL2 instances, including isolated version folders (`.minecraft/versions/<name>`).
- **Smart Mod Inspection**: Inspects `.jar` metadata (`fabric.mod.json` & `mods.toml`) to detect installed mods (e.g., Axiom, Flashback) and syncs their dedicated config files safely.
- **Two Flexible Sync Modes**:
  - **Partial Merge**: Injects your custom keybinds while keeping the modpack's original render distance, FOV, and sound levels intact.
  - **Full Overwrite**: Synchronizes your full player profile (render distance, sensitivity, master volumes, FOV, and controls).
- **Safety First**: Automatically creates a `.bak` backup before modifying any files.
- **Zero Overhead**: Single, tiny binary (~4MB) with instantaneous startup and zero runtime dependencies.