use super::parser;
use super::{MAX_HEADER, MAX_MESSAGE, ProtocolError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartLine {
    Request { method: String, uri: String },
    Response { status: u16, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub start: StartLine,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Encodes one complete message, replacing Content-Length with the body size.
    ///
    /// Applies the inbound parser's limits and rejects header/start-line injection.
    pub fn serialize(&self) -> Result<Vec<u8>, ProtocolError> {
        use std::io::Write;

        if self.body.len() > MAX_MESSAGE || self.headers.len() > 128 {
            return Err(ProtocolError::TooLarge);
        }
        let mut bytes = Vec::new();
        match &self.start {
            StartLine::Request { method, uri } => write!(bytes, "{method} {uri} RTSP/1.0\r\n")?,
            StartLine::Response { status, reason } => {
                write!(bytes, "RTSP/1.0 {status:03} {reason}\r\n")?;
            }
        }
        for (key, value) in &self.headers {
            if key.chars().any(char::is_control) || value.chars().any(char::is_control) {
                return Err(ProtocolError::Malformed("header injection"));
            }
            if !key.eq_ignore_ascii_case("Content-Length") {
                write!(bytes, "{key}: {value}\r\n")?;
            }
            if bytes.len() > MAX_HEADER {
                return Err(ProtocolError::TooLarge);
            }
        }
        write!(bytes, "Content-Length: {}\r\n\r\n", self.body.len())?;
        bytes.extend_from_slice(&self.body);
        // Use the same validation for both inbound and outbound messages.
        let Some((validated, consumed)) = parser::parse(&bytes)? else {
            return Err(ProtocolError::Malformed("incomplete serialized message"));
        };
        if consumed != bytes.len() || validated.start != self.start {
            return Err(ProtocolError::Malformed("start line injection"));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Message {
        Message {
            start: StartLine::Request {
                method: "OPTIONS".into(),
                uri: "*".into(),
            },
            headers: vec![("CSeq".into(), "1".into())],
            body: Vec::new(),
        }
    }

    #[test]
    fn preserves_wire_format_and_replaces_content_length_for_binary_body() {
        let mut message = request();
        message
            .headers
            .push(("content-length".into(), "999".into()));
        message.headers.push(("X-Test".into(), "value".into()));
        message.body = vec![0, 0xff, b'\r', b'\n'];
        assert_eq!(
            message.serialize().unwrap(),
            b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nX-Test: value\r\nContent-Length: 4\r\n\r\n\0\xff\r\n"
        );
        message.start = StartLine::Response {
            status: 200,
            reason: "OK".into(),
        };
        assert_eq!(
            message.serialize().unwrap(),
            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nX-Test: value\r\nContent-Length: 4\r\n\r\n\0\xff\r\n"
        );
    }

    #[test]
    fn rejects_outbound_injection_and_duplicate_headers() {
        let mut message = request();
        message
            .headers
            .push(("X-Test".into(), "ok\r\nInjected: yes".into()));
        assert!(matches!(
            message.serialize(),
            Err(ProtocolError::Malformed("header injection"))
        ));
        message.headers[1] = ("cseq".into(), "2".into());
        assert!(matches!(
            message.serialize(),
            Err(ProtocolError::Malformed(_))
        ));
        message.headers.truncate(1);
        for start in [
            StartLine::Request {
                method: "OPTIONS".into(),
                uri: "* RTSP/1.0\r\nCSeq: 1\r\n\r\n".into(),
            },
            StartLine::Response {
                status: 200,
                reason: "OK\r\nCSeq: 1\r\n\r\n".into(),
            },
        ] {
            message.start = start;
            assert!(
                message.serialize().is_err(),
                "injected start line: {:?}",
                message.start
            );
        }
    }

    #[test]
    fn enforces_outbound_header_and_message_limits() {
        let mut message = request();
        message.headers.push(("X-Test".into(), String::new()));
        let overhead = message.serialize().unwrap().len();
        message.headers[1].1 = "x".repeat(MAX_HEADER - overhead);
        assert_eq!(message.serialize().unwrap().len(), MAX_HEADER);
        message.headers[1].1.push('x');
        assert!(matches!(message.serialize(), Err(ProtocolError::TooLarge)));
        message.headers.truncate(1);
        message.body.resize(MAX_MESSAGE, 0);
        assert!(matches!(message.serialize(), Err(ProtocolError::TooLarge)));
    }
}
