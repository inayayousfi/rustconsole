param(
    [string] $Executable = (Join-Path $PSScriptRoot '..\target\release\rustconsole-host.exe'),
    [switch] $PlaySignal,
    [switch] $EncodeOpus
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($PlaySignal) {
    # Generate a quiet five-second stereo test signal in memory, never a recording.
    Start-Sleep -Seconds 2
    $stream = [IO.MemoryStream]::new()
    $writer = [IO.BinaryWriter]::new($stream)
    $player = $null
    try {
        $frames = 48000 * 5
        $dataBytes = $frames * 4
        $writer.Write([Text.Encoding]::ASCII.GetBytes('RIFF'))
        $writer.Write([int]($dataBytes + 36))
        $writer.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
        $writer.Write([int]16)
        $writer.Write([int16]1)
        $writer.Write([int16]2)
        $writer.Write([int]48000)
        $writer.Write([int]192000)
        $writer.Write([int16]4)
        $writer.Write([int16]16)
        $writer.Write([Text.Encoding]::ASCII.GetBytes('data'))
        $writer.Write([int]$dataBytes)
        for ($i = 0; $i -lt $frames; $i++) {
            $ramp = [Math]::Min(1.0, [Math]::Min($i, $frames - 1 - $i) / 480.0)
            $writer.Write([int16](3276 * $ramp * [Math]::Sin(2 * [Math]::PI * 440 * $i / 48000)))
            $writer.Write([int16](3276 * $ramp * [Math]::Sin(2 * [Math]::PI * 660 * $i / 48000)))
        }
        $writer.Flush()
        $stream.Position = 0
        $player = [Media.SoundPlayer]::new($stream)
        $player.PlaySync()
    }
    finally {
        if ($null -ne $player) { $player.Dispose() }
        $writer.Dispose()
        $stream.Dispose()
    }
    exit 0
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this proof from an administrator PowerShell session.'
}
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$serviceName = 'RustConsoleHostDev'
$taskName = 'RustConsoleAudioProofSignal'
$proofCommand = if ($EncodeOpus) { 'audio-encode-proof' } else { 'audio-proof' }
$reportPath = "C:\ProgramData\RustConsole\$proofCommand.txt"
$recordPath = 'C:\ProgramData\RustConsole\opaque-record.bin'
$routeRecordPath = 'C:\ProgramData\RustConsole\audio-route-recovery.txt'
$cableDriver = Get-CimInstance Win32_SystemDriver -Filter "Name='VBAudioVACMME'"
if ($null -eq $cableDriver -or $cableDriver.State -ne 'Running') {
    throw 'VB-CABLE Standard is not installed and running.'
}
if (Test-Path -LiteralPath $routeRecordPath) {
    throw "Existing audio route recovery state must be handled first: $routeRecordPath"
}
$saved = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
if ($null -eq $saved -or $saved.State -ne 'Running' -or $saved.StartName -ne 'LocalSystem') {
    throw 'The proof requires the existing running LocalSystem host service.'
}
if (Test-Path -LiteralPath $reportPath) { throw "Existing proof report must be preserved: $reportPath" }
if ($null -ne (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue)) {
    throw "An existing task already owns $taskName."
}
$originalExecutable = (Get-CimInstance Win32_Process -Filter "ProcessId=$($saved.ProcessId)").ExecutablePath
if ($Executable -eq $originalExecutable) { throw 'Use a separate diagnostic build, not the installed executable.' }
$originalHash = (Get-FileHash -LiteralPath $originalExecutable -Algorithm SHA256).Hash
$recordHash = (Get-FileHash -LiteralPath $recordPath -Algorithm SHA256).Hash
$user = (Get-CimInstance Win32_ComputerSystem).UserName
if ([string]::IsNullOrWhiteSpace($user)) { throw 'No logged-in user is available to play the test signal.' }

function Stop-Host {
    $service = Get-Service -Name $serviceName
    if ($service.Status -ne 'Stopped') {
        $service.Stop()
        $service.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(15))
    }
}

