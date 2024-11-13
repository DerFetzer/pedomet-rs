#[cfg(target_os = "android")]
mod android;
mod ble;
mod error;
mod gui;
mod persistence;
mod runtime;

#[cfg(target_os = "android")]
use app_dirs2::app_root;
use app_dirs2::AppInfo;
use ble::{PedometerDeviceHandler, PedometerDeviceHandlerCommand, BLE_CMD_TX};
use eframe::{NativeOptions, Renderer};
use gui::PedometerApp;
use log::{debug, error, info};
use persistence::{PedometerDatabase, PedometerDatabaseCommand, DB_CMD_TX};
use std::path::Path;
use tokio::sync::{mpsc, watch::Sender};
#[cfg(target_os = "android")]
use winit::platform::android::activity::AndroidApp;

pub const APP_INFO: AppInfo = AppInfo {
    name: "pedomet-rs",
    author: "DerFetzer",
};

fn tokio_thread(
    database_cmd_rx: mpsc::Receiver<PedometerDatabaseCommand>,
    device_cmd_rx: mpsc::Receiver<PedometerDeviceHandlerCommand>,
) {
    debug!("tokio_thread");
    runtime::create_runtime_and_block(async {
        debug!("inside future");
        let db_handle = PedometerDatabase::new()
            .await
            .unwrap()
            .spawn_message_handler(database_cmd_rx)
            .await;
        let dev_handle = PedometerDeviceHandler::new()
            .await
            .unwrap()
            .spawn_message_handler(device_cmd_rx)
            .await;

        db_handle.await.unwrap();
        dev_handle.await.unwrap();
    });
}

fn _main(mut options: NativeOptions) -> eframe::Result<()> {
    info!("Hello pedomet-rs!");

    let (database_cmd_tx, database_cmd_rx) = mpsc::channel(1000);
    let (device_cmd_tx, device_cmd_rx) = mpsc::channel(1000);
    BLE_CMD_TX.get_or_init(|| device_cmd_tx);
    DB_CMD_TX.get_or_init(|| database_cmd_tx);

    let thread_builder = std::thread::Builder::new().name("tokio".to_string());
    thread_builder
        .spawn(move || tokio_thread(database_cmd_rx, device_cmd_rx))
        .expect("Could not spawn tokio thread");

    options.renderer = Renderer::Wgpu;
    eframe::run_native(
        "My egui App",
        options,
        Box::new(|_cc| Ok(Box::new(PedometerApp::new()))),
    )
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    use std::path::PathBuf;

    use app_dirs2::{get_app_root, AppDataType};
    use winit::platform::android::EventLoopBuilderExtAndroid;

    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );

    let options = NativeOptions {
        event_loop_builder: Some(Box::new(move |builder| {
            builder.with_android_app(app);
        })),
        persistence_path: Some(app_root(AppDataType::UserConfig, &APP_INFO).unwrap()),
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
