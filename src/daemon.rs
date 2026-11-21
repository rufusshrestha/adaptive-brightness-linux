use std::fs;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::ambient::{AmbientSensor, AmbientTracker};
use crate::backlight::InternalPanel;
use crate::config::{get_config_path, load_config, save_config, Config, DeviceId};
use crate::ddc::{start_ddc_worker, DdcCommand};
use crate::profile::interpolate_linear;
use crate::state::{
    clear_manual_override, get_state_dir, read_pause_state, set_pause_state, write_device_state,
    PauseState,
};

/// Internal smooth fading state machine.
pub struct SmoothFader {
    pub current_raw: i64,
    pub target_raw: i64,
    pub max_raw: i64,
    pub step_raw: i64,
}

impl SmoothFader {
    pub fn new(current_raw: i64, max_raw: i64) -> Self {
        Self {
            current_raw,
            target_raw: current_raw,
            max_raw,
            step_raw: (max_raw / 100).max(1), // ~1% per step
        }
    }

    pub fn set_target_pct(&mut self, pct: u8) {
        let clamped = pct.clamp(1, 100);
        self.target_raw = (clamped as i64 * self.max_raw + 50) / 100;
        let diff = (self.target_raw - self.current_raw).abs();
        self.step_raw = (diff / 10).max(self.max_raw / 200).max(1);
    }

    /// Ticks the fader towards the target raw brightness. Returns true if value changed.
    pub fn tick(&mut self) -> (i64, bool) {
        if self.current_raw == self.target_raw {
            return (self.current_raw, false);
        }

        if self.current_raw < self.target_raw {
            self.current_raw = (self.current_raw + self.step_raw).min(self.target_raw);
        } else {
            self.current_raw = (self.current_raw - self.step_raw).max(self.target_raw);
        }

        (self.current_raw, true)
    }
}

/// Learning tracker for internal panel manual adjustments.
#[allow(dead_code)]
pub struct LearningTracker {
    pub last_manual_pct: u8,
    pub manual_detected_time: Option<Instant>,
}

#[allow(dead_code)]
impl LearningTracker {
    pub fn new() -> Self {
        Self {
            last_manual_pct: 0,
            manual_detected_time: None,
        }
    }
}

/// Determines if hardware brightness was overridden manually based on the difference
/// between the current hardware raw/pct and what lbright last applied.
pub fn is_manual_override_detected(
    hw_raw: i64,
    hw_pct: u8,
    last_applied_raw: i64,
    max_raw: i64,
) -> bool {
    if max_raw <= 0 {
        return false;
    }
    let last_applied_pct = ((last_applied_raw * 100 + max_raw / 2) / max_raw).clamp(0, 100) as u8;
    let pct_diff = (hw_pct as i32 - last_applied_pct as i32).abs();
    let raw_diff = (hw_raw - last_applied_raw).abs();
    let threshold_raw = (max_raw / 50).max(2);

    pct_diff >= 2 || raw_diff >= threshold_raw
}

pub fn get_current_minute_of_day() -> u16 {
    // Current local time minutes from midnight
    let now = chrono_or_std_now_minutes();
    now
}

fn chrono_or_std_now_minutes() -> u16 {
    // Standard library calculation using /etc/localtime or local time offset via libc/time
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Use libc localtime_r if available or standard calculation
    let mut tm = [0i32; 16];
    unsafe {
        let time_val = now_secs as LibcTimeT;
        localtime_r_compat(&time_val, tm.as_mut_ptr());
    }
    let hour = tm[2] as u16; // tm_hour
    let min = tm[1] as u16;  // tm_min
    hour * 60 + min
}

type LibcTimeT = i64;

unsafe fn localtime_r_compat(time_p: *const LibcTimeT, result: *mut i32) {
    #[cfg(target_os = "linux")]
    extern "C" {
        fn localtime_r(time_p: *const LibcTimeT, result: *mut i32) -> *mut i32;
    }
    #[cfg(target_os = "linux")]
    {
        localtime_r(time_p, result);
    }
}

