@echo off
setlocal
set "KOVREK_HOME=%LOCALAPPDATA%\Kovrek"
if not exist "%KOVREK_HOME%" mkdir "%KOVREK_HOME%"
if not exist "%KOVREK_HOME%\bin" mkdir "%KOVREK_HOME%\bin"
if not exist "%KOVREK_HOME%\registry" mkdir "%KOVREK_HOME%\registry"
if exist "%~dp0bin\kovrek-native.exe" (
    copy /Y "%~dp0bin\kovrek-native.exe" "%KOVREK_HOME%\bin\kovrek-native.exe" >nul
    copy /Y "%~dp0bin\kovrek-native.exe" "%KOVREK_HOME%\kovrek.exe" >nul
)
if exist "%~dp0registry\index.json" (
    copy /Y "%~dp0registry\index.json" "%KOVREK_HOME%\registry\index.json" >nul
)
if exist "%~dp0packages" (
    xcopy /E /I /Y "%~dp0packages" "%KOVREK_HOME%\packages" >nul
)
copy /Y "%~dp0kovrek.py" "%KOVREK_HOME%\kovrek.py" >nul
(
 echo @echo off
 echo set "KOVREK_HOME=%%LOCALAPPDATA%%\Kovrek"
 echo "%%LOCALAPPDATA%%\Kovrek\bin\kovrek-native.exe" %%*
) > "%KOVREK_HOME%\kovrek.bat"
(
 echo @echo off
 echo python "%%LOCALAPPDATA%%\Kovrek\kovrek.py" %%*
) > "%KOVREK_HOME%\kovrek-py.bat"
powershell -NoProfile -Command "$p=[Environment]::GetEnvironmentVariable('Path','User'); $v=$env:LOCALAPPDATA+'\Kovrek'; if (($p -split ';') -notcontains $v) {[Environment]::SetEnvironmentVariable('Path', (($p.TrimEnd(';')+';'+$v).Trim(';')), 'User')}"
echo.
echo Kovrek native engine installed for this Windows user.
echo Open a NEW terminal and run: kovrek --version
pause
