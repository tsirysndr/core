use std::path::Path;

use crate::command::{self, Spec};
use crate::protocol::{self, Message, v1};
use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, User};
use pty_process::Size;
use tokio::io::{AsyncReadExt, BufReader};
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::{info, warn};

const WF_USER: &str = "spindle-workflow";
const READ_CHUNK: usize = 32 * 1024;

pub async fn run(host_cid: u32, open: v1::OpenDebugShell) {
    if let Err(error) = serve(host_cid, open).await {
        warn!(%error, "debug shell session failed");
    }
}

async fn serve(host_cid: u32, open: v1::OpenDebugShell) -> Result<()> {
    let conn = VsockStream::connect(VsockAddr::new(host_cid, open.vsock_port))
        .await
        .with_context(|| format!("dial host debug vsock port {}", open.vsock_port))?;
    info!(port = open.vsock_port, "debug shell connected");

    let user = resolve_user(WF_USER)?;

    let rows = clamp_tty_dim(open.rows);
    let cols = clamp_tty_dim(open.cols);

    let spec = Spec::new(&user.shell)
        .arg("-l")
        .envs(user.env(&open.term))
        .run_as(user.uid, user.gid)
        .cwd(Path::new(&user.home).join("repo")); // /workflow/repo

    let (mut pty_reader, mut pty_writer, mut child) =
        command::spawn_pty(spec, rows, cols).context("spawn pty shell")?;
    let pid = child.id();

    let (conn_reader, conn_writer) = tokio::io::split(conn);
    let mut conn_reader = BufReader::new(conn_reader);
    let mut conn_writer = conn_writer;

    let mut buf = vec![0u8; READ_CHUNK];
    let client_gone = loop {
        tokio::select! {
            read = pty_reader.read(&mut buf) => match read {
                Ok(0) => break false, // shell exited
                Ok(n) => {
                    let msg = Message {
                        id: "pty".to_owned(),
                        pty_data: Some(v1::PtyData { data: buf[..n].to_vec().into() }),
                        ..Default::default()
                    };
                    if protocol::write_message(&mut conn_writer, &msg).await.is_err() {
                        break true;
                    }
                }
                Err(error) => {
                    // linux returns EIO (not a clean EOF) on the master once the
                    // slave side is fully closed, so treat that as the shell
                    // exiting normally rather than a real read failure.
                    if error.raw_os_error() != Some(nix::libc::EIO) {
                        warn!(%error, "pty master read failed");
                    }
                    break false;
                }
            },
            incoming = protocol::read_message(&mut conn_reader) => match incoming {
                Ok(Some(msg)) => {
                    if let Some(data) = msg.pty_data {
                        use tokio::io::AsyncWriteExt;
                        if pty_writer.write_all(&data.data).await.is_err() {
                            break false;
                        }
                    } else if let Some(resize) = msg.pty_resize {
                        let size = Size::new(clamp_tty_dim(resize.rows), clamp_tty_dim(resize.cols));
                        if let Err(error) = pty_writer.resize(size) {
                            warn!(%error, "pty resize failed");
                        }
                    }
                    // anything else on the debug channel is ignored
                }
                Ok(None) => break true, // client closed the connection
                Err(error) => {
                    warn!(%error, "debug channel read failed");
                    break true;
                }
            },
        }
    };

    // if the client disconnected first, hang up the shell's process group so we
    // don't leak a detached session. (pty-process calls setsid in the child, so
    // it leads a new session and process group => pgid == pid.)
    if client_gone && let Some(pid) = pid {
        let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGHUP);
    }

    let exit_code = match child.wait().await {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .unwrap_or(1)
        }
        Err(error) => {
            warn!(%error, "waiting on debug shell failed");
            1
        }
    };

    let exit = Message {
        id: "pty".to_owned(),
        exec_exit: Some(v1::ExecExit {
            exit_code,
            error: String::new(),
            timed_out: false,
        }),
        ..Default::default()
    };
    let _ = protocol::write_message(&mut conn_writer, &exit).await;
    info!(exit_code, "debug shell session ended");
    Ok(())
}

struct ResolvedUser {
    uid: u32,
    gid: u32,
    name: String,
    home: String,
    shell: String,
}

impl ResolvedUser {
    fn env(&self, term: &str) -> Vec<(String, String)> {
        let term = if term.is_empty() {
            "xterm-256color"
        } else {
            term
        };
        vec![
            ("TERM".to_owned(), term.to_owned()),
            ("HOME".to_owned(), self.home.clone()),
            ("USER".to_owned(), self.name.clone()),
            ("LOGNAME".to_owned(), self.name.clone()),
            ("SHELL".to_owned(), self.shell.clone()),
            (
                "PATH".to_owned(),
                "/run/current-system/sw/bin:/usr/bin:/bin".to_owned(),
            ),
        ]
    }
}

fn resolve_user(name: &str) -> Result<ResolvedUser> {
    let user = User::from_name(name)
        .with_context(|| format!("lookup user {name:?}"))?
        .with_context(|| format!("debug shell user {name:?} not found"))?;
    if user.uid.as_raw() == 0 || user.gid.as_raw() == 0 {
        bail!("refusing to open a debug shell as privileged user {name:?}");
    }
    let shell = user.shell.to_string_lossy().into_owned();
    if shell.is_empty() {
        bail!("debug shell user {name:?} has no login shell set in the image");
    }
    Ok(ResolvedUser {
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        name: user.name,
        home: user.dir.to_string_lossy().into_owned(),
        shell,
    })
}

fn clamp_tty_dim(value: u32) -> u16 {
    value.clamp(1, u16::MAX as u32) as u16
}
