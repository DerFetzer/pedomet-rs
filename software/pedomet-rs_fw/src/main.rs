#![no_std]
#![no_main]

mod error;
mod fmt;
mod imu;
mod storage_event_queue;

use defmt::{info, unwrap, warn};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::{Channel, Receiver, Sender, TrySendError},
};
use embassy_time::{Instant, Timer};
use imu::Imu;
#[cfg(not(feature = "defmt"))]
use panic_halt as _;
use static_cell::StaticCell;

#[cfg(feature = "defmt")]
use {defmt_rtt as _, panic_probe as _};

use core::mem;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_nrf::{
    self as _, bind_interrupts,
    gpio::{Input, Level, Output, OutputDrive, Pull},
    interrupt::{self, InterruptExt, Priority},
    peripherals::{self, TWISPI0},
    twim::{self, Frequency, Twim},
};
use nrf_softdevice::ble::{gatt_server, peripheral, Connection};
use nrf_softdevice::{
    ble::advertisement_builder::{
        Flag, LegacyAdvertisementBuilder, LegacyAdvertisementPayload, ServiceList, ServiceUuid16,
    },
    Flash,
};
use nrf_softdevice::{raw, Softdevice};
use pedomet_rs_common::PedometerEventType;
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

#[nrf_softdevice::gatt_service(uuid = "1c2a0000-abf2-4b98-ba1c-25d5ea728525")]
struct PedometerService {
    #[characteristic(uuid = "1c2a0001-abf2-4b98-ba1c-25d5ea728525", write)]
    request_events: u32,
    #[characteristic(uuid = "1c2a0002-abf2-4b98-ba1c-25d5ea728525", notify)]
    response_events: [u8; EVENT_RESPONSE_SIZE],
    #[characteristic(uuid = "1c2a0003-abf2-4b98-ba1c-25d5ea728525", write)]
    delete_events: u32,
    #[characteristic(uuid = "1c2a0004-abf2-4b98-ba1c-25d5ea728525", notify, write)]
    epoch_ms: u64,
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
static READ_EVENT_CHANNEL: StaticCell<
    Channel<CriticalSectionRawMutex, [u8; EVENT_RESPONSE_SIZE], 2>,
> = StaticCell::new();

#[embassy_executor::task]
async fn flash_task(
    sd: &'static Softdevice,
    command_receiver: Receiver<'static, CriticalSectionRawMutex, FlashCommand, 4>,
    event_sender: Sender<'static, CriticalSectionRawMutex, [u8; EVENT_RESPONSE_SIZE], 2>,
) {
    let flash = Flash::take(sd);
    let mut event_queue = unwrap!(StorageEventQueue::new(flash).await);

    loop {
        let command = command_receiver.receive().await;
        info!("Received command: {:?}", command);
        match command {
            FlashCommand::PushEvent(event_type) => {
                if let Err(e) = event_queue.push_event(event_type, None).await {
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
    events_receiver: Receiver<'_, CriticalSectionRawMutex, [u8; EVENT_RESPONSE_SIZE], 2>,
) -> ! {
    loop {
        let response = events_receiver.receive().await;
        if let Err(e) = server
            .pedometer
            .response_events_notify(connection, &response)
        {
            warn!("Could not send event response! {:?}", e);
        }
    }
}

#[embassy_executor::task]
async fn imu_task(mut imu: Imu<Twim<'static, TWISPI0>>, mut imu_int: Input<'static>) {
    unwrap!(imu.dump_all_registers().await);

    unwrap!(imu.init().await);
    unwrap!(imu.enable_pedometer(true).await);
    unwrap!(imu.enable_fifo_for_pedometer(None).await);
    //unwrap!(imu.enable_fifo_for_pedometer(Some(3 * 20)).await);
    unwrap!(imu.dump_all_registers().await);

    imu_int.wait_for_low().await;
    loop {
        select(Timer::after_secs(10 * 60), imu_int.wait_for_rising_edge()).await;
        info!("Imu interrupt or timer elapsed");
        let steps = unwrap!(imu.read_steps_from_registers().await);
        info!(
            "From registers: {:?}@{}ms",
            steps,
            steps.timestamp.as_duration().as_millis()
        );
        let timestamp = unwrap!(imu.read_timestamp().await);
        info!(
            "Time: {:?}@{:?}",
            Instant::now().as_millis(),
            timestamp.as_duration().as_millis()
        );
        while let Some(steps) = unwrap!(imu.read_steps_from_fifo().await) {
            info!(
                "From FIFO: {:?}@{}ms",
                steps,
                steps.timestamp.as_duration().as_millis()
            );
        }
        let timestamp = unwrap!(imu.read_timestamp().await);
        info!(
            "{:?}@{:?}",
            Instant::now().as_millis(),
            timestamp.as_duration().as_millis()
        );
        imu_int.wait_for_low().await;
    }
}

bind_interrupts!(struct Irqs {
    SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0 => twim::InterruptHandler<peripherals::TWISPI0>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut nrf_hal_config = embassy_nrf::config::Config::default();
    nrf_hal_config.gpiote_interrupt_priority = Priority::P2;
    nrf_hal_config.time_interrupt_priority = Priority::P2;

    info!("Init nrf-hal");
    let peripherals = embassy_nrf::init(nrf_hal_config);

    info!("Init IMU");
    let mut imu_pwr = Output::new(peripherals.P1_08, Level::Low, OutputDrive::HighDrive);
    Timer::after_millis(20).await;
    imu_pwr.set_high();
    Timer::after_millis(20).await;

    interrupt::SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0.set_priority(interrupt::Priority::P3);
    let mut twi_config = twim::Config::default();
    twi_config.frequency = Frequency::K400;
    let twi = Twim::new(
        peripherals.TWISPI0,
        Irqs,
        peripherals.P0_07,
        peripherals.P0_27,
        twi_config,
    );
    let imu = Imu::new(twi);

    let imu_int = Input::new(peripherals.P0_11, Pull::None);
    unwrap!(spawner.spawn(imu_task(imu, imu_int)));

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
            p_value: b"pedomet-rs" as *const u8 as _,
            current_len: 10,
            max_len: 10,
            write_perm: unsafe { mem::zeroed() },
            _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(
                raw::BLE_GATTS_VLOC_STACK as u8,
            ),
        }),
        ..Default::default()
    };

    info!("Enable softdevice");
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
                    }
                }
                PedometerServiceEvent::ResponseEventsCccdWrite { notifications } => {
                    info!("pedometer response_events notifications: {}", notifications)
                }
                PedometerServiceEvent::DeleteEventsWrite(min_event_index) => {
                    info!("pedometer delete_events: {}", min_event_index);
                    if let Err(TrySendError::Full(_)) =
                        flash_command_channel.try_send(FlashCommand::DeleteEvents(min_event_index))
                    {
                        warn!("Could not send command.");
                    }
                }
                PedometerServiceEvent::EpochMsWrite(epoch_ms) => {
                    info!("pedometer time: {}", epoch_ms);
                    if let Err(TrySendError::Full(_)) = flash_command_channel.try_send(
                        FlashCommand::PushEvent(PedometerEventType::HostEpochMs(epoch_ms)),
                    ) {
                        warn!("Could not send command.");
                    } else if let Err(e) = server
                        .pedometer
                        .epoch_ms_notify(&conn, &Instant::now().as_millis())
                    {
                        info!("send notification error: {:?}", e);
                    }
                }
                PedometerServiceEvent::EpochMsCccdWrite { notifications } => {
                    info!("pedometer host_epoch_ms notifications: {}", notifications)
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
