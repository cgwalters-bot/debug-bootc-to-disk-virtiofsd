use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const TERM_GRACE: Duration = Duration::from_secs(5);
pub const KILL_REAP: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct Outcome {
    pub status: ExitStatus,
    pub pid: u32,
    pub pgid: i32,
    pub elapsed: Duration,
    pub log: PathBuf,
}

fn timeout_error(pid: u32, pgid: i32, log: &Path, detail: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "command timed out; pid={pid} pgid={pgid}; {detail}; log={}",
            log.display()
        ),
    )
}

fn signal_group(pgid: i32, signal: i32) -> io::Result<()> {
    if pgid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to signal unsafe process group",
        ));
    }
    // SAFETY: pgid is the child's PID.  spawn() created that new process group,
    // so a negative value can target only this invocation and its descendants.
    if unsafe { libc::kill(-pgid, signal) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn child_group(pid: u32) -> io::Result<Option<i32>> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("child PID exceeds i32"))?;
    // SAFETY: pid came directly from the Child we just spawned; getpgid only
    // reads kernel process state and does not signal or otherwise modify it.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid > 1 {
        Ok(Some(pgid))
    } else if pgid == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn poll(child: &mut std::process::Child, until: Instant) -> io::Result<Option<ExitStatus>> {
    while Instant::now() < until {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.try_wait()
}
fn kill_unvalidated_child(child: &mut std::process::Child, pid: i32) {
    // SAFETY: pid belongs to the still-owned Child and is targeted directly;
    // unlike a process group this cannot affect unrelated descendants.
    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = poll(child, Instant::now() + KILL_REAP);
}

/// Run a command with a wall-clock deadline.  Its output never inherits the
/// caller's pipes, so a daemon descendant cannot keep a cargo/API pipe open.
pub fn run(command: &mut Command, timeout: Duration, log: &Path) -> io::Result<Outcome> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timeout must be non-zero",
        ));
    }
    let parent = log
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut record = OpenOptions::new().create_new(true).write(true).open(log)?;
    writeln!(record, "command: {command:?}")?;
    let output: File = record.try_clone()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(record));
    // A zero pgid asks setpgid(0, 0): the child becomes a new group leader.
    command.process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();
    let start = Instant::now();
    let expected_pgid =
        i32::try_from(pid).map_err(|_| io::Error::other("child PID exceeds i32"))?;
    let pgid = match child_group(pid) {
        Err(error) => {
            kill_unvalidated_child(&mut child, expected_pgid);
            append_failure(log, start.elapsed(), pid, "process-group validation failed")?;
            return Err(io::Error::other(format!(
                "could not validate child process group: pid={pid}; {error}"
            )));
        }
        Ok(Some(pgid)) if pgid == expected_pgid => pgid,
        Ok(Some(pgid)) => {
            kill_unvalidated_child(&mut child, expected_pgid);
            append_failure(log, start.elapsed(), pid, "process-group mismatch")?;
            return Err(io::Error::other(format!(
                "child process group mismatch: pid={pid} pgid={pgid}"
            )));
        }
        Ok(None) => {
            if let Some(status) = child.try_wait()? {
                let outcome = Outcome {
                    status,
                    pid,
                    pgid: -1,
                    elapsed: start.elapsed(),
                    log: log.into(),
                };
                append(&outcome, "exited before process-group validation")?;
                return Ok(outcome);
            }
            append_failure(
                log,
                start.elapsed(),
                pid,
                "child disappeared before process-group validation",
            )?;
            return Err(io::Error::other(format!(
                "child disappeared before process-group validation: pid={pid}"
            )));
        }
    };
    if let Some(status) = poll(&mut child, start + timeout)? {
        let outcome = Outcome {
            status,
            pid,
            pgid,
            elapsed: start.elapsed(),
            log: log.into(),
        };
        append(&outcome, "exited")?;
        return Ok(outcome);
    }
    let term = signal_group(pgid, libc::SIGTERM).err();
    if let Some(status) = poll(&mut child, Instant::now() + TERM_GRACE)? {
        let outcome = Outcome {
            status,
            pid,
            pgid,
            elapsed: start.elapsed(),
            log: log.into(),
        };
        append(&outcome, "timed out after TERM")?;
        return Err(timeout_error(
            pid,
            pgid,
            log,
            format!("TERM result={term:?}"),
        ));
    }
    let kill = signal_group(pgid, libc::SIGKILL).err();
    if let Some(status) = poll(&mut child, Instant::now() + KILL_REAP)? {
        let outcome = Outcome {
            status,
            pid,
            pgid,
            elapsed: start.elapsed(),
            log: log.into(),
        };
        append(&outcome, "timed out after KILL")?;
        return Err(timeout_error(
            pid,
            pgid,
            log,
            format!("TERM result={term:?}; KILL result={kill:?}"),
        ));
    }
    append_unreaped(log, start.elapsed(), pid, pgid)?;
    Err(timeout_error(pid, pgid, log, format!("TERM result={term:?}; KILL result={kill:?}; unreaped process (possibly uninterruptible D state)")))
}

