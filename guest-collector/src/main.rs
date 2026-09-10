//! Early-boot collector.  This intentionally has no async runtime: the guest
//! may be sufficiently broken that allocating a runtime or resolving a mount
//! is itself useful evidence to avoid.
use std::sync::atomic::{AtomicUsize, Ordering};
mod irq_affinity;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::{fs::OpenOptionsExt, net::UnixDatagram},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime},
};

const RECORD_MAX: usize = 16 * 1024;
const TRACE_BUDGET: usize = 128 * 1024 * 1024;
const META_BUDGET: usize = 8 * 1024 * 1024;
const CRITICAL_BUDGET: usize = 2 * 1024 * 1024;
const RUN_FOR: Duration = Duration::from_secs(600);
const STACK_INTERVAL: Duration = Duration::from_secs(3);
const MAX_ACTIVE_REQUESTS: usize = 8192;
const MAX_ERROR_SAMPLES: u8 = 8;
const EXPECTED_KERNEL: &str = "5.14.0-742.el9.x86_64";
const PROBE_SOURCE_REV: &str = "c234fccc3093bbe4fbe7482b00e037d0810dc720";
const PROBES: &[(&str, &str, &str)] = &[
    (
        "request_entry",
        "p:bootc_virtiofs_enqueue virtio_fs_enqueue_req req=%si",
        "virtio_fs_enqueue_req",
    ),
    (
        "request_return",
        "r:bootc_virtiofs_enqueue_ret virtio_fs_enqueue_req ret=$retval",
        "virtio_fs_enqueue_req",
    ),
    (
        "request_completion",
        "p:bootc_virtiofs_completion virtio_fs_request_complete req=%di",
        "virtio_fs_request_complete",
    ),
    (
        "request_end",
        "p:bootc_fuse_request_end fuse_request_end req=%di",
        "fuse_request_end",
    ),
];
static TRACE_BYTES: AtomicUsize = AtomicUsize::new(0);
static META_BYTES: AtomicUsize = AtomicUsize::new(0);
static CRITICAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static DROPPED_TRACE: AtomicUsize = AtomicUsize::new(0);
static DROPPED_META: AtomicUsize = AtomicUsize::new(0);
static DROPPED_WRITE: AtomicUsize = AtomicUsize::new(0);

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn bounded(s: impl AsRef<str>) -> String {
    s.as_ref().chars().take(RECORD_MAX).collect()
}

fn within_budget(previous: usize, bytes: usize, limit: usize) -> bool {
    previous.saturating_add(bytes) <= limit
}

fn parse_return_value(value: &str) -> Option<i64> {
    let value = value.trim();
    value
        .strip_prefix("0x")
        .map(|value| {
            u64::from_str_radix(value, 16)
                .ok()
                .map(|value| value as i64)
        })
        .unwrap_or_else(|| value.parse().ok())
}

