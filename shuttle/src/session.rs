use crate::cache::{CacheUploadManager, ReadCacheProxy, WriteCacheProxy};
use crate::command::Spec;
use crate::dns_proxy::DnsProxy;
use crate::exec;
use crate::nix_config::{self, SYSTEMCTL_EXECUTABLE};
use crate::on_payload;
use crate::protocol::{self, Message, v1};
use crate::pty;
use crate::{activation, command};
use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::io::{AsyncWrite, BufReader};
use tokio::sync::mpsc::{self, Sender};
use tokio::task::{JoinError, JoinSet};
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::{info, warn};

pub async fn run(host_cid: u32, port: u32) -> Result<()> {
    let mut conn = VsockStream::connect(VsockAddr::new(host_cid, port))
        .await
        .with_context(|| format!("dial host vsock cid={host_cid} port={port}"))?;

    send_hello(&mut conn).await?;

    let (reader_conn, writer_conn) = tokio::io::split(conn);
    let (out_tx, out_rx) = mpsc::channel::<Message>(256);
    let writer = tokio::spawn(async move { writer_loop(writer_conn, out_rx).await });
    let mut reader = BufReader::new(reader_conn);

    let init = match protocol::read_message(&mut reader).await? {
        Some(Message {
            init: Some(init), ..
        }) => init,
        Some(other) => bail!("expected init, got {}", protocol::kind(&other)),
        None => bail!("read init: EOF"),
    };
    info!(job_id = %init.job_id, "received init");

    let read_proxy = ReadCacheProxy::start(host_cid, init.cache_read_proxy_port)
        .await
        .context("start read cache proxy")?;
    let write_proxy = WriteCacheProxy::start(host_cid, init.cache_upload_proxy_port)
        .await
        .context("start write cache proxy")?;
    let _dns_proxy = DnsProxy::start(host_cid, init.dns_proxy_port)
        .await
        .context("start dns proxy")?;
    let _cache_cfg = nix_config::configure(
        &init,
        read_proxy.as_ref().map(ReadCacheProxy::url).unwrap_or(""),
    )
    .await
    .context("configure nix cache")?;
    let uploader = CacheUploadManager::start(
        write_proxy.as_ref().map(WriteCacheProxy::url).unwrap_or(""),
        out_tx.clone(),
    )
    .await
    .context("start cache upload manager")?;

    let mut tasks = JoinSet::new();
    let read_result: Result<()> = loop {
        tokio::select! {
            read = protocol::read_message(&mut reader) => match read {
                Ok(Some(msg)) => spawn_message_task(&mut tasks, host_cid, msg, &out_tx, uploader.clone()),
                Ok(None) => break Ok(()),
                Err(error) => break Err(error).context("read message"),
            },
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                log_task_result(result, false);
            }
        }
    };

    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        log_task_result(result, true);
    }

    drop(out_tx);
    let _ = writer.await;
    read_result?;
    Ok(())
}

fn spawn_message_task(
    tasks: &mut JoinSet<()>,
    host_cid: u32,
    msg: Message,
    out_tx: &Sender<Message>,
    uploader: Option<CacheUploadManager>,
) {
    let kind = protocol::kind(&msg);
    let handle = on_payload!(msg, {
        activate_config => tasks.spawn(activation::run(msg.id, activate_config, out_tx.clone())),
        exec_start => tasks.spawn(exec::run(msg.id, exec_start, out_tx.clone())),
        cache_drain => tasks.spawn(run_cache_drain(msg.id, cache_drain, out_tx.clone(), uploader)),
        poweroff => tasks.spawn(run_poweroff(msg.id, poweroff, out_tx.clone())),
        open_debug_shell => tasks.spawn(pty::run(host_cid, open_debug_shell)),
    });
    if handle.is_none() {
        warn!(kind, "ignoring unsupported message");
    }
}

