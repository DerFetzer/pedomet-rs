use chrono::{Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use egui::{Align2, Button, Direction, Frame, Margin, ScrollArea, Slider, TopBottomPanel, Vec2};
use egui_extras::DatePickerButton;
use egui_plot::{uniform_grid_spacer, Bar, BarChart, HLine, Legend, Plot};
use egui_toast::{ToastKind, Toasts};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::{cmp::min, sync::OnceLock};
use strum::{EnumIter, IntoEnumIterator};
use tokio::sync::{mpsc, oneshot};

use crate::{
    ble::{PedometerDeviceHandlerCommand, BLE_CMD_TX},
    persistence::{
        PedometerDatabaseCommand, PedometerDatabaseGetEventsInTimeRangeReceiver,
        PedometerPersistenceEvent, DB_CMD_TX,
    },
};

pub static GUI_EVENT_TX: OnceLock<mpsc::Sender<PedometerGuiEvent>> = OnceLock::new();

pub(crate) struct PedometerApp {
    state: PedometerAppState,
    db_events_rx: MessageReceiver<PedometerDatabaseGetEventsInTimeRangeReceiver>,
    connect_events_rx: MessageReceiver<anyhow::Result<()>>,
    gui_events_rx: mpsc::Receiver<PedometerGuiEvent>,
    event_id: u32,
    request_repaint_db: bool,
    request_repaint_ble: bool,
    connected: bool,
    soc: Option<u8>,
}

impl PedometerApp {
    pub(crate) fn new(
        cc: &eframe::CreationContext<'_>,
        gui_events_rx: mpsc::Receiver<PedometerGuiEvent>,
    ) -> Self {
        let state = if let Some(storage) = cc.storage {
            info!("Get state from storage");
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Default::default()
        };
        info!("Current state: {:?}", state);
        let mut app = Self {
            state,
            db_events_rx: Default::default(),
            connect_events_rx: Default::default(),
            gui_events_rx,
            event_id: 0,
            request_repaint_db: false,
            request_repaint_ble: false,
            connected: false,
            soc: None,
        };
        app.get_db_events();
        app
    }
}

impl eframe::App for PedometerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut toasts = Toasts::new()
            .anchor(Align2::LEFT_TOP, (10.0, 10.0))
            .direction(Direction::TopDown);

        ctx.set_zoom_factor(1.0);
        ctx.style_mut(|style| {
            style.spacing.slider_width = 140.0;
            style.spacing.button_padding = Vec2::new(12.0, 4.0);
        });

        self.recv_events();

        if self.db_events_rx.try_recv(Some(
            |events: anyhow::Result<Vec<PedometerPersistenceEvent>>| {
                events.map(transform_events_to_relative_steps)
            },
        )) {
            self.request_repaint_db = false;
            if let Some(Err(e)) = &self.db_events_rx.current {
                toasts.add(egui_toast::Toast {
                    kind: ToastKind::Error,
                    text: format!("Es ist ein Fehler aufgetreten:\n{}", e).into(),
                    ..Default::default()
                });
            }
        }

        if self
            .connect_events_rx
            .try_recv(None::<fn(anyhow::Result<()>) -> anyhow::Result<()>>)
        {
            self.request_repaint_ble = false;
            if let Some(Err(e)) = &self.connect_events_rx.current {
                toasts.add(egui_toast::Toast {
                    kind: ToastKind::Error,
                    text: format!("Es ist ein Fehler aufgetreten:\n{}", e).into(),
                    ..Default::default()
                });
            } else {
                if self.connected {
                    self.soc = None;
                }
                self.connected = !self.connected;
            }
        }

        self.draw_header(ctx);
        self.draw_footer(ctx);
        self.draw_main_view(ctx);

        toasts.show(ctx);

        if self.request_repaint_db || self.request_repaint_ble {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs(5));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        info!("Save state to storage: {:?}", self.state);
        eframe::set_value(storage, eframe::APP_KEY, &self.state);
    }

    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(10)
    }
}

