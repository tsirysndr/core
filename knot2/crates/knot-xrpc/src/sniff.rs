const SNIFF_LIMIT: usize = 512;

fn is_ws(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_tt(byte: u8) -> bool {
    matches!(byte, b' ' | b'>')
}

enum Sig {
    Exact(&'static [u8], &'static str),
    Masked {
        mask: &'static [u8],
        pat: &'static [u8],
        skip_ws: bool,
        ct: &'static str,
    },
    Html(&'static [u8]),
    Mp4,
    Text,
}

impl Sig {
    fn detect(&self, data: &[u8], first_non_ws: usize) -> Option<&'static str> {
        match self {
            Sig::Exact(sig, ct) => data.starts_with(sig).then_some(*ct),
            Sig::Masked {
                mask,
                pat,
                skip_ws,
                ct,
            } => {
                let data = if *skip_ws {
                    &data[first_non_ws..]
                } else {
                    data
                };
                (mask.len() == pat.len()
                    && data.len() >= pat.len()
                    && pat
                        .iter()
                        .zip(mask.iter())
                        .enumerate()
                        .all(|(index, (byte, mask))| data[index] & mask == *byte))
                .then_some(*ct)
            }
            Sig::Html(tag) => {
                let data = &data[first_non_ws..];
                (data.len() > tag.len()
                    && tag.iter().enumerate().all(|(index, byte)| {
                        let candidate = data[index];
                        let candidate = match byte.is_ascii_uppercase() {
                            true => candidate & 0xDF,
                            false => candidate,
                        };
                        *byte == candidate
                    })
                    && is_tt(data[tag.len()]))
                .then_some("text/html; charset=utf-8")
            }
            Sig::Mp4 => mp4(data),
            Sig::Text => text(data, first_non_ws),
        }
    }
}

fn mp4(data: &[u8]) -> Option<&'static str> {
    if data.len() < 12 {
        return None;
    }
    let box_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < box_size || !box_size.is_multiple_of(4) || &data[4..8] != b"ftyp" {
        return None;
    }
    (8..box_size)
        .step_by(4)
        .filter(|start| *start != 12)
        .any(|start| &data[start..start + 3] == b"mp4")
        .then_some("video/mp4")
}

fn text(data: &[u8], first_non_ws: usize) -> Option<&'static str> {
    data[first_non_ws..]
        .iter()
        .all(|byte| !matches!(byte, 0x00..=0x08 | 0x0b | 0x0e..=0x1a | 0x1c..=0x1f))
        .then_some("text/plain; charset=utf-8")
}

