use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::raw::{c_int, c_ulong};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

// libc is always linked on Linux; declared here to keep zero crate dependencies.
extern "C" {
    fn prctl(option: c_int, ...) -> c_int;
}
const PR_SET_PDEATHSIG: c_int = 1;
const SIGTERM: c_int = 15;

/// Environment marker set on child processes spawned by lbright.
pub const CHILD_MARKER_ENV: &str = "LBRIGHT_CHILD";
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Resolves sysfs IIO root path (default: /sys/bus/iio/devices).
pub fn get_iio_root() -> PathBuf {
    if let Ok(root) = std::env::var("LBRIGHT_SYSFS_ROOT") {
        return PathBuf::from(root).join("bus/iio/devices");
    }
    PathBuf::from("/sys/bus/iio/devices")
}

#[derive(Debug, Clone)]
pub struct AmbientSensor {
    pub path: PathBuf,
    pub scale_path: Option<PathBuf>,
}

impl AmbientSensor {
    /// Detects available ambient light sensors in sysfs IIO devices.
    pub fn detect() -> Option<Self> {
        let base = get_iio_root();
        if let Ok(entries) = fs::read_dir(&base) {
            for entry in entries.flatten() {
                let dev_path = entry.path();
                for filename in &[
                    "in_illuminance_input",
                    "in_illuminance_raw",
                    "in_illuminance0_input",
                    "in_illuminance0_raw",
                ] {
                    let p = dev_path.join(filename);
                    if p.exists() {
                        let scale_candidate = if filename.contains("0") {
                            dev_path.join("in_illuminance0_scale")
                        } else {
                            dev_path.join("in_illuminance_scale")
                        };
                        let scale_path = if scale_candidate.exists() {
                            Some(scale_candidate)
                        } else {
                            None
                        };
                        return Some(Self { path: p, scale_path });
                    }
                }
            }
        }
        None
    }

    /// Reads illuminance lux value from sysfs, applying scale when available.
    pub fn read_lux(&self) -> Option<f64> {
        let content = fs::read_to_string(&self.path).ok()?;
        let raw = content.trim().parse::<f64>().ok()?;

        let scaled = if let Some(ref sp) = self.scale_path {
            if let Ok(scale_str) = fs::read_to_string(sp) {
                if let Ok(scale) = scale_str.trim().parse::<f64>() {
                    raw * scale
                } else {
                    raw
                }
            } else {
                raw
            }
        } else {
            raw
        };

        // Keep sub-lux precision: rounding here turned dim readings (<0.5 lux) into 0.
        Some(scaled)
    }
}

/// Parses a lux value from a line of `monitor-sensor --light` output.
///
/// Recognised lines:
///   `=== Has ambient light sensor (value: 13.400001, unit: lux)`
///   `    Light changed: 15.272001 (lux)`
/// Values in non-lux units (e.g. some sensors report a percentage) are ignored.
pub fn parse_monitor_sensor_line(line: &str) -> Option<f64> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("=== Has ambient light sensor (value:") {
        let (value, unit) = rest.split_once(',')?;
        if !unit.contains("lux") {
            return None;
        }
        return value.trim().parse::<f64>().ok();
    }
    if let Some(rest) = line.strip_prefix("Light changed:") {
        let rest = rest.trim();
        if !rest.ends_with("(lux)") {
            return None;
        }
        return rest.split_whitespace().next()?.parse::<f64>().ok();
    }
    None
}

/// Reads ambient light from iio-sensor-proxy (the service GNOME/KDE use) by keeping a
/// `monitor-sensor --light` child process running. The child holds the D-Bus light claim,
/// so the proxy keeps the sensor powered and streaming. On some Intel ISH sensors the
/// sysfs `in_illuminance_raw` value can freeze while the proxy keeps updating.
pub struct ProxyLightMonitor {
    child: Child,
    latest: Arc<Mutex<Option<f64>>>,
    alive: Arc<Mutex<bool>>,
}

