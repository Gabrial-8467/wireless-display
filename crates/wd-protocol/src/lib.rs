mod framing;
mod messages;

pub use framing::{decode_frame, encode_frame, read_frame, write_frame, CodecError};
pub use messages::{
    AudioCodec, AudioOffer, Message, PairingOutcomeInfo, VideoCodec, VideoOffer, MAX_MESSAGE_LEN,
};

pub const PROTOCOL_NAME: &str = "WDL";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}

impl Version {
    pub const CURRENT: Self = Self { major: 1, minor: 0 };

    pub fn compatible_with(&self, peer: Self) -> bool {
        self.major == peer.major && peer.minor <= self.minor
    }
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_version_is_compatible() {
        assert!(Version::CURRENT.compatible_with(Version::CURRENT));
    }

    #[test]
    fn older_peer_within_major_is_compatible_newer_is_not() {
        let older = Version { major: 1, minor: 0 };
        let newer = Version { major: 1, minor: 3 };
        assert!(newer.compatible_with(older));
        assert!(!older.compatible_with(newer));
    }

    #[test]
    fn different_major_is_incompatible() {
        let a = Version { major: 1, minor: 0 };
        let b = Version { major: 2, minor: 0 };
        assert!(!a.compatible_with(b));
    }
}
