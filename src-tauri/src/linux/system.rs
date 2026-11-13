//! The Hardware tool and the closed notch's readings on Linux (the Windows ones are system.rs and system/hw.rs).
//!
//! `system_stats` answers the page's poll with the same `Stats` the Windows backend builds. CPU, memory, processes and drives
//! come from `sysinfo`; temperatures, battery, network and GPUs from /proc and /sys, which every distribution has; an NVIDIA
//! card is asked through `nvidia-smi`, and only when the card is awake, so a hybrid laptop's GPU is not woken to be measured.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tauri::State;

const NO_PING: u64 = u64::MAX;
/// The ping thread works while the page asked for a ping this recently.
const PING_IDLE: Duration = Duration::from_secs(6);
const PING_EVERY: Duration = Duration::from_secs(2);
/// Processes are read at most this often, and let go of this long after the tool closed.
const PROCESSES_EVERY: Duration = Duration::from_millis(1800);
const PROCESS_LINGER: Duration = Duration::from_secs(10);
const DRIVES_EVERY: Duration = Duration::from_secs(10);

// ---- what the page receives -----------------------------------------------------------------------------------------

/// Extras the closed row asks for outside the tool (the tool gets everything).
#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "camelCase", default)]
pub struct Want {
    net: bool,
    temps: bool,
    power: bool,
    battery: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    cpu: f32,
    cpu_name: String,
    mem_used: u64,
    mem_total: u64,
    gpu: Option<f64>,
    gpu_name: Option<String>,
    gpu_mem_used: u64,
    gpu_mem_total: u64,
    ping: Option<u64>,
    at: f64,
    cores: Vec<Core>,
    cpu_mhz: Option<f64>,
    physical_cores: Option<usize>,
    mem_cached: u64,
    mem_commit: u64,
    mem_commit_limit: u64,
    gpus: Vec<GpuReading>,
    net: Option<[u64; 2]>,
    zones: Vec<(String, f64)>,
    energy: Vec<(String, f64)>,
    meter: Vec<(String, f64)>,
    nvml: Vec<NvmlGpu>,
    external: Option<Reading>,
    battery: Option<BatteryRaw>,
    drives: Option<Drives>,
    processes: Option<Vec<Proc>>,
}

