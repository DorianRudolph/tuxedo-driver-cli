// SPDX-License-Identifier: GPL-2.0-or-later

use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use serde::Deserialize;
use signal_hook::consts::{SIGINT, SIGTERM};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const DEFAULT_CONFIG: &str = "/etc/tuxedo-driver-daemon.toml";
const TUXEDO_IO: &str = "/dev/tuxedo_io";
const TUXEDO_KEYBOARD: &str = "/sys/devices/platform/tuxedo_keyboard";

const IOCTL_MAGIC: u8 = 0xec;
const MAGIC_READ_UW: u8 = IOCTL_MAGIC + 3;
const MAGIC_WRITE_UW: u8 = IOCTL_MAGIC + 4;
const NB02_FAN_SPEED_MAX: f64 = 0xc8 as f64;

const R_HWCHECK_UW: IoctlReq = IoctlReq::read(IOCTL_MAGIC, 0x06, ArgKind::Ptr);
const R_UW_FANSPEED: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x10, ArgKind::Ptr);
const R_UW_FANSPEED2: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x11, ArgKind::Ptr);
const R_UW_FAN_TEMP: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x12, ArgKind::Ptr);
const R_UW_FANS_OFF_AVAILABLE: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x16, ArgKind::Ptr);
const R_UW_FANS_MIN_SPEED: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x17, ArgKind::Ptr);

const W_UW_FANSPEED: IoctlReq = IoctlReq::write(MAGIC_WRITE_UW, 0x10, ArgKind::Ptr);
const W_UW_FANSPEED2: IoctlReq = IoctlReq::write(MAGIC_WRITE_UW, 0x11, ArgKind::Ptr);
const W_UW_FANAUTO: IoctlReq = IoctlReq::none(MAGIC_WRITE_UW, 0x14);
const W_UW_PERF_PROF: IoctlReq = IoctlReq::write(MAGIC_WRITE_UW, 0x18, ArgKind::Ptr);

#[derive(Parser)]
#[command(version, about = "Minimal TUXEDO driver fan/profile daemon")]
struct Args {
    #[arg(short, long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    #[arg(short, long, help = "Log every fan target write")]
    verbose: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    profile: Option<PerformanceProfile>,
    charging_profile: Option<String>,
    ac_auto_boot: Option<bool>,
    usb_powershare: Option<bool>,
    fn_lock: Option<bool>,
    #[serde(default)]
    fan: FanConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FanConfig {
    #[serde(default = "default_fan_enabled")]
    enabled: bool,
    #[serde(default = "default_interval_ms")]
    interval_ms: u64,
    #[serde(default = "default_hysteresis_c")]
    hysteresis_c: i32,
    #[serde(default = "default_preset")]
    preset: FanPreset,
    curve: Option<Vec<FanPoint>>,
}

impl Default for FanConfig {
    fn default() -> Self {
        Self {
            enabled: default_fan_enabled(),
            interval_ms: default_interval_ms(),
            hysteresis_c: default_hysteresis_c(),
            preset: default_preset(),
            curve: None,
        }
    }
}

fn default_fan_enabled() -> bool {
    false
}

fn default_interval_ms() -> u64 {
    1_000
}

fn default_hysteresis_c() -> i32 {
    5
}

fn default_preset() -> FanPreset {
    FanPreset::Silent
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PerformanceProfile {
    PowerSave,
    Enthusiast,
    Overboost,
}

impl PerformanceProfile {
    fn id(self) -> i32 {
        match self {
            Self::PowerSave => 1,
            Self::Enthusiast => 2,
            Self::Overboost => 3,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::PowerSave => "power_save",
            Self::Enthusiast => "enthusiast",
            Self::Overboost => "overboost",
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FanPreset {
    Silent,
    Quiet,
    Balanced,
    Cool,
    Freezy,
}

impl FanPreset {
    fn name(self) -> &'static str {
        match self {
            Self::Silent => "silent",
            Self::Quiet => "quiet",
            Self::Balanced => "balanced",
            Self::Cool => "cool",
            Self::Freezy => "freezy",
        }
    }

    fn curve(self) -> &'static [FanPoint] {
        match self {
            Self::Silent => &SILENT,
            Self::Quiet => &QUIET,
            Self::Balanced => &BALANCED,
            Self::Cool => &COOL,
            Self::Freezy => &FREEZY,
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize)]
struct FanPoint {
    temp: i32,
    speed: u8,
}

#[derive(Copy, Clone)]
enum ArgKind {
    None,
    Ptr,
}

#[derive(Copy, Clone)]
struct IoctlReq {
    dir: u8,
    ty: u8,
    nr: u8,
    arg: ArgKind,
}

impl IoctlReq {
    const fn none(ty: u8, nr: u8) -> Self {
        Self {
            dir: 0,
            ty,
            nr,
            arg: ArgKind::None,
        }
    }

    const fn read(ty: u8, nr: u8, arg: ArgKind) -> Self {
        Self {
            dir: 2,
            ty,
            nr,
            arg,
        }
    }

    const fn write(ty: u8, nr: u8, arg: ArgKind) -> Self {
        Self {
            dir: 1,
            ty,
            nr,
            arg,
        }
    }

    fn code(self) -> libc::c_ulong {
        const NRSHIFT: u32 = 0;
        const TYPESHIFT: u32 = 8;
        const SIZESHIFT: u32 = 16;
        const DIRSHIFT: u32 = 30;

        let size = match self.arg {
            ArgKind::None => 0,
            ArgKind::Ptr => std::mem::size_of::<*mut i32>(),
        } as libc::c_ulong;

        ((self.dir as libc::c_ulong) << DIRSHIFT)
            | ((self.ty as libc::c_ulong) << TYPESHIFT)
            | ((self.nr as libc::c_ulong) << NRSHIFT)
            | (size << SIZESHIFT)
    }
}

struct TuxedoIo {
    file: File,
}

impl TuxedoIo {
    fn open() -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(TUXEDO_IO)?;
        let io = Self { file };
        io.ensure_uniwill_available()?;
        Ok(io)
    }

    fn ensure_uniwill_available(&self) -> Result<()> {
        if self.read_int(R_HWCHECK_UW)? == 1 {
            Ok(())
        } else {
            Err("Uniwill interface is not available".into())
        }
    }

    fn read_int(&self, req: IoctlReq) -> Result<i32> {
        let mut value = 0_i32;
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), req.code(), &mut value) };
        if ret < 0 {
            return Err(Box::new(io::Error::last_os_error()));
        }
        Ok(value)
    }

