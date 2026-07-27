use std::io::Write;

use axum::http::{HeaderMap, HeaderValue, Method, header};
use knot_git::{Filter, RefRecord, Repo};
use knot_runtime::{HttpRequest, HttpResponse, HttpTransport, NetworkError};
use knot_types::{HttpStatus, Oid, RefName};
use url::Url;

use crate::error::PackError;
use crate::pkt::{self, Frame};
use crate::{HaveOids, WantOids};

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("upstream url: {0}")]
    Url(String),
    #[error("upstream network: {0}")]
    Network(#[from] NetworkError),
    #[error("upstream returned http status {0}")]
    Status(HttpStatus),
    #[error("upstream protocol: {0}")]
    Protocol(String),
    #[error("upstream reported: {0}")]
    Remote(String),
    #[error("fetched pack exceeds {limit} bytes")]
    PackTooLarge { limit: u64 },
    #[error(transparent)]
    Pack(#[from] PackError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRefs {
    pub head_symref: Option<RefName>,
    pub refs: Vec<RefRecord>,
}

impl UpstreamRefs {
    pub fn tips(&self) -> Vec<Oid> {
        let mut tips: Vec<Oid> = self.refs.iter().map(|record| record.target).collect();
        tips.sort_unstable();
        tips.dedup();
        tips
    }

    pub fn find(&self, name: &RefName) -> Option<Oid> {
        self.refs
            .iter()
            .find(|record| record.name == *name)
            .map(|record| record.target)
    }
}

fn protocol(message: impl Into<String>) -> FetchError {
    FetchError::Protocol(message.into())
}

fn endpoint(base: &Url, suffix: &str) -> Result<Url, FetchError> {
    let trimmed = base.as_str().trim_end_matches('/');
    Url::parse(&format!("{trimmed}/{suffix}")).map_err(|error| FetchError::Url(error.to_string()))
}

fn headers_v2(content_type: Option<&'static str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("git-protocol", HeaderValue::from_static("version=2"));
    if let Some(content_type) = content_type {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    headers
}

async fn execute(
    http: &dyn HttpTransport,
    request: HttpRequest,
) -> Result<HttpResponse, FetchError> {
    let response = http.execute(request).await?;
    if !response.status.is_success() {
        return Err(FetchError::Status(HttpStatus::new(
            response.status.as_u16(),
        )));
    }
    Ok(response)
}

pub fn parse_advertisement(body: &[u8]) -> Result<(), FetchError> {
    let lines = pkt::data_payloads_all(body).map_err(|error| protocol(error.to_string()))?;
    let lines: Vec<&str> = lines
        .iter()
        .map(|line| std::str::from_utf8(line).unwrap_or_default().trim_end())
        .collect();
    let has = |name: &str| {
        lines
            .iter()
            .any(|line| *line == name || line.starts_with(&format!("{name}=")))
    };
    if !has("version 2") {
        return Err(protocol("upstream doesn't speak git protocol v2"));
    }
    if !has("ls-refs") || !has("fetch") {
        return Err(protocol("upstream is missing ls-refs or fetch v2 command"));
    }
    Ok(())
}

pub fn ls_refs_request(prefixes: &[&str]) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    pkt::write_data(&mut buf, b"command=ls-refs\n")?;
    pkt::write_data(&mut buf, b"agent=knot/0\n")?;
    pkt::write_delim(&mut buf)?;
    pkt::write_data(&mut buf, b"symrefs\n")?;
    prefixes.iter().try_for_each(|prefix| {
        pkt::write_data(&mut buf, format!("ref-prefix {prefix}\n").as_bytes())
    })?;
    pkt::write_flush(&mut buf)?;
    Ok(buf)
}

pub fn parse_ls_refs(body: &[u8]) -> Result<UpstreamRefs, FetchError> {
    let lines = pkt::data_payloads(body).map_err(|error| protocol(error.to_string()))?;
    lines.iter().try_fold(
        UpstreamRefs {
            head_symref: None,
            refs: Vec::new(),
        },
        |mut refs, line| {
            let text = std::str::from_utf8(line)
                .map_err(|_| protocol("ref line isn't utf-8"))?
                .trim_end();
            if let Some(message) = text.strip_prefix("ERR ") {
                return Err(FetchError::Remote(message.to_string()));
            }
            let (oid, rest) = text
                .split_once(' ')
                .ok_or_else(|| protocol(format!("malformed ref line: {text}")))?;
            let target =
                Oid::from_hex(oid).map_err(|_| protocol(format!("malformed ref oid: {oid}")))?;
            let mut attributes = rest.split(' ');
            match attributes.next() {
                Some("HEAD") => {
                    refs.head_symref = attributes
                        .find_map(|attribute| attribute.strip_prefix("symref-target:"))
                        .and_then(|symref| RefName::new(symref).ok());
                }
                Some(name) => {
                    if let Ok(name) = RefName::new(name) {
                        refs.refs.push(RefRecord { name, target });
                    }
                }
                None => return Err(protocol(format!("malformed ref line: {text}"))),
            }
            Ok(refs)
        },
    )
}

pub fn fetch_request(wants: &WantOids, haves: &HaveOids) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    pkt::write_data(&mut buf, b"command=fetch\n")?;
    pkt::write_data(&mut buf, b"agent=knot/0\n")?;
    pkt::write_delim(&mut buf)?;
    pkt::write_data(&mut buf, b"no-progress\n")?;
    pkt::write_data(&mut buf, b"ofs-delta\n")?;
    wants
        .iter()
        .try_for_each(|want| pkt::write_data(&mut buf, format!("want {want}\n").as_bytes()))?;
    haves
        .iter()
        .try_for_each(|have| pkt::write_data(&mut buf, format!("have {have}\n").as_bytes()))?;
    pkt::write_data(&mut buf, b"done\n")?;
    pkt::write_flush(&mut buf)?;
    Ok(buf)
}

