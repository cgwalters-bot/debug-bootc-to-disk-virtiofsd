#[path = "../irq_affinity.rs"]
mod irq_affinity;
use irq_affinity::{Config, Mode};
use std::{env, io, path::PathBuf, time::Duration};

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().collect();
    let value = |name: &str| args.windows(2).find(|x| x[0] == name).map(|x| x[1].clone());
    if !args.iter().any(|x| x == "--guest-confirm") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guest-irq-test requires --guest-confirm",
        ));
    }
    let mode = if args.iter().any(|x| x == "--mode-from-fwcfg") {
        irq_affinity::mode_from_fwcfg(
            PathBuf::from("/sys/firmware/qemu_fw_cfg/by_name/opt/bootc-debug/irq-mode/raw")
                .as_path(),
        )?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no guest IRQ opt-in in fw_cfg"))?
    } else {
        match value("--mode").as_deref() {
            Some("fixed") => Mode::Fixed,
            Some("move") => Mode::Move,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--mode must be fixed or move",
                ))
            }
        }
    };
    let duration: u64 = value("--duration-secs")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing --duration-secs"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad duration"))?;
    let interval: u64 = value("--interval-ms")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing --interval-ms"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad interval"))?;
    if !std::path::Path::new("/sys/hypervisor/type").exists()
        && !std::fs::read_to_string("/proc/cpuinfo")
            .unwrap_or_default()
            .contains("hypervisor")
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing: virtualization was not detected",
        ));
    }
    let c = Config {
        sysfs: PathBuf::from("/sys"),
        procfs: PathBuf::from("/proc"),
        online: PathBuf::from("/sys/devices/system/cpu/online"),
        mode,
        duration: Duration::from_secs(duration),
        interval: Duration::from_millis(interval),
    };
    irq_affinity::run(&c, |line| eprintln!("guest-irq-test: {line}"))
}
