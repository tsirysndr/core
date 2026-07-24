use anyhow::{Context, Result};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Gid, Pid, Uid, User, getgrouplist, setgid, setgroups, setuid};
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct Spec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    pub timeout: Option<Duration>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl Spec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            timeout: None,
            uid: None,
            gid: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn envs<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env.extend(
            env.into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn run_as(mut self, uid: u32, gid: u32) -> Self {
        self.uid = Some(uid);
        self.gid = Some(gid);
        self
    }
}

#[derive(Clone, Debug)]
pub struct ExitResult {
    pub exit_code: i32,
    pub error: Option<String>,
    pub timed_out: bool,
}

#[derive(Clone, Debug)]
pub struct CaptureOutput {
    pub exit: ExitResult,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CaptureOutput {
    pub fn success(&self) -> bool {
        self.exit.exit_code == 0 && self.exit.error.is_none()
    }

    pub fn combined_lossy(&self) -> String {
        let mut data = self.stdout.clone();
        data.extend_from_slice(&self.stderr);
        String::from_utf8_lossy(&data).trim().to_owned()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum OutKind {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug)]
pub struct OutData {
    pub data: Vec<u8>,
    pub kind: OutKind,
}

pub struct StreamingCommand {
    events: Receiver<OutData>,
    exit: JoinHandle<Result<ExitResult>>,
}

impl StreamingCommand {
    pub fn into_parts(self) -> (Receiver<OutData>, JoinHandle<Result<ExitResult>>) {
        (self.events, self.exit)
    }
}

pub async fn run_capture(spec: Spec) -> Result<CaptureOutput> {
    let running = spawn_streaming(spec)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let (mut events, exit_task) = running.into_parts();

    while let Some(event) = events.recv().await {
        match event.kind {
            OutKind::Stdout => stdout.extend_from_slice(&event.data),
            OutKind::Stderr => stderr.extend_from_slice(&event.data),
        }
    }

    let exit = exit_task
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("command supervisor failed: {error}")))?;
    Ok(CaptureOutput {
        exit,
        stdout,
        stderr,
    })
}

fn read_oom_kill_count() -> u32 {
    let content = match std::fs::read_to_string("/proc/vmstat") {
        Ok(c) => c,
        Err(_) => return 0,
    };
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("oom_kill") {
            if let Some(count_str) = parts.next() {
                if let Ok(count) = count_str.parse::<u32>() {
                    return count;
                }
            }
        }
    }
    0
}

pub fn spawn_streaming(mut spec: Spec) -> Result<StreamingCommand> {
    let oom_kill_before = read_oom_kill_count();
    let mut child = spawn(&mut spec)?;
    let stdout = child.stdout.take().context("stdout pipe missing")?;
    let stderr = child.stderr.take().context("stderr pipe missing")?;

    let (events_tx, events_rx) = mpsc::channel(64);
    let stdout_thread = spawn_reader(stdout, events_tx.clone(), OutKind::Stdout);
    let stderr_thread = spawn_reader(stderr, events_tx.clone(), OutKind::Stderr);
    drop(events_tx);

    let exit = tokio::spawn(async move {
        let exit = wait_child(&mut child, spec.timeout, oom_kill_before).await;

        // ensure all output is observed before exiting
        // this assumes children dont daemonize and hold onto the stdout/err
        stdout_thread.await.context("stdout reader task failed")?;
        stderr_thread.await.context("stderr reader task failed")?;

        Ok(exit)
    });

    Ok(StreamingCommand {
        events: events_rx,
        exit,
    })
}

fn spawn(spec: &mut Spec) -> Result<Child> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }

    // don't use rust's .uid() / .gid() methods here because they clear
    // supplemantary groups, which means for example adding a user to "docker"
    // group won't actually let it access the sock.
    // https://github.com/rust-lang/rust/issues/90747
    if let (Some(uid), Some(gid)) = (spec.uid, spec.gid) {
        let username = User::from_uid(Uid::from_raw(uid))
            .ok()
            .flatten()
            .map(|u| u.name)
            .with_context(|| format!("lookup passwd entry for uid {uid}"))?;
        let cname = CString::new(username)
            .with_context(|| format!("username for uid {uid} contained a null byte"))?;
        // resolve groups beforehand so we don't have to read /etc/group in the pre_exec
        let groups =
            getgrouplist(&cname, Gid::from_raw(gid)).context("resolve supplementary groups")?;
        // SAFETY: pre_exec runs between fork and execve in the child.
        // we only call async-signal-safe syscalls and we don't touch any
        // shared state, no allocator, no mutexes, no globals.
        unsafe {
            cmd.pre_exec(move || {
                setgroups(&groups).map_err(io::Error::from)?;
                setgid(Gid::from_raw(gid)).map_err(io::Error::from)?;
                setuid(Uid::from_raw(uid)).map_err(io::Error::from)?;
                Ok(())
            });
        }
    }

    // allow us to kill this whole process tree on deadline
    cmd.process_group(0);

    cmd.spawn()
        .with_context(|| format!("spawn {}", display_os(&spec.program)))
}

async fn wait_child(
    child: &mut Child,
    timeout: Option<Duration>,
    oom_kill_before: u32,
) -> ExitResult {
    let wait = child.wait();
    let status = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, wait).await {
            Ok(status) => status,
            Err(_) => {
                if let Some(pid) = child.id()
                    && let Err(error) = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL)
                {
                    warn!(pid, %error, "failed to kill process group");
                }
                let _ = child.wait().await;
                return ExitResult {
                    exit_code: 124,
                    error: Some("command timed out".to_owned()),
                    timed_out: true,
                };
            }
        },
        None => wait.await,
    };

    match status {
        Ok(status) => {
            let code = status.code();
            let signal = status.signal();
            let exit_code = code.or_else(|| signal.map(|sig| 128 + sig)).unwrap_or(1);

            let mut error = None;
            if signal == Some(9) {
                let oom_kill_after = read_oom_kill_count();
                if oom_kill_after > oom_kill_before {
                    error = Some("guest process killed by guest kernel OOM".to_owned());
                }
            }

            ExitResult {
                exit_code,
                error,
                timed_out: false,
            }
        }
        Err(error) => ExitResult {
            exit_code: 1,
            error: Some(error.to_string()),
            timed_out: false,
        },
    }
}

fn spawn_reader(
    mut reader: impl AsyncRead + Unpin + Send + 'static,
    events: Sender<OutData>,
    kind: OutKind,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0_u8; 32 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => {
                    let event = OutData {
                        data: buf[..n].to_vec(),
                        kind,
                    };
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    warn!(%error, "failed to read command stream");
                    return;
                }
            }
        }
    })
}

fn display_os(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}
