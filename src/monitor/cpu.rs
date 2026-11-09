//! Reads live CPU data from the Linux kernel.

use crate::{error, warning};
use cpu_monitor::CpuInstant;
use std::{
    fs::{read_dir, read_to_string, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::exit,
};

const MSR_AMD_RAPL_POWER_UNIT: u64 = 0xC001_0299;
const MSR_AMD_PKG_ENERGY_STATUS: u64 = 0xC001_029B;
const MSR_DEVICE: &str = "/dev/cpu/0/msr";

struct EnergySensor {
    path: PathBuf,
    max_uj: Option<u64>,
}

struct MsrSensor {
    /// Energy represented by one counter unit, in joules.
    energy_unit_j: f64,
}

struct PackagePowerSensor {
    /// hwmon file exposing the APU/package power in microwatts.
    path: PathBuf,
}

enum PowerSensor {
    Energy(EnergySensor),
    Msr(MsrSensor),
    Package(PackagePowerSensor),
}

pub struct Cpu {
    temp_sensor: Option<String>,
    power_sensor: Option<PowerSensor>,
}

impl Cpu {
    pub fn new() -> Self {
        Cpu {
            temp_sensor: find_temp_sensor(),
            power_sensor: find_power_sensor(),
        }
    }

    /// Human-readable name of the CPU power source in use.
    pub fn power_source(&self) -> &'static str {
        match &self.power_sensor {
            Some(PowerSensor::Energy(_)) => "hwmon energy",
            Some(PowerSensor::Msr(_)) => "AMD MSR (RAPL)",
            Some(PowerSensor::Package(_)) => "amdgpu PPT (APU package)",
            None => "none",
        }
    }

    /// Displays a warning message if temperature sensor is not initialized.
    pub fn warn_temp(&self) {
        if self.temp_sensor == None {
            warning!("No supported CPU temperature sensor was found");
            eprintln!("         CPU temperature will not be displayed, and alarm will be disabled.");
            eprintln!("         Supported kernel modules are: asusec, coretemp, k10temp, and zenpower.");
        }
    }

    /// Displays a warning message if no supported CPU power sensor is available.
    pub fn warn_rapl(&self) {
        if self.power_sensor.is_none() {
            warning!("No supported CPU power sensor was found");
            eprintln!("         CPU power consumption will not be displayed.");
            eprintln!("         Supported sources are: intel-rapl, amd_energy, zenergy,");
            eprintln!("         AMD MSR (RAPL), and amdgpu PPT on the AMD BC-250.");
        }
    }

    /// Reads the value of the CPU temperature sensor and calculates it to be `˚C` or `˚F`.
    pub fn get_temp(&self, fahrenheit: bool) -> u8 {
        if let Some(sensor) = &self.temp_sensor {
            // Read sensor data
            let data = read_to_string(sensor).unwrap_or_else(|_| {
                error!("Failed to get CPU temperature");
                exit(1);
            });
            // Calculate temperature
            let mut temp = data.trim_end().parse::<u32>().unwrap();
            if fahrenheit {
                temp = temp * 9 / 5 + 32000
            }
            return (temp as f32 / 1000.0).round() as u8;
        }

        0
    }

    /// Reads the energy consumption of the CPU in microjoules.
    pub fn read_energy(&self) -> u64 {
        match &self.power_sensor {
            Some(PowerSensor::Energy(sensor)) => {
                let data = read_to_string(&sensor.path).unwrap_or_else(|_| {
                    error!("Failed to get CPU power");
                    exit(1);
                });
                data.trim_end().parse::<u64>().unwrap()
            }
            Some(PowerSensor::Msr(_)) => {
                // The package energy counter is 32 bits wide.
                read_msr(MSR_AMD_PKG_ENERGY_STATUS).unwrap_or(0) & 0xFFFF_FFFF
            }
            Some(PowerSensor::Package(_)) | None => 0,
        }
    }

    /// Reads the energy consumption one more time and calculates the CPU power by using the inital energy and the delta time.
    ///
    /// Formula: `P[W] = ΔE[μJ] / (Δt[ms] * 1000)`
    pub fn get_power(&self, initial_energy: u64, delta_millisec: u64) -> u16 {
        let power = match &self.power_sensor {
            Some(PowerSensor::Energy(sensor)) => {
                let current_energy = self.read_energy();
                let delta_energy = if current_energy > initial_energy {
                    current_energy - initial_energy
                } else if let Some(max_uj) = sensor.max_uj {
                    // Offset the current measurement if a bounded counter wraps.
                    (max_uj + current_energy) - initial_energy
                } else {
                    // Unbounded hwmon counters only decrease when the sensor resets.
                    return 0;
                };
                delta_energy as f64 / (delta_millisec * 1000) as f64
            }
            Some(PowerSensor::Msr(sensor)) => {
                let seconds = delta_millisec as f64 / 1000.0;
                if seconds <= 0.0 {
                    return 0;
                }
                let current_energy = read_msr(MSR_AMD_PKG_ENERGY_STATUS).unwrap_or(initial_energy) & 0xFFFF_FFFF;
                let delta_energy = current_energy.wrapping_sub(initial_energy) & 0xFFFF_FFFF;
                delta_energy as f64 * sensor.energy_unit_j / seconds
            }
            Some(PowerSensor::Package(sensor)) => {
                // The amdgpu PPT file reports microwatts.
                read_to_string(&sensor.path)
                    .ok()
                    .and_then(|data| data.trim_end().parse::<u64>().ok())
                    .map_or(0.0, |power_uw| power_uw as f64 / 1_000_000.0)
            }
            None => return 0,
        };

        power.round().clamp(0.0, u16::MAX as f64) as u16
    }

    /// Reads the CPU instant and provides usage statistics.
    pub fn read_instant(&self) -> CpuInstant {
        CpuInstant::now().unwrap_or_else(|_| {
            error!("Failed to get CPU usage");
            exit(1);
        })
    }

    /// Reads the CPU instant one more time and calculates the utilization as a `0-100` number.
    pub fn get_usage(&self, initial_instant: CpuInstant) -> u8 {
        let usage = (self.read_instant() - initial_instant).non_idle() * 100.0;

        (usage).round() as u8
    }

    /// Reads the frequency of all CPU cores and returns the highest one in MHz.
    pub fn get_frequency(&self) -> u16 {
        let cpuinfo = read_to_string("/proc/cpuinfo").unwrap_or_else(|_| {
            error!("Failed to get CPU clock");
            exit(1);
        });

        let mut highest_core = 0.0;
        for info in cpuinfo.lines() {
            if info.starts_with("cpu MHz") {
                let clock = info.split(":").nth(1).unwrap();
                let clock = clock.trim().parse::<f32>().unwrap();
                if clock > highest_core {
                    highest_core = clock;
                }
            }
        }

        highest_core.round() as u16
    }
}

