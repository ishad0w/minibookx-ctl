//! Private inherited socket protocol. No addresses, paths, commands or raw registers.
use crate::power::PowerConfig;
use std::io;
pub const REQUEST_BYTES: usize = 16;
pub const REPLY_BYTES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Initialize { fan_control: bool },
    Heartbeat,
    Ready,
    Activate,
    Duty(u8),
    Restore,
    Power(PowerConfig),
}
impl Request {
    pub fn encode(&self, sequence: u32) -> [u8; REQUEST_BYTES] {
        let mut b = [0; REQUEST_BYTES];
        b[0] = 0x58;
        b[1] = 1;
        match self {
            Self::Initialize { fan_control } => {
                b[2] = 7;
                b[3] = u8::from(*fan_control);
            }
            Self::Heartbeat => b[2] = 8,
            Self::Ready => b[2] = 9,
            Self::Activate => b[2] = 1,
            Self::Duty(raw) => {
                b[2] = 2;
                b[3] = *raw
            }
            Self::Restore => b[2] = 5,
            Self::Power(p) => {
                b[2] = 6;
                b[3] = p.pl1_w.unwrap_or(0) as u8;
                b[4] = p.pl2_w.unwrap_or(0) as u8;
                b[5] = u8::from(p.enabled)
            }
        }
        b[8..12].copy_from_slice(&sequence.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> io::Result<(Self, u32)> {
        let bad = || io::Error::other("invalid bounded hardware request");
        if b.len() != REQUEST_BYTES
            || b[0] != 0x58
            || b[1] != 1
            || b[6..8] != [0, 0]
            || b[12..16] != [0; 4]
        {
            return Err(bad());
        }
        let request = match b[2] {
            7 if b[3] <= 1 && b[4..6] == [0; 2] => Self::Initialize {
                fan_control: b[3] == 1,
            },
            8 if b[3..6] == [0; 3] => Self::Heartbeat,
            9 if b[3..6] == [0; 3] => Self::Ready,
            1 if b[3..6] == [0; 3] => Self::Activate,
            2 if b[4..6] == [0; 2]
                && (b[3] == 0 || (crate::ec::MIN_RAW..=crate::ec::MAX_RAW).contains(&b[3])) =>
            {
                Self::Duty(b[3])
            }
            5 if b[3..6] == [0; 3] => Self::Restore,
            6 if b[5] <= 1 => {
                let p = PowerConfig {
                    enabled: b[5] == 1,
                    pl1_w: (b[3] != 0).then_some(u32::from(b[3])),
                    pl2_w: (b[4] != 0).then_some(u32::from(b[4])),
                };
                p.validate().map_err(io::Error::other)?;
                Self::Power(p)
            }
            _ => return Err(bad()),
        };
        Ok((request, u32::from_le_bytes(b[8..12].try_into().unwrap())))
    }
}

/// Broker-owned result of the independent tachometer guard. A recent plausible
/// value is not a calibrated RPM measurement or proof of a new sample now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum TachState {
    #[default]
    Unknown = 0,
    Stopped = 1,
    Starting = 2,
    Recent = 3,
    Failed = 4,
}
impl TachState {
    fn decode(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Unknown),
            1 => Some(Self::Stopped),
            2 => Some(Self::Starting),
            3 => Some(Self::Recent),
            4 => Some(Self::Failed),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Recent => "recent-plausible-feedback",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub sequence: u32,
    pub rejected: bool,
    pub changed: bool,
    pub temperature: Option<u8>,
    pub tach: Option<u16>,
    pub actual_raw: Option<u8>,
    pub emergency: bool,
    pub tach_state: TachState,
    /// Optional power application or restoration is explicitly deferred.
    /// This must never be reported to the caller as an applied power override.
    pub recovery_pending: bool,
}
impl Reply {
    pub fn encode(&self) -> [u8; REPLY_BYTES] {
        let mut b = [0; REPLY_BYTES];
        b[0] = 0x58;
        b[1] = 1;
        b[2] = u8::from(self.rejected);
        b[3] = u8::from(self.changed);
        b[4] = self.temperature.unwrap_or(0);
        b[5] = self.actual_raw.unwrap_or(u8::MAX);
        b[6..8].copy_from_slice(&self.tach.unwrap_or(u16::MAX).to_le_bytes());
        b[8..12].copy_from_slice(&self.sequence.to_le_bytes());
        b[12] = u8::from(self.emergency);
        b[13] = self.tach_state as u8;
        b[14] = u8::from(self.temperature.is_some());
        b[15] = u8::from(self.recovery_pending);
        b
    }
    pub fn decode(b: &[u8], sequence: u32) -> io::Result<Self> {
        if b.len() != REPLY_BYTES
            || b[0] != 0x58
            || b[1] != 1
            || b[2] > 1
            || b[3] > 1
            || !(b[5] == u8::MAX
                || b[5] == 0
                || (crate::ec::MIN_RAW..=crate::ec::MAX_RAW).contains(&b[5]))
            || b[12] > 1
            || TachState::decode(b[13]).is_none()
            || b[14] > 1
            || (b[14] == 0 && b[4] != 0)
            || b[15] > 1
            || u32::from_le_bytes(b[8..12].try_into().unwrap()) != sequence
        {
            return Err(io::Error::other("invalid hardware reply"));
        }
        if b[2] != 0 {
            return Err(io::Error::other(
                "hardware broker rejected request; see system journal",
            ));
        }
        let tach = u16::from_le_bytes(b[6..8].try_into().unwrap());
        Ok(Self {
            sequence,
            rejected: false,
            changed: b[3] != 0,
            temperature: (b[14] != 0).then_some(b[4]),
            tach: (tach != u16::MAX).then_some(tach),
            actual_raw: (b[5] != u8::MAX).then_some(b[5]),
            emergency: b[12] != 0,
            tach_state: TachState::decode(b[13]).unwrap(),
            recovery_pending: b[15] != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_round_trip_and_no_raw_register_opcode() {
        for r in [
            Request::Initialize { fan_control: false },
            Request::Initialize { fan_control: true },
            Request::Heartbeat,
            Request::Ready,
            Request::Activate,
            Request::Duty(0),
            Request::Duty(32),
            Request::Restore,
            Request::Power(PowerConfig {
                enabled: true,
                pl1_w: Some(28),
                pl2_w: Some(30),
            }),
        ] {
            assert_eq!(Request::decode(&r.encode(42)).unwrap(), (r, 42));
        }
        for op in [3, 4].into_iter().chain(10..=255) {
            let mut b = Request::Activate.encode(0);
            b[2] = op;
            assert!(Request::decode(&b).is_err());
        }
    }

    #[test]
    fn startup_mode_and_heartbeat_have_no_unbounded_operands() {
        let mut request = Request::Initialize { fan_control: false }.encode(1);
        request[3] = 2;
        assert!(Request::decode(&request).is_err());
        for request in [Request::Heartbeat, Request::Ready] {
            for index in 3..6 {
                let mut bytes = request.encode(2);
                bytes[index] = 1;
                assert!(Request::decode(&bytes).is_err());
            }
        }
    }
    #[test]
    fn rejects_injected_offsets_values_versions_and_wrong_lengths() {
        for raw in [1, 23, 31, 75, 91, 255] {
            assert!(Request::decode(&Request::Duty(raw).encode(0)).is_err());
        }
        let b = Request::Activate.encode(0);
        for n in 0..16 {
            assert!(Request::decode(&b[..n]).is_err());
        }
        for i in [0, 1, 3, 4, 5, 6, 7, 12, 13, 14, 15] {
            let mut b = b;
            b[i] = 255;
            assert!(Request::decode(&b).is_err());
        }
        assert!(
            Request::decode(
                &Request::Power(PowerConfig {
                    enabled: true,
                    pl1_w: Some(28),
                    pl2_w: Some(31)
                })
                .encode(0)
            )
            .is_err()
        );
    }
    #[test]
    fn reject_unmatched_reply_and_transport_error() {
        let r = Reply {
            sequence: 20,
            tach: Some(2500),
            ..Reply::default()
        };
        assert_eq!(Reply::decode(&r.encode(), 20).unwrap().tach, Some(2500));
        assert!(Reply::decode(&r.encode(), 21).is_err());
        assert!(
            Reply::decode(
                &Reply {
                    rejected: true,
                    ..r
                }
                .encode(),
                20
            )
            .is_err()
        );
    }

    #[test]
    fn replies_report_actual_fan_state_and_reject_unbounded_values() {
        let reply = Reply {
            sequence: 7,
            actual_raw: Some(crate::ec::MAX_RAW),
            emergency: true,
            ..Reply::default()
        };
        let decoded = Reply::decode(&reply.encode(), 7).unwrap();
        assert_eq!(decoded.actual_raw, Some(crate::ec::MAX_RAW));
        assert!(decoded.emergency);
        assert_eq!(
            Reply::decode(&Reply::default().encode(), 0)
                .unwrap()
                .actual_raw,
            None
        );
        for invalid in [1, 31, 75, 91] {
            let mut bytes = reply.encode();
            bytes[5] = invalid;
            assert!(Reply::decode(&bytes, 7).is_err());
        }
        for index in [12, 14, 15] {
            let mut bytes = reply.encode();
            bytes[index] = 2;
            assert!(Reply::decode(&bytes, 7).is_err());
        }
        for invalid in 5..=255 {
            let mut bytes = reply.encode();
            bytes[13] = invalid;
            assert!(Reply::decode(&bytes, 7).is_err());
        }
    }

    #[test]
    fn cached_feedback_preserves_unknown_values_and_broker_health() {
        let unknown = Reply::decode(&Reply::default().encode(), 0).unwrap();
        assert_eq!(unknown.temperature, None);
        assert_eq!(unknown.tach, None);
        assert_eq!(unknown.tach_state, TachState::Unknown);
        let snapshot = Reply {
            temperature: Some(0),
            tach: Some(1200),
            tach_state: TachState::Recent,
            ..Reply::default()
        };
        let decoded = Reply::decode(&snapshot.encode(), 0).unwrap();
        assert_eq!(decoded.temperature, Some(0));
        assert_eq!(decoded.tach, Some(1200));
        assert_eq!(decoded.tach_state, TachState::Recent);
    }
}
