@echo off
setlocal
set "VEYRA_HOME=%LOCALAPPDATA%\Veyra"
if not exist "%VEYRA_HOME%" mkdir "%VEYRA_HOME%"
if not exist "%VEYRA_HOME%\bin" mkdir "%VEYRA_HOME%\bin"
if not exist "%VEYRA_HOME%\registry" mkdir "%VEYRA_HOME%\registry"
if exist "%~dp0bin\veyra-native.exe" (
    copy /Y "%~dp0bin\veyra-native.exe" "%VEYRA_HOME%\bin\veyra-native.exe" >nul
    copy /Y "%~dp0bin\veyra-native.exe" "%VEYRA_HOME%\veyra.exe" >nul
)
if exist "%~dp0registry\index.json" (
    copy /Y "%~dp0registry\index.json" "%VEYRA_HOME%\registry\index.json" >nul
)
if exist "%~dp0packages" (
    xcopy /E /I /Y "%~dp0packages" "%VEYRA_HOME%\packages" >nul
)
copy /Y "%~dp0veyra.py" "%VEYRA_HOME%\veyra.py" >nul
(
 echo @echo off
 echo set "VEYRA_HOME=%%LOCALAPPDATA%%\Veyra"
 echo "%%LOCALAPPDATA%%\Veyra\bin\veyra-native.exe" %%*
) > "%VEYRA_HOME%\veyra.bat"
(
 echo @echo off
 echo python "%%LOCALAPPDATA%%\Veyra\veyra.py" %%*
) > "%VEYRA_HOME%\veyra-py.bat"
powershell -NoProfile -Command "$p=[Environment]::GetEnvironmentVariable('Path','User'); $v=$env:LOCALAPPDATA+'\Veyra'; if (($p -split ';') -notcontains $v) {[Environment]::SetEnvironmentVariable('Path', (($p.TrimEnd(';')+';'+$v).Trim(';')), 'User')}"
echo.
echo Veyra native engine installed for this Windows user.
echo Open a NEW terminal and run: veyra --version
pause
