use serde::{Deserialize, Serialize};

use crate::Version;

pub const MAX_MESSAGE_LEN: usize = 4096;
const MAX_SPAKE_MESSAGE_LEN: usize = 128;
const MAX_NAME_LEN: usize = 64;
const MAX_TOKEN_LEN: usize = 64;
const MAX_REASON_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VideoCodec {
    H264Baseline,
    H264Main,
}

impl VideoCodec {
    pub fn name(self) -> &'static str {
        match self {
            Self::H264Baseline => "H.264 baseline",
            Self::H264Main => "H.264 main",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum AudioCodec {
    Opus,
    AacLc,
}

impl AudioCodec {
    pub fn name(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::AacLc => "AAC-LC",
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '\'' | '(' | ')'))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoOffer {
    pub codec: VideoCodec,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioOffer {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingOutcomeInfo {
    pub device_id: String,
    pub device_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        proto_version: Version,
        device_name: String,
        auth_token: Option<String>,
    },
    HelloAck {
        proto_version: Version,
        receiver_name: String,
    },
    PairBegin {
        device_name: String,
        spake_message: Vec<u8>,
    },
    PairChallenge {
        spake_reply: Vec<u8>,
        receiver_fingerprint: String,
        receiver_confirmation: [u8; 32],
    },
    PairVerify {
        phone_confirmation: [u8; 32],
    },
    PairResult {
        accepted: bool,
        reason: Option<String>,
        outcome: Option<PairingOutcomeInfo>,
    },
    SessionOffer {
        video: VideoOffer,
        audio: AudioOffer,
    },
    SessionAnswer {
        accepted: bool,
        reason: Option<String>,
        max_video_bitrate_kbps: u32,
    },
    KeyframeRequest,
    BitrateHint {
        video_kbps: u32,
    },
    ClockSync {
        t1: u64,
        t2: u64,
        t3: u64,
        t4: u64,
    },
    Ping {
        sender_time_ms: u64,
    },
    Pong {
        echoed_time_ms: u64,
    },
    Bye {
        reason: String,
    },
}

impl Message {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Hello {
                device_name,
                auth_token,
                ..
            } => {
                if !valid_name(device_name) {
                    return Err("invalid device_name");
                }
                match auth_token {
                    Some(t) if t.len() > MAX_TOKEN_LEN => return Err("auth_token too long"),
                    _ => {}
                }
                Ok(())
            }
            Self::HelloAck { receiver_name, .. } => {
                if !valid_name(receiver_name) {
                    Err("invalid receiver_name")
                } else {
                    Ok(())
                }
            }
            Self::PairBegin {
                device_name,
                spake_message,
            } => {
                if !valid_name(device_name) {
                    return Err("invalid device_name");
                }
                if spake_message.len() > MAX_SPAKE_MESSAGE_LEN {
                    Err("spake_message too long")
                } else {
                    Ok(())
                }
            }
            Self::PairChallenge {
                spake_reply,
                receiver_fingerprint,
                ..
            } => {
                if spake_reply.len() > MAX_SPAKE_MESSAGE_LEN {
                    return Err("spake_reply too long");
                }
                if receiver_fingerprint.len() != 64
                    || !receiver_fingerprint.chars().all(|c| c.is_ascii_hexdigit())
                {
                    Err("invalid fingerprint")
                } else {
                    Ok(())
                }
            }
            Self::PairResult {
                reason: Some(r), ..
            } if r.chars().count() > MAX_REASON_LEN => Err("reason too long"),
            Self::SessionAnswer {
                reason: Some(r), ..
            } if r.chars().count() > MAX_REASON_LEN => Err("reason too long"),
            Self::Bye { reason } if reason.chars().count() > MAX_REASON_LEN => {
                Err("reason too long")
            }
            _ => Ok(()),
        }
    }
}
