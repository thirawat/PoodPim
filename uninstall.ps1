# Removes PoodPim from this PC: the program, its Start menu shortcut, start-at-login entry,
# Settings > Apps entry, settings and logs, and, if you say so, the downloaded model (2.4 GB).
# Settings > Apps > PoodPim > Uninstall runs this; so does the app's menu item
# "Uninstall PoodPim...". In PowerShell:
#
#   irm https://raw.githubusercontent.com/thirawat/PoodPim/main/uninstall.ps1 | iex
#
# -Yes: no questions (keeps the model unless -Model). POODPIM_APP: the install folder.
# Messages are in English: Windows PowerShell 5.1 misreads UTF-8 files without a BOM.
param([switch]$Yes, [switch]$Model, [switch]$KeepModel)

$ErrorActionPreference = 'Continue'
Set-Location $env:TEMP # the install folder can't be deleted while it is the working directory

$Dir = if ($env:POODPIM_APP) { $env:POODPIM_APP }
       elseif ($PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot 'PoodPim.exe'))) { $PSScriptRoot }
       else { Join-Path $env:LOCALAPPDATA 'Programs\PoodPim' }
$Data = Join-Path $env:APPDATA 'poodpim'      # settings and the model
$Logs = Join-Path $env:LOCALAPPDATA 'poodpim' # logs

function Ask([string]$Question) {
    if ($Yes) { return $false }
    return (Read-Host "$Question [y/N]") -match '^(y|yes)$'
}

if (-not $Yes -and -not (Ask 'Remove PoodPim from this PC (program, settings, start at login)?')) {
    Write-Host 'Cancelled.'
    return
}
$DeleteModel = $Model -and -not $KeepModel
if (-not $Model -and -not $KeepModel -and (Test-Path (Join-Path $Data 'openthai-asr'))) {
    $DeleteModel = Ask 'Also delete the speech model (2.4 GB)? Keep it to skip the download if you install again.'
}

# Quit the program (it may be the one that started this).
$Exe = Join-Path $Dir 'PoodPim.exe'
foreach ($p in @(Get-Process PoodPim -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe })) {
    if (-not $p.WaitForExit(10000)) { Stop-Process -Id $p.Id -Force; $p.WaitForExit(5000) | Out-Null }
}

Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name PoodPim -ErrorAction SilentlyContinue
Remove-Item -Force (Join-Path ([Environment]::GetFolderPath('Programs')) 'PoodPim.lnk') -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\PoodPim' -ErrorAction SilentlyContinue

$ok = $true
if (Test-Path $Dir) {
    Remove-Item -Recurse -Force $Dir -ErrorAction SilentlyContinue
    if (Test-Path $Dir) { Write-Host "Could not delete $Dir; delete it yourself." -ForegroundColor Yellow; $ok = $false }
    else { Write-Host "Deleted $Dir" }
}
if ($DeleteModel) {
    Remove-Item -Recurse -Force $Data -ErrorAction SilentlyContinue
    Write-Host 'Deleted the settings and the model.'
} elseif (Test-Path $Data) {
    Get-ChildItem $Data | Where-Object { $_.Name -ne 'openthai-asr' } | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Deleted the settings (the model is kept in $Data\openthai-asr)."
}
Remove-Item -Recurse -Force $Logs -ErrorAction SilentlyContinue # also this run's log, if it can

if ($ok) { Write-Host 'PoodPim is uninstalled.' }
if (-not $Yes -and $PSCommandPath) { Read-Host 'Press Enter to close' | Out-Null }