impl ProxyLightMonitor {
    /// Spawns `monitor-sensor --light` (line-buffered via `stdbuf`). Returns None if it
    /// cannot be started (e.g. iio-sensor-proxy is not installed).
    pub fn spawn() -> Option<Self> {
        if std::env::var("LBRIGHT_SYSFS_ROOT").is_ok() || std::env::var("LBRIGHT_NO_SENSOR_PROXY").is_ok() {
            return None;
        }
        let mut cmd = Command::new("stdbuf");
        cmd.args(["-oL", "monitor-sensor", "--light"])
            // Marker so uninstall.sh can find any child we spawned (and nothing else).
            .env(CHILD_MARKER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: the closure only calls prctl(), which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                // Kill monitor-sensor if lbright dies for any reason (even SIGKILL),
                // so it is never left running as an orphan. Survives the exec.
                if prctl(PR_SET_PDEATHSIG, SIGTERM as c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().ok()?;
        let stdout = child.stdout.take()?;
        let latest = Arc::new(Mutex::new(None));
        let alive = Arc::new(Mutex::new(true));
        let (latest_t, alive_t) = (Arc::clone(&latest), Arc::clone(&alive));
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.contains("No ambient light sensor") {
                    *latest_t.lock().unwrap() = None;
                } else if let Some(lux) = parse_monitor_sensor_line(&line) {
                    *latest_t.lock().unwrap() = Some(lux);
                }
            }
            // EOF: monitor-sensor exited (missing binary, proxy crash, etc.)
            *alive_t.lock().unwrap() = false;
            *latest_t.lock().unwrap() = None;
        });
        Some(Self { child, latest, alive })
    }

    pub fn latest(&self) -> Option<f64> {
        *self.latest.lock().unwrap()
    }

    pub fn is_alive(&self) -> bool {
        *self.alive.lock().unwrap()
    }

    /// Waits up to `timeout` for the first reading (used by one-shot `lbright status`).
    pub fn wait_for_reading(&self, timeout: Duration) -> Option<f64> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(v) = self.latest() {
                return Some(v);
            }
            if !self.is_alive() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.latest()
    }
}

impl Drop for ProxyLightMonitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Name of the root oneshot unit (installed by the .deb or `install.sh`)
/// that unbinds/rebinds the hid_sensor_als driver.
pub const ALS_RESET_UNIT: &str = "lbright-als-reset.service";

/// True when the sysfs and iio-sensor-proxy readings disagree enough that one of them
/// (in practice sysfs `in_illuminance_raw`) is likely frozen. When healthy, both come
/// from the same sensor and match closely.
pub fn readings_disagree(proxy: f64, sysfs: f64) -> bool {
    let (lo, hi) = if proxy < sysfs { (proxy, sysfs) } else { (sysfs, proxy) };
    (hi - lo) > 5.0 && hi / lo.max(0.5) > 2.0
}

/// Detects a stuck sensor: readings must disagree continuously for `hold`.
/// Brief disagreements (e.g. the proxy lagging a light change) reset the streak.
#[derive(Debug, Clone)]
pub struct StuckDetector {
    hold: Duration,
    disagree_since: Option<Instant>,
}

impl StuckDetector {
    pub fn new(hold: Duration) -> Self {
        Self { hold, disagree_since: None }
    }

    pub fn observe(&mut self, proxy: Option<f64>, sysfs: Option<f64>, now: Instant) -> bool {
        match (proxy, sysfs) {
            (Some(p), Some(s)) if readings_disagree(p, s) => {
                let since = *self.disagree_since.get_or_insert(now);
                now.duration_since(since) >= self.hold
            }
            _ => {
                self.disagree_since = None;
                false
            }
        }
    }

    pub fn clear(&mut self) {
        self.disagree_since = None;
    }
}

/// Builds the command that asks systemd (authorised via the lbright polkit rule)
/// to run the root ALS reset unit. Never prompts for a password.
pub fn als_reset_command() -> Command {
    let mut cmd = Command::new("systemctl");
    cmd.args(["start", "--no-ask-password", ALS_RESET_UNIT]);
    cmd
}

