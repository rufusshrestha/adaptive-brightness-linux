# Release Notes - LBrightness 3.0.0 (Reliable Ambient Light & Complete Packaging)

**Release Tag:** `v3.0.0`  
**Status:** Major Version 3.0 Stable Release

---

## 🌟 Highlights

- **Self-Healing Ambient Light Sensor:** Reads light levels from `iio-sensor-proxy` (via `monitor-sensor`) when available and falls back to sysfs IIO. Re-detects the sensor if its IIO device number changes and restarts sensor reads after resume from standby.
- **Automatic Stuck-Sensor Reset:** If the sysfs reading stays frozen (disagreeing with `iio-sensor-proxy` for 3 minutes), the daemon resets the `hid_sensor_als` driver through a small root helper unit, `lbright-als-reset.service`. A polkit rule lets only the active local user start that one unit. Trigger it manually with the new `lbright als-reset` command.
- **Sub-Lux Precision:** Dim readings (below 0.5 lux) no longer round down to 0.
- **Better `lbright status`:** Shows both the iio-sensor-proxy and sysfs readings and warns when they disagree.
- **Pause on Manual Adjustment:** Changing brightness by hand pauses auto brightness until the next standby or restart. Startup and resume brightness restores from the desktop session are ignored.
- **Auto-Recovery:** Auto brightness re-enables when brightness drops below 5%, and a missing `config.ini` is recreated automatically.
- **Full Brightness Fix:** Brightness can now reach 100% in bright sunlight (previously capped at 60%).

## ⚠️ Behaviour Changes

- **`uninstall.sh` removes everything by default**, including `~/.config/lbrightness/config.ini` and `~/.local/state/lbrightness`. Use `./uninstall.sh --keep-config` to keep them. It also stops any lbright processes and checks that nothing is left behind.
- **`install.sh` installs the root sensor-reset helper by default** and asks for `sudo`. Use `./install.sh --user-only` to skip it. It refuses to run if the `.deb` is installed.
- **The `.deb` now enables and starts `lbright.service` automatically** for all users, and stops it on removal. It recommends `iio-sensor-proxy`.

## 📝 Commits in This Release

All changes since 2.0.0 (`29310cf`), by Rufus Shrestha:

- `2c90aef` Fix brightness not reaching 100 percent when hitting sun - bug maxes out at 60
- `2874c91` feat: pause auto brightness on manual adjustment until standby or restart
- `2a6dea5` feat: auto enable auto brightness when brightness < 5% and recreate missing config
- `b761eec` fix(daemon): ignore desktop session brightness restore during startup/resume grace period
- `a6d0146` feat(ambient): self-healing light sensor reads and complete install/uninstall
- chore(release): bump version to 3.0.0 (the `v3.0.0` tagged commit)

**Contributors:** Rufus Shrestha

---

## 📦 Release Assets

- `lbright_3.0.0_amd64.deb` — Debian/Ubuntu/Mint installable package
- `lbright-v3.0.0-x86_64-linux.tar.gz` — Standalone binary archive
- `SHA256SUMS.txt` — SHA256 checksums for package integrity verification

---

# Release Notes - LBrightness 2.0.0 (Rust Rewrite)

**Release Tag:** `v2.0.0`  
**Status:** Major Version 2.0 Stable Release

---

## 🌟 Highlights

- **Complete 2.0 Rust Architecture (`lbright`):** Core background daemon and CLI rewritten in 100% pure standard library Rust, reducing background memory consumption from **~80MB to < 3.5MB RSS** with 0.0% idle CPU overhead.
- **Direct Sysfs Kernel I/O:** Fast direct reading and writing to `/sys/class/backlight/*` with microsecond latency and zero subprocess spawning overhead.
- **External Monitor Support (DDC/CI):** Dedicated background worker thread communicates with external monitors via DDC/CI (VCP `0x10`) through `ddcutil` with strict non-blocking timeouts (5s set, 10s detect) to prevent desktop freezes.
- **Interactive ANSI Terminal UI:** Fast, zero-dependency ANSI menu (`lbright tui`) to inspect live status, interactively edit 24h curves, toggle ALS, pause/resume, and rescan displays.
- **Non-Destructive Migration Engine:** `lbright migrate [--dry-run]` imports existing `~/.config/auto-brightness/profiles.conf` into structured `~/.config/lbrightness/config.ini` without modifying the original configuration.
- **Advisory Config Locking:** Prevents write collisions between background daemon learning and interactive TUI saves.
- **Hardened Systemd User Service:** `lbright.service` configured with `NoNewPrivileges=true` and `PrivateTmp=true`.
- **Multi-Channel Packaging:** Full support for Debian package installation (`.deb`), automated user installer (`install.sh`), standalone precompiled tarballs, and source builds via Cargo.

---

## 📦 Release Assets

- `lbright_2.0.0_amd64.deb` — Debian/Ubuntu/Mint installable package
- `lbright-v2.0.0-x86_64-linux.tar.gz` — Standalone binary archive
- `SHA256SUMS.txt` — SHA256 checksums for package integrity verification
