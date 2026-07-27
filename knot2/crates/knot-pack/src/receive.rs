use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use knot_git::{Filter, RefTxn, RefUpdate, Repo};
use knot_messages::{RefKey, RejectMessages};
use knot_types::{ObjectFormat, Oid, PushOption, PushOptions, RefName};

use crate::error::PackError;
use crate::meter::PackLimits;
use crate::objects;
use crate::pkt;
use crate::quarantine::Quarantine;
use crate::receiver::ReceivedPack;
use crate::{HaveOids, WantOids};

fn stage_pack_bytes(
    dir: &Path,
    pack: &[u8],
    kind: gix::hash::Kind,
) -> Result<Option<(tempfile::NamedTempFile, gix_pack::data::File)>, PackError> {
    if pack.is_empty() {
        return Ok(None);
    }
    if !pack.starts_with(b"PACK") {
        return Err(PackError::Pack(
            "packfile is missing its PACK signature".to_string(),
        ));
    }
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(pack)?;
    tmp.flush()?;
    let file = gix_pack::data::File::at(tmp.path(), kind)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    Ok(Some((tmp, file)))
}

pub fn handle_bytes(repo: &Repo, body: &[u8], limits: &PackLimits) -> Result<Vec<u8>, PackError> {
    let kind = repo.object_format().kind();
    let messages = &crate::default_catalog().reject;
    match stage_pack_bytes(&repo.objects_dir(), pkt::split_receive(body)?.pack, kind) {
        Ok(staged) => handle(
            repo,
            body,
            Ok(staged.as_ref().map(|(_, file)| file)),
            limits,
            messages,
        ),
        Err(error) => handle(repo, body, Err(error), limits, messages),
    }
}

pub fn handle_guarded_bytes(
    live: &Repo,
    body: &[u8],
    limits: &PackLimits,
    guard: &dyn ReceiveGuard,
    seal: &dyn Fn(&[RefUpdate]),
    messages: &RejectMessages,
) -> Result<ReceiveOutcome, PackError> {
    let kind = live.object_format().kind();
    match stage_pack_bytes(&live.objects_dir(), pkt::split_receive(body)?.pack, kind) {
        Ok(staged) => handle_guarded(
            live,
            body,
            Ok(staged.as_ref().map(|(_, file)| file)),
            limits,
            guard,
            seal,
            messages,
        ),
        Err(error) => handle_guarded(live, body, Err(error), limits, guard, seal, messages),
    }
}

