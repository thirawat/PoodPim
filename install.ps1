# Installs or updates PoodPim on Windows 10/11 (64-bit). No administrator rights needed.
# In PowerShell:
#
#   irm https://raw.githubusercontent.com/thirawat/PoodPim/main/install.ps1 | iex
#
# A given version, e.g. a release candidate:
#   $env:POODPIM_VERSION = 'v1.0.1-rc.1'; irm https://raw.githubusercontent.com/thirawat/PoodPim/main/install.ps1 | iex
#
# It downloads the release zip from GitHub, checks its SHA-256, puts PoodPim in
# %LOCALAPPDATA%\Programs\PoodPim, adds a Start menu shortcut and an entry in
# Settings > Apps (to uninstall), quits a running copy and starts the new one.
# The app updates and uninstalls itself (PoodPim.exe --uninstall, also run by Settings > Apps);
# POODPIM_APP picks another folder to install into. The release exe is not code-signed, so
# unlike install.sh this can only check that the download is intact, not who built it.
# Messages are in English: Windows PowerShell 5.1 misreads UTF-8 files without a BOM.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # Invoke-WebRequest is very slow with a progress bar
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = 'thirawat/PoodPim'

function Fail([string]$Message) {
    Write-Host "PoodPim was not installed: $Message" -ForegroundColor Red
    throw $Message # not `exit`: under `irm | iex` that would close the user's window
}

if (-not [Environment]::Is64BitOperatingSystem) { Fail 'PoodPim needs 64-bit Windows.' }

