use std::io;

use gix_packetline::{Channel, blocking_io::encode};

pub const MAX_BAND: usize = 65515;

pub enum Frame<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    ResponseEnd,
}

fn hex4(prefix: &[u8]) -> io::Result<u16> {
    std::str::from_utf8(prefix)
        .ok()
        .and_then(|text| u16::from_str_radix(text, 16).ok())
        .ok_or_else(|| io::Error::other("invalid pkt-line length prefix"))
}

pub fn frames(
    input: &[u8],
    stop_after_flushes: Option<usize>,
) -> impl Iterator<Item = io::Result<(Frame<'_>, usize)>> + '_ {
    let mut pos = 0usize;
    let mut flushes = 0usize;
    let mut stopped = false;
    std::iter::from_fn(move || {
        (!stopped && pos + 4 <= input.len()).then(|| {
            let frame = hex4(&input[pos..pos + 4]).and_then(|len| {
                pos += 4;
                match len {
                    0 => {
                        flushes += 1;
                        stopped = stop_after_flushes == Some(flushes);
                        Ok((Frame::Flush, pos))
                    }
                    1 => Ok((Frame::Delim, pos)),
                    2 => Ok((Frame::ResponseEnd, pos)),
                    3 => Err(io::Error::other("invalid pkt-line length 3")),
                    n => {
                        let end = pos - 4 + usize::from(n);
                        (end <= input.len())
                            .then(|| {
                                let payload = &input[pos..end];
                                pos = end;
                                (Frame::Data(payload), end)
                            })
                            .ok_or_else(|| io::Error::other("truncated pkt-line"))
                    }
                }
            });
            stopped |= frame.is_err();
            frame
        })
    })
}

pub fn data_payloads(input: &[u8]) -> io::Result<Vec<&[u8]>> {
    collect_data(input, Some(1))
}

pub fn data_payloads_all(input: &[u8]) -> io::Result<Vec<&[u8]>> {
    collect_data(input, None)
}

fn collect_data(input: &[u8], stop_after_flushes: Option<usize>) -> io::Result<Vec<&[u8]>> {
    frames(input, stop_after_flushes)
        .filter_map(|item| match item {
            Ok((Frame::Data(payload), _)) => Some(Ok(payload)),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub atomic: bool,
    pub side_band_64k: bool,
    pub push_options: bool,
}

pub(crate) fn first_command(input: &[u8]) -> Option<&[u8]> {
    frames(input, Some(1)).find_map(|item| match item {
        Ok((Frame::Data(payload), _)) if !is_preamble(payload) => Some(payload),
        _ => None,
    })
}

fn is_preamble(line: &[u8]) -> bool {
    line.starts_with(b"shallow ")
}

pub(crate) fn parse_caps(first_command: &[u8]) -> Caps {
    let caps = first_command
        .split(|byte| *byte == 0)
        .nth(1)
        .and_then(|caps| std::str::from_utf8(caps).ok())
        .unwrap_or_default();
    let has = |needle: &str| caps.split_whitespace().any(|cap| cap == needle);
    Caps {
        atomic: has("atomic"),
        side_band_64k: has("side-band-64k"),
        push_options: has("push-options"),
    }
}

pub struct Receive<'a> {
    pub commands: Vec<&'a [u8]>,
    pub options: Vec<&'a [u8]>,
    pub pack: &'a [u8],
    pub caps: Caps,
}

pub fn split_receive(input: &[u8]) -> io::Result<Receive<'_>> {
    let caps = first_command(input).map(parse_caps).unwrap_or_default();
    let boundary = if caps.push_options { 2 } else { 1 };
    frames(input, Some(boundary))
        .try_fold(
            (Vec::new(), Vec::new(), 0usize, None),
            |(mut commands, mut options, flushes, end), item| {
                item.map(|(frame, at)| match frame {
                    Frame::Data(payload) if flushes == 0 && !is_preamble(payload) => {
                        commands.push(payload);
                        (commands, options, flushes, end)
                    }
                    Frame::Data(payload) if flushes == 1 && caps.push_options => {
                        options.push(payload);
                        (commands, options, flushes, end)
                    }
                    Frame::Data(_) => (commands, options, flushes, end),
                    Frame::Flush => (commands, options, flushes + 1, Some(at)),
                    _ => (commands, options, flushes, end),
                })
            },
        )
        .map(|(commands, options, _flushes, end)| Receive {
            commands,
            options,
            caps,
            pack: &input[end.unwrap_or(input.len())..],
        })
}

pub fn write_data(buf: &mut Vec<u8>, payload: &[u8]) -> io::Result<()> {
    encode::data_to_write(payload, buf).map(|_| ())
}

pub fn write_flush(buf: &mut Vec<u8>) -> io::Result<()> {
    encode::flush_to_write(buf).map(|_| ())
}

pub fn write_delim(buf: &mut Vec<u8>) -> io::Result<()> {
    encode::delim_to_write(buf).map(|_| ())
}

pub fn write_band(buf: &mut Vec<u8>, chunk: &[u8]) -> io::Result<()> {
    encode::band_to_write(Channel::Data, chunk, buf).map(|_| ())
}

pub fn write_band_progress(buf: &mut Vec<u8>, message: &[u8]) -> io::Result<()> {
    encode::band_to_write(Channel::Progress, message, buf).map(|_| ())
}

pub fn write_band_error(buf: &mut Vec<u8>, message: &[u8]) -> io::Result<()> {
    encode::band_to_write(Channel::Error, message, buf).map(|_| ())
}

pub fn frame_report(report: &[u8], messages: &[String], side_band: bool) -> Vec<u8> {
    if !side_band {
        return report.to_vec();
    }
    let mut buf = Vec::new();
    report
        .chunks(MAX_BAND)
        .for_each(|chunk| write_band(&mut buf, chunk).expect("band write to vec never fails"));
    messages.iter().for_each(|message| {
        format!("{message}\n")
            .into_bytes()
            .chunks(MAX_BAND)
            .for_each(|chunk| {
                write_band_progress(&mut buf, chunk).expect("band write to vec never fails")
            });
    });
    write_flush(&mut buf).expect("flush write to vec never fails");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_line(caps: &str) -> Vec<u8> {
        let mut line = b"\
            0000000000000000000000000000000000000000 \
            1111111111111111111111111111111111111111 refs/heads/main"
            .to_vec();
        line.push(0);
        line.extend_from_slice(caps.as_bytes());
        line.push(b'\n');
        line
    }

    #[test]
    fn a_malformed_length_prefix_terminates_instead_of_spinning() {
        let garbage = b"zzzz this is not a pkt-line stream at all";
        assert_eq!(
            frames(garbage, None).count(),
            1,
            "bad length prefix yields one error frame then the stream ends"
        );
        assert!(
            first_command(garbage).is_none(),
            "no command is parsed out of garbage, and scan does not loop"
        );
        assert!(
            split_receive(garbage).is_err(),
            "malformed prefix is a parse error, never an infinite loop"
        );
    }

    #[test]
    fn split_receive_skips_the_push_options_section_before_the_pack() {
        let mut body = Vec::new();
        write_data(
            &mut body,
            &command_line("report-status side-band-64k push-options"),
        )
        .unwrap();
        write_flush(&mut body).unwrap();
        write_data(&mut body, b"ci-skip").unwrap();
        write_data(&mut body, b"verbose-ci").unwrap();
        write_flush(&mut body).unwrap();
        body.extend_from_slice(b"PACKreal-pack-bytes");

        let parsed = split_receive(&body).unwrap();
        assert!(parsed.caps.push_options);
        assert!(parsed.caps.side_band_64k);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(parsed.options, vec![&b"ci-skip"[..], &b"verbose-ci"[..]]);
        assert_eq!(parsed.pack, b"PACKreal-pack-bytes");
    }

    #[test]
    fn split_receive_reads_caps_past_a_shallow_preamble_line() {
        let mut body = Vec::new();
        write_data(
            &mut body,
            b"shallow 1111111111111111111111111111111111111111\n",
        )
        .unwrap();
        write_data(&mut body, &command_line("report-status side-band-64k")).unwrap();
        write_flush(&mut body).unwrap();
        body.extend_from_slice(b"PACKbytes");

        let parsed = split_receive(&body).unwrap();
        assert!(
            parsed.caps.side_band_64k,
            "capabilities come from the command line, not the shallow preamble"
        );
        assert_eq!(
            parsed.commands.len(),
            1,
            "the shallow line is not a command"
        );
        assert_eq!(parsed.pack, b"PACKbytes");
    }

    #[test]
    fn split_receive_without_push_options_starts_the_pack_after_the_command_flush() {
        let mut body = Vec::new();
        write_data(&mut body, &command_line("report-status side-band-64k")).unwrap();
        write_flush(&mut body).unwrap();
        body.extend_from_slice(b"PACKbytes");

        let parsed = split_receive(&body).unwrap();
        assert!(!parsed.caps.push_options);
        assert!(parsed.options.is_empty());
        assert_eq!(parsed.pack, b"PACKbytes");
    }

    #[test]
    fn frame_report_muxes_the_report_on_band_one_and_messages_on_band_two() {
        let report = b"unpack ok\n";
        let messages = vec!["hello there".to_string()];
        let framed = frame_report(report, &messages, true);

        let bands: Vec<(u8, Vec<u8>)> = frames(&framed, None)
            .filter_map(|item| match item {
                Ok((Frame::Data(payload), _)) => Some((payload[0], payload[1..].to_vec())),
                _ => None,
            })
            .collect();
        assert_eq!(bands[0].0, 1, "report rides band 1");
        assert_eq!(bands[0].1, report);
        assert_eq!(bands[1].0, 2, "message rides band 2");
        assert_eq!(bands[1].1, b"hello there\n");
        assert!(framed.ends_with(b"0000"), "outer flush closes the stream");
    }

    #[test]
    fn frame_report_passes_through_raw_without_side_band() {
        let report = b"unpack ok\n0000";
        assert_eq!(
            frame_report(report, &["dropped".to_string()], false),
            report
        );
    }
}
