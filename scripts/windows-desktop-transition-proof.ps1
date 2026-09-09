param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet(
        "prepare-round-trip",
        "prepare-login",
        "prepare-display-mode",
        "start",
        "run-display-mode",
        "run-display-mode-interactive",
        "trigger-uac",
        "trigger-lock",
        "status",
        "report",
        "reboot",
        "cleanup"
    )]
    [string] $Action,

    [string] $Executable
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($Executable)) {
    $Executable = Join-Path $PSScriptRoot "..\target\release\rustconsole-host.exe"
}

$serviceName = "RustConsoleHostDev"
$uacTaskName = "RustConsoleUacPrompt"
$lockTaskName = "RustConsoleLock"
$displayModeActivityTaskName = "RustConsoleDisplayModeActivity"
$displayModeRunnerTaskName = "RustConsoleDisplayModeRunner"
$displayModeActivityScript = "C:\ProgramData\RustConsole\display-mode-activity.ps1"
$displayModeRunnerResult = "C:\ProgramData\RustConsole\display-mode-runner-result.txt"
$roundTripReport = "C:\ProgramData\RustConsole\desktop-transition-proof.txt"
$loginReport = "C:\ProgramData\RustConsole\login-transition-proof.txt"
$displayModeReport = "C:\ProgramData\RustConsole\display-mode-transition-proof.txt"

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "This proof harness must run from an administrator PowerShell session."
    }
}

function Stop-ProofService {
    $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
    if ($null -ne $service -and $service.Status -ne "Stopped") {
        Stop-Service -Name $serviceName -Force
    }
}

function Set-ProofService([string] $serviceCommand) {
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
        throw "Host service executable not found: $Executable"
    }

    Stop-ProofService
    $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        & $Executable "install-$serviceCommand"
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to install $serviceName in $serviceCommand mode."
        }
    }
    else {
        & sc.exe config $serviceName start= auto binPath= "`"$Executable`" $serviceCommand"
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to configure $serviceName in $serviceCommand mode."
        }
    }

    Remove-Item -LiteralPath $roundTripReport, $loginReport, $displayModeReport -Force -ErrorAction SilentlyContinue
    Write-Output "$serviceName is prepared and stopped in $serviceCommand mode."
}

function Get-InteractiveUser {
    $user = (Get-CimInstance Win32_ComputerSystem).UserName
    if ([string]::IsNullOrWhiteSpace($user)) {
        throw "Windows has no interactive user for the proof task."
    }
    $user
}

function Start-ProofTask(
    [string] $taskName,
    [string] $program,
    [string] $arguments
) {
    $action = New-ScheduledTaskAction -Execute $program -Argument $arguments
    $principal = New-ScheduledTaskPrincipal `
        -UserId (Get-InteractiveUser) `
        -LogonType Interactive `
        -RunLevel Limited
    Register-ScheduledTask `
        -TaskName $taskName `
        -Action $action `
        -Principal $principal `
        -Force | Out-Null
    Start-ScheduledTask -TaskName $taskName
    Write-Output "Started interactive task $taskName."
}

function Start-DisplayModeActivity {
    $directory = Split-Path -Parent $displayModeActivityScript
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    Set-Content -LiteralPath $displayModeActivityScript -Encoding ASCII -Value @'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
$form = [Windows.Forms.Form]::new()
$form.Text = "Rust Console display-mode proof activity"
$form.StartPosition = [Windows.Forms.FormStartPosition]::Manual
$form.Location = [Drawing.Point]::new(32, 32)
$form.Size = [Drawing.Size]::new(320, 180)
$form.TopMost = $true
$colors = @(
    [Drawing.Color]::FromArgb(255, 32, 32),
    [Drawing.Color]::FromArgb(32, 255, 32),
    [Drawing.Color]::FromArgb(32, 32, 255),
    [Drawing.Color]::FromArgb(255, 255, 32)
)
$index = 0
$deadline = [DateTime]::UtcNow.AddSeconds(240)
$timer = [Windows.Forms.Timer]::new()
$timer.Interval = 50
$timer.Add_Tick({
    $script:index = ($script:index + 1) % $colors.Count
    $form.BackColor = $colors[$script:index]
    if ([DateTime]::UtcNow -ge $deadline) {
        $form.Close()
    }
})
$form.Add_Shown({ $timer.Start() })
[Windows.Forms.Application]::Run($form)
$timer.Dispose()
$form.Dispose()
'@
    Start-ProofTask `
        $displayModeActivityTaskName `
        "powershell.exe" `
        "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$displayModeActivityScript`""
}

