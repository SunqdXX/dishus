//! H.264 Annex-B helpers: the stream must look like the Linux one (h264parse
//! `config-interval=-1`): SPS and PPS in front of every IDR, keyframe = contains an IDR slice.
//! Media Foundation encoders differ here (some repeat SPS/PPS, some only put them in the media
//! type or the first frame), so we remember them and put them back where they are missing.

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// `(start of the start code, start of the NAL header, end)` of every NAL unit.
pub fn units(buf: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let sc = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
            starts.push((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(sc, hdr)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(buf.len(), |n| n.0);
        if hdr < end {
            out.push((sc, hdr, end));
        }
    }
    out
}

fn nal_type(buf: &[u8], hdr: usize) -> u8 {
    buf[hdr] & 0x1F
}

/// The last SPS and PPS seen (with start codes).
#[derive(Default, Debug)]
pub struct ParamSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// Remember SPS/PPS from an access unit or a sequence header blob.
    pub fn observe(&mut self, buf: &[u8]) {
        for (_, h, end) in units(buf) {
            let unit = || {
                let mut v = vec![0, 0, 0, 1];
                v.extend_from_slice(&buf[h..end]);
                v
            };
            match nal_type(buf, h) {
                NAL_SPS => self.sps = Some(unit()),
                NAL_PPS => self.pps = Some(unit()),
                _ => {}
            }
        }
    }

    /// Returns `(access unit, keyframe)`. A keyframe without its own SPS/PPS gets the
    /// remembered ones inserted in front of it (after an access unit delimiter, if any).
    pub fn fix(&mut self, au: Vec<u8>) -> (Vec<u8>, bool) {
        self.observe(&au);
        let units = units(&au);
        let key = units.iter().any(|&(_, h, _)| nal_type(&au, h) == NAL_IDR);
        if !key || units.iter().any(|&(_, h, _)| nal_type(&au, h) == NAL_SPS) {
            return (au, key);
        }
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
            return (au, key);
        };
        let at = match units.first() {
            Some(&(_, h, end)) if nal_type(&au, h) == NAL_AUD => end,
            _ => 0,
        };
        let mut out = Vec::with_capacity(au.len() + sps.len() + pps.len());
        out.extend_from_slice(&au[..at]);
        out.extend_from_slice(sps);
        out.extend_from_slice(pps);
        out.extend_from_slice(&au[at..]);
        (out, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84];
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A, 0x02];
    const AUD: &[u8] = &[0, 0, 0, 1, 0x09, 0xF0];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn finds_units_with_3_and_4_byte_start_codes() {
        let au = cat(&[SPS, PPS, IDR]);
        let types: Vec<u8> = units(&au).iter().map(|&(_, h, _)| au[h] & 0x1F).collect();
        assert_eq!(types, vec![7, 8, 5]);
        assert!(units(&[0, 0]).is_empty());
    }

    #[test]
    fn complete_keyframes_pass_through() {
        let mut ps = ParamSets::default();
        let au = cat(&[SPS, PPS, IDR]);
        assert_eq!(ps.fix(au.clone()), (au, true));
        assert!(ps.sps.is_some() && ps.pps.is_some());
        assert_eq!(ps.fix(P.to_vec()), (P.to_vec(), false));
    }

    #[test]
    fn bare_idr_gets_parameter_sets() {
        let mut ps = ParamSets::default();
        ps.observe(&cat(&[SPS, PPS]));
        let (au, key) = ps.fix(IDR.to_vec());
        assert!(key);
        assert_eq!(au, cat(&[SPS, PPS, IDR]));
        // after an access unit delimiter, not before it
        let (au, _) = ps.fix(cat(&[AUD, IDR]));
        assert_eq!(au, cat(&[AUD, SPS, PPS, IDR]));
    }

    #[test]
    fn unknown_parameter_sets_leave_frame_alone() {
        let mut ps = ParamSets::default();
        assert_eq!(ps.fix(IDR.to_vec()), (IDR.to_vec(), true));
    }
}