pub fn handle_guarded_streamed(
    live: &Repo,
    received: &ReceivedPack,
    limits: &PackLimits,
    guard: &dyn ReceiveGuard,
    seal: &dyn Fn(&[RefUpdate]),
    messages: &RejectMessages,
) -> Result<ReceiveOutcome, PackError> {
    match received.open_pack() {
        Ok(pack) => handle_guarded(
            live,
            received.preamble(),
            Ok(pack.as_ref()),
            limits,
            guard,
            seal,
            messages,
        ),
        Err(error) => handle_guarded(
            live,
            received.preamble(),
            Err(error),
            limits,
            guard,
            seal,
            messages,
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefDecision {
    Allow,
    Reject(String),
}

pub struct ReceiveOutcome {
    pub report: Vec<u8>,
    pub side_band: bool,
    pub push_options: PushOptions,
}

pub trait ReceiveGuard {
    fn authorize(&self, staged: &Repo, commands: &[ReceiveCommand]) -> Vec<RefDecision>;
}

const CAPS_BASE: &str =
    "report-status delete-refs atomic ofs-delta side-band-64k push-options agent=knot/0";

fn caps(format: ObjectFormat) -> String {
    format!("{CAPS_BASE} object-format={}", format.capability())
}

pub fn advertise(repo: &Repo) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    pkt::write_data(&mut buf, b"# service=git-receive-pack\n")?;
    pkt::write_flush(&mut buf)?;
    write_advert(&mut buf, repo)?;
    Ok(buf)
}

pub fn advertise_ssh(repo: &Repo) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    write_advert(&mut buf, repo)?;
    Ok(buf)
}

fn write_advert(buf: &mut Vec<u8>, repo: &Repo) -> Result<(), PackError> {
    let format = repo.object_format();
    let caps = caps(format);
    let refs = repo.advertised_refs_for(knot_git::AdvertScope::Receive)?;
    match refs.split_first() {
        Some((first, rest)) => {
            let mut line = format!("{} {}", first.target, first.name).into_bytes();
            line.push(0);
            line.extend_from_slice(caps.as_bytes());
            line.push(b'\n');
            pkt::write_data(buf, &line)?;
            rest.iter().try_fold(&mut *buf, |buf, record| {
                pkt::write_data(
                    buf,
                    format!("{} {}\n", record.target, record.name).as_bytes(),
                )?;
                Ok::<_, PackError>(buf)
            })?;
        }
        None => {
            let mut line = format!("{} capabilities^{{}}", format.null_oid()).into_bytes();
            line.push(0);
            line.extend_from_slice(caps.as_bytes());
            line.push(b'\n');
            pkt::write_data(buf, &line)?;
        }
    }
    pkt::write_flush(buf)?;
    Ok(())
}

enum CommandRef {
    Named(RefName),
    Unparsed(String),
}

pub struct ReceiveCommand {
    old: Oid,
    new: Oid,
    name: CommandRef,
}

impl ReceiveCommand {
    pub fn refname(&self) -> &str {
        match &self.name {
            CommandRef::Named(name) => name.as_str(),
            CommandRef::Unparsed(raw) => raw,
        }
    }

    pub fn name(&self) -> Option<&RefName> {
        match &self.name {
            CommandRef::Named(name) => Some(name),
            CommandRef::Unparsed(_) => None,
        }
    }

    pub fn is_delete(&self) -> bool {
        self.new.is_null()
    }

    pub fn is_create(&self) -> bool {
        self.old.is_null()
    }

    fn parse(line: &[u8], first: bool) -> Option<ReceiveCommand> {
        let line = if first {
            line.split(|byte| *byte == 0).next().unwrap_or(line)
        } else {
            line
        };
        let text = std::str::from_utf8(line).ok()?;
        let mut parts = text.trim_end().split(' ');
        let old = Oid::from_hex(parts.next()?).ok()?;
        let new = Oid::from_hex(parts.next()?).ok()?;
        let raw = parts.next()?.to_string();
        let name = match RefName::new(raw.as_str()) {
            Ok(name) => CommandRef::Named(name),
            Err(_) => CommandRef::Unparsed(raw),
        };
        Some(ReceiveCommand { old, new, name })
    }

    fn to_update(&self) -> Result<RefUpdate, PackError> {
        let name = self
            .name()
            .cloned()
            .ok_or_else(|| PackError::Protocol(invalid_refname(self.refname())))?;
        Ok(match (self.old.is_null(), self.new.is_null()) {
            (_, true) => RefUpdate::Delete {
                name,
                old: self.old,
            },
            (true, false) => RefUpdate::Create {
                name,
                new: self.new,
            },
            (false, false) => RefUpdate::Update {
                name,
                old: self.old,
                new: self.new,
            },
        })
    }
}

pub(crate) fn invalid_refname(raw: &str) -> String {
    format!("invalid ref name {raw:?}")
}

fn forbidden_ref(command: &ReceiveCommand, messages: &RejectMessages) -> Option<String> {
    match command.name() {
        Some(name) => (!knot_git::is_public_ref(name)).then(|| messages.reserved_refs.text()),
        None => Some(invalid_refname(command.refname())),
    }
}

fn reserved_create_only(command: &ReceiveCommand, messages: &RejectMessages) -> Option<String> {
    (command.name().is_some_and(knot_git::is_reserved) && !command.is_create())
        .then(|| messages.cob_create_only.text())
}

struct RefSnapshot {
    by_name: HashMap<RefName, Oid>,
}

impl RefSnapshot {
    fn capture(repo: &Repo) -> Result<RefSnapshot, PackError> {
        let by_name = repo
            .references()?
            .into_iter()
            .map(|record| (record.name, record.target))
            .collect();
        Ok(RefSnapshot { by_name })
    }

    fn tips(&self) -> HaveOids {
        self.by_name.values().copied().collect()
    }

    fn conflict(&self, command: &ReceiveCommand, messages: &RejectMessages) -> Option<String> {
        match (
            command.old.is_null(),
            command
                .name()
                .and_then(|name| self.by_name.get(name).copied()),
        ) {
            (true, Some(_)) => Some(messages.ref_exists.text()),
            (false, found) if found != Some(command.old) => Some(messages.stale_old_value.text()),
            _ => None,
        }
    }
}

struct RefResult {
    refname: String,
    failure: Option<String>,
}

impl RefResult {
    fn of(command: &ReceiveCommand, failure: Option<String>) -> RefResult {
        RefResult {
            refname: command.refname().to_string(),
            failure,
        }
    }
}

struct Conflict {
    refname: String,
    reason: String,
}

fn first_conflict(
    snapshot: &RefSnapshot,
    commands: &[ReceiveCommand],
    messages: &RejectMessages,
) -> Option<Conflict> {
    commands.iter().find_map(|command| {
        snapshot.conflict(command, messages).map(|reason| Conflict {
            refname: command.refname().to_string(),
            reason,
        })
    })
}

fn objects_present(
    repo: &Repo,
    wants: &WantOids,
    haves: &HaveOids,
    closure: Option<&objects::FreshClosure>,
) -> bool {
    if let Some(closure) = closure {
        return closure.self_contained
            && wants
                .as_slice()
                .iter()
                .all(|want| closure.present.contains(want) || repo.contains(*want));
    }
    matches!(
        repo.select_pack_objects_filtered(
            wants.wants(),
            haves.haves(),
            Filter::None,
            crate::upload::selection_budget(),
        ),
        Ok(selection) if selection.send.iter().all(|oid| repo.contains(*oid))
    )
}

fn connectivity_reasons(
    repo: &Repo,
    commands: &[ReceiveCommand],
    haves: &HaveOids,
    closure: Option<&objects::FreshClosure>,
    messages: &RejectMessages,
) -> Vec<Option<String>> {
    let news: WantOids = commands
        .iter()
        .map(|command| command.new)
        .filter(|new| !new.is_null())
        .collect();
    let batched_ok = news.is_empty() || objects_present(repo, &news, haves, closure);
    commands
        .iter()
        .map(|command| {
            if command.new.is_null()
                || batched_ok
                || objects_present(repo, &WantOids::new(vec![command.new]), haves, closure)
            {
                None
            } else {
                Some(messages.missing_objects.text())
            }
        })
        .collect()
}

pub(crate) fn fuzz(body: &[u8]) {
    if let Ok(parsed) = pkt::split_receive(body) {
        parsed
            .commands
            .iter()
            .enumerate()
            .for_each(|(index, line)| {
                if let Some(command) = ReceiveCommand::parse(line, index == 0) {
                    let _ = command.to_update();
                }
            });
    }
}

pub(crate) fn is_empty(repo: &Repo) -> bool {
    repo.references()
        .map(|refs| refs.is_empty())
        .unwrap_or(false)
}

pub(crate) fn ingest(
    objects_dir: &Path,
    pack: Option<&gix_pack::data::File>,
    limits: &PackLimits,
    kind: gix::hash::Kind,
    live_empty: bool,
) -> (Result<(), PackError>, Option<objects::FreshClosure>) {
    let Some(pack) = pack else {
        return (Ok(()), None);
    };
    if let Err(error) = objects::admit_ingest(pack, kind) {
        return (Err(error), None);
    }
    if live_empty {
        match objects::ingest_and_close(
            objects_dir,
            pack,
            limits,
            kind,
            knot_resource::ingest_base_budget(),
            false,
        ) {
            Ok(Some(closure)) => return (Ok(()), Some(closure)),
            Ok(None) => {}
            Err(error) => return (Err(error), None),
        }
    }
    (
        objects::index_pack_bounded(objects_dir, pack, limits, kind),
        None,
    )
}

pub fn handle(
    repo: &Repo,
    body: &[u8],
    pack: Result<Option<&gix_pack::data::File>, PackError>,
    limits: &PackLimits,
    messages: &RejectMessages,
) -> Result<Vec<u8>, PackError> {
    let parsed = pkt::split_receive(body)?;
    let atomic = parsed.caps.atomic;
    let commands = parse_commands(&parsed);
    let kind = repo.object_format().kind();

    let (unpack, closure) = match pack {
        Ok(pack) => ingest(&repo.objects_dir(), pack, limits, kind, is_empty(repo)),
        Err(error) => (Err(error), None),
    };
    let results: Vec<RefResult> = match &unpack {
        Err(_) => all_failed(&commands, &messages.unpacker_error.text()),
        Ok(()) => match RefSnapshot::capture(repo) {
            Err(_) => all_failed(&commands, &messages.ref_snapshot_unavailable.text()),
            Ok(snapshot) => {
                let haves = snapshot.tips();
                if atomic {
                    apply_atomic(
                        repo,
                        &snapshot,
                        &commands,
                        &haves,
                        closure.as_ref(),
                        messages,
                    )
                } else {
                    commands
                        .iter()
                        .zip(connectivity_reasons(
                            repo,
                            &commands,
                            &haves,
                            closure.as_ref(),
                            messages,
                        ))
                        .map(|(command, connectivity)| {
                            RefResult::of(command, apply_one(repo, command, connectivity, messages))
                        })
                        .collect()
                }
            }
        },
    };
    report(&unpack, &results)
        .map(|report| pkt::frame_report(&report, &[], parsed.caps.side_band_64k))
}

fn apply_one(
    repo: &Repo,
    command: &ReceiveCommand,
    connectivity: Option<String>,
    messages: &RejectMessages,
) -> Option<String> {
    if let Some(reason) = forbidden_ref(command, messages) {
        return Some(reason);
    }
    if let Some(reason) = connectivity {
        return Some(reason);
    }
    command
        .to_update()
        .and_then(|update| repo.update_ref(&update).map_err(PackError::from))
        .err()
        .map(|error| error.to_string().replace('\n', " "))
}

fn atomic_failure(
    snapshot: &RefSnapshot,
    commands: &[ReceiveCommand],
    messages: &RejectMessages,
) -> Vec<RefResult> {
    let conflict = first_conflict(snapshot, commands, messages);
    commands
        .iter()
        .map(|command| match &conflict {
            Some(conflict) if conflict.refname == command.refname() => {
                RefResult::of(command, Some(conflict.reason.clone()))
            }
            _ => RefResult::of(command, Some(messages.atomic_failed.text())),
        })
        .collect()
}

fn apply_atomic(
    repo: &Repo,
    snapshot: &RefSnapshot,
    commands: &[ReceiveCommand],
    haves: &HaveOids,
    closure: Option<&objects::FreshClosure>,
    messages: &RejectMessages,
) -> Vec<RefResult> {
    let fail = |reason: String| all_failed(commands, &reason);
    if commands
        .iter()
        .any(|command| forbidden_ref(command, messages).is_some())
    {
        return commands
            .iter()
            .map(|command| {
                let reason = forbidden_ref(command, messages)
                    .unwrap_or_else(|| messages.atomic_aborted.text());
                RefResult::of(command, Some(reason))
            })
            .collect();
    }
    if let Some(command) = commands
        .iter()
        .zip(connectivity_reasons(
            repo, commands, haves, closure, messages,
        ))
        .find_map(|(command, reason)| reason.map(|_| command))
    {
        return fail(
            messages
                .missing_objects_for
                .line(|RefKey::Ref| command.refname().to_string()),
        );
    }
    let updates = match commands
        .iter()
        .map(ReceiveCommand::to_update)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(updates) => updates,
        Err(error) => return fail(error.to_string().replace('\n', " ")),
    };
    match repo.update_refs(&updates) {
        Ok(()) => commands
            .iter()
            .map(|command| RefResult::of(command, None))
            .collect(),
        Err(_) => atomic_failure(snapshot, commands, messages),
    }
}

