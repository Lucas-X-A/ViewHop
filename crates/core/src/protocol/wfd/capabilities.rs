use super::rtsp::ProtocolError;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WfdParameters(BTreeMap<String, String>);

impl WfdParameters {
    pub fn parse(body: &[u8]) -> Result<Self, ProtocolError> {
        if body.len() > super::rtsp::MAX_MESSAGE {
            return Err(ProtocolError::TooLarge);
        }
        let text = std::str::from_utf8(body).map_err(|_| ProtocolError::InvalidUtf8)?;
        let mut values = BTreeMap::new();
        let lines = text.split("\r\n").filter(|line| !line.is_empty());
        for line in lines {
            if values.len() >= 128 {
                return Err(ProtocolError::TooLarge);
            }
            let (key, value) = line
                .split_once(':')
                .ok_or(ProtocolError::Malformed("capability"))?;
            if !key.starts_with("wfd_")
                || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || value.chars().any(char::is_control)
            {
                return Err(ProtocolError::Malformed("capability"));
            }
            if values.insert(key.into(), value.trim().into()).is_some() {
                return Err(ProtocolError::Malformed("duplicate capability"));
            }
        }
        Ok(Self(values))
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_bounded_parameters() {
        assert_eq!(
            WfdParameters::parse(b"wfd_audio_codecs: LPCM 00000002 00\r\n")
                .unwrap()
                .get("wfd_audio_codecs"),
            Some("LPCM 00000002 00")
        );
        assert!(WfdParameters::parse(b"wfd_x: a\nwfd_y: b").is_err());
        assert!(WfdParameters::parse(b"wfd_x: a\r\nwfd_x: b\r\n").is_err());
    }
}
