use image::io::Reader as ImageReader;
use image::{imageops::FilterType, DynamicImage, GenericImage, GenericImageView, Rgba};
use log;
use notify::{
    event::{EventKind, ModifyKind},
    Config, Event, RecommendedWatcher, RecursiveMode, Watcher,
};
use once_cell::sync::Lazy;
use simplelog::{CombinedLogger, LevelFilter, WriteLogger};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::channel,
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

static SHUTDOWN: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));
static RECENTLY_PROCESSED: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

trait ServiceStatusExt {
    fn running() -> Self;
    fn stopped() -> Self;
}

impl ServiceStatusExt for ServiceStatus {
    fn running() -> Self {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(0),
            process_id: None,
        }
    }

    fn stopped() -> Self {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(0),
            process_id: None,
        }
    }
}

fn main() -> windows_service::Result<()> {
    load_env();

    let console_mode = env::var("NI_CONSOLE").is_ok() || env::args().any(|arg| arg == "--console");

    if console_mode {
        if let Err(error) = run_service() {
            eprintln!("Service encountered a critical error: {error}");
        }
        Ok(())
    } else {
        log::info!("Starting service in non-console mode.");
        service_dispatcher::start("ni-watcher", ffi_service_main)
    }
}

extern "system" fn ffi_service_main(_argc: u32, _argv: *mut *mut u16) {
    let _ = run_service();
}

fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    let status_handle = service_control_handler::register("ni-watcher", |control| match control {
        ServiceControl::Stop => {
            log::info!("Received Stop command. Shutting down...");
            SHUTDOWN.store(true, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        _ => {
            log::warn!("Received unsupported service control command.");
            ServiceControlHandlerResult::NotImplemented
        }
    })
    .ok();

    let exe_dir = current_exe_dir();
    let watch_dir = PathBuf::from(env::var("WATCH_FOLDER").unwrap_or_else(|_| {
        log::warn!("WATCH_FOLDER environment variable not set. Using default directory.");
        exe_dir.join("ni_watch").to_string_lossy().into_owned()
    }));
    fs::create_dir_all(&watch_dir).map_err(|e| {
        log::error!("Failed to create watch directory {:?}: {}", watch_dir, e);
        e
    })?;

    let log_dir = exe_dir.join("logs");
    fs::create_dir_all(&log_dir).map_err(|e| {
        log::error!("Failed to create log directory {:?}: {}", log_dir, e);
        e
    })?;

    CombinedLogger::init(vec![WriteLogger::new(
        LevelFilter::Info,
        simplelog::Config::default(),
        RollingFileLogger::new(&log_dir, 5 * 1024 * 1024, 3),
    )])?;
    log::info!("Service initialized. Watching folder: {:?}", watch_dir);

    if let Some(handle) = &status_handle {
        handle.set_service_status(ServiceStatus::running())?;
        log::info!("Service status set to Running.");
    }

    let (tx, rx) = channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default()).map_err(|e| {
        log::error!("Failed to initialize file watcher: {}", e);
        e
    })?;
    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;

    while !SHUTDOWN.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                handle_file_event(event, Duration::from_secs(2));
            }
            Ok(Err(e)) => log::warn!("Error receiving file event: {}", e),
            Err(_) => {}
        }
    }

    if let Some(handle) = &status_handle {
        handle.set_service_status(ServiceStatus::stopped())?;
        log::info!("Service status set to Stopped.");
    }
    log::info!("Service has stopped.");
    Ok(())
}

fn handle_file_event(event: Event, debounce_duration: Duration) {
    static PENDING_FILES: Lazy<Mutex<HashMap<PathBuf, Instant>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    match event.kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Any) => {
            let paths = event.paths;
            let now = Instant::now();

            for path in paths {
                if should_ignore(&path) || !is_image_file(&path) {
                    continue;
                }

                let mut pending_files = PENDING_FILES.lock().unwrap();
                pending_files.insert(path.clone(), now);

                let debounce_duration = debounce_duration.clone();
                let path_clone = path.clone();
                thread::spawn(move || {
                    thread::sleep(debounce_duration);
                    let mut pending_files = PENDING_FILES.lock().unwrap();

                    if let Some(&last_event_time) = pending_files.get(&path_clone) {
                        if now == last_event_time {
                            pending_files.remove(&path_clone);
                            if let Err(err) = process_and_save(&path_clone, (800, 800), 50, 10) {
                                log::error!("Error processing file {:?}: {}", path_clone, err);
                            } else {
                                log::info!("File processed successfully: {:?}", path_clone);
                            }
                        }
                    }
                });
            }
        }
        _ => {
            log::debug!("Ignoring unrelated event: {:?}", event.kind);
        }
    }
}