    fn write_int(&self, req: IoctlReq, value: i32) -> Result<()> {
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), req.code(), &value) };
        if ret < 0 {
            return Err(Box::new(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn ioctl_none(&self, req: IoctlReq) -> Result<()> {
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), req.code()) };
        if ret < 0 {
            return Err(Box::new(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn set_fans_auto(&self) -> Result<()> {
        self.ioctl_none(W_UW_FANAUTO)
    }
}

struct FanAutoGuard<'a> {
    io: &'a TuxedoIo,
    enabled: bool,
}

impl Drop for FanAutoGuard<'_> {
    fn drop(&mut self) {
        if self.enabled {
            if let Err(err) = self.io.set_fans_auto() {
                eprintln!("failed to restore firmware fan auto mode: {err}");
            } else {
                eprintln!("restored firmware fan auto mode");
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_config(&args.config)?;
    wait_for_path(Path::new(TUXEDO_IO), Duration::from_secs(30))?;
    wait_for_path(Path::new(TUXEDO_KEYBOARD), Duration::from_secs(30))?;

    let io = TuxedoIo::open()?;
    apply_startup_settings(&io, &config)?;

    if !config.fan.enabled {
        eprintln!("fan control disabled; startup settings applied");
        return Ok(());
    }

    run_fan_loop(&io, &config.fan, args.verbose)
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        eprintln!("config {} not found; using defaults", path.display());
        return Ok(Config::default());
    }

    let raw = fs::read_to_string(path)?;
    Ok(toml::from_str(&raw)?)
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(format!("{} did not appear within {:?}", path.display(), timeout).into())
}

fn apply_startup_settings(io: &TuxedoIo, config: &Config) -> Result<()> {
    if let Some(profile) = config.profile {
        io.write_int(W_UW_PERF_PROF, profile.id())?;
        eprintln!("profile: set {}", profile.name());
    }

    if let Some(profile) = config.charging_profile.as_deref() {
        set_charging_profile(profile)?;
        eprintln!("charging_profile: set {profile}");
    }

    if let Some(value) = config.ac_auto_boot {
        write_bool(&sysfs("ac_auto_boot/ac_auto_boot"), value)?;
        eprintln!("ac_auto_boot: set {}", on_off(value));
    }

    if let Some(value) = config.usb_powershare {
        write_bool(&sysfs("usb_powershare/usb_powershare"), value)?;
        eprintln!("usb_powershare: set {}", on_off(value));
    }

    if let Some(value) = config.fn_lock {
        write_bool(&sysfs("fn_lock"), value)?;
        eprintln!("fn_lock: set {}", on_off(value));
    }

    Ok(())
}

fn set_charging_profile(profile: &str) -> Result<()> {
    let current = sysfs("charging_profile/charging_profile");
    let available = read_trimmed(&sysfs("charging_profile/charging_profiles_available"))?;
    if !available
        .split_whitespace()
        .any(|candidate| candidate == profile)
    {
        return Err(format!("{profile} is not in available charging profiles: {available}").into());
    }
    write_value(&current, profile)
}

fn run_fan_loop(io: &TuxedoIo, config: &FanConfig, verbose: bool) -> Result<()> {
    let terminate = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&terminate))?;
    signal_hook::flag::register(SIGINT, Arc::clone(&terminate))?;

    let fans = fan_count(io);
    if fans == 0 {
        return Err("no controllable fans detected".into());
    }

    let fans_off_available = io.read_int(R_UW_FANS_OFF_AVAILABLE)? == 1;
    let min_speed = io.read_int(R_UW_FANS_MIN_SPEED)?.clamp(0, 100) as u8;
    let curve = selected_curve(config);
    let interval = Duration::from_millis(config.interval_ms.max(500));
    let _guard = FanAutoGuard { io, enabled: true };

    eprintln!(
        "fan control started: fans={fans}, preset={}, interval={interval:?}, hysteresis={}C, min_speed={min_speed}%, fans_off_available={}",
        config.preset.name(),
        config.hysteresis_c,
        on_off(fans_off_available)
    );

    let mut previous_target = None;
    while !terminate.load(Ordering::Relaxed) {
        let temp = read_fan_temp_raw(io, 0)?;
        let mut target = target_speed_for_temp(temp, min_speed, fans_off_available, curve);
        if let Some(previous) = previous_target {
            if config.hysteresis_c > 0 && target < previous {
                let hysteresis_temp = temp.saturating_add(config.hysteresis_c);
                let hysteresis_target =
                    target_speed_for_temp(hysteresis_temp, min_speed, fans_off_available, curve);
                if hysteresis_target >= previous {
                    target = previous;
                }
            }
        }

        for fan in (0..fans).rev() {
            set_fan_percent(io, fan, target)?;
        }
        previous_target = Some(target);
        if verbose {
            eprintln!("fans: temp={temp}C set {target}%");
        }
        sleep_interruptible(interval, &terminate);
    }

    Ok(())
}

fn selected_curve(config: &FanConfig) -> &[FanPoint] {
    config.curve.as_deref().unwrap_or(config.preset.curve())
}

fn target_speed_for_temp(
    temp: i32,
    min_speed: u8,
    fans_off_available: bool,
    curve: &[FanPoint],
) -> u8 {
    let speed = curve_speed(curve, temp);
    apply_hw_limit(speed, min_speed, fans_off_available)
}

fn curve_speed(curve: &[FanPoint], temp: i32) -> u8 {
    curve
        .iter()
        .find(|point| temp <= point.temp)
        .or_else(|| curve.last())
        .map(|point| point.speed.min(100))
        .unwrap_or(0)
}

fn apply_hw_limit(speed: u8, min_speed: u8, fans_off_available: bool) -> u8 {
    if speed < min_speed {
        let half_min = min_speed / 2;
        if fans_off_available && speed < half_min {
            0
        } else if fans_off_available || speed >= half_min {
            min_speed
        } else {
            speed
        }
    } else {
        speed
    }
}

fn sleep_interruptible(duration: Duration, terminate: &AtomicBool) {
    let chunk = Duration::from_millis(250);
    let start = Instant::now();
    while start.elapsed() < duration && !terminate.load(Ordering::Relaxed) {
        thread::sleep(chunk.min(duration.saturating_sub(start.elapsed())));
    }
}

fn fan_count(io: &TuxedoIo) -> u8 {
    let mut fans = 0;
    if io.read_int(R_UW_FANSPEED).is_ok() {
        fans += 1;
    }
    if io.read_int(R_UW_FANSPEED2).is_ok() {
        fans += 1;
    }
    fans
}

fn read_fan_temp_raw(io: &TuxedoIo, fan: u8) -> Result<i32> {
    match fan {
        0 => io.read_int(R_UW_FAN_TEMP),
        _ => Err("fan index out of range".into()),
    }
}

fn set_fan_percent(io: &TuxedoIo, fan: u8, percent: u8) -> Result<()> {
    let raw = (NB02_FAN_SPEED_MAX * percent.min(100) as f64 / 100.0).round() as i32;
    let req = match fan {
        0 => W_UW_FANSPEED,
        1 => W_UW_FANSPEED2,
        _ => return Err("fan index out of range".into()),
    };
    io.write_int(req, raw)
}

fn sysfs(relative: &str) -> PathBuf {
    Path::new(TUXEDO_KEYBOARD).join(relative)
}

fn read_trimmed(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn write_bool(path: &Path, value: bool) -> Result<()> {
    write_value(path, if value { 1 } else { 0 })
}

fn write_value(path: &Path, value: impl std::fmt::Display) -> Result<()> {
    fs::write(path, format!("{value}\n"))?;
    Ok(())
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

const fn p(temp: i32, speed: u8) -> FanPoint {
    FanPoint { temp, speed }
}

const SILENT: [FanPoint; 18] = [
    p(60, 0),
    p(65, 20),
    p(69, 25),
    p(71, 30),
    p(73, 35),
    p(75, 40),
    p(77, 45),
    p(79, 50),
    p(81, 55),
    p(83, 60),
    p(85, 65),
    p(87, 70),
    p(88, 75),
    p(89, 80),
    p(90, 85),
    p(92, 90),
    p(94, 95),
    p(100, 100),
];

const QUIET: [FanPoint; 25] = [
    p(50, 0),
    p(60, 20),
    p(63, 22),
    p(64, 23),
    p(65, 24),
    p(67, 25),
    p(68, 28),
    p(69, 30),
    p(70, 33),
    p(71, 37),
    p(72, 40),
    p(73, 43),
    p(74, 44),
    p(75, 46),
    p(76, 48),
    p(78, 52),
    p(80, 55),
    p(82, 60),
    p(84, 65),
    p(86, 70),
    p(88, 80),
    p(90, 85),
    p(92, 90),
    p(94, 95),
    p(100, 100),
];

const BALANCED: [FanPoint; 26] = [
    p(45, 0),
    p(51, 20),
    p(53, 23),
    p(56, 26),
    p(59, 30),
    p(62, 33),
    p(64, 35),
    p(65, 38),
    p(66, 40),
    p(67, 42),
    p(68, 45),
    p(69, 47),
    p(71, 50),
    p(72, 52),
    p(74, 53),
    p(76, 57),
    p(78, 60),
    p(79, 63),
    p(81, 65),
    p(83, 70),
    p(85, 75),
    p(87, 80),
    p(88, 85),
    p(91, 90),
    p(94, 95),
    p(100, 100),
];

const COOL: [FanPoint; 26] = [
    p(39, 0),
    p(45, 20),
    p(50, 25),
    p(55, 30),
    p(56, 32),
    p(57, 33),
    p(58, 34),
    p(60, 35),
    p(61, 37),
    p(63, 40),
    p(64, 42),
    p(67, 45),
    p(68, 47),
    p(70, 50),
    p(71, 52),
    p(73, 55),
    p(74, 57),
    p(76, 60),
    p(78, 65),
    p(80, 70),
    p(82, 75),
    p(84, 80),
    p(86, 85),
    p(89, 90),
    p(94, 95),
    p(100, 100),
];

const FREEZY: [FanPoint; 17] = [
    p(29, 20),
    p(39, 25),
    p(45, 30),
    p(49, 35),
    p(55, 40),
    p(60, 45),
    p(65, 50),
    p(70, 55),
    p(75, 60),
    p(77, 65),
    p(79, 70),
    p(81, 75),
    p(83, 80),
    p(85, 85),
    p(89, 90),
    p(94, 95),
    p(100, 100),
];