#[derive(Serialize, Clone, Copy)]
pub struct Core {
    usage: f32,
    kind: u8,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GpuReading {
    key: String,
    name: String,
    mem_total: u64,
    usage: Option<f64>,
    engine: Option<String>,
    mem_used: u64,
    temp: Option<f64>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Proc {
    id: String,
    name: String,
    path: Option<String>,
    count: u32,
    cpu: f32,
    mem: u64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct NvmlGpu {
    name: String,
    watts: Option<f64>,
    temp: Option<f64>,
}

/// Temperature and power from a monitoring program the Windows build reads (HWiNFO); there is none here.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    source: &'static str,
    cpu_temp: Option<f64>,
    cpu_watts: Option<f64>,
    gpus: Vec<ExtGpu>,
}

#[derive(Serialize)]
pub struct ExtGpu {
    name: String,
    temp: Option<f64>,
    watts: Option<f64>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BatteryRaw {
    /// 0 not present, 1 discharging, 2 idle, 3 charging.
    status: i32,
    /// Positive while charging, negative while discharging.
    rate_mw: Option<i32>,
    remaining_mwh: Option<i32>,
    full_mwh: Option<i32>,
    design_mwh: Option<i32>,
    ac: Option<bool>,
    percent: Option<u8>,
    life_secs: Option<u32>,
    saver: bool,
}

#[derive(Serialize, Clone, Default)]
pub struct Drives {
    volumes: Vec<Volume>,
    disks: Vec<Disk>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Volume {
    /// The mount point: there are no drive letters.
    letter: String,
    total: u64,
    free: u64,
    disk: Option<u32>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    number: u32,
    model: String,
    bus: u32,
    removable: bool,
    ssd: bool,
    temp: Option<[i16; 3]>,
}

// ---- readers over /proc and /sys ------------------------------------------------------------------------------------

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_num<T: std::str::FromStr>(path: impl AsRef<Path>) -> Option<T> {
    read_trim(path)?.parse().ok()
}

/// "Intel(R) Core(TM) i7-1185G7 @ 3.00GHz" becomes "Core i7-1185G7", like the Windows names.
fn clean_name(name: &str) -> String {
    let mut s = name.replace("(R)", "").replace("(TM)", "").replace("(tm)", "");
    if let Some(i) = s.find(" @ ") {
        s.truncate(i);
    }
    for prefix in ["NVIDIA ", "AMD ", "Intel "] {
        if s.starts_with(prefix) && s.len() > prefix.len() + 6 {
            s = s[prefix.len()..].to_string();
        }
    }
    s.replace(" CPU", "").replace("  ", " ").replace(" Processor", "").trim().to_string()
}

/// "Cached" and "Buffers" of /proc/meminfo, in bytes.
fn meminfo_cached(meminfo: &str) -> u64 {
    let kb = |key: &str| meminfo.lines().find_map(|l| l.strip_prefix(key)?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()).unwrap_or(0);
    (kb("Cached:") + kb("Buffers:")) * 1024
}

/// Cumulative bytes received and sent over the physical interfaces of /proc/net/dev (a virtual one, a bridge or a container's
/// would count the same traffic twice).
fn net_totals(dev: &str, physical: impl Fn(&str) -> bool) -> Option<[u64; 2]> {
    let mut total = [0u64; 2];
    let mut any = false;
    for line in dev.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name == "lo" || !physical(name) {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        if f.len() >= 9 {
            total[0] += f[0];
            total[1] += f[8];
            any = true;
        }
    }
    any.then_some(total)
}

fn is_physical(interface: &str) -> bool {
    Path::new("/sys/class/net").join(interface).join("device").exists()
}

/// The hwmon names whose temperatures are the CPU's.
const CPU_SENSORS: [&str; 5] = ["k10temp", "coretemp", "zenpower", "cpu_thermal", "x86_pkg_temp"];

/// A temperature sensor's name for the page: the CPU's carry "CPU", which is what it looks for.
fn zone_name(chip: &str, label: &str) -> String {
    let label = if label.is_empty() { "temp" } else { label };
    if CPU_SENSORS.contains(&chip) { format!("CPU {label}") } else { format!("{chip} {label}") }
}

/// Every hwmon temperature as (name, kelvin).
fn read_zones() -> Vec<(String, f64)> {
    let mut zones = Vec::new();
    for chip in fs::read_dir("/sys/class/hwmon").into_iter().flatten().flatten() {
        let dir = chip.path();
        let name = read_trim(dir.join("name")).unwrap_or_default();
        // The GPU's and the drives' own sensors are shown where those are.
        if name.starts_with("amdgpu") || name.starts_with("nvme") || name.starts_with("nouveau") || name.is_empty() {
            continue;
        }
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(n) = file.strip_prefix("temp").and_then(|r| r.strip_suffix("_input")) else { continue };
            let Some(milli) = read_num::<f64>(entry.path()) else { continue };
            let label = read_trim(dir.join(format!("temp{n}_label"))).unwrap_or_default();
            zones.push((zone_name(&name, &label), milli / 1000.0 + 273.15));
        }
    }
    zones.sort_by(|a, b| a.0.cmp(&b.0));
    zones
}

fn battery_status(status: &str) -> i32 {
    match status {
        "Discharging" => 1,
        "Charging" => 3,
        "Full" | "Not charging" => 2,
        _ => 0,
    }
}

fn read_battery() -> Option<BatteryRaw> {
    let supplies: Vec<PathBuf> = fs::read_dir("/sys/class/power_supply").ok()?.flatten().map(|e| e.path()).collect();
    let bat = supplies.iter().find(|p| read_trim(p.join("type")).as_deref() == Some("Battery") && read_trim(p.join("scope")).as_deref() != Some("Device"))?;
    let status = battery_status(&read_trim(bat.join("status")).unwrap_or_default());
    // µWh and µW; some batteries give charge (µAh) and a voltage (µV) instead.
    let volt = read_num::<f64>(bat.join("voltage_now")).unwrap_or(0.0);
    let from_charge = |file: &str| read_num::<f64>(bat.join(file)).map(|c| (c * volt / 1e9) as i32);
    let energy = |file: &str, charge: &str| read_num::<f64>(bat.join(file)).map(|e| (e / 1000.0) as i32).or_else(|| from_charge(charge));
    let power = read_num::<f64>(bat.join("power_now")).map(|p| (p / 1000.0) as i32).or_else(|| read_num::<f64>(bat.join("current_now")).map(|c| (c * volt / 1e9) as i32));
    let mains = supplies.iter().filter(|p| read_trim(p.join("type")).as_deref() == Some("Mains")).find_map(|p| read_num::<u8>(p.join("online")).map(|o| o == 1));
    Some(BatteryRaw {
        status,
        rate_mw: power.map(|p| if status == 1 { -p.abs() } else { p.abs() }),
        remaining_mwh: energy("energy_now", "charge_now"),
        full_mwh: energy("energy_full", "charge_full"),
        design_mwh: energy("energy_full_design", "charge_full_design"),
        ac: mains,
        percent: read_num(bat.join("capacity")),
        life_secs: None,
        saver: false,
    })
}

/// Filesystems that are not storage the user thinks of as a drive.
fn real_filesystem(fs_name: &str, mount: &str) -> bool {
    const VIRTUAL: [&str; 12] = ["tmpfs", "devtmpfs", "squashfs", "overlay", "ramfs", "efivarfs", "proc", "sysfs", "cgroup2", "fuse.portal", "fuse.gvfsd-fuse", "fuse.snapfuse"];
    !VIRTUAL.contains(&fs_name) && !mount.starts_with("/boot") && !mount.starts_with("/snap") && !mount.starts_with("/var/lib/docker") && !mount.starts_with("/run")
}

/// The mounted drives of real filesystems, each device once (a btrfs subvolume is mounted several times), over 1 GiB.
fn read_drives() -> Drives {
    let mut seen = HashSet::new();
    let mut volumes: Vec<Volume> = Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| real_filesystem(&d.file_system().to_string_lossy(), &d.mount_point().to_string_lossy()))
        .filter(|d| d.total_space() > (1 << 30) && seen.insert(d.name().to_string_lossy().into_owned()))
        .map(|d| Volume { letter: d.mount_point().to_string_lossy().into_owned(), total: d.total_space(), free: d.available_space(), disk: None })
        .collect();
    volumes.sort_by(|a, b| a.letter.cmp(&b.letter));
    Drives { volumes, disks: Vec::new() }
}

// ---- GPUs -----------------------------------------------------------------------------------------------------------

/// A graphics card of /sys/class/drm.
#[derive(Clone, Debug, PartialEq)]
struct Card {
    /// The PCI slot, which is stable and unique.
    key: String,
    dir: PathBuf,
    vendor: u32,
}

const AMD: u32 = 0x1002;
const NVIDIA: u32 = 0x10de;
const INTEL: u32 = 0x8086;

fn cards() -> Vec<Card> {
    let mut found: Vec<Card> = fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // "card0", not "card0-eDP-1" (a connector).
            if !name.starts_with("card") || name[4..].chars().any(|c| !c.is_ascii_digit()) {
                return None;
            }
            let dir = e.path().join("device");
            let vendor = u32::from_str_radix(read_trim(dir.join("vendor"))?.trim_start_matches("0x"), 16).ok()?;
            let key = read_trim(dir.join("uevent"))?.lines().find_map(|l| l.strip_prefix("PCI_SLOT_NAME=").map(str::to_string))?;
            Some(Card { key, dir, vendor })
        })
        .collect();
    found.sort_by(|a, b| a.key.cmp(&b.key));
    found
}

