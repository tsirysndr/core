use crate::command::{self, OutKind, Spec};
use crate::protocol::{self, Message, v1};
use nix::unistd::{Group, User};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};

const DEFAULT_USER: &str = "spindle-workflow";

pub async fn run(id: String, req: v1::ExecStart, out: Sender<Message>) {
    let send_exit = async |exit_code: i32, error: Option<String>, timed_out: bool| {
        let msg = Message {
            id: id.clone(),
            exec_exit: Some(v1::ExecExit {
                exit_code,
                error: protocol::error_or_empty(error),
                timed_out,
            }),
            ..Default::default()
        };
        let _ = out.send(msg).await;
    };

    if req.argv.is_empty() {
        send_exit(127, Some("missing argv".to_owned()), false).await;
        return;
    }

    let user = if req.user.is_empty() {
        DEFAULT_USER
    } else {
        req.user.as_str()
    };
    let run_as = match resolve_user(user) {
        Ok(run_as) => run_as,
        Err(err) => {
            send_exit(127, Some(err), false).await;
            return;
        }
    };

    let mut env = run_as.login_env();
    let runtime_dir = run_as.runtime_dir();
    match runtime_dir.try_exists() {
        Ok(true) => env.push((
            OsString::from("XDG_RUNTIME_DIR"),
            runtime_dir.into_os_string(),
        )),
        Ok(false) => {}
        Err(err) => warn!(error = %err, "could not stat XDG_RUNTIME_DIR for workflow user"),
    }

    let mut spec = Spec::new(req.argv[0].clone())
        .args(req.argv[1..].iter().cloned())
        .envs(env)
        .envs(parse_env(&req.env))
        .run_as(run_as.uid, run_as.gid);
    if !req.cwd.is_empty() {
        spec = spec.cwd(req.cwd.clone());
    }
    let timeout =
        (req.timeout_seconds > 0).then(|| Duration::from_secs(u64::from(req.timeout_seconds)));
    if let Some(timeout) = timeout {
        spec = spec.timeout(timeout);
    }

    info!(
        %id,
        user = %run_as.name,
        uid = run_as.uid,
        gid = run_as.gid,
        argv = ?req.argv,
        cwd = ?req.cwd,
        "starting exec"
    );

    let cmd = match command::spawn_streaming(spec) {
        Ok(cmd) => cmd,
        Err(err) => {
            send_exit(127, Some(err.to_string()), false).await;
            return;
        }
    };
    let (mut events, exit_task) = cmd.into_parts();
    while let Some(event) = events.recv().await {
        let data = String::from_utf8_lossy(&event.data).into_owned();
        let output = match event.kind {
            OutKind::Stdout => Message {
                id: id.clone(),
                exec_stdout: Some(v1::ExecStdout { data }),
                ..Default::default()
            },
            OutKind::Stderr => Message {
                id: id.clone(),
                exec_stderr: Some(v1::ExecStderr { data }),
                ..Default::default()
            },
        };
        let _ = out.send(output).await;
    }
    let exit = match exit_task
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("command supervisor failed: {error}")))
    {
        Ok(exit) => exit,
        Err(err) => {
            send_exit(127, Some(err.to_string()), false).await;
            return;
        }
    };

    send_exit(exit.exit_code, exit.error, exit.timed_out).await
}

#[derive(Clone, Debug)]
struct ResolvedUser {
    name: String,
    uid: u32,
    gid: u32,
    home: OsString,
    shell: OsString,
}

impl ResolvedUser {
    fn login_env(&self) -> Vec<(OsString, OsString)> {
        let xdg_cache_home = PathBuf::from(&self.home).join(".cache");
        vec![
            (OsString::from("USER"), OsString::from(&self.name)),
            (OsString::from("LOGNAME"), OsString::from(&self.name)),
            (OsString::from("HOME"), self.home.clone()),
            (
                OsString::from("XDG_CACHE_HOME"),
                xdg_cache_home.into_os_string(),
            ),
            (OsString::from("SHELL"), self.shell.clone()),
        ]
    }

    fn runtime_dir(&self) -> PathBuf {
        PathBuf::from(format!("/run/user/{}", self.uid))
    }
}

fn resolve_user(spec: &str) -> Result<ResolvedUser, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return resolve_user(DEFAULT_USER);
    }

    let (user_part, group_part) = spec
        .split_once(':')
        .map(|(user, group)| (user, Some(group)))
        .unwrap_or((spec, None));

    let mut user = lookup_user(user_part)?;
    if let Some(group) = group_part.filter(|group| !group.is_empty()) {
        user.gid = lookup_group(group)?;
    }

    if user.uid == 0 || user.gid == 0 {
        return Err(format!("refusing to run exec as privileged user {spec:?}"));
    }

    Ok(user)
}

fn lookup_user(name: &str) -> Result<ResolvedUser, String> {
    match name.parse::<u32>() {
        Ok(uid) => Ok(ResolvedUser {
            name: name.to_owned(),
            uid,
            gid: uid,
            home: OsString::from("/"),
            shell: OsString::from("/bin/sh"),
        }),
        Err(_) => match User::from_name(name) {
            Ok(Some(user)) => Ok(ResolvedUser {
                name: name.to_owned(),
                uid: user.uid.as_raw(),
                gid: user.gid.as_raw(),
                home: user.dir.into_os_string(),
                shell: user.shell.into_os_string(),
            }),
            Ok(None) => Err(format!("workflow user {name:?} was not found")),
            Err(error) => Err(format!("lookup workflow user {name:?}: {error}")),
        },
    }
}

fn lookup_group(name: &str) -> Result<u32, String> {
    match name.parse::<u32>() {
        Ok(gid) => Ok(gid),
        Err(_) => match Group::from_name(name) {
            Ok(Some(group)) => Ok(group.gid.as_raw()),
            Ok(None) => Err(format!("workflow group {name:?} was not found")),
            Err(error) => Err(format!("lookup workflow group {name:?}: {error}")),
        },
    }
}

fn parse_env(values: &[String]) -> Vec<(OsString, OsString)> {
    values
        .iter()
        .filter_map(|value| value.split_once('='))
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_exec_as_uid_zero() {
        let err = resolve_user("0").unwrap_err();
        assert!(err.contains("refusing to run exec as privileged user"));
    }

    #[test]
    fn refuses_exec_as_gid_zero() {
        let err = resolve_user("65534:0").unwrap_err();
        assert!(err.contains("refusing to run exec as privileged user"));
    }

    #[test]
    fn resolves_numeric_spec_without_a_user_database() {
        let user = resolve_user("65534:65533").unwrap();
        assert_eq!((user.uid, user.gid), (65534, 65533));
    }
}
