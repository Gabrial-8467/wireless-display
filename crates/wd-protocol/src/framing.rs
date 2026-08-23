use std::io;

#[cfg(feature = "async")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::Message;
use crate::messages::MAX_MESSAGE_LEN;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame exceeds protocol limit: {0} bytes (max {1})")]
    TooLarge(usize, usize),
    #[error("unexpected end of stream while reading frame header")]
    UnexpectedEof,
    #[error("frame length {declared} does not match payload size {actual}")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("message decode failed: {0}")]
    Decode(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub fn encode_frame(message: &Message) -> Result<Vec<u8>, CodecError> {
    let body = postcard::to_allocvec(message).map_err(|e| CodecError::Decode(e.to_string()))?;
    let total = body.len() + 4;
    if total > MAX_MESSAGE_LEN {
        return Err(CodecError::TooLarge(total, MAX_MESSAGE_LEN));
    }
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub fn decode_frame(frame: &[u8]) -> Result<Message, CodecError> {
    if frame.len() < 4 {
        return Err(CodecError::UnexpectedEof);
    }
    let declared = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if declared > MAX_MESSAGE_LEN {
        return Err(CodecError::TooLarge(declared, MAX_MESSAGE_LEN));
    }
    let payload = &frame[4..];
    if declared != payload.len() {
        return Err(CodecError::LengthMismatch {
            declared,
            actual: payload.len(),
        });
    }
    validate_and_decode(payload)
}

fn validate_and_decode(payload: &[u8]) -> Result<Message, CodecError> {
    let message: Message =
        postcard::from_bytes(payload).map_err(|e| CodecError::Decode(e.to_string()))?;
    message
        .validate()
        .map_err(|reason| CodecError::Decode(reason.to_string()))?;
    Ok(message)
}

#[cfg(feature = "async")]
pub async fn write_frame<W>(sink: &mut W, message: &Message) -> Result<(), CodecError>
where
    W: AsyncWriteExt + Unpin + Send,
{
    let frame = encode_frame(message)?;
    sink.write_all(&frame)
        .await
        .map_err(|e| CodecError::Io(io::Error::other(e.to_string())))?;
    sink.flush()
        .await
        .map_err(|e| CodecError::Io(io::Error::other(e.to_string())))?;
    Ok(())
}

#[cfg(feature = "async")]
pub async fn read_frame<R>(source: &mut R) -> Result<Message, CodecError>
where
    R: AsyncReadExt + Unpin + Send,
{
    let mut header = [0u8; 4];
    match source.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(CodecError::UnexpectedEof);
        }
        Err(e) => return Err(CodecError::Io(io::Error::other(e.to_string()))),
    }
    let declared = u32::from_le_bytes(header) as usize;
    if declared > MAX_MESSAGE_LEN {
        return Err(CodecError::TooLarge(declared, MAX_MESSAGE_LEN));
    }
    let mut payload = vec![0u8; declared];
    match source.read_exact(&mut payload).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(CodecError::UnexpectedEof);
        }
        Err(e) => return Err(CodecError::Io(io::Error::other(e.to_string()))),
    }
    validate_and_decode(&payload)
}

#[cfg(feature = "async")]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioCodec, AudioOffer, VideoCodec, VideoOffer};

    fn all_sample_messages() -> Vec<Message> {
        vec![
            Message::Hello {
                proto_version: crate::Version::CURRENT,
                device_name: "Test Phone".into(),
                auth_token: Some(hex::encode([7u8; 32])),
            },
            Message::HelloAck {
                proto_version: crate::Version::CURRENT,
                receiver_name: "fedora".into(),
            },
            Message::PairBegin {
                device_name: "Phone".into(),
                spake_message: vec![1u8; 65],
            },
            Message::PairChallenge {
                spake_reply: vec![2u8; 65],
                receiver_fingerprint: "ab".repeat(32),
                receiver_confirmation: [9u8; 32],
            },
            Message::PairVerify {
                phone_confirmation: [3u8; 32],
            },
            Message::PairResult {
                accepted: true,
                reason: None,
                outcome: Some(crate::PairingOutcomeInfo {
                    device_id: "dev123".into(),
                    device_token: hex::encode([4u8; 32]),
                }),
            },
            Message::SessionOffer {
                video: VideoOffer {
                    codec: VideoCodec::H264Baseline,
                    width: 1920,
                    height: 1080,
                    fps: 60,
                    bitrate_kbps: 8000,
                },
                audio: AudioOffer {
                    codec: AudioCodec::Opus,
                    sample_rate: 48000,
                    channels: 2,
                },
            },
            Message::SessionAnswer {
                accepted: false,
                reason: Some("resolution too large".into()),
                max_video_bitrate_kbps: 25000,
            },
            Message::KeyframeRequest,
            Message::BitrateHint { video_kbps: 5000 },
            Message::ClockSync {
                t1: 1,
                t2: 2,
                t3: 3,
                t4: 4,
            },
            Message::Ping {
                sender_time_ms: 123456,
            },
            Message::Pong {
                echoed_time_ms: 123456,
            },
            Message::Bye {
                reason: "user closed app".into(),
            },
        ]
    }

    #[test]
    fn every_message_roundtrips_through_frame_codec() {
        for msg in all_sample_messages() {
            let frame = encode_frame(&msg).expect("encode");
            assert!(frame.len() <= MAX_MESSAGE_LEN);
            let decoded = decode_frame(&frame).expect("decode");
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn oversized_payload_is_rejected_at_encode_and_decode() {
        let big = Message::Bye {
            reason: "x".repeat(MAX_MESSAGE_LEN * 2),
        };
        match encode_frame(&big) {
            Err(e @ CodecError::TooLarge(..)) => assert!(e.to_string().contains("exceeds")),
            other => panic!("expected TooLarge failure, got {other:?}"),
        }
        let junk = vec![0xffu8; MAX_MESSAGE_LEN + 1];
        assert!(matches!(decode_frame(&junk), Err(CodecError::TooLarge(..))));
    }

    #[test]
    fn truncated_frames_error_without_panic() {
        let frame = encode_frame(&Message::KeyframeRequest).unwrap();
        for cut in 0..frame.len() {
            let result = decode_frame(&frame[..cut]);
            assert!(result.is_err(), "cut at {cut} should fail");
        }
    }

    #[test]
    fn garbage_bytes_decode_to_error_not_panic() {
        let seeds: Vec<Vec<u8>> = vec![
            vec![0u8; 16],
            vec![0xff; 64],
            (0u8..=64).collect(),
            b"\x01\x00\x00\x00\x00".to_vec(),
        ];
        for s in seeds {
            assert!(decode_frame(&s).is_err());
        }
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn frames_survive_async_roundtrip_via_duplex_pipe() {
        let msg = Message::Hello {
            proto_version: crate::Version::CURRENT,
            device_name: "Roundtrip".into(),
            auth_token: None,
        };
        let (mut writer, mut reader) = tokio::io::duplex(256);
        let task = tokio::spawn(async move { read_frame(&mut reader).await });
        write_frame(&mut writer, &msg).await.unwrap();
        drop(writer);
        let decoded = task.await.unwrap().unwrap();
        assert_eq!(decoded, msg);
    }
}