function Stop-DisplayModeActivity {
    if ($null -ne (Get-ScheduledTask -TaskName $displayModeActivityTaskName -ErrorAction SilentlyContinue)) {
        Stop-ScheduledTask -TaskName $displayModeActivityTaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask `
            -TaskName $displayModeActivityTaskName `
            -Confirm:$false `
            -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $displayModeActivityScript -Force -ErrorAction SilentlyContinue
}

function Initialize-DisplayModeApi {
    if ("RustConsole.DisplayModeApi" -as [type]) {
        return
    }

    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

namespace RustConsole {
    public static class DisplayModeApi {
        private const int EnumCurrentSettings = -1;
        private const int Success = 0;
        private const int DmBitsPerPel = 0x00040000;
        private const int DmPelsWidth = 0x00080000;
        private const int DmPelsHeight = 0x00100000;
        private const int DmDisplayFrequency = 0x00400000;

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        public struct Mode {
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)]
            public string DeviceName;
            public short SpecVersion;
            public short DriverVersion;
            public short Size;
            public short DriverExtra;
            public int Fields;
            public int PositionX;
            public int PositionY;
            public int DisplayOrientation;
            public int DisplayFixedOutput;
            public short Color;
            public short Duplex;
            public short YResolution;
            public short TTOption;
            public short Collate;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)]
            public string FormName;
            public short LogPixels;
            public int BitsPerPel;
            public int PelsWidth;
            public int PelsHeight;
            public int DisplayFlags;
            public int DisplayFrequency;
            public int ICMMethod;
            public int ICMIntent;
            public int MediaType;
            public int DitherType;
            public int Reserved1;
            public int Reserved2;
            public int PanningWidth;
            public int PanningHeight;
        }

        [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "EnumDisplaySettingsW")]
        private static extern bool EnumDisplaySettings(
            string deviceName,
            int modeNumber,
            ref Mode mode
        );

        [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "ChangeDisplaySettingsExW")]
        private static extern int ChangeDisplaySettingsEx(
            string deviceName,
            ref Mode mode,
            IntPtr window,
            int flags,
            IntPtr parameter
        );

        private static Mode EmptyMode() {
            Mode mode = new Mode();
            mode.DeviceName = String.Empty;
            mode.FormName = String.Empty;
            mode.Size = (short)Marshal.SizeOf(typeof(Mode));
            return mode;
        }

        public static Mode Current() {
            Mode mode = EmptyMode();
            if (!EnumDisplaySettings(null, EnumCurrentSettings, ref mode)) {
                throw new InvalidOperationException("Cannot read the current default display mode.");
            }
            return mode;
        }

        public static Mode Alternative(Mode current) {
            bool found = false;
            long bestScore = long.MaxValue;
            Mode best = EmptyMode();
            for (int index = 0; ; index++) {
                Mode candidate = EmptyMode();
                if (!EnumDisplaySettings(null, index, ref candidate)) {
                    break;
                }
                if (candidate.BitsPerPel != current.BitsPerPel ||
                    candidate.DisplayFrequency <= 0 ||
                    (candidate.PelsWidth == current.PelsWidth &&
                     candidate.PelsHeight == current.PelsHeight &&
                     candidate.DisplayFrequency == current.DisplayFrequency)) {
                    continue;
                }
                candidate.Fields |= DmBitsPerPel | DmPelsWidth | DmPelsHeight | DmDisplayFrequency;
                bool sameDimensions = candidate.PelsWidth == current.PelsWidth &&
                    candidate.PelsHeight == current.PelsHeight;
                long dimensionDifference = Math.Abs((long)candidate.PelsWidth - current.PelsWidth) +
                    Math.Abs((long)candidate.PelsHeight - current.PelsHeight);
                long frequencyDifference = Math.Abs((long)candidate.DisplayFrequency - current.DisplayFrequency);
                long score = (sameDimensions ? 0L : 1000000000L) +
                    dimensionDifference * 1000L + frequencyDifference;
                if (score < bestScore) {
                    found = true;
                    bestScore = score;
                    best = candidate;
                }
            }
            if (!found) {
                throw new InvalidOperationException("No different tested default display mode is available.");
            }
            return best;
        }

        public static void Apply(Mode mode) {
            mode.Fields |= DmBitsPerPel | DmPelsWidth | DmPelsHeight | DmDisplayFrequency;
            int result = ChangeDisplaySettingsEx(null, ref mode, IntPtr.Zero, 0, IntPtr.Zero);
            if (result != Success) {
                throw new InvalidOperationException("Display mode change failed with result " + result + ".");
            }
        }

        public static string Describe(Mode mode) {
            return mode.PelsWidth + "x" + mode.PelsHeight + "@" + mode.DisplayFrequency +
                " " + mode.BitsPerPel + "bpp";
        }
    }
}
'@
}

function Wait-ProofReport(
    [string] $ExpectedStatus,
    [string] $ExpectedPhase,
    [int] $TimeoutSeconds
) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $displayModeReport) {
            try {
                $report = Get-Content -LiteralPath $displayModeReport -Raw
                $hasStatus = $report -match "(?m)^status=$([regex]::Escape($ExpectedStatus))$"
                $hasPhase = [string]::IsNullOrEmpty($ExpectedPhase) -or
                    $report -match "(?m)^phase=$([regex]::Escape($ExpectedPhase))$"
                if ($hasStatus -and $hasPhase) {
                    return $report
                }
                if ($report -match "(?m)^status=error$") {
                    throw "Display-mode proof failed:`n$report"
                }
            }
            catch [System.IO.IOException] {
            }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Timed out waiting for display-mode proof status=$ExpectedStatus phase=$ExpectedPhase."
}