pub fn parse_fetch_response(body: &[u8], max_pack_bytes: u64) -> Result<Vec<u8>, FetchError> {
    let (pack, in_packfile) = pkt::frames(body, None)
        .map(|frame| frame.map_err(|error| protocol(error.to_string())))
        .try_fold(
            (Vec::new(), false),
            |(mut pack, in_packfile), frame| match (frame?.0, in_packfile) {
                (Frame::Data(payload), false) => {
                    if let Some(message) = payload
                        .strip_prefix(b"ERR ".as_slice())
                        .map(|rest| String::from_utf8_lossy(rest).trim_end().to_string())
                    {
                        return Err(FetchError::Remote(message));
                    }
                    let entered =
                        payload.strip_suffix(b"\n".as_slice()).unwrap_or(payload) == b"packfile";
                    Ok((pack, entered))
                }
                (Frame::Data(payload), true) => match payload.split_first() {
                    Some((1, data)) => {
                        if pack.len() as u64 + data.len() as u64 > max_pack_bytes {
                            return Err(FetchError::PackTooLarge {
                                limit: max_pack_bytes,
                            });
                        }
                        pack.extend_from_slice(data);
                        Ok((pack, true))
                    }
                    Some((2, _)) => Ok((pack, true)),
                    Some((3, message)) => Err(FetchError::Remote(
                        String::from_utf8_lossy(message).trim_end().to_string(),
                    )),
                    _ => Err(protocol("empty sideband frame in packfile section")),
                },
                (_, in_packfile) => Ok((pack, in_packfile)),
            },
        )?;
    if !in_packfile {
        return Err(protocol("upstream response has no packfile section"));
    }
    Ok(pack)
}