fn append(outcome: &Outcome, result: &str) -> io::Result<()> {
    let mut log = OpenOptions::new().append(true).open(&outcome.log)?;
    writeln!(
        log,
        "result: {result}; status={}; elapsed_ms={}; pid={}; pgid={}",
        outcome.status,
        outcome.elapsed.as_millis(),
        outcome.pid,
        outcome.pgid
    )
}
fn append_unreaped(log: &Path, elapsed: Duration, pid: u32, pgid: i32) -> io::Result<()> {
    let mut log = OpenOptions::new().append(true).open(log)?;
    writeln!(
        log,
        "result: timed out; status=unreaped; elapsed_ms={}; pid={pid}; pgid={pgid}",
        elapsed.as_millis()
    )
}
fn append_failure(log: &Path, elapsed: Duration, pid: u32, result: &str) -> io::Result<()> {
    let mut log = OpenOptions::new().append(true).open(log)?;
    writeln!(
        log,
        "result: {result}; elapsed_ms={}; pid={pid}",
        elapsed.as_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn log(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "xtask-supervisor-{name}-{}-{}.log",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn fast_success_is_reaped() {
        let result = run(&mut sh("exit 0"), Duration::from_secs(1), &log("ok")).unwrap();
        assert!(result.status.success());
    }
    #[test]
    fn nonzero_exit_is_reported() {
        let result = run(&mut sh("exit 7"), Duration::from_secs(1), &log("bad")).unwrap();
        assert_eq!(result.status.code(), Some(7));
    }
    #[test]
    fn ignores_term_then_group_is_killed() {
        let start = Instant::now();
        let error = run(
            &mut sh("trap '' TERM; while :; do sleep 30 & wait; done"),
            Duration::from_secs(1),
            &log("term"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(9));
        let pgid = error
            .to_string()
            .split("pgid=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(poll_group_gone(pgid, Duration::from_secs(1)));
    }
    #[test]
    fn descendant_cannot_hold_callers_output_pipe() {
        let start = Instant::now();
        let result = run(
            &mut sh("sleep 2 & exit 0"),
            Duration::from_secs(1),
            &log("descendant"),
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(stop_group(result.pgid, Duration::from_secs(1)).unwrap());
    }
    #[test]
    fn zero_timeout_is_rejected() {
        assert_eq!(
            run(&mut sh("exit 0"), Duration::ZERO, &log("zero"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    fn poll_group_gone(pgid: i32, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            // SAFETY: this is the test's supervisor-created process group.
            if unsafe { libc::kill(-pgid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
    fn stop_group(pgid: i32, timeout: Duration) -> io::Result<bool> {
        signal_group(pgid, libc::SIGKILL)?;
        Ok(poll_group_gone(pgid, timeout))
    }
}
