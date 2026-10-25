//! OS counters for this process, not a Rust heap estimate or a claim about
//! subprocesses. Scenario measurements must account for helpers separately.
use serde_json::{Value, json};
pub fn memory_and_cpu() -> Value {
    let (rss, private_bytes, cpu_ms) = counters();
    json!({"memory":{"rss":rss,"private_bytes":private_bytes},"cpu_time_ms":cpu_ms})
}
#[cfg(windows)]
fn counters() -> (Option<usize>, Option<usize>, Option<f64>) {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: size,
        ..Default::default()
    };
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // Pseudo-handle belongs to this process; all output buffers have the exact
    // Win32 structure size and remain alive for the duration of these calls.
    let memory_ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::addr_of_mut!(memory).cast(),
            size,
        ) != 0
    };
    let cpu_ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ) != 0
    };
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    (
        memory_ok.then_some(memory.WorkingSetSize),
        memory_ok.then_some(memory.PrivateUsage),
        cpu_ok.then_some((ticks(kernel) + ticks(user)) as f64 / 10000.0),
    )
}
#[cfg(unix)]
fn counters() -> (Option<usize>, Option<usize>, Option<f64>) {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Only writes the supplied timespec for this process's CPU clock.
    let cpu_ok = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) == 0 };
    #[cfg(target_os = "linux")]
    let rss = {
        // sysconf queries the page size; it does not access another process.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|text| text.split_whitespace().nth(1)?.parse::<usize>().ok())
            .and_then(|pages| {
                usize::try_from(page)
                    .ok()
                    .and_then(|page| pages.checked_mul(page))
            })
    };
    #[cfg(not(target_os = "linux"))]
    let rss = None;
    (
        rss,
        None,
        cpu_ok.then_some(time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1e6),
    )
}
#[cfg(not(any(windows, unix)))]
fn counters() -> (Option<usize>, Option<usize>, Option<f64>) {
    (None, None, None)
}