pub async fn remote_refs(
    http: &dyn HttpTransport,
    base: &Url,
    prefixes: &[&str],
) -> Result<UpstreamRefs, FetchError> {
    let advertise = endpoint(base, "info/refs?service=git-upload-pack")?;
    let response = execute(
        http,
        HttpRequest {
            method: Method::GET,
            url: advertise,
            headers: headers_v2(None),
            body: None,
        },
    )
    .await?;
    parse_advertisement(&response.body)?;

    let upload = endpoint(base, "git-upload-pack")?;
    let response = execute(
        http,
        HttpRequest {
            method: Method::POST,
            url: upload,
            headers: headers_v2(Some("application/x-git-upload-pack-request")),
            body: Some(ls_refs_request(prefixes)?.into()),
        },
    )
    .await?;
    parse_ls_refs(&response.body)
}

pub async fn remote_pack(
    http: &dyn HttpTransport,
    base: &Url,
    wants: &WantOids,
    haves: &HaveOids,
    max_pack_bytes: u64,
) -> Result<Vec<u8>, FetchError> {
    if wants.is_empty() {
        return Ok(Vec::new());
    }
    let upload = endpoint(base, "git-upload-pack")?;
    let response = execute(
        http,
        HttpRequest {
            method: Method::POST,
            url: upload,
            headers: headers_v2(Some("application/x-git-upload-pack-request")),
            body: Some(fetch_request(wants, haves)?.into()),
        },
    )
    .await?;
    parse_fetch_response(&response.body, max_pack_bytes)
}

pub fn local_refs(source: &Repo, prefixes: &[&str]) -> Result<UpstreamRefs, FetchError> {
    let refs = source
        .advertised_refs()
        .map_err(PackError::from)?
        .iter()
        .filter(|record| crate::upload::matches_prefix(record.name.as_str(), prefixes))
        .cloned()
        .collect();
    Ok(UpstreamRefs {
        head_symref: source.head().map(|head| head.name),
        refs,
    })
}

struct BoundedPack {
    buf: Vec<u8>,
    limit: u64,
    overflowed: bool,
}

