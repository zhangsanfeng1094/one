//! Minimal WebSocket (RFC 6455) frame parser and encoder.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::sha1::sha1;

const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Compute `Sec-WebSocket-Accept` header response value.
pub fn compute_accept_key(sec_key: &str) -> String {
    let mut combined = Vec::with_capacity(sec_key.len() + WS_GUID.len());
    combined.extend_from_slice(sec_key.trim().as_bytes());
    combined.extend_from_slice(WS_GUID);
    let hash = sha1(&combined);

    const BASE64_TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(32);
    let mut i = 0;
    while i < hash.len() {
        let b0 = hash[i] as u32;
        let b1 = if i + 1 < hash.len() {
            hash[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < hash.len() {
            hash[i + 2] as u32
        } else {
            0
        };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(BASE64_TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(BASE64_TABLE[((triple >> 12) & 0x3F) as usize] as char);

        if i + 1 < hash.len() {
            out.push(BASE64_TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }

        if i + 2 < hash.len() {
            out.push(BASE64_TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }

        i += 3;
    }
    out
}

#[derive(Debug)]
pub enum WsFrame {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<u16>),
}

/// Read a single WebSocket frame from an async reader.
pub async fn read_ws_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<WsFrame> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head).await?;

    let fin = (head[0] & 0x80) != 0;
    let opcode = head[0] & 0x0F;
    let masked = (head[1] & 0x80) != 0;
    let mut payload_len = (head[1] & 0x7F) as u64;

    if payload_len == 126 {
        let mut ext = [0u8; 2];
        reader.read_exact(&mut ext).await?;
        payload_len = u16::from_be_bytes(ext) as u64;
    } else if payload_len == 127 {
        let mut ext = [0u8; 8];
        reader.read_exact(&mut ext).await?;
        payload_len = u64::from_be_bytes(ext);
    }

    let mask = if masked {
        let mut m = [0u8; 4];
        reader.read_exact(&mut m).await?;
        Some(m)
    } else {
        None
    };

    if payload_len > 32 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WebSocket payload exceeds 32MB limit",
        ));
    }

    let mut payload = vec![0u8; payload_len as usize];
    reader.read_exact(&mut payload).await?;

    if let Some(mask) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }

    match opcode {
        0x1 => {
            // Text frame
            let text = String::from_utf8(payload).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("Invalid UTF-8: {e}"))
            })?;
            Ok(WsFrame::Text(text))
        }
        0x2 => Ok(WsFrame::Binary(payload)),
        0x8 => {
            // Close frame
            let code = if payload.len() >= 2 {
                Some(u16::from_be_bytes([payload[0], payload[1]]))
            } else {
                None
            };
            Ok(WsFrame::Close(code))
        }
        0x9 => Ok(WsFrame::Ping(payload)),
        0xA => Ok(WsFrame::Pong(payload)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported opcode 0x{opcode:x}, fin={fin}"),
        )),
    }
}

/// Send a text frame to a WebSocket client.
pub async fn write_ws_text<W: AsyncWrite + Unpin>(writer: &mut W, text: &str) -> io::Result<()> {
    write_frame(writer, 0x1, text.as_bytes()).await
}

/// Send a pong frame.
pub async fn write_ws_pong<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    write_frame(writer, 0xA, data).await
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    opcode: u8,
    payload: &[u8],
) -> io::Result<()> {
    let mut header = Vec::with_capacity(10);
    // FIN + opcode (0x80 | opcode)
    header.push(0x80 | opcode);

    let len = payload.len();
    if len <= 125 {
        header.push(len as u8);
    } else if len <= 65535 {
        header.push(126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }

    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}