function Get-ProofValue([string] $Report, [string] $Name) {
    $line = @($Report -split "`r?`n" | Where-Object { $_.StartsWith("$Name=") })
    if ($line.Count -ne 1) {
        throw "Display-mode proof report has no unique $Name value."
    }
    $line[0].Substring($Name.Length + 1)
}

function Invoke-DisplayModeProof {
    Initialize-DisplayModeApi
    Remove-Item -LiteralPath $displayModeReport -Force -ErrorAction SilentlyContinue
    Start-DisplayModeActivity
    try {
        Start-Service -Name $serviceName
        $initialReport = Wait-ProofReport "running" "initial" 30
        $display = Get-ProofValue $initialReport "display"
        $original = [RustConsole.DisplayModeApi]::Current()
        $alternative = [RustConsole.DisplayModeApi]::Alternative($original)
        Write-Output "Changing $display from $([RustConsole.DisplayModeApi]::Describe($original)) to $([RustConsole.DisplayModeApi]::Describe($alternative))."
        try {
            [RustConsole.DisplayModeApi]::Apply($alternative)
            $null = Wait-ProofReport "running" "changed" 60
        }
        finally {
            [RustConsole.DisplayModeApi]::Apply($original)
            Write-Output "Restored $display to $([RustConsole.DisplayModeApi]::Describe($original))."
        }
        $finalReport = Wait-ProofReport "ok" "" 60
        Write-Output $finalReport
    }
    finally {
        Stop-ProofService
        Stop-DisplayModeActivity
    }
}

