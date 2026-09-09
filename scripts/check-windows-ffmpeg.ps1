param(
    [string]$Triplet = "x64-windows-static"
)

$ErrorActionPreference = "Stop"

if (-not $env:VCPKG_ROOT) {
    throw "VCPKG_ROOT is not set"
}

$archive = Join-Path $env:VCPKG_ROOT "installed\$Triplet\lib\avcodec.lib"
if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
    throw "FFmpeg static codec archive was not found at $archive"
}

$archiveText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($archive))
foreach ($symbol in @("ff_av1_nvenc_encoder", "ff_libopus_encoder")) {
    if ($archiveText.IndexOf($symbol, [StringComparison]::Ordinal) -lt 0) {
        throw "FFmpeg static codec archive does not contain $symbol"
    }
}

Write-Output "Windows FFmpeg AV1 NVENC and libopus symbol checks passed."
