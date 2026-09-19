use std::{
    io::{self, Read, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, kill_process_group},
};
use zeroize::Zeroizing;

use super::{KuramaError, SecretValue, native_secret};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const IO_CHUNK_BYTES: usize = 4096;

pub(super) fn read(service: &str, account: &str) -> Result<SecretValue, KuramaError> {
    let bytes = run(
        Command::new("secret-tool").args(["lookup", "service", service, "account", account]),
        None,
        COMMAND_TIMEOUT,
    )?;
    native_secret(&bytes, true)
}

pub(super) fn write(service: &str, account: &str, secret: &SecretValue) -> Result<(), KuramaError> {
    run(
        Command::new("secret-tool").args([
            "store",
            "--label=Kurama",
            "service",
            service,
            "account",
            account,
        ]),
        Some(secret.expose().as_bytes()),
        COMMAND_TIMEOUT,
    )?;
    Ok(())
}

// Own the process group until all pipe work completes, including when the leader
// exits first. Drop cleans up errors and unwinding without spawning reader threads.
// Abrupt parent termination cannot run Drop; process groups are not crash containment.
struct NativeChild {
    child: Child,
    group: Pid,
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        let _ = kill_process_group(self.group, Signal::Kill);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn failure(message: &'static str) -> KuramaError {
    KuramaError::Configuration(message.into())
}

fn nonblocking(fd: &impl AsFd) -> Result<(), KuramaError> {
    let flags = fcntl_getfl(fd).map_err(|_| failure("native keychain pipe setup failed"))?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)
        .map_err(|_| failure("native keychain pipe setup failed"))
}

