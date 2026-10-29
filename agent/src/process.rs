//! What every child process of the agent has in common: it never sees the
//! agent's own credentials. Script stacks run vendor installers downloaded
//! at run time, and Docker, `cp` and PHP run beside them; none of them has
//! a use for the token or Wren's address.
//!
//! [`run`] is how the agent runs them: in a process group of their own that a
//! timeout kills as a whole (a script's children included), and with what they
//! print held to a fixed size, however much it is.

use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

/// The environment variables that carry the agent's credentials.
pub const SECRET_VARS: [&str; 2] = ["WW_AGENT_TOKEN", "WW_SERVER_URL"];

/// Removes [`SECRET_VARS`] from the child's environment, whether inherited
/// or set before (a script stack's `env` included). Call it last.
pub fn scrub(cmd: &mut Command) -> &mut Command {
    for var in SECRET_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// The most output kept from one stream of a child process.
pub const MAX_CAPTURE: usize = 256 * 1024;

/// Which part of a long output is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// The first [`MAX_CAPTURE`] bytes (a version command's first line).
    Head,
    /// The last [`MAX_CAPTURE`] bytes (a log: the end says what failed).
    Tail,
}

/// What a child process printed and how it ended.
#[derive(Debug)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why [`run`] has no output.
#[derive(Debug)]
pub enum RunError {
    /// The program didn't start, or its output couldn't be read.
    Start(std::io::Error),
    /// It ran out of time; it and everything it started were killed.
    TimedOut,
}

/// Runs `cmd` to its end for at most `timeout`. Its stdout and stderr are
/// read as they come, but only [`MAX_CAPTURE`] bytes of each are kept, so a
/// program that never stops printing can't fill the agent's memory. On unix
/// the child leads a process group of its own and a timeout kills the whole
/// group: a `sleep` or a daemon started by a script does not outlive it.
/// The caller sets the program, its arguments and `stdin`.
pub async fn run(cmd: &mut Command, timeout: Duration, keep: Keep) -> Result<Captured, RunError> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(RunError::Start)?;
    let pid = child.id();
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let work = async {
        let (stdout, stderr, status) =
            tokio::join!(read_capped(out, keep), read_capped(err, keep), child.wait());
        status.map(|status| Captured {
            status,
            stdout,
            stderr,
        })
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(done) => done.map_err(RunError::Start),
        Err(_) => {
            kill_group(pid).await;
            let _ = child.kill().await;
            Err(RunError::TimedOut)
        }
    }
}

/// Kills the process group led by `pid` (`kill -KILL -pid`; the agent
/// forbids unsafe code, so it asks the shell to).
async fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let mut kill = Command::new("sh");
        kill.arg("-c")
            .arg(format!("kill -KILL -{pid}"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Ok(mut child) = kill.spawn() {
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Reads `reader` to its end, keeping at most [`MAX_CAPTURE`] bytes.
async fn read_capped(reader: Option<impl AsyncRead + Unpin>, keep: Keep) -> Vec<u8> {
    let mut kept = Vec::new();
    let Some(mut reader) = reader else {
        return kept;
    };
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        match keep {
            Keep::Head => {
                let room = MAX_CAPTURE.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Keep::Tail => {
                kept.extend_from_slice(&chunk[..n]);
                if kept.len() > 2 * MAX_CAPTURE {
                    kept.drain(..kept.len() - MAX_CAPTURE);
                }
            }
        }
    }
    if keep == Keep::Tail && kept.len() > MAX_CAPTURE {
        kept.drain(..kept.len() - MAX_CAPTURE);
    }
    kept
}

#[cfg(all(test, unix))]
mod run_tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).stdin(std::process::Stdio::null());
        cmd
    }

    /// Whether process `pid` is running (a zombie is not).
    fn alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z'),
            Err(_) => false,
        }
    }

    #[tokio::test]
    async fn output_and_status_are_returned() {
        let out = run(
            &mut sh("echo out; echo err >&2; exit 3"),
            Duration::from_secs(10),
            Keep::Tail,
        )
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_children_of_the_script_too() {
        let dir = std::env::temp_dir().join(format!("wren-process-kill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("child.pid");
        // The shell starts a child and waits for it.
        let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let err = run(&mut sh(&script), Duration::from_secs(2), Keep::Tail)
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::TimedOut), "{err:?}");
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The kill is a signal away; give the kernel a moment to deliver it.
        for _ in 0..50 {
            if !alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!alive(pid), "the script's child outlived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn only_the_tail_of_a_long_output_is_kept() {
        let out = run(
            &mut sh("head -c 1000000 /dev/zero | tr '\\0' x; echo END"),
            Duration::from_secs(30),
            Keep::Tail,
        )
        .await
        .unwrap();
        assert!(out.stdout.len() <= MAX_CAPTURE, "{}", out.stdout.len());
        assert!(out.stdout.ends_with(b"xxEND\n"));
    }

    #[tokio::test]
    async fn only_the_head_of_a_long_output_is_kept() {
        let out = run(
            &mut sh("echo START; head -c 1000000 /dev/zero | tr '\\0' x"),
            Duration::from_secs(30),
            Keep::Head,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout.len(), MAX_CAPTURE);
        assert!(out.stdout.starts_with(b"START\nxx"));
    }

    #[tokio::test]
    async fn a_program_that_does_not_exist_is_a_start_error() {
        let err = run(
            &mut Command::new("/nonexistent/wren-test"),
            Duration::from_secs(5),
            Keep::Tail,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RunError::Start(_)), "{err:?}");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn shell(script: &str, scrubbed: bool) -> String {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            // As if the agent's own environment held them.
            .env("WW_AGENT_TOKEN", "wwa_secret")
            .env("WW_SERVER_URL", "https://wren.example.com")
            .env("WW_OTHER", "kept");
        if scrubbed {
            scrub(&mut cmd);
        }
        let out = cmd.output().await.unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn children_do_not_see_the_token_or_the_server() {
        let script = r#"echo "${WW_AGENT_TOKEN:-none} ${WW_SERVER_URL:-none} ${WW_OTHER:-none}""#;
        // The probe itself works: without the helper the child sees them.
        assert_eq!(
            shell(script, false).await,
            "wwa_secret https://wren.example.com kept"
        );
        assert_eq!(shell(script, true).await, "none none kept");
        assert!(!shell("env", true).await.contains("wwa_secret"));
    }

    #[tokio::test]
    async fn the_real_helpers_scrub_too() {
        // `snapshot::exec` and the script runners go through `scrub`; the
        // parent's own variables can't be changed in a test (no unsafe), so
        // check what a script stack's `env` can't bring back either.
        let env = std::collections::BTreeMap::from([(
            "WW_AGENT_TOKEN".to_string(),
            "wwa_from_toml".to_string(),
        )]);
        let out = crate::run::run_script(
            r#"echo "${WW_AGENT_TOKEN:-none}""#,
            std::path::Path::new("/"),
            &env,
            std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out.trim(), "none");
        let out = crate::discover::run(
            r#"echo "${WW_AGENT_TOKEN:-none}""#,
            std::path::Path::new("/"),
            &env,
            std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out, "none");
    }
}
