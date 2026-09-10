use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const PORT: &str = "org.bootc.debug";
const DEFAULT_BASE: &str = "localhost/bootc-pr2290-a8df21a8-centos9-sealed-debugtools:debug";

fn usage() {
    println!("xtask build-image [--base IMAGE] [--dry-run]\nxtask to-disk --image IMAGE --bcvk PATH --output PATH [--disk-size SIZE] [--qemu PATH] [--dry-run]\nThe output disk must not already exist; disk-size defaults to 20G.");
}
fn run(mut c: Command, timeout: Duration) -> io::Result<()> {
    let mut child = c
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other(format!(
                    "command exited unsuccessfully: {status}"
                )));
            }
            return Ok(());
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
    run(c, Duration::from_secs(900))
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
    let version = Command::new(&bcvk)
        .arg("--version")
        .output()
        .ok()
        .map(|x| String::from_utf8_lossy(&x.stdout).into_owned())
        .unwrap_or_else(|| "unknown".into());
    let bcvk_sha256 = Command::new("sha256sum")
        .arg(&bcvk)
        .output()
        .ok()
        .map(|x| String::from_utf8_lossy(&x.stdout).into_owned())
        .unwrap_or_else(|| "unknown".into());
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
    run(c, Duration::from_secs(300))?;
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
        _ => {
            usage();
            Ok(())
        }
    }
}
