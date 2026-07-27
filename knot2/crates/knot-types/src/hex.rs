const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

pub fn lowercase_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| {
            [
                HEX_DIGITS[usize::from(byte >> 4)],
                HEX_DIGITS[usize::from(byte & 0x0f)],
            ]
        })
        .map(char::from)
        .collect()
}

pub fn decode_hex(text: impl AsRef<[u8]>) -> Option<Vec<u8>> {
    let text = text.as_ref();
    match text.len() % 2 {
        0 => text
            .chunks_exact(2)
            .map(|pair| {
                let hi = (pair[0] as char).to_digit(16)?;
                let lo = (pair[1] as char).to_digit(16)?;
                Some((hi * 16 + lo) as u8)
            })
            .collect(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_hex_inverts_an_encoder_that_covers_every_nibble() {
        [
            (&[][..], ""),
            (&[0x00][..], "00"),
            (&[0xff][..], "ff"),
            (
                &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef][..],
                "0123456789abcdef",
            ),
        ]
        .iter()
        .for_each(|&(bytes, expected)| {
            assert_eq!(lowercase_hex(bytes), expected);
            assert_eq!(decode_hex(expected).unwrap(), bytes);
        });
        assert_eq!(
            decode_hex("abc"),
            None,
            "an odd digit count can't form whole bytes"
        );
        assert_eq!(decode_hex("zz"), None, "a non-hex digit decodes to nothing");
        assert_eq!(
            decode_hex("AB").unwrap(),
            [0xab],
            "uppercase input still decodes even though the encoder emits lowercase"
        );
    }
}
