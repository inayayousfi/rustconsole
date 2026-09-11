param(
    [string]$OutputDirectory = "target\windows-package"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($env:OS -ne "Windows_NT") {
    throw "Windows packaging requires a Windows host with the WDK and cargo-wdk installed."
}

if (-not $env:VCPKG_ROOT) {
    $vcpkg = Join-Path $env:USERPROFILE "tools\vcpkg"
    if (-not (Test-Path -LiteralPath $vcpkg -PathType Container)) {
        throw "VCPKG_ROOT is not set and vcpkg was not found at $vcpkg."
    }
    $env:VCPKG_ROOT = $vcpkg
}
$env:VCPKGRS_TRIPLET = "x64-windows-static"
if (-not $env:LIBCLANG_PATH) {
    $llvm = Join-Path $env:ProgramFiles "LLVM\bin"
    if (-not (Test-Path -LiteralPath $llvm -PathType Container)) {
        throw "LIBCLANG_PATH is not set and LLVM was not found at $llvm."
    }
    $env:LIBCLANG_PATH = $llvm
}

$root = Split-Path -Parent $PSScriptRoot
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
    throw "Rustup Cargo was not found at $cargo."
}
$output = Join-Path $root $OutputDirectory
$driverRoot = Join-Path $root "native\windows-input-driver"
$driverPackage = Join-Path $driverRoot "target\release\rustconsole_input_driver_package"
$hostExecutable = Join-Path $root "target\release\rustconsole-host.exe"

function Assert-ExpectedSignature {
    param(
        [string]$Path,
        [Security.Cryptography.X509Certificates.X509Certificate2]$Certificate
    )

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -notin @("Valid", "UnknownError")) {
        throw "Authenticode rejected $Path with status $($signature.Status)."
    }
    if (-not $signature.SignerCertificate -or $signature.SignerCertificate.Thumbprint -ne $Certificate.Thumbprint) {
        throw "$Path is not signed by RustConsoleLocalDriverSigning."
    }

    $chain = [Security.Cryptography.X509Certificates.X509Chain]::new()
    try {
        $chain.ChainPolicy.RevocationMode = [Security.Cryptography.X509Certificates.X509RevocationMode]::NoCheck
        $chain.ChainPolicy.VerificationFlags = [Security.Cryptography.X509Certificates.X509VerificationFlags]::AllowUnknownCertificateAuthority
        if (-not $chain.Build($signature.SignerCertificate)) {
            throw "The signing certificate chain for $Path is invalid."
        }
        $unexpected = @($chain.ChainStatus | Where-Object {
            $_.Status -notin @(
                [Security.Cryptography.X509Certificates.X509ChainStatusFlags]::NoError,
                [Security.Cryptography.X509Certificates.X509ChainStatusFlags]::UntrustedRoot
            )
        })
        if ($unexpected.Count -ne 0) {
            throw "The signing certificate chain for $Path has errors other than an untrusted root."
        }
        $root = $chain.ChainElements[$chain.ChainElements.Count - 1].Certificate
        if ($root.Thumbprint -ne $Certificate.Thumbprint) {
            throw "The signing certificate chain for $Path has an unexpected root."
        }
    }
    finally {
        $chain.Dispose()
    }
}

& $cargo +1.95.0 build --release --locked -p rustconsole-host
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
    "rustconsole_input_driver.cat",
    "RustConsoleLocalDriverSigning.cer"
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
foreach ($name in $requiredDriverFiles) {
    Copy-Item -LiteralPath (Join-Path $driverPackage $name) -Destination $outputDriver
}
Copy-Item -LiteralPath (Join-Path $root "THIRD_PARTY.md") -Destination $output

$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
    (Join-Path $outputDriver "RustConsoleLocalDriverSigning.cer")
)
if ($certificate.Subject -ne "CN=RustConsoleLocalDriverSigning") {
    throw "The packaged input driver certificate has the wrong identity: $($certificate.Subject)."
}
foreach ($name in @("rustconsole_input_driver.dll", "rustconsole_input_driver.cat")) {
    $path = Join-Path $outputDriver $name
    Assert-ExpectedSignature -Path $path -Certificate $certificate
}

Write-Output $output
