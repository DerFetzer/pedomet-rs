use anyhow::anyhow;
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::Utc;
use futures::StreamExt;
use log::{debug, error, info, warn};
use pedomet_rs_common::{PedometerEvent, PedometerEventType};
use std::cmp::max;
use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::gui::GUI_EVENT_TX;
use crate::persistence::{PedometerDatabaseCommand, PedometerPersistenceEvent, DB_CMD_TX};

/// Only devices whose name contains this string will be tried.
const PERIPHERAL_NAME_MATCH_FILTER: &str = "pedomet-rs";

/// Characteristics
const CHARACTERISTIC_UUID_SOC: Uuid = Uuid::from_u128(0x00002A19_0000_1000_8000_00805F9B34FB);
const CHARACTERISTIC_UUID_REQUEST_EVENTS: Uuid =
    Uuid::from_u128(0x1C2A0001_ABF2_4B98_BA1C_25D5EA728525);
const CHARACTERISTIC_UUID_RESPONSE_EVENTS: Uuid =
    Uuid::from_u128(0x1C2A0002_ABF2_4B98_BA1C_25D5EA728525);
#[allow(unused)]
const CHARACTERISTIC_UUID_DELETE_EVENTS: Uuid =
    Uuid::from_u128(0x1C2A0003_ABF2_4B98_BA1C_25D5EA728525);
const CHARACTERISTIC_UUID_EPOCH_MS: Uuid = Uuid::from_u128(0x1C2A0004_ABF2_4B98_BA1C_25D5EA728525);
const CHARACTERISTIC_BOOT_ID: Uuid = Uuid::from_u128(0x1C2A0005_ABF2_4B98_BA1C_25D5EA728525);
const CHARACTERISTIC_MAX_EVENT_ID: Uuid = Uuid::from_u128(0x1C2A0006_ABF2_4B98_BA1C_25D5EA728525);

const SUB_CHARACTERISTICS: [Uuid; 4] = [
    CHARACTERISTIC_UUID_SOC,
    CHARACTERISTIC_UUID_EPOCH_MS,
    CHARACTERISTIC_UUID_RESPONSE_EVENTS,
    CHARACTERISTIC_MAX_EVENT_ID,
];

pub static BLE_CMD_TX: OnceLock<mpsc::Sender<PedometerDeviceHandlerCommand>> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct PedometerDeviceHandler {
    device: Option<Peripheral>,
}

impl PedometerDeviceHandler {
    pub(crate) async fn new() -> anyhow::Result<Self> {
        Ok(Self { device: None })
    }