fn log_task_result(result: Result<(), JoinError>, shutting_down: bool) {
    match result {
        Ok(()) => {}
        Err(error) if shutting_down && error.is_cancelled() => {}
        Err(error) => warn!(%error, "session handler task failed"),
    }
}

async fn writer_loop<W>(mut conn: W, mut rx: mpsc::Receiver<Message>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(msg) = rx.recv().await {
        if let Err(error) = protocol::write_message(&mut conn, &msg).await {
            warn!(%error, "failed to write protocol message");
            break;
        }
    }
}

async fn send_hello(conn: &mut VsockStream) -> Result<()> {
    let boot_id = tokio::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .await
        .unwrap_or_default()
        .trim()
        .to_owned();
    let nix_version = nix_config::nix_version().await;

    let hello_payload = v1::Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        boot_id: boot_id.clone(),
        nix_version: nix_version.clone(),
    };
    info!(
        protocol = hello_payload.protocol_version,
        version = %hello_payload.agent_version,
        boot = %hello_payload.boot_id,
        nix = %hello_payload.nix_version,
        "sent hello"
    );
    let hello = Message {
        id: "hello".to_owned(),
        hello: Some(hello_payload),
        ..Default::default()
    };

    protocol::write_message(conn, &hello)
        .await
        .context("send hello")?;
    Ok(())
}

async fn run_cache_drain(
    id: String,
    req: v1::CacheDrain,
    out: Sender<Message>,
    uploader: Option<CacheUploadManager>,
) {
    let timeout =
        (req.timeout_seconds > 0).then(|| Duration::from_secs(u64::from(req.timeout_seconds)));
    let stats = match uploader.as_ref() {
        Some(uploader) => uploader.drain(timeout).await,
        None => Default::default(),
    };

    if let Some(error) = &stats.last_error {
        warn!(
            %id,
            pending = stats.pending,
            active = stats.active,
            uploaded = stats.uploaded,
            failed = stats.failed,
            %error,
            "cache drain completed with error"
        );
    } else {
        info!(
            %id,
            uploaded = stats.uploaded,
            failed = stats.failed,
            "cache drain completed"
        );
    }

    let result = Message {
        id,
        cache_drain_result: Some(v1::CacheDrainResult {
            error: protocol::error_or_empty(stats.last_error),
            cache_queued: stats.pending,
            cache_active: stats.active,
            cache_uploaded: stats.uploaded,
            cache_failed: stats.failed,
        }),
        ..Default::default()
    };
    let _ = out.send(result).await;
}

async fn run_poweroff(id: String, _req: v1::Poweroff, out: Sender<Message>) {
    let result = Message {
        id,
        poweroff_result: Some(v1::PoweroffResult {
            error: String::new(),
        }),
        ..Default::default()
    };
    let _ = out.send(result).await;

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;

        // prefer a clean shutdown through the init system when one is around
        // (systemd on NixOS, busybox/openrc elsewhere), then fall back to the
        // raw reboot(2) syscall on minimal guests
        for poweroff in [SYSTEMCTL_EXECUTABLE, "/sbin/poweroff", "/usr/sbin/poweroff"] {
            if !std::path::Path::new(poweroff).exists() {
                continue;
            }
            let mut spec = Spec::new(poweroff).timeout(Duration::from_secs(5));
            if poweroff == SYSTEMCTL_EXECUTABLE {
                spec = spec.args(["poweroff"]);
            }
            match command::run_capture(spec).await {
                Ok(output) if output.success() => return,
                Ok(output) => warn!(
                    %poweroff,
                    exit_code = output.exit.exit_code,
                    error = ?output.exit.error,
                    output = %output.combined_lossy(),
                    "poweroff command failed"
                ),
                Err(error) => warn!(%poweroff, %error, "poweroff command failed"),
            }
        }

        // only ever returns on failure
        let error =
            nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF).unwrap_err();
        warn!(%error, "reboot(RB_POWER_OFF) syscall failed");
    });
}
