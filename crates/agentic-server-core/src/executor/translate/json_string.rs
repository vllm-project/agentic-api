//! Incremental decoding of a JSON string value whose closing quote may not have streamed yet.

/// Decodes the complete prefix of the JSON string body `encoded` (the text after its opening quote).
///
/// Returns the decoded text and the bytes of `encoded` it consumed, or `None` when no complete
/// character has arrived. A split escape or surrogate pair waits for the rest of it.
pub(super) fn decode_string_prefix(encoded: &str) -> Result<Option<(String, usize)>, serde_json::Error> {
    let end = complete_json_string_prefix(encoded);
    if end == 0 {
        return Ok(None);
    }
    let text = serde_json::from_str::<String>(&format!("\"{}\"", &encoded[..end]))?;
    Ok(Some((text, end)))
}

fn complete_json_string_prefix(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return index,
            b'\\' => {
                let Some(escape) = bytes.get(index + 1) else {
                    return index;
                };
                if *escape == b'u' {
                    let unicode_end = index.saturating_add(6);
                    if unicode_end > bytes.len() {
                        return index;
                    }
                    let Some(code_unit) = json_hex_quad(&bytes[index + 2..unicode_end]) else {
                        index = unicode_end;
                        continue;
                    };
                    if (0xD800..=0xDBFF).contains(&code_unit) {
                        let pair_end = index.saturating_add(12);
                        if pair_end > bytes.len() {
                            return index;
                        }
                        index = pair_end;
                    } else {
                        index = unicode_end;
                    }
                } else {
                    index = index.saturating_add(2);
                }
            }
            _ => index = index.saturating_add(1),
        }
    }
    index
}

fn json_hex_quad(bytes: &[u8]) -> Option<u16> {
    if bytes.len() != 4 {
        return None;
    }
    bytes.iter().try_fold(0_u16, |value, byte| {
        let digit = byte.to_ascii_lowercase();
        let digit = match digit {
            b'0'..=b'9' => u16::from(digit - b'0'),
            b'a'..=b'f' => u16::from(digit - b'a' + 10),
            _ => return None,
        };
        value.checked_mul(16)?.checked_add(digit)
    })
}
