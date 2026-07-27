use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::sync::OnceLock;
use std::time::Duration;

use knot_git::{
    CommitDepth, Deepen, Filter, Haves, PackBudget, PackfileUri, Repo, ShallowCommits, Wants,
};
use knot_messages::{CountKey, FetchMessages, KnotKey};
use knot_types::{KnotHostname, ObjectCount, ObjectFormat, Oid, UnixSeconds};

use crate::error::PackError;
use crate::objects;
use crate::pkt;
use crate::{HaveOids, WantOids};

const AGENT: &[u8] = b"agent=knot/0\n";
const SELECTION_MAX_OBJECTS: ObjectCount = ObjectCount::new(16_000_000);
const SELECTION_TIME_BUDGET: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy)]
pub struct SelectionLimits {
    pub max_objects: ObjectCount,
    pub time_budget: Duration,
}

impl Default for SelectionLimits {
    fn default() -> Self {
        Self {
            max_objects: SELECTION_MAX_OBJECTS,
            time_budget: SELECTION_TIME_BUDGET,
        }
    }
}

static SELECTION_LIMITS: OnceLock<SelectionLimits> = OnceLock::new();

pub fn init_selection_limits(limits: SelectionLimits) {
    if SELECTION_LIMITS.set(limits).is_err() {
        debug_assert!(false, "selection limits initialized more than once");
    }
}

fn selection_limits() -> SelectionLimits {
    SELECTION_LIMITS.get().copied().unwrap_or_default()
}
const V0_CAPS_BASE: &str = "multi_ack_detailed no-done side-band-64k ofs-delta shallow deepen-since deepen-not filter agent=knot/0";

fn v0_caps(format: ObjectFormat) -> String {
    format!("{V0_CAPS_BASE} object-format={}", format.capability())
}

pub struct StreamOpts {
    pub side_band: bool,
    pub sideband_all: bool,
    pub no_progress: bool,
    pub thin: bool,
    pub filter: Filter,
    pub shallow_commits: Option<Vec<Oid>>,
    pub packfile_uris: Vec<PackfileUri>,
    pub emit_packfile_header: bool,
}

pub enum UploadOutcome {
    Buffered(Vec<u8>),
    Streaming {
        preamble: Vec<u8>,
        wants: WantOids,
        haves: HaveOids,
        opts: StreamOpts,
    },
}

fn write_v2_caps(buf: &mut Vec<u8>, format: ObjectFormat) -> Result<(), PackError> {
    pkt::write_data(buf, b"version 2\n")?;
    pkt::write_data(buf, AGENT)?;
    pkt::write_data(buf, b"ls-refs\n")?;
    pkt::write_data(
        buf,
        b"fetch=shallow filter wait-for-done packfile-uris sideband-all\n",
    )?;
    pkt::write_data(buf, b"server-option\n")?;
    pkt::write_data(
        buf,
        format!("object-format={}\n", format.capability()).as_bytes(),
    )?;
    pkt::write_flush(buf)?;
    Ok(())
}

pub fn advertise(repo: &Repo) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    pkt::write_data(&mut buf, b"# service=git-upload-pack\n")?;
    pkt::write_flush(&mut buf)?;
    write_v2_caps(&mut buf, repo.object_format())?;
    Ok(buf)
}

pub fn advertise_ssh(repo: &Repo) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    write_v2_caps(&mut buf, repo.object_format())?;
    Ok(buf)
}

pub fn advertise_v0(repo: &Repo) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    pkt::write_data(&mut buf, b"# service=git-upload-pack\n")?;
    pkt::write_flush(&mut buf)?;
    write_v0_advert(&mut buf, repo)?;
    Ok(buf)
}

pub fn advertise_v0_ssh(repo: &Repo) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    write_v0_advert(&mut buf, repo)?;
    Ok(buf)
}