fn report(unpack: &Result<(), PackError>, results: &[RefResult]) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    match unpack {
        Ok(()) => pkt::write_data(&mut buf, b"unpack ok\n")?,
        Err(error) => pkt::write_data(
            &mut buf,
            format!("unpack {}\n", error.to_string().replace('\n', " ")).as_bytes(),
        )?,
    }
    results.iter().try_fold(&mut buf, |buf, result| {
        let line = match &result.failure {
            None => format!("ok {}\n", result.refname),
            Some(reason) => format!("ng {} {reason}\n", result.refname),
        };
        pkt::write_data(buf, line.as_bytes())?;
        Ok::<_, PackError>(buf)
    })?;
    pkt::write_flush(&mut buf)?;
    Ok(buf)
}

fn parse_commands(parsed: &pkt::Receive) -> Vec<ReceiveCommand> {
    parsed
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, line)| ReceiveCommand::parse(line, index == 0))
        .collect()
}

pub struct Preflight {
    pub creates_branch: bool,
}

pub(crate) fn preflight(body: &[u8]) -> Preflight {
    pkt::split_receive(body)
        .map(|parsed| {
            let commands = parse_commands(&parsed);
            Preflight {
                creates_branch: commands.iter().any(|command| {
                    command.is_create() && command.name().is_some_and(knot_git::is_branch)
                }),
            }
        })
        .unwrap_or(Preflight {
            creates_branch: false,
        })
}

