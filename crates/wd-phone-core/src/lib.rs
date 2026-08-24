//! Wireless Display phone-side core: QUIC protocol client + RTP media
//! packetizer, exported to Android via JNI (`com.example.wd_flutter.WdNative`).

mod jni;
pub mod proto;
pub mod rtp;
pub mod session;
pub mod store;

#[cfg(test)]
mod tests {
    use crate::rtp::{avcc_to_annexb, contains_idr, lp_to_annexb, packetize_au};

    #[test]
    fn avcc_parses_sps_pps() {
        // version=1, profile, compat, level, 0xff (lenSize-1=3), numSPS=1,
        // len=4 [67 64 00 1f], numPPS=1, len=4 [68 eb ec b2]
        let csd = [
            0x01, 0x64, 0x00, 0x1f, 0xff, 0xe1, 0x00, 0x04, 0x67, 0x64, 0x00, 0x1f, 0x01,
            0x00, 0x04, 0x68, 0xeb, 0xec, 0xb2,
        ];
        let out = avcc_to_annexb(&csd).expect("parses");
        assert!(out.starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(out.ends_with(&[0x68, 0xeb, 0xec, 0xb2]));
    }

    #[test]
    fn lp_converts_to_annexb() {
        let lp = [0, 0, 0, 2, 0x41, 0x9a, 0, 0, 0, 1, 0x42];
        let out = lp_to_annexb(&lp).expect("converts");
        assert_eq!(out, vec![0, 0, 0, 1, 0x41, 0x9a, 0, 0, 0, 1, 0x42]);
    }

    #[test]
    fn idr_detection() {
        let au = [0, 0, 0, 1, 0x65, 1, 2, 3];
        assert!(contains_idr(&au));
        let au = [0, 0, 0, 1, 0x41, 1, 2, 3];
        assert!(!contains_idr(&au));
    }

    #[test]
    fn packetizes_single_and_fragmented() {
        let mut seq = 100u16;
        // Single small NAL AU.
        let small = [0, 0, 0, 1, 0x61, 1, 2, 3];
        let pkts = packetize_au(&small, 3000, 7, 1100, &mut seq);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0][1] & 0x80, 0x80, "marker on last packet of AU");
        assert_eq!(pkts[0][12], 0x61);

        // Fragment a big IDR: FU-A chunks, S/E bits, marker only at end.
        let mut big = vec![0, 0, 0, 1, 0x65];
        big.extend(std::iter::repeat_n(0xa5u8, 5000));
        let pkts = packetize_au(&big, 6000, 7, 1100, &mut seq);
        assert!(pkts.len() >= 5);
        assert_eq!(pkts[0][13] & 0x80, 0x80, "start bit");
        assert_eq!(
            pkts[pkts.len() - 1][13] & 0x40,
            0x40,
            "end bit on final chunk"
        );
        assert_eq!(pkts[pkts.len() - 1][1] & 0x80, 0x80, "marker");
        for p in &pkts {
            assert!(p.len() <= 1100);
        }
    }
}
