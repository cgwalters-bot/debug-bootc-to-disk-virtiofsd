mod supervisor;

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use supervisor::run as supervise;

const PORT: &str = "org.bootc.debug";
const DEFAULT_BASE: &str = "localhost/bootc-pr2290-a8df21a8-centos9-sealed-debugtools:debug";
const TO_DISK_TIMEOUT: Duration = Duration::from_secs(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const AUX_TIMEOUT: Duration = Duration::from_secs(10);

fn usage() {
    println!("xtask build-image [--base IMAGE] [--dry-run]\nxtask to-disk --image IMAGE --bcvk PATH --output PATH [--disk-size SIZE] [--qemu PATH] [--dry-run]\nxtask probe --image IMAGE --bcvk PATH --run-dir DIR [--timeout-secs N] [--execute]\nThe output disk must not already exist; disk-size defaults to 20G. probe plans only unless --execute is supplied.");
}
fn run(mut c: Command, timeout: Duration, log: PathBuf) -> io::Result<()> {
    let outcome = supervise(&mut c, timeout, &log)?;
    if outcome.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "command exited unsuccessfully: {}; log={}",
            outcome.status,
            outcome.log.display()
        )))
    }
}
fn log_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/xtask-logs")
        .join(format!(
            "{name}-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
}

#[derive(Debug)]
struct Probe {
    bcvk: String,
    image: String,
    run_dir: PathBuf,
    timeout: Duration,
    name: String,
    label: String,
}
fn option(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|x| x[0] == name).map(|x| x[1].clone())
}
fn probe(args: &[String]) -> io::Result<Probe> {
    let required = |name| {
        option(args, name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required"))
        })
    };
    let run_dir = PathBuf::from(required("--run-dir")?);
    if !run_dir.is_absolute() || !run_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--run-dir must be an existing absolute directory",
        ));
    }
    let secs = option(args, "--timeout-secs")
        .map(|s| {
            s.parse::<u64>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--timeout-secs must be an integer",
                )
            })
        })
        .transpose()?
        .unwrap_or(PROBE_TIMEOUT.as_secs());
    if !(45..=60).contains(&secs) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--timeout-secs must be 45 through 60",
        ));
    }
    let name = format!(
        "bootc-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    Ok(Probe {
        bcvk: required("--bcvk")?,
        image: required("--image")?,
        run_dir,
        timeout: Duration::from_secs(secs),
        label: format!("org.bootc.xtask.probe={name}"),
        name,
    })
}
fn launch_args(p: &Probe) -> Vec<String> {
    let mut args = vec![
        "ephemeral".into(),
        "run".into(),
        "-K".into(),
        "--name".into(),
        p.name.clone(),
        "--label".into(),
        p.label.clone(),
        "--console".into(),
        "--log-dir".into(),
        format!("console={}", p.run_dir.display()),
    ];
    if let Ok(qemu) = env::var("BCVK_QEMU") {
        args.extend(["--qemu".into(), qemu]);
    }
    args.push(p.image.clone());
    args
}
fn cleanup_args(p: &Probe) -> Vec<String> {
    vec!["rm".into(), "-f".into(), p.name.clone()]
}
fn ownership_inspect_args(p: &Probe) -> Vec<String> {
    vec![
        "inspect".into(),
        "--format".into(),
        "{\"Name\":{{json .Name}},\"Labels\":{{json .Config.Labels}}}".into(),
        p.name.clone(),
    ]
}
fn container_logs_args(p: &Probe) -> Vec<String> {
    vec!["logs".into(), p.name.clone()]
}
fn container_exists_args(p: &Probe) -> Vec<String> {
    vec!["container".into(), "exists".into(), p.name.clone()]
}
fn probe_log(p: &Probe, stage: &str) -> PathBuf {
    p.run_dir.join(format!("{}-{stage}.log", p.name))
}
fn ensure_name_absent(p: &Probe) -> io::Result<()> {
    let mut inspect = Command::new("podman");
    inspect.args(container_exists_args(p));
    let log = probe_log(p, "preflight-exists");
    let result = supervise(&mut inspect, AUX_TIMEOUT, &log)?;
    if result.status.code() == Some(1) {
        return Ok(());
    }
    let reason = if result.status.success() {
        "container name already exists"
    } else {
        "podman container exists failed"
    };
    Err(io::Error::new(
        if result.status.success() {
            io::ErrorKind::AlreadyExists
        } else {
            io::ErrorKind::Other
        },
        format!("refusing probe: {reason}; log={}", log.display()),
    ))
}
#[derive(Deserialize)]
struct OwnedIdentity {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Labels")]
    labels: BTreeMap<String, String>,
}
impl OwnedIdentity {
    fn matches_probe(&self, name: &str) -> bool {
        self.name.strip_prefix('/').unwrap_or(&self.name) == name
            && self.labels.get("org.bootc.xtask.probe").map(String::as_str) == Some(name)
    }
}
fn read_owned_identity(log: &Path) -> io::Result<OwnedIdentity> {
    fs::read_to_string(log)?
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| {
            io::Error::other(format!(
                "inspect did not return limited ownership JSON; log={}",
                log.display()
            ))
        })
}
fn capture_container_logs(p: &Probe) -> io::Result<()> {
    let mut logs = Command::new("podman");
    logs.args(container_logs_args(p));
    let log = probe_log(p, "container-logs");
    let result = supervise(&mut logs, AUX_TIMEOUT, &log)?;
    if result.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "podman logs failed; log={}",
            log.display()
        )))
    }
}
fn cleanup(p: &Probe) -> io::Result<()> {
    let mut inspect = Command::new("podman");
    inspect.args(ownership_inspect_args(p));
    let log = probe_log(p, "cleanup-inspect");
    let result = supervise(&mut inspect, AUX_TIMEOUT, &log)?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "owned-container inspect failed; log={}",
            log.display()
        )));
    }
    let identity = read_owned_identity(&log)?;
    if !identity.matches_probe(&p.name) {
        return Err(io::Error::other(format!(
            "refusing cleanup: inspect did not verify exact owned name and label; log={}",
            log.display()
        )));
    }
    let mut remove = Command::new("podman");
    remove.args(cleanup_args(p));
    run(remove, AUX_TIMEOUT, probe_log(p, "cleanup-rm"))
}
fn run_probe(args: &[String]) -> io::Result<()> {
    let p = probe(args)?;
    let command = launch_args(&p);
    println!(
        "planned owned probe name={} label={} timeout={}s\n{:?} {:?}",
        p.name,
        p.label,
        p.timeout.as_secs(),
        p.bcvk,
        command
    );
    if !args.iter().any(|arg| arg == "--execute") {
        return Ok(());
    }
    println!(
        "logs: {}, {}, {}, {}, {}",
        probe_log(&p, "preflight-exists").display(),
        probe_log(&p, "launch").display(),
        probe_log(&p, "container-logs").display(),
        probe_log(&p, "cleanup-inspect").display(),
        probe_log(&p, "cleanup-rm").display()
    );
    ensure_name_absent(&p)?;
    let mut launch = Command::new(&p.bcvk);
    launch.args(command);
    let launch_result = run(launch, p.timeout, probe_log(&p, "launch"));
    let logs_result = capture_container_logs(&p);
    let cleanup_result = cleanup(&p);
    if let Err(error) = &logs_result {
        eprintln!("Container log capture failed: {error}");
    }
    if let Err(error) = &cleanup_result {
        eprintln!("Container cleanup failed for {}: {error}", p.name);
    }
    launch_result.and(logs_result).and(cleanup_result)
}
fn image(dry: bool, base: Option<String>) -> io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let tag = "localhost/bootc-debug-virtiofsd:debug";
    let mut c = Command::new("podman");
    c.args([
        "build",
        "--tag",
        tag,
        "--build-arg",
        &format!("BASE_IMAGE={}", base.unwrap_or_else(|| DEFAULT_BASE.into())),
        ".",
    ]);
    let keydir = env::var("BOOTC_SECUREBOOT_DIR").unwrap_or_else(|_| {
        format!(
            "{}/src/github/bootc-dev/bootc/target/test-secureboot",
            env::var("HOME").unwrap_or_else(|_| "/var/home/sandbox-walters".into())
        )
    });
    c.args([
        "--cap-add=all",
        "--security-opt=label=type:container_runtime_t",
        "--device=/dev/fuse",
        &format!("--secret=id=secureboot_key,src={keydir}/db.key"),
        &format!("--secret=id=secureboot_cert,src={keydir}/db.crt"),
    ]);
    c.current_dir(root);
    if dry {
        println!("{c:?}");
        return Ok(());
    }
    run(c, Duration::from_secs(900), log_path("build-image"))
}
fn to_disk(args: &[String], dry: bool) -> io::Result<()> {
    let get = |name: &str| args.windows(2).find(|x| x[0] == name).map(|x| x[1].clone());
    let image = get("--image")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--image is required"))?;
    let bcvk = get("--bcvk").unwrap_or_else(|| "~/.local/bin/bcvk".into());
    let bcvk = if bcvk == "~/.local/bin/bcvk" {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".local/bin/bcvk")
            .to_string_lossy()
            .into_owned()
    } else {
        bcvk
    };
    let output = PathBuf::from(
        get("--output")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--output is required"))?,
    );
    let disk_size = get("--disk-size").unwrap_or_else(|| "20G".into());
    let qemu = get("--qemu");
    if output.exists() || output.as_os_str().to_string_lossy().starts_with("/dev/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output must be a new disposable regular path",
        ));
    }
    let capture = output.with_extension("virtio-serial.log");
    let capture_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&capture)?;
    let metadata = output.with_extension("run.txt");
    let version = "recorded in command log";
    let bcvk_sha256 = "not collected";
    let text = format!(
        "image={image}\nbcvk={bcvk}\nbcvk-version={version}\nbcvk-sha256={bcvk_sha256}\nqemu={}\noutput={output:?}\ndisk-size={disk_size}\nmemory=4G\nvcpus=32\nfilesystem=ext4\ncomposefs-backend=true\nbootloader=systemd\nport={PORT}\n",
        qemu.as_deref().unwrap_or("system-default")
    );
    fs::write(&metadata, text)?;
    let mut c = Command::new(&bcvk);
    c.args([
        "to-disk",
        &image,
        output.to_str().unwrap(),
        "--filesystem",
        "ext4",
        "--disk-size",
        &disk_size,
        "--memory",
        "4G",
        "--vcpus",
        "32",
        "--composefs-backend",
        "--bootloader",
        "systemd",
        "--virtio-serial-out",
        &format!("{PORT}:{}", capture.display()),
    ]);
    if let Some(qemu) = qemu {
        c.arg("--qemu").arg(qemu);
    }
    use std::os::fd::AsRawFd;
    if !dry {
        let fd = capture_file.as_raw_fd();
        if fd != 3 {
            unsafe {
                libc::dup2(fd, 3);
            }
        }
        unsafe {
            libc::fcntl(3, libc::F_SETFD, 0);
        }
        c.env("BCVK_SERIAL_FDS", format!("{PORT}:3"));
    }
    if dry {
        println!("{c:?}");
        return Ok(());
    }
    run(c, TO_DISK_TIMEOUT, log_path("to-disk"))?;
    let data = fs::read_to_string(&capture).unwrap_or_default();
    if !data.contains("\"event\":\"ready\"") {
        return Err(io::Error::other(format!(
            "missing collector ready record; capture: {}",
            capture.display()
        )));
    }
    if !data.contains("probes_installed=complete trace_reader_open=true") {
        return Err(io::Error::other(format!(
            "trace validation failed: collector did not report complete probes; capture: {}",
            capture.display()
        )));
    }
    Ok(())
}
fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().collect();
    let dry = args.iter().any(|x| x == "--dry-run");
    match args.get(1).map(String::as_str) {
        Some("build-image") => image(
            dry,
            args.windows(2)
                .find(|x| x[0] == "--base")
                .map(|x| x[1].clone()),
        ),
        Some("to-disk") => to_disk(&args, dry),
        Some("probe") => run_probe(&args),
        _ => {
            usage();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_accepts_podman_and_prefixed_names_but_requires_the_label() {
        let name = "bootc-probe-test";
        for (reported_name, label, expected) in [
            (name, name, true),
            ("/bootc-probe-test", name, true),
            ("//bootc-probe-test", name, false),
            ("bootc-probe-test-other", name, false),
            (name, "another-probe", false),
            (name, "", false),
        ] {
            let identity = OwnedIdentity {
                name: reported_name.into(),
                labels: BTreeMap::from([("org.bootc.xtask.probe".into(), label.into())]),
            };
            assert_eq!(identity.matches_probe(name), expected);
        }
        let unlabelled = OwnedIdentity {
            name: name.into(),
            labels: BTreeMap::new(),
        };
        assert!(!unlabelled.matches_probe(name));
    }
    #[test]
    fn nonzero_command_is_propagated() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let log = std::env::temp_dir().join(format!("xtask-nonzero-{}.log", std::process::id()));
        let error = run(command, Duration::from_secs(1), log).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
    #[test]
    fn probe_construction_is_owned_and_scoped() {
        let dir = std::env::temp_dir();
        let p = Probe {
            bcvk: "bcvk".into(),
            image: "localhost/image".into(),
            run_dir: dir,
            timeout: Duration::from_secs(60),
            name: "bootc-probe-test".into(),
            label: "org.bootc.xtask.probe=bootc-probe-test".into(),
        };
        let launch = launch_args(&p);
        assert!(launch
            .windows(2)
            .any(|a| a == ["--name", "bootc-probe-test"]));
        assert!(launch
            .windows(2)
            .any(|a| a == ["--label", "org.bootc.xtask.probe=bootc-probe-test"]));
        assert_eq!(
            launch,
            [
                "ephemeral",
                "run",
                "-K",
                "--name",
                "bootc-probe-test",
                "--label",
                "org.bootc.xtask.probe=bootc-probe-test",
                "--console",
                "--log-dir",
                "console=/tmp",
                "localhost/image"
            ]
        );
        assert_eq!(
            ownership_inspect_args(&p),
            [
                "inspect",
                "--format",
                "{\"Name\":{{json .Name}},\"Labels\":{{json .Config.Labels}}}",
                "bootc-probe-test"
            ]
        );
        assert_eq!(container_logs_args(&p), ["logs", "bootc-probe-test"]);
        assert_eq!(
            container_exists_args(&p),
            ["container", "exists", "bootc-probe-test"]
        );
        assert_eq!(cleanup_args(&p), ["rm", "-f", "bootc-probe-test"]);
    }
    #[test]
    fn invalid_probe_timeout_is_rejected() {
        let args = vec![
            "xtask".into(),
            "probe".into(),
            "--image".into(),
            "i".into(),
            "--bcvk".into(),
            "b".into(),
            "--run-dir".into(),
            std::env::temp_dir().display().to_string(),
            "--timeout-secs".into(),
            "0".into(),
        ];
        assert_eq!(
            probe(&args).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    #[test]
    fn planned_probe_does_not_create_files_or_execute_bcvk() {
        let dir = std::env::temp_dir().join(format!("xtask-plan-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let args = vec![
            "xtask".into(),
            "probe".into(),
            "--image".into(),
            "i".into(),
            "--bcvk".into(),
            "/not-an-executable".into(),
            "--run-dir".into(),
            dir.display().to_string(),
        ];
        run_probe(&args).unwrap();
        assert!(fs::read_dir(&dir).unwrap().next().is_none());
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn to_disk_keeps_its_long_timeout() {
        assert_eq!(TO_DISK_TIMEOUT, Duration::from_secs(300));
    }
}
