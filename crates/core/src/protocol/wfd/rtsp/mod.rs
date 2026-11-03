pub mod message;
pub mod parser;
pub mod transport;

pub use message::*;
pub use parser::*;

pub const MAX_HEADER: usize = 16 * 1024;
pub const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("RTSP message exceeds the size limit")]
    TooLarge,
    #[error("Invalid UTF-8 in RTSP message")]
    InvalidUtf8,
    #[error("Malformed RTSP message: {0}")]
    Malformed(&'static str),
    #[error("RTSP timeout")]
    Timeout,
    #[error("RTSP operation cancelled")]
    Cancelled,
    #[error("RTSP connection closed")]
    Closed,
    #[error("RTSP transport error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unsupported WFD capability: {0}")]
    Unsupported(&'static str),
    #[error("Receiver rejected RTSP (status {0})")]
    Rejected(u16),
    #[error("WFD media failure: {0}")]
    Media(String),
}
