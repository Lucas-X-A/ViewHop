use super::rtsp::ProtocolError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NegotiationPhase {
    #[default]
    AwaitingOptions,
    CapabilityExchange,
    Configuring,
    Ready,
    Playing,
    Closed,
}

#[derive(Default)]
pub struct WfdSession {
    phase: NegotiationPhase,
}

impl WfdSession {
    #[must_use]
    pub fn phase(&self) -> NegotiationPhase {
        self.phase
    }
    pub fn advance(&mut self, next: NegotiationPhase) -> Result<(), ProtocolError> {
        use NegotiationPhase::*;
        if !matches!(
            (self.phase, next),
            (AwaitingOptions, CapabilityExchange)
                | (CapabilityExchange, Configuring)
                | (Configuring, Ready)
                | (Ready, Playing)
                | (Playing, Ready)
                | (_, Closed)
        ) {
            return Err(ProtocolError::Malformed("invalid WFD phase transition"));
        }
        self.phase = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_play_before_configuration() {
        let mut session = WfdSession::default();
        assert!(session.advance(NegotiationPhase::Playing).is_err());
        session
            .advance(NegotiationPhase::CapabilityExchange)
            .unwrap();
    }
}
