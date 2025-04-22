
// ni-service.rs
use notify::{Watcher, RecursiveMode, RecommendedWatcher, Config, Event};
use notify::event::{EventKind, ModifyKind};
use std::{env, fs, path::PathBuf, process::Command, sync::mpsc::channel, time::Duration};
use std::ffi::OsString;
use windows_service::{
    service::{ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType, ServiceControl},
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};
use dotenvy::dotenv;

fn main() -> windows_service::Result<()> {
    let service_name = OsString::from("ni-watcher");
    service_dispatcher::start(service_name, ffi_service_main)?;
    Ok(())
}

extern "system" fn ffi_service_main(_argc: u32, _argv: *mut *mut u16) {
    run_service();
}

fn run_service() {
    let status_handle: ServiceStatusHandle = service_control_handler::register("ni-watcher", move |control_event| {
        match control_event {
            ServiceControl::Stop => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    }).expect("Failed to register service control handler");

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(0),
        process_id: None,
    }).expect("Failed to set service status");

    dotenv().ok();

    let watch_path_str = env::var("WATCH_FOLDER").unwrap_or_else(|_| "C:/ni_watch".to_string());
    let watch_path = PathBuf::from(&watch_path_str);

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .expect("Failed to determine service executable path");
    let cli_path = exe_dir.join("ni.exe");

    println!("Watching folder: {}", watch_path_str);
    println!("Normalizer path: {:?}", cli_path);

    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher =
        Watcher::new(tx, Config::default()).expect("Failed to initialize watcher");

    watcher.watch(&watch_path, RecursiveMode::NonRecursive).expect("Failed to watch folder");

    loop {
        match rx.recv() {
            Ok(Ok(Event { kind, paths, .. })) => {
                for path in paths {
                    if kind.is_create() && path.is_file() {
                        println!("New file: {:?}", path);
                        handle_new_image(&cli_path, &path);
                    } else if let EventKind::Modify(ModifyKind::Name(_)) = kind {
                        if path.is_file() {
                            println!("File renamed into: {:?}", path);
                            handle_new_image(&cli_path, &path);
                        }
                    }
                }
            }
            Ok(Err(e)) => println!("Watch error: {:?}", e),
            Err(e) => println!("Watch channel error: {:?}", e),
        }
    }
}

fn handle_new_image(cli_path: &PathBuf, path: &PathBuf) {
    let temp_output = path.with_extension("normalized.tmp");

    let status = Command::new(cli_path)
        .args(["-i", path.to_str().unwrap(), "-o", temp_output.parent().unwrap().to_str().unwrap()])
        .status();

    match status {
        Ok(code) if code.success() => {
            println!("Image normalized: {:?}", path);
            fs::rename(&temp_output, path).expect("Failed to overwrite original image");
        }
        Ok(code) => {
            println!("Normalizer exited with code: {}", code);
        }
        Err(err) => {
            println!("Failed to run normalizer: {}", err);
        }
    }
}

