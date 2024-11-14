use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use egui::{DragValue, ScrollArea, TopBottomPanel};
use egui_extras::DatePickerButton;
use egui_plot::{Bar, BarChart, HLine, Plot};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::cmp::min;
use strum::{EnumIter, IntoEnumIterator};
use tokio::sync::oneshot;

use crate::{
    ble::{PedometerDeviceHandlerCommand, BLE_CMD_TX},
    persistence::{
        PedometerDatabaseCommand, PedometerDatabaseGetEventsInTimeRangeReceiver, DB_CMD_TX,
    },
};

pub(crate) struct PedometerApp {
    state: PedometerAppState,
    events_rx: MessageReceiver<PedometerDatabaseGetEventsInTimeRangeReceiver>,
    event_id: u32,
}

impl PedometerApp {
    pub(crate) fn new() -> Self {
        Self {
            state: Default::default(),
            events_rx: Default::default(),
            event_id: 0,
        }
    }
}

impl eframe::App for PedometerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_zoom_factor(1.5);
        self.events_rx.try_recv();
        self.draw_header(ctx);
        self.draw_footer(ctx);
        self.draw_main_view(ctx);
    }
}

#[derive(
    Debug, Copy, Clone, Default, PartialEq, EnumIter, strum::Display, Serialize, Deserialize,
)]
enum MainView {
    #[default]
    #[strum(to_string = "Übersicht")]
    Overview,
    #[strum(to_string = "Einstellungen")]
    Settings,
    #[strum(to_string = "Debug")]
    Debug,
}

impl PedometerApp {
    fn draw_header(&mut self, ctx: &egui::Context) {
        TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.heading("pedomet-rs");
        });
    }

    fn draw_main_view(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                match self.state.main_view {
                    MainView::Overview => self.draw_main_view_overview(ui),
                    MainView::Settings => self.draw_main_view_settings(ui),
                    MainView::Debug => self.draw_main_view_debug(ui),
                };
            });
        });
    }

    fn draw_main_view_overview(&mut self, ui: &mut egui::Ui) {
        let date_before = self.state.selected_date;
        ui.horizontal(|ui| {
            if ui.button("<").clicked() {
                self.state.selected_date -= chrono::Duration::days(1);
            }
            ui.add(DatePickerButton::new(&mut self.state.selected_date).calendar_week(false));
            if ui.button(">").clicked() {
                self.state.selected_date += chrono::Duration::days(1);
            }
            if ui.button("Heute").clicked() {
                self.state.selected_date = Local::now().date_naive();
            }
            self.state.selected_date = min(self.state.selected_date, Local::now().date_naive());
        });
        if date_before != self.state.selected_date {
            debug!("Selected date changed to: {:?}", self.state.selected_date);
            let (resp_tx, resp_rx) = oneshot::channel();
            self.events_rx.receiver = Some(resp_rx);
            let _ = DB_CMD_TX.get().unwrap().blocking_send(
                PedometerDatabaseCommand::GetEventsInTimeRange {
                    start: (self.state.selected_date - Duration::days(6))
                        .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
                        .and_local_timezone(Local)
                        .unwrap()
                        .to_utc(),
                    end: (self.state.selected_date + Duration::days(1))
                        .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
                        .and_local_timezone(Local)
                        .unwrap()
                        .to_utc(),
                    responder: resp_tx,
                },
            );
        }
        ui.separator();
        ui.heading("Tag");
        if let Some(Ok(events)) = &self.events_rx.current {
            let mut bars: Vec<_> = (0..24)
                .map(|h| Bar::new(h as f64, 0.0).width(1.0))
                .collect();
            for event in events.iter().filter(|e| {
                let event_dt = e.get_date_time().unwrap();
                self.state.selected_date == event_dt.naive_local().into()
            }) {
                let event_dt = event.get_date_time().unwrap().naive_local();
                bars.get_mut(event_dt.hour() as usize).unwrap().value += event.steps as f64;
            }
            Plot::new("day_plot")
                .height(300.0)
                .include_y(0)
                .allow_zoom(false)
                .allow_drag(false)
                .allow_scroll(false)
                .reset()
                .show(ui, |plot_ui| {
                    plot_ui.bar_chart(BarChart::new(bars));
                });
        }
        ui.separator();
        ui.heading("Woche");
        if let Some(Ok(events)) = &self.events_rx.current {
            let mut bars: Vec<_> = (0..7)
                .map(|i| {
                    let day = self.state.selected_date - Duration::days(i);
                    Bar::new(-i as f64, 0.0)
                        .name(day.format("%a %d.%m"))
                        .width(1.0)
                })
                .collect();
            for event in events.iter().filter(|e| {
                let event_dt = e.get_date_time().unwrap();
                let local = event_dt.naive_local();

                let selected_dt: NaiveDateTime = self.state.selected_date.into();

                local > selected_dt - Duration::days(7) && local <= selected_dt
            }) {
                let event_dt = event.get_date_time().unwrap().naive_local();
                bars.get_mut((self.state.selected_date - event_dt.date()).num_days() as usize)
                    .unwrap()
                    .value += event.steps as f64;
            }
            Plot::new("week_plot")
                .height(300.0)
                .include_y(0)
                .allow_zoom(false)
                .allow_drag(false)
                .allow_scroll(false)
                .reset()
                .show(ui, |plot_ui| {
                    plot_ui.hline(HLine::new(self.state.daily_target).name("Schrittziel"));
                    plot_ui.bar_chart(BarChart::new(bars));
                });
        }
    }

    fn draw_main_view_settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Tägliches Schrittziel");
            ui.add(DragValue::new(&mut self.state.daily_target).range(0..=50_000));
        });
    }

    fn draw_main_view_debug(&mut self, ui: &mut egui::Ui) {
        ui.add(egui::DragValue::new(&mut self.event_id));
        if ui.button("Events aus DB holen").clicked() {
            let (resp_tx, resp_rx) = oneshot::channel();
            self.events_rx.receiver = Some(resp_rx);
            let _ = DB_CMD_TX.get().unwrap().blocking_send(
                PedometerDatabaseCommand::GetEventsInTimeRange {
                    start: DateTime::UNIX_EPOCH,
                    end: Utc::now(),
                    responder: resp_tx,
                },
            );
        };
        if ui.button("Try connect...").clicked() {
            let (resp_tx, _resp_rx) = oneshot::channel();
            let _ = BLE_CMD_TX
                .get()
                .unwrap()
                .blocking_send(PedometerDeviceHandlerCommand::TryConnect { responder: resp_tx });
        };
        if ui.button("Request events...").clicked() {
            let (resp_tx, _resp_rx) = oneshot::channel();
            let _ = BLE_CMD_TX.get().unwrap().blocking_send(
                PedometerDeviceHandlerCommand::RequestEvents {
                    min_event_id: Some(self.event_id),
                    responder: resp_tx,
                },
            );
        };
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
    }

    fn draw_footer(&mut self, ctx: &egui::Context) {
        TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                for view in MainView::iter() {
                    ui.selectable_value(&mut self.state.main_view, view, view.to_string());
                }
            });
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PedometerAppState {
    main_view: MainView,
    selected_date: NaiveDate,
    daily_target: u32,
}

impl Default for PedometerAppState {
    fn default() -> Self {
        let now = Local::now();
        Self {
            main_view: Default::default(),
            selected_date: now.date_naive(),
            daily_target: 10_000,
        }
    }
}

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
