use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const TUXEDO_IO: &str = "/dev/tuxedo_io";
const TUXEDO_KEYBOARD: &str = "/sys/devices/platform/tuxedo_keyboard";

const IOCTL_MAGIC: u8 = 0xec;
const MAGIC_READ_UW: u8 = IOCTL_MAGIC + 3;
const MAGIC_WRITE_UW: u8 = IOCTL_MAGIC + 4;
const NB02_FAN_SPEED_MAX: f64 = 0xc8 as f64;

const R_MOD_VERSION: IoctlReq = IoctlReq::read(IOCTL_MAGIC, 0x00, ArgKind::Ptr);
const R_HWCHECK_UW: IoctlReq = IoctlReq::read(IOCTL_MAGIC, 0x06, ArgKind::Ptr);
const R_UW_HW_IF_STR: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x00, ArgKind::Ptr);
const R_UW_MODEL_ID: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x01, ArgKind::Ptr);
const R_UW_FANSPEED: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x10, ArgKind::Ptr);
const R_UW_FANSPEED2: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x11, ArgKind::Ptr);
const R_UW_FAN_TEMP: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x12, ArgKind::Ptr);
const R_UW_FAN_TEMP2: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x13, ArgKind::Ptr);
const R_UW_MODE: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x14, ArgKind::Ptr);
const R_UW_MODE_ENABLE: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x15, ArgKind::Ptr);
const R_UW_FANS_OFF_AVAILABLE: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x16, ArgKind::Ptr);
const R_UW_FANS_MIN_SPEED: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x17, ArgKind::Ptr);
const R_UW_PROFS_AVAILABLE: IoctlReq = IoctlReq::read(MAGIC_READ_UW, 0x21, ArgKind::Ptr);

const W_UW_FANSPEED: IoctlReq = IoctlReq::write(MAGIC_WRITE_UW, 0x10, ArgKind::Ptr);
const W_UW_FANSPEED2: IoctlReq = IoctlReq::write(MAGIC_WRITE_UW, 0x11, ArgKind::Ptr);
const W_UW_FANAUTO: IoctlReq = IoctlReq::none(MAGIC_WRITE_UW, 0x14);
const W_UW_PERF_PROF: IoctlReq = IoctlReq::write(MAGIC_WRITE_UW, 0x18, ArgKind::Ptr);

#[derive(Parser)]
#[command(
    version,
    about = "Control supported TUXEDO driver features on this XMG/Uniwill laptop"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a compact overview of all supported controls and current values.
    Status,
    /// Print driver/interface information from /dev/tuxedo_io.
    Info,
    /// Read or write direct fan controls through /dev/tuxedo_io.
    Fans {
        #[command(subcommand)]
        command: FanCommand,
    },
    /// Read or write ODM performance profile: power_save, enthusiast, overboost.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Read or write battery charging profile sysfs control.
    Charging {
        #[command(subcommand)]
        command: ChargingCommand,
    },
    /// Read or write monochrome keyboard backlight brightness.
    Backlight {
        #[command(subcommand)]
        command: BacklightCommand,
    },
    /// Read or write Fn lock.
    FnLock {
        #[command(subcommand)]
        command: BoolCommand,
    },
    /// Read or write boot-on-AC attach behavior.
    AcAutoBoot {
        #[command(subcommand)]
        command: BoolCommand,
    },
    /// Read or write USB powershare behavior.
    UsbPowershare {
        #[command(subcommand)]
        command: BoolCommand,
    },
}

#[derive(Subcommand)]
enum FanCommand {
    /// Show fan count, speeds, temperatures, and min-speed metadata.
    Status,
    /// Set one fan to a fixed percentage. Fan numbers are 1-based.
    Set { fan: u8, percent: u8 },
    /// Return both fans to firmware automatic mode.
    Auto,
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// List available ODM performance profiles.
    List,
    /// Show current raw/mapped ODM performance profile.
    Get,
    /// Set ODM performance profile.
    Set { profile: PerformanceProfile },
}

#[derive(Subcommand)]
enum ChargingCommand {
    /// List available charging profiles.
    List,
    /// Show current charging profile.
    Get,
    /// Set charging profile.
    Set { profile: String },
}

#[derive(Subcommand)]
enum BacklightCommand {
    /// Show current and maximum keyboard backlight brightness.
    Get,
    /// Set keyboard backlight brightness.
    Set { brightness: u32 },
}

