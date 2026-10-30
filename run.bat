@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
cd /d "%~dp0"
echo Starting CueLabs...
target\debug\cuelabs.exe
if errorlevel 1 (
    echo.
    echo === CueLabs exited with error ===
    pause
)
