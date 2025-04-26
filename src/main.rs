use image::{imageops::FilterType, DynamicImage, GenericImage, GenericImageView, ImageReader, Rgba};
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

/// Global flag toggled by the service-control handler to indicate shutdown.
static SHUTDOWN: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

/// Global set to track recently processed files (thread-safe).
static RECENTLY_PROCESSED: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Trait to extend `ServiceStatus` with helper methods for common service states.
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
    // Load environment variables
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

/// Entry point for the Windows Service Control Manager (SCM).
extern "system" fn ffi_service_main(_argc: u32, _argv: *mut *mut u16) {
    let _ = run_service();
}

/// The main logic of the service.
fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    // Register the service control handler to respond to stop commands.
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

    // Prepare working directories.
    let exe_dir = current_exe_dir();
    let watch_dir = PathBuf::from(
        env::var("WATCH_FOLDER").unwrap_or_else(|_| {
            log::warn!("WATCH_FOLDER environment variable not set. Using default directory.");
            exe_dir.join("ni_watch").to_string_lossy().into_owned()
        }),
    );
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

    // Update service status
    if let Some(handle) = &status_handle {
        handle.set_service_status(ServiceStatus::running())?;
        log::info!("Service status set to Running.");
    }

    // File watcher setup
    let (tx, rx) = channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default()).map_err(|e| {
        log::error!("Failed to initialize file watcher: {}", e);
        e
    })?;
    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;

    // Main loop
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                handle_file_event(event, Duration::from_secs(2)); // Adjust debounce duration as needed
            }
            Ok(Err(e)) => log::warn!("Error receiving file event: {}", e),
            Err(_) => {}
        }
    }

    // Service shutdown
    if let Some(handle) = &status_handle {
        handle.set_service_status(ServiceStatus::stopped())?;
        log::info!("Service status set to Stopped.");
    }
    log::info!("Service has stopped.");
    Ok(())
}

/// Handle file events from the watcher.
/// Handle file events from the watcher.
fn handle_file_event(event: Event, debounce_duration: Duration) {
    static PENDING_FILES: Lazy<Mutex<HashMap<PathBuf, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

    match event.kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Any) => {
            let paths = event.paths;
            let now = Instant::now();

            for path in paths {
                if should_ignore(&path) || !is_image_file(&path) {
                    // Skip non-image files and ignored files
                    continue;
                }

                // Add the file to the pending list
                let mut pending_files = PENDING_FILES.lock().unwrap();
                pending_files.insert(path.clone(), now);

                // Spawn a thread to debounce and process the file
                let debounce_duration = debounce_duration.clone();
                let path_clone = path.clone();
                thread::spawn(move || {
                    thread::sleep(debounce_duration);
                    let mut pending_files = PENDING_FILES.lock().unwrap();

                    // Check if the file is still in the pending list and hasn't been updated recently
                    if let Some(&last_event_time) = pending_files.get(&path_clone) {
                        if now == last_event_time {
                            // File is stable, process it
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

/// Load environment variables from a .env file in the executable directory.
fn load_env() {
    let env_file = current_exe_dir().join(".env");
    if env_file.exists() {
        let _ = dotenvy::from_path(env_file);
    }
}

/// Get the directory of the running executable.
fn current_exe_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .expect("Unable to determine the executable directory")
}

/// Check if a file is an image based on its extension.
fn is_image_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        matches!(
            ext.to_string_lossy().to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tiff"
        )
    } else {
        false
    }
}

/// Determine if a file should be ignored.
fn should_ignore(path: &Path) -> bool {
    // Ignore files with the `.normalized` suffix
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        // Skip temporary files (e.g., `_tmpXXXX`)
        if file_name.contains("_tmp") {
            log::info!("Ignoring temporary file: {:?}", path);
            return true;
        }
        if file_name.contains(".normalized.") {
            log::info!("Ignoring processed file: {:?}", path);
            return true;
        }
    }

    // Debounce recently processed files
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

/// Process an image file: crop, resize, and save it back.
fn process_and_save(path: &PathBuf, size: (u32, u32), pad: u32, tol: u8) -> Result<(), String> {
    if !path.exists() {
        log::error!("File not found: {:?}", path);
        return Err(format!("File not found: {:?}", path));
    }

    log::info!("Processing file: {:?}", path);
    let mut retries = 0;
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY: u64 = 200; // in milliseconds

    let img = loop {
        match ImageReader::open(path) {
            Ok(reader) => match reader.decode() {
                Ok(img) => break img, // Successfully decoded
                Err(e) if retries < MAX_RETRIES => {
                    log::warn!(
                        "Failed to decode image {:?} on attempt {}: {}. Retrying...",
                        path,
                        retries + 1,
                        e
                    );
                    retries += 1;
                    thread::sleep(Duration::from_millis(RETRY_DELAY));
                }
                Err(e) => {
                    log::error!(
                        "Failed to decode image {:?} after {} attempts: {}",
                        path,
                        MAX_RETRIES,
                        e
                    );
                    return Err(format!("Failed to decode image {:?}: {}", path, e));
                }
            },
            Err(e) if retries < MAX_RETRIES => {
                log::warn!(
                    "Failed to open image {:?} on attempt {}: {}. Retrying...",
                    path,
                    retries + 1,
                    e
                );
                retries += 1;
                thread::sleep(Duration::from_millis(RETRY_DELAY));
            }
            Err(e) => {
                log::error!(
                    "Failed to open image {:?} after {} attempts: {}",
                    path,
                    MAX_RETRIES,
                    e
                );
                return Err(format!("Failed to open image {:?}: {}", path, e));
            }
        }
    };

    let processed_image = process_image(img, size, pad, tol);
    log::info!("Image processed successfully: {:?}", path);

    let mut tmp_path = path.clone();
    if let Some(extension) = path.extension() {
        let new_extension = format!("normalized.{}", extension.to_string_lossy());
        tmp_path.set_extension(new_extension);
    } else {
        log::error!(
            "Failed to determine file extension for {:?}. Skipping file.",
            path
        );
        return Err(format!(
            "Failed to determine file extension for {:?}",
            path
        ));
    }

    if let Err(e) = processed_image.save(&tmp_path) {
        log::error!(
            "Failed to save processed image {:?}: {}",
            tmp_path,
            e
        );
        return Err(format!(
            "Failed to save processed image {:?}: {}",
            tmp_path,
            e
        ));
    }
    log::info!("Temporary processed image saved: {:?}", tmp_path);

    fs::rename(&tmp_path, path).map_err(|e| format!("Failed to replace original file {:?}: {}", path, e))?;
    log::info!("Successfully replaced original file with processed image: {:?}", path);

    Ok(())
}

/// Crop an image to its bounding box, resize it, and center it in a padded canvas.
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
            canvas.put_pixel(x, y, Rgba([255, 255, 255, 255])); // Fill with white
        }
    }

    let offset_x = (size.0 - new_width) / 2;
    let offset_y = (size.1 - new_height) / 2;

    canvas.copy_from(&resized, offset_x, offset_y).unwrap();
    canvas
}

/// Calculate the bounding box of non-white pixels in a grayscale image.
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
    (left.min(width - 1), top.min(height - 1), (right + 1).min(width), (bottom + 1).min(height))
}

/// Rolling file logger with configurable size and retention.
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
