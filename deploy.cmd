@echo off
rem os-watcher Windows deploy entry point.
rem
rem Double-click to run: a non-admin session is re-launched through UAC, and the
rem arguments are handed to deploy.ps1 unchanged. Pass arguments from a console:
rem     deploy.cmd -Package full -Force
setlocal EnableExtensions

set "PS_SCRIPT=%~dp0deploy.ps1"
if not exist "%PS_SCRIPT%" (
    echo [FAIL] deploy.ps1 not found: %PS_SCRIPT%
    pause
    exit /b 1
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%PS_SCRIPT%" -Elevate %*
set "EXIT_CODE=%errorlevel%"

echo.
if "%EXIT_CODE%"=="0" (
    echo [ OK ] deploy finished
) else (
    echo [FAIL] deploy failed, exit code %EXIT_CODE%
)
pause
exit /b %EXIT_CODE%
