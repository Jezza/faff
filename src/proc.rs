//! Running external commands (jj, wezterm) with a deadline.
//!
//! `Command::output` waits forever, and a wedged wezterm mux or a jj stuck on a lock would
//! stall faff's engine with it. [`output`] reads the child's pipes on helper threads and
//! kills it once the deadline passes, so a hang becomes an ordinary error.

use std::fmt::Display;
use std::io::{self, Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Run `cmd` to completion with stdout/stderr captured and stdin closed, killing it if it
/// is still running after `timeout`.
pub fn output(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    run(cmd, None, timeout)
}

/// Like [`output`], but feed `input` to the child's stdin (then close it).
pub fn output_with_input(cmd: &mut Command, input: &[u8], timeout: Duration) -> io::Result<Output> {
    run(cmd, Some(input), timeout)
}

/// A failed [`output`] as one readable error. faff shows errors with `{e}`, which prints
/// only the outermost message, so the cause (not found, timed out) is folded into it.
pub fn failure(what: impl Display, hint: &str, e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::NotFound {
        anyhow::anyhow!("{what}: not found ({hint})")
    } else {
        anyhow::anyhow!("{what}: {e}")
    }
}

fn run(cmd: &mut Command, input: Option<&[u8]>, timeout: Duration) -> io::Result<Output> {
    let stdin = if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = cmd
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Every pipe is serviced on its own thread: a child blocked writing a full stdout
    // pipe, or reading stdin we haven't written yet, can't then deadlock us.
    let writer = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut stdin)) => {
            let bytes = bytes.to_vec();
            Some(thread::spawn(move || stdin.write_all(&bytes)))
        }
        _ => None,
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = match wait_until(&mut child, Instant::now() + timeout)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out after {}s", timeout.as_secs()),
            ));
        }
    };
    if let Some(w) = writer {
        // A child that exits without reading all its input is not our failure.
        let _ = w.join();
    }
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

/// Poll `child` until it exits or `deadline` passes. The poll interval starts at 1ms, so
/// the common few-millisecond jj call isn't padded out, and backs off to 25ms.
fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Option<std::process::ExitStatus>> {
    let mut nap = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep(nap.min(deadline - now));
        nap = (nap * 2).min(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout_and_stderr() {
        let out = output(
            Command::new("sh").args(["-c", "echo out; echo err >&2"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn feeds_stdin() {
        let out = output_with_input(&mut Command::new("cat"), b"hello", Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn stdin_is_closed_rather_than_inherited() {
        // `cat` with an inherited terminal would block forever; with stdin closed it exits.
        let out = output(&mut Command::new("cat"), Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn a_hung_child_is_killed_at_the_deadline() {
        let start = Instant::now();
        let err = output(
            Command::new("sleep").arg("30"),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5), "returned promptly");
    }
}
