
# ni-watcher Windows Service

A Windows service that watches a folder for new image files and runs a standalone image normalization CLI (`ni.exe`) on them automatically. Designed for headless server environments.

---

## 🔧 Build Instructions

### Prerequisites
- Rust (with MSVC toolchain)
- Visual Studio Build Tools ("Desktop Development with C++" workload)

### Build
```cmd
cargo build --release
```

### Output
The compiled service binary will be at:
```
target\release\ni-service.exe
```

---

## 🚀 Install the Service

### 1. Prepare the binaries
Place the following files into a folder:
- `target\release\ni-service.exe`
- `ni.exe` (your image normalizer CLI tool)

### 2. Run the installer
Use the provided `install_ni_service.bat` script:
```cmd
install_ni_service.bat
```
This will:
- Ask you for the folder to monitor (e.g., `C:\Images`)
- Create a `.env` file pointing to that folder
- Copy everything to `C:\Program Files\ni-watcher\`
- Register the Windows service
- Start it automatically

---

## 🛑 Uninstall the Service
Use the included `uninstall_ni_service.bat` script:
```cmd
uninstall_ni_service.bat
```
This will:
- Stop the service
- Remove it from Windows
- Optionally delete the install folder

---

## 🧪 Test Locally Without Service
```cmd
cargo run --release
```
This runs the watcher logic in the foreground (for development/testing).

---

## 📂 Behavior
- Watches the folder specified in `.env` as `WATCH_FOLDER`
- On new file creation or rename:
  - Runs `ni.exe -i <path> -o <same dir>`
  - Replaces the original image with the normalized one

---

## 📦 Future Ideas
- Logging to a file or Windows Event Log
- Image-type filtering
- Restart resilience
- GUI installer

---

## 📃 License
MIT


