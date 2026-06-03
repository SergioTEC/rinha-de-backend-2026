use std::io::{Read, Write};
use std::net::TcpListener;

static SCORE_RESPONSES: [&[u8]; 6] = [
    b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.0}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":1.0}",
];

pub const HTTP_READY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
pub const HTTP_NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
pub const HTTP_BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

#[inline(always)]
pub fn write_fraud_response(stream: &mut impl Write, fraud_count: usize) {
    let _ = stream.write_all(SCORE_RESPONSES[fraud_count]);
}

#[inline]
pub fn parse_http_request(buf: &[u8]) -> Option<(&str, &str, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() && !(buf[i] == b'\r' && buf[i + 1] == b'\n') {
        i += 1;
    }
    if i + 1 >= buf.len() { return None; }
    
    let mut parts = buf[..i].split(|b| *b == b' ');
    let method = parts.next()?;
    let path = parts.next()?;
    let method_str = std::str::from_utf8(method).ok()?;
    let path_str = std::str::from_utf8(path).ok()?;
    
    let mut header_end = i + 2;
    while header_end + 3 < buf.len() {
        if buf[header_end] == b'\r' && buf[header_end + 1] == b'\n'
            && buf[header_end + 2] == b'\r' && buf[header_end + 3] == b'\n' {
            return Some((method_str, path_str, header_end + 4));
        }
        header_end += 1;
    }
    None
}

#[inline]
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

pub fn handle_connection(mut stream: impl Write + Read, mut handler: impl FnMut(&[u8]) -> FraudResult) {
    let mut buf = [0u8; 4096];
    let mut buf_len: usize = 0;
    
    loop {
        match stream.read(&mut buf[buf_len..]) {
            Ok(0) => break,
            Ok(n) => buf_len += n,
            Err(_) => break,
        }
        
        while let Some((method, path, body_start)) = parse_http_request(&buf[..buf_len]) {
            let body_len = get_content_length(&buf[..body_start]);
            let total_len = body_start + body_len;
            
            if buf_len < total_len { break; }
            
            let body = &buf[body_start..total_len];
            
            if method == "GET" && path == "/ready" {
                let _ = stream.write_all(HTTP_READY);
            } else if method == "POST" && path == "/fraud-score" {
                match handler(body) {
                    FraudResult::Score(fraud_count) => write_fraud_response(&mut stream, fraud_count),
                    FraudResult::Error => { let _ = stream.write_all(HTTP_BAD_REQUEST); }
                }
            } else {
                let _ = stream.write_all(HTTP_NOT_FOUND);
            }
            
            if buf_len > total_len {
                buf.copy_within(total_len..buf_len, 0);
                buf_len -= total_len;
            } else {
                buf_len = 0;
                break;
            }
        }
        
        if buf_len >= buf.len() - 1024 {
            buf_len = 0;
        }
    }
}

pub enum FraudResult {
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
}