/// Looks for the appropriate CPU temperature sensor datastream in the hwmon directory.
fn find_temp_sensor() -> Option<String> {
    for sensor in read_dir("/sys/class/hwmon").ok()? {
        let path = sensor.ok()?.path().to_str()?.to_owned();
        if let Ok(name) = read_to_string(format!("{path}/name")) {
            if ["asusec", "coretemp", "k10temp", "zenpower"].contains(&name.trim_end()) {
                return Some(format!("{path}/temp1_input"));
            }
        }
    }

    None
}

/// Finds a CPU power source, preferring one that is known to work on this machine.
fn find_power_sensor() -> Option<PowerSensor> {
    // On the AMD BC-250 the CPU and GPU share the same die, and the amdgpu hwmon
    // exposes the APU package power (PPT), which follows the CPU load as well.
    if is_bc250() {
        if let Some(sensor) = find_package_power_sensor() {
            return Some(PowerSensor::Package(sensor));
        }
    }

    if let Some(sensor) = find_energy_sensor() {
        return Some(PowerSensor::Energy(sensor));
    }

    if let Some(sensor) = find_msr_sensor() {
        return Some(PowerSensor::Msr(sensor));
    }

    None
}

/// Finds an Intel RAPL or AMD zenergy/amd_energy package energy counter.
fn find_energy_sensor() -> Option<EnergySensor> {
    let intel_rapl = Path::new("/sys/class/powercap/intel-rapl/intel-rapl:0");
    let intel_path = intel_rapl.join("energy_uj");
    if intel_path.is_file() {
        let max_uj = read_to_string(intel_rapl.join("max_energy_range_uj"))
            .ok()
            .and_then(|data| data.trim_end().parse::<u64>().ok());
        return Some(EnergySensor {
            path: intel_path,
            max_uj,
        });
    }

    for entry in read_dir("/sys/class/hwmon").ok()?.flatten() {
        let path = entry.path();
        let Ok(name) = read_to_string(path.join("name")) else {
            continue;
        };
        if !["zenergy", "amd_energy"].contains(&name.trim_end()) {
            continue;
        }

        if let Some(path) = find_socket_energy(&path) {
            return Some(EnergySensor { path, max_uj: None });
        }
    }

    None
}

