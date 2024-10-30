#[derive(Debug, Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PedometerFwError {
    Misc,
    Flash,
    Postcard,
    Imu,
}

impl<S> From<sequential_storage::Error<S>> for PedometerFwError {
    fn from(_value: sequential_storage::Error<S>) -> Self {
        PedometerFwError::Flash
    }
}

impl From<postcard::Error> for PedometerFwError {
    fn from(_value: postcard::Error) -> Self {
        PedometerFwError::Postcard
    }
}

impl From<pedomet_rs_common::PedometerCommonError> for PedometerFwError {
    fn from(value: pedomet_rs_common::PedometerCommonError) -> Self {
        match value {
            pedomet_rs_common::PedometerCommonError::Postcard => PedometerFwError::Postcard,
        }
    }
}

impl From<embedded_hal_async::i2c::ErrorKind> for PedometerFwError {
    fn from(_value: embedded_hal_async::i2c::ErrorKind) -> Self {
        PedometerFwError::Imu
    }
}

pub type PedometerResult<T> = Result<T, PedometerFwError>;