$taskCreated = $false
$proofReport = $null
try {
    Stop-Host
    $result = Invoke-CimMethod -InputObject $saved -MethodName Change -Arguments @{
        PathName = "`"$Executable`" $proofCommand"
    }
    if ($result.ReturnValue -ne 0) { throw "Service configuration failed: $($result.ReturnValue)" }
    Start-Service -Name $serviceName
    (Get-Service -Name $serviceName).WaitForStatus([ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(15))
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$PSCommandPath`" -PlaySignal"
    $taskPrincipal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::FromSeconds(30)) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $taskName -Action $action -Principal $taskPrincipal -Settings $settings | Out-Null
    $taskCreated = $true
    Start-ScheduledTask -TaskName $taskName
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while (-not (Test-Path -LiteralPath $reportPath)) {
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Audio proof report timed out after 30 seconds.' }
        Start-Sleep -Milliseconds 100
    }
    # The service writes a small report once, after the worker has exited.
    Start-Sleep -Milliseconds 100
    $proofReport = Get-Content -LiteralPath $reportPath -Raw
    Write-Output $proofReport
    if ($proofReport -notmatch '(?m)^status=ok\r?$') { throw 'Audio capture proof did not capture enough non-silent audio.' }
    if ($proofReport -notmatch '(?m)^worker_shutdown_micros=\d+\r?$') { throw 'Worker shutdown evidence is missing.' }
    if ($proofReport -notmatch '(?m)^capture_mode=direct-vb-cable\r?$') { throw 'Direct VB-CABLE capture evidence is missing.' }
    if ($proofReport -notmatch '(?m)^routing_restored=true\r?$') { throw 'Audio routing restoration evidence is missing.' }
    if (Test-Path -LiteralPath $routeRecordPath) { throw 'Audio route recovery state remains after the proof.' }
    if ($EncodeOpus) {
        if ($proofReport -notmatch '(?m)^codec=libopus\r?$' -or $proofReport -notmatch '(?m)^encoded_packets=[1-9]\d*\r?$') {
            throw 'No libopus encoding evidence was returned.'
        }
        if ($proofReport -notmatch '(?m)^input_copied_bytes=0\r?$') {
            throw 'The live proof did not retain an entirely shared complete-frame input path.'
        }
    }
    $signal = Get-ScheduledTaskInfo -TaskName $taskName
    if ($signal.LastTaskResult -ne 0) { throw "Signal task failed or is unfinished: $($signal.LastTaskResult)" }
    Write-Output 'signal_task_result=0'
}
finally {
    # Restore the host even if the proof or signal task failed. Never replace its binary.
    $cleanupErrors = [Collections.Generic.List[string]]::new()
    try {
        Stop-Host
        $current = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
        $result = Invoke-CimMethod -InputObject $current -MethodName Change -Arguments @{ PathName = $saved.PathName }
        if ($result.ReturnValue -ne 0) { throw "Service restoration failed: $($result.ReturnValue)" }
        Start-Service -Name $serviceName
        (Get-Service -Name $serviceName).WaitForStatus([ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(15))
        $restored = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
        if ($restored.PathName -ne $saved.PathName -or $restored.StartMode -ne $saved.StartMode -or $restored.StartName -ne $saved.StartName) {
            throw 'Restored service configuration does not match the saved configuration.'
        }
        if ((Get-FileHash -LiteralPath $originalExecutable -Algorithm SHA256).Hash -ne $originalHash) { throw 'Installed executable changed.' }
        if ((Get-FileHash -LiteralPath $recordPath -Algorithm SHA256).Hash -ne $recordHash) { throw 'Password record changed.' }
        if (Test-Path -LiteralPath $routeRecordPath) { throw 'Audio route recovery state remains after service restoration.' }
        Write-Output "service_restored=Running`ninstalled_executable_unchanged=true`npassword_record_unchanged=true"
    }
    catch { $cleanupErrors.Add($_.ToString()) }
    if ($taskCreated) {
        try {
            Stop-ScheduledTask -TaskName $taskName
            Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
        }
        catch { $cleanupErrors.Add($_.ToString()) }
    }
    if ($cleanupErrors.Count -gt 0) { throw ($cleanupErrors -join "`n") }
}
