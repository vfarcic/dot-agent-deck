use serde_json::{Value, json};

/// Replace evidence bytes and rebase indexes; do not add a test-only failure knob.
pub fn replace_bytes(
    capture: &mut Vec<u8>,
    index: &mut Value,
    start: usize,
    end: usize,
    bytes: &[u8],
) {
    assert!(start <= end && end <= capture.len());
    let shift = bytes.len() as isize - (end - start) as isize;
    let rebase = |offset: usize| {
        if offset >= end {
            offset.checked_add_signed(shift).unwrap()
        } else if offset > start {
            start + bytes.len()
        } else {
            offset
        }
    };
    let mut read_ends: Vec<_> = index["read_lens"]
        .as_array()
        .unwrap()
        .iter()
        .scan(0, |offset, len| {
            *offset += len.as_u64().unwrap() as usize;
            Some(rebase(*offset))
        })
        .collect();
    for edge in index["edges"].as_array_mut().unwrap() {
        let offset = edge["off"].as_u64().unwrap() as usize;
        assert!(
            offset <= start || offset >= end,
            "replacement must not erase a geometry boundary"
        );
        edge["off"] = json!(rebase(offset));
        read_ends.push(rebase(offset));
    }
    for point in index["points"].as_object_mut().unwrap().values_mut() {
        *point = json!(rebase(point.as_u64().unwrap() as usize));
    }
    capture.splice(start..end, bytes.iter().copied());
    read_ends.push(capture.len());
    read_ends.sort_unstable();
    read_ends.dedup();
    // Keep reads split at geometry edges, including when the original index
    // placed one inside a read. Equal-offset edges retain their array order.
    index["read_lens"] = json!(
        read_ends
            .into_iter()
            .filter(|end| *end > 0)
            .scan(0, |offset, end| {
                let len = end - *offset;
                *offset = end;
                Some(len)
            })
            .collect::<Vec<_>>()
    );
}