function Start-DisplayModeRunner {
    Remove-Item -LiteralPath $displayModeRunnerResult -Force -ErrorAction SilentlyContinue
    $arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" run-display-mode-interactive -Executable `"$Executable`""
    $action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $arguments
    $principal = New-ScheduledTaskPrincipal `
        -UserId (Get-InteractiveUser) `
        -LogonType Interactive `
        -RunLevel Highest
    Register-ScheduledTask `
        -TaskName $displayModeRunnerTaskName `
        -Action $action `
        -Principal $principal `
        -Force | Out-Null
    Start-ScheduledTask -TaskName $displayModeRunnerTaskName

    $deadline = [DateTime]::UtcNow.AddSeconds(210)
    try {
        while ([DateTime]::UtcNow -lt $deadline) {
            if (Test-Path -LiteralPath $displayModeRunnerResult) {
                $result = Get-Content -LiteralPath $displayModeRunnerResult -Raw
                if ($result -match "(?m)^status=ok$") {
                    Write-Output $result
                    return
                }
                if ($result -match "(?m)^status=error$") {
                    throw "Interactive display-mode runner failed:`n$result"
                }
            }
            Start-Sleep -Milliseconds 100
        }
        throw "Timed out waiting for the interactive display-mode runner."
    }
    finally {
        $task = Get-ScheduledTask -TaskName $displayModeRunnerTaskName -ErrorAction SilentlyContinue
        if ($null -ne $task -and $task.State -ne "Running") {
            Unregister-ScheduledTask `
                -TaskName $displayModeRunnerTaskName `
                -Confirm:$false `
                -ErrorAction SilentlyContinue
        }
    }
}

function Show-ProofStatus {
    $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        Write-Output "$serviceName is not installed."
    }
    else {
        $service | Select-Object Name, Status, StartType
    }
    Get-Process rustconsole-host -ErrorAction SilentlyContinue |
        Select-Object Id, SessionId, StartTime
    foreach ($report in $roundTripReport, $loginReport, $displayModeReport) {
        if (Test-Path -LiteralPath $report) {
            Write-Output "Report: $report"
        }
    }
}

Assert-Administrator

switch ($Action) {
    "prepare-round-trip" {
        Set-ProofService "desktop-transition-proof"
    }
    "prepare-login" {
        Set-ProofService "login-transition-proof"
    }
    "prepare-display-mode" {
        Set-ProofService "display-mode-transition-proof"
    }
    "start" {
        Remove-Item -LiteralPath $roundTripReport, $loginReport, $displayModeReport -Force -ErrorAction SilentlyContinue
        Start-Service -Name $serviceName
        Write-Output "Started $serviceName."
    }
    "run-display-mode" {
        Start-DisplayModeRunner
    }
    "run-display-mode-interactive" {
        try {
            $output = Invoke-DisplayModeProof | Out-String
            Set-Content `
                -LiteralPath $displayModeRunnerResult `
                -Encoding ASCII `
                -Value "status=ok`n$output"
        }
        catch {
            Set-Content `
                -LiteralPath $displayModeRunnerResult `
                -Encoding ASCII `
                -Value "status=error`nerror=$($_.Exception.Message)"
            throw
        }
    }
    "trigger-uac" {
        Start-ProofTask `
            $uacTaskName `
            "powershell.exe" `
            "-NoProfile -Command Start-Process cmd.exe -Verb RunAs"
    }
    "trigger-lock" {
        Start-ProofTask $lockTaskName "rundll32.exe" "user32.dll,LockWorkStation"
    }
    "status" {
        Show-ProofStatus
    }
    "report" {
        $reports = @($roundTripReport, $loginReport, $displayModeReport) | Where-Object { Test-Path -LiteralPath $_ }
        if ($reports.Count -eq 0) {
            throw "No desktop-transition proof report exists."
        }
        foreach ($report in $reports) {
            Write-Output "[$report]"
            Get-Content -LiteralPath $report
        }
    }
    "reboot" {
        $service = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
        if ($null -eq $service -or $service.StartMode -ne "Auto" -or
            $service.PathName -notlike "*login-transition-proof*") {
            throw "Prepare the automatic login-transition proof before rebooting."
        }
        Restart-Computer -Force
    }
    "cleanup" {
        Stop-ProofService
        Unregister-ScheduledTask -TaskName $uacTaskName -Confirm:$false -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $lockTaskName -Confirm:$false -ErrorAction SilentlyContinue
        Stop-DisplayModeActivity
        Stop-ScheduledTask -TaskName $displayModeRunnerTaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $displayModeRunnerTaskName -Confirm:$false -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $roundTripReport, $loginReport, $displayModeReport -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $displayModeRunnerResult -Force -ErrorAction SilentlyContinue
        if ($null -ne (Get-Service -Name $serviceName -ErrorAction SilentlyContinue)) {
            & sc.exe delete $serviceName
            if ($LASTEXITCODE -ne 0) {
                throw "Failed to delete $serviceName."
            }
        }
        Write-Output "Removed the proof service, tasks, and reports."
    }
}
