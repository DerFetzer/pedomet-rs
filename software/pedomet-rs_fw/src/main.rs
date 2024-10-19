#![no_std]
#![no_main]

mod error;
mod fmt;
mod imu;
mod storage_event_queue;

use defmt::{info, unwrap, warn};
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    channel::{Channel, Receiver, Sender, TrySendError},
};
use heapless::Vec;
#[cfg(not(feature = "defmt"))]
use panic_halt as _;
use static_cell::StaticCell;

#[cfg(feature = "defmt")]
use {defmt_rtt as _, panic_probe as _};

use core::mem;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_nrf::{self as _, interrupt::Priority};
use nrf_softdevice::ble::{gatt_server, peripheral, Connection};
use nrf_softdevice::{
    ble::advertisement_builder::{
        Flag, LegacyAdvertisementBuilder, LegacyAdvertisementPayload, ServiceList, ServiceUuid16,
    },
    Flash,
};
use nrf_softdevice::{raw, Softdevice};
use pedomet_rs_common::{PedometerEvent, PedometerEventType};
use storage_event_queue::{BreakIteration, HandleEntry, PopEntry, StorageEventQueue};

#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice) -> ! {
    sd.run().await
}

#[nrf_softdevice::gatt_service(uuid = "180f")]
struct BatteryService {
    #[characteristic(uuid = "2a19", read, notify)]
    battery_level: u8,
}

const EVENT_RESPONSE_SIZE: usize = 250;
const MAX_EVENTS_IN_RESPONSE: usize =
    EVENT_RESPONSE_SIZE / PedometerEvent::get_max_serialized_transport_size();

#[nrf_softdevice::gatt_service(uuid = "1c2a0000-abf2-4b98-ba1c-25d5ea728525")]
struct PedometerService {
    #[characteristic(uuid = "1c2a0001-abf2-4b98-ba1c-25d5ea728525", write)]
    request_events: u32,
    #[characteristic(uuid = "1c2a0002-abf2-4b98-ba1c-25d5ea728525", notify)]
    response_events: [u8; EVENT_RESPONSE_SIZE],
    #[characteristic(uuid = "1c2a0003-abf2-4b98-ba1c-25d5ea728525", write)]
    delete_events: u32,
}

#[nrf_softdevice::gatt_server]
struct Server {
    bas: BatteryService,
    pedometer: PedometerService,
}

#[derive(Debug, Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum FlashCommand {
    PushEvent(PedometerEventType),
    GetEvents(u32),
    DeleteEvents(u32),
}

static FLASH_COMMAND_CHANNEL: StaticCell<Channel<CriticalSectionRawMutex, FlashCommand, 4>> =
    StaticCell::new();
static READ_EVENT_CHANNEL: StaticCell<Channel<NoopRawMutex, [u8; EVENT_RESPONSE_SIZE], 2>> =
    StaticCell::new();

#[embassy_executor::task]
async fn flash_task(
    sd: &'static Softdevice,
    command_receiver: Receiver<'static, CriticalSectionRawMutex, FlashCommand, 4>,
    event_sender: Sender<'static, NoopRawMutex, [u8; EVENT_RESPONSE_SIZE], 2>,
) {
    let flash = Flash::take(sd);
    let mut event_queue = unwrap!(StorageEventQueue::new(flash, 0).await);

    loop {
        let command = command_receiver.receive().await;
        info!("Received command: {:?}", command);
        match command {
            FlashCommand::PushEvent(event_type) => {
                if let Err(e) = event_queue.push_event(event_type).await {
                    warn!("Could not push event! {:?}", e);
                }
            }
            FlashCommand::GetEvents(min_event_index) => {
                let mut buf = [0u8; EVENT_RESPONSE_SIZE];
                let mut offset = 0;
                let mut num_events = 0;

                if let Err(e) = event_queue
                    .for_each(|event| {
                        let br = if event.index >= min_event_index {
                            match event
                                .serialize_for_transport(&mut buf[offset..])
                                .map(|buf| buf.len())
                            {
                                Ok(length) => {
                                    offset += length;
                                    num_events += 1;
                                    if offset >= buf.len() {
                                        BreakIteration::Break
                                    } else {
                                        BreakIteration::Continue
                                    }
                                }
                                Err(_e) => {
                                    // Zero out the non-used bytes
                                    buf[offset..].fill(0);
                                    BreakIteration::Break
                                }
                            }
                        } else {
                            BreakIteration::Continue
                        };
                        Ok(HandleEntry {
                            pop: PopEntry::Keep,
                            br,
                        })
                    })
                    .await
                {
                    warn!("Could not push event! {:?}", e);
                } else {
                    info!("Send {} events to notification task", num_events);
                    event_sender.send(buf).await;
                }
            }
            FlashCommand::DeleteEvents(min_event_index) => {
                if let Err(e) = event_queue
                    .for_each(|event| {
                        Ok(HandleEntry {
                            pop: PopEntry::Pop,
                            br: if event.index < min_event_index {
                                BreakIteration::Continue
                            } else {
                                BreakIteration::Break
                            },
                        })
                    })
                    .await
                {
                    warn!("Could not delete events! {:?}", e);
                }
            }
        }
    }
}