#[derive(Subcommand)]
enum BoolCommand {
    /// Show current value.
    Get,
    /// Set current value.
    Set { value: OnOff },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl From<OnOff> for bool {
    fn from(value: OnOff) -> Self {
        matches!(value, OnOff::On)
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum PerformanceProfile {
    #[value(name = "power_save", alias = "power-save")]
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

    fn from_count(count: i32) -> Vec<Self> {
        match count {
            2 => vec![Self::PowerSave, Self::Enthusiast],
            3 => vec![Self::PowerSave, Self::Enthusiast, Self::Overboost],
            _ => Vec::new(),
        }
    }
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

#[derive(Debug)]
struct MissingPath {
    path: PathBuf,
}

impl fmt::Display for MissingPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not available", self.path.display())
    }
}

impl Error for MissingPath {}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Status => print_status(),
        Command::Info => print_info(),
        Command::Fans { command } => run_fans(command),
        Command::Profile { command } => run_profile(command),
        Command::Charging { command } => run_charging(command),
        Command::Backlight { command } => run_backlight(command),
        Command::FnLock { command } => run_bool(command, &sysfs("fn_lock")),
        Command::AcAutoBoot { command } => run_bool(command, &sysfs("ac_auto_boot/ac_auto_boot")),
        Command::UsbPowershare { command } => {
            run_bool(command, &sysfs("usb_powershare/usb_powershare"))
        }
    }
}

fn print_status() -> Result<()> {
    print_info()?;
    println!();
    run_fans(FanCommand::Status)?;
    println!();
    run_profile(ProfileCommand::Get)?;
    run_profile(ProfileCommand::List)?;
    println!();
    run_charging(ChargingCommand::Get)?;
    run_charging(ChargingCommand::List)?;
    println!();
    run_backlight(BacklightCommand::Get)?;
    println!("fn_lock: {}", bool_word(read_bool(&sysfs("fn_lock"))?));
    println!(
        "ac_auto_boot: {}",
        bool_word(read_bool(&sysfs("ac_auto_boot/ac_auto_boot"))?)
    );
    println!(
        "usb_powershare: {}",
        bool_word(read_bool(&sysfs("usb_powershare/usb_powershare"))?)
    );
    Ok(())
}

fn print_info() -> Result<()> {
    let io = TuxedoIo::open()?;
    println!("module_version: {}", io.read_string(R_MOD_VERSION)?);
    println!("uniwill_available: {}", io.read_int(R_HWCHECK_UW)? == 1);
    println!("interface: {}", io.read_string(R_UW_HW_IF_STR)?);
    println!("model_id: {}", io.read_int(R_UW_MODEL_ID)?);
    Ok(())
}

fn run_fans(command: FanCommand) -> Result<()> {
    let io = TuxedoIo::open()?;
    io.ensure_uniwill_available()?;
    match command {
        FanCommand::Status => {
            let fans = fan_count(&io);
            println!("fans: {fans}");
            println!(
                "fan_min_speed_percent: {}",
                io.read_int(R_UW_FANS_MIN_SPEED)?
            );
            println!(
                "fans_off_available: {}",
                bool_word(io.read_int(R_UW_FANS_OFF_AVAILABLE)? == 1)
            );
            println!("manual_mode_register: {}", io.read_int(R_UW_MODE_ENABLE)?);
            for idx in 0..fans {
                let raw_speed = read_fan_speed_raw(&io, idx)?;
                let speed = raw_to_percent(raw_speed);
                let raw_temp = read_fan_temp_raw(&io, idx).unwrap_or(0);
                let effective_temp = if idx == 1 && raw_temp <= 0 {
                    read_fan_temp_raw(&io, 0).unwrap_or(0)
                } else {
                    raw_temp
                };
                println!(
                    "fan{}: speed={}%, raw_speed={}, temp={}C, raw_temp={}C",
                    idx + 1,
                    speed,
                    raw_speed,
                    effective_temp,
                    raw_temp
                );
            }
        }
        FanCommand::Set { fan, percent } => {
            if !(1..=2).contains(&fan) {
                return Err("fan must be 1 or 2".into());
            }
            if percent > 100 {
                return Err("percent must be 0..=100".into());
            }
            let raw = percent_to_raw(percent);
            let req = if fan == 1 {
                W_UW_FANSPEED
            } else {
                W_UW_FANSPEED2
            };
            io.write_int(req, raw)?;
            println!("fan{fan}: set {percent}% (raw {raw})");
        }
        FanCommand::Auto => {
            io.ioctl_none(W_UW_FANAUTO)?;
            println!("fans: automatic mode requested");
        }
    }
    Ok(())
}

fn run_profile(command: ProfileCommand) -> Result<()> {
    let io = TuxedoIo::open()?;
    io.ensure_uniwill_available()?;
    match command {
        ProfileCommand::List => {
            let count = io.read_int(R_UW_PROFS_AVAILABLE)?;
            let profiles = PerformanceProfile::from_count(count)
                .into_iter()
                .map(PerformanceProfile::name)
                .collect::<Vec<_>>();
            println!("available_profiles: {}", profiles.join(", "));
        }
        ProfileCommand::Get => {
            let raw = io.read_int(R_UW_MODE)?;
            println!("profile: {} (raw 0x{raw:02x})", profile_from_mode(raw));
        }
        ProfileCommand::Set { profile } => {
            io.write_int(W_UW_PERF_PROF, profile.id())?;
            println!("profile: set {}", profile.name());
        }
    }
    Ok(())
}