fn run(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Zeroizing<Vec<u8>>, KuramaError> {
    let started = Instant::now();
    command
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if input.is_none() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null());
    let child = command
        .spawn()
        .map_err(|_| failure("native keychain command failed to start"))?;
    let group = Pid::from_child(&child);
    let mut child = NativeChild { child, group };
    let mut stdin = child.child.stdin.take();
    let mut stdout = child.child.stdout.take();
    if let Some(stdin) = &stdin {
        nonblocking(stdin)?;
    }
    if let Some(stdout) = &stdout {
        nonblocking(stdout)?;
    }
    // A fixed-capacity secret buffer avoids leaving old, unzeroized allocations
    // behind when an incrementally accumulated credential would otherwise grow.
    let mut output = Zeroizing::new(Vec::with_capacity(if stdout.is_some() {
        MAX_OUTPUT_BYTES
    } else {
        0
    }));
    let mut chunk = Zeroizing::new([0_u8; IO_CHUNK_BYTES]);
    let mut written = 0;
    loop {
        if started.elapsed() >= timeout {
            return Err(failure("native keychain command timed out"));
        }
        let mut progressed = false;
        if let Some(pipe) = stdin.as_mut() {
            let bytes = input.expect("piped stdin has input");
            if written == bytes.len() {
                stdin.take();
                progressed = true;
            } else {
                let end = written.saturating_add(IO_CHUNK_BYTES).min(bytes.len());
                match pipe.write(&bytes[written..end]) {
                    Ok(0) => return Err(failure("native keychain command closed stdin")),
                    Ok(count) => {
                        written += count;
                        progressed = true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err(failure("native keychain command stdin failed")),
                }
            }
        }
        if let Some(pipe) = stdout.as_mut() {
            match pipe.read(&mut chunk[..]) {
                Ok(0) => {
                    stdout.take();
                    progressed = true;
                }
                Ok(count) => {
                    if count > MAX_OUTPUT_BYTES - output.len() {
                        return Err(failure("native keychain output exceeds limit"));
                    }
                    output.extend_from_slice(&chunk[..count]);
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(failure("native keychain command stdout failed")),
            }
        }
        // Do not reap the group leader before draining pipes: descendants may
        // still hold them. The same deadline covers that drain.
        if stdin.is_none()
            && stdout.is_none()
            && let Some(status) = child
                .child
                .try_wait()
                .map_err(|_| failure("native keychain command wait failed"))?
        {
            return if status.success() {
                Ok(output)
            } else {
                Err(failure("native keychain command failed"))
            };
        }
        if !progressed {
            std::thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};

    fn fake_tool(directory: &Path, script: &str) -> Command {
        let path = directory.join("fake-secret-tool.sh");
        fs::write(&path, script).unwrap();
        let mut command = Command::new("/bin/sh");
        command.arg(path);
        command
    }

    fn assert_reaped(pid_file: &Path) {
        let pid = fs::read_to_string(pid_file).unwrap();
        assert!(
            !Path::new("/proc").join(pid.trim()).exists(),
            "direct child was not reaped"
        );
    }

    fn assert_not_running(pid_file: &Path) {
        let pid = fs::read_to_string(pid_file).unwrap();
        let stat = Path::new("/proc").join(pid.trim()).join("stat");
        let started = Instant::now();
        loop {
            match fs::read_to_string(&stat) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Ok(value)
                    if value
                        .rsplit_once(") ")
                        .is_some_and(|(_, fields)| fields.starts_with('Z')) =>
                {
                    return;
                }
                _ => {}
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "descendant still running"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn write_pipes_all_secret_bytes_once_without_arguments_or_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fake_tool(
            directory.path(),
            "printf '%s\\n' \"$@\" > \"$1/args\"\ncat > \"$1/input\"\nprintf 'secret-output'\nprintf 'secret-error' >&2\n",
        );
        command.arg(directory.path()).args([
            "store",
            "service",
            "test-service",
            "account",
            "test-account",
        ]);
        let input = Zeroizing::new("test-owned-secret\n".repeat(8192));
        let output = run(&mut command, Some(input.as_bytes()), Duration::from_secs(2)).unwrap();
        assert!(output.is_empty());
        assert_eq!(
            fs::read(directory.path().join("input")).unwrap(),
            input.as_bytes()
        );
        assert!(
            !fs::read_to_string(directory.path().join("args"))
                .unwrap()
                .contains("test-owned-secret")
        );
    }

    #[test]
    fn lookup_decodes_cli_output_and_rejects_non_utf8_without_disclosure() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fake_tool(directory.path(), "printf 'test-owned-secret\\r\\n'");
        let bytes = run(&mut command, None, Duration::from_secs(2)).unwrap();
        assert_eq!(
            native_secret(&bytes, true).unwrap().expose(),
            "test-owned-secret"
        );
        let mut command = fake_tool(directory.path(), "printf 'test-owned-secret\\377'");
        let bytes = run(&mut command, None, Duration::from_secs(2)).unwrap();
        let error = native_secret(&bytes, true).unwrap_err();
        assert!(!format!("{error:?}").contains("test-owned-secret"));
    }

    #[test]
    fn lookup_output_limit_accepts_exact_limit_and_rejects_overflow() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fake_tool(directory.path(), "printf '%065536d' 0");
        let bytes = run(&mut command, None, Duration::from_secs(2)).unwrap();
        assert_eq!(bytes.as_slice(), "0".repeat(MAX_OUTPUT_BYTES).as_bytes());
        let mut command = fake_tool(
            directory.path(),
            "printf '%s' $$ > \"$1\"\nprintf '%065537d' 0\nsleep 30",
        );
        let pid_file = directory.path().join("pid");
        command.arg(&pid_file);
        let error = run(&mut command, None, Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("exceeds limit"));
        assert_reaped(&pid_file);
    }

    #[test]
    fn nonzero_exit_discards_secret_diagnostics_and_reaps_child() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fake_tool(
            directory.path(),
            "printf '%s' $$ > \"$1\"\nprintf 'test-owned-secret'\nprintf 'test-owned-secret' >&2\nexit 7",
        );
        let pid_file = directory.path().join("pid");
        command.arg(&pid_file);
        let error = run(&mut command, None, Duration::from_secs(2)).unwrap_err();
        assert!(!error.to_string().contains("test-owned-secret"));
        assert!(!format!("{error:?}").contains("test-owned-secret"));
        assert_reaped(&pid_file);
    }

    #[test]
    fn early_stdin_close_fails_and_reaps_child() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fake_tool(
            directory.path(),
            "printf '%s' $$ > \"$1\"\nexec 0<&-\nsleep 30",
        );
        let pid_file = directory.path().join("pid");
        command.arg(&pid_file);
        let input = Zeroizing::new(vec![b'x'; 1024 * 1024]);
        let error = run(&mut command, Some(&input), Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("stdin"));
        assert_reaped(&pid_file);
    }

    #[test]
    fn deadline_covers_blocked_stdin_and_stdout_and_terminates_descendants() {
        for input in [None, Some(vec![b'x'; 1024 * 1024])] {
            let directory = tempfile::tempdir().unwrap();
            let mut command = fake_tool(
                directory.path(),
                "printf '%s' $$ > \"$1\"\nsleep 30 &\nprintf '%s' $! > \"$2\"\nwait",
            );
            let pid_file = directory.path().join("pid");
            let descendant = directory.path().join("descendant");
            command.arg(&pid_file).arg(&descendant);
            let started = Instant::now();
            let error =
                run(&mut command, input.as_deref(), Duration::from_millis(250)).unwrap_err();
            assert!(error.to_string().contains("timed out"));
            assert!(started.elapsed() < Duration::from_secs(3));
            assert_reaped(&pid_file);
            assert_not_running(&descendant);
        }
    }

    #[test]
    fn deadline_covers_descendant_pipe_after_leader_exits() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fake_tool(
            directory.path(),
            "printf '%s' $$ > \"$1\"\nsleep 30 &\nprintf '%s' $! > \"$2\"\nexit 0",
        );
        let pid_file = directory.path().join("pid");
        let descendant = directory.path().join("descendant");
        command.arg(&pid_file).arg(&descendant);
        let started = Instant::now();
        let error = run(&mut command, None, Duration::from_millis(250)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_reaped(&pid_file);
        assert_not_running(&descendant);
    }

    #[test]
    fn successful_leader_does_not_leave_background_descendant_running() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fake_tool(
            directory.path(),
            "printf '%s' $$ > \"$1\"\nsleep 30 </dev/null >/dev/null 2>&1 &\nprintf '%s' $! > \"$2\"\nexit 0",
        );
        let pid_file = directory.path().join("pid");
        let descendant = directory.path().join("descendant");
        command.arg(&pid_file).arg(&descendant);
        assert!(
            run(&mut command, None, Duration::from_secs(2))
                .unwrap()
                .is_empty()
        );
        assert_reaped(&pid_file);
        assert_not_running(&descendant);
    }
}
