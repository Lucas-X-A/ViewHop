use super::{MAX_HEADER, MAX_MESSAGE, Message, ProtocolError, StartLine};
use std::collections::HashSet;

fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Returns one complete message and bytes consumed; preserves pipelined input.
pub fn parse(input: &[u8]) -> Result<Option<(Message, usize)>, ProtocolError> {
    let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") else {
        if input.len() >= MAX_HEADER {
            return Err(ProtocolError::TooLarge);
        }
        return Ok(None);
    };
    let header_len = end + 4;
    if header_len > MAX_HEADER {
        return Err(ProtocolError::TooLarge);
    }
    let text = std::str::from_utf8(&input[..end]).map_err(|_| ProtocolError::InvalidUtf8)?;
    let mut lines = text.split("\r\n");
    let line = lines
        .next()
        .ok_or(ProtocolError::Malformed("missing start line"))?;
    if line.chars().any(char::is_control) {
        return Err(ProtocolError::Malformed("start line"));
    }
    let start = if let Some(rest) = line.strip_prefix("RTSP/1.0 ") {
        let (code, reason) = rest
            .split_once(' ')
            .ok_or(ProtocolError::Malformed("status"))?;
        if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) || reason.is_empty() {
            return Err(ProtocolError::Malformed("status"));
        }
        let status: u16 = code
            .parse()
            .map_err(|_| ProtocolError::Malformed("status"))?;
        if !(100..=599).contains(&status) {
            return Err(ProtocolError::Malformed("status"));
        }
        StartLine::Response {
            status,
            reason: reason.into(),
        }
    } else {
        let (method, rest) = line
            .split_once(' ')
            .ok_or(ProtocolError::Malformed("request"))?;
        let (uri, version) = rest
            .rsplit_once(' ')
            .ok_or(ProtocolError::Malformed("request"))?;
        if !token(method) || uri.is_empty() || uri.contains(' ') || version != "RTSP/1.0" {
            return Err(ProtocolError::Malformed("request"));
        }
        StartLine::Request {
            method: method.into(),
            uri: uri.into(),
        }
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut names = HashSet::new();
    let mut length = 0usize;
    for line in lines {
        if headers.len() >= 128 {
            return Err(ProtocolError::TooLarge);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(ProtocolError::Malformed("header"))?;
        let value = value.trim_matches(' ');
        if !token(name)
            || value.chars().any(char::is_control)
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err(ProtocolError::Malformed("invalid or duplicate header"));
        }
        if name.eq_ignore_ascii_case("Content-Length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ProtocolError::Malformed("Content-Length"));
            }
            length = value.parse().map_err(|_| ProtocolError::TooLarge)?;
        }
        headers.push((name.into(), value.into()));
    }
    let cseq = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("CSeq"))
        .ok_or(ProtocolError::Malformed("missing CSeq"))?
        .1
        .as_str();
    if cseq.is_empty() || !cseq.bytes().all(|b| b.is_ascii_digit()) || cseq.parse::<u32>().is_err()
    {
        return Err(ProtocolError::Malformed("CSeq"));
    }
    let total = header_len
        .checked_add(length)
        .filter(|n| *n <= MAX_MESSAGE)
        .ok_or(ProtocolError::TooLarge)?;
    if input.len() < total {
        return Ok(None);
    }
    Ok(Some((
        Message {
            start,
            headers,
            body: input[header_len..total].to_vec(),
        },
        total,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_fragmentation_and_pipelining() {
        let bytes = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 4\r\n\r\ntest";
        for len in 0..bytes.len() {
            assert!(parse(&bytes[..len]).unwrap().is_none());
        }
        let (msg, consumed) = parse(bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parse(&msg.serialize().unwrap()).unwrap().unwrap().0, msg);
        let pipeline = [bytes.as_slice(), bytes.as_slice()].concat();
        assert_eq!(parse(&pipeline).unwrap().unwrap().1, bytes.len());
    }
    #[test]
    fn rejects_untrusted_sizes_and_syntax() {
        for input in [
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: -1\r\n\r\n",
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 9999999999999999999999999999\r\n\r\n",
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 1\r\ncontent-length: 1\r\n\r\na",
            "OPTIONS * RTSP/1.0\r\nCSeq: 1\nInjected: yes\r\n\r\n",
            "OPTIONS * RTSP/1.0\r\n\r\n",
        ] {
            assert!(parse(input.as_bytes()).is_err(), "{input:?}");
        }
        assert!(parse(&vec![b'x'; MAX_HEADER]).is_err());
        assert!(parse(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nX: \xff\r\n\r\n").is_err());
    }
}
