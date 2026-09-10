param(
    [string]$SourceDirectory = (Split-Path -Parent $PSScriptRoot),
    [string]$InstallDirectory = (Join-Path $env:ProgramFiles "RustConsole"),
    [string]$ServiceName = "RustConsoleHostDev"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Rust Console host installation requires an elevated administrator terminal."
}

if (-not $env:VCPKG_ROOT) {
    $vcpkg = Join-Path $env:USERPROFILE "tools\vcpkg"
    if (Test-Path -LiteralPath $vcpkg -PathType Container) {
        $env:VCPKG_ROOT = $vcpkg
    }
}
$env:VCPKGRS_TRIPLET = "x64-windows-static"
if (-not $env:LIBCLANG_PATH) {
    $llvm = Join-Path $env:ProgramFiles "LLVM\bin"
    if (Test-Path -LiteralPath $llvm -PathType Container) {
        $env:LIBCLANG_PATH = $llvm
    }
}

$packageScript = Join-Path $SourceDirectory "scripts\package-windows.ps1"
$packageOutput = & $packageScript | Select-Object -Last 1
if ($LASTEXITCODE -ne 0) {
    throw "The Windows package build failed."
}
$packageDirectory = if ([IO.Path]::IsPathRooted($packageOutput)) {
    [IO.Path]::GetFullPath($packageOutput)
} else {
    [IO.Path]::GetFullPath((Join-Path $SourceDirectory $packageOutput))
}
$packagedExecutable = Join-Path $packageDirectory "rustconsole-host.exe"
$packagedDriver = Join-Path $packageDirectory "input-driver"
$driverInf = Join-Path $packagedDriver "rustconsole_input_driver.inf"
$driverCertificate = Join-Path $packagedDriver "WDRLocalTestCert.cer"
foreach ($path in @($packagedExecutable, $driverInf, $driverCertificate)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "The Windows package is incomplete: $path is missing."
    }
}

Write-Warning "The input driver uses a self-signed development certificate. Do not use this installation process for production distribution."
$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new($driverCertificate)
foreach ($store in @("Root", "TrustedPublisher")) {
    $installedCertificate = "Cert:\LocalMachine\$store\$($certificate.Thumbprint)"
    if (-not (Test-Path -LiteralPath $installedCertificate)) {
        Import-Certificate -FilePath $driverCertificate -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
    }
}

$stagedDirectory = "$InstallDirectory.new"
$previousDirectory = "$InstallDirectory.previous"
Remove-Item -LiteralPath $stagedDirectory -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $previousDirectory -Recurse -Force -ErrorAction SilentlyContinue
Copy-Item -LiteralPath $packageDirectory -Destination $stagedDirectory -Recurse

$existingService = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
$hadService = $null -ne $existingService
$hadInstallation = Test-Path -LiteralPath $InstallDirectory -PathType Container
$previousSaved = $false
$replacementInstalled = $false
$installedExecutable = Join-Path $InstallDirectory "rustconsole-host.exe"
$credentialRecord = Join-Path $env:ProgramData "RustConsole\opaque-record.bin"

try {
    if ($hadService -and $existingService.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Stopped) {
        Stop-Service -Name $ServiceName
        (Get-Service -Name $ServiceName).WaitForStatus(
            [System.ServiceProcess.ServiceControllerStatus]::Stopped,
            [TimeSpan]::FromSeconds(30)
        )
    }

    $driverOutput = & pnputil.exe /add-driver $driverInf /install 2>&1
    $driverOutput | Write-Output
    if ($LASTEXITCODE -ne 0) {
        throw "The Rust Console input driver installation failed with exit code $LASTEXITCODE."
    }

    if ($hadInstallation) {
        Move-Item -LiteralPath $InstallDirectory -Destination $previousDirectory
        $previousSaved = $true
    }
    Move-Item -LiteralPath $stagedDirectory -Destination $InstallDirectory
    $replacementInstalled = $true

    if (-not $hadService) {
        & $installedExecutable install
        if ($LASTEXITCODE -ne 0) {
            throw "The Rust Console host service installation failed."
        }
    } else {
        & sc.exe config $ServiceName binPath= "`"$installedExecutable`"" start= auto | Write-Output
        if ($LASTEXITCODE -ne 0) {
            throw "The Rust Console host service configuration failed."
        }
    }

    if (-not (Test-Path -LiteralPath $credentialRecord -PathType Leaf)) {
        Write-Output "No host password exists. Enter it now; later runs preserve it."
        & $installedExecutable set-password
        if ($LASTEXITCODE -ne 0) {
            throw "The Rust Console host password setup failed."
        }
    }

    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus(
        [System.ServiceProcess.ServiceControllerStatus]::Running,
        [TimeSpan]::FromSeconds(30)
    )
} catch {
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    if ($replacementInstalled) {
        Remove-Item -LiteralPath $InstallDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($previousSaved) {
        Move-Item -LiteralPath $previousDirectory -Destination $InstallDirectory
    }
    if ($hadService) {
        Start-Service -Name $ServiceName
    } else {
        & sc.exe delete $ServiceName | Out-Null
    }
    throw
}

Remove-Item -LiteralPath $previousDirectory -Recurse -Force -ErrorAction SilentlyContinue
Get-FileHash -Algorithm SHA256 -LiteralPath $installedExecutable
Get-Service -Name $ServiceName
