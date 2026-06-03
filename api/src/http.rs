// Minimal HTTP/1.1 server for Rinha 2026.
// No frameworks, no allocations per request.
// Uses pre-rendered response bodies for all possible fraud scores.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// Pre-rendered HTTP response bodies for each possible fraud_score (0.0 to 1.0 in 0.2 increments).
/// Actually fraud_score can be 0/5, 1/5, 2/5, 3/5, 4/5, 5/5 = 0.0, 0.2, 0.4, 0.6, 0.8, 1.0
pub static RESPONSE_BODIES: [&[u8]; 6] = [
    b"{\"approved\":true,\"fraud_score\":0.0}",
    b"{\"approved\":true,\"fraud_score\":0.2}",
    b"{\"approved\":true,\"fraud_score\":0.4}",
    b"{\"approved\":false,\"fraud_score\":0.6}",
    b"{\"approved\":false,\"fraud_score\":0.8}",
    b"{\"approved\":false,\"fraud_score\":1.0}",
];

/// HTTP 200 OK headers (shared prefix)
pub const HTTP_OK_PREFIX: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ";

/// HTTP 200 OK for /ready
pub const HTTP_READY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

/// HTTP 404
pub const HTTP_NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

/// HTTP 400 Bad Request
pub const HTTP_BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

/// Write a fraud score response to the stream.
/// fraud_count is 0..5 (number of frauds among top 5).
#[inline(always)]
pub fn write_fraud_response(stream: &mut TcpStream, fraud_count: usize) {
    let body = RESPONSE_BODIES[fraud_count];
    let mut buf = [0u8; 256];
    let mut pos = 0;
    
    // Copy HTTP OK prefix
    for &b in HTTP_OK_PREFIX {
        buf[pos] = b;
        pos += 1;
    }
    
    // Content-Length as ascii
    let cl = body.len();
    if cl >= 100 {
        buf[pos] = b'1';
        pos += 1;
        buf[pos] = b'0';
        pos += 1;
        buf[pos] = b'0' + (cl - 100) as u8;
        pos += 1;
    } else if cl >= 10 {
        buf[pos] = b'0' + (cl / 10) as u8;
        pos += 1;
        buf[pos] = b'0' + (cl % 10) as u8;
        pos += 1;
    } else {
        buf[pos] = b'0' + cl as u8;
        pos += 1;
    }
    
    // End headers
    let suffix = b"\r\nConnection: keep-alive\r\n\r\n";
    for &b in suffix {
        buf[pos] = b;
        pos += 1;
    }
    
    // Body
    for &b in body {
        buf[pos] = b;
        pos += 1;
    }
    
    let _ = stream.write_all(&buf[..pos]);
}

/// Parse a very simple HTTP/1.1 request.
/// Returns: (method, path, body_start_offset)
/// body_start_offset is the index in buf where body starts (after headers).
/// Returns None if request is incomplete or invalid.
pub fn parse_http_request(buf: &[u8]) -> Option<(&str, &str, usize)> {
    // Find end of first line (\r\n)
    let mut i = 0;
    while i + 1 < buf.len() && !(buf[i] == b'\r' && buf[i + 1] == b'\n') {
        i += 1;
    }
    if i + 1 >= buf.len() {
        return None;
    }
    let first_line = &buf[..i];
    
    // Parse: METHOD PATH HTTP/1.1
    let mut parts = first_line.split(|b| *b == b' ');
    let method = parts.next()?;
    let path = parts.next()?;
    
    let method_str = std::str::from_utf8(method).ok()?;
    let path_str = std::str::from_utf8(path).ok()?;
    
    // Find end of headers (\r\n\r\n)
    let mut header_end = i + 2;
    while header_end + 3 < buf.len() {
        if buf[header_end] == b'\r' && buf[header_end + 1] == b'\n'
            && buf[header_end + 2] == b'\r' && buf[header_end + 3] == b'\n' {
            header_end += 4;
            return Some((method_str, path_str, header_end));
        }
        header_end += 1;
    }
    
    None
}

/// Find Content-Length header value
pub fn get_content_length(buf: &[u8]) -> usize {
    let needle = b"Content-Length: ";
    let mut i = 0;
    while i + needle.len() < buf.len() {
        if &buf[i..i + needle.len()] == needle {
            i += needle.len();
            let mut val: usize = 0;
            while i < buf.len() && buf[i].is_ascii_digit() {
                val = val * 10 + (buf[i] - b'0') as usize;
                i += 1;
            }
            return val;
        }
        i += 1;
    }
    0
}

/// Handle a single connection.
/// Reads requests in a loop (keep-alive).
pub fn handle_connection(mut stream: TcpStream, mut handler: impl FnMut(&[u8]) -> FraudResult) {
    let mut buf = [0u8; 4096];
    let mut buf_len: usize = 0;
    
    loop {
        // Read more data
        match stream.read(&mut buf[buf_len..]) {
            Ok(0) => break, // Connection closed
            Ok(n) => buf_len += n,
            Err(_) => break,
        }
        
        // Try to parse requests
        while let Some((method, path, body_start)) = parse_http_request(&buf[..buf_len]) {
            let body_len = get_content_length(&buf[..body_start]);
            let total_len = body_start + body_len;
            
            if buf_len < total_len {
                break; // Need more data
            }
            
            let body = &buf[body_start..total_len];
            
            if method == "GET" && path == "/ready" {
                let _ = stream.write_all(HTTP_READY);
            } else if method == "POST" && path == "/fraud-score" {
                let result = handler(body);
                match result {
                    FraudResult::Score(fraud_count) => {
                        write_fraud_response(&mut stream, fraud_count);
                    }
                    FraudResult::Error => {
                        let _ = stream.write_all(HTTP_BAD_REQUEST);
                    }
                }
            } else {
                let _ = stream.write_all(HTTP_NOT_FOUND);
            }
            
            // Shift remaining data
            if buf_len > total_len {
                buf.copy_within(total_len..buf_len, 0);
                buf_len -= total_len;
            } else {
                buf_len = 0;
                break;
            }
        }
        
        // If buffer is getting full, reset
        if buf_len >= buf.len() - 1024 {
            buf_len = 0;
        }
    }
}

/// Result of fraud detection
pub enum FraudResult {
    /// fraud_count: 0..5 (number of frauds in top 5 neighbors)
    Score(usize),
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http() {
        let req = b"POST /fraud-score HTTP/1.1\r\nContent-Length: 10\r\n\r\n1234567890";
        let (method, path, body_start) = parse_http_request(req).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/fraud-score");
        assert_eq!(body_start, 50);
        assert_eq!(get_content_length(req), 10);
    }

    #[test]
    fn test_pre_rendered_bodies() {
        assert_eq!(RESPONSE_BODIES[0], b"{\"approved\":true,\"fraud_score\":0.0}");
        assert_eq!(RESPONSE_BODIES[5], b"{\"approved\":false,\"fraud_score\":1.0}");
    }
}