//! One deadline for a short administrative command and every descendant that
//! keeps its output pipes open. The hardware daemon never uses this.
use std::{
    io::{self, Read},
    os::fd::{AsRawFd, RawFd},
    process::{Child, ExitStatus},
    thread,
    time::{Duration, Instant},
};

pub(crate) enum Failure {
    Io(io::Error),
    Timeout,
    Oversized,
}

pub(crate) struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    /// At most the output limit; the rest is read and discarded.
    pub stderr: Vec<u8>,
}

/// Collect a child spawned with `process_group(0)`. Piped stdout past `limit`
/// fails; piped stderr is truncated. Any failure kills the whole group.
pub(crate) fn collect(
    mut child: Child,
    timeout: Duration,
    limit: usize,
) -> Result<Output, Failure> {
    let result = read_until_exit(&mut child, timeout, limit);
    if result.is_err() {
        // SAFETY: kill takes plain integers. The child leads its own process
        // group, so this reaches it and descendants still holding its pipes.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        // Without its own group the direct child must still not block wait().
        // std skips the signal once the child has been reaped.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn read_until_exit(child: &mut Child, timeout: Duration, limit: usize) -> Result<Output, Failure> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    for fd in [
        stdout.as_ref().map(AsRawFd::as_raw_fd),
        stderr.as_ref().map(AsRawFd::as_raw_fd),
    ]
    .into_iter()
    .flatten()
    {
        nonblocking(fd).map_err(Failure::Io)?;
    }
    let deadline = Instant::now() + timeout;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut status = None;
    loop {
        if Instant::now() >= deadline {
            return Err(Failure::Timeout);
        }
        if let Some(pipe) = &mut stdout
            && drain(pipe, &mut out, limit, true)?
        {
            stdout = None;
        }
        if let Some(pipe) = &mut stderr
            && drain(pipe, &mut err, limit, false)?
        {
            stderr = None;
        }
        if status.is_none() {
            status = child.try_wait().map_err(Failure::Io)?;
        }
        if let Some(status) = status
            && stdout.is_none()
            && stderr.is_none()
        {
            return Ok(Output {
                status,
                stdout: out,
                stderr: err,
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Read what is available now. A bounded number of reads keeps a flooding
/// child from holding the loop past its deadline. Returns true at EOF.
fn drain(
    pipe: &mut impl Read,
    bytes: &mut Vec<u8>,
    limit: usize,
    fatal: bool,
) -> Result<bool, Failure> {
    let mut buffer = [0; 8192];
    for _ in 0..16 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let remaining = limit - bytes.len();
                if fatal && count > remaining {
                    return Err(Failure::Oversized);
                }
                bytes.extend_from_slice(&buffer[..count.min(remaining)]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            Err(error) => return Err(Failure::Io(error)),
        }
    }
    Ok(false)
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl only reads the status flags of this live, owned pipe.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above; the only change is adding O_NONBLOCK.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    #[test]
    fn failure_kills_a_descendant_that_outlives_the_shell() {
        let pid_file =
            std::env::temp_dir().join(format!("minibookx-process-{}", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30 & echo $! > \"$0\"")
            .arg(&pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let result = collect(command.spawn().unwrap(), Duration::from_millis(200), 64);
        assert!(matches!(result, Err(Failure::Timeout)));
        let pid = fs::read_to_string(&pid_file).unwrap();
        fs::remove_file(&pid_file).unwrap();
        // Dead means gone or a zombie awaiting its new parent's reap.
        let stat = format!("/proc/{}/stat", pid.trim());
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs::read_to_string(&stat).is_ok_and(|text| {
            text.rsplit_once(") ")
                .is_some_and(|(_, rest)| !rest.starts_with('Z'))
        }) {
            assert!(
                Instant::now() < deadline,
                "the descendant holding stdout survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
