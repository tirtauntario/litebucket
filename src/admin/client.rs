//! Minimal blocking HTTP/1.1 client for the admin Unix socket (used by the
//! `litebucket admin` commands; no async runtime needed).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

pub struct AdminClient {
    socket: PathBuf,
}

impl AdminClient {
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call("GET", path, None::<&()>)
    }

    pub fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call("DELETE", path, None::<&()>)
    }

    pub fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.call(method, path, Some(body))
    }

    fn call<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let body = match body {
            Some(b) => serde_json::to_vec(b).map_err(|e| Error::other(e.to_string()))?,
            None => Vec::new(),
        };
        let mut stream = UnixStream::connect(&self.socket).map_err(|e| {
            Error::other(format!(
                "cannot connect to the admin socket {}: {e}\n  Is `litebucket serve` running with this config? Run the command as the server's user or root (in Docker: `docker compose exec litebucket litebucket admin ...`).",
                self.socket.display()
            ))
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        stream.set_write_timeout(Some(Duration::from_secs(60)))?;
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: litebucket-admin\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&body)?;
        let (status, payload) = read_response(&mut BufReader::new(stream))?;
        if status >= 400 {
            let msg = serde_json::from_slice::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or_else(|| String::from_utf8_lossy(&payload).into_owned());
            return Err(match status {
                400 => Error::config(msg),
                404 => Error::NotFound(msg),
                409 => Error::Conflict(msg),
                _ => Error::other(format!("admin API error {status}: {msg}")),
            });
        }
        serde_json::from_slice(&payload)
            .map_err(|e| Error::other(format!("invalid admin API response: {e}")))
    }
}

fn read_response(r: &mut impl BufRead) -> Result<(u16, Vec<u8>)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::other("invalid admin API response line"))?;
    let mut length: Option<usize> = None;
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                length = v.parse().ok();
            } else if k.eq_ignore_ascii_case("transfer-encoding")
                && v.eq_ignore_ascii_case("chunked")
            {
                chunked = true;
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| Error::other("invalid chunk size in admin API response"))?;
            if size == 0 {
                break;
            }
            let mut chunk = vec![0u8; size];
            r.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            line.clear();
            r.read_line(&mut line)?;
        }
    } else if let Some(n) = length {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
    }
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_length_and_chunked_bodies() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(read_response(&mut &raw[..]).unwrap(), (200, b"{}".to_vec()));
        let raw = b"HTTP/1.1 404 Not Found\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(
            read_response(&mut &raw[..]).unwrap(),
            (404, b"abcde".to_vec())
        );
    }
}