fn all_failed(commands: &[ReceiveCommand], reason: &str) -> Vec<RefResult> {
    commands
        .iter()
        .map(|command| RefResult::of(command, Some(reason.to_string())))
        .collect()
}

fn stage_to_quarantine(staged: &Repo, commands: &[ReceiveCommand]) {
    commands
        .iter()
        .filter(|command| !command.is_delete())
        .for_each(|command| {
            if let Some(name) = command.name() {
                let _ = staged.update_ref(&RefUpdate::Create {
                    name: name.clone(),
                    new: command.new,
                });
            }
        });
}

fn apply_guarded_atomic(
    txn: &RefTxn<'_>,
    snapshot: &RefSnapshot,
    commands: &[ReceiveCommand],
    seal: &dyn Fn(&[RefUpdate]),
    messages: &RejectMessages,
) -> Vec<RefResult> {
    let updates = match commands
        .iter()
        .map(ReceiveCommand::to_update)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(updates) => updates,
        Err(error) => return all_failed(commands, &error.to_string().replace('\n', " ")),
    };
    match txn.update_refs(&updates) {
        Ok(()) => {
            seal(&updates);
            commands
                .iter()
                .map(|command| RefResult::of(command, None))
                .collect()
        }
        Err(_) => atomic_failure(snapshot, commands, messages),
    }
}