fn write_retry(out: &mut File, bytes: &[u8]) -> io::Result<bool> {
    let mut written = 0;
    let deadline = Instant::now() + Duration::from_secs(1);
    while written < bytes.len() {
        match out.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "serial output closed",
                ))
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut pfd = libc::pollfd {
                    fd: out.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(false);
                }
                let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
                let rc = unsafe { libc::poll(&mut pfd, 1, timeout) };
                if rc < 0 {
                    return Err(io::Error::last_os_error());
                }
                if rc == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "serial output stalled before record deadline",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn record(out: &mut File, event: &str, data: &str) -> io::Result<()> {
    let line = format!(
        "{{\"ts\":{},\"event\":{},\"data\":{}}}\n",
        json_string(&format!("{:?}", SystemTime::now())),
        json_string(event),
        json_string(&bounded(data))
    );
    let bytes = line.len();
    let critical = matches!(
        event,
        "kernel"
            | "mounts"
            | "ready"
            | "health"
            | "complete"
            | "blocked_stack"
            | "probe_error"
            | "kernel_mismatch"
            | "capability_error"
            | "module_load"
    );
    let (used, limit, dropped) = if event == "probe" {
        (&TRACE_BYTES, TRACE_BUDGET, &DROPPED_TRACE)
    } else if critical {
        (&CRITICAL_BYTES, CRITICAL_BUDGET, &DROPPED_META)
    } else {
        (&META_BYTES, META_BUDGET, &DROPPED_META)
    };
    let previous = used.fetch_add(bytes, Ordering::Relaxed);
    if !within_budget(previous, bytes, limit) {
        used.fetch_sub(bytes, Ordering::Relaxed);
        dropped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    if !write_retry(out, line.as_bytes())? {
        DROPPED_WRITE.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

#[derive(Default)]
struct TraceCorrelator {
    by_task: HashMap<u64, VecDeque<u64>>,
    active: HashMap<u64, u64>,
    active_order: VecDeque<(u64, u64)>,
    next_generation: u64,
    returns: u64,
    completions: u64,
    request_ends: u64,
    unmatched_returns: u64,
    unmatched_completions: u64,
    successful_returns: u64,
    error_returns: u64,
    pointer_reuses: u64,
    evictions: u64,
}

impl TraceCorrelator {
    fn observe(&mut self, line: &str) {
        let task = line.split_whitespace().next().unwrap_or_default();
        let tid = task
            .rsplit_once('-')
            .map(|(_, n)| n)
            .and_then(|n| n.parse().ok());
        let req = line
            .split("req=0x")
            .nth(1)
            .and_then(|n| u64::from_str_radix(n.split_whitespace().next()?, 16).ok());
        if line.contains("bootc_virtiofs_enqueue:") {
            if let (Some(tid), Some(req)) = (tid, req) {
                let queue = self.by_task.entry(tid).or_default();
                queue.push_back(req);
                if queue.len() > 1024 {
                    queue.pop_front();
                }
                self.next_generation += 1;
                let generation = self.next_generation;
                if self.active.insert(req, generation).is_some() {
                    self.pointer_reuses += 1;
                }
                self.active_order.push_back((generation, req));
                while self.active.len() > MAX_ACTIVE_REQUESTS {
                    let Some((oldest_generation, oldest_req)) = self.active_order.pop_front()
                    else {
                        break;
                    };
                    if self.active.get(&oldest_req) == Some(&oldest_generation) {
                        self.active.remove(&oldest_req);
                        self.evictions += 1;
                    }
                }
            }
        } else if line.contains("bootc_virtiofs_enqueue_ret:") {
            self.returns += 1;
            match line
                .split("ret=")
                .nth(1)
                .and_then(|value| value.split_whitespace().next())
                .and_then(parse_return_value)
            {
                Some(value) if value < 0 => self.error_returns += 1,
                Some(_) => self.successful_returns += 1,
                None => {}
            }
            if tid
                .and_then(|tid| self.by_task.get_mut(&tid).and_then(VecDeque::pop_back))
                .is_none()
            {
                self.unmatched_returns += 1;
            }
        } else if line.contains("bootc_virtiofs_completion:") {
            self.completions += 1;
            if let Some(req) = req {
                if self.active.remove(&req).is_none() {
                    self.unmatched_completions += 1;
                }
            } else {
                self.unmatched_completions += 1;
            }
        } else if line.contains("bootc_fuse_request_end:") {
            self.request_ends += 1;
            if let Some(req) = req {
                self.active.remove(&req);
            }
        }
    }
    fn summary(&self) -> String {
        format!("returns={} successful_returns={} error_returns={} completions={} request_ends={} active={} unmatched_returns={} unmatched_completions={} pointer_reuses={} evictions={}", self.returns, self.successful_returns, self.error_returns, self.completions, self.request_ends, self.active.len(), self.unmatched_returns, self.unmatched_completions, self.pointer_reuses, self.evictions)
    }

    fn oldest_active_samples(&self, limit: usize) -> String {
        self.active_order
            .iter()
            .filter(|(generation, req)| self.active.get(req) == Some(generation))
            .take(limit)
            .map(|(_, req)| format!("0x{req:x}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Default)]
struct TraceFramer {
    pending: String,
}
impl TraceFramer {
    fn push(&mut self, bytes: &[u8], correlator: &mut TraceCorrelator) {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        while let Some(pos) = self.pending.find('\n') {
            let line = self.pending[..pos].to_owned();
            self.pending.drain(..=pos);
            correlator.observe(&line);
        }
        if self.pending.len() > RECORD_MAX {
            self.pending.clear();
        }
    }
}

fn read_bounded(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let mut bytes = Vec::with_capacity(RECORD_MAX + 1);
    match File::open(path)
        .and_then(|file| file.take((RECORD_MAX + 1) as u64).read_to_end(&mut bytes))
    {
        Ok(_) => bounded(String::from_utf8_lossy(&bytes)),
        Err(e) => format!("unavailable: {e}"),
    }
}

fn notify(message: &[u8]) -> io::Result<()> {
    let socket = match std::env::var("NOTIFY_SOCKET") {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let socket = socket
        .strip_prefix('@')
        .map_or(socket.clone(), |s| format!("\0{s}"));
    UnixDatagram::unbound()?
        .send_to(message, socket)
        .map(|_| ())
}

fn tracefs() -> Option<PathBuf> {
    ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"]
        .iter()
        .map(Path::new)
        .find(|p| p.join("kprobe_events").exists())
        .map(Path::to_owned)
}

/// Create a tracefs instance without touching a directory we did not create.
fn create_trace_instance(path: &Path) -> io::Result<()> {
    fs::create_dir(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("creating dedicated trace instance {path:?}: {e}"),
        )
    })
}

fn install_probes(out: &mut File) -> io::Result<(Option<PathBuf>, Vec<String>)> {
    let Some(base) = tracefs() else {
        record(out, "probe_error", "tracefs/kprobe_events unavailable")?;
        return Ok((None, vec![]));
    };
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // tracefs limits instance/event names; keep both names comfortably below
    // that limit while still avoiding stale instances from a prior run.
    let short_nonce = nonce & 0xffff_ffff;
    let group = format!("bootc_{}_{}", std::process::id(), short_nonce);
    let dir = base
        .join("instances")
        .join(format!("bootc-{}-{}", std::process::id(), short_nonce));
    if let Err(e) = create_trace_instance(&dir) {
        record(out, "probe_error", &e.to_string())?;
        return Ok((None, vec![]));
    }
    if let Err(e) = fs::write(dir.join("tracing_on"), b"1") {
        record(
            out,
            "probe_error",
            &format!("enabling dedicated trace instance {dir:?}: {e}"),
        )?;
        let _ = fs::remove_dir(&dir);
        return Ok((None, vec![]));
    }
    if fs::read_to_string(dir.join("tracing_on"))
        .map(|value| value.trim() != "1")
        .unwrap_or(true)
    {
        record(
            out,
            "probe_error",
            &format!("dedicated trace instance {dir:?} did not enable tracing"),
        )?;
        let _ = fs::remove_dir(&dir);
        return Ok((None, vec![]));
    }
    // Do not use read_bounded here: truncating this index could falsely report
    // a valid symbol as absent and turn a complete probe set into a partial one.
    let available = match fs::read_to_string(base.join("available_filter_functions")) {
        Ok(s) => s,
        Err(e) => {
            record(
                out,
                "probe_error",
                &format!("reading available_filter_functions: {e}"),
            )?;
            let _ = fs::remove_dir(&dir);
            return Ok((None, vec![]));
        }
    };
    let kallsyms = fs::read_to_string("/proc/kallsyms").unwrap_or_default();
    let symbol_present = |symbol: &str| {
        available.lines().any(|line| line.trim() == symbol)
            || kallsyms
                .lines()
                .any(|line| line.split_whitespace().nth(2) == Some(symbol))
    };
    for (name, _, symbol) in PROBES {
        if !symbol_present(symbol) {
            record(
                out,
                "probe_error",
                &format!("{name}: symbol {symbol} absent from tracefs index and kallsyms"),
            )?;
            let _ = fs::remove_dir(&dir);
            return Ok((None, vec![]));
        }
    }
    let mut installed = Vec::new();
    for (name, definition, _symbol) in PROBES {
        // kprobe definitions are global to tracefs.  The instance gets its
        // own enable state and trace buffer, so use a unique event group and
        // never write definitions through the instance's pseudo-file.
        let event = definition
            .split_once(':')
            .and_then(|(_, rest)| rest.split_once(' '))
            .map(|(event, args)| format!("{}:{}/{} {}", &definition[..1], group, event, args))
            .unwrap_or_else(|| (*definition).to_owned());
        match OpenOptions::new()
            .write(true)
            .open(base.join("kprobe_events"))
            .and_then(|mut f| writeln!(f, "{event}"))
        {
            Ok(()) => {
                let event_name = definition
                    .split(':')
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .unwrap_or(name);
                let qualified_name = format!("{group}/{event_name}");
                let enable = dir.join(format!("events/{qualified_name}/enable"));
                match fs::write(&enable, b"1") {
                    Ok(())
                        if fs::read_to_string(&enable)
                            .map(|value| value.trim() == "1")
                            .unwrap_or(false) =>
                    {
                        installed.push(qualified_name)
                    }
                    Ok(()) => {
                        record(
                            out,
                            "probe_error",
                            &format!("{name}: enable verification failed"),
                        )?;
                        let _ = fs::write(&enable, b"0");
                        if let Ok(mut probes) = OpenOptions::new()
                            .write(true)
                            .open(base.join("kprobe_events"))
                        {
                            let _ = writeln!(probes, "-:{qualified_name}");
                        }
                    }
                    Err(e) => {
                        record(out, "probe_error", &format!("{name}: enable failed: {e}"))?;
                        if let Ok(mut probes) = OpenOptions::new()
                            .write(true)
                            .open(base.join("kprobe_events"))
                        {
                            let _ = writeln!(probes, "-:{qualified_name}");
                        }
                    }
                }
            }
            Err(e) => record(out, "probe_error", &format!("{name}: install failed: {e}"))?,
        }
    }
    if installed.is_empty() {
        let _ = fs::remove_dir(&dir);
        return Ok((None, vec![]));
    }
    record(out, "probe_status", &format!("source_rev={PROBE_SOURCE_REV} instance={} installed={installed:?} correlation=request_pointer+thread_id" , dir.display()))?;
    Ok((Some(dir), installed))
}

fn remove_probes(dir: Option<&Path>, installed: &[String]) {
    let Some(dir) = dir else { return };
    let Some(base) = dir.parent().and_then(Path::parent) else {
        return;
    };
    for name in installed {
        let _ = fs::write(dir.join(format!("events/{name}/enable")), b"0");
    }
    if let Ok(mut f) = OpenOptions::new()
        .write(true)
        .open(base.join("kprobe_events"))
    {
        for name in installed {
            let _ = writeln!(f, "-:{name}");
        }
    }
    let _ = fs::remove_dir(dir);
}

fn state_letter(status: &str) -> Option<char> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("State:")?.trim_start().chars().next())
}

fn collect_stacks(out: &mut File) -> io::Result<()> {
    let Ok(processes) = fs::read_dir("/proc") else {
        record(out, "stack_error", "cannot enumerate /proc")?;
        return Ok(());
    };
    for process in processes.flatten().filter(|e| {
        e.file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
    }) {
        let tasks = process.path().join("task");
        let Ok(tasks) = fs::read_dir(tasks) else {
            continue;
        };
        for task in tasks.flatten() {
            let tid = task.file_name();
            let status = read_bounded(task.path().join("status"));
            let state = state_letter(&status);
            let comm = read_bounded(task.path().join("comm"));
            let interested = comm.contains("bootc") || comm.contains("skopeo");
            if matches!(state, Some('D' | 'K' | 'W')) || (state == Some('S') && interested) {
                let stack = read_bounded(task.path().join("stack"));
                let wchan = read_bounded(task.path().join("wchan"));
                record(
                    out,
                    "blocked_stack",
                    &format!(
                        "pid={} tid={} comm={} state={:?} wchan={}\n{}",
                        process.file_name().to_string_lossy(),
                        tid.to_string_lossy(),
                        comm.trim(),
                        state,
                        wchan.trim(),
                        stack
                    ),
                )?;
            }
        }
    }
    Ok(())
}

fn trace_buffer_stats(dir: &Path) -> String {
    let mut result = String::new();
    let Ok(cpus) = fs::read_dir(dir.join("per_cpu")) else {
        return "unavailable".to_owned();
    };
    for cpu in cpus.flatten() {
        let stats = cpu.path().join("stats");
        if let Ok(data) = fs::read_to_string(stats) {
            let line = data
                .lines()
                .filter(|line| {
                    line.contains("entries")
                        || line.contains("overrun")
                        || line.contains("commit_overrun")
                })
                .collect::<Vec<_>>()
                .join(";");
            if !line.is_empty() {
                result.push_str(&format!("{}:{} ", cpu.file_name().to_string_lossy(), line));
            }
        }
    }
    if result.is_empty() {
        "unavailable".to_owned()
    } else {
        bounded(result)
    }
}

fn capability(probe_count: usize, trace_open: bool) -> String {
    if probe_count == PROBES.len() && trace_open {
        "probes_installed=complete trace_reader_open=true trace_lossless=false".to_owned()
    } else if probe_count == 0 {
        "probes_installed=none trace_reader_open=false trace_lossless=false".to_owned()
    } else {
        format!("probes_installed=partial trace_reader_open={trace_open} trace_lossless=false")
    }
}

fn filesystem_stats(path: &Path) -> String {
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs initializes the provided struct when it returns zero;
    // the path is a fixed, NUL-terminated string supplied by CString.
    let path = match std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
        Ok(path) => path,
        Err(_) => return "invalid_path".to_owned(),
    };
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return format!("unavailable: {}", io::Error::last_os_error());
    }
    // SAFETY: the successful statvfs call initialized stats.
    let stats = unsafe { stats.assume_init() };
    format!(
        "free_blocks={} free_inodes={}",
        stats.f_bavail, stats.f_favail
    )
}

fn workload_running() -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return false;
    };
    processes.flatten().any(|process| {
        fs::read_to_string(process.path().join("cmdline"))
            .map(|cmdline| {
                cmdline.contains("bootc")
                    && (cmdline.contains("install") || cmdline.contains("to-disk"))
            })
            .unwrap_or(false)
    })
}