impl Write for BoundedPack {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() as u64 + data.len() as u64 > self.limit {
            self.overflowed = true;
            return Err(std::io::Error::other("pack byte limit exceeded"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn local_pack(
    source: &Repo,
    wants: &WantOids,
    haves: &HaveOids,
    max_pack_bytes: u64,
) -> Result<Vec<u8>, FetchError> {
    if wants.is_empty() {
        return Ok(Vec::new());
    }
    let oids = source
        .select_pack_objects_filtered(
            wants.wants(),
            haves.haves(),
            Filter::None,
            crate::upload::selection_budget(),
        )
        .map_err(PackError::from)?
        .send;
    let mut out = BoundedPack {
        buf: Vec::new(),
        limit: max_pack_bytes,
        overflowed: false,
    };
    match crate::objects::write_pack(
        &source.objects_dir(),
        oids,
        None,
        &mut out,
        source.object_format().kind(),
    ) {
        Ok(()) => Ok(out.buf),
        Err(_) if out.overflowed => Err(FetchError::PackTooLarge {
            limit: max_pack_bytes,
        }),
        Err(error) => Err(FetchError::Pack(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(buf: &mut Vec<u8>, line: &[u8]) {
        pkt::write_data(buf, line).unwrap();
    }

    #[test]
    fn the_v2_advertisement_is_accepted_and_v0_is_refused() {
        let scan = tempfile::tempdir().unwrap();
        let repo = knot_git::Layout::new(scan.path())
            .create(&knot_types::RepoDid::new("did:plc:squid").unwrap())
            .unwrap();
        let v2 = crate::upload::advertise(&repo).unwrap();
        assert!(parse_advertisement(&v2).is_ok());

        let mut v0 = Vec::new();
        data(&mut v0, b"# service=git-upload-pack\n");
        pkt::write_flush(&mut v0).unwrap();
        data(
            &mut v0,
            b"95d09f2b10159347eece71399a7e2e907ea3df4f HEAD\0side-band-64k\n",
        );
        pkt::write_flush(&mut v0).unwrap();
        assert!(matches!(
            parse_advertisement(&v0),
            Err(FetchError::Protocol(_))
        ));
    }

    #[test]
    fn ls_refs_lines_parse_with_symref_and_skip_head() {
        let mut body = Vec::new();
        data(
            &mut body,
            b"95d09f2b10159347eece71399a7e2e907ea3df4f HEAD symref-target:refs/heads/main\n",
        );
        data(
            &mut body,
            b"95d09f2b10159347eece71399a7e2e907ea3df4f refs/heads/main\n",
        );
        pkt::write_flush(&mut body).unwrap();
        let refs = parse_ls_refs(&body).unwrap();
        assert_eq!(
            refs.head_symref.as_ref().map(RefName::as_str),
            Some("refs/heads/main")
        );
        assert_eq!(refs.refs.len(), 1);
        assert_eq!(refs.refs[0].name.as_str(), "refs/heads/main");
        assert_eq!(refs.tips().len(), 1);
    }

    #[test]
    fn a_malformed_ref_oid_is_a_protocol_error() {
        let mut body = Vec::new();
        data(&mut body, b"zzzz refs/heads/main\n");
        pkt::write_flush(&mut body).unwrap();
        assert!(matches!(parse_ls_refs(&body), Err(FetchError::Protocol(_))));
    }

    #[test]
    fn an_err_line_is_surfaced_as_remote() {
        let mut body = Vec::new();
        data(&mut body, b"ERR access denied\n");
        pkt::write_flush(&mut body).unwrap();
        assert!(matches!(
            parse_ls_refs(&body),
            Err(FetchError::Remote(message)) if message == "access denied"
        ));
    }

    #[test]
    fn the_packfile_section_demuxes_data_and_drops_progress() {
        let mut body = Vec::new();
        data(&mut body, b"packfile\n");
        data(&mut body, b"\x01PACKDATA");
        data(&mut body, b"\x02counting objects\n");
        data(&mut body, b"\x01MORE");
        pkt::write_flush(&mut body).unwrap();
        let pack = parse_fetch_response(&body, 1024).unwrap();
        assert_eq!(pack, b"PACKDATAMORE");
    }

    #[test]
    fn a_sideband_error_band_is_remote_and_the_limit_holds() {
        let mut body = Vec::new();
        data(&mut body, b"packfile\n");
        data(&mut body, b"\x03out of disk\n");
        pkt::write_flush(&mut body).unwrap();
        assert!(matches!(
            parse_fetch_response(&body, 1024),
            Err(FetchError::Remote(message)) if message == "out of disk"
        ));

        let mut big = Vec::new();
        data(&mut big, b"packfile\n");
        data(&mut big, b"\x01PACKDATA");
        pkt::write_flush(&mut big).unwrap();
        assert!(matches!(
            parse_fetch_response(&big, 4),
            Err(FetchError::PackTooLarge { limit: 4 })
        ));
    }

    #[test]
    fn a_response_without_a_packfile_section_is_refused() {
        let mut body = Vec::new();
        data(&mut body, b"acknowledgments\n");
        data(&mut body, b"NAK\n");
        pkt::write_flush(&mut body).unwrap();
        assert!(matches!(
            parse_fetch_response(&body, 1024),
            Err(FetchError::Protocol(_))
        ));
    }

    #[test]
    fn the_fetch_request_includes_wants_haves_and_done() {
        let want = Oid::from_hex("95d09f2b10159347eece71399a7e2e907ea3df4f").unwrap();
        let have = Oid::from_hex("2222222222222222222222222222222222222222").unwrap();
        let body = fetch_request(&WantOids::new(vec![want]), &HaveOids::new(vec![have])).unwrap();
        let lines = pkt::data_payloads_all(&body).unwrap();
        let text: Vec<&str> = lines
            .iter()
            .map(|line| std::str::from_utf8(line).unwrap().trim_end())
            .collect();
        assert!(text.contains(&"command=fetch"));
        assert!(text.contains(&"want 95d09f2b10159347eece71399a7e2e907ea3df4f"));
        assert!(text.contains(&"have 2222222222222222222222222222222222222222"));
        assert!(text.contains(&"done"));
        assert!(text.contains(&"no-progress"));
    }
}