fn write_v0_advert(buf: &mut Vec<u8>, repo: &Repo) -> Result<(), PackError> {
    let format = repo.object_format();
    let caps = v0_caps(format);
    let refs = repo.advertised_refs_for(knot_git::AdvertScope::Upload)?;
    match repo.head() {
        Some(head) => {
            pkt::write_data(
                buf,
                format!("{} HEAD\0{caps} symref=HEAD:{}\n", head.target, head.name).as_bytes(),
            )?;
            write_plain_refs(buf, &refs)?;
        }
        None => match refs.split_first() {
            Some((first, rest)) => {
                pkt::write_data(
                    buf,
                    format!("{} {}\0{caps}\n", first.target, first.name).as_bytes(),
                )?;
                write_plain_refs(buf, rest)?;
            }
            None => {
                pkt::write_data(
                    buf,
                    format!("{} capabilities^{{}}\0{caps}\n", format.null_oid()).as_bytes(),
                )?;
            }
        },
    }
    pkt::write_flush(buf)?;
    Ok(())
}

fn write_plain_refs(buf: &mut Vec<u8>, refs: &[knot_git::RefRecord]) -> Result<(), PackError> {
    refs.iter().try_for_each(|record| {
        pkt::write_data(
            buf,
            format!("{} {}\n", record.target, record.name).as_bytes(),
        )
        .map_err(PackError::from)
    })
}

pub(crate) fn fuzz(body: &[u8]) {
    if let Ok(lines) = pkt::data_payloads_all(body) {
        let _ = parse_wants(&lines);
        let _ = parse_oids(&lines, b"want ");
        let _ = parse_oids(&lines, b"have ");
        let _ = first_caps(&lines);
        let _ = parse_ls_refs_args(&lines);
    }
}

pub fn plan(repo: &Repo, body: &[u8]) -> Result<UploadOutcome, PackError> {
    let peek = pkt::data_payloads(body)?;
    match peek.first() {
        Some(line) if line.starts_with(b"command=") => plan_v2(repo, &peek),
        _ => plan_v0(repo, body),
    }
}

pub fn buffered(
    repo: &Repo,
    body: &[u8],
    messages: &FetchMessages,
    knot: &KnotHostname,
) -> Result<Vec<u8>, PackError> {
    let mut out = Vec::new();
    streamed(repo, body, messages, knot, &mut |chunk| {
        out.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(out)
}

pub fn streamed(
    repo: &Repo,
    body: &[u8],
    messages: &FetchMessages,
    knot: &KnotHostname,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    match plan(repo, body)? {
        UploadOutcome::Buffered(bytes) => sink(&bytes).map_err(PackError::from),
        UploadOutcome::Streaming {
            preamble,
            wants,
            haves,
            opts,
        } => {
            sink(&preamble)?;
            stream_pack(repo, &wants, &haves, &opts, messages, knot, sink)?;
            if opts.side_band {
                let mut flush = Vec::new();
                pkt::write_flush(&mut flush)?;
                sink(&flush)?;
            }
            Ok(())
        }
    }
}

fn plan_v2(repo: &Repo, lines: &[&[u8]]) -> Result<UploadOutcome, PackError> {
    match lines.first().copied().unwrap_or_default() {
        command if command.starts_with(b"command=ls-refs") => {
            Ok(UploadOutcome::Buffered(ls_refs(repo, lines)?))
        }
        command if command.starts_with(b"command=fetch") => plan_v2_fetch(repo, lines),
        _ => Err(PackError::Protocol(
            "unsupported protocol v2 command".to_string(),
        )),
    }
}

struct LsRefsArgs {
    symrefs: bool,
    peel: bool,
    prefixes: Vec<String>,
}

fn parse_ls_refs_args(lines: &[&[u8]]) -> LsRefsArgs {
    LsRefsArgs {
        symrefs: lines.iter().any(|line| line.starts_with(b"symrefs")),
        peel: lines.iter().any(|line| line.starts_with(b"peel")),
        prefixes: lines
            .iter()
            .filter_map(|line| {
                std::str::from_utf8(line)
                    .ok()?
                    .trim_end()
                    .strip_prefix("ref-prefix ")
                    .map(str::to_string)
            })
            .collect(),
    }
}

pub(crate) fn matches_prefix<S: AsRef<str>>(name: &str, prefixes: &[S]) -> bool {
    prefixes.is_empty()
        || prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix.as_ref()))
}

