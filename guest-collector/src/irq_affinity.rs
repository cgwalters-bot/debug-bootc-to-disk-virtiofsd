//! Narrow, explicitly guest-only virtio-fs IRQ affinity experiment.
use std::{
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub const VIRTIO_ID_FS: u16 = 26; // include/uapi/linux/virtio_ids.h
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Fixed,
    Move,
}
#[derive(Clone, Debug)]
pub struct Config {
    pub sysfs: PathBuf,
    pub procfs: PathBuf,
    pub online: PathBuf,
    pub mode: Mode,
    pub duration: Duration,
    pub interval: Duration,
}
struct Irq {
    number: u32,
    action: String,
    affinity: PathBuf,
    effective: PathBuf,
    original: String,
}

fn pci_name(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && b[..4].iter().all(u8::is_ascii_hexdigit)
        && b[5..7].iter().all(u8::is_ascii_hexdigit)
        && b[8..10].iter().all(u8::is_ascii_hexdigit)
        && b[11].is_ascii_hexdigit()
}
fn online_cpus(path: &Path) -> io::Result<[u32; 2]> {
    let mut cpus = Vec::new();
    for part in fs::read_to_string(path)?.trim().split(',') {
        let (a, b) = part.split_once('-').map_or((part, part), |(a, b)| (a, b));
        let a: u32 = a
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad CPU list"))?;
        let b: u32 = b
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad CPU list"))?;
        cpus.extend(a..=b);
    }
    cpus.get(..2)
        .map(|x| [x[0], x[1]])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "need two online CPUs"))
}
fn action_is_safe(action: &str, device: &str) -> bool {
    let a = action.to_ascii_lowercase();
    !a.contains("hiprio")
        && !a.contains("config")
        && a.split(|c: char| c.is_whitespace() || c == ',')
            .any(|name| {
                let prefix = format!("{device}-requests.");
                name.strip_prefix(&prefix)
                    .is_some_and(|q| !q.is_empty() && q.bytes().all(|b| b.is_ascii_digit()))
            })
}
fn interrupt_action(procfs: &Path, number: u32) -> io::Result<String> {
    let prefix = format!("{number}:");
    fs::read_to_string(procfs.join("interrupts"))?
        .lines()
        .find(|line| line.trim_start().starts_with(&prefix))
        .map(|line| line.trim_start()[prefix.len()..].trim().to_owned())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("IRQ {number} absent from /proc/interrupts"),
            )
        })
}
pub fn mode_from_fwcfg(path: &Path) -> io::Result<Option<Mode>> {
    match fs::read_to_string(path)?.trim() {
        "fixed" => Ok(Some(Mode::Fixed)),
        "move" => Ok(Some(Mode::Move)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown fw_cfg IRQ mode {other:?}"),
        )),
    }
}
fn irq_paths(c: &Config) -> io::Result<Vec<Irq>> {
    let mut out = Vec::new();
    for e in fs::read_dir(c.sysfs.join("bus/virtio/devices"))? {
        let e = e?;
        let n = e.file_name();
        let n = n.to_string_lossy();
        if !n.starts_with("virtio") {
            continue;
        }
        let id_text = fs::read_to_string(e.path().join("device"))?;
        let id = id_text.trim().trim_start_matches("0x");
        if id
            .parse::<u16>()
            .or_else(|_| u16::from_str_radix(id, 16))
            .ok()
            != Some(VIRTIO_ID_FS)
        {
            continue;
        }
        // Bus entries are symlinks; canonicalizing is what exposes the real
        // PCI ancestry instead of the /sys/bus/virtio view.
        let mut p = fs::canonicalize(e.path())?;
        let pci = loop {
            if pci_name(p.file_name().unwrap_or_default().to_string_lossy().as_ref())
                && p.join("msi_irqs").is_dir()
            {
                break p;
            }
            if !p.pop() {
                break PathBuf::new();
            }
        };
        if pci.as_os_str().is_empty() {
            continue;
        }
        for q in fs::read_dir(pci.join("msi_irqs"))?.flatten() {
            let number: u32 = match q.file_name().to_string_lossy().parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let proc = c.procfs.join("irq").join(number.to_string());
            let action = fs::read_to_string(
                c.sysfs
                    .join("kernel/irq")
                    .join(number.to_string())
                    .join("actions"),
            )
            .or_else(|_| interrupt_action(&c.procfs, number))
            .unwrap_or_default();
            if !action_is_safe(&action, &n) {
                continue;
            }
            let affinity = proc.join("smp_affinity_list");
            out.push(Irq {
                number,
                action,
                effective: proc.join("effective_affinity_list"),
                original: fs::read_to_string(&affinity)?,
                affinity,
            });
        }
    }
    out.sort_by_key(|i| i.number);
    out.dedup_by_key(|i| i.number);
    Ok(out)
}
pub fn run(c: &Config, mut log: impl FnMut(String)) -> io::Result<()> {
    if c.duration > Duration::from_secs(120) || c.interval < Duration::from_millis(100) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "duration must be <=120s and interval >=100ms",
        ));
    }
    let cpus = online_cpus(&c.online)?;
    let irqs = irq_paths(c)?;
    if irqs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no verified virtio-fs MSI IRQs with safe actions",
        ));
    }
    let fixed = cpus[0].to_string();
    for i in &irqs {
        log(format!(
            "irq={} original_affinity={}",
            i.number,
            i.original.trim()
        ));
    }
    let start = Instant::now();
    let mut flip = false;
    let operation = (|| -> io::Result<()> {
        let mut iteration = 0;
        while start.elapsed() < c.duration {
            if c.mode == Mode::Fixed && iteration > 0 {
                thread::sleep(c.duration.saturating_sub(start.elapsed()));
                break;
            }
            let value = if flip {
                cpus[1].to_string()
            } else {
                fixed.clone()
            };
            flip = !flip;
            iteration += 1;
            for i in &irqs {
                fs::write(&i.affinity, format!("{value}\n"))?;
                log(format!(
                    "irq={} action={} effective_affinity={}",
                    i.number,
                    i.action.trim(),
                    fs::read_to_string(&i.effective)?.trim()
                ));
            }
            thread::sleep(c.interval.min(c.duration.saturating_sub(start.elapsed())));
        }
        Ok(())
    })();
    let mut restore_error = None;
    for i in &irqs {
        if let Err(e) = fs::write(&i.affinity, &i.original) {
            restore_error.get_or_insert(e);
            continue;
        }
        log(format!(
            "irq={} restored_affinity={}",
            i.number,
            i.original.trim()
        ));
    }
    match (operation, restore_error) {
        (Err(e), _) => Err(e),
        (Ok(()), Some(e)) => Err(e),
        (Ok(()), None) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::SystemTime};
    fn fixture() -> (PathBuf, Config) {
        let r = std::env::temp_dir().join(format!(
            "irq-fixture-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let p = r.join("sys/devices/pci0000:00/0000:00:02.0");
        let v = p.join("virtio0");
        let b = r.join("sys/bus/virtio/devices");
        fs::create_dir_all(&v).unwrap();
        fs::create_dir_all(p.join("msi_irqs")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(v.join("device"), "26").unwrap();
        fs::write(p.join("msi_irqs/44"), "").unwrap();
        std::os::unix::fs::symlink(&v, b.join("virtio0")).unwrap();
        for (name, id) in [("virtio1", "1"), ("virtio2", "2")] {
            let pci = r.join(format!(
                "sys/devices/pci0000:00/0000:00:0{}.0/{name}",
                if name == "virtio1" { 3 } else { 4 }
            ));
            let other = pci;
            fs::create_dir_all(other.parent().unwrap().join("msi_irqs")).unwrap();
            fs::create_dir_all(&other).unwrap();
            fs::write(other.join("device"), id).unwrap();
            let irq = if name == "virtio1" { 45 } else { 46 };
            fs::write(other.parent().unwrap().join(format!("msi_irqs/{irq}")), "").unwrap();
            std::os::unix::fs::symlink(&other, b.join(name)).unwrap();
            let q = r.join(format!("proc/irq/{irq}"));
            fs::create_dir_all(&q).unwrap();
            fs::write(
                q.join("action"),
                if name == "virtio1" {
                    "virtio-net-Tx\n"
                } else {
                    "virtio_blk\n"
                },
            )
            .unwrap();
        }
        let q = r.join("proc/irq/44");
        fs::create_dir_all(&q).unwrap();
        fs::write(q.join("action"), "virtio0-requests.0\n").unwrap();
        fs::write(q.join("smp_affinity_list"), "3\n").unwrap();
        fs::write(q.join("effective_affinity_list"), "3\n").unwrap();
        let actions = r.join("sys/kernel/irq/44");
        fs::create_dir_all(&actions).unwrap();
        fs::write(actions.join("actions"), "virtio0-requests.0\n").unwrap();
        fs::write(r.join("proc/interrupts"), " 44: 0 virtio0-requests.0\n").unwrap();
        let online = r.join("online");
        fs::write(&online, "4-5\n").unwrap();
        (
            r.clone(),
            Config {
                sysfs: r.join("sys"),
                procfs: r.join("proc"),
                online,
                mode: Mode::Fixed,
                duration: Duration::ZERO,
                interval: Duration::from_millis(100),
            },
        )
    }
    #[test]
    fn action_filter_is_narrow() {
        assert!(!action_is_safe("virtio0-requests.0", "virtio1"));
        assert!(!action_is_safe("virtio0-requests.hiprio", "virtio0"));
        assert!(!action_is_safe("virtio0-config-requests.0", "virtio0"));
        assert!(action_is_safe("virtio0-requests.0", "virtio0"));
    }
    #[test]
    fn selects_only_verified_fs_irq() {
        let (r, c) = fixture();
        assert_eq!(irq_paths(&c).unwrap().len(), 1);
        fs::remove_dir_all(r).unwrap();
    }
    #[test]
    fn fixed_writes_once_and_restores() {
        let (r, mut c) = fixture();
        c.duration = Duration::from_millis(100);
        let mut logs = Vec::new();
        run(&c, |line| logs.push(line)).unwrap();
        assert_eq!(
            logs.iter()
                .filter(|l| l.contains("effective_affinity"))
                .count(),
            1
        );
        assert!(logs.iter().any(|l| l.contains("restored_affinity=3")));
        assert_eq!(
            fs::read_to_string(r.join("proc/irq/44/smp_affinity_list")).unwrap(),
            "3\n"
        );
        fs::remove_dir_all(r).unwrap();
    }
}
