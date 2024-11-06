use pedomet_rs_common::PedometerEventType;
use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub(crate) enum PedometerGuiError {
    #[error("Invalid event type for persistence: {:?}", .0)]
    InvalidEventType(PedometerEventType),
}

pub(crate) type PedometerGuiResult<T> = Result<T, PedometerGuiError>;