/// Runs the background adaptive daemon.
pub fn run_daemon(foreground: bool) {
    if !foreground {
        eprintln!("Running lbright daemon in background loop...");
    } else {
        println!("Starting lbright daemon (foreground mode)...");
    }

    let config_path = get_config_path();
    let state_dir = get_state_dir();
    let _ = fs::create_dir_all(&state_dir);

    // On start or restart, clear any manual override from previous daemon session
    if clear_manual_override(&state_dir) {
        eprintln!("[lbright] Daemon started/restarted: cleared manual override, resuming auto brightness.");
    }

    // If config file is missing or deleted, recreate it with default values
    if !config_path.exists() {
        eprintln!(
            "[lbright] Configuration missing at '{}'. Recreating default config...",
            config_path.display()
        );
        let default_config = Config::default();
        if let Err(e) = save_config(&config_path, &default_config) {
            eprintln!("[lbright] Failed to recreate config file: {}", e);
        }
    }

    let (mut config, warnings) = load_config(&config_path);
    for w in warnings {
        eprintln!("[lbright] Config warning: {}", w);
    }

    let mut last_config_mtime = fs::metadata(&config_path).ok().and_then(|m| m.modified().ok());
    let panel = InternalPanel::detect_primary();
    let als_sensor = AmbientSensor::detect();
    let mut ambient_tracker = AmbientTracker::new(config.general.ambient_average as usize);

    let ddc_worker = start_ddc_worker(
        Duration::from_secs(config.general.ddc_set_timeout_sec),
        Duration::from_secs(config.general.ddc_detect_timeout_sec),
        Duration::from_secs(config.general.ddc_cooldown_sec as u64),
    );

    let mut fader = panel.as_ref().map(|p| {
        let cur = p.get_raw().unwrap_or(p.max_raw / 2);
        SmoothFader::new(cur, p.max_raw)
    });

    let mut last_applied_raw = panel.as_ref().and_then(|p| p.get_raw().ok()).unwrap_or(0);
    let mut was_paused = read_pause_state(&state_dir).is_paused();

    let mut last_profile_calc = Instant::now() - Duration::from_secs(3600); // Trigger immediately
    let mut last_ambient_check = Instant::now();
    let mut last_config_check = Instant::now();
    let mut last_ddc_rescan = Instant::now();
    let mut last_tick_instant = Instant::now();
    let mut last_tick_system_time = SystemTime::now();

    loop {
        thread_sleep_tick(Duration::from_millis(100));

        // Detect system sleep/resume via monotonic clock gap or system time gap (> 5s jump)
        let mono_elapsed = last_tick_instant.elapsed();
        let sys_elapsed = SystemTime::now()
            .duration_since(last_tick_system_time)
            .unwrap_or_default();

        if mono_elapsed > Duration::from_secs(5) || sys_elapsed > Duration::from_secs(5) {
            let gap_sec = mono_elapsed.as_secs_f32().max(sys_elapsed.as_secs_f32());
            eprintln!(
                "[lbright] System resume detected (gap of {:.1}s). Triggering profile recalculation and DDC rescan...",
                gap_sec
            );
            if clear_manual_override(&state_dir) {
                eprintln!("[lbright] Resumed from standby: cleared manual override, resuming auto brightness.");
            }
            last_profile_calc = Instant::now() - Duration::from_secs(3600);
            let _ = ddc_worker.tx.send(DdcCommand::Rescan);
        }
        last_tick_instant = Instant::now();
        last_tick_system_time = SystemTime::now();

        // 1. Check pause state
        let mut pause_state = read_pause_state(&state_dir);
        let mut is_paused = pause_state.is_paused();

        // If transitioning from paused to active, trigger immediate profile recalculation
        if was_paused && !is_paused {
            eprintln!("[lbright] Auto brightness resumed. Recalculating profile target...");
            last_profile_calc = Instant::now() - Duration::from_secs(3600);
        }
        was_paused = is_paused;

        // 2. Hardware brightness check & manual override detection for internal panel
        if let Some(p) = &panel {
            if let (Ok(hw_raw), Ok(hw_pct)) = (p.get_raw(), p.get_percent()) {
                if hw_pct < 5 {
                    // Manual brightness reduced to < 5%: auto enable auto brightness!
                    if is_paused {
                        eprintln!(
                            "[lbright] Manual brightness reduced to {}% (< 5%). Auto-enabling auto brightness...",
                            hw_pct
                        );
                        let _ = set_pause_state(&state_dir, &PauseState::Active);
                        pause_state = PauseState::Active;
                        is_paused = false;
                        was_paused = false;
                        last_applied_raw = hw_raw;
                        if let Some(f) = fader.as_mut() {
                            f.current_raw = hw_raw;
                        }
                        last_profile_calc = Instant::now() - Duration::from_secs(3600);
                    } else if is_manual_override_detected(hw_raw, hw_pct, last_applied_raw, p.max_raw) {
                        eprintln!(
                            "[lbright] Manual brightness reduced to {}% (< 5%). Auto brightness enabled.",
                            hw_pct
                        );
                        last_applied_raw = hw_raw;
                        if let Some(f) = fader.as_mut() {
                            f.current_raw = hw_raw;
                        }
                        last_profile_calc = Instant::now() - Duration::from_secs(3600);
                    }
                } else if !is_paused {
                    // Hardware brightness >= 5% and active: check if hardware brightness was overridden manually
                    if is_manual_override_detected(hw_raw, hw_pct, last_applied_raw, p.max_raw) {
                        let last_applied_pct = if p.max_raw > 0 {
                            ((last_applied_raw * 100 + p.max_raw / 2) / p.max_raw).clamp(0, 100) as u8
                        } else {
                            0
                        };
                        eprintln!(
                            "[lbright] Manual brightness adjustment detected: hardware is at {}% (was {}%). Stopping auto brightness until standby or restart.",
                            hw_pct, last_applied_pct
                        );
                        let _ = set_pause_state(&state_dir, &PauseState::PausedManualOverride);
                        pause_state = PauseState::PausedManualOverride;
                        is_paused = true;
                        was_paused = true;
                        last_applied_raw = hw_raw;
                        if let Some(f) = fader.as_mut() {
                            f.current_raw = hw_raw;
                            f.target_raw = hw_raw;
                        }
                    }
                } else {
                    // While paused / stopped and >= 5%, keep fader and last_applied_raw in sync with hardware
                    last_applied_raw = hw_raw;
                    if let Some(f) = fader.as_mut() {
                        f.current_raw = hw_raw;
                        f.target_raw = hw_raw;
                    }
                }
            }
        }

        // 3. Advance internal smooth fade state machine (only when active!)
        if !is_paused {
            if let (Some(f), Some(p)) = (fader.as_mut(), panel.as_ref()) {
                if config.general.fade_internal {
                    let (raw, changed) = f.tick();
                    if changed {
                        let _ = p.set_raw(raw);
                        last_applied_raw = p.get_raw().unwrap_or(raw);
                    }
                } else if f.current_raw != f.target_raw {
                    f.current_raw = f.target_raw;
                    let _ = p.set_raw(f.target_raw);
                    last_applied_raw = p.get_raw().unwrap_or(f.target_raw);
                }
            }
        }

        // 4. Config check (every 1 second)
        if last_config_check.elapsed() >= Duration::from_secs(1) {
            last_config_check = Instant::now();
            if !config_path.exists() {
                eprintln!(
                    "[lbright] Configuration deleted at '{}'. Recreating default config...",
                    config_path.display()
                );
                let default_config = Config::default();
                let _ = save_config(&config_path, &default_config);
                config = default_config;
                ambient_tracker.set_capacity(config.general.ambient_average as usize);
                last_config_mtime = fs::metadata(&config_path).ok().and_then(|m| m.modified().ok());
                last_profile_calc = Instant::now() - Duration::from_secs(3600);
            } else {
                let cur_mtime = fs::metadata(&config_path).ok().and_then(|m| m.modified().ok());
                if cur_mtime != last_config_mtime {
                    last_config_mtime = cur_mtime;
                    let (reloaded_config, warnings) = load_config(&config_path);
                    for w in warnings {
                        eprintln!("[lbright] Config reload warning: {}", w);
                    }
                    config = reloaded_config;
                    ambient_tracker.set_capacity(config.general.ambient_average as usize);
                    eprintln!("[lbright] Configuration reloaded.");
                    // Trigger immediate profile recalculation
                    last_profile_calc = Instant::now() - Duration::from_secs(3600);
                }
            }
        }

        // 5. Ambient light sensor check (every ambient_poll_sec)
        if config.general.ambient && !is_paused && last_ambient_check.elapsed() >= Duration::from_secs(config.general.ambient_poll_sec as u64) {
            last_ambient_check = Instant::now();
            if let Some(sensor) = &als_sensor {
                if let Some(lux) = sensor.read_lux() {
                    let (_offset, changed) = ambient_tracker.evaluate_offset(lux, config.general.ambient_hysteresis_pct);
                    if changed {
                        // Ambient condition changed significantly, update targets
                        last_profile_calc = Instant::now() - Duration::from_secs(3600);
                    }
                }
            }
        }

        // 6. Profile evaluation (every interval_min)
        let interval_duration = Duration::from_secs((config.general.interval_min as u64 * 60).max(1));
        if !is_paused && last_profile_calc.elapsed() >= interval_duration {
            last_profile_calc = Instant::now();

            let cur_minutes = get_current_minute_of_day();
            let ambient_offset = if config.general.ambient {
                ambient_tracker.current_offset()
            } else {
                0
            };

            // Evaluate internal panel target
            if let Some(internal_dev) = config.get_internal() {
                if internal_dev.enabled {
                    let base_target = interpolate_linear(
                        &internal_dev.points,
                        cur_minutes,
                        config.general.min_internal,
                        config.general.max_internal,
                    );
                    let final_target = ((base_target as i32 + ambient_offset as i32)
                        .clamp(config.general.min_internal as i32, config.general.max_internal as i32))
                        as u8;

                    if let Some(f) = fader.as_mut() {
                        f.set_target_pct(final_target);
                    }
                    let _ = write_device_state(&state_dir, "internal", final_target);
                }
            }

            // Evaluate external DDC targets
            for dev in &config.devices {
                if let DeviceId::Ddc(ddc_id) = &dev.id {
                    if dev.enabled {
                        let base_target = interpolate_linear(
                            &dev.points,
                            cur_minutes,
                            config.general.min_external,
                            config.general.max_external,
                        );
                        let final_target = ((base_target as i32 + ambient_offset as i32)
                            .clamp(config.general.min_external as i32, config.general.max_external as i32))
                            as u8;

                        let _ = ddc_worker.tx.send(DdcCommand::Set {
                            id: ddc_id.clone(),
                            pct: final_target,
                        });
                        let _ = write_device_state(&state_dir, &format!("ddc_{}", ddc_id), final_target);
                    }
                }
            }
        }

        // 7. Periodic DDC display rescan (every ddc_rescan_min minutes)
        if last_ddc_rescan.elapsed() >= Duration::from_secs(config.general.ddc_rescan_min as u64 * 60) {
            last_ddc_rescan = Instant::now();
            let _ = ddc_worker.tx.send(DdcCommand::Rescan);
        }
    }
}

