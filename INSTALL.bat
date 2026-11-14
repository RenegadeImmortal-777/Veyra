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
(
 echo @echo off
 echo set "KOVREK_HOME=%%LOCALAPPDATA%%\Kovrek"
 echo title Kovrek Terminal
 echo "%%LOCALAPPDATA%%\Kovrek\kovrek.exe" terminal
) > "%KOVREK_HOME%\kovrek-terminal.bat"
reg add "HKCU\Software\Classes\.kvr" /ve /d "Kovrek.SourceFile" /f >nul
reg add "HKCU\Software\Classes\.kvr" /v "PerceivedType" /d "text" /f >nul
reg add "HKCU\Software\Classes\Kovrek.SourceFile" /ve /d "Kovrek Source File" /f >nul
reg add "HKCU\Software\Classes\Kovrek.SourceFile\DefaultIcon" /ve /d "\"%KOVREK_HOME%\kovrek.exe\",0" /f >nul
reg add "HKCU\Software\Classes\Kovrek.SourceFile\shell\open" /ve /d "Run with Kovrek" /f >nul
reg add "HKCU\Software\Classes\Kovrek.SourceFile\shell\open\command" /ve /d "\"%KOVREK_HOME%\kovrek.exe\" launch \"%%1\"" /f >nul
reg add "HKCU\Software\Classes\.kvbc" /ve /d "Kovrek.BytecodeFile" /f >nul
reg add "HKCU\Software\Classes\Kovrek.BytecodeFile" /ve /d "Kovrek Bytecode File" /f >nul
reg add "HKCU\Software\Classes\Kovrek.BytecodeFile\DefaultIcon" /ve /d "\"%KOVREK_HOME%\kovrek.exe\",0" /f >nul
reg add "HKCU\Software\Classes\Kovrek.BytecodeFile\shell\open" /ve /d "Run with Kovrek" /f >nul
reg add "HKCU\Software\Classes\Kovrek.BytecodeFile\shell\open\command" /ve /d "\"%KOVREK_HOME%\kovrek.exe\" launch \"%%1\"" /f >nul
powershell -NoProfile -Command "$p=[Environment]::GetEnvironmentVariable('Path','User'); $v=$env:LOCALAPPDATA+'\Kovrek'; if (($p -split ';') -notcontains $v) {[Environment]::SetEnvironmentVariable('Path', (($p.TrimEnd(';')+';'+$v).Trim(';')), 'User')}"
echo.
echo Kovrek native engine, file associations (.kvr, .kvbc), and Kovrek Terminal installed for this Windows user.
echo Open a NEW terminal and run: kovrek --version  or  kovrek terminal
if /i "%~1"=="--quiet" goto :eof
pause
