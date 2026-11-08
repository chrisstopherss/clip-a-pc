//! Annex B (start code) to AVCC (length prefixed) conversion for MP4.

/// Split an Annex B stream into NAL units (without start codes).
pub fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut e = if n + 1 < starts.len() { starts[n + 1] - 3 } else { data.len() };
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

#[derive(Default, Debug)]
pub struct Converted {
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub key: bool,
}

/// Append the frame in `data` to `out` as length-prefixed NAL units.
/// SPS, PPS and access unit delimiters are not copied; SPS and PPS are returned
/// so they can go into the MP4 header instead.
pub fn annexb_to_avcc(data: &[u8], out: &mut Vec<u8>) -> Converted {
    let mut c = Converted::default();
    for nal in split_nals(data) {
        match nal[0] & 0x1f {
            7 => c.sps = Some(nal.to_vec()),
            8 => c.pps = Some(nal.to_vec()),
            9 => {}
            t => {
                if t == 5 {
                    c.key = true;
                }
                out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let n = split_nals(&d);
        assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
    }

    #[test]
    fn converts_and_strips_parameter_sets() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let mut out = Vec::new();
        let c = annexb_to_avcc(&d, &mut out);
        assert!(c.key);
        assert_eq!(c.sps.as_deref(), Some(&[0x67, 1, 2][..]));
        assert_eq!(c.pps.as_deref(), Some(&[0x68, 3][..]));
        assert_eq!(out, vec![0, 0, 0, 3, 0x65, 9, 9]);
    }
}