fn ls_refs(repo: &Repo, lines: &[&[u8]]) -> Result<Vec<u8>, PackError> {
    let args = parse_ls_refs_args(lines);
    let mut buf = Vec::new();
    if let Some(head) = repo.head()
        && matches_prefix("HEAD", &args.prefixes)
    {
        let mut line = format!("{} HEAD", head.target);
        if args.symrefs {
            line.push_str(&format!(" symref-target:{}", head.name));
        }
        line.push('\n');
        pkt::write_data(&mut buf, line.as_bytes())?;
    }
    repo.advertised_refs_for(knot_git::AdvertScope::Upload)?
        .iter()
        .filter(|record| matches_prefix(record.name.as_str(), &args.prefixes))
        .try_fold(&mut buf, |buf, record| {
            let mut line = format!("{} {}", record.target, record.name);
            if args.peel
                && let Some(peeled) = repo.peeled_target(record.target)?
            {
                line.push_str(&format!(" peeled:{peeled}"));
            }
            line.push('\n');
            pkt::write_data(buf, line.as_bytes())?;
            Ok::<_, PackError>(buf)
        })?;
    pkt::write_flush(&mut buf)?;
    Ok(buf)
}

fn plan_v2_fetch(repo: &Repo, lines: &[&[u8]]) -> Result<UploadOutcome, PackError> {
    let wants = parse_wants(lines)?;
    ensure_wanted(repo, &wants)?;
    let haves = HaveOids::new(parse_oids(lines, b"have "));
    let done = lines.iter().any(|line| line.starts_with(b"done"));
    let wait_for_done = lines.iter().any(|line| line.starts_with(b"wait-for-done"));
    let sideband_all = lines.iter().any(|line| line.starts_with(b"sideband-all"));
    let no_progress = lines.iter().any(|line| line.starts_with(b"no-progress"));
    let thin = lines.iter().any(|line| line.starts_with(b"thin-pack"));
    let filter = parse_filter(lines)?;
    let deepen = parse_deepen(repo, lines)?;
    let client_shallow = parse_oids(lines, b"shallow ");
    let common: HaveOids = haves
        .iter()
        .copied()
        .filter(|oid| repo.contains(*oid))
        .collect();

    let mut preamble = Vec::new();
    if !haves.is_empty() && !done {
        seg(&mut preamble, sideband_all, b"acknowledgments\n")?;
        if common.is_empty() {
            seg(&mut preamble, sideband_all, b"NAK\n")?;
            pkt::write_flush(&mut preamble)?;
            return Ok(UploadOutcome::Buffered(preamble));
        }
        common.iter().try_for_each(|oid| {
            seg(
                &mut preamble,
                sideband_all,
                format!("ACK {oid}\n").as_bytes(),
            )
        })?;
        if wait_for_done {
            pkt::write_flush(&mut preamble)?;
            return Ok(UploadOutcome::Buffered(preamble));
        }
        seg(&mut preamble, sideband_all, b"ready\n")?;
        pkt::write_delim(&mut preamble)?;
    }

    let shallow_commits = if deepen.is_shallow_request() || repo.is_shallow() {
        let plan =
            repo.shallow_walk(wants.wants(), &deepen, ShallowCommits::new(&client_shallow))?;
        seg(&mut preamble, sideband_all, b"shallow-info\n")?;
        plan.shallow.iter().try_for_each(|oid| {
            seg(
                &mut preamble,
                sideband_all,
                format!("shallow {oid}\n").as_bytes(),
            )
        })?;
        plan.unshallow.iter().try_for_each(|oid| {
            seg(
                &mut preamble,
                sideband_all,
                format!("unshallow {oid}\n").as_bytes(),
            )
        })?;
        pkt::write_delim(&mut preamble)?;
        Some(plan.commits)
    } else {
        None
    };

    Ok(UploadOutcome::Streaming {
        preamble,
        wants,
        haves: common,
        opts: StreamOpts {
            side_band: true,
            sideband_all,
            no_progress,
            thin,
            filter,
            shallow_commits,
            packfile_uris: packfile_uri_candidates(repo, lines),
            emit_packfile_header: true,
        },
    })
}

