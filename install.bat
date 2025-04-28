@echo off
setlocal

REM Prompt user for folder to monitor
set /p WATCH_FOLDER="C:\ΦΩΤΟ SOFT1\"

REM Set default install directory
set TARGET_DIR=C:\ProgramData\ni-watcher

REM Create target directory if it doesn't exist
if not exist "%TARGET_DIR%" mkdir "%TARGET_DIR%"

REM Check for ni.exe in default build location
set NI_BIN=..\ni-rs\target\release\ni.exe
if not exist "%NI_BIN%" (
echo.
echo ERROR: Could not find ni.exe at "%NI_BIN%"
echo Please build the CLI first or move ni.exe to this path.
echo Installation aborted.
pause
exit /b 1
)

REM Copy binaries
copy /Y target\release\ni-service.exe "%TARGET_DIR%" >nul
copy /Y "%NI_BIN%" "%TARGET_DIR%" >nul

REM Create .env file with the selected watch folder
echo WATCH_FOLDER=%WATCH_FOLDER% > "%TARGET_DIR%.env"

REM Register service
sc create "ni-watcher" binPath= ""%TARGET_DIR%\ni-service.exe"" start= auto

REM Start service
sc start ni-watcher

echo Service ni-watcher installed and started.
endlocal
pause
