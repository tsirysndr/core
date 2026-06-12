use crate::command::{self, Spec, run_capture};
use crate::nix_config::{SPINDLE_RUN_DIR, nix_executable};
use crate::protocol::{self, Message, v1};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tracing::info;

const USER_CONFIG_DIR: &str = "/run/spindle/user-config";

pub async fn run(id: String, req: v1::ActivateConfig, out: Sender<Message>) {
    let config_key = req.config_key.clone();
    let result = activate(&req).await;
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

async fn activate(req: &v1::ActivateConfig) -> Result<PathBuf> {
    let need_build = req.toplevel.is_empty();
    let timeout = (req.timeout_seconds > 0)
        .then(|| Duration::from_secs(u64::from(req.timeout_seconds)))
        .or_else(|| need_build.then_some(Duration::from_secs(10 * 60)))
        .unwrap_or(Duration::from_secs(2 * 60));

    let toplevel = if need_build {
        build_toplevel(req, timeout).await?
    } else {
        realise_toplevel(&req.toplevel, timeout).await?
    };

    if !toplevel.starts_with("/nix/store/") {
        anyhow::bail!("config toplevel {toplevel:?} is not a nix store path");
    }

    switch_to_configuration(&toplevel, timeout).await?;
    info!(
        config_key = %req.config_key,
        base_config_hash = %req.base_config_hash,
        ?toplevel,
        "activated NixOS config"
    );
    Ok(toplevel)
}

async fn build_toplevel(req: &v1::ActivateConfig, timeout: Duration) -> Result<PathBuf> {
    let user_config = (req.user_config.is_empty())
        .then_some("{}")
        .unwrap_or_else(|| &req.user_config);

    info!("writing user config to {USER_CONFIG_DIR}/config.json");
    write_user_config(user_config).context("write user config")?;

    info!("running nix build command for user config toplevel...");
    let output = run_capture(
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

async fn realise_toplevel(toplevel: &str, timeout: Duration) -> Result<PathBuf> {
    if !toplevel.starts_with("/nix/store/") {
        anyhow::bail!("cached config toplevel {toplevel:?} is not a nix store path");
    }
    let output = command::run_capture(
        Spec::new(nix_executable())
            .args(["build", "--no-link", "--show-trace", toplevel])
            .timeout(timeout),
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