fn seg(buf: &mut Vec<u8>, sideband_all: bool, content: &[u8]) -> io::Result<()> {
    if sideband_all {
        pkt::write_band(buf, content)
    } else {
        pkt::write_data(buf, content)
    }
}

fn packfile_uri_candidates(repo: &Repo, lines: &[&[u8]]) -> Vec<PackfileUri> {
    let Some(protocols) = lines.iter().find_map(|line| {
        std::str::from_utf8(line)
            .ok()?
            .trim_end()
            .strip_prefix("packfile-uris ")
            .map(str::to_string)
    }) else {
        return Vec::new();
    };
    let allowed: Vec<&str> = protocols.split(',').map(str::trim).collect();
    repo.blob_packfile_uris()
        .into_iter()
        .filter(|candidate| {
            candidate
                .uri
                .as_str()
                .split_once("://")
                .is_some_and(|(scheme, _)| allowed.contains(&scheme))
        })
        .collect()
}

fn parse_filter(lines: &[&[u8]]) -> Result<Filter, PackError> {
    let spec = lines.iter().find_map(|line| {
        std::str::from_utf8(line)
            .ok()?
            .trim_end()
            .strip_prefix("filter ")
    });
    match spec {
        None => Ok(Filter::None),
        Some("blob:none") => Ok(Filter::BlobNone),
        Some(rest) if rest.starts_with("blob:limit=") => parse_size(&rest["blob:limit=".len()..])
            .map(Filter::BlobLimit)
            .ok_or_else(|| PackError::Protocol(format!("bad blob:limit filter: {rest}"))),
        Some(rest) if rest.starts_with("tree:") => rest["tree:".len()..]
            .parse::<u32>()
            .map(|depth| Filter::TreeDepth(knot_git::TreeDepth::new(depth)))
            .map_err(|_| PackError::Protocol(format!("bad tree filter: {rest}"))),
        Some(other) => Err(PackError::Protocol(format!("unsupported filter: {other}"))),
    }
}