/// Runs the ALS reset in the background and enforces a cooldown between attempts.
struct AlsResetter {
    child: Option<Child>,
    last_attempt: Option<Instant>,
    unavailable: bool,
}

const RESET_COOLDOWN: Duration = Duration::from_secs(30 * 60);
const STUCK_HOLD: Duration = Duration::from_secs(3 * 60);

impl AlsResetter {
    fn new() -> Self {
        Self { child: None, last_attempt: None, unavailable: false }
    }

    fn can_start(&self) -> bool {
        !self.unavailable
            && self.child.is_none()
            && self.last_attempt.is_none_or(|t| t.elapsed() >= RESET_COOLDOWN)
    }

    fn start(&mut self) {
        self.last_attempt = Some(Instant::now());
        match als_reset_command().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn() {
            Ok(child) => self.child = Some(child),
            Err(e) => {
                eprintln!("[lbright] Could not run systemctl for sensor reset: {}", e);
                self.unavailable = true;
            }
        }
    }

    /// Returns Some(true) once a reset finishes successfully, Some(false) on failure.
    fn poll(&mut self) -> Option<bool> {
        let child = self.child.as_mut()?;
        let status = child.try_wait().ok()??;
        let child = self.child.take()?;
        let output = child.wait_with_output().ok();
        if status.success() {
            return Some(true);
        }
        let err = output
            .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
            .unwrap_or_default();
        eprintln!("[lbright] Ambient sensor reset failed: {}", err);
        // Missing unit or no polkit permission won't fix itself: stop trying until restart.
        if err.contains("not found") || err.contains("authentication") || err.contains("Access denied") {
            eprintln!(
                "[lbright] Automatic sensor reset disabled. Install the lbright .deb (or run 'install.sh') to enable it."
            );
            self.unavailable = true;
        }
        Some(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightSourceKind {
    SensorProxy,
    Sysfs,
}

/// Self-healing ambient light source used by the daemon.
///
/// Prefers iio-sensor-proxy and falls back to sysfs. Heals by:
/// - respawning `monitor-sensor` if it exits (throttled),
/// - re-detecting the sysfs sensor when a read fails (IIO device numbers can change,
///   e.g. the ALS moved from iio:device3 to iio:device0 across a reboot),
/// - restarting both on system resume, when sensor-hub glitches are most likely.
pub struct AmbientSource {
    proxy: Option<ProxyLightMonitor>,
    sysfs: Option<AmbientSensor>,
    last_proxy_attempt: Option<Instant>,
    last_sysfs_detect: Instant,
    last_kind: Option<LightSourceKind>,
    stuck: StuckDetector,
    resetter: AlsResetter,
}

const PROXY_RETRY: Duration = Duration::from_secs(60);
const SYSFS_REDETECT: Duration = Duration::from_secs(30);

impl AmbientSource {
    pub fn new() -> Self {
        let mut s = Self {
            proxy: None,
            sysfs: AmbientSensor::detect(),
            last_proxy_attempt: None,
            last_sysfs_detect: Instant::now(),
            last_kind: None,
            stuck: StuckDetector::new(STUCK_HOLD),
            resetter: AlsResetter::new(),
        };
        s.ensure_proxy();
        s
    }

    fn ensure_proxy(&mut self) {
        if self.proxy.as_ref().is_some_and(|p| p.is_alive()) {
            return;
        }
        // Drop a dead monitor immediately so the exited child is reaped (no zombie).
        self.proxy = None;
        if self.last_proxy_attempt.is_some_and(|t| t.elapsed() < PROXY_RETRY) {
            return;
        }
        self.last_proxy_attempt = Some(Instant::now());
        self.proxy = ProxyLightMonitor::spawn();
    }

    fn redetect_sysfs(&mut self) {
        self.last_sysfs_detect = Instant::now();
        let new = AmbientSensor::detect();
        let old_path = self.sysfs.as_ref().map(|s| s.path.clone());
        let new_path = new.as_ref().map(|s| s.path.clone());
        if old_path != new_path {
            match &new_path {
                Some(p) => eprintln!("[lbright] Ambient sensor (re)detected at {}", p.display()),
                None => eprintln!("[lbright] Ambient sensor no longer found in sysfs."),
            }
        }
        self.sysfs = new;
    }

    /// Restart all light sources (call after system resume).
    pub fn reset(&mut self) {
        self.stuck.clear();
        self.proxy = None;
        self.last_proxy_attempt = None;
        self.ensure_proxy();
        self.redetect_sysfs();
    }

    pub fn read_lux(&mut self) -> Option<(f64, LightSourceKind)> {
        self.ensure_proxy();
        self.check_reset_progress();

        let proxy_lux = self.proxy.as_ref().and_then(|p| p.latest());
        let mut sysfs_lux = self.sysfs.as_ref().and_then(|s| s.read_lux());
        if sysfs_lux.is_none() && self.last_sysfs_detect.elapsed() >= SYSFS_REDETECT {
            self.redetect_sysfs();
            sysfs_lux = self.sysfs.as_ref().and_then(|s| s.read_lux());
        }

        if self.stuck.observe(proxy_lux, sysfs_lux, Instant::now()) && self.resetter.can_start() {
            eprintln!(
                "[lbright] Ambient sensor looks stuck (iio-sensor-proxy {:.2} lux vs sysfs {:.2} lux for {}s). Resetting sensor driver...",
                proxy_lux.unwrap_or(0.0),
                sysfs_lux.unwrap_or(0.0),
                STUCK_HOLD.as_secs()
            );
            self.stuck.clear();
            self.resetter.start();
        }

        let result = match (proxy_lux, sysfs_lux) {
            (Some(v), _) => Some((v, LightSourceKind::SensorProxy)),
            (None, Some(v)) => Some((v, LightSourceKind::Sysfs)),
            (None, None) => None,
        };
        if let Some((_, kind)) = result {
            if self.last_kind != Some(kind) {
                eprintln!(
                    "[lbright] Ambient light source: {}",
                    match kind {
                        LightSourceKind::SensorProxy => "iio-sensor-proxy",
                        LightSourceKind::Sysfs => "sysfs IIO",
                    }
                );
                self.last_kind = Some(kind);
            }
        }
        result
    }

    fn check_reset_progress(&mut self) {
        if let Some(ok) = self.resetter.poll() {
            if ok {
                eprintln!("[lbright] Ambient sensor driver reset complete. Re-detecting sensor...");
            }
            // Either way, re-open both sources: device numbers change after a rebind.
            self.reset();
        }
    }
}

/// Calculates brightness offset percentage from lux.
pub fn calc_ambient_offset(lux: f64) -> i8 {
    if lux < 10.0 {
        -10
    } else if lux < 50.0 {
        -5
    } else if lux < 150.0 {
        0
    } else if lux > 5000.0 {
        75
    } else if lux > 2000.0 {
        55
    } else if lux > 800.0 {
        35
    } else if lux > 300.0 {
        15
    } else {
        5
    }
}

/// Moving average and hysteresis buffer for ambient light readings.
#[derive(Debug, Clone)]
pub struct AmbientTracker {
    buffer: VecDeque<f64>,
    capacity: usize,
    last_applied_offset: i8,
}

impl AmbientTracker {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            last_applied_offset: 0,
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.buffer.len() > self.capacity {
            self.buffer.pop_front();
        }
    }

    pub fn push_sample(&mut self, lux: f64) -> f64 {
        if self.buffer.len() >= self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(lux);
        self.average()
    }

    pub fn average(&self) -> f64 {
        if self.buffer.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.buffer.iter().sum();
        sum / self.buffer.len() as f64
    }

    /// Evaluates whether the offset has changed enough to exceed the hysteresis threshold.
    pub fn evaluate_offset(&mut self, lux: f64, hysteresis_pct: u32) -> (i8, bool) {
        let avg_lux = self.push_sample(lux);
        let target_offset = calc_ambient_offset(avg_lux);
        let diff = (target_offset as i32 - self.last_applied_offset as i32).abs();

        if diff >= hysteresis_pct as i32 {
            self.last_applied_offset = target_offset;
            (target_offset, true)
        } else {
            (self.last_applied_offset, false)
        }
    }

    pub fn current_offset(&self) -> i8 {
        self.last_applied_offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ambient_offset() {
        assert_eq!(calc_ambient_offset(5.0), -10);
        assert_eq!(calc_ambient_offset(30.0), -5);
        assert_eq!(calc_ambient_offset(100.0), 0);
        assert_eq!(calc_ambient_offset(200.0), 5);
        assert_eq!(calc_ambient_offset(500.0), 15);
        assert_eq!(calc_ambient_offset(1500.0), 35);
        assert_eq!(calc_ambient_offset(3000.0), 55);
        assert_eq!(calc_ambient_offset(9000.0), 75);
    }

    #[test]
    fn test_read_lux_keeps_sub_lux_precision() {
        let dir = std::env::temp_dir().join(format!("lbright_als_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("in_illuminance_raw");
        let scale = dir.join("in_illuminance_scale");
        fs::write(&raw, "429\n").unwrap();
        fs::write(&scale, "0.001000000\n").unwrap();
        let sensor = AmbientSensor { path: raw, scale_path: Some(scale) };
        let lux = sensor.read_lux().unwrap();
        assert!((lux - 0.429).abs() < 1e-9, "got {lux}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_parse_monitor_sensor_line() {
        assert_eq!(
            parse_monitor_sensor_line("=== Has ambient light sensor (value: 13.400001, unit: lux)"),
            Some(13.400001)
        );
        assert_eq!(parse_monitor_sensor_line("    Light changed: 15.272001 (lux)"), Some(15.272001));
        assert_eq!(parse_monitor_sensor_line("    Light changed: 42.000000 (%)"), None);
        assert_eq!(
            parse_monitor_sensor_line("=== Has ambient light sensor (value: 42.000000, unit: vendor)"),
            None
        );
        assert_eq!(parse_monitor_sensor_line("+++ iio-sensor-proxy appeared"), None);
        assert_eq!(parse_monitor_sensor_line("=== No ambient light sensor"), None);
    }

    #[test]
    fn test_readings_disagree() {
        assert!(!readings_disagree(14.79, 14.79));
        assert!(!readings_disagree(0.2, 3.0)); // dim-room noise: absolute diff too small
        assert!(!readings_disagree(400.0, 600.0)); // normal lag during a light change
        assert!(readings_disagree(8.8, 0.43)); // observed stuck case on Latitude 7320
        assert!(readings_disagree(0.43, 900.0));
    }

    #[test]
    fn test_stuck_detector_requires_sustained_disagreement() {
        let hold = Duration::from_secs(180);
        let mut d = StuckDetector::new(hold);
        let t0 = Instant::now();
        assert!(!d.observe(Some(8.8), Some(0.43), t0));
        assert!(!d.observe(Some(8.8), Some(0.43), t0 + Duration::from_secs(100)));
        // A single agreeing sample resets the streak
        assert!(!d.observe(Some(8.8), Some(8.8), t0 + Duration::from_secs(120)));
        assert!(!d.observe(Some(8.8), Some(0.43), t0 + Duration::from_secs(130)));
        assert!(!d.observe(Some(8.8), Some(0.43), t0 + Duration::from_secs(300)));
        assert!(d.observe(Some(8.8), Some(0.43), t0 + Duration::from_secs(310)));
        // Missing source never counts as stuck
        let mut d = StuckDetector::new(hold);
        assert!(!d.observe(None, Some(0.43), t0));
        assert!(!d.observe(None, Some(0.43), t0 + Duration::from_secs(1000)));
    }

    #[test]
    fn test_tracker_hysteresis() {
        let mut tracker = AmbientTracker::new(3);

        // Step into bright light (lux 500 -> offset +15)
        let (offset, changed) = tracker.evaluate_offset(500.0, 3);
        assert_eq!(offset, 15);
        assert!(changed);

        // Minor change (lux 520 -> offset still +15, diff 0 < 3)
        let (offset, changed) = tracker.evaluate_offset(520.0, 3);
        assert_eq!(offset, 15);
        assert!(!changed);
    }
}
