//! H.264 → RTP (RFC 6184) packetization plus MediaCodec output conversions.

const START4: [u8; 4] = [0, 0, 0, 1];

/// Parse an `avcC` record (MediaCodec csd-0) and return SPS/PPS as Annex-B.
pub fn avcc_to_annexb(csd: &[u8]) -> Option<Vec<u8>> {
    if csd.len() < 8 || csd[0] != 1 {
        return None;
    }
    let num_sps = csd[5] & 0x1f;
    let mut out = Vec::with_capacity(csd.len() + 16);
    let mut i = 6usize;
    for _ in 0..num_sps {
        if i + 2 > csd.len() {
            return None;
        }
        let len = u16::from_be_bytes([csd[i], csd[i + 1]]) as usize;
        i += 2;
        if i + len > csd.len() {
            return None;
        }
        out.extend_from_slice(&START4);
        out.extend_from_slice(&csd[i..i + len]);
        i += len;
    }
    if i >= csd.len() {
        return if out.is_empty() { None } else { Some(out) };
    }
    let num_pps = csd[i];
    i += 1;
    for _ in 0..num_pps {
        if i + 2 > csd.len() {
            break;
        }
        let len = u16::from_be_bytes([csd[i], csd[i + 1]]) as usize;
        i += 2;
        if i + len > csd.len() {
            break;
        }
        out.extend_from_slice(&START4);
        out.extend_from_slice(&csd[i..i + len]);
        i += len;
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Convert one MediaCodec length-prefixed access unit to Annex-B.
/// Returns `None` when the buffer is already Annex-B (no valid 4-byte
/// length framing could be walked).
pub fn lp_to_annexb(buf: &[u8]) -> Option<Vec<u8>> {
    if buf.len() < 5 {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len() + 16);
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        let len = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > buf.len() {
            return None; // not length-prefixed after all
        }
        out.extend_from_slice(&START4);
        out.extend_from_slice(&buf[i..i + len]);
        i += len;
    }
    Some(out)
}

/// Find Annex-B start codes: returns (code_start, payload_start) pairs.
/// Leading zeros of a code are merged into it per the Annex-B spec.
fn start_codes(au: &[u8]) -> Vec<(usize, usize)> {
    let n = au.len();
    let mut codes = Vec::new();
    let mut i = 0usize;
    while i + 2 < n {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let mut cs = i;
            while cs > 0 && au[cs - 1] == 0 {
                cs -= 1;
            }
            codes.push((cs, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    codes
}

fn nals(au: &[u8]) -> Vec<&[u8]> {
    let codes = start_codes(au);
    let mut out = Vec::with_capacity(codes.len());
    for (idx, (_, ps)) in codes.iter().enumerate() {
        let end = match codes.get(idx + 1) {
            Some((next_cs, _)) => *next_cs,
            None => au.len(),
        };
        if end > *ps {
            out.push(&au[*ps..end]);
        }
    }
    out
}

pub fn contains_idr(au: &[u8]) -> bool {
    nals(au).iter().any(|n| !n.is_empty() && n[0] & 0x1f == 5)
}

/// Packetize a full access unit (Annex-B) into RTP packets (PT 96,
/// clock 90 kHz), each at most `mtu` bytes on the wire.
pub fn packetize_au(au: &[u8], ts90k: u32, ssrc: u32, mtu: usize, seq: &mut u16) -> Vec<Vec<u8>> {
    let nal_list = nals(au);
    let total = nal_list.len();
    let mut out: Vec<Vec<u8>> = Vec::new();
    if total == 0 {
        return out;
    }

    #[derive(Clone, Copy)]
    struct Ctx {
        ts: u32,
        ssrc: u32,
    }
    let ctx = Ctx { ts: ts90k, ssrc };

    fn header(pkt: &mut Vec<u8>, marker: bool, c: Ctx, seq: &mut u16) {
        pkt.push(0x80);
        pkt.push(if marker { 0xe0 } else { 0x60 }); // version2|marker|PT96
        pkt.extend_from_slice(&seq.to_be_bytes());
        *seq = seq.wrapping_add(1);
        pkt.extend_from_slice(&c.ts.to_be_bytes());
        pkt.extend_from_slice(&c.ssrc.to_be_bytes());
    }

    for (idx, nal) in nal_list.iter().enumerate() {
        if nal.is_empty() {
            continue;
        }
        let first = nal[0];
        let last_of_au = idx == total - 1;
        if nal.len() <= mtu {
            let mut pkt = Vec::with_capacity(12 + nal.len());
            header(&mut pkt, last_of_au, ctx, seq);
            pkt.extend_from_slice(nal);
            out.push(pkt);
        } else {
            let payload = &nal[1..];
            let chunk = mtu.saturating_sub(14).max(1);
            let groups: Vec<&[u8]> = payload.chunks(chunk).collect();
            for (gi, part) in groups.iter().enumerate() {
                let end_of_nal = gi == groups.len() - 1;
                let mut pkt = Vec::with_capacity(14 + part.len());
                header(&mut pkt, end_of_nal && last_of_au, ctx, seq);
                pkt.push((first & 0xe0) | 28); // FU-A indicator
                let mut fu = first & 0x1f;
                if gi == 0 {
                    fu |= 0x80; // S
                }
                if end_of_nal {
                    fu |= 0x40; // E
                }
                pkt.push(fu);
                pkt.extend_from_slice(part);
                out.push(pkt);
            }
        }
    }
    out
}
