//! Minimal Connect-RPC (JSON codec) framing used by E2B's in-sandbox daemon `envd`.
//!
//! Streaming Connect messages are "enveloped": 1 flag byte, a 4-byte big-endian
//! length, then the payload. A frame with the `END_STREAM` flag carries the
//! trailer (`{"error": {...}, "metadata": {...}}`) instead of a message.

use anyhow::{bail, Result};
use bytes::{Buf, BytesMut};
use serde::Deserialize;

pub const FLAG_END_STREAM: u8 = 0b10;
const HEADER_LEN: usize = 5;

pub fn encode_envelope(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, PartialEq)]
pub struct Frame {
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn is_end_stream(&self) -> bool {
        self.flags & FLAG_END_STREAM != 0
    }
}

/// Incremental decoder: feed it network chunks, pull out complete frames.
#[derive(Default)]
pub struct Decoder {
    buf: BytesMut,
}

impl Decoder {
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        if self.buf.len() < HEADER_LEN {
            return None;
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < HEADER_LEN + len {
            return None;
        }
        let flags = self.buf[0];
        self.buf.advance(HEADER_LEN);
        let payload = self.buf.split_to(len).to_vec();
        Some(Frame { flags, payload })
    }
}

#[derive(Debug, Deserialize)]
pub struct EndStream {
    pub error: Option<ConnectError>,
}

#[derive(Debug, Deserialize)]
pub struct ConnectError {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// Turn an END_STREAM trailer into `Err` when the server reported an error.
pub fn check_end_stream(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    let end: EndStream = serde_json::from_slice(payload)?;
    if let Some(err) = end.error {
        bail!(
            "envd error [{}]: {}",
            err.code.unwrap_or_default(),
            err.message.unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_split_across_chunks() {
        let a = encode_envelope(br#"{"a":1}"#);
        let mut b = encode_envelope(b"{}");
        b[0] = FLAG_END_STREAM;
        let wire: Vec<u8> = [a, b].concat();

        let mut dec = Decoder::default();
        dec.push(&wire[..3]);
        assert!(dec.next_frame().is_none());
        dec.push(&wire[3..9]);
        assert!(dec.next_frame().is_none());
        dec.push(&wire[9..]);

        let f1 = dec.next_frame().unwrap();
        assert_eq!(f1.payload, br#"{"a":1}"#);
        assert!(!f1.is_end_stream());
        let f2 = dec.next_frame().unwrap();
        assert!(f2.is_end_stream());
        assert!(dec.next_frame().is_none());
    }

    #[test]
    fn end_stream_error_is_surfaced() {
        assert!(check_end_stream(b"{}").is_ok());
        let err = check_end_stream(br#"{"error":{"code":"not_found","message":"nope"}}"#);
        assert!(err.unwrap_err().to_string().contains("not_found"));
    }
}