fn irq_mode_from_fwcfg() -> Option<irq_affinity::Mode> {
    if !Path::new("/sys/hypervisor/type").exists()
        && !read_bounded("/proc/cpuinfo").contains("hypervisor")
    {
        return None;
    }
    match irq_affinity::mode_from_fwcfg(Path::new(
        "/sys/firmware/qemu_fw_cfg/by_name/opt/bootc-debug/irq-mode/raw",
    )) {
        Ok(mode) => mode,
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("guest IRQ opt-in read failed: {e}");
            None
        }
    }
}

fn drain_irq_logs(out: &mut File, rx: &std::sync::mpsc::Receiver<String>) -> io::Result<()> {
    while let Ok(line) = rx.try_recv() {
        record(out, "guest_irq", &line)?;
    }
    Ok(())
}

fn drain(
    file: &mut Option<File>,
    event: &str,
    limit: usize,
    out: &mut File,
    framer: &mut TraceFramer,
    correlator: &mut TraceCorrelator,
    error_samples: &mut u8,
) -> io::Result<()> {
    let Some(file) = file else { return Ok(()) };
    for _ in 0..limit {
        let mut buf = [0u8; 16 * 1024];
        match file.read(&mut buf) {
            Ok(n) if n > 0 => {
                if event == "probe" {
                    framer.push(&buf[..n], correlator);
                    if *error_samples < MAX_ERROR_SAMPLES {
                        if let Some(line) =
                            String::from_utf8_lossy(&buf[..n]).lines().find(|line| {
                                line.contains("bootc_virtiofs_enqueue_ret:")
                                    && line.contains("ret=0xffff")
                            })
                        {
                            record(out, "probe_error_sample", line)?;
                            *error_samples += 1;
                        }
                    }
                } else {
                    record(out, event, &String::from_utf8_lossy(&buf[..n]))?;
                }
            }
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => {
                record(out, "read_error", &format!("{event}: {e}"))?;
                break;
            }
        }
    }
    Ok(())
}

