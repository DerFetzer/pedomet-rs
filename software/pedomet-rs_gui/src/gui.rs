use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use egui::{ScrollArea, Slider, TopBottomPanel, Vec2};
use egui_extras::DatePickerButton;
use egui_plot::{uniform_grid_spacer, Bar, BarChart, HLine, Plot};
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
    request_repaint: bool,
}

impl PedometerApp {
    pub(crate) fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let state = if let Some(storage) = cc.storage {
            info!("Get state from storage");
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Default::default()
        };
        info!("Current state: {:?}", state);
        let mut app = Self {
            state,
            events_rx: Default::default(),
            event_id: 0,
            request_repaint: false,
        };
        app.get_events();
        app
    }
}

impl eframe::App for PedometerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_zoom_factor(1.4);
        ctx.style_mut(|style| {
            style.spacing.slider_width = 140.0;
            style.spacing.button_padding = Vec2::new(12.0, 4.0);
        });

        if self.events_rx.try_recv() {
            self.request_repaint = false;
        }

        self.draw_header(ctx);
        self.draw_footer(ctx);
        self.draw_main_view(ctx);

        if self.request_repaint {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
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
            self.get_events();
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
                .height(200.0)
                .include_y(0)
                .allow_zoom(false)
                .allow_drag(false)
                .allow_scroll(false)
                .clamp_grid(true)
                .x_grid_spacer(uniform_grid_spacer(|_| [6., 3., 1.]))
                .y_axis_min_width(40.)
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

                local > selected_dt - Duration::days(6) && local <= selected_dt + Duration::days(1)
            }) {
                let event_dt = event.get_date_time().unwrap().naive_local();
                bars.get_mut((self.state.selected_date - event_dt.date()).num_days() as usize)
                    .unwrap()
                    .value += event.steps as f64;
            }
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
                .reset()
                .show(ui, |plot_ui| {
                    plot_ui.hline(HLine::new(self.state.daily_target).name("Schrittziel"));
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
            self.get_events();
        };
        if ui.button("Try connect...").clicked() {
            let (resp_tx, _resp_rx) = oneshot::channel();
            BLE_CMD_TX
                .get()
                .unwrap()
                .blocking_send(PedometerDeviceHandlerCommand::TryConnect { responder: resp_tx })
                .unwrap();
        };
        if ui.button("Request events...").clicked() {
            let (resp_tx, _resp_rx) = oneshot::channel();
            BLE_CMD_TX
                .get()
                .unwrap()
                .blocking_send(PedometerDeviceHandlerCommand::RequestEvents {
                    min_event_id: Some(self.event_id),
                    responder: resp_tx,
                })
                .unwrap();
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
        TopBottomPanel::bottom("bottom_panel")
            .exact_height(50.)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    for view in MainView::iter() {
                        ui.selectable_value(&mut self.state.main_view, view, view.to_string());
                    }
                });
            });
    }

    fn get_events(&mut self) {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.events_rx.receiver = Some(resp_rx);
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
        self.request_repaint = true;
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
