$ErrorActionPreference = 'Stop'

$workspace = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$installDirectory = Join-Path $env:RUNNER_TEMP 'cmr-nsis-verification'
$installer = Get-ChildItem -Path (Join-Path $workspace 'target') -Recurse -File -Filter '*-setup.exe' |
    Where-Object { $_.FullName -match '[\\/]bundle[\\/]nsis[\\/]' } |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1

if ($null -eq $installer) {
    throw 'The Tauri build did not produce an NSIS installer.'
}
if (Test-Path -LiteralPath $installDirectory) {
    throw "Refusing to reuse existing verification directory: $installDirectory"
}

$install = Start-Process -FilePath $installer.FullName `
    -ArgumentList @('/S', "/D=$installDirectory") `
    -Wait -PassThru -WindowStyle Hidden
if ($install.ExitCode -ne 0) {
    throw "Silent NSIS installation failed with exit code $($install.ExitCode)."
}

$desktop = Join-Path $installDirectory 'cmr-desktop.exe'
$router = Join-Path $installDirectory 'cmr.exe'
$service = Join-Path $installDirectory 'cmr-service.exe'
foreach ($binary in @($desktop, $router, $service)) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Installed bundle is missing $binary"
    }
    if ((Get-Item -LiteralPath $binary).Length -eq 0) {
        throw "Installed binary is empty: $binary"
    }
}
if ((Split-Path -Parent $desktop) -ne (Split-Path -Parent $router) -or
    (Split-Path -Parent $desktop) -ne (Split-Path -Parent $service)) {
    throw 'cmr.exe and cmr-service.exe are not siblings of cmr-desktop.exe.'
}
$qualifiedSidecars = @(Get-ChildItem -LiteralPath $installDirectory -File |
    Where-Object { $_.Name -match '^cmr(?:-service)?-[^-]+-pc-windows-msvc\.exe$' })
if ($qualifiedSidecars.Count -ne 0) {
    throw 'Tauri retained a target suffix instead of installing the sidecars by their stable names.'
}

& $router '--version' | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "The installed cmr.exe failed its version probe with exit code $LASTEXITCODE."
}

$uninstaller = Join-Path $installDirectory 'uninstall.exe'
if (-not (Test-Path -LiteralPath $uninstaller -PathType Leaf)) {
    throw "Installed bundle is missing $uninstaller"
}
$uninstall = Start-Process -FilePath $uninstaller -ArgumentList '/S' `
    -Wait -PassThru -WindowStyle Hidden
if ($uninstall.ExitCode -ne 0) {
    throw "Silent NSIS uninstall failed with exit code $($uninstall.ExitCode)."
}

Write-Host "Verified installed sibling sidecars: $router, $service"
