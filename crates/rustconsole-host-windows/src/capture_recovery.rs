#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureFailure {
    Timeout,
    AccessLost,
    AccessDenied,
    DesktopNotReady,
    TemporarilyUnavailable,
    Fatal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureAction {
    Continue,
    Reinitialize,
    Fail,
}

pub const fn acquisition_action(failure: CaptureFailure) -> CaptureAction {
    match failure {
        CaptureFailure::Timeout => CaptureAction::Continue,
        CaptureFailure::AccessLost
        | CaptureFailure::AccessDenied
        | CaptureFailure::DesktopNotReady => CaptureAction::Reinitialize,
        CaptureFailure::TemporarilyUnavailable | CaptureFailure::Fatal => CaptureAction::Fail,
    }
}

pub const fn retry_initialization(failure: CaptureFailure, before_deadline: bool) -> bool {
    before_deadline
        && matches!(
            failure,
            CaptureFailure::AccessLost
                | CaptureFailure::AccessDenied
                | CaptureFailure::DesktopNotReady
                | CaptureFailure::TemporarilyUnavailable
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquisition_reinitializes_only_for_desktop_access_changes() {
        assert_eq!(
            acquisition_action(CaptureFailure::AccessLost),
            CaptureAction::Reinitialize
        );
        assert_eq!(
            acquisition_action(CaptureFailure::AccessDenied),
            CaptureAction::Reinitialize
        );
        assert_eq!(
            acquisition_action(CaptureFailure::Timeout),
            CaptureAction::Continue
        );
        assert_eq!(
            acquisition_action(CaptureFailure::DesktopNotReady),
            CaptureAction::Reinitialize
        );
        assert_eq!(
            acquisition_action(CaptureFailure::Fatal),
            CaptureAction::Fail
        );
    }

    #[test]
    fn initialization_retries_recoverable_failures_only_before_deadline() {
        assert!(retry_initialization(CaptureFailure::AccessLost, true));
        assert!(retry_initialization(CaptureFailure::AccessDenied, true));
        assert!(retry_initialization(
            CaptureFailure::TemporarilyUnavailable,
            true
        ));
        assert!(retry_initialization(CaptureFailure::DesktopNotReady, true));
        assert!(!retry_initialization(CaptureFailure::Fatal, true));
        assert!(!retry_initialization(CaptureFailure::AccessLost, false));
    }
}
