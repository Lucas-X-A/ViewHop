use crate::DeviceId;
use thiserror::Error;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionState {
    #[default]
    Idle,
    Discovering,
    DeviceSelected(DeviceId),
    Connecting(DeviceId),
    Negotiating(DeviceId),
    Streaming {
        device: DeviceId,
        simulated: bool,
    },
    Paused(DeviceId),
    Disconnecting,
    Error(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CaptureState {
    #[default]
    Stopped,
    Selecting,
    Ready,
    Running,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppState {
    pub session: SessionState,
    pub capture: CaptureState,
    pub discovering: bool,
}

#[derive(Debug, Error)]
#[error("Invalid session transition: {from:?} to {to:?}")]
pub struct TransitionError {
    pub from: SessionState,
    pub to: SessionState,
}

#[derive(Default)]
pub struct SessionMachine {
    state: AppState,
}

impl SessionMachine {
    #[must_use]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn transition(&mut self, next: SessionState) -> Result<(), TransitionError> {
        use SessionState::*;
        let valid = match (&self.state.session, &next) {
            (Idle, Discovering | DeviceSelected(_)) | (Discovering, Idle | DeviceSelected(_)) => {
                true
            }
            (DeviceSelected(a), Connecting(b)) | (Connecting(a), Negotiating(b)) => a == b,
            (Negotiating(a), Streaming { device: b, .. }) => a == b,
            (Negotiating(a), Paused(b)) => a == b,
            (Streaming { device: a, .. }, Paused(b)) | (Paused(a), Streaming { device: b, .. }) => {
                a == b
            }
            (
                DeviceSelected(_) | Connecting(_) | Negotiating(_) | Streaming { .. } | Paused(_),
                Disconnecting,
            ) => true,
            (Disconnecting | Error(_), Idle) => true,
            (_, Error(_)) => true,
            _ => false,
        };
        if !valid {
            return Err(TransitionError {
                from: self.state.session.clone(),
                to: next,
            });
        }
        self.state.session = next;
        Ok(())
    }

    pub fn set_discovering(&mut self, discovering: bool) {
        self.state.discovering = discovering;
    }

    pub fn set_capture(&mut self, capture: CaptureState) {
        self.state.capture = capture;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_resume_preserves_receiver_identity() {
        let id = DeviceId::try_new("tv").unwrap();
        let mut machine = SessionMachine::default();
        machine
            .transition(SessionState::DeviceSelected(id.clone()))
            .unwrap();
        machine
            .transition(SessionState::Connecting(id.clone()))
            .unwrap();
        machine
            .transition(SessionState::Negotiating(id.clone()))
            .unwrap();
        machine
            .transition(SessionState::Streaming {
                device: id.clone(),
                simulated: false,
            })
            .unwrap();
        machine
            .transition(SessionState::Paused(id.clone()))
            .unwrap();
        assert!(
            machine
                .transition(SessionState::Streaming {
                    device: DeviceId::try_new("other").unwrap(),
                    simulated: false
                })
                .is_err()
        );
        machine
            .transition(SessionState::Streaming {
                device: id,
                simulated: false,
            })
            .unwrap();
        machine.transition(SessionState::Disconnecting).unwrap();
    }
    #[test]
    fn connection_lifecycle_and_identity() {
        let id = DeviceId::try_new("tv").unwrap();
        let mut m = SessionMachine::default();
        m.transition(SessionState::Discovering).unwrap();
        m.transition(SessionState::DeviceSelected(id.clone()))
            .unwrap();
        assert!(
            m.transition(SessionState::Connecting(
                DeviceId::try_new("other").unwrap()
            ))
            .is_err()
        );
        m.transition(SessionState::Connecting(id.clone())).unwrap();
        m.transition(SessionState::Negotiating(id.clone())).unwrap();
        m.transition(SessionState::Streaming {
            device: id,
            simulated: true,
        })
        .unwrap();
        m.transition(SessionState::Disconnecting).unwrap();
        m.transition(SessionState::Idle).unwrap();
    }

    #[test]
    fn rejects_skips_and_recovers_error() {
        let mut m = SessionMachine::default();
        assert!(
            m.transition(SessionState::Negotiating(DeviceId::try_new("tv").unwrap()))
                .is_err()
        );
        assert_eq!(m.state().session, SessionState::Idle);
        m.transition(SessionState::Error("failure".into())).unwrap();
        m.transition(SessionState::Idle).unwrap();
    }
}
