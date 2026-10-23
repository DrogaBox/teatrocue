//! RAM y CPU del proceso y del sistema, sin dependencias externas en Linux y con la API de Win32 en Windows.

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    /// Tiempo de CPU acumulado del proceso (ns, usuario + kernel).
    pub process_cpu_ns: u64,
    /// Tiempo de CPU acumulado de todo el sistema (ns, sumado entre núcleos) y la parte ociosa.
    pub system_total_ns: u64,
    pub system_idle_ns: u64,
    /// Memoria residente del proceso y su pico (bytes).
    pub process_rss: u64,
    pub process_peak_rss: u64,
    /// Memoria física total y disponible del sistema (bytes).
    pub mem_total: u64,
    pub mem_available: u64,
}

pub fn logical_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

#[cfg(target_os = "linux")]
pub fn snapshot() -> Snapshot {
    let mut s = Snapshot::default();
    let page = 4096u64;
    if let Ok(t) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(rss) = t.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()) {
            s.process_rss = rss * page;
        }
    }
    if let Ok(t) = std::fs::read_to_string("/proc/self/status") {
        for l in t.lines() {
            if let Some(v) = l.strip_prefix("VmHWM:") {
                s.process_peak_rss = v.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0) * 1024;
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string("/proc/self/stat") {
        // Los campos 14 y 15 (utime, stime) vienen después del `)` del nombre del proceso.
        if let Some(rest) = t.rsplit(')').next() {
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() > 12 {
                let ticks: u64 = f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
                s.process_cpu_ns = ticks * 10_000_000; // CLK_TCK = 100
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string("/proc/stat") {
        if let Some(l) = t.lines().next() {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            if v.len() >= 5 {
                s.system_total_ns = v.iter().sum::<u64>() * 10_000_000;
                s.system_idle_ns = (v[3] + v[4]) * 10_000_000;
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string("/proc/meminfo") {
        for l in t.lines() {
            let kb = |p: &str| l.strip_prefix(p).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).map(|v| v * 1024);
            if let Some(v) = kb("MemTotal:") {
                s.mem_total = v;
            }
            if let Some(v) = kb("MemAvailable:") {
                s.mem_available = v;
            }
        }
    }
    s
}

#[cfg(windows)]
pub fn snapshot() -> Snapshot {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};

    fn ft(f: FILETIME) -> u64 {
        ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) * 100 // unidades de 100 ns
    }
    let mut s = Snapshot::default();
    unsafe {
        let proc = GetCurrentProcess();
        let mut pmc = PROCESS_MEMORY_COUNTERS::default();
        if GetProcessMemoryInfo(proc, &mut pmc, std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32).is_ok() {
            s.process_rss = pmc.WorkingSetSize as u64;
            s.process_peak_rss = pmc.PeakWorkingSetSize as u64;
        }
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetProcessTimes(proc, &mut c, &mut e, &mut k, &mut u).is_ok() {
            s.process_cpu_ns = ft(k) + ft(u);
        }
        let (mut idle, mut kern, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetSystemTimes(Some(&mut idle), Some(&mut kern), Some(&mut user)).is_ok() {
            // `kern` ya incluye el tiempo ocioso.
            s.system_total_ns = ft(kern) + ft(user);
            s.system_idle_ns = ft(idle);
        }
        let mut ms = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if GlobalMemoryStatusEx(&mut ms).is_ok() {
            s.mem_total = ms.ullTotalPhys;
            s.mem_available = ms.ullAvailPhys;
        }
    }
    s
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn snapshot() -> Snapshot {
    Snapshot::default()
}