async fn notify_response_events(
    server: &Server,
    connection: &Connection,
    events_receiver: Receiver<'_, NoopRawMutex, [u8; EVENT_RESPONSE_SIZE], 2>,
) -> ! {
    loop {
        let response = events_receiver.receive().await;
        if let Err(e) = server
            .pedometer
            .response_events_notify(&connection, &response)
        {
            warn!("Could not send event response! {:?}", e);
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello World!");

    let mut nrf_hal_config = embassy_nrf::config::Config::default();
    nrf_hal_config.gpiote_interrupt_priority = Priority::P2;
    nrf_hal_config.time_interrupt_priority = Priority::P2;

    let _peripherals = embassy_nrf::init(nrf_hal_config);

    let softdevice_config = nrf_softdevice::Config {
        clock: Some(raw::nrf_clock_lf_cfg_t {
            source: raw::NRF_CLOCK_LF_SRC_XTAL as u8,
            rc_ctiv: 0,
            rc_temp_ctiv: 0,
            accuracy: raw::NRF_CLOCK_LF_ACCURACY_50_PPM as u8,
        }),
        conn_gap: Some(raw::ble_gap_conn_cfg_t {
            conn_count: 1,
            event_length: 24,
        }),
        conn_gatt: Some(raw::ble_gatt_conn_cfg_t { att_mtu: 256 }),
        gatts_attr_tab_size: Some(raw::ble_gatts_cfg_attr_tab_size_t {
            attr_tab_size: raw::BLE_GATTS_ATTR_TAB_SIZE_DEFAULT,
        }),
        gap_role_count: Some(raw::ble_gap_cfg_role_count_t {
            adv_set_count: 1,
            periph_role_count: 1,
            central_role_count: 0,
            central_sec_count: 0,
            _bitfield_1: raw::ble_gap_cfg_role_count_t::new_bitfield_1(0),
        }),
        gap_device_name: Some(raw::ble_gap_cfg_device_name_t {
            p_value: b"peodmet-rs" as *const u8 as _,
            current_len: 10,
            max_len: 10,
            write_perm: unsafe { mem::zeroed() },
            _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(
                raw::BLE_GATTS_VLOC_STACK as u8,
            ),
        }),
        ..Default::default()
    };

    let sd = Softdevice::enable(&softdevice_config);

    let server = unwrap!(Server::new(sd));
    unwrap!(spawner.spawn(softdevice_task(sd)));

    let flash_command_channel = FLASH_COMMAND_CHANNEL.init(Channel::new());
    let read_event_channel = READ_EVENT_CHANNEL.init(Channel::new());

    unwrap!(spawner.spawn(flash_task(
        sd,
        flash_command_channel.receiver(),
        read_event_channel.sender()
    )));

    static ADV_DATA: LegacyAdvertisementPayload = LegacyAdvertisementBuilder::new()
        .flags(&[Flag::GeneralDiscovery, Flag::LE_Only])
        .services_16(ServiceList::Complete, &[ServiceUuid16::BATTERY])
        .full_name("pedomet-rs")
        .build();

    static SCAN_DATA: LegacyAdvertisementPayload = LegacyAdvertisementBuilder::new()
        .services_128(
            ServiceList::Complete,
            &[0x9e7312e0_2354_11eb_9f10_fbc30a62cf38_u128.to_le_bytes()],
        )
        .build();

    unwrap!(server.bas.battery_level_set(&0xab));

    loop {
        let config = peripheral::Config::default();
        let adv = peripheral::ConnectableAdvertisement::ScannableUndirected {
            adv_data: &ADV_DATA,
            scan_data: &SCAN_DATA,
        };
        let conn = unwrap!(peripheral::advertise_connectable(sd, adv, &config).await);

        info!("advertising done!");

        // Run the GATT server on the connection. This returns when the connection gets disconnected.
        //
        // Event enums (ServerEvent's) are generated by nrf_softdevice::gatt_server
        // proc macro when applied to the Server struct above
        let gatt_fut = gatt_server::run(&conn, &server, |e| match e {
            ServerEvent::Bas(e) => match e {
                BatteryServiceEvent::BatteryLevelCccdWrite { notifications } => {
                    info!("battery notifications: {}", notifications)
                }
            },
            ServerEvent::Pedometer(e) => match e {
                PedometerServiceEvent::RequestEventsWrite(min_event_index) => {
                    info!("pedometer request_events from: {}", min_event_index);
                    if let Err(TrySendError::Full(_)) =
                        flash_command_channel.try_send(FlashCommand::GetEvents(min_event_index))
                    {
                        warn!("Could not send command.");
                        if let Err(e) = server.pedometer.response_events_notify(&conn, &[0; 250]) {
                            info!("send notification error: {:?}", e);
                        }
                    }
                }
                PedometerServiceEvent::ResponseEventsCccdWrite { notifications } => {
                    info!("pedometer notifications: {}", notifications)
                }
                PedometerServiceEvent::DeleteEventsWrite(min_event_index) => {
                    info!("pedometer delete_events: {}", min_event_index);
                    if let Err(TrySendError::Full(_)) =
                        flash_command_channel.try_send(FlashCommand::DeleteEvents(min_event_index))
                    {
                        warn!("Could not send command.");
                    }
                }
            },
        });
        let notify_response_fut =
            notify_response_events(&server, &conn, read_event_channel.receiver());

        match select(gatt_fut, notify_response_fut).await {
            Either::First(e) => {
                info!("gatt_server run exited with error: {:?}", e);
            }
            Either::Second(_) => {
                info!("notify_response exited");
            }
        };
    }
}
