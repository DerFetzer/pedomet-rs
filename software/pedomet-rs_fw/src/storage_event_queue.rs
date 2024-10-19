use core::{cmp::max, ops::Range};

use defmt::info;
use embassy_time::Instant;
use embedded_storage_async::nor_flash::MultiwriteNorFlash;
use pedomet_rs_common::{PedometerEvent, PedometerEventType};
use sequential_storage::{cache::PagePointerCache, queue};

use crate::error::PedometerResult;

const FLASH_SIZE: u32 = 1024 * 1024;
const PAGE_SIZE: u32 = 4096;
const QUEUE_FLASH_SIZE: u32 = 64 * 1024;
const QUEUE_FLASH_RANGE: Range<u32> = (FLASH_SIZE - QUEUE_FLASH_SIZE)..FLASH_SIZE;
const QUEUE_FLASH_PAGE_COUNT: usize = (QUEUE_FLASH_SIZE / PAGE_SIZE) as usize;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HandleEntry {
    pub pop: PopEntry,
    pub br: BreakIteration,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PopEntry {
    Pop,
    Keep,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BreakIteration {
    Break,
    Continue,
}

#[derive(Debug)]
pub(crate) struct StorageEventQueue<S: embedded_storage_async::nor_flash::NorFlash> {
    flash: S,
    cache: PagePointerCache<QUEUE_FLASH_PAGE_COUNT>,
    next_event_index: u32,
    boot_id: u32,
}

impl<S: MultiwriteNorFlash> StorageEventQueue<S> {
    pub async fn new(flash: S, boot_id: u32) -> PedometerResult<Self> {
        info!("FLASH_SIZE: {}, PAGE_SIZE: {}, QUEUE_FLASH_SIZE: {}, QUEUE_FLASH_RANGE: {}, QUEUE_FLASH_PAGE_COUNT: {}", FLASH_SIZE, PAGE_SIZE, QUEUE_FLASH_SIZE, QUEUE_FLASH_RANGE, QUEUE_FLASH_PAGE_COUNT);
        let mut queue = Self {
            flash,
            cache: PagePointerCache::new(),
            next_event_index: 0,
            boot_id,
        };

        let mut max_event_index = 0;

        queue
            .for_each(|event| {
                max_event_index = max(max_event_index, event.index);
                Ok(HandleEntry {
                    pop: PopEntry::Keep,
                    br: BreakIteration::Continue,
                })
            })
            .await?;
        queue.next_event_index = max_event_index + 1;
        info!("max_event_index: {}", max_event_index);
        Ok(queue)
    }

    pub async fn clear(&mut self) -> PedometerResult<()> {
        Ok(sequential_storage::erase_all(&mut self.flash, QUEUE_FLASH_RANGE).await?)
    }

    pub async fn push_event(&mut self, event_type: PedometerEventType) -> PedometerResult<()> {
        let event_index = self.next_event_index;
        self.next_event_index += 1;

        let event = PedometerEvent {
            index: event_index,
            timestamp_ms: Instant::now().as_millis(),
            boot_id: self.boot_id,
            event_type,
        };

        let data = event.serialize()?;
        queue::push(
            &mut self.flash,
            QUEUE_FLASH_RANGE,
            &mut self.cache,
            &data,
            true,
        )
        .await?;
        Ok(())
    }

    pub async fn for_each<F>(&mut self, mut f: F) -> PedometerResult<()>
    where
        F: FnMut(PedometerEvent) -> PedometerResult<HandleEntry>,
    {
        let mut buf = [0_u8; PedometerEvent::get_max_serialized_size()];
        let mut iterator = queue::iter(&mut self.flash, QUEUE_FLASH_RANGE, &mut self.cache).await?;
        while let Some(entry) = iterator.next(&mut buf).await? {
            let event: PedometerEvent = postcard::from_bytes(&entry)?;
            let handle_entry = f(event)?;
            if handle_entry.pop == PopEntry::Pop {
                entry.pop().await?;
            }
            if handle_entry.br == BreakIteration::Break {
                break;
            }
        }
        Ok(())
    }
}