fn apply_guarded_update(
    txn: &RefTxn<'_>,
    command: &ReceiveCommand,
    seal: &dyn Fn(&[RefUpdate]),
) -> Option<String> {
    let update = match command.to_update() {
        Ok(update) => update,
        Err(error) => return Some(error.to_string().replace('\n', " ")),
    };
    match txn.update_ref(&update) {
        Ok(()) => {
            seal(std::slice::from_ref(&update));
            None
        }
        Err(error) => Some(PackError::from(error).to_string().replace('\n', " ")),
    }
}

fn parse_push_options(parsed: &pkt::Receive) -> PushOptions {
    PushOptions::new(parsed.options.iter().filter_map(|option| {
        PushOption::new(
            String::from_utf8_lossy(option).trim_matches(|byte: char| byte == '\n' || byte == '\r'),
        )
        .ok()
    }))
}

#[allow(clippy::too_many_arguments)]
pub fn handle_guarded(
    live: &Repo,
    body: &[u8],
    pack: Result<Option<&gix_pack::data::File>, PackError>,
    limits: &PackLimits,
    guard: &dyn ReceiveGuard,
    seal: &dyn Fn(&[RefUpdate]),
    messages: &RejectMessages,
) -> Result<ReceiveOutcome, PackError> {
    let parsed = pkt::split_receive(body)?;
    let atomic = parsed.caps.atomic;
    let side_band = parsed.caps.side_band_64k;
    let push_options = parse_push_options(&parsed);
    let build = |report: Vec<u8>| ReceiveOutcome {
        report,
        side_band,
        push_options: push_options.clone(),
    };
    let commands = parse_commands(&parsed);
    if commands.is_empty() {
        return report(&Ok(()), &[]).map(build);
    }

    let pack = match pack {
        Ok(pack) => pack,
        Err(error) => {
            let results = all_failed(&commands, &messages.unpacker_error.text());
            return report(&Err(error), &results).map(build);
        }
    };
    let (quarantine, closure) = match Quarantine::stage(
        live,
        pack,
        limits,
        live.object_format().kind(),
        is_empty(live),
    ) {
        Ok(staged) => staged,
        Err(error) => {
            let results = all_failed(&commands, &messages.unpacker_error.text());
            return report(&Err(error), &results).map(build);
        }
    };
    let staged = quarantine.repo();
    stage_to_quarantine(staged, &commands);
    let snapshot = match RefSnapshot::capture(live) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            let results = all_failed(&commands, &messages.ref_snapshot_unavailable.text());
            return report(&Ok(()), &results).map(build);
        }
    };
    let haves = snapshot.tips();

    let verdicts = guard.authorize(staged, &commands);
    let reasons: Vec<Option<String>> = if verdicts.len() == commands.len() {
        commands
            .iter()
            .zip(verdicts)
            .zip(connectivity_reasons(
                staged,
                &commands,
                &haves,
                closure.as_ref(),
                messages,
            ))
            .map(|((command, verdict), connectivity)| match verdict {
                RefDecision::Reject(reason) => Some(reason),
                RefDecision::Allow => reserved_create_only(command, messages)
                    .or(connectivity)
                    .or_else(|| snapshot.conflict(command, messages)),
            })
            .collect()
    } else {
        commands
            .iter()
            .map(|_| Some(messages.authorization_unavailable.text()))
            .collect()
    };

    let any_reject = reasons.iter().any(Option::is_some);
    if atomic && any_reject {
        let results = commands
            .iter()
            .zip(reasons)
            .map(|(command, reason)| {
                RefResult::of(
                    command,
                    Some(reason.unwrap_or_else(|| messages.atomic_aborted.text())),
                )
            })
            .collect::<Vec<_>>();
        return report(&Ok(()), &results).map(build);
    }

    let applied = live.with_ref_txn(|txn| {
        if reasons.iter().any(Option::is_none) {
            quarantine.migrate_into(live)?;
        }
        let results = if atomic {
            apply_guarded_atomic(txn, &snapshot, &commands, seal, messages)
        } else {
            commands
                .iter()
                .zip(reasons)
                .map(|(command, reason)| match reason {
                    Some(reason) => RefResult::of(command, Some(reason)),
                    None => RefResult::of(command, apply_guarded_update(txn, command, seal)),
                })
                .collect()
        };
        Ok::<_, PackError>(results)
    });
    match applied {
        Ok(results) => report(&Ok(()), &results).map(build),
        Err(error) => {
            let results = all_failed(&commands, &messages.object_migration_failed.text());
            report(&Err(error), &results).map(build)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_push_options;
    use crate::pkt;
    use knot_types::{PushOption, PushOptions};

    fn options(raw: &[&[u8]]) -> PushOptions {
        parse_push_options(&pkt::Receive {
            commands: Vec::new(),
            options: raw.to_vec(),
            pack: &[],
            caps: pkt::Caps::default(),
        })
    }

    #[test]
    fn a_push_option_the_lexicon_rejects_never_reaches_the_event() {
        let long = vec![b'x'; 1025];
        let parsed = options(&[b"verbose-ci\n", b"", &long, b"has\nnewline", b"ci-skip\r"]);
        assert_eq!(
            parsed
                .as_slice()
                .iter()
                .map(PushOption::as_str)
                .collect::<Vec<&str>>(),
            vec!["verbose-ci", "ci-skip"],
            "parsing trims trailing end-of-line and rejects empty, oversized, and multi-line options"
        );

        let raw: Vec<Vec<u8>> = (0..PushOptions::MAX + 10)
            .map(|index| format!("option-{index}").into_bytes())
            .collect();
        let borrowed: Vec<&[u8]> = raw.iter().map(Vec::as_slice).collect();
        assert_eq!(
            options(&borrowed).as_slice().len(),
            PushOptions::MAX,
            "directives parse from the same truncated list the event reports"
        );
    }
}
