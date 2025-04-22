
@echo off
setlocal

REM Prompt user for folder to monitor
set /p WATCH_FOLDER=Enter the full path to the folder to watch (e.g. C:\Images): 

REM Set default install directory
set TARGET_DIR=C:\Program Files\ni-watcher

REM Create target directory if it doesn't exist
if not exist "%TARGET_DIR%" mkdir "%TARGET_DIR%"

REM Copy binaries (edit these paths if needed)
copy /Y target\release\ni-service.exe "%TARGET_DIR%" >nul
copy /Y path\to\ni.exe "%TARGET_DIR%" >nul

REM Create .env file with the selected watch folder
echo WATCH_FOLDER=%WATCH_FOLDER% > "%TARGET_DIR%\.env"

REM Register service
sc create "ni-watcher" binPath= "\"%TARGET_DIR%\ni-service.exe\"" start= auto

REM Start service
sc start ni-watcher

echo Service ni-watcher installed and started.
endlocal
pause