fn run_charging(command: ChargingCommand) -> Result<()> {
    let current = sysfs("charging_profile/charging_profile");
    let available = sysfs("charging_profile/charging_profiles_available");
    match command {
        ChargingCommand::List => {
            println!("available_charging_profiles: {}", read_trimmed(&available)?);
        }
        ChargingCommand::Get => {
            println!("charging_profile: {}", read_trimmed(&current)?);
        }
        ChargingCommand::Set { profile } => {
            let available_profiles = read_trimmed(&available)?;
            let requested = profile.trim();
            if !available_profiles
                .split_whitespace()
                .any(|p| p == requested)
            {
                return Err(format!(
                    "{requested} is not in available profiles: {available_profiles}"
                )
                .into());
            }
            write_value(&current, requested)?;
            println!("charging_profile: set {requested}");
        }
    }
    Ok(())
}

fn run_backlight(command: BacklightCommand) -> Result<()> {
    let brightness = sysfs("leds/white:kbd_backlight/brightness");
    let max_brightness = sysfs("leds/white:kbd_backlight/max_brightness");
    match command {
        BacklightCommand::Get => {
            println!(
                "backlight: {}/{}",
                read_trimmed(&brightness)?,
                read_trimmed(&max_brightness)?
            );
        }
        BacklightCommand::Set { brightness: value } => {
            let max = read_trimmed(&max_brightness)?.parse::<u32>()?;
            if value > max {
                return Err(format!("brightness must be 0..={max}").into());
            }
            write_value(&brightness, value)?;
            println!("backlight: set {value}/{max}");
        }
    }
    Ok(())
}

fn run_bool(command: BoolCommand, path: &Path) -> Result<()> {
    match command {
        BoolCommand::Get => {
            println!("{}: {}", label_for(path), bool_word(read_bool(path)?));
        }
        BoolCommand::Set { value } => {
            let enabled = bool::from(value);
            write_value(path, if enabled { 1 } else { 0 })?;
            println!("{}: set {}", label_for(path), bool_word(enabled));
        }
    }
    Ok(())
}

struct TuxedoIo {
    file: File,
}

impl TuxedoIo {
    fn open() -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(TUXEDO_IO)?;
        Ok(Self { file })
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

    fn read_string(&self, req: IoctlReq) -> Result<String> {
        let mut bytes = vec![0_u8; 64];
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), req.code(), bytes.as_mut_ptr()) };
        if ret < 0 {
            return Err(Box::new(io::Error::last_os_error()));
        }
        let nul = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..nul]).trim().to_owned())
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

fn read_fan_speed_raw(io: &TuxedoIo, fan: u8) -> Result<i32> {
    match fan {
        0 => io.read_int(R_UW_FANSPEED),
        1 => io.read_int(R_UW_FANSPEED2),
        _ => Err("fan index out of range".into()),
    }
}

fn read_fan_temp_raw(io: &TuxedoIo, fan: u8) -> Result<i32> {
    match fan {
        0 => io.read_int(R_UW_FAN_TEMP),
        1 => io.read_int(R_UW_FAN_TEMP2),
        _ => Err("fan index out of range".into()),
    }
}

fn raw_to_percent(raw: i32) -> u8 {
    ((raw as f64 * 100.0 / NB02_FAN_SPEED_MAX).round()).clamp(0.0, 100.0) as u8
}

fn percent_to_raw(percent: u8) -> i32 {
    (NB02_FAN_SPEED_MAX * percent as f64 / 100.0).round() as i32
}

fn profile_from_mode(raw: i32) -> &'static str {
    match raw & 0xb0 {
        0xa0 => "power_save",
        0x10 => "overboost",
        0x00 => "enthusiast",
        _ => "unknown",
    }
}

fn sysfs(relative: &str) -> PathBuf {
    Path::new(TUXEDO_KEYBOARD).join(relative)
}

fn read_trimmed(path: &Path) -> Result<String> {
    ensure_available(path)?;
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn read_bool(path: &Path) -> Result<bool> {
    Ok(read_trimmed(path)? == "1")
}

fn write_value(path: &Path, value: impl fmt::Display) -> Result<()> {
    ensure_available(path)?;
    fs::write(path, format!("{value}\n"))?;
    Ok(())
}

fn ensure_available(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(Box::new(MissingPath {
            path: path.to_owned(),
        }))
    }
}

fn bool_word(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn label_for(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("value")
        .to_owned()
}
