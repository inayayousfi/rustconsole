//! Transport-independent session lifecycle and channel behavior.

use std::fmt;

pub mod audio_datagram;
pub mod authentication;
pub mod input_datagram;
pub mod media_queue;
pub mod observability;
pub mod quic;
pub mod statistics;
pub mod video_datagram;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Connected,
    Authenticated,
    Negotiated,
    Streaming,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLifecycle {
    phase: SessionPhase,
}

impl SessionLifecycle {
    #[must_use]
    pub const fn connected() -> Self {
        Self {
            phase: SessionPhase::Connected,
        }
    }

    #[must_use]
    pub const fn phase(self) -> SessionPhase {
        self.phase
    }

    pub fn authenticate(&mut self) -> Result<(), SessionTransitionError> {
        self.transition(SessionPhase::Connected, SessionPhase::Authenticated)
    }

    pub fn negotiate(&mut self) -> Result<(), SessionTransitionError> {
        self.transition(SessionPhase::Authenticated, SessionPhase::Negotiated)
    }

    pub fn start_streaming(&mut self) -> Result<(), SessionTransitionError> {
        self.transition(SessionPhase::Negotiated, SessionPhase::Streaming)
    }

    pub fn close(&mut self) {
        self.phase = SessionPhase::Closed;
    }

    fn transition(
        &mut self,
        expected: SessionPhase,
        next: SessionPhase,
    ) -> Result<(), SessionTransitionError> {
        if self.phase != expected {
            return Err(SessionTransitionError {
                current: self.phase,
                attempted: next,
            });
        }

        self.phase = next;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionTransitionError {
    pub current: SessionPhase,
    pub attempted: SessionPhase,
}

impl fmt::Display for SessionTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot transition session from {:?} to {:?}",
            self.current, self.attempted
        )
    }
}

impl std::error::Error for SessionTransitionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_follows_connection_order() {
        let mut lifecycle = SessionLifecycle::connected();

        lifecycle.authenticate().unwrap();
        lifecycle.negotiate().unwrap();
        lifecycle.start_streaming().unwrap();

        assert_eq!(lifecycle.phase(), SessionPhase::Streaming);
    }

    #[test]
    fn lifecycle_rejects_skipped_phase() {
        let mut lifecycle = SessionLifecycle::connected();

        assert_eq!(
            lifecycle.start_streaming(),
            Err(SessionTransitionError {
                current: SessionPhase::Connected,
                attempted: SessionPhase::Streaming,
            })
        );
    }

    #[test]
    fn close_is_safe_from_every_phase_and_is_idempotent() {
        for phase in [
            SessionPhase::Connected,
            SessionPhase::Authenticated,
            SessionPhase::Negotiated,
            SessionPhase::Streaming,
            SessionPhase::Closed,
        ] {
            let mut lifecycle = SessionLifecycle { phase };
            lifecycle.close();
            lifecycle.close();
            assert_eq!(lifecycle.phase(), SessionPhase::Closed);
        }
    }
}