fn load_env() {
    let env_file = current_exe_dir().join(".env");
    match dotenvy::from_path(&env_file) {
        Ok(_) => println!(".env loaded from {:?}", env_file),
        Err(e) => println!("Warning: failed to load .env from {:?}: {}", env_file, e),
    }
}

fn current_exe_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .expect("Unable to determine the executable directory")
}

fn is_image_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        matches!(
            ext.to_string_lossy().to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tiff" | "webp"
        )
    } else {
        false
    }
}

fn should_ignore(path: &Path) -> bool {
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        if file_name.contains("_tmp") {
            log::info!("Ignoring temporary file: {:?}", path);
            return true;
        }
        if file_name.contains(".normalized.") {
            log::info!("Ignoring processed file: {:?}", path);
            return true;
        }
    }

    let mut recently_processed = RECENTLY_PROCESSED.lock().unwrap();
    let path_str = path.to_string_lossy().to_string();

    if recently_processed.contains(&path_str) {
        log::info!("Ignoring recently processed file: {:?}", path);
        return true;
    }

    recently_processed.insert(path_str.clone());
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        let mut recently_processed = RECENTLY_PROCESSED.lock().unwrap();
        recently_processed.remove(&path_str);
    });

    false
}

fn process_and_save(path: &PathBuf, size: (u32, u32), pad: u32, tol: u8) -> Result<(), String> {
    if !path.exists() {
        log::error!("File not found: {:?}", path);
        return Err(format!("File not found: {:?}", path));
    }

    let output_ext_lc = env::var("OUTPUT_FORMAT")
        .unwrap_or_else(|_| "jpg".to_string())
        .to_lowercase();

    let format = match output_ext_lc.as_str() {
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        "png" => image::ImageFormat::Png,
        "gif" => image::ImageFormat::Gif,
        "bmp" => image::ImageFormat::Bmp,
        "tiff" => image::ImageFormat::Tiff,
        "webp" => image::ImageFormat::WebP,
        other => {
            log::error!("Unsupported output format: {}", other);
            return Err(format!("Unsupported output format: {}", other));
        }
    };

    let stem = path
        .file_stem()
        .ok_or_else(|| {
            log::error!("Missing filename stem in {:?}", path);
            format!("Missing filename stem in {:?}", path)
        })?
        .to_str()
        .ok_or_else(|| {
            log::error!("Non-UTF8 filename in {:?}", path);
            format!("Non-UTF8 filename in {:?}", path)
        })?;

    log::info!("Processing file: {:?}", path);

    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY_MS: u64 = 200;
    let mut retries = 0;

    let img = loop {
        match ImageReader::open(path) {
            Ok(reader) => match reader.decode() {
                Ok(img) => break img,
                Err(e) if retries < MAX_RETRIES => {
                    retries += 1;
                    log::warn!(
                        "Failed to decode image {:?} on attempt {}: {}. Retrying...",
                        path,
                        retries,
                        e
                    );
                    thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
                }
                Err(e) => {
                    log::error!(
                        "Failed to decode image {:?} after {} attempts: {}",
                        path,
                        retries,
                        e
                    );
                    return Err(format!("Failed to decode image {:?}: {}", path, e));
                }
            },
            Err(e) if retries < MAX_RETRIES => {
                retries += 1;
                log::warn!(
                    "Failed to open image {:?} on attempt {}: {}. Retrying...",
                    path,
                    retries,
                    e
                );
                thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
            }
            Err(e) => {
                log::error!(
                    "Failed to open image {:?} after {} attempts: {}",
                    path,
                    retries,
                    e
                );
                return Err(format!("Failed to open image {:?}: {}", path, e));
            }
        }
    };

    let processed_image = process_image(img, size, pad, tol);
    log::info!("Image processed successfully: {:?}", path);

    let tmp_filename = format!("{}.normalized.{}", stem, &output_ext_lc);
    let tmp_path = path.with_file_name(&tmp_filename);

    let tmp_file = fs::File::create(&tmp_path)
        .map_err(|e| format!("Failed to create temp file {:?}: {}", tmp_path, e))?;

    if let Err(e) = processed_image.write_to(&mut std::io::BufWriter::new(tmp_file), format) {
        log::error!("Failed to write image in {:?} format: {}", format, e);
        return Err(format!("Failed to write image to {:?}: {}", tmp_path, e));
    }

    log::info!("Temporary processed image saved: {:?}", tmp_path);

    let final_filename = format!("{}.{}", stem, &output_ext_lc);
    let final_path = path.with_file_name(&final_filename);

    fs::rename(&tmp_path, &final_path)
        .map_err(|e| format!("Failed to rename to {:?}: {}", final_path, e))?;

    log::info!("Final processed image saved: {:?}", final_path);

    if path.as_path() != final_path && path.exists() {
        log::info!(
            "Removing original file {:?} due to output being saved separately as {:?}",
            path,
            final_path
        );
        fs::remove_file(path)
            .map_err(|e| format!("Failed to remove original file {:?}: {}", path, e))?;
    } else {
        log::info!(
            "Original file {:?} has been replaced by processed file {:?}",
            path,
            final_path
        );
    }

    log::info!("Processing complete for {:?}", path);
    Ok(())
}