fn parse_size(text: &str) -> Option<u64> {
    let (digits, scale) = match text.chars().last() {
        Some('k') | Some('K') => (&text[..text.len() - 1], 1024),
        Some('m') | Some('M') => (&text[..text.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(scale))
}

fn parse_deepen(repo: &Repo, lines: &[&[u8]]) -> Result<Deepen, PackError> {
    let value = |prefix: &str| -> Option<&str> {
        lines.iter().find_map(|line| {
            std::str::from_utf8(line)
                .ok()?
                .trim_end()
                .strip_prefix(prefix)
        })
    };
    let depth = value("deepen ")
        .and_then(|text| text.parse::<u32>().ok())
        .map(CommitDepth::new);
    let since = value("deepen-since ")
        .and_then(|text| text.parse::<i64>().ok())
        .map(UnixSeconds::new);
    let relative = lines
        .iter()
        .any(|line| line.starts_with(b"deepen-relative"));
    let not = lines
        .iter()
        .filter_map(|line| {
            std::str::from_utf8(line)
                .ok()?
                .trim_end()
                .strip_prefix("deepen-not ")
        })
        .map(|spec| resolve_commitish(repo, spec))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Deepen {
        depth,
        since,
        not,
        relative,
    })
}

fn resolve_commitish(repo: &Repo, spec: &str) -> Result<Oid, PackError> {
    if let Ok(oid) = Oid::from_hex(spec)
        && repo.contains(oid)
    {
        return Ok(oid);
    }
    let candidates = [
        spec.to_string(),
        format!("refs/{spec}"),
        format!("refs/tags/{spec}"),
        format!("refs/heads/{spec}"),
    ];
    repo.advertised_refs_for(knot_git::AdvertScope::Upload)?
        .iter()
        .find(|record| candidates.iter().any(|name| record.name.as_str() == name))
        .map(|record| record.target)
        .ok_or_else(|| PackError::Protocol(format!("deepen-not {spec}: unknown ref")))
}

fn plan_v0(repo: &Repo, body: &[u8]) -> Result<UploadOutcome, PackError> {
    let lines = pkt::data_payloads_all(body)?;
    let wants = parse_wants(&lines)?;
    ensure_wanted(repo, &wants)?;
    let haves = HaveOids::new(parse_oids(&lines, b"have "));
    let done = lines.iter().any(|line| line.starts_with(b"done"));
    let caps = first_caps(&lines);
    let side_band = caps
        .map(|caps| {
            caps.split(' ')
                .any(|cap| cap == "side-band-64k" || cap == "side-band")
        })
        .unwrap_or(false);
    let no_progress = caps
        .map(|caps| caps.split(' ').any(|cap| cap == "no-progress"))
        .unwrap_or(false);
    let thin = caps
        .map(|caps| caps.split(' ').any(|cap| cap == "thin-pack"))
        .unwrap_or(false);
    let multi_ack_detailed = caps
        .map(|caps| caps.split(' ').any(|cap| cap == "multi_ack_detailed"))
        .unwrap_or(false);
    let no_done = caps
        .map(|caps| caps.split(' ').any(|cap| cap == "no-done"))
        .unwrap_or(false);
    let filter = parse_filter(&lines)?;
    let deepen = parse_deepen(repo, &lines)?;
    let client_shallow = parse_oids(&lines, b"shallow ");
    let common: HaveOids = haves
        .iter()
        .copied()
        .filter(|oid| repo.contains(*oid))
        .collect();

    let mut preamble = Vec::new();
    let shallow_commits = if deepen.is_shallow_request() || repo.is_shallow() {
        let plan =
            repo.shallow_walk(wants.wants(), &deepen, ShallowCommits::new(&client_shallow))?;
        plan.shallow.iter().try_for_each(|oid| {
            pkt::write_data(&mut preamble, format!("shallow {oid}\n").as_bytes())
        })?;
        plan.unshallow.iter().try_for_each(|oid| {
            pkt::write_data(&mut preamble, format!("unshallow {oid}\n").as_bytes())
        })?;
        pkt::write_flush(&mut preamble)?;
        Some(plan.commits)
    } else {
        None
    };

    if haves.is_empty() && !done {
        if !(deepen.is_shallow_request() || repo.is_shallow()) {
            pkt::write_data(&mut preamble, b"NAK\n")?;
        }
        return Ok(UploadOutcome::Buffered(preamble));
    }

    let ready = multi_ack_detailed
        && !done
        && !common.is_empty()
        && common.len() == haves.len()
        && repo.wants_satisfied_by(wants.wants(), common.haves())?;

    let stream = move |preamble: Vec<u8>, common: HaveOids| UploadOutcome::Streaming {
        preamble,
        wants,
        haves: common,
        opts: StreamOpts {
            side_band,
            sideband_all: false,
            no_progress,
            thin,
            filter,
            shallow_commits,
            packfile_uris: Vec::new(),
            emit_packfile_header: false,
        },
    };

    if multi_ack_detailed {
        common.iter().try_for_each(|oid| {
            pkt::write_data(&mut preamble, format!("ACK {oid} common\n").as_bytes())
        })?;
        let last = common.as_slice().last().copied();
        if done {
            match last {
                Some(oid) => {
                    pkt::write_data(&mut preamble, format!("ACK {oid}\n").as_bytes())?;
                }
                None => pkt::write_data(&mut preamble, b"NAK\n")?,
            }
            return Ok(stream(preamble, common));
        }
        match (ready, last) {
            (true, Some(oid)) => {
                pkt::write_data(&mut preamble, format!("ACK {oid} ready\n").as_bytes())?;
                pkt::write_data(&mut preamble, b"NAK\n")?;
                if no_done {
                    pkt::write_data(&mut preamble, format!("ACK {oid}\n").as_bytes())?;
                    return Ok(stream(preamble, common));
                }
            }
            _ => pkt::write_data(&mut preamble, b"NAK\n")?,
        }
        return Ok(UploadOutcome::Buffered(preamble));
    }

    match common.as_slice().first() {
        Some(oid) => pkt::write_data(&mut preamble, format!("ACK {oid}\n").as_bytes())?,
        None => pkt::write_data(&mut preamble, b"NAK\n")?,
    }
    match done {
        true => Ok(stream(preamble, common)),
        false => Ok(UploadOutcome::Buffered(preamble)),
    }
}

pub fn stream_pack(
    repo: &Repo,
    wants: &WantOids,
    haves: &HaveOids,
    opts: &StreamOpts,
    messages: &FetchMessages,
    knot: &KnotHostname,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    let progress = opts.side_band && !opts.no_progress;
    if opts.shallow_commits.is_none()
        && haves.is_empty()
        && opts.filter == Filter::None
        && opts.packfile_uris.is_empty()
    {
        if let Ok(Some(pack)) = knot_git::verbatim_clone_pack(repo, wants.wants()) {
            write_packfile_header(opts, sink)?;
            return stream_verbatim_pack(pack, opts, progress, messages, knot, sink);
        }
        if let Ok(Some(oids)) = knot_git::reachable_via_bitmap(repo, wants.wants(), Haves::new(&[]))
        {
            write_packfile_header(opts, sink)?;
            return stream_object_set(repo, oids, opts, progress, messages, knot, sink);
        }
        write_packfile_header(opts, sink)?;
        return stream_full_clone(repo, wants.as_slice(), opts, progress, messages, knot, sink);
    }
    let budget = selection_budget();
    let mut selection = match &opts.shallow_commits {
        Some(commits) => repo.select_shallow_objects(
            wants.wants(),
            ShallowCommits::new(commits),
            haves.haves(),
            opts.filter,
            budget,
        )?,
        None => {
            repo.select_pack_objects_filtered(wants.wants(), haves.haves(), opts.filter, budget)?
        }
    };
    if !opts.packfile_uris.is_empty() {
        offload_packfile_uris(
            &mut selection.send,
            &opts.packfile_uris,
            opts.sideband_all,
            sink,
        )?;
    }
    write_packfile_header(opts, sink)?;
    let count = ObjectCount::new(selection.send.len());
    emit_preamble(progress, messages, knot, count, sink)?;
    {
        let mut pack_sink = PackSink {
            side_band: opts.side_band,
            sink: &mut *sink,
        };
        let thin_bases = opts.thin.then_some(&selection.client_has);
        objects::write_pack(
            &repo.objects_dir(),
            selection.send,
            thin_bases,
            &mut pack_sink,
            repo.object_format().kind(),
        )?;
    }
    emit_total(progress, messages, count, sink)
}

fn emit_progress(
    progress: bool,
    lines: impl FnOnce() -> Vec<String>,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    if !progress {
        return Ok(());
    }
    let rendered = lines();
    if rendered.is_empty() {
        return Ok(());
    }
    let mut buf = Vec::new();
    rendered.iter().try_for_each(|line| {
        format!("{line}\n")
            .into_bytes()
            .chunks(pkt::MAX_BAND)
            .try_for_each(|chunk| pkt::write_band_progress(&mut buf, chunk))
    })?;
    sink(&buf).map_err(PackError::from)
}

fn emit_preamble(
    progress: bool,
    messages: &FetchMessages,
    knot: &KnotHostname,
    count: ObjectCount,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    emit_progress(
        progress,
        || {
            messages
                .motd
                .lines(|KnotKey::Knot| knot.as_str().to_string())
                .into_iter()
                .chain(
                    messages
                        .enumerating
                        .lines(|CountKey::Count| count.to_string()),
                )
                .collect()
        },
        sink,
    )
}

fn emit_total(
    progress: bool,
    messages: &FetchMessages,
    count: ObjectCount,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    emit_progress(
        progress,
        || messages.total.lines(|CountKey::Count| count.to_string()),
        sink,
    )
}

fn write_packfile_header(
    opts: &StreamOpts,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    if !opts.emit_packfile_header {
        return Ok(());
    }
    let mut buf = Vec::new();
    seg(&mut buf, opts.sideband_all, b"packfile\n")?;
    sink(&buf)?;
    Ok(())
}

fn offload_packfile_uris(
    send: &mut Vec<Oid>,
    candidates: &[PackfileUri],
    sideband_all: bool,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    let present: std::collections::HashSet<Oid> = send.iter().copied().collect();
    let kept: Vec<&PackfileUri> = candidates
        .iter()
        .filter(|candidate| present.contains(&candidate.oid))
        .collect();
    if kept.is_empty() {
        return Ok(());
    }
    let excluded: std::collections::HashSet<Oid> =
        kept.iter().map(|candidate| candidate.oid).collect();
    send.retain(|oid| !excluded.contains(oid));
    let mut buf = Vec::new();
    seg(&mut buf, sideband_all, b"packfile-uris\n")?;
    kept.iter().try_for_each(|candidate| {
        seg(
            &mut buf,
            sideband_all,
            format!(
                "{} {}\n",
                candidate.pack_hash.as_str(),
                candidate.uri.as_str()
            )
            .as_bytes(),
        )
    })?;
    pkt::write_delim(&mut buf)?;
    sink(&buf)?;
    Ok(())
}

fn stream_verbatim_pack(
    mut file: std::fs::File,
    opts: &StreamOpts,
    progress: bool,
    messages: &FetchMessages,
    knot: &KnotHostname,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    if &header[..4] != b"PACK" {
        return Err(PackError::Pack(
            "reused pack is missing its PACK signature".to_string(),
        ));
    }
    let count = ObjectCount::from(u32::from_be_bytes([
        header[8], header[9], header[10], header[11],
    ]));
    emit_preamble(progress, messages, knot, count, sink)?;
    {
        let mut pack_sink = PackSink {
            side_band: opts.side_band,
            sink: &mut *sink,
        };
        pack_sink.write_all(&header)?;
        io::copy(&mut file, &mut pack_sink)?;
    }
    emit_total(progress, messages, count, sink)
}

fn stream_object_set(
    repo: &Repo,
    oids: Vec<Oid>,
    opts: &StreamOpts,
    progress: bool,
    messages: &FetchMessages,
    knot: &KnotHostname,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    let count = ObjectCount::new(oids.len());
    emit_preamble(progress, messages, knot, count, sink)?;
    {
        let mut pack_sink = PackSink {
            side_band: opts.side_band,
            sink: &mut *sink,
        };
        objects::write_pack(
            &repo.objects_dir(),
            oids,
            None,
            &mut pack_sink,
            repo.object_format().kind(),
        )?;
    }
    emit_total(progress, messages, count, sink)
}

fn stream_full_clone(
    repo: &Repo,
    wants: &[Oid],
    opts: &StreamOpts,
    progress: bool,
    messages: &FetchMessages,
    knot: &KnotHostname,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    let limits = selection_limits();
    let stall = limits.time_budget;
    let roots = repo.clone_roots(wants, PackBudget::new(limits.max_objects, stall))?;
    let pack = objects::count_expanded(
        &repo.objects_dir(),
        roots,
        limits.max_objects,
        stall,
        repo.object_format().kind(),
    )?;
    let count = ObjectCount::new(pack.len());
    emit_preamble(progress, messages, knot, count, sink)?;
    {
        let mut pack_sink = PackSink {
            side_band: opts.side_band,
            sink: &mut *sink,
        };
        objects::write_expanded(pack, &mut pack_sink)?;
    }
    emit_total(progress, messages, count, sink)
}

struct PackSink<'a> {
    side_band: bool,
    sink: &'a mut dyn FnMut(&[u8]) -> io::Result<()>,
}

