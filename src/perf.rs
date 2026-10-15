//! Memory/CPU sampling for the browser process plus every WebKit helper it owns
//! (page, GPU and network processes) — the same "Memory" figure Activity Monitor shows.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Browser,
    Page,
    Gpu,
    Network,
}

#[derive(Clone, Copy, Debug)]
pub struct Proc {
    pub pid: i32,
    pub kind: Kind,
    /// Physical footprint in bytes.
    pub footprint: u64,
    /// Total CPU time consumed, in nanoseconds.
    pub cpu_ns: u64,
}

/// macOS memory pressure: 1 = normal, 2 = warning, 4 = critical.
pub const PRESSURE_WARN: u32 = 2;

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::OnceLock;

    use super::{Kind, Proc};

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
        fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    }

    /// rusage CPU times are in Mach ticks on Apple silicon.
    fn ticks_to_ns(t: u64) -> u64 {
        static TB: OnceLock<(u64, u64)> = OnceLock::new();
        let (n, d) = *TB.get_or_init(|| {
            let mut tb = Timebase { numer: 0, denom: 0 };
            unsafe { mach_timebase_info(&mut tb) };
            (tb.numer.max(1) as u64, tb.denom.max(1) as u64)
        });
        t.saturating_mul(n) / d
    }

    pub fn usage(pid: i32) -> Option<(u64, u64)> {
        let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, &mut ri as *mut _ as *mut libc::rusage_info_t) };
        (ok == 0).then(|| (ri.ri_phys_footprint, ticks_to_ns(ri.ri_user_time + ri.ri_system_time)))
    }

    fn name(pid: i32) -> String {
        let mut buf = [0u8; 256];
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
    }

    /// WebKit's helpers are XPC services, so they're children of launchd; they are tied to
    /// us through the "responsible process" (rzen itself when launched as an app, the
    /// terminal when run from one).
    pub fn processes() -> Vec<Proc> {
        let me = std::process::id() as i32;
        let owner = unsafe { responsibility_get_pid_responsible_for_pid(me) };
        let mut pids = vec![0i32; 4096];
        let bytes = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
        pids.truncate(bytes.max(0) as usize);
        let mut out = Vec::new();
        for pid in pids.into_iter().filter(|&p| p > 0) {
            let kind = if pid == me {
                Kind::Browser
            } else {
                if unsafe { responsibility_get_pid_responsible_for_pid(pid) } != owner {
                    continue;
                }
                let n = name(pid);
                match n.strip_prefix("com.apple.WebKit.") {
                    Some("WebContent") => Kind::Page,
                    Some("GPU") => Kind::Gpu,
                    Some("Networking") => Kind::Network,
                    _ => continue,
                }
            };
            if let Some((footprint, cpu_ns)) = usage(pid) {
                out.push(Proc { pid, kind, footprint, cpu_ns });
            }
        }
        out
    }

    /// True when macOS attributes WebKit helpers to rzen itself (launched as an app), false
    /// when they're attributed to a terminal shared with other programs.
    pub fn exact() -> bool {
        let me = std::process::id() as i32;
        unsafe { responsibility_get_pid_responsible_for_pid(me) == me }
    }

    pub fn pressure() -> u32 {
        let mut level: libc::c_int = 1;
        let mut size = std::mem::size_of::<libc::c_int>();
        unsafe {
            libc::sysctlbyname(
                c"kern.memorystatus_vm_pressure_level".as_ptr(),
                (&mut level as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        level.max(1) as u32
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Proc;

    pub fn processes() -> Vec<Proc> {
        Vec::new()
    }

    pub fn exact() -> bool {
        true
    }

    pub fn pressure() -> u32 {
        1
    }
}

pub use imp::{exact, pressure, processes};