    #[allow(unused_variables)]
    pub(crate) async fn spawn_message_handler(
        mut self,
        mut event_receiver: mpsc::Receiver<PedometerDeviceHandlerCommand>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = event_receiver.recv().await {
                match cmd {
                    PedometerDeviceHandlerCommand::TryConnect { responder } => {
                        let res = self.try_connect().await;
                        if let Err(e) = &res {
                            warn!("Could not connect to device: {e}");
                        }
                        let _ = responder.send(res);
                    }
                    PedometerDeviceHandlerCommand::IsConnected { responder } => {
                        let _ = responder.send(self.is_connected().await);
                    }
                    PedometerDeviceHandlerCommand::RequestEvents {
                        min_event_id,
                        responder,
                    } => {
                        let _ = responder.send(self.request_events(min_event_id).await);
                    }
                    PedometerDeviceHandlerCommand::DeleteEvents { .. } => {
                        todo!()
                    }
                    PedometerDeviceHandlerCommand::Disconnect { responder } => {
                        let _ = responder.send(self.disconnect().await);
                    }
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
            let manager = Manager::new().await?;
            let adapter_list = manager.adapters().await?;
            if adapter_list.is_empty() {
                error!("Could not find any adapters");
                return Err(anyhow!("Could not find any adapters"));
            }
            let adapter = adapter_list.first().unwrap().clone();

            info!("Starting scan on {}...", adapter.adapter_info().await?);

            adapter
                .start_scan(ScanFilter {
                    //services: vec![SERVICE_UUID_PEDOMETER],
                    services: vec![],
                })
                .await?;

            if let Ok(Ok(Some(device))) = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match find_device(&adapter).await {
                        Ok(None) => tokio::time::sleep(Duration::from_millis(200)).await,
                        res => return res,
                    }
                }
            })
            .await
            {
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

            tokio::time::sleep(Duration::from_millis(100)).await;

            for uuid in SUB_CHARACTERISTICS {
                if let Some(char) = find_characteristic(device, uuid) {
                    info!("Found characteristic: {:?}", char);
                    device.subscribe(&char).await?;
                } else {
                    warn!("Could not find characteristic: {}", uuid);
                }
            }
            self.send_host_epoch().await?;
            let boot_id = u32::from_le_bytes(
                device
                    .read(&find_characteristic(device, CHARACTERISTIC_BOOT_ID).unwrap())
                    .await?[..]
                    .try_into()?,
            );
            let max_event_id = u32::from_le_bytes(
                device
                    .read(&find_characteristic(device, CHARACTERISTIC_MAX_EVENT_ID).unwrap())
                    .await?[..]
                    .try_into()?,
            );
            let soc = device
                .read(&find_characteristic(device, CHARACTERISTIC_UUID_SOC).unwrap())
                .await?[0];
            info!("Connected: boot_id: {boot_id}, max_event_id: {max_event_id}, soc: {soc}");

            if let Err(e) = GUI_EVENT_TX
                .get()
                .unwrap()
                .send(crate::gui::PedometerGuiEvent::Soc(soc))
                .await
            {
                error!("Could not send gui soc event: {e}");
            }

            let mut notification_stream = device.notifications().await?;
            tokio::spawn(async move {
                let mut event_queue = VecDeque::new();
                let mut device_time_offsets = HashMap::new();
                let mut max_time_offset_boot_id = 0;
                while let Some(notification) = notification_stream.next().await {
                    match notification.uuid {
                        CHARACTERISTIC_UUID_RESPONSE_EVENTS => {
                            info!("Received event response");
                            Self::process_event_response(
                                notification,
                                &mut event_queue,
                                &mut device_time_offsets,
                                &mut max_time_offset_boot_id,
                            )
                            .await;
                        }
                        CHARACTERISTIC_UUID_EPOCH_MS => {
                            // Process event instead
                            info!("Received epoch characteristic: {:?}", notification.value);
                        }
                        CHARACTERISTIC_UUID_SOC => {
                            info!("Received soc characteristic: {:?}", notification.value);
                            if let Err(e) = GUI_EVENT_TX
                                .get()
                                .unwrap()
                                .send(crate::gui::PedometerGuiEvent::Soc(notification.value[0]))
                                .await
                            {
                                error!("Could not send gui soc event: {e}");
                            }
                        }
                        CHARACTERISTIC_MAX_EVENT_ID => {
                            // Todo!
                            info!(
                                "Received max_event_id characteristic: {:?}",
                                notification.value
                            );
                        }
                        char => warn!("Received unknown characteristic: {char}"),
                    }
                }
            });
            let device = device.clone();
            tokio::spawn(async move {
                while let Ok(true) = device.is_connected().await {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                if let Err(e) = GUI_EVENT_TX
                    .get()
                    .unwrap()
                    .send(crate::gui::PedometerGuiEvent::Disconnected)
                    .await
                {
                    error!("Could not send gui disconnected event: {e}");
                }
            });
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        if let Some(device) = &self.device {
            if device.is_connected().await? {
                device.disconnect().await?;
            }
        }
        Ok(())
    }

    async fn process_event_response(
        mut notification: ValueNotification,
        event_queue: &mut VecDeque<PedometerEvent>,
        device_time_offsets: &mut HashMap<u32, Duration>,
        max_time_offset_boot_id: &mut u32,
    ) {
        info!(
            "Got event response with length: {}",
            notification.value.len()
        );
        let mut buf = &mut notification.value[..];
        let mut max_event_id = 0;
        let mut received_events = false;
        while let Ok((event, rest)) = PedometerEvent::deserialize_from_transport(buf) {
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
                            Duration::from_millis(host_epoch_ms - event.timestamp_ms),
                        );
                        *max_time_offset_boot_id = max(*max_time_offset_boot_id, event.boot_id);
                    } else {
                        warn!("Got invalid host epoch event: {event:?}");
                    }
                }
                PedometerEventType::Steps(_) => event_queue.push_back(event),
                PedometerEventType::Boot => {}
            }
        }
        let mut events_retain = Vec::with_capacity(event_queue.len());
        for event in event_queue.iter() {
            if let PedometerEventType::Steps(_) = event.event_type {
                match device_time_offsets.get(&event.boot_id) {
                    None if event.boot_id < *max_time_offset_boot_id => {
                        warn!("Dropped step event because the device time offset could not be determined anymore: {event:?}");
                        events_retain.push(false);
                        continue;
                    }
                    None => {
                        info!("Wait for timestamp");
                        events_retain.push(true);
                    }
                    Some(offset) => {
                        match PedometerPersistenceEvent::from_common_event(*event, *offset) {
                            Ok(persistence_event) => {
                                let (responder_tx, responder_rx) = oneshot::channel();
                                info!("Send event to db: {persistence_event:?}");
                                if let Err(e) = DB_CMD_TX
                                    .get()
                                    .unwrap()
                                    .send(PedometerDatabaseCommand::AddEvent {
                                        event: persistence_event,
                                        responder: responder_tx,
                                    })
                                    .await
                                {
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
                                warn!("Could not convert event: {event:?} -> {e}");
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
            info!("Notify gui about new events");
            if let Err(e) = GUI_EVENT_TX
                .get()
                .unwrap()
                .send(crate::gui::PedometerGuiEvent::NewEvents)
                .await
            {
                error!("Could not send gui new_events event: {e}");
            }

            info!("Try to read more events");
            let (resp_tx, _resp_rx) = oneshot::channel();
            let _ = BLE_CMD_TX
                .get()
                .unwrap()
                .send(PedometerDeviceHandlerCommand::RequestEvents {
                    min_event_id: Some(max_event_id + 1),
                    responder: resp_tx,
                })
                .await;
        }
        debug!("Retain events: {event_queue:?} {events_retain:?}");
        let mut retain_iter = events_retain.iter();
        event_queue.retain(|_| *retain_iter.next().unwrap());
    }

    async fn is_connected(&self) -> anyhow::Result<bool> {
        Ok(match &self.device {
            Some(device) => device.is_connected().await?,
            None => false,
        })
    }

    async fn request_events(&self, min_event_id: Option<u32>) -> anyhow::Result<()> {
        match &self.device {
            Some(device) if device.is_connected().await? => {
                let min_event_id = match min_event_id {
                    Some(min_event_id) => min_event_id,
                    None => {
                        let (responder_tx, responder_rx) = oneshot::channel();
                        info!("Get last event from db");
                        DB_CMD_TX
                            .get()
                            .unwrap()
                            .send(PedometerDatabaseCommand::GetLastEvent {
                                responder: responder_tx,
                            })
                            .await?;
                        if let Some(last_db_event) = responder_rx.await?? {
                            let current_boot_id = u32::from_le_bytes(
                                device
                                    .read(
                                        &find_characteristic(device, CHARACTERISTIC_BOOT_ID)
                                            .ok_or_else(|| {
                                                anyhow!("Could not find boot_id characteristic")
                                            })?,
                                    )
                                    .await?[..]
                                    .try_into()?,
                            );
                            let current_max_event_id = u32::from_le_bytes(
                                device
                                    .read(
                                        &find_characteristic(device, CHARACTERISTIC_MAX_EVENT_ID)
                                            .ok_or_else(|| {
                                            anyhow!("Could not find boot_id characteristic")
                                        })?,
                                    )
                                    .await?[..]
                                    .try_into()?,
                            );
                            info!(
                            "last_db_event: {:?}, current_boot_id: {}, current_max_event_id: {}",
                            last_db_event, current_boot_id, current_max_event_id
                        );
                            if current_max_event_id as i64 >= last_db_event.event_id
                                && current_boot_id as i64 >= last_db_event.boot_id
                            {
                                (last_db_event.event_id + 1).try_into()?
                            } else {
                                0
                            }
                        } else {
                            0
                        }
                    }
                };
                info!("Request events from id {}", min_event_id);
                device
                    .write(
                        &find_characteristic(device, CHARACTERISTIC_UUID_REQUEST_EVENTS)
                            .ok_or_else(|| anyhow!("Could not find characteristic"))?,
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

    async fn send_host_epoch(&self) -> anyhow::Result<()> {
        if let Some(device) = &self.device {
            info!("Send current time to device...");
            let epoch_ms_char = find_characteristic(device, CHARACTERISTIC_UUID_EPOCH_MS)
                .ok_or_else(|| anyhow!("Could not find characteristic"))?;
            Ok(device
                .write(
                    &epoch_ms_char,
                    &((Utc::now().timestamp_millis()) as u64).to_le_bytes(),
                    btleplug::api::WriteType::WithResponse,
                )
                .await?)
        } else {
            Ok(())
        }
    }
}

#[allow(unused)]
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
    Disconnect {
        responder: oneshot::Sender<Result<(), anyhow::Error>>,
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
