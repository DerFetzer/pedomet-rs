use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use winit::event;

use crate::{
    ble::PedometerDeviceHandlerCommand,
    persistence::{PedometerDatabaseCommand, PedometerDatabaseGetEventsInTimeRangeReceiver},
};

pub(crate) struct PedometerApp {
    state: PedometerAppState,
    database_cmd_tx: mpsc::Sender<PedometerDatabaseCommand>,
    device_cmd_tx: mpsc::Sender<PedometerDeviceHandlerCommand>,
    events_rx: MessageReceiver<PedometerDatabaseGetEventsInTimeRangeReceiver>,
    event_id: u32,
}

impl PedometerApp {
    pub(crate) fn new(
        database_cmd_tx: mpsc::Sender<PedometerDatabaseCommand>,
        device_cmd_tx: mpsc::Sender<PedometerDeviceHandlerCommand>,
    ) -> Self {
        Self {
            database_cmd_tx,
            device_cmd_tx,
            state: Default::default(),
            events_rx: Default::default(),
            event_id: 0,
        }
    }
}

impl eframe::App for PedometerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add(egui::DragValue::new(&mut self.event_id));
            if ui.button("Events aus DB holen").clicked() {
                let (resp_tx, resp_rx) = oneshot::channel();
                self.events_rx.receiver = Some(resp_rx);
                let _ = self.database_cmd_tx.blocking_send(
                    PedometerDatabaseCommand::GetEventsInTimeRange {
                        start: OffsetDateTime::UNIX_EPOCH,
                        end: OffsetDateTime::now_utc(),
                        responder: resp_tx,
                    },
                );
            };
            if ui.button("Try connect...").clicked() {
                let (resp_tx, _resp_rx) = oneshot::channel();
                let _ =
                    self.device_cmd_tx
                        .blocking_send(PedometerDeviceHandlerCommand::TryConnect {
                            responder: resp_tx,
                        });
            };
            if ui.button("Request events...").clicked() {
                let (resp_tx, _resp_rx) = oneshot::channel();
                let _ = self.device_cmd_tx.blocking_send(
                    PedometerDeviceHandlerCommand::RequestEvents {
                        min_event_id: Some(self.event_id),
                        responder: resp_tx,
                    },
                );
            };
            self.events_rx.try_recv();
            if let Some(events) = &self.events_rx.current {
                if let Err(err) = events {
                    ui.label(format!("Error: {err}"));
                } else {
                    ui.label("Ok!");
                }
                if let Ok(events) = events {
                    for event in events {
                        ui.label(format!("{event:?}"));
                    }
                }
            }
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PedometerAppState {}

#[derive(Debug)]
struct MessageReceiver<T> {
    current: Option<T>,
    receiver: Option<oneshot::Receiver<T>>,
}

impl<T> Default for MessageReceiver<T> {
    fn default() -> Self {
        Self {
            current: Default::default(),
            receiver: Default::default(),
        }
    }
}

impl<T> MessageReceiver<T> {
    fn try_recv(&mut self) -> bool {
        if let Some(receiver) = &mut self.receiver {
            if let Ok(data) = receiver.try_recv() {
                self.current = Some(data);
                self.receiver = None;
                return true;
            }
        }
        false
    }
}