/// Finds the AMD package energy counter through the RAPL MSRs.
fn find_msr_sensor() -> Option<MsrSensor> {
    if !is_amd_cpu() {
        return None;
    }

    let unit = read_msr(MSR_AMD_RAPL_POWER_UNIT)?;
    if unit == 0 {
        return None;
    }

    // Bits 12:8 hold the energy status unit (ESU): 1 unit = 2^-ESU joules.
    let esu = ((unit >> 8) & 0x1F) as i32;
    if esu > 24 {
        return None;
    }

    // Make sure the energy counter itself is readable before selecting this source.
    read_msr(MSR_AMD_PKG_ENERGY_STATUS)?;

    Some(MsrSensor {
        energy_unit_j: 2f64.powi(-esu),
    })
}

/// Finds a hwmon channel exposing the APU/package power in microwatts.
fn find_package_power_sensor() -> Option<PackagePowerSensor> {
    for entry in read_dir("/sys/class/hwmon").ok()?.flatten() {
        let path = entry.path();
        let Ok(name) = read_to_string(path.join("name")) else {
            continue;
        };
        if name.trim_end() != "amdgpu" {
            continue;
        }

        for file in ["power1_input", "power1_average"] {
            let candidate = path.join(file);
            if candidate.is_file() {
                return Some(PackagePowerSensor { path: candidate });
            }
        }
    }

    None
}

/// Reads a 64-bit MSR from the first CPU.
fn read_msr(register: u64) -> Option<u64> {
    let mut file = File::open(MSR_DEVICE).ok()?;
    file.seek(SeekFrom::Start(register)).ok()?;
    let mut data = [0u8; 8];
    file.read_exact(&mut data).ok()?;
    Some(u64::from_le_bytes(data))
}

/// Checks whether the CPU vendor is AMD.
fn is_amd_cpu() -> bool {
    read_to_string("/proc/cpuinfo")
        .map(|data| {
            data.lines()
                .any(|line| line.starts_with("vendor_id") && line.contains("AuthenticAMD"))
        })
        .unwrap_or(false)
}

/// Checks whether this is an AMD BC-250 board.
fn is_bc250() -> bool {
    ["/sys/class/dmi/id/board_name", "/sys/class/dmi/id/product_name"]
        .iter()
        .any(|path| {
            read_to_string(path)
                .map(|data| data.to_ascii_lowercase().contains("bc-250"))
                .unwrap_or(false)
        })
}

/// Finds the package/socket energy channel exposed by an AMD hwmon device.
fn find_socket_energy(hwmon: &Path) -> Option<PathBuf> {
    for entry in read_dir(hwmon).ok()?.flatten() {
        let label_path = entry.path();
        let Some(file_name) = label_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(channel) = file_name
            .strip_prefix("energy")
            .and_then(|name| name.strip_suffix("_label"))
        else {
            continue;
        };
        let Ok(label) = read_to_string(&label_path) else {
            continue;
        };
        if label.trim_end().starts_with("Esocket") {
            let input_path = hwmon.join(format!("energy{channel}_input"));
            if input_path.is_file() {
                return Some(input_path);
            }
        }
    }

    None
}

/// Gets the CPU model name.
pub fn get_name() -> Option<String> {
    let file = File::open("/proc/cpuinfo").ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line.ok()?;
        if line.starts_with("model name") {
            if let Some(colon_pos) = line.find(':') {
                return Some(line[colon_pos + 1..].trim().to_string());
            }
        }
    }

    None
}
