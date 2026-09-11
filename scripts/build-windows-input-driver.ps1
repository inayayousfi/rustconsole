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
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
    throw "Rustup Cargo was not found at $cargo."
}
$driverRoot = Join-Path $root "native\windows-input-driver"
$driverManifest = Join-Path $driverRoot "Cargo.toml"
$driverPackage = Join-Path $driverRoot "target\$Profile\rustconsole_input_driver_package"
$wdkBuildRevision = "36558802149bc92455bf5719fe77f3a829d8580f"
$wdkBuildPatch = Join-Path $root "patches\windows-drivers-rs\0001-disable-force-integrity-for-umdf.patch"
$certificateSubject = "CN=RustConsoleLocalDriverSigning"
$certificateFile = Join-Path $driverPackage "RustConsoleLocalDriverSigning.cer"
$windowsKitsBin = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
$wdkVersion = Get-ChildItem -LiteralPath $windowsKitsBin -Directory |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName "x64\signtool.exe") } |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName "x86\Inf2Cat.exe") } |
    Sort-Object { [Version]$_.Name } -Descending |
    Select-Object -First 1
if (-not $wdkVersion) {
    throw "The WDK SignTool and Inf2Cat executables were not found under $windowsKitsBin."
}
$signTool = Join-Path $wdkVersion.FullName "x64\signtool.exe"
$inf2Cat = Join-Path $wdkVersion.FullName "x86\Inf2Cat.exe"

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
        throw "$Path is not signed by $certificateSubject."
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

$metadataJson = & $cargo +1.95.0 metadata --format-version 1 --locked --manifest-path $driverManifest
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

& $cargo +1.95.0 clean --manifest-path $driverManifest
if ($LASTEXITCODE -ne 0) {
    throw "Cleaning the UMDF2 input driver build failed."
}

Push-Location $driverRoot
try {
    & $cargo +1.95.0 wdk build --profile $Profile
    if ($LASTEXITCODE -ne 0) {
        throw "The UMDF2 input driver $Profile build failed."
    }
}
finally {
    Pop-Location
}

$certificate = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object {
        $_.Subject -eq $certificateSubject -and
        $_.HasPrivateKey -and
        $_.NotAfter -gt (Get-Date).AddDays(30)
    } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1
if (-not $certificate) {
    $certificate = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $certificateSubject `
        -CertStoreLocation Cert:\CurrentUser\My `
        -KeyAlgorithm RSA `
        -KeyLength 3072 `
        -HashAlgorithm SHA256 `
        -KeyExportPolicy NonExportable `
        -NotAfter (Get-Date).AddYears(10)
}

Export-Certificate -Cert $certificate -FilePath $certificateFile -Force | Out-Null

$driverDll = Join-Path $driverPackage "rustconsole_input_driver.dll"
$driverCatalog = Join-Path $driverPackage "rustconsole_input_driver.cat"
$driverInf = Join-Path $driverPackage "rustconsole_input_driver.inf"
foreach ($path in @($driverDll, $driverInf)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "The UMDF2 input driver build did not produce $path."
    }
}

& $signTool sign /sha1 $certificate.Thumbprint /s My /fd SHA256 $driverDll
if ($LASTEXITCODE -ne 0) {
    throw "Signing the UMDF2 input driver DLL failed."
}
Remove-Item -LiteralPath $driverCatalog -Force -ErrorAction SilentlyContinue
& $inf2Cat "/driver:$driverPackage" /os:10_X64
if ($LASTEXITCODE -ne 0) {
    throw "Regenerating the UMDF2 input driver catalog failed."
}
& $signTool sign /sha1 $certificate.Thumbprint /s My /fd SHA256 $driverCatalog
if ($LASTEXITCODE -ne 0) {
    throw "Signing the UMDF2 input driver catalog failed."
}

foreach ($path in @($driverDll, $driverCatalog)) {
    Assert-ExpectedSignature -Path $path -Certificate $certificate
}

Remove-Item -LiteralPath (Join-Path $driverPackage "WDRLocalTestCert.cer") -Force -ErrorAction SilentlyContinue
