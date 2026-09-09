param(
    [ValidateSet("dev", "release")]
    [string]$Profile = "release"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($env:OS -ne "Windows_NT") {
    throw "The UMDF2 input driver build requires a Windows host with the WDK and cargo-wdk installed."
}

$root = Split-Path -Parent $PSScriptRoot
$driverRoot = Join-Path $root "native\windows-input-driver"
$driverManifest = Join-Path $driverRoot "Cargo.toml"
$wdkBuildRevision = "36558802149bc92455bf5719fe77f3a829d8580f"
$wdkBuildPatch = Join-Path $root "patches\windows-drivers-rs\0001-disable-force-integrity-for-umdf.patch"

$metadataJson = & cargo +1.95.0 metadata --format-version 1 --locked --manifest-path $driverManifest
if ($LASTEXITCODE -ne 0) {
    throw "Cargo metadata failed while locating the pinned wdk-build source."
}
$metadata = $metadataJson | ConvertFrom-Json
$wdkBuildPackages = @($metadata.packages | Where-Object {
    $_.name -eq "wdk-build" -and $_.source -like "git+https://github.com/microsoft/windows-drivers-rs*#$wdkBuildRevision"
})
if ($wdkBuildPackages.Count -ne 1) {
    throw "Expected exactly one wdk-build package at revision $wdkBuildRevision."
}

$wdkBuildManifest = $wdkBuildPackages[0].manifest_path
$wdkBuildCheckout = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $wdkBuildManifest))
function Test-WdkBuildPatch {
    param([string[]]$Arguments)

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & git -C $wdkBuildCheckout apply @Arguments $wdkBuildPatch 2>$null
        return $LASTEXITCODE -eq 0
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
}

if (-not (Test-WdkBuildPatch @("--reverse", "--check"))) {
    if (-not (Test-WdkBuildPatch @("--check"))) {
        throw "The UMDF integrity patch no longer applies to the pinned wdk-build source."
    }
    & git -C $wdkBuildCheckout apply $wdkBuildPatch
    if ($LASTEXITCODE -ne 0) {
        throw "Applying the UMDF integrity patch to the Cargo git cache failed."
    }
}

& cargo +1.95.0 clean --manifest-path $driverManifest
if ($LASTEXITCODE -ne 0) {
    throw "Cleaning the UMDF2 input driver build failed."
}

Push-Location $driverRoot
try {
    & cargo +1.95.0 wdk build --profile $Profile
    if ($LASTEXITCODE -ne 0) {
        throw "The UMDF2 input driver $Profile build failed."
    }
}
finally {
    Pop-Location
}
