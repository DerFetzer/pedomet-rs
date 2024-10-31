#[cfg(target_os = "android")]
mod android;
mod runtime;

use btleplug::api::{Central, Characteristic, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use eframe::egui;
use eframe::{NativeOptions, Renderer};
use futures::stream::StreamExt;
use log::{debug, error, info};
use std::error::Error;
use std::time::Duration;
use tokio::sync::watch::{Receiver, Sender};
use uuid::Uuid;
#[cfg(target_os = "android")]
use winit::platform::android::activity::AndroidApp;

/// Only devices whose name contains this string will be tried.
const PERIPHERAL_NAME_MATCH_FILTER: &str = "pedomet-rs";
/// UUID of the characteristic for which we should subscribe to notifications.
const SOC_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x00002A1900001000800000805F9B34FB);

struct DemoApp {
    rx: Receiver<u32>,
}

impl eframe::App for DemoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let num = self.rx.borrow().to_string();
            for _ in 0..20 {
                ui.label(&num);
            }
        });
    }
}

fn tokio_thread(watch_sender: Sender<u32>) {
    debug!("tokio_thread");
    runtime::create_runtime_and_block(async {
        debug!("inside future");
        if let Err(e) = ble(watch_sender).await {
            error!("{}", e);
        }
    });
}

async fn ble(watch_sender: Sender<u32>) -> Result<(), Box<dyn Error>> {
    let manager = Manager::new().await?;
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        error!("Could not find any adapters");
    }
    let adapter = adapter_list.first().unwrap();

    info!("Starting scan on {}...", adapter.adapter_info().await?);

    adapter
        .start_scan(ScanFilter::default())
        .await
        .expect("Can't scan BLE adapter for connected devices...");

    tokio::time::sleep(Duration::from_secs(10)).await;

    let peripherals = adapter.peripherals().await?;
    if peripherals.is_empty() {
        error!("->>> BLE peripheral devices were not found, sorry. Exiting...");
    } else if let Some(device) = find_device(adapter).await {
        info!("Found device: {:?}", device);
        if !device.is_connected().await? {
            device.connect().await?;
        }
        device.discover_services().await?;
        if let Some(soc_char) = find_bas_characteristic(&device).await {
            info!("Found characteristic: {:?}", soc_char);
            device.subscribe(&soc_char).await?;
            let mut notification_stream = device.notifications().await?;
            while let Some(data) = notification_stream.next().await {
                if data.uuid == SOC_CHARACTERISTIC_UUID {
                    watch_sender.send(data.value[0].into())?;
                }
            }
        }
    } else {
        error!("Could not find device");
    }
    Ok(())
}

async fn find_device(central: &Adapter) -> Option<Peripheral> {
    for p in central.peripherals().await.unwrap() {
        if p.properties()
            .await
            .unwrap()
            .unwrap()
            .local_name
            .iter()
            .any(|name| name.contains(PERIPHERAL_NAME_MATCH_FILTER))
        {
            return Some(p);
        }
    }
    None
}

async fn find_bas_characteristic(peripheral: &Peripheral) -> Option<Characteristic> {
    for c in peripheral.characteristics() {
        debug!("Characteristic: {:?}", c);
        if c.uuid == SOC_CHARACTERISTIC_UUID {
            return Some(c);
        }
    }
    None
}

fn _main(mut options: NativeOptions) -> eframe::Result<()> {
    info!("Hello pedomet-rs!");
    let (tx, rx) = tokio::sync::watch::channel(0);
    let thread_builder = std::thread::Builder::new().name("tokio".to_string());
    thread_builder
        .spawn(move || tokio_thread(tx))
        .expect("Could not spawn tokio thread");
    options.renderer = Renderer::Wgpu;
    eframe::run_native(
        "My egui App",
        options,
        Box::new(|_cc| Ok(Box::new(DemoApp { rx }))),
    )
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;

    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    let options = NativeOptions {
        event_loop_builder: Some(Box::new(move |builder| {
            builder.with_android_app(app);
        })),
        ..Default::default()
    };

    _main(options).unwrap_or_else(|err| {
        log::error!("Failure while running EFrame application: {err:?}");
    });
}

#[allow(unused)]
#[cfg(not(target_os = "android"))]
fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn) // Default Log Level
        .parse_default_env()
        .init();

    _main(NativeOptions::default()).unwrap();
}