fn hwmon_temp(card: &Card) -> Option<f64> {
    let chip = fs::read_dir(card.dir.join("hwmon")).ok()?.flatten().next()?.path();
    read_num::<f64>(chip.join("temp1_input")).map(|t| t / 1000.0)
}

fn vendor_name(vendor: u32) -> &'static str {
    match vendor {
        AMD => "AMD Radeon Graphics",
        NVIDIA => "NVIDIA Graphics",
        INTEL => "Intel Graphics",
        _ => "Graphics",
    }
}

/// One line of `nvidia-smi --query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw
/// --format=csv,noheader,nounits` (MiB, percent, °C, watts).
#[derive(Debug, PartialEq)]
struct Smi {
    name: String,
    usage: Option<f64>,
    mem_used: u64,
    mem_total: u64,
    temp: Option<f64>,
    watts: Option<f64>,
}

fn parse_smi(out: &str) -> Vec<Smi> {
    out.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let [name, usage, used, total, temp, watts] = f.as_slice() else { return None };
            let mib = |s: &str| s.parse::<f64>().ok().map_or(0, |m| (m * 1_048_576.0) as u64);
            Some(Smi { name: name.to_string(), usage: usage.parse().ok(), mem_used: mib(used), mem_total: mib(total), temp: temp.parse().ok(), watts: watts.parse().ok() })
        })
        .collect()
}

