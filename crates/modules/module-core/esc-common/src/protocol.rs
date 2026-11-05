//! Control-plane wire protocol.
//!
//! Every control frame is `u32 big-endian length ‖ JSON payload`, written on a
//! yamux stream. Data-plane streams carry exactly one [`StreamHeader`] frame
//! and then raw bytes in both directions, so the per-connection framing cost is
//! a single small write.
//!
//! ```text
//! spoke ──── yamux control stream ───▶ hub
//!          Hello { id, services[] }
//!        ◀── Ack { granted[], denied[] }
//!        (stream stays open: EOF == spoke gone)
//!
//! hub ──── yamux data stream ───▶ spoke
//!          StreamHeader { service }
//!        ◀──▶ raw bytes (bidirectional copy)
//! ```

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::MAX_FRAME_LEN;

/// Bumped whenever the wire format changes incompatibly. Both sides compare it
/// during the handshake and refuse to continue on a mismatch.
pub const PROTO_VERSION: u32 = 1;

/// First frame a spoke sends on a fresh control stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    /// Stable spoke identifier (from `spoke.id`).
    pub spoke_id: String,
    /// Services this spoke *claims* it can serve. The hub decides which of
    /// them (if any) actually get exposed.
    pub services: Vec<ServiceDecl>,
    /// Free-form agent version string, for operator diagnostics only.
    #[serde(default)]
    pub agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDecl {
    pub name: String,
    /// Local target, reported for diagnostics. Never trusted for routing.
    #[serde(default)]
    pub local: String,
}

/// Hub's answer to [`Hello`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ack {
    pub version: u32,
    /// Services the hub accepted and will forward.
    pub granted: Vec<String>,
    /// Services the hub refused, with a human-readable reason.
    pub denied: Vec<DeniedService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeniedService {
    pub name: String,
    pub reason: String,
}

/// Sent as the first frame of every data stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHeader {
    pub service: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// framing
// ─────────────────────────────────────────────────────────────────────────────

/// Write one length-prefixed JSON frame.
pub async fn write_msg<W, T>(writer: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize,
{
    let body = serde_json::to_vec(msg).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame serialization failed: {e}"),
        )
    })?;
    if body.len() > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {} > {MAX_FRAME_LEN}", body.len()),
        ));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    writer.write_all(&buf).await?;
    writer.flush().await
}

/// Read one length-prefixed JSON frame.
///
/// Returns `UnexpectedEof` when the peer closed cleanly before sending a frame,
/// which callers treat as "peer went away" rather than an error.
pub async fn read_msg<R, T>(reader: &mut R) -> std::io::Result<T>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} > {MAX_FRAME_LEN}"),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame deserialization failed: {e}"),
        )
    })
}

/// Split `host:port`, tolerating bracketed IPv6 literals.
pub fn split_host_port(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        return Some((host.to_string(), port.parse().ok()?));
    }
    let (host, port) = s.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_hello() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let msg = Hello {
                version: PROTO_VERSION,
                spoke_id: "nas".into(),
                services: vec![ServiceDecl { name: "grafana".into(), local: "127.0.0.1:3000".into() }],
                agent: "0.1.0".into(),
            };
            let mut buf = Vec::new();
            write_msg(&mut buf, &msg).await.unwrap();
            let back: Hello = read_msg(&mut &buf[..]).await.unwrap();
            assert_eq!(back.spoke_id, "nas");
            assert_eq!(back.services.len(), 1);
        });
    }

    #[test]
    fn rejects_oversized_frame() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let mut buf = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes().to_vec();
            buf.extend_from_slice(b"{}");
            let err = read_msg::<_, Hello>(&mut &buf[..]).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn eof_is_reported() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let empty: &[u8] = &[];
            let err = read_msg::<_, Ack>(&mut &empty[..]).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        });
    }

    #[test]
    fn host_port_parsing() {
        assert_eq!(split_host_port("a.b:4433"), Some(("a.b".into(), 4433)));
        assert_eq!(split_host_port("[::1]:4433"), Some(("::1".into(), 4433)));
        assert_eq!(split_host_port("::1:4433"), Some(("::1".into(), 4433)));
        assert_eq!(split_host_port(":4433"), None);
        assert_eq!(split_host_port("noport"), None);
    }
}
