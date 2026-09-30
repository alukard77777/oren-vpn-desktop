//! A deliberately tiny HTTP/1.1 client for the service's own Mihomo controller on loopback.
//! Only the service uses it (TUN on/off); the raw controller secret never leaves the service.

use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

pub(crate) fn request(
    port: u16,
    secret: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> io::Result<(u16, Vec<u8>)> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let body = body.unwrap_or("");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {secret}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    let mut response = Vec::new();
    (&mut stream)
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut response)?;
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(invalid("controller response is too large"));
    }
    parse_response(&response)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn parse_response(response: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("controller response has no header end"))?;
    let head = std::str::from_utf8(&response[..split])
        .map_err(|_| invalid("controller response head is not UTF-8"))?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| invalid("controller response has no status"))?;
    let chunked = head.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    });
    let body = &response[split + 4..];
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

fn dechunk(mut body: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| invalid("chunk size line is not terminated"))?;
        let size_text = std::str::from_utf8(&body[..line_end])
            .map_err(|_| invalid("chunk size is not UTF-8"))?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| invalid("chunk size is not hexadecimal"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err(invalid("chunk is truncated"));
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

/// `PATCH /configs {"tun":{"enable":<on>}}`. Verified against Mihomo v1.19.30 (source unchanged
/// in v1.19.32): only `enable` changes (other TUN fields come from the loaded config) and the
/// reply is 204 even if the adapter failed, so callers confirm with [`tun_enabled`] and the
/// adapter probe.
pub(crate) fn set_tun_enabled(port: u16, secret: &str, on: bool) -> io::Result<()> {
    let body = if on {
        r#"{"tun":{"enable":true}}"#
    } else {
        r#"{"tun":{"enable":false}}"#
    };
    let (status, _) = request(port, secret, "PATCH", "/configs", Some(body))?;
    if status == 204 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "PATCH /configs returned {status}"
        )))
    }
}

pub(crate) fn tun_enabled(port: u16, secret: &str) -> io::Result<bool> {
    let (status, body) = request(port, secret, "GET", "/configs", None)?;
    if status != 200 {
        return Err(io::Error::other(format!("GET /configs returned {status}")));
    }
    let document: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| invalid("GET /configs is not JSON"))?;
    Ok(document["tun"]["enable"].as_bool() == Some(true))
}
