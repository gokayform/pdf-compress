//! Byte-level checks for the classic cross-reference format emitted by the compressor.
//! Deliberately independent of lopdf's parser and its repair behavior.
use std::collections::BTreeMap;

pub fn classic_xref(bytes: &[u8]) -> Result<BTreeMap<(u32, u16), usize>, String> {
    if !bytes.starts_with(b"%PDF-") || !bytes.ends_with(b"%%EOF") {
        return Err("missing PDF header or final EOF".into());
    }
    let marker = b"startxref";
    let pos = bytes
        .windows(marker.len())
        .rposition(|w| w == marker)
        .ok_or("missing startxref")?;
    let tail =
        std::str::from_utf8(&bytes[pos + marker.len()..]).map_err(|_| "invalid startxref")?;
    let offset: usize = tail
        .split_whitespace()
        .next()
        .ok_or("missing xref offset")?
        .parse()
        .map_err(|_| "bad xref offset")?;
    let body = bytes.get(offset..).ok_or("xref offset out of range")?;
    if !body.starts_with(b"xref\n") && !body.starts_with(b"xref\r") {
        return Err("startxref does not address a classic xref table".into());
    }
    let end = body
        .windows(7)
        .position(|w| w == b"trailer")
        .ok_or("missing trailer")?
        + 7;
    let text = std::str::from_utf8(&body[..end]).map_err(|_| "non-ASCII classic xref")?;
    let mut lines = text.lines().skip(1);
    let mut entries = BTreeMap::new();
    let mut saw_free_zero = false;
    loop {
        let line = lines.next().ok_or("truncated xref")?;
        if line.trim() == "trailer" {
            break;
        }
        let mut parts = line.split_whitespace();
        let start: u32 = parts
            .next()
            .ok_or("missing subsection start")?
            .parse()
            .map_err(|_| "bad subsection start")?;
        let count: u32 = parts
            .next()
            .ok_or("missing subsection count")?
            .parse()
            .map_err(|_| "bad subsection count")?;
        if parts.next().is_some() || count as usize > bytes.len() / 18 {
            return Err("bad xref subsection".into());
        }
        for n in 0..count {
            let entry = lines.next().ok_or("truncated xref entries")?;
            let parts = entry.split_whitespace().collect::<Vec<_>>();
            if parts.len() != 3 || parts[0].len() != 10 || parts[1].len() != 5 {
                return Err("invalid fixed-width xref entry".into());
            }
            let off: usize = parts[0].parse().map_err(|_| "bad object offset")?;
            let generation: u16 = parts[1].parse().map_err(|_| "bad generation")?;
            let id = start.checked_add(n).ok_or("object id overflow")?;
            match parts[2] {
                "n" => {
                    let expected = format!("{id} {generation} obj");
                    if !bytes
                        .get(off..)
                        .is_some_and(|b| b.starts_with(expected.as_bytes()))
                    {
                        return Err(format!(
                            "xref offset for {id} {generation} does not address its object"
                        ));
                    }
                    if entries.insert((id, generation), off).is_some() {
                        return Err("duplicate live xref entry".into());
                    }
                }
                "f" => {
                    if id == 0 && generation == 65535 {
                        saw_free_zero = true;
                    }
                }
                _ => return Err("invalid xref entry type".into()),
            }
        }
    }
    if !saw_free_zero {
        return Err("object zero must be free generation 65535".into());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bad_offsets_are_not_repaired() {
        assert!(classic_xref(
            b"%PDF-1.4\nxref\n0 1\n0000000000 65535 f \ntrailer\n<<>>\nstartxref\n12\n%%EOF"
        )
        .is_err());
    }
}
