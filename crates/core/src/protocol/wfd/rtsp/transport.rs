use super::parser;
use super::{MAX_MESSAGE, Message, ProtocolError};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

pub struct RtspTransport {
    stream: TcpStream,
    buffer: Vec<u8>,
    timeout: Duration,
}

impl RtspTransport {
    pub fn from_stream(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            timeout: Duration::from_secs(10),
        }
    }

    pub async fn connect(
        address: SocketAddr,
        cancel: &CancellationToken,
    ) -> Result<Self, ProtocolError> {
        let timeout = Duration::from_secs(10);
        let stream = tokio::select! {
            _ = cancel.cancelled() => return Err(ProtocolError::Cancelled),
            result = tokio::time::timeout(timeout, TcpStream::connect(address)) => result.map_err(|_| ProtocolError::Timeout)??,
        };
        Ok(Self::from_stream(stream))
    }

    pub async fn receive(&mut self, cancel: &CancellationToken) -> Result<Message, ProtocolError> {
        tokio::select! {
            _ = cancel.cancelled() => Err(ProtocolError::Cancelled),
            result = tokio::time::timeout(self.timeout, async {
                loop {
                    if let Some((message, consumed)) = parser::parse(&self.buffer)? {
                        self.buffer.drain(..consumed);
                        return Ok(message);
                    }
                    let remaining = MAX_MESSAGE.saturating_sub(self.buffer.len());
                    if remaining == 0 {
                        return Err(ProtocolError::TooLarge);
                    }
                    let mut chunk = [0u8; 4096];
                    let limit = remaining.min(chunk.len());
                    let count = self.stream.read(&mut chunk[..limit]).await?;
                    if count == 0 {
                        return Err(ProtocolError::Closed);
                    }
                    self.buffer.extend_from_slice(&chunk[..count]);
                }
            }) => result.map_err(|_| ProtocolError::Timeout)?,
        }
    }

    pub async fn send(
        &mut self,
        message: &Message,
        cancel: &CancellationToken,
    ) -> Result<(), ProtocolError> {
        let bytes = message.serialize()?;
        tokio::select! {
            _ = cancel.cancelled() => Err(ProtocolError::Cancelled),
            result = tokio::time::timeout(self.timeout, self.stream.write_all(&bytes)) => {
                result.map_err(|_| ProtocolError::Timeout)??;
                Ok(())
            },
        }
    }
}