/// A card that is powered down is not woken to be measured (a hybrid laptop's discrete GPU idles most of the time).
fn awake(card: &Card) -> bool {
    read_trim(card.dir.join("power/runtime_status")).is_none_or(|s| s != "suspended")
}

fn nvidia_smi() -> Vec<Smi> {
    Command::new("nvidia-smi")
        .args(["--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw", "--format=csv,noheader,nounits"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_smi(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// The readings of every card, and the NVIDIA power and temperature lines for `nvml`.
fn read_gpus(counters: bool) -> (Vec<GpuReading>, Vec<NvmlGpu>) {
    let cards = cards();
    let nvidia_awake: Vec<&Card> = cards.iter().filter(|c| c.vendor == NVIDIA && awake(c)).collect();
    let smi = if counters && !nvidia_awake.is_empty() { nvidia_smi() } else { Vec::new() };
    let mut smi = smi.into_iter();
    let mut gpus = Vec::new();
    let mut nvml = Vec::new();
    for card in &cards {
        let mut reading = GpuReading { key: card.key.clone(), name: read_trim(card.dir.join("product_name")).unwrap_or_else(|| vendor_name(card.vendor).into()), mem_total: 0, usage: None, engine: None, mem_used: 0, temp: None };
        match card.vendor {
            AMD => {
                reading.mem_total = read_num(card.dir.join("mem_info_vram_total")).unwrap_or(0);
                if counters {
                    reading.usage = read_num(card.dir.join("gpu_busy_percent"));
                    reading.mem_used = read_num(card.dir.join("mem_info_vram_used")).unwrap_or(0);
                }
                reading.temp = hwmon_temp(card);
            }
            NVIDIA if awake(card) => {
                if let Some(s) = smi.next() {
                    reading.name = s.name.clone();
                    reading.usage = s.usage;
                    reading.mem_used = s.mem_used;
                    reading.mem_total = s.mem_total;
                    reading.temp = s.temp;
                    nvml.push(NvmlGpu { name: s.name, watts: s.watts, temp: s.temp });
                }
            }
            _ => {}
        }
        gpus.push(reading);
    }
    (gpus, nvml)
}

// ---- processes ------------------------------------------------------------------------------------------------------

/// The programs by executable name (an app's processes fold into one entry): the top 12 by processor time and the top 12 by
/// memory.
fn fold_processes(rows: impl Iterator<Item = (String, Option<String>, f32, u64)>) -> Vec<Proc> {
    let mut by_name: HashMap<String, Proc> = HashMap::new();
    for (name, path, cpu, mem) in rows {
        let entry = by_name.entry(name.to_lowercase()).or_insert_with(|| Proc { id: name.to_lowercase(), name, path, count: 0, cpu: 0.0, mem: 0 });
        entry.count += 1;
        entry.cpu += cpu;
        entry.mem += mem;
    }
    let mut all: Vec<Proc> = by_name.into_values().collect();
    let mut keep: HashSet<String> = HashSet::new();
    all.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    keep.extend(all.iter().take(12).map(|p| p.id.clone()));
    all.sort_by_key(|p| std::cmp::Reverse(p.mem));
    keep.extend(all.iter().take(12).map(|p| p.id.clone()));
    all.retain(|p| keep.contains(&p.id));
    all.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    all
}

// ---- the probe ------------------------------------------------------------------------------------------------------

struct Ping {
    ms: AtomicU64,
    /// The last time the page asked for the ping.
    wanted_at: Mutex<Instant>,
}

/// Times a TCP connect to a public resolver in the background, while the page shows the ping.
fn spawn_ping(ping: Arc<Ping>) {
    thread::spawn(move || loop {
        thread::sleep(PING_EVERY);
        if ping.wanted_at.lock().map(|w| w.elapsed() > PING_IDLE).unwrap_or(true) {
            ping.ms.store(NO_PING, Ordering::Relaxed);
            continue;
        }
        let target = SocketAddr::from(([1, 1, 1, 1], 443));
        let started = Instant::now();
        let ms = TcpStream::connect_timeout(&target, Duration::from_millis(1500)).map_or(NO_PING, |_| started.elapsed().as_millis() as u64);
        ping.ms.store(ms, Ordering::Relaxed);
    });
}

struct Probe {
    sys: System,
    started: Instant,
    full_at: Instant,
    /// Processes are being read (the tool is open).
    procs_on: bool,
    processes: Vec<Proc>,
    processes_at: Option<Instant>,
    drives: Option<(Instant, Drives)>,
    ping: Arc<Ping>,
    /// The package energy counter and when it was read, to turn two readings into watts.
    energy: Option<(Instant, u64)>,
}

pub struct SystemState(Mutex<Probe>);

impl Default for SystemState {
    fn default() -> Self {
        let ping = Arc::new(Ping { ms: AtomicU64::new(NO_PING), wanted_at: Mutex::new(Instant::now() - PING_IDLE) });
        spawn_ping(ping.clone());
        Self(Mutex::new(Probe { sys: System::new(), started: Instant::now(), full_at: Instant::now(), procs_on: false, processes: Vec::new(), processes_at: None, drives: None, ping, energy: None }))
    }
}

impl Probe {
    fn read_processes(&mut self) -> Vec<Proc> {
        if self.processes_at.is_some_and(|at| at.elapsed() < PROCESSES_EVERY) {
            return self.processes.clone();
        }
        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cpu().with_memory().with_exe(UpdateKind::OnlyIfNotSet));
        // Linux lists a program's threads as processes too, which would count its memory once per thread.
        self.processes = fold_processes(self.sys.processes().values().filter(|p| p.thread_kind().is_none()).map(|p| {
            let path = p.exe().map(|e| e.to_string_lossy().into_owned());
            // The executable's file name: the process name is cut to 15 characters.
            let name = path.as_deref().and_then(|e| Path::new(e).file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.name().to_string_lossy().into_owned());
            (name, path, p.cpu_usage(), p.memory())
        }));
        self.processes_at = Some(Instant::now());
        self.processes.clone()
    }

    /// The package's power in milliwatts from RAPL's energy counter, where the kernel lets the user read it.
    fn package_power(&mut self) -> Vec<(String, f64)> {
        let Some(uj) = read_num::<u64>("/sys/class/powercap/intel-rapl:0/energy_uj") else { return Vec::new() };
        let now = Instant::now();
        let watts = self.energy.and_then(|(at, before)| {
            let secs = now.duration_since(at).as_secs_f64();
            (secs > 0.2 && uj >= before).then(|| (uj - before) as f64 / 1e6 / secs * 1000.0)
        });
        self.energy = Some((now, uj));
        watts.map(|mw| vec![("RAPL_Package0_PKG".to_string(), mw)]).unwrap_or_default()
    }
}

#[tauri::command]
pub async fn system_stats(full: bool, with_gpu: Option<bool>, want: Option<Want>, state: State<'_, SystemState>) -> Result<Stats, String> {
    if crate::standby::asleep() {
        return Err("asleep".into());
    }
    let mut guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
    Ok(collect(&mut guard, full, with_gpu, want))
}

/// One poll's readings.
fn collect(p: &mut Probe, full: bool, with_gpu: Option<bool>, want: Option<Want>) -> Stats {
    let want = if full { Want { net: true, temps: true, power: true, battery: true } } else { want.unwrap_or_default() };
    p.sys.refresh_cpu_usage();
    p.sys.refresh_memory();
    if p.sys.cpus().first().is_none_or(|c| c.brand().is_empty()) {
        p.sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
    }
    if full {
        // The clock speed is only read when asked for.
        p.sys.refresh_cpu_frequency();
        *p.ping.wanted_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        p.full_at = Instant::now();
        p.procs_on = true;
    } else if p.procs_on && p.full_at.elapsed() > PROCESS_LINGER {
        // The tool closed: let go of the process list.
        p.procs_on = false;
        p.processes.clear();
        p.processes_at = None;
    }

    let counters = full || with_gpu == Some(true);
    let (mut gpus, nvml) = read_gpus(counters);
    // The busiest adapter, or the one with most video memory while none is busy; the page replaces it with the one chosen.
    let strongest = gpus
        .iter()
        .enumerate()
        .filter(|(_, g)| g.usage.is_some_and(|u| u > 0.0))
        .max_by(|a, b| a.1.usage.unwrap_or(0.0).total_cmp(&b.1.usage.unwrap_or(0.0)))
        .map(|(i, _)| i)
        .or_else(|| gpus.iter().enumerate().max_by_key(|(_, g)| g.mem_total).map(|(i, _)| i));
    let (gpu, gpu_mem_used, gpu_mem_total, gpu_name) = strongest.and_then(|i| gpus.get(i)).map(|g| (g.usage, g.mem_used, g.mem_total, Some(g.name.clone()))).unwrap_or((None, 0, 0, None));
    if !want.temps && !full {
        for g in &mut gpus {
            g.temp = None;
        }
    }

    let cpus = p.sys.cpus();
    let cores = if full { cpus.iter().map(|c| Core { usage: c.cpu_usage(), kind: 0 }).collect() } else { Vec::new() };
    let cpu_mhz = (full && !cpus.is_empty()).then(|| cpus.iter().map(|c| c.frequency() as f64).sum::<f64>() / cpus.len() as f64).filter(|m| *m > 0.0);
    let meminfo = if full { fs::read_to_string("/proc/meminfo").unwrap_or_default() } else { String::new() };
    // "Commit" has no Linux twin that stays below the limit (overcommit): memory and swap in use against memory and swap there is.
    let (mem_commit, mem_commit_limit) = if full { (p.sys.used_memory() + p.sys.used_swap(), p.sys.total_memory() + p.sys.total_swap()) } else { (0, 0) };

    let zones = if want.temps { read_zones() } else { Vec::new() };
    let energy = if want.power { p.package_power() } else { Vec::new() };
    let drives = if full {
        if p.drives.as_ref().is_none_or(|(at, _)| at.elapsed() > DRIVES_EVERY) {
            p.drives = Some((Instant::now(), read_drives()));
        }
        p.drives.as_ref().map(|(_, d)| d.clone())
    } else {
        None
    };
    let processes = if full { Some(p.read_processes()) } else { None };
    let ping = if full { p.ping.ms.load(Ordering::Relaxed) } else { NO_PING };

    Stats {
        cpu: p.sys.global_cpu_usage(),
        cpu_name: p.sys.cpus().first().map(|c| clean_name(c.brand())).unwrap_or_default(),
        mem_used: p.sys.used_memory(),
        mem_total: p.sys.total_memory(),
        gpu,
        gpu_name,
        gpu_mem_used,
        gpu_mem_total,
        ping: (ping != NO_PING).then_some(ping),
        at: p.started.elapsed().as_secs_f64() * 1000.0,
        cores,
        cpu_mhz,
        physical_cores: if full { System::physical_core_count() } else { None },
        mem_cached: meminfo_cached(&meminfo),
        mem_commit,
        mem_commit_limit,
        gpus,
        net: if want.net { net_totals(&fs::read_to_string("/proc/net/dev").unwrap_or_default(), is_physical) } else { None },
        zones,
        energy,
        meter: Vec::new(),
        nvml: if want.temps || want.power { nvml } else { Vec::new() },
        external: None,
        battery: if want.battery || want.power { read_battery() } else { None },
        drives,
        processes,
    }
}

/// The program's icon for the process list; there is no per-file icon lookup here, so the page shows its generic one.
#[tauri::command]
pub async fn system_icon(path: String) -> Result<Option<String>, String> {
    let _ = path;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads this machine for real and prints what the Hardware tool would show. `cargo test -- --ignored --nocapture linux::system`
    #[test]
    #[ignore = "reads the live machine"]
    fn live_stats_look_like_this_machine() {
        let SystemState(probe) = SystemState::default();
        let mut p = probe.lock().unwrap();
        // The first reading has no earlier one to compare with.
        collect(&mut p, true, Some(true), None);
        thread::sleep(Duration::from_millis(1200));
        let s = collect(&mut p, true, Some(true), None);
        println!("cpu {:.0}% {:?} | {} cores, {:?} MHz, {:?} physical", s.cpu, s.cpu_name, s.cores.len(), s.cpu_mhz.map(|m| m as u64), s.physical_cores);
        println!("mem {} / {} MiB, cached {} MiB, commit {} / {} MiB", s.mem_used >> 20, s.mem_total >> 20, s.mem_cached >> 20, s.mem_commit >> 20, s.mem_commit_limit >> 20);
        for g in &s.gpus {
            println!("gpu {} {:?} usage={:?} mem {}/{} MiB temp={:?}", g.key, g.name, g.usage, g.mem_used >> 20, g.mem_total >> 20, g.temp);
        }
        println!("nvml {:?}", s.nvml);
        println!("net {:?} | zones {:?}", s.net, s.zones.iter().map(|(n, k)| format!("{n}={:.0}C", k - 273.15)).collect::<Vec<_>>());
        println!("battery {:?}", s.battery);
        println!("drives {:?}", s.drives.as_ref().map(|d| d.volumes.iter().map(|v| format!("{} {}/{} GiB", v.letter, (v.total - v.free) >> 30, v.total >> 30)).collect::<Vec<_>>()));
        let procs = s.processes.as_ref().unwrap();
        println!("top process {:?}", procs.first().map(|p| (&p.name, p.cpu, p.mem >> 20, p.count)));
        assert!(s.mem_total > 0 && !s.cores.is_empty() && !procs.is_empty() && s.drives.as_ref().is_some_and(|d| !d.volumes.is_empty()));
        assert!(s.net.is_some() && !s.cpu_name.is_empty());
    }

    #[test]
    fn cpu_names_are_cleaned_like_the_windows_ones() {
        assert_eq!(clean_name("Intel(R) Core(TM) i7-1185G7 @ 3.00GHz"), "Core i7-1185G7");
        assert_eq!(clean_name("AMD Ryzen 9 7940HS w/ Radeon 780M Graphics"), "Ryzen 9 7940HS w/ Radeon 780M Graphics");
        assert_eq!(clean_name("AMD Ryzen 7 5800X 8-Core Processor"), "Ryzen 7 5800X 8-Core");
    }

    #[test]
    fn cached_memory_is_cache_and_buffers_in_bytes() {
        let m = "MemTotal:       32000000 kB\nBuffers:          100 kB\nCached:          2000 kB\nSwapCached:         0 kB\n";
        assert_eq!(meminfo_cached(m), 2100 * 1024);
        assert_eq!(meminfo_cached("nothing"), 0);
    }

    #[test]
    fn network_totals_count_physical_interfaces_only() {
        let dev = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 1000 1 0 0 0 0 0 0 1000 1 0 0 0 0 0 0\n  eth0: 5000 10 0 0 0 0 0 0 2500 8 0 0 0 0 0 0\n docker0: 9999 1 0 0 0 0 0 0 9999 1 0 0 0 0 0 0\n wlan0: 700 5 0 0 0 0 0 0 300 4 0 0 0 0 0 0\n";
        assert_eq!(net_totals(dev, |n| n == "eth0" || n == "wlan0"), Some([5700, 2800]));
        assert_eq!(net_totals(dev, |_| false), None);
    }

    #[test]
    fn cpu_sensors_are_named_so_the_page_finds_them() {
        assert_eq!(zone_name("k10temp", "Tctl"), "CPU Tctl");
        assert_eq!(zone_name("coretemp", "Package id 0"), "CPU Package id 0");
        assert_eq!(zone_name("iwlwifi_1", ""), "iwlwifi_1 temp");
    }

    #[test]
    fn battery_states_follow_the_windows_numbers() {
        assert_eq!((battery_status("Discharging"), battery_status("Charging"), battery_status("Full"), battery_status("Unknown")), (1, 3, 2, 0));
    }

    #[test]
    fn only_real_storage_is_a_drive() {
        assert!(real_filesystem("ext4", "/"));
        assert!(real_filesystem("btrfs", "/home"));
        assert!(!real_filesystem("tmpfs", "/tmp"));
        assert!(!real_filesystem("vfat", "/boot/efi"));
        assert!(!real_filesystem("squashfs", "/snap/core/1"));
        assert!(!real_filesystem("ext4", "/var/lib/docker/overlay2"));
    }

    #[test]
    fn nvidia_smi_lines_are_read_in_mib_percent_celsius_and_watts() {
        let out = "NVIDIA GeForce RTX 5060 Laptop GPU, 12, 512, 8151, 43, 9.35\n";
        let g = &parse_smi(out)[0];
        assert_eq!((g.usage, g.mem_used, g.mem_total, g.temp, g.watts), (Some(12.0), 512 * 1_048_576, 8151 * 1_048_576, Some(43.0), Some(9.35)));
        // A card that reports "[N/A]" for a figure still gives the rest.
        let na = parse_smi("GPU, [N/A], 1, 2, [N/A], [N/A]\n");
        assert_eq!((na[0].usage, na[0].temp, na[0].mem_total), (None, None, 2 * 1_048_576));
        assert!(parse_smi("garbage").is_empty());
    }

    #[test]
    fn processes_fold_by_executable_and_keep_the_busiest_and_largest() {
        let rows = vec![
            ("Chrome".to_string(), Some("/opt/chrome".to_string()), 10.0, 100),
            ("chrome".to_string(), None, 15.0, 300),
            ("bash".to_string(), None, 0.0, 5),
        ];
        let folded = fold_processes(rows.into_iter());
        let chrome = folded.iter().find(|p| p.id == "chrome").unwrap();
        assert_eq!((chrome.count, chrome.cpu, chrome.mem), (2, 25.0, 400));
        assert_eq!(folded.len(), 2);
        // More than 12 of each kind: only the busiest 12 and the largest 12 remain.
        let many = (0..40).map(|i| (format!("p{i}"), None, i as f32, (40 - i) as u64));
        assert!(fold_processes(many).len() <= 24);
    }
}