fn poll_inputs(trace: &Option<File>, kmsg: &Option<File>) -> io::Result<()> {
    let mut active = [trace, kmsg]
        .iter()
        .filter_map(|file| {
            file.as_ref().map(|file| libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
        })
        .collect::<Vec<_>>();
    if active.is_empty() {
        thread::sleep(Duration::from_millis(10));
        return Ok(());
    }
    let rc = unsafe { libc::poll(active.as_mut_ptr(), active.len() as libc::nfds_t, 10) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn main() -> io::Result<()> {
    TRACE_BYTES.store(0, Ordering::Relaxed);
    META_BYTES.store(0, Ordering::Relaxed);
    CRITICAL_BYTES.store(0, Ordering::Relaxed);
    DROPPED_TRACE.store(0, Ordering::Relaxed);
    DROPPED_META.store(0, Ordering::Relaxed);
    DROPPED_WRITE.store(0, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(30);
    let path = Path::new("/dev/virtio-ports/org.bootc.debug");
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    let mut out = loop {
        match OpenOptions::new().write(true).open(path) {
            Ok(f) => break f,
            Err(e) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(100));
                if e.kind() != io::ErrorKind::NotFound {
                    continue;
                }
            }
            Err(e) => return Err(io::Error::new(e.kind(), format!("opening {path:?}: {e}"))),
        }
    };
    let mut flags = unsafe { libc::fcntl(out.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    flags |= libc::O_NONBLOCK;
    if unsafe { libc::fcntl(out.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    record(
        &mut out,
        "kernel",
        &format!(
            "release={} cmdline={} kallsyms_bytes={} tracefs={}",
            read_bounded("/proc/sys/kernel/osrelease"),
            read_bounded("/proc/cmdline"),
            read_bounded("/proc/kallsyms").len(),
            tracefs().is_some()
        ),
    )?;
    record(&mut out, "mounts", &read_bounded("/proc/self/mountinfo"))?;
    let kernel_release = read_bounded("/proc/sys/kernel/osrelease").trim().to_owned();
    // virtiofs is commonly a module.  The first early-boot snapshot can race
    // module loading, so explicitly request it and retry probe installation
    // before advertising readiness.
    let mut probes = Vec::new();
    let mut trace_dir = None;
    if kernel_release != EXPECTED_KERNEL {
        record(
            &mut out,
            "kernel_mismatch",
            &format!("expected={EXPECTED_KERNEL} actual={kernel_release}"),
        )?;
    } else {
        for attempt in 0..10 {
            let (dir, found) = install_probes(&mut out)?;
            trace_dir = dir;
            probes = found;
            if probes.len() == PROBES.len() {
                break;
            }
            remove_probes(trace_dir.as_deref(), &probes);
            trace_dir = None;
            probes.clear();
            if attempt == 0 {
                match Command::new("modprobe").arg("virtiofs").output() {
                    Ok(result) => record(
                        &mut out,
                        "module_load",
                        &format!("virtiofs status={}", result.status),
                    )?,
                    Err(e) => record(&mut out, "module_load", &format!("virtiofs failed: {e}"))?,
                }
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
    let mut kmsg = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/kmsg")
        .ok();
    if kmsg.is_none() {
        record(
            &mut out,
            "capability_error",
            "/dev/kmsg unavailable; kernel log coverage is absent",
        )?;
    }
    let mut trace = trace_dir.as_ref().and_then(|d| {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(d.join("trace_pipe"))
            .ok()
    });
    if !probes.is_empty() && trace.is_none() {
        record(
            &mut out,
            "capability_error",
            "trace_pipe unavailable; installed probes cannot be collected",
        )?;
    }
    let capability = capability(probes.len(), trace.is_some());
    let (irq_tx, irq_rx) = std::sync::mpsc::channel();
    let irq_worker = irq_mode_from_fwcfg().map(|mode| {
        let tx = irq_tx.clone();
        thread::spawn(move || {
            let config = irq_affinity::Config {
                sysfs: "/sys".into(),
                procfs: "/proc".into(),
                online: "/sys/devices/system/cpu/online".into(),
                mode,
                duration: Duration::from_secs(90),
                interval: Duration::from_secs(1),
            };
            if let Err(e) = irq_affinity::run(&config, |line| {
                let _ = tx.send(line);
            }) {
                let _ = tx.send(format!("error={e}"));
            }
        })
    });
    drop(irq_tx);
    drain_irq_logs(&mut out, &irq_rx)?;
    record(&mut out, "ready", &format!("collector=initialized {capability} blocked_stacks=true kmsg={} source_rev={PROBE_SOURCE_REV}", kmsg.is_some()))?;
    if let Some(dir) = trace_dir.as_deref() {
        record(
            &mut out,
            "trace_config",
            &format!(
                "instance={} current_tracer={} tracing_on={} buffer_size_kb={}",
                dir.display(),
                read_bounded(dir.join("current_tracer")),
                read_bounded(dir.join("tracing_on")),
                read_bounded(dir.join("buffer_size_kb"))
            ),
        )?;
    }
    notify(format!("READY=1\nSTATUS=virtio-serial diagnostics; {capability}").as_bytes())?;
    let start = Instant::now();
    let mut framer = TraceFramer::default();
    let mut correlator = TraceCorrelator::default();
    let mut next_health = Instant::now();
    let mut next_stacks = Instant::now();
    let mut workload_seen = false;
    let mut error_samples = 0;
    while start.elapsed() < RUN_FOR {
        if start.elapsed() >= RUN_FOR {
            break;
        }
        drain_irq_logs(&mut out, &irq_rx)?;
        drain(
            &mut trace,
            "probe",
            256,
            &mut out,
            &mut framer,
            &mut correlator,
            &mut error_samples,
        )?;
        drain(
            &mut kmsg,
            "kmsg",
            32,
            &mut out,
            &mut framer,
            &mut correlator,
            &mut error_samples,
        )?;
        if Instant::now() >= next_stacks {
            collect_stacks(&mut out)?;
            next_stacks = Instant::now() + STACK_INTERVAL;
        }
        if !workload_seen && workload_running() {
            workload_seen = true;
            record(
                &mut out,
                "workload_start",
                "bootc install/to-disk process observed",
            )?;
        }
        if Instant::now() >= next_health {
            record(&mut out, "health", &format!("{} oldest_active=[{}] workload_seen={} trace_stats={} rootfs={} varfs={} dropped_trace={} dropped_meta={} dropped_write={} bytes_trace={} bytes_meta={} bytes_critical={}", correlator.summary(), correlator.oldest_active_samples(8), workload_seen, trace_dir.as_deref().map(trace_buffer_stats).unwrap_or_else(|| "unavailable".to_owned()), filesystem_stats(Path::new("/")), filesystem_stats(Path::new("/var")), DROPPED_TRACE.load(Ordering::Relaxed), DROPPED_META.load(Ordering::Relaxed), DROPPED_WRITE.load(Ordering::Relaxed), TRACE_BYTES.load(Ordering::Relaxed), META_BYTES.load(Ordering::Relaxed), CRITICAL_BYTES.load(Ordering::Relaxed)))?;
            record(&mut out, "mounts", &read_bounded("/proc/self/mountinfo"))?;
            record(
                &mut out,
                "proc",
                &format!(
                    "load={} uptime={}",
                    read_bounded("/proc/loadavg"),
                    read_bounded("/proc/uptime")
                ),
            )?;
            next_health = Instant::now() + Duration::from_secs(3);
        }
        poll_inputs(&trace, &kmsg)?;
    }
    remove_probes(trace_dir.as_deref(), &probes);
    if let Some(worker) = irq_worker {
        let _ = worker.join();
    }
    drain_irq_logs(&mut out, &irq_rx)?;
    record(
        &mut out,
        "complete",
        &format!("duration_limit={RUN_FOR:?} workload_seen={workload_seen} probes={probes:?} correlation={} dropped_trace={} dropped_meta={} dropped_write={} bytes_trace={} bytes_meta={} bytes_critical={}", correlator.summary(), DROPPED_TRACE.load(Ordering::Relaxed), DROPPED_META.load(Ordering::Relaxed), DROPPED_WRITE.load(Ordering::Relaxed), TRACE_BYTES.load(Ordering::Relaxed), META_BYTES.load(Ordering::Relaxed), CRITICAL_BYTES.load(Ordering::Relaxed)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaping_is_json_safe() {
        assert_eq!(json_string("a\n\"\\\t"), r#""a\n\"\\\t""#);
    }
    #[test]
    fn bounds_are_by_characters() {
        assert_eq!(
            bounded("é".repeat(RECORD_MAX + 1)).chars().count(),
            RECORD_MAX
        );
    }
    #[test]
    fn trace_framer_handles_partial_lines_and_cross_thread_completion() {
        let mut framer = TraceFramer::default();
        let mut c = TraceCorrelator::default();
        framer.push(
            b"worker-a-10 [0] bootc_virtiofs_enqueue: req=0xabc\n",
            &mut c,
        );
        framer.push(b"worker-a-10 [0] bootc_virtiofs_enqueue_ret: ret=0\nworker-b-20 [1] bootc_virtiofs_completion: req=0xabc\n", &mut c);
        assert_eq!(c.returns, 1);
        assert_eq!(c.successful_returns, 1);
        assert_eq!(c.completions, 1);
        assert_eq!(c.unmatched_completions, 0);
        assert!(c.active.is_empty());
    }
    #[test]
    fn trace_correlation_bounds_reused_requests() {
        let mut c = TraceCorrelator::default();
        c.observe("worker-10 [0] bootc_virtiofs_enqueue: req=0xabc");
        c.observe("worker-11 [0] bootc_virtiofs_enqueue: req=0xabc");
        assert_eq!(c.pointer_reuses, 1);
        c.observe("worker-11 [0] bootc_virtiofs_enqueue_ret: ret=-12");
        assert_eq!(c.error_returns, 1);
        c.observe("worker-11 [0] bootc_virtiofs_completion: req=0xdead");
        assert_eq!(c.unmatched_completions, 1);
    }
    #[test]
    fn trace_correlation_evicts_in_constant_work_and_terminal_end_crosses_threads() {
        let mut c = TraceCorrelator::default();
        for req in 0..(MAX_ACTIVE_REQUESTS as u64 + 100) {
            c.observe(&format!(
                "worker-10 [0] bootc_virtiofs_enqueue: req=0x{req:x}"
            ));
        }
        assert_eq!(c.active.len(), MAX_ACTIVE_REQUESTS);
        assert_eq!(c.evictions, 100);
        assert_eq!(c.oldest_active_samples(1), format!("0x{:x}", 100_u64));

        c.observe("worker-99 [1] bootc_fuse_request_end: req=0x100");
        assert_eq!(c.active.len(), MAX_ACTIVE_REQUESTS - 1);
        assert!(!c
            .oldest_active_samples(MAX_ACTIVE_REQUESTS)
            .split(',')
            .any(|sample| sample == "0x100"));
    }
    #[test]
    fn return_values_accept_trace_hex_and_signed_decimal() {
        assert_eq!(parse_return_value("0x0"), Some(0));
        assert_eq!(parse_return_value("0xfffffffffffffff4"), Some(-12));
        assert_eq!(parse_return_value("-12"), Some(-12));
    }
    #[test]
    fn readiness_does_not_claim_collection_without_reader() {
        assert_eq!(
            capability(PROBES.len(), false),
            "probes_installed=partial trace_reader_open=false trace_lossless=false"
        );
        assert!(capability(0, false).contains("probes_installed=none"));
    }

    #[test]
    fn create_trace_instance_creates_once_and_preserves_existing_directory() {
        let parent = std::env::temp_dir().join(format!(
            "bootc-debug-collector-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&parent).unwrap();
        let instance = parent.join("instance");

        create_trace_instance(&instance).unwrap();
        assert!(instance.is_dir());
        let error = create_trace_instance(&instance).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(instance.is_dir());

        fs::remove_dir(&instance).unwrap();
        fs::remove_dir(&parent).unwrap();
    }
    #[test]
    fn proc_status_state_fixtures_use_the_state_letter() {
        let fixtures = [
            ("Name:\tworker\nState:\tD (disk sleep)\n", Some('D')),
            ("Name:\tbootc\nState:\tS (sleeping)\n", Some('S')),
            ("Name:\tworker\nState:\tK (wakekill)\n", Some('K')),
            ("Name:\tworker\nState:\tR (running)\n", Some('R')),
        ];
        for (status, expected) in fixtures {
            assert_eq!(state_letter(status), expected);
        }
    }
    #[test]
    fn trace_budget_exhaustion_is_a_drop_and_collection_can_continue() {
        assert!(!within_budget(TRACE_BUDGET, 1, TRACE_BUDGET));
        assert!(within_budget(0, 1, TRACE_BUDGET));
    }
}