fn process_image(img: DynamicImage, size: (u32, u32), pad: u32, tol: u8) -> DynamicImage {
    let gray = img.to_luma8();
    let (l, t, r, b) = bounding_box(&gray, tol);
    let cropped = img.crop_imm(l, t, r - l, b - t);

    let target_size = (size.0 - 2 * pad, size.1 - 2 * pad);
    let (w, h) = cropped.dimensions();
    let (new_width, new_height) = if w > h {
        let scale = target_size.0 as f32 / w as f32;
        (target_size.0, (h as f32 * scale) as u32)
    } else {
        let scale = target_size.1 as f32 / h as f32;
        ((w as f32 * scale) as u32, target_size.1)
    };

    let resized = cropped.resize_exact(new_width, new_height, FilterType::Gaussian);

    let mut canvas = DynamicImage::new_rgb8(size.0, size.1);
    for x in 0..size.0 {
        for y in 0..size.1 {
            canvas.put_pixel(x, y, Rgba([255, 255, 255, 255]));
        }
    }

    let offset_x = (size.0 - new_width) / 2;
    let offset_y = (size.1 - new_height) / 2;

    canvas.copy_from(&resized, offset_x, offset_y).unwrap();
    canvas
}

fn bounding_box(img: &image::GrayImage, tol: u8) -> (u32, u32, u32, u32) {
    let (width, height) = img.dimensions();
    let threshold = 255 - tol;
    let (mut left, mut right, mut top, mut bottom) = (width, 0, height, 0);

    for y in 0..height {
        for x in 0..width {
            if img.get_pixel(x, y)[0] < threshold {
                left = left.min(x);
                right = right.max(x);
                top = top.min(y);
                bottom = bottom.max(y);
            }
        }
    }
    (
        left.min(width - 1),
        top.min(height - 1),
        (right + 1).min(width),
        (bottom + 1).min(height),
    )
}

struct RollingFileLogger;

impl RollingFileLogger {
    fn new(base: &PathBuf, max_size: usize, max_files: usize) -> std::fs::File {
        let current_log = base.join("log0.txt");
        if current_log.exists() {
            if let Ok(metadata) = fs::metadata(&current_log) {
                if metadata.len() as usize >= max_size {
                    Self::rotate(base, max_files);
                }
            }
        }
        Self::cleanup(base, max_files);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current_log)
            .expect("Failed to open log file")
    }

    fn rotate(base: &PathBuf, max_files: usize) {
        for i in (0..max_files).rev() {
            let src = base.join(format!("log{i}.txt"));
            let dst = base.join(format!("log{}.txt", i + 1));
            if src.exists() {
                let _ = fs::rename(src, dst);
            }
        }
    }

    fn cleanup(base: &PathBuf, max_files: usize) {
        let oldest_log = base.join(format!("log{}.txt", max_files));
        let _ = fs::remove_file(oldest_log);
    }
}
