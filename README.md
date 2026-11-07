# diskhog

Finds out where your disk space went, then suggests cleanups ranked by how much
they free up. It never deletes anything.

```bash
cargo install --path .     # or: cargo build --release && ./target/release/diskhog
diskhog                    # scan home, /Applications, /Library, Homebrew, temp, swap
diskhog ~/projects         # scan only specific folders
diskhog -n 40 -m 1GB       # show more suggestions, hide anything under 1 GB
```

What it does:

- **Fast parallel walk** (rayon). On macOS it uses `getattrlistbulk`, which gets a
  whole directory's names and sizes in a few syscalls instead of one `lstat` per file.
  It measures real on-disk usage (sparse files such as Docker.raw are handled
  correctly) and counts hard links once.
- **"Where the space is" tree** that drills into anything ≥ 1 GB and collapses
  single-child chains.
- **Cleanup suggestions** sorted by size, each tagged `SAFE` (regenerable cache or
  build output) or `REVIEW` (your data or installs), with the command or menu path to clean it:
  - ~60 known hogs: Docker/VMs, Xcode/Simulators, Android, Unity/Unreal editors,
    npm/yarn/pnpm/cargo/gradle/pip/go caches, AI models, iOS backups, Messages,
    browser/Electron caches, Spotify, and more (see `src/rules.rs`)
  - build artifacts found anywhere: `node_modules`, Cargo `target/`, Unity `Library/`,
    Gradle/Flutter/CMake/SwiftPM builds, venvs, … with the age of each folder and how
    much of it is stale
  - large files and leftover installer images (.dmg/.pkg/.iso/.xip/.ipsw)
- Space claimed by a more specific rule isn't counted twice (e.g. the Homebrew cache
  is subtracted from "Application caches").

Grant your terminal **Full Disk Access** (System Settings › Privacy & Security) to
let it see Mail, Trash and other protected folders.
