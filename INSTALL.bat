@echo off
setlocal
set "VEYRA_HOME=%LOCALAPPDATA%\Veyra"
if not exist "%VEYRA_HOME%" mkdir "%VEYRA_HOME%"
copy /Y "%~dp0veyra.py" "%VEYRA_HOME%\veyra.py" >nul
(
 echo @echo off
 echo python "%%LOCALAPPDATA%%\Veyra\veyra.py" %%*
) > "%VEYRA_HOME%\veyra.bat"
powershell -NoProfile -Command "$p=[Environment]::GetEnvironmentVariable('Path','User'); $v=$env:LOCALAPPDATA+'\Veyra'; if (($p -split ';') -notcontains $v) {[Environment]::SetEnvironmentVariable('Path', (($p.TrimEnd(';')+';'+$v).Trim(';')), 'User')}"
echo.
echo Veyra installed for this Windows user.
echo Open a NEW terminal and run: veyra --version
pause
