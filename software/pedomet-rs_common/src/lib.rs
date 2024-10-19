#![no_std]

use postcard::experimental::max_size::MaxSize;
use serde::{Deserialize, Serialize};

#[cfg(feature = "std")]
extern crate std;

#[derive(Debug, Copy, Clone, Serialize, Deserialize, MaxSize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PedometerEvent {
    pub index: u32,
    pub timestamp_ms: u64,
    pub boot_id: u32,
    pub event_type: PedometerEventType,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, MaxSize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PedometerEventType {
    HostEpochMs(u64),
    Steps(u32),
    Boot(u32),
}

impl PedometerEvent {
    pub fn serialize(
        &self,
    ) -> PedometerCommonResult<heapless::Vec<u8, { <Self as MaxSize>::POSTCARD_MAX_SIZE }>> {
        Ok(postcard::to_vec(&self)?)
    }

    pub fn serialize_for_transport<'a>(
        &self,
        buf: &'a mut [u8],
    ) -> PedometerCommonResult<&'a [u8]> {
        Ok(postcard::to_slice_cobs(&self, buf)?)
    }

    pub fn deserialize(buf: &[u8]) -> PedometerCommonResult<(Self, &[u8])> {
        Ok(postcard::take_from_bytes(buf)?)
    }

    pub fn deserialize_from_transport(buf: &mut [u8]) -> PedometerCommonResult<(Self, &mut [u8])> {
        Ok(postcard::take_from_bytes_cobs(buf)?)
    }

    pub const fn get_max_serialized_size() -> usize {
        Self::POSTCARD_MAX_SIZE
    }

    pub const fn get_max_serialized_transport_size() -> usize {
        let serialized_size = Self::get_max_serialized_size();

        // https://en.wikipedia.org/wiki/Consistent_Overhead_Byte_Stuffing
        serialized_size + 1 + (254.0 / serialized_size as f32 + 1.0) as usize
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PedometerCommonError {
    Postcard,
}

impl From<postcard::Error> for PedometerCommonError {
    fn from(_value: postcard::Error) -> Self {
        PedometerCommonError::Postcard
    }
}

pub type PedometerCommonResult<T> = Result<T, PedometerCommonError>;
