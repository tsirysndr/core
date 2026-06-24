use crate::command::{CaptureOutput, OutKind, Spec, run_capture, spawn_streaming};
use crate::nix_config::{SPINDLE_RUN_DIR, nix_executable};
use crate::protocol::{self, Message, v1};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tracing::info;

const USER_CONFIG_DIR: &str = "/run/spindle/user-config";
const DEVSHELL_ENV_PATH: &str = "/run/spindle/devshell-env.sh";
const DEVSHELL_DRV: &str = "/etc/spindle/devshell-drv";

pub async fn run(id: String, req: v1::ActivateConfig, out: Sender<Message>) {
    let config_key = req.config_key.clone();
    let result = activate(&id, &req, &out).await;
    let msg = Message {
        id,
        activate_config_result: Some(v1::ActivateConfigResult {
            config_key,
            toplevel: (result.as_ref())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            error: protocol::error_or_empty(result.err().map(|e| format!("{e:#}"))),
        }),
        ..Default::default()
    };
    let _ = out.send(msg).await;
}

async fn activate(id: &str, req: &v1::ActivateConfig, out: &Sender<Message>) -> Result<PathBuf> {
    let need_build = req.toplevel.is_empty();
    let timeout = (req.timeout_seconds > 0)
        .then(|| Duration::from_secs(u64::from(req.timeout_seconds)))
        .or_else(|| need_build.then_some(Duration::from_secs(10 * 60)))
        .unwrap_or(Duration::from_secs(2 * 60));

    let toplevel = if need_build {
        build_toplevel(id, req, timeout, out).await?
    } else {
        realise_toplevel(id, &req.toplevel, timeout, out).await?
    };

    if !toplevel.starts_with("/nix/store/") {
        anyhow::bail!("config toplevel {toplevel:?} is not a nix store path");
    }

    switch_to_configuration(&toplevel, timeout).await?;
    write_devshell_env(timeout).await?;
    info!(
        config_key = %req.config_key,
        base_config_hash = %req.base_config_hash,
        ?toplevel,
        "activated NixOS config"
    );
    Ok(toplevel)
}

async fn write_devshell_env(timeout: Duration) -> Result<()> {
    let pointer = Path::new(DEVSHELL_DRV);
    if !pointer.exists() {
        let _ = fs::remove_file(DEVSHELL_ENV_PATH);
        return Ok(());
    }
    let drv = fs::read_to_string(&pointer)
        .with_context(|| format!("read {pointer:?}"))?
        .trim()
        .to_owned();

    info!(
        ?drv,
        "running nix print-dev-env for dependencies devshell..."
    );
    let output = run_capture(
        Spec::new(nix_executable())
            .args(["print-dev-env", "--show-trace", &drv])
            .cwd(SPINDLE_RUN_DIR)
            .timeout(timeout),
    )
    .await?;

    if !output.success() {
        anyhow::bail!(
            "nix print-dev-env failed: exit={} error={:?} output={}",
            output.exit.exit_code,
            output.exit.error,
            output.combined_lossy(),
        );
    }

    fs::write(DEVSHELL_ENV_PATH, &output.stdout)
        .with_context(|| format!("write {DEVSHELL_ENV_PATH}"))?;
    info!(path = %DEVSHELL_ENV_PATH, "wrote devshell env");
    Ok(())
}

async fn build_toplevel(
    id: &str,
    req: &v1::ActivateConfig,
    timeout: Duration,
    out: &Sender<Message>,
) -> Result<PathBuf> {
    let user_config = (req.user_config.is_empty())
        .then_some("{}")
        .unwrap_or_else(|| &req.user_config);

    info!("writing user config to {USER_CONFIG_DIR}/config.json");
    write_user_config(user_config).context("write user config")?;

    info!("running nix build command for user config toplevel...");
    let output = run_streaming_stderr(
        Spec::new(nix_executable())
            .args([
                "build",
                "--no-link",
                "--show-trace",
                "--json",
                "--file",
                "/etc/spindle/nixos/default.nix",
            ])
            .cwd(SPINDLE_RUN_DIR)
            .timeout(timeout),
        id,
        out,
    )
    .await?;

    if !output.success() {
        anyhow::bail!(
            "nix config build failed: exit={} error={:?} output={}",
            output.exit.exit_code,
            output.exit.error,
            output.combined_lossy(),
        );
    }

    #[derive(Debug, serde::Deserialize)]
    struct NixBuildResult {
        outputs: NixBuildOutputs,
    }
    #[derive(Debug, serde::Deserialize)]
    struct NixBuildOutputs {
        out: PathBuf,
    }
    let [result] = serde_json::from_slice::<[NixBuildResult; 1]>(&output.stdout)
        .context("parse nix build --json output")?;
    Ok(result.outputs.out)
}

fn write_user_config(user_config: &str) -> Result<()> {
    fs::create_dir_all(USER_CONFIG_DIR).with_context(|| format!("create {USER_CONFIG_DIR}"))?;

    let config_path = format!("{USER_CONFIG_DIR}/config.json");
    fs::write(&config_path, user_config).with_context(|| format!("write {config_path}"))?;
    Ok(())
}

async fn realise_toplevel(
    id: &str,
    toplevel: &str,
    timeout: Duration,
    out: &Sender<Message>,
) -> Result<PathBuf> {
    if !toplevel.starts_with("/nix/store/") {
        anyhow::bail!("cached config toplevel {toplevel:?} is not a nix store path");
    }
    let output = run_streaming_stderr(
        Spec::new(nix_executable())
            .args(["build", "--no-link", "--show-trace", toplevel])
            .timeout(timeout),
        id,
        out,
    )
    .await?;
    if !output.success() {
        anyhow::bail!(
            "realise cached config failed: exit={} error={:?} output={}",
            output.exit.exit_code,
            output.exit.error,
            output.combined_lossy(),
        );
    }

    Ok(PathBuf::from(toplevel))
}

// streams stderr but captures stdout
async fn run_streaming_stderr(
    spec: Spec,
    id: &str,
    out: &Sender<Message>,
) -> Result<CaptureOutput> {
    let running = spawn_streaming(spec)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let (mut events, exit_task) = running.into_parts();

    while let Some(event) = events.recv().await {
        match event.kind {
            OutKind::Stdout => stdout.extend_from_slice(&event.data),
            OutKind::Stderr => {
                stderr.extend_from_slice(&event.data);
                let data = String::from_utf8_lossy(&event.data).into_owned();
                let _ = out
                    .send(Message {
                        id: id.to_owned(),
                        exec_stderr: Some(v1::ExecStderr { data }),
                        ..Default::default()
                    })
                    .await;
            }
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

async fn switch_to_configuration(toplevel: &Path, timeout: Duration) -> Result<()> {
    info!("switching to new configuration: {:?}", toplevel);
    let switch = toplevel.join("bin/switch-to-configuration");
    let output = run_capture(Spec::new(switch).args(["test"]).timeout(timeout)).await?;
    if !output.success() {
        anyhow::bail!(
            "switch-to-configuration test failed: exit={} error={:?} output={}",
            output.exit.exit_code,
            output.exit.error,
            output.combined_lossy(),
        );
    }
    Ok(())
}
