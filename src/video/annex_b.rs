//! Annex-B access units: start-code framing, NAL types, and the parameter
//! sets a decoder needs before an IDR. The KMS pipeline only asks whether a
//! unit carries parameter sets; the rest serves the RAM pipeline.
#![cfg_attr(target_os = "linux", allow(dead_code))]

use crate::protocol::VideoCodec;

/// True when a packet starts with a start code. FFmpeg's encoders write
/// Annex B when the codec context has no global header.
pub fn is_annex_b(packet: &[u8]) -> bool {
    packet.starts_with(&[0, 0, 1]) || packet.starts_with(&[0, 0, 0, 1])
}

fn nal_type(codec: VideoCodec, first_byte: u8) -> u8 {
    match codec {
        VideoCodec::H264 => first_byte & 0x1f,
        VideoCodec::H265 => (first_byte >> 1) & 0x3f,
    }
}

pub fn unit_has_idr(codec: VideoCodec, data: &[u8]) -> bool {
    annex_b_nals(data).any(|nal| match codec {
        VideoCodec::H264 => nal_type(codec, nal[0]) == 5,
        // IDR_W_RADL, IDR_N_LP, and CRA
        VideoCodec::H265 => matches!(nal_type(codec, nal[0]), 19..=21),
    })
}

fn is_parameter_set(codec: VideoCodec, first_byte: u8) -> bool {
    match codec {
        // SPS, PPS
        VideoCodec::H264 => matches!(nal_type(codec, first_byte), 7 | 8),
        // VPS, SPS, PPS
        VideoCodec::H265 => matches!(nal_type(codec, first_byte), 32..=34),
    }
}

pub fn unit_has_parameter_sets(codec: VideoCodec, data: &[u8]) -> bool {
    let has = |t| annex_b_nals(data).any(|nal| nal_type(codec, nal[0]) == t);
    match codec {
        VideoCodec::H264 => has(7) && has(8),
        VideoCodec::H265 => has(32) && has(33) && has(34),
    }
}

pub fn extract_parameter_sets(codec: VideoCodec, data: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    for nal in annex_b_nals(data) {
        if is_parameter_set(codec, nal[0]) {
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(nal);
        }
    }
    output
}

fn annex_b_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut current = find_start_code(data, 0);
    std::iter::from_fn(move || loop {
        let (start, start_len) = current?;
        let nal_start = start + start_len;
        current = find_start_code(data, nal_start);
        let nal_end = current.map_or(data.len(), |(offset, _)| offset);
        if nal_start < nal_end {
            return Some(&data[nal_start..nal_end]);
        }
    })
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut offset = from;
    while offset + 3 <= data.len() {
        if offset + 4 <= data.len() && data[offset..offset + 4] == [0, 0, 0, 1] {
            return Some((offset, 4));
        }
        if data[offset..offset + 3] == [0, 0, 1] {
            return Some((offset, 3));
        }
        offset += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_annex_b_from_length_prefixes() {
        assert!(is_annex_b(&[0, 0, 0, 1, 0x65, 1]));
        assert!(is_annex_b(&[0, 0, 1, 0x65, 1]));
        assert!(!is_annex_b(&[0, 0, 0, 3, 0x67, 1, 2]));
    }

    #[test]
    fn finds_nal_types() {
        let unit = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3];
        assert!(unit_has_parameter_sets(VideoCodec::H264, &unit));
        assert!(unit_has_idr(VideoCodec::H264, &unit));
        // HEVC IDR_W_RADL + VPS/SPS/PPS
        let hevc = [
            0, 0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 2, 0, 0, 1, 0x44, 3, 0, 0, 0, 1, 0x26, 4,
        ];
        assert!(unit_has_parameter_sets(VideoCodec::H265, &hevc));
        assert!(unit_has_idr(VideoCodec::H265, &hevc));
        assert!(!unit_has_idr(VideoCodec::H264, &hevc));
        let hevc_cra = [0, 0, 0, 1, 0x2a, 1, 2];
        assert!(unit_has_idr(VideoCodec::H265, &hevc_cra));
    }

    #[test]
    fn extracts_sps_and_pps() {
        let unit = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 4, 0, 0, 1, 0x65, 5,
        ];
        assert_eq!(
            extract_parameter_sets(VideoCodec::H264, &unit),
            [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 4]
        );
    }
}
