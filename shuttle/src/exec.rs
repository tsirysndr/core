use crate::command::{self, OutKind, Spec};
use crate::protocol::{self, Message, v1};
use nix::unistd::{Group, User};
use std::ffi::OsString;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tracing::info;

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

    let mut spec = Spec::new(req.argv[0].clone())
        .args(req.argv[1..].iter().cloned())
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
    match User::from_name(name) {
        Ok(Some(user)) => Ok(ResolvedUser {
            name: name.to_owned(),
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
        }),
        Ok(None) => {
            let uid = name
                .parse::<u32>()
                .map_err(|_| format!("workflow user {name:?} was not found"))?;
            Ok(ResolvedUser {
                name: name.to_owned(),
                uid,
                gid: uid,
            })
        }
        Err(error) => Err(format!("lookup workflow user {name:?}: {error}")),
    }
}

fn lookup_group(name: &str) -> Result<u32, String> {
    match Group::from_name(name) {
        Ok(Some(group)) => Ok(group.gid.as_raw()),
        Ok(None) => name
            .parse::<u32>()
            .map_err(|_| format!("workflow group {name:?} was not found")),
        Err(error) => Err(format!("lookup workflow group {name:?}: {error}")),
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
    fn refuses_root_exec_user() {
        let err = resolve_user("root").unwrap_err();
        assert!(err.contains("refusing to run exec as privileged user"));
    }

    #[test]
    fn refuses_root_exec_group() {
        let err = resolve_user("65534:0").unwrap_err();
        assert!(err.contains("refusing to run exec as privileged user"));
    }
}