$Tag = $env:POODPIM_VERSION
if (-not $Tag) {
    try { $Tag = (Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest").tag_name }
    catch { Fail "could not reach GitHub ($($_.Exception.Message))" }
}
if ($Tag -notmatch '^v\d') { Fail "unexpected version '$Tag'" }
$Version = $Tag.Substring(1)
$Zip = "PoodPim-$Version-windows.zip"
$Url = "https://github.com/$Repo/releases/download/$Tag"

$Dir = if ($env:POODPIM_APP) { $env:POODPIM_APP } else { Join-Path $env:LOCALAPPDATA 'Programs\PoodPim' }
$Dir = [IO.Path]::GetFullPath($Dir)
$Parent = Split-Path $Dir -Parent
$Exe = Join-Path $Dir 'PoodPim.exe'
New-Item -ItemType Directory -Force -Path $Parent | Out-Null
Set-Location $env:TEMP # a folder can't be renamed while it is the working directory

$Tmp = Join-Path $env:TEMP "poodpim-install-$PID"
$Stage = Join-Path $Parent ".PoodPim-install-$PID" # same drive as $Dir, so it can be moved in
$Old = Join-Path $Parent ".PoodPim-old-$PID"
# Started by the app to update it: the app has quit already, so whatever happens, end with
# PoodPim running again.
$Reopen = [bool]$env:POODPIM_APP
$Done = $false
try {
    New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
    Write-Host "Downloading PoodPim $Version..."
    Invoke-WebRequest -UseBasicParsing -Uri "$Url/$Zip" -OutFile "$Tmp\$Zip"
    Invoke-WebRequest -UseBasicParsing -Uri "$Url/$Zip.sha256" -OutFile "$Tmp\$Zip.sha256"
    $Expected = ((Get-Content "$Tmp\$Zip.sha256" -Raw).Trim() -split '\s+')[0]
    $Actual = (Get-FileHash -Algorithm SHA256 "$Tmp\$Zip").Hash
    if ($Actual -ne $Expected) { Fail 'the download is damaged (checksum mismatch)' }

    Expand-Archive -Path "$Tmp\$Zip" -DestinationPath $Stage -Force
    $New = Join-Path $Stage 'PoodPim'
    if (-not (Test-Path (Join-Path $New 'PoodPim.exe'))) { Fail 'PoodPim.exe is missing from the zip' }

    # Quit the running copy (it may be the one that started this update) and wait for it.
    $running = @(Get-Process PoodPim -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe })
    if ($running.Count -gt 0) {
        $Reopen = $true
        Write-Host 'Closing the running PoodPim...'
        foreach ($p in $running) {
            if (-not $p.WaitForExit(10000)) { Stop-Process -Id $p.Id -Force; $p.WaitForExit(5000) | Out-Null }
        }
    }

    if (Test-Path $Dir) { Move-Item $Dir $Old }
    try { Move-Item $New $Dir }
    catch { if (Test-Path $Old) { Move-Item $Old $Dir }; throw }
    $Done = $true
    if (Test-Path $Old) { Remove-Item -Recurse -Force $Old -ErrorAction SilentlyContinue }

    # Start menu shortcut.
    $Lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'PoodPim.lnk'
    $Shell = New-Object -ComObject WScript.Shell
    $Shortcut = $Shell.CreateShortcut($Lnk)
    $Shortcut.TargetPath = $Exe
    $Shortcut.WorkingDirectory = $Dir
    $Ico = Join-Path $Dir 'PoodPim.ico'
    if (Test-Path $Ico) { $Shortcut.IconLocation = "$Ico,0" }
    $Shortcut.Description = 'PoodPim: Thai dictation'
    $Shortcut.Save()

    # Settings > Apps entry, for uninstalling.
    $Key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\PoodPim'
    New-Item -Force -Path $Key | Out-Null
    $SizeKb = [int]((Get-ChildItem -Recurse $Dir | Measure-Object Length -Sum).Sum / 1KB)
    $Values = @{
        DisplayName     = 'PoodPim'
        DisplayVersion  = $Version
        Publisher       = 'PoodPim'
        DisplayIcon     = $(if (Test-Path (Join-Path $Dir 'PoodPim.ico')) { Join-Path $Dir 'PoodPim.ico' } else { $Exe })
        InstallLocation = $Dir
        URLInfoAbout    = "https://github.com/$Repo"
        UninstallString = "`"$Exe`" --uninstall"
    }
    foreach ($k in $Values.Keys) { Set-ItemProperty -Path $Key -Name $k -Value $Values[$k] }
    foreach ($k in 'NoModify', 'NoRepair') { Set-ItemProperty -Path $Key -Name $k -Value 1 -Type DWord }
    Set-ItemProperty -Path $Key -Name EstimatedSize -Value $SizeKb -Type DWord

    # PoodPim.exe (x64) needs the x64 Visual C++ Runtime (msvcp140.dll...), present on most PCs;
    # without it Windows only says "MSVCP140.dll was not found". Installing it needs administrator
    # rights (Windows asks); if that fails, PoodPim stays installed and this says where to get it.
    $VcUrl = 'https://aka.ms/vs/17/release/vc_redist.x64.exe'
    $Reg64 = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', 'Registry64')
    # The real CPU, also from a 32-bit or x64-emulated PowerShell.
    $Arch = $Reg64.OpenSubKey('SYSTEM\CurrentControlSet\Control\Session Manager\Environment').GetValue('PROCESSOR_ARCHITECTURE')
    function Test-VcRuntime {
        # Registered by the x64 redistributable (on ARM64 too, where x64 apps are emulated).
        $Key = $Reg64.OpenSubKey('SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\X64')
        if ($Key -and $Key.GetValue('Installed') -eq 1) { return $true }
        # Or the DLLs, put there by something else. Not on ARM64, where they are ARM64 ones; a
        # 32-bit PowerShell sees SysWOW64 as System32, so look through Sysnative.
        if ($Arch -eq 'ARM64') { return $false }
        $Sys = if ([Environment]::Is64BitProcess) { 'System32' } else { 'Sysnative' }
        return -not ('msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll' |
            Where-Object { -not (Test-Path (Join-Path $env:windir "$Sys\$_")) })
    }
    $VcReady = Test-VcRuntime
    if (-not $VcReady) {
        Write-Host 'PoodPim needs the Microsoft Visual C++ Runtime, which this PC does not have yet.'
        Write-Host 'Installing it now: click Yes when Windows asks for permission.'
        try {
            $Vc = Join-Path $Tmp 'vc_redist.x64.exe'
            Invoke-WebRequest -UseBasicParsing -Uri $VcUrl -OutFile $Vc
            $Sig = Get-AuthenticodeSignature $Vc
            if ($Sig.Status -ne 'Valid' -or $Sig.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation') {
                throw 'the download is not signed by Microsoft'
            }
            $P = Start-Process -FilePath $Vc -ArgumentList '/install', '/quiet', '/norestart' -Verb RunAs -Wait -PassThru
            # 3010: installed, Windows wants a restart; 1638: another version is already there,
            # which the check below decides about.
            if (@(0, 3010, 1638) -notcontains $P.ExitCode) { throw "its installer exited with code $($P.ExitCode)" }
            $VcReady = Test-VcRuntime
            if (-not $VcReady) { throw 'it is still missing after its installer ran' }
            if ($P.ExitCode -eq 3010) { Write-Host 'Installed it. If PoodPim does not start, restart Windows.' }
        }
        catch {
            Write-Host "Could not install the Visual C++ Runtime ($($_.Exception.Message))." -ForegroundColor Yellow
            Write-Host "Install it from $VcUrl, then open PoodPim from the Start menu." -ForegroundColor Yellow
        }
    }

    if ($VcReady) { Start-Process -FilePath $Exe -WorkingDirectory $Dir }
    Write-Host "PoodPim $Version is installed in $Dir"
    if ($VcReady) { Write-Host 'Look for the microphone icon next to the clock, then hold Right Alt to talk.' }
}
finally {
    Remove-Item -Recurse -Force $Tmp, $Stage -ErrorAction SilentlyContinue
    # The old copy was closed: start it again if the new one didn't go in.
    if ($Reopen -and -not $Done -and (Test-Path $Exe)) { Start-Process -FilePath $Exe -WorkingDirectory $Dir }
}