fn transform_events_to_relative_steps(
    mut events: Vec<PedometerPersistenceEvent>,
) -> Vec<PedometerPersistenceEvent> {
    if events.is_empty() {
        return events;
    }
    let first_steps = events.first().unwrap().steps;
    let first_boot_id = events.first().unwrap().boot_id;
    debug!("Db events: {events:?}");
    events = events
        .into_iter()
        .scan(
            (first_steps, first_boot_id),
            |(last_steps, last_boot_id), mut event| {
                let event_steps = event.steps as u16;
                if *last_boot_id == event.boot_id {
                    event.steps = (event_steps).overflowing_sub(*last_steps as u16).0 as i64;
                }
                *last_steps = event_steps as i64;
                *last_boot_id = event.boot_id;
                Some(event)
            },
        )
        .collect();
    debug!("Mapped events: {events:?}");
    events
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
        TopBottomPanel::top("top_panel")
            .frame(Frame {
                inner_margin: Margin::symmetric(8.0, 12.0),
                ..Frame::side_top_panel(&ctx.style())
            })
            .show(ctx, |ui| {
                ui.heading("pedomet-rs");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "Schrittzähler {}",
                        if self.connected {
                            "verbunden"
                        } else {
                            "getrennt"
                        }
                    ));
                    if let Some(soc) = self.soc {
                        ui.label(format!("🔋{}%", soc));
                    }
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.request_repaint_ble,
                            Button::new(if self.connected {
                                "Trennen..."
                            } else {
                                "Verbinden..."
                            }),
                        )
                        .clicked()
                    {
                        let (resp_tx, resp_rx) = oneshot::channel();
                        self.connect_events_rx.receiver = Some(resp_rx);
                        let event = if !self.connected {
                            PedometerDeviceHandlerCommand::TryConnect { responder: resp_tx }
                        } else {
                            PedometerDeviceHandlerCommand::Disconnect { responder: resp_tx }
                        };
                        BLE_CMD_TX.get().unwrap().blocking_send(event).unwrap();
                        self.request_repaint_ble = true;
                    }
                });
                ui.add_space(12.0);
                if ui
                    .add_enabled(self.connected, Button::new("Schritte abrufen"))
                    .clicked()
                {
                    let (resp_tx, _resp_rx) = oneshot::channel();
                    BLE_CMD_TX
                        .get()
                        .unwrap()
                        .blocking_send(PedometerDeviceHandlerCommand::RequestEvents {
                            min_event_id: None,
                            responder: resp_tx,
                        })
                        .unwrap();
                }
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
            self.get_db_events();
        }
        ui.separator();
        ui.heading("Tag");
        if let Some(Ok(events)) = &self.db_events_rx.current {
            let mut bars: Vec<_> = (0..24)
                .map(|h| Bar::new(h as f64, 0.0).width(1.0))
                .collect();
            let mut steps_day = 0;
            for event in events.iter().filter(|e| {
                let event_dt = e.get_date_time_local().unwrap();
                self.state.selected_date == event_dt.naive_local().into()
            }) {
                let event_dt = event.get_date_time_local().unwrap();
                bars.get_mut(event_dt.hour() as usize).unwrap().value += event.steps as f64;
                steps_day += event.steps;
            }
            ui.label(format!("Schritte gesamt: {steps_day}"));
            Plot::new("day_plot")
                .height(200.0)
                .include_y(0)
                .allow_zoom(false)
                .allow_drag(false)
                .allow_scroll(false)
                .clamp_grid(true)
                .x_grid_spacer(uniform_grid_spacer(|_| [6., 3., 1.]))
                .y_axis_min_width(40.)
                .set_margin_fraction((0.01, 0.1).into())
                .reset()
                .show(ui, |plot_ui| {
                    plot_ui.bar_chart(BarChart::new(bars));
                });
        }
        ui.separator();
        ui.heading("Woche");
        if let Some(Ok(events)) = &self.db_events_rx.current {
            let mut bars: Vec<_> = (0..7)
                .map(|i| {
                    let day = self.state.selected_date - Duration::days(i);
                    Bar::new(-i as f64, 0.0)
                        .name(day.format("%a %d.%m"))
                        .width(1.0)
                })
                .collect();
            let mut steps_week = 0;
            for event in events.iter().filter(|e| {
                let event_dt = e.get_date_time_local().unwrap();
                let local = event_dt.naive_local();

                let selected_dt: NaiveDateTime = self.state.selected_date.into();

                local > selected_dt - Duration::days(6) && local <= selected_dt + Duration::days(1)
            }) {
                let event_dt = event.get_date_time_local().unwrap();
                let naive_event_dt = event_dt.naive_local();
                bars.get_mut(
                    (self.state.selected_date - naive_event_dt.date()).num_days() as usize,
                )
                .unwrap()
                .value += event.steps as f64;
                steps_week += event.steps;
            }
            ui.label(format!("Schritte gesamt: {steps_week}"));
            Plot::new("week_plot")
                .height(200.0)
                .include_y(0)
                .allow_zoom(false)
                .allow_drag(false)
                .allow_scroll(false)
                .show_grid([false, true])
                .x_axis_formatter(|mark, _range| {
                    let day = self.state.selected_date + Duration::days(mark.value as i64);
                    day.format("%d.%m\n%a").to_string()
                })
                .x_grid_spacer(uniform_grid_spacer(|_| [2., 2., 1.]))
                .y_axis_min_width(40.)
                .clamp_grid(true)
                .set_margin_fraction((0.01, 0.1).into())
                .legend(Legend::default())
                .reset()
                .show(ui, |plot_ui| {
                    plot_ui.hline(
                        HLine::new(self.state.daily_target)
                            .name("Schrittziel")
                            .highlight(true),
                    );
                    plot_ui.bar_chart(BarChart::new(bars));
                });
        }
    }

    fn draw_main_view_settings(&mut self, ui: &mut egui::Ui) {
        ui.add(
            Slider::new(&mut self.state.daily_target, 1000..=20000)
                .step_by(1000.0)
                .text("Tägliches Schrittziel"),
        );
    }

    fn draw_main_view_debug(&mut self, ui: &mut egui::Ui) {
        ui.add(egui::DragValue::new(&mut self.event_id));
        if ui.button("Events aus DB holen").clicked() {
            self.get_db_events();
        };
        if let Some(events) = &self.db_events_rx.current {
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
        TopBottomPanel::bottom("bottom_panel")
            .frame(Frame {
                inner_margin: Margin::symmetric(8.0, 12.0),
                ..Frame::side_top_panel(&ctx.style())
            })
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    for view in MainView::iter() {
                        ui.selectable_value(&mut self.state.main_view, view, view.to_string());
                    }
                });
            });
    }

    fn get_db_events(&mut self) {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.db_events_rx.receiver = Some(resp_rx);
        DB_CMD_TX
            .get()
            .unwrap()
            .blocking_send(PedometerDatabaseCommand::GetEventsInTimeRange {
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
            })
            .unwrap();
        self.request_repaint_db = true;
    }

    fn recv_events(&mut self) {
        while let Ok(event) = self.gui_events_rx.try_recv() {
            info!("Received gui event: {:?}", event);
            match event {
                PedometerGuiEvent::Soc(soc) => self.soc = Some(soc),
                PedometerGuiEvent::Disconnected => {
                    self.soc = None;
                    self.connected = false;
                }
                PedometerGuiEvent::NewEvents => self.get_db_events(),
            }
        }
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
    fn try_recv<F: Fn(T) -> T>(&mut self, data_modifier: Option<F>) -> bool {
        if let Some(receiver) = &mut self.receiver {
            if let Ok(data) = receiver.try_recv() {
                if let Some(data_modifier) = data_modifier {
                    self.current = Some(data_modifier(data));
                } else {
                    self.current = Some(data);
                }
                self.receiver = None;
                return true;
            }
        }
        false
    }
}

#[derive(Debug)]
pub(crate) enum PedometerGuiEvent {
    Soc(u8),
    Disconnected,
    NewEvents,
}
