use anyhow::{Result, bail};

pub const AUTH: u8 = 1;
pub const AUTH_OK: u8 = 2;
pub const OPEN: u8 = 3;
pub const OPEN_OK: u8 = 4;
pub const OPEN_ERROR: u8 = 5;
pub const DATA: u8 = 6;
pub const CLOSE: u8 = 7;

#[derive(Debug)]
pub struct Frame {
    pub kind: u8,
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: u8, stream_id: u32, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            stream_id,
            payload: payload.into(),
        }
    }

    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + self.payload.len());
        out.push(1);
        out.push(self.kind);
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 6 {
            bail!("frame curto");
        }
        if data[0] != 1 {
            bail!("versao de protocolo invalida");
        }
        Ok(Self {
            kind: data[1],
            stream_id: u32::from_be_bytes(data[2..6].try_into()?),
            payload: data[6..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let decoded = Frame::decode(&Frame::new(DATA, 42, b"abc").encode()).unwrap();
        assert_eq!(decoded.kind, DATA);
        assert_eq!(decoded.stream_id, 42);
        assert_eq!(decoded.payload, b"abc");
    }

    #[test]
    fn rejects_short_or_unknown_version() {
        assert!(Frame::decode(&[1, DATA]).is_err());
        assert!(Frame::decode(&[2, DATA, 0, 0, 0, 1]).is_err());
    }
}
