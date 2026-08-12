use std::sync::LazyLock;

const MARKERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

const BYTES_PER_LINE: usize = MARKERS.len();

const ALPHABET: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

static DIGITS: LazyLock<[Option<u8>; 256]> = LazyLock::new(|| {
    std::array::from_fn(|byte| {
        ALPHABET
            .iter()
            .position(|&candidate| candidate as usize == byte)
            .map(|digit| digit as u8)
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Malformed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineLength(u8);

impl LineLength {
    fn of(bytes: usize) -> Option<Self> {
        bytes
            .checked_sub(1)
            .filter(|index| *index < MARKERS.len())
            .map(|index| Self(index as u8))
    }

    fn from_marker(marker: u8) -> Option<Self> {
        MARKERS
            .iter()
            .position(|&candidate| candidate == marker)
            .map(|index| Self(index as u8))
    }

    fn marker(self) -> char {
        MARKERS[self.0 as usize] as char
    }

    fn get(self) -> usize {
        self.0 as usize + 1
    }
}

pub(crate) fn encode(packed: &[u8], out: &mut String) {
    out.reserve(encoded_len(packed.len() as u64) as usize);
    packed.chunks(BYTES_PER_LINE).for_each(|chunk| {
        let length = LineLength::of(chunk.len())
            .expect("chunking by the line width yields 1..=BYTES_PER_LINE bytes");
        out.push(length.marker());
        chunk.chunks(4).for_each(|group| {
            let word = group
                .iter()
                .fold(0u32, |acc, &byte| (acc << 8) | byte as u32)
                << (8 * (4 - group.len()));
            (0..5).rev().for_each(|power| {
                let digit = (word / 85u32.pow(power)) % 85;
                out.push(ALPHABET[digit as usize] as char);
            });
        });
        out.push('\n');
    });
}

pub(crate) fn encoded_len(packed: u64) -> u64 {
    packed.div_ceil(4) * 5 + packed.div_ceil(BYTES_PER_LINE as u64) * 2
}

pub(crate) fn decode_line(line: &str, out: &mut Vec<u8>) -> Result<(), Malformed> {
    let (marker, data) = line.as_bytes().split_first().ok_or(Malformed)?;
    let length = LineLength::from_marker(*marker).ok_or(Malformed)?;
    if data.len() != length.get().div_ceil(4) * 5 {
        return Err(Malformed);
    }
    data.chunks(5).enumerate().try_for_each(|(group, chunk)| {
        let word = chunk
            .iter()
            .try_fold(0u64, |acc, &character| {
                DIGITS[character as usize].map(|digit| acc * 85 + digit as u64)
            })
            .filter(|&word| word <= u32::MAX as u64)
            .ok_or(Malformed)?;
        let take = (length.get() - group * 4).min(4);
        out.extend_from_slice(&(word as u32).to_be_bytes()[..take]);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips_at_every_payload_length() {
        (0..=260usize).for_each(|len| {
            let bytes: Vec<u8> = (0..len)
                .map(|index| (index as u8).wrapping_mul(37) ^ 0x5a)
                .collect();
            let mut encoded = String::new();
            encode(&bytes, &mut encoded);
            assert_eq!(
                encoded_len(len as u64),
                encoded.len() as u64,
                "encoded length of a {len} byte payload"
            );
            let decoded = encoded.lines().fold(Vec::new(), |mut out, line| {
                decode_line(line, &mut out).unwrap();
                out
            });
            assert_eq!(decoded, bytes, "round trip of a {len} byte payload");
        });
    }

    #[test]
    fn line_markers_map_both_ways() {
        (1..=BYTES_PER_LINE).for_each(|len| {
            let length = LineLength::of(len).unwrap();
            assert_eq!(
                LineLength::from_marker(length.marker() as u8),
                Some(length),
                "marker for a line of {len} bytes"
            );
        });
        assert_eq!(LineLength::of(0), None);
        assert_eq!(LineLength::of(BYTES_PER_LINE + 1), None);
        assert_eq!(LineLength::of(1).unwrap().marker(), 'A');
        assert_eq!(LineLength::of(26).unwrap().marker(), 'Z');
        assert_eq!(LineLength::of(27).unwrap().marker(), 'a');
        assert_eq!(LineLength::of(BYTES_PER_LINE).unwrap().marker(), 'z');
        let mut encoded = String::new();
        encode(&[0u8; BYTES_PER_LINE + 1], &mut encoded);
        assert_eq!(
            encoded.lines().map(|line| &line[..1]).collect::<Vec<_>>(),
            vec!["z", "A"],
            "a payload past the line width gets a second marker"
        );
    }

    #[test]
    fn the_marker_sets_the_line_length_and_anything_else_is_malformed() {
        let mut out = Vec::new();
        decode_line("D00000", &mut out).unwrap();
        decode_line("B00000", &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_line("D0000", &mut Vec::new()), Err(Malformed));
        assert_eq!(decode_line("D0\"000", &mut Vec::new()), Err(Malformed));
        assert_eq!(decode_line("?00000", &mut Vec::new()), Err(Malformed));
        assert_eq!(decode_line("", &mut Vec::new()), Err(Malformed));
    }
}
