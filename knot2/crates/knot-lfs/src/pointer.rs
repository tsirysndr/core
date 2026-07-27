use crate::{ClaimedSize, LfsOid};

pub const POINTER_MAX_BYTES: u64 = 1024;

const SPEC_URLS: [&str; 2] = [
    "https://git-lfs.github.com/spec/v1",
    "https://hawser.github.com/spec/v1",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    pub oid: LfsOid,
    pub size: ClaimedSize,
}

pub fn parse_pointer(blob: &[u8]) -> Option<LfsPointer> {
    if blob.len() as u64 > POINTER_MAX_BYTES {
        return None;
    }
    let text = std::str::from_utf8(blob).ok()?;
    let mut lines = text.lines();
    let spec = lines.next()?.strip_prefix("version ")?;
    SPEC_URLS.contains(&spec).then_some(())?;
    let fields: Vec<(&str, &str)> = lines.filter_map(|line| line.split_once(' ')).collect();
    let value_of = |key: &str| {
        fields
            .iter()
            .find_map(|(name, value)| (*name == key).then_some(*value))
    };
    let oid = LfsOid::new(value_of("oid")?.strip_prefix("sha256:")?).ok()?;
    let size = ClaimedSize::new(value_of("size")?.parse().ok()?);
    Some(LfsPointer { oid, size })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "6c17f2007cbe934aee6e309b28b2fba3c119d98be6ea4156da3aa3173456ad16";

    fn pointer_text() -> String {
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 12345\n")
    }

    #[test]
    fn valid_variants_beyond_the_canonical_form_still_parse() {
        let hawser = pointer_text().replace("git-lfs.github.com", "hawser.github.com");
        assert!(
            parse_pointer(hawser.as_bytes()).is_some(),
            "the legacy hawser spec still counts"
        );
        let extra = format!(
            "version https://git-lfs.github.com/spec/v1\nname media/clip.mp4\noid sha256:{OID}\nsize 7\n"
        );
        assert!(
            parse_pointer(extra.as_bytes()).is_some(),
            "extra sorted keys are tolerated"
        );
    }

    #[test]
    fn non_pointers_are_rejected() {
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"plain source file\n".to_vec(),
            format!("oid sha256:{OID}\nsize 7\n").into_bytes(),
            format!("version https://oyster.cafe/spec/v1\noid sha256:{OID}\nsize 7\n").into_bytes(),
            pointer_text().replace("sha256:", "sha512:").into_bytes(),
            pointer_text()
                .replace("size 12345", "size lots")
                .into_bytes(),
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\n").into_bytes(),
            b"version https://git-lfs.github.com/spec/v1\nsize 7\n".to_vec(),
            [pointer_text().into_bytes(), vec![0xff, 0xfe]].concat(),
            [
                pointer_text().into_bytes(),
                vec![b' '; POINTER_MAX_BYTES as usize],
            ]
            .concat(),
        ];
        cases.iter().enumerate().for_each(|(index, blob)| {
            assert!(
                parse_pointer(blob).is_none(),
                "case {index} parsed as a pointer"
            );
        });
    }
}
