use anyhow::anyhow;
use btleplug::api::{Central, Characteristic, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use log::{debug, error, info, warn};
use pedomet_rs_common::{PedometerEvent, PedometerEventType};
use std::cmp::max;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::persistence::{PedometerDatabaseCommand, PedometerPersistenceEvent};

/// Only devices whose name contains this string will be tried.
const PERIPHERAL_NAME_MATCH_FILTER: &str = "pedomet-rs";

/// Service
const SERVICE_UUID_PEDOMETER: Uuid = Uuid::from_u128(0x1C2A0000_ABF2_4B98_BA1C_25D5EA728525);
/// Characteristics
const CHARACTERISTIC_UUID_SOC: Uuid = Uuid::from_u128(0x00002A19_0000_1000_8000_00805F9B34FB);
const CHARACTERISTIC_UUID_REQUEST_EVENTS: Uuid =
    Uuid::from_u128(0x1C2A0001_ABF2_4B98_BA1C_25D5EA728525);
const CHARACTERISTIC_UUID_RESPONSE_EVENTS: Uuid =
    Uuid::from_u128(0x1C2A0002_ABF2_4B98_BA1C_25D5EA728525);
const CHARACTERISTIC_UUID_DELETE_EVENTS: Uuid =
    Uuid::from_u128(0x1C2A0003_ABF2_4B98_BA1C_25D5EA728525);
const CHARACTERISTIC_UUID_EPOCH_MS: Uuid = Uuid::from_u128(0x1C2A0004_ABF2_4B98_BA1C_25D5EA728525);

const SUB_CHARACTERISTICS: [Uuid; 3] = [
    CHARACTERISTIC_UUID_SOC,
    CHARACTERISTIC_UUID_EPOCH_MS,
    CHARACTERISTIC_UUID_RESPONSE_EVENTS,
];

#[derive(Debug)]
pub(crate) struct PedometerDeviceHandler {
    db_cmd_sender: mpsc::Sender<PedometerDatabaseCommand>,
    ble_cmd_sender: mpsc::Sender<PedometerDeviceHandlerCommand>,
    adapter: Adapter,
    device: Option<Peripheral>,
}

impl PedometerDeviceHandler {
    pub(crate) async fn new(
        db_cmd_sender: mpsc::Sender<PedometerDatabaseCommand>,
        ble_cmd_sender: mpsc::Sender<PedometerDeviceHandlerCommand>,
    ) -> anyhow::Result<Self> {
        let manager = Manager::new().await?;
        let adapter_list = manager.adapters().await?;
        if adapter_list.is_empty() {
            error!("Could not find any adapters");
        }
        let adapter = adapter_list.first().unwrap().clone();
        Ok(Self {
            db_cmd_sender,
            ble_cmd_sender,
            adapter,
            device: None,
        })
    }

    pub(crate) async fn spawn_message_handler(
        mut self,
        mut event_receiver: mpsc::Receiver<PedometerDeviceHandlerCommand>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = event_receiver.recv().await {
                match cmd {
                    PedometerDeviceHandlerCommand::TryConnect { responder } => {
                        let _ = responder.send(self.try_connect().await);
                    }
                    PedometerDeviceHandlerCommand::IsConnected { responder } => {
                        let _ = responder.send(self.is_connected().await);
                    }
                    PedometerDeviceHandlerCommand::RequestEvents {
                        min_event_id,
                        responder,
                    } => {
                        let _ =
                            responder.send(self.request_events(min_event_id.unwrap_or(0)).await);
                    }
                    PedometerDeviceHandlerCommand::DeleteEvents {
                        max_event_id,
                        responder,
                    } => {}
                    PedometerDeviceHandlerCommand::Exit => break,
                }
            }
        })
    }

    async fn try_connect(&mut self) -> anyhow::Result<()> {
        if self.is_connected().await? {
            return Ok(());
        }
        if self.device.is_none() {
            info!("Starting scan on {}...", self.adapter.adapter_info().await?);

            self.adapter
                .start_scan(ScanFilter {
                    //services: vec![SERVICE_UUID_PEDOMETER],
                    services: vec![],
                })
                .await?;

            tokio::time::sleep(Duration::from_secs(5)).await;

            if let Ok(Some(device)) = find_device(&self.adapter).await {
                info!("Found device: {:?}", device);
                self.device = Some(device);
            } else {
                warn!("Could not find device");
                return Err(anyhow!("Could not find device"));
            }
        }
        if let Some(device) = &self.device {
            device.connect().await?;
            device.discover_services().await?;

            tokio::time::sleep(Duration::from_secs(1)).await;

            for uuid in SUB_CHARACTERISTICS {
                if let Some(soc_char) = find_characteristic(device, uuid) {
                    info!("Found characteristic: {:?}", soc_char);
                    device.subscribe(&soc_char).await?;
                } else {
                    warn!("Could not find characteristic: {}", uuid);
                }
            }
            info!("Send current time to device...");
            let epoch_ms_char = find_characteristic(device, CHARACTERISTIC_UUID_EPOCH_MS).unwrap();
            device
                .write(
                    &epoch_ms_char,
                    &((OffsetDateTime::now_utc().unix_timestamp_nanos() / 1000 / 1000) as u64)
                        .to_le_bytes(),
                    btleplug::api::WriteType::WithResponse,
                )
                .await
                .unwrap();

            let mut notification_stream = device.notifications().await?;
            let db_command_sender = self.db_cmd_sender.clone();
            let ble_command_sender = self.ble_cmd_sender.clone();
            tokio::spawn(async move {
                while let Some(mut notification) = notification_stream.next().await {
                    let mut event_queue = VecDeque::new();
                    let mut device_time_offsets = HashMap::new();
                    let mut max_time_offset_boot_id = 0;
                    match notification.uuid {
                        CHARACTERISTIC_UUID_RESPONSE_EVENTS => {
                            info!("Got event response");
                            let mut buf = &mut notification.value[..];
                            let mut max_event_id = 0;
                            let mut received_events = false;
                            while let Ok((event, rest)) =
                                PedometerEvent::deserialize_from_transport(buf)
                            {
                                received_events = true;
                                buf = rest;
                                info!("Got event from device: {event:?}");
                                max_event_id = max(event.index, max_event_id);
                                debug!("Set max_event_id to {max_event_id}");
                                match event.event_type {
                                    PedometerEventType::HostEpochMs(host_epoch_ms) => {
                                        if host_epoch_ms >= event.timestamp_ms {
                                            device_time_offsets.insert(
                                                event.boot_id,
                                                Duration::from_millis(
                                                    host_epoch_ms - event.timestamp_ms,
                                                ),
                                            );
                                            max_time_offset_boot_id =
                                                max(max_time_offset_boot_id, event.boot_id);
                                        } else {
                                            warn!("Got invalid host epoch event: {event:?}");
                                        }
                                    }
                                    PedometerEventType::Steps(_) => event_queue.push_back(event),
                                    PedometerEventType::Boot => {}
                                }
                            }
                            let mut events_retain = Vec::with_capacity(event_queue.len());
                            for event in &event_queue {
                                if let PedometerEventType::Steps(_) = event.event_type {
                                    match device_time_offsets.get(&event.boot_id) {
                                        None if event.boot_id < max_time_offset_boot_id => {
                                            warn!("Dropped step event because the device time offset could not be determined anymore: {event:?}");
                                            events_retain.push(false);
                                            continue;
                                        }
                                        None => {
                                            info!("Wait for timestamp");
                                            events_retain.push(true);
                                        }
                                        Some(offset) => {
                                            match PedometerPersistenceEvent::from_common_event(
                                                *event, *offset,
                                            ) {
                                                Ok(persistence_event) => {
                                                    let (responder_tx, responder_rx) =
                                                        oneshot::channel();
                                                    info!(
                                                        "Send event to db: {persistence_event:?}"
                                                    );
                                                    if let Err(e) = db_command_sender.try_send(
                                                        PedometerDatabaseCommand::AddEvent {
                                                            event: persistence_event,
                                                            responder: responder_tx,
                                                        },
                                                    ) {
                                                        warn!("Could not send event to database! ({e})");
                                                        events_retain.push(true);
                                                    } else if let Err(e) = responder_rx.await {
                                                        warn!("Could not add event to db: {e}");
                                                        events_retain.push(false);
                                                    } else {
                                                        events_retain.push(false);
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Could not convert event: {event:?} -> {e}"
                                                    );
                                                    events_retain.push(false);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    error!("This event should not be here! {event:?}");
                                }
                            }
                            info!("Max event id: {max_event_id}");
                            if received_events {
                                info!("Try to read more events");

                                let (resp_tx, _resp_rx) = oneshot::channel();
                                let _ = ble_command_sender
                                    .send(PedometerDeviceHandlerCommand::RequestEvents {
                                        min_event_id: Some(max_event_id + 1),
                                        responder: resp_tx,
                                    })
                                    .await;
                            }
                            info!("Retain events: {event_queue:?} {events_retain:?}");
                            let mut retain_iter = events_retain.iter();
                            event_queue.retain(|_| *retain_iter.next().unwrap());
                        }
                        CHARACTERISTIC_UUID_EPOCH_MS => {
                            // Process event instead
                            info!("Received epoch characteristic: {:?}", notification.value);
                        }
                        CHARACTERISTIC_UUID_SOC => {
                            // Todo!
                            info!("Received soc characteristic: {:?}", notification.value);
                        }
                        char => warn!("Received unknown characteristic: {char}"),
                    }
                }
            });
        }
        Ok(())
    }

    async fn is_connected(&self) -> anyhow::Result<bool> {
        Ok(match &self.device {
            Some(device) => device.is_connected().await?,
            None => false,
        })
    }

    async fn request_events(&self, min_event_id: u32) -> anyhow::Result<()> {
        match &self.device {
            Some(device) if device.is_connected().await? => {
                device
                    .write(
                        &find_characteristic(device, CHARACTERISTIC_UUID_REQUEST_EVENTS).unwrap(),
                        &min_event_id.to_le_bytes(),
                        btleplug::api::WriteType::WithResponse,
                    )
                    .await?
            }
            Some(_) => Err(anyhow!("Not connected"))?,
            None => Err(anyhow!("Device not seen, yet"))?,
        };
        Ok(())
    }
}

pub(crate) enum PedometerDeviceHandlerCommand {
    TryConnect {
        responder: oneshot::Sender<anyhow::Result<()>>,
    },
    IsConnected {
        responder: oneshot::Sender<anyhow::Result<bool>>,
    },
    RequestEvents {
        min_event_id: Option<u32>,
        responder: oneshot::Sender<anyhow::Result<()>>,
    },
    DeleteEvents {
        max_event_id: Option<u32>,
        responder: oneshot::Sender<anyhow::Result<()>>,
    },
    Exit,
}

async fn find_device(central: &Adapter) -> anyhow::Result<Option<Peripheral>> {
    for p in central.peripherals().await? {
        if let Some(pp) = p.properties().await? {
            if pp
                .local_name
                .iter()
                .any(|name| name.contains(PERIPHERAL_NAME_MATCH_FILTER))
            {
                return Ok(Some(p));
            }
        }
    }
    Ok(None)
}

fn find_characteristic(peripheral: &Peripheral, uuid: Uuid) -> Option<Characteristic> {
    for c in peripheral.characteristics() {
        debug!("Characteristic: {:?}", c);
        if c.uuid == uuid {
            return Some(c);
        }
    }
    None
}
