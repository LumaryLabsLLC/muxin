//! The wire format between the agent's client commands and its server, over a
//! Unix socket: one JSON request line, one JSON reply line; an attach then
//! switches to binary frames `[kind: u8][len: u32 BE][payload]`.

use std::io::{self, Read, Write};

use serde_json::Value;

/// Terminal bytes (both ways).
pub const DATA: u8 = 0;
/// Client to server: `[cols: u16 BE][rows: u16 BE]`.
pub const RESIZE: u8 = 1;
/// Server to client: the session was ended; `[code: i32 BE]`.
pub const EXIT: u8 = 2;
/// Server to client, for clients that ask (`"sizes": true`): the session's
/// size, `[cols: u16 BE][rows: u16 BE]`, on attach and whenever it changes.
pub const SIZE: u8 = 3;

const MAX_FRAME: usize = 16 << 20;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(kind);
    buf.extend((payload.len() as u32).to_be_bytes());
    buf.extend(payload);
    w.write_all(&buf)
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

pub fn write_line(w: &mut impl Write, value: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)
}

/// Reads one JSON line without reading past it (frames may follow).
pub fn read_line(r: &mut impl Read) -> io::Result<Value> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if r.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
    }
    serde_json::from_slice(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