impl Write for PackSink<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.side_band {
            data.chunks(pkt::MAX_BAND).try_for_each(|chunk| {
                let mut framed = Vec::with_capacity(chunk.len() + 5);
                pkt::write_band(&mut framed, chunk)?;
                (self.sink)(&framed)
            })?;
        } else {
            (self.sink)(data)?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn selection_budget() -> PackBudget {
    let limits = selection_limits();
    PackBudget::new(limits.max_objects, limits.time_budget)
}

fn ensure_wanted(repo: &Repo, wants: &WantOids) -> Result<(), PackError> {
    let tips: Vec<Oid> = repo
        .advertised_refs_for(knot_git::AdvertScope::Upload)?
        .iter()
        .map(|record| record.target)
        .collect();
    let advertised: HashSet<Oid> = tips.iter().copied().collect();
    if wants.iter().all(|want| advertised.contains(want)) {
        return Ok(());
    }
    let commit_closure = repo.reachable_commits(&tips, selection_budget())?;
    let unresolved: Vec<Oid> = wants
        .iter()
        .copied()
        .filter(|want| !advertised.contains(want) && !commit_closure.contains(want))
        .collect();
    if unresolved.is_empty() {
        return Ok(());
    }
    let reachable: HashSet<Oid> = repo
        .select_pack_objects_filtered(
            Wants::new(&tips),
            Haves::new(&[]),
            Filter::None,
            selection_budget(),
        )?
        .send
        .into_iter()
        .collect();
    match unresolved.iter().find(|want| !reachable.contains(want)) {
        Some(hidden) => Err(PackError::Protocol(format!(
            "want {hidden} isn't reachable from public ref"
        ))),
        None => Ok(()),
    }
}

fn first_caps<'a>(lines: &[&'a [u8]]) -> Option<&'a str> {
    let line = lines.iter().find(|line| line.starts_with(b"want "))?;
    let text = std::str::from_utf8(line).ok()?.trim_end();
    text.strip_prefix("want ")?
        .split_once(' ')
        .map(|(_oid, caps)| caps)
}

fn parse_wants(lines: &[&[u8]]) -> Result<WantOids, PackError> {
    lines
        .iter()
        .filter_map(|line| line.strip_prefix(b"want "))
        .map(|rest| {
            let hex = rest
                .split(|byte| *byte == b' ' || *byte == b'\n')
                .next()
                .unwrap_or_default();
            std::str::from_utf8(hex)
                .ok()
                .and_then(|text| Oid::from_hex(text).ok())
                .ok_or_else(|| PackError::Protocol("malformed want line".to_string()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(WantOids::new)
}

fn parse_oids(lines: &[&[u8]], prefix: &[u8]) -> Vec<Oid> {
    lines
        .iter()
        .filter_map(|line| line.strip_prefix(prefix))
        .filter_map(|rest| {
            let hex = rest.split(|byte| *byte == b' ' || *byte == b'\n').next()?;
            let hex = std::str::from_utf8(hex).ok()?;
            Oid::from_hex(hex).ok()
        })
        .collect()
}
