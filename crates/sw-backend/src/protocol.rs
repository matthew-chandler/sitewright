//! Messages between the daemon and a backend worker.
//!
//! Each message is one frame on the worker's stdin or stdout: a 4-byte big-endian length
//! followed by that many bytes of JSON. A worker handles one request at a time, so the
//! conversation is strictly sequential:
//!
//! ```text
//! daemon                         worker
//!   Init            ─────────►
//!                   ◄─────────   Ready | Failed
//!   Request         ─────────►
//!                   ◄─────────   Log*            (console output)
//!                   ◄─────────   HostCall        (zero or more, each answered)
//!   HostReply       ─────────►
//!                   ◄─────────   Response | Failed
//! ```

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Largest accepted frame. Bodies have their own, smaller limits.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Daemon to worker.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToWorker {
    /// The backend's source files, sent once. Keys are paths such as `backend/index.js`.
    Init {
        files: BTreeMap<String, String>,
        memory_bytes: u64,
        stack_bytes: u64,
    },
    Request(RequestMsg),
    /// The answer to a `HostCall`: exactly one of the two is set.
    HostReply {
        ok: Option<Value>,
        error: Option<String>,
    },
}

/// One HTTP request for the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMsg {
    pub method: String,
    /// Path only, starting with `/`.
    pub path: String,
    /// Raw query string without the `?`, possibly empty.
    pub query: String,
    pub headers: Vec<(String, String)>,
    /// The body as text. Backends receive UTF-8 bodies only.
    pub body: String,
    /// How long the handler may run, in milliseconds.
    pub timeout_ms: u64,
}

/// One HTTP response from the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseMsg {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Worker to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromWorker {
    /// The backend's code loaded.
    Ready,
    Response(ResponseMsg),
    /// Loading or handling failed. After a `fatal` failure the worker exits.
    Failed {
        message: String,
        fatal: bool,
    },
    /// The backend asks the daemon to do something on its behalf, for example run SQL
    /// on the site's database. The worker waits for a `HostReply`.
    HostCall {
        op: String,
        args: Value,
    },
    Log {
        level: String,
        message: String,
    },
}

/// Write one frame and flush it.
pub fn write_frame<W: Write, T: Serialize>(out: &mut W, message: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(message)?;
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|_| bytes.len() <= MAX_FRAME_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "message too large"))?;
    out.write_all(&len.to_be_bytes())?;
    out.write_all(&bytes)?;
    out.flush()
}

/// Read one frame. `Ok(None)` means the other side closed the stream between frames.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(input: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut bytes = vec![0u8; len];
    input.read_exact(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_end_cleanly() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &FromWorker::Ready).unwrap();
        write_frame(
            &mut buf,
            &FromWorker::Log {
                level: "info".into(),
                message: "héllo".into(),
            },
        )
        .unwrap();
        let mut input = buf.as_slice();
        assert!(matches!(
            read_frame::<_, FromWorker>(&mut input).unwrap(),
            Some(FromWorker::Ready)
        ));
        match read_frame::<_, FromWorker>(&mut input).unwrap() {
            Some(FromWorker::Log { message, .. }) => assert_eq!(message, "héllo"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(read_frame::<_, FromWorker>(&mut input).unwrap().is_none());
    }

    #[test]
    fn oversized_and_truncated_frames_are_errors() {
        let mut input: &[u8] = &[0xff, 0xff, 0xff, 0xff, b'{'];
        assert!(read_frame::<_, FromWorker>(&mut input).is_err());
        let mut input: &[u8] = &[0, 0, 0, 9, b'{'];
        assert!(read_frame::<_, FromWorker>(&mut input).is_err());
    }
}
