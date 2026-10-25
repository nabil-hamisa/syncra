//! Split an H.264 Annex-B elementary stream into access units (one per picture).
//! Only needed for file playback; over the network the host marks frame
//! boundaries explicitly.

/// Byte ranges of each NAL unit (including its start code).
fn nal_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // Include a leading zero byte of a 4-byte start code.
            let s = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((s, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &(s, _))| (s, starts.get(k + 1).map_or(data.len(), |n| n.0)))
        .collect()
}

fn payload_start(data: &[u8], start: usize) -> usize {
    if data[start..].starts_with(&[0, 0, 0, 1]) {
        start + 4
    } else {
        start + 3
    }
}

/// Group NAL units into access units.
pub fn split_access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut aus = Vec::new();
    let mut au_start = None::<usize>;
    let mut au_has_vcl = false;

    for (s, e) in nal_ranges(data) {
        let p = payload_start(data, s);
        if p >= e {
            continue;
        }
        let nal_type = data[p] & 0x1f;
        let is_vcl = nal_type == 1 || nal_type == 5;
        // first_mb_in_slice is the first ue(v) of the slice header: it is 0
        // exactly when the first bit after the NAL header is 1.
        let first_slice = is_vcl && p + 1 < e && data[p + 1] & 0x80 != 0;
        let starts_new_au = au_has_vcl && (first_slice || matches!(nal_type, 6..=9));

        if starts_new_au {
            aus.push(&data[au_start.unwrap()..s]);
            au_start = Some(s);
            au_has_vcl = false;
        }
        au_start.get_or_insert(s);
        au_has_vcl |= is_vcl;
    }
    if let Some(s) = au_start {
        aus.push(&data[s..]);
    }
    aus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_slices_and_parameter_sets() {
        // SPS, PPS, IDR slice (first), IDR slice (second), P slice (first), P slice (second)
        let stream = [
            &[0, 0, 0, 1, 0x67, 0xAA][..],
            &[0, 0, 0, 1, 0x68, 0xBB],
            &[0, 0, 0, 1, 0x65, 0x88, 0x01],
            &[0, 0, 1, 0x65, 0x40, 0x02],
            &[0, 0, 0, 1, 0x41, 0x9A, 0x03],
            &[0, 0, 1, 0x41, 0x20, 0x04],
        ]
        .concat();
        let aus = split_access_units(&stream);
        assert_eq!(aus.len(), 2);
        assert_eq!(aus[0].len(), 6 + 6 + 7 + 6);
        assert_eq!(aus[1].len(), 7 + 6);
    }
}