fn thread_sleep_tick(d: Duration) {
    std::thread::sleep(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manual_override_detection() {
        let max_raw = 96000;
        let last_applied_raw = 48000; // 50%
        let last_applied_pct = 50;

        // Exact match -> no override
        assert!(!is_manual_override_detected(
            last_applied_raw,
            last_applied_pct,
            last_applied_raw,
            max_raw
        ));

        // Minor 1-raw-unit driver quantization -> no false positive
        assert!(!is_manual_override_detected(
            last_applied_raw + 1,
            last_applied_pct,
            last_applied_raw,
            max_raw
        ));
        assert!(!is_manual_override_detected(
            last_applied_raw - 1,
            last_applied_pct,
            last_applied_raw,
            max_raw
        ));

        // Small panel (e.g. 255 max raw) 1 unit off -> no false positive
        assert!(!is_manual_override_detected(128, 50, 127, 255));

        // User manual adjustment +5% on 96000 panel -> detected!
        let new_raw = 52800; // 55%
        assert!(is_manual_override_detected(
            new_raw,
            55,
            last_applied_raw,
            max_raw
        ));

        // User manual adjustment down to 20% -> detected!
        let low_raw = 19200; // 20%
        assert!(is_manual_override_detected(
            low_raw,
            20,
            last_applied_raw,
            max_raw
        ));

        // User manual adjustment on 255 panel -> detected!
        assert!(is_manual_override_detected(204, 80, 128, 255));
    }

    #[test]
    fn test_manual_override_cleared_on_restart() {
        let tmp = std::env::temp_dir().join("lbright_test_daemon_restart");
        let _ = fs::remove_dir_all(&tmp);

        // Simulate manual override active
        set_pause_state(&tmp, &PauseState::PausedManualOverride).unwrap();
        assert_eq!(read_pause_state(&tmp), PauseState::PausedManualOverride);

        // When daemon starts/restarts, manual override is cleared
        assert!(clear_manual_override(&tmp));
        assert_eq!(read_pause_state(&tmp), PauseState::Active);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_manual_override_cleared_on_standby_resume() {
        let tmp = std::env::temp_dir().join("lbright_test_daemon_resume");
        let _ = fs::remove_dir_all(&tmp);

        set_pause_state(&tmp, &PauseState::PausedManualOverride).unwrap();
        assert!(read_pause_state(&tmp).is_paused());

        // System resume triggers clear_manual_override
        assert!(clear_manual_override(&tmp));
        assert_eq!(read_pause_state(&tmp), PauseState::Active);
        assert!(!read_pause_state(&tmp).is_paused());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_indefinite_pause_preserved_on_resume_and_restart() {
        let tmp = std::env::temp_dir().join("lbright_test_daemon_indefinite");
        let _ = fs::remove_dir_all(&tmp);

        // User explicitly set indefinite pause
        set_pause_state(&tmp, &PauseState::PausedIndefinite).unwrap();

        // Neither restart nor standby resume should clear an explicit indefinite pause
        assert!(!clear_manual_override(&tmp));
        assert_eq!(read_pause_state(&tmp), PauseState::PausedIndefinite);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_reduced_to_under_5_auto_enables() {
        let tmp = std::env::temp_dir().join("lbright_test_daemon_under5");
        let _ = fs::remove_dir_all(&tmp);

        // Scenario: Auto brightness was stopped/paused by manual override (e.g. at 50%)
        set_pause_state(&tmp, &PauseState::PausedManualOverride).unwrap();
        assert!(read_pause_state(&tmp).is_paused());

        // When brightness is reduced to < 5%, auto brightness is re-enabled
        let hw_pct: u8 = 3;
        if hw_pct < 5 {
            let _ = set_pause_state(&tmp, &PauseState::Active);
        }
        assert!(!read_pause_state(&tmp).is_paused());
        assert_eq!(read_pause_state(&tmp), PauseState::Active);

        let _ = fs::remove_dir_all(&tmp);
    }
}
