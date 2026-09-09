param(
    [string]$OutputDirectory = "target\windows-package"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($env:OS -ne "Windows_NT") {
    throw "Windows packaging requires a Windows host with the WDK and cargo-wdk installed."
}

$root = Split-Path -Parent $PSScriptRoot
$output = Join-Path $root $OutputDirectory
$driverRoot = Join-Path $root "native\windows-input-driver"
$driverPackage = Join-Path $driverRoot "target\release\rustconsole_input_driver_package"
$hostExecutable = Join-Path $root "target\release\rustconsole-host.exe"

& cargo +1.95.0 build --release --locked -p rustconsole-host
if ($LASTEXITCODE -ne 0) {
    throw "The Windows host release build failed."
}

& (Join-Path $PSScriptRoot "build-windows-input-driver.ps1") -Profile release
if ($LASTEXITCODE -ne 0) {
    throw "The UMDF2 input driver release build failed."
}

$requiredDriverFiles = @(
    "rustconsole_input_driver.dll",
    "rustconsole_input_driver.inf",
    "rustconsole_input_driver.cat"
)
if (-not (Test-Path -LiteralPath $hostExecutable -PathType Leaf)) {
    throw "The host build did not produce $hostExecutable."
}
foreach ($name in $requiredDriverFiles) {
    $path = Join-Path $driverPackage $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "The driver package did not produce $path."
    }
}

Remove-Item -LiteralPath $output -Recurse -Force -ErrorAction SilentlyContinue
$outputDriver = Join-Path $output "input-driver"
New-Item -ItemType Directory -Path $outputDriver -Force | Out-Null
Copy-Item -LiteralPath $hostExecutable -Destination $output
Copy-Item -Path (Join-Path $driverPackage "*") -Destination $outputDriver -Recurse
Copy-Item -LiteralPath (Join-Path $root "THIRD_PARTY.md") -Destination $output

Write-Output $output