const SIGNATURES: &[Sig] = &[
    Sig::Html(b"<!DOCTYPE HTML"),
    Sig::Html(b"<HTML"),
    Sig::Html(b"<HEAD"),
    Sig::Html(b"<SCRIPT"),
    Sig::Html(b"<IFRAME"),
    Sig::Html(b"<H1"),
    Sig::Html(b"<DIV"),
    Sig::Html(b"<FONT"),
    Sig::Html(b"<TABLE"),
    Sig::Html(b"<A"),
    Sig::Html(b"<STYLE"),
    Sig::Html(b"<TITLE"),
    Sig::Html(b"<B"),
    Sig::Html(b"<BODY"),
    Sig::Html(b"<BR"),
    Sig::Html(b"<P"),
    Sig::Html(b"<!--"),
    Sig::Masked {
        mask: b"\xff\xff\xff\xff\xff",
        pat: b"<?xml",
        skip_ws: true,
        ct: "text/xml; charset=utf-8",
    },
    Sig::Exact(b"%PDF-", "application/pdf"),
    Sig::Exact(b"%!PS-Adobe-", "application/postscript"),
    Sig::Masked {
        mask: b"\xff\xff\x00\x00",
        pat: b"\xfe\xff\x00\x00",
        skip_ws: false,
        ct: "text/plain; charset=utf-16be",
    },
    Sig::Masked {
        mask: b"\xff\xff\x00\x00",
        pat: b"\xff\xfe\x00\x00",
        skip_ws: false,
        ct: "text/plain; charset=utf-16le",
    },
    Sig::Masked {
        mask: b"\xff\xff\xff\x00",
        pat: b"\xef\xbb\xbf\x00",
        skip_ws: false,
        ct: "text/plain; charset=utf-8",
    },
    Sig::Exact(b"\x00\x00\x01\x00", "image/x-icon"),
    Sig::Exact(b"\x00\x00\x02\x00", "image/x-icon"),
    Sig::Exact(b"BM", "image/bmp"),
    Sig::Exact(b"GIF87a", "image/gif"),
    Sig::Exact(b"GIF89a", "image/gif"),
    Sig::Masked {
        mask: b"\xff\xff\xff\xff\x00\x00\x00\x00\xff\xff\xff\xff\xff\xff",
        pat: b"RIFF\x00\x00\x00\x00WEBPVP",
        skip_ws: false,
        ct: "image/webp",
    },
    Sig::Exact(b"\x89PNG\x0d\x0a\x1a\x0a", "image/png"),
    Sig::Exact(b"\xff\xd8\xff", "image/jpeg"),
    Sig::Masked {
        mask: b"\xff\xff\xff\xff\x00\x00\x00\x00\xff\xff\xff\xff",
        pat: b"FORM\x00\x00\x00\x00AIFF",
        skip_ws: false,
        ct: "audio/aiff",
    },
    Sig::Masked {
        mask: b"\xff\xff\xff",
        pat: b"ID3",
        skip_ws: false,
        ct: "audio/mpeg",
    },
    Sig::Masked {
        mask: b"\xff\xff\xff\xff\xff",
        pat: b"OggS\x00",
        skip_ws: false,
        ct: "application/ogg",
    },
    Sig::Masked {
        mask: b"\xff\xff\xff\xff\xff\xff\xff\xff",
        pat: b"MThd\x00\x00\x00\x06",
        skip_ws: false,
        ct: "audio/midi",
    },
    Sig::Masked {
        mask: b"\xff\xff\xff\xff\x00\x00\x00\x00\xff\xff\xff\xff",
        pat: b"RIFF\x00\x00\x00\x00AVI ",
        skip_ws: false,
        ct: "video/avi",
    },
    Sig::Masked {
        mask: b"\xff\xff\xff\xff\x00\x00\x00\x00\xff\xff\xff\xff",
        pat: b"RIFF\x00\x00\x00\x00WAVE",
        skip_ws: false,
        ct: "audio/wave",
    },
    Sig::Mp4,
    Sig::Exact(b"\x1a\x45\xdf\xa3", "video/webm"),
    Sig::Masked {
        mask: b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xff\xff",
        pat: b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00LP",
        skip_ws: false,
        ct: "application/vnd.ms-fontobject",
    },
    Sig::Exact(b"\x00\x01\x00\x00", "font/ttf"),
    Sig::Exact(b"OTTO", "font/otf"),
    Sig::Exact(b"ttcf", "font/collection"),
    Sig::Exact(b"wOFF", "font/woff"),
    Sig::Exact(b"wOF2", "font/woff2"),
    Sig::Exact(b"\x1f\x8b\x08", "application/x-gzip"),
    Sig::Exact(b"PK\x03\x04", "application/zip"),
    Sig::Exact(b"Rar!\x1a\x07\x00", "application/x-rar-compressed"),
    Sig::Exact(b"Rar!\x1a\x07\x01\x00", "application/x-rar-compressed"),
    Sig::Exact(b"\x00\x61\x73\x6d", "application/wasm"),
    Sig::Text,
];

pub(crate) fn detect_content_type(content: &[u8]) -> &'static str {
    let data = &content[..content.len().min(SNIFF_LIMIT)];
    let first_non_ws = data
        .iter()
        .position(|byte| !is_ws(*byte))
        .unwrap_or(data.len());
    SIGNATURES
        .iter()
        .find_map(|sig| sig.detect(data, first_non_ws))
        .unwrap_or("application/octet-stream")
}

pub(crate) fn override_by_extension(path: &str, detected: &'static str) -> &'static str {
    let extension = path.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "jxl" => "image/jxl",
        "heic" | "heif" => "image/heif",
        _ => detected,
    }
}

pub(crate) fn is_textual_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/javascript"
                | "application/ecmascript"
        )
}

#[cfg(test)]
mod tests {
    use super::detect_content_type;

    #[test]
    fn detects_common_content_signatures() {
        let cases: &[(&[u8], &str)] = &[
            (b"  <!DOCTYPE html>\n", "text/html; charset=utf-8"),
            (b"<html><body>", "text/html; charset=utf-8"),
            (b"<a href>", "text/html; charset=utf-8"),
            (b"\n\t<?xml version=\"1.0\"?>", "text/xml; charset=utf-8"),
            (b"\xfe\xff\x00h", "text/plain; charset=utf-16be"),
            (b"\xef\xbb\xbfhello", "text/plain; charset=utf-8"),
            (b"\x89PNG\x0d\x0a\x1a\x0a", "image/png"),
            (b"GIF89a", "image/gif"),
            (b"fn main() {}\n", "text/plain; charset=utf-8"),
            (b"\x00\x01\x02\x03", "application/octet-stream"),
            (b"", "text/plain; charset=utf-8"),
        ];
        cases.iter().for_each(|(input, expected)| {
            assert_eq!(detect_content_type(input), *expected);
        });
    }
}
