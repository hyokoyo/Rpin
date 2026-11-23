param(
    [string]$Exe = "target\release\rpin.exe",
    [ValidateRange(1, 100)][int]$StartupRuns = 10,
    [ValidateRange(1, 3600)][int]$IdleSeconds = 30,
    [ValidateRange(0, 2000)][int]$FullscreenRuns = 0,
    [switch]$MessageLoop
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exePath = (Resolve-Path (Join-Path $root $Exe)).Path
if (Get-Process rpin -ErrorAction SilentlyContinue | Where-Object SessionId -eq (Get-Process -Id $PID).SessionId) {
    throw "Exit RPin before measuring its startup."
}
$run = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -ErrorAction SilentlyContinue
if ($run.RPin -and $run.RPin -ne ('"' + $exePath + '" --minimized')) {
    throw "Another RPin executable owns the autostart entry. Benchmark that executable to preserve the setting."
}
# Fresh profile for the first launch. Following runs reuse its configuration.
$profileDir = Join-Path $root ("target\bench\" + [guid]::NewGuid())
$logDir = Join-Path $profileDir "RPin\logs"
if ($FullscreenRuns -gt 0) {
    $configDir = Join-Path $profileDir "RPin"
    New-Item -ItemType Directory -Force $configDir | Out-Null
    @'
[capture]
copy_to_clipboard = true
notifications = false
auto_save = false
'@ | Set-Content -Encoding ascii (Join-Path $configDir "config.toml")
}
function Start-RPin([string]$Arguments) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $exePath
    $info.Arguments = $Arguments
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.WindowStyle = "Hidden"
    $info.EnvironmentVariables["APPDATA"] = $profileDir
    $info.EnvironmentVariables["LOCALAPPDATA"] = $profileDir
    [Diagnostics.Process]::Start($info)
}

function Measure-Fullscreen([Diagnostics.Process]$Primary) {
    for ($iteration = 1; $iteration -le $FullscreenRuns; $iteration++) {
        $request = Start-RPin "--capture-fullscreen"
        try {
            if (!$request.WaitForExit(5000) -or $request.ExitCode -ne 0) {
                throw "Could not send fullscreen request $iteration."
            }
        } finally {
            $request.Dispose()
        }
        $deadline = [Diagnostics.Stopwatch]::StartNew()
        do {
            if ($Primary.HasExited) { throw "RPin exited during fullscreen request $iteration." }
            $logs = Get-ChildItem $logDir -Filter "*.log"
            if (@($logs | Select-String -Pattern '(capture|clipboard) failed:').Count -gt 0) {
                throw "Fullscreen output failed. Logs: $logDir"
            }
            $completed = @($logs | Select-String -SimpleMatch "clipboard ready in ").Count
            if ($completed -ge $iteration) { break }
            Start-Sleep -Milliseconds 10
        } while ($deadline.Elapsed.TotalSeconds -lt 5)
        if ($completed -lt $iteration) {
            throw "Fullscreen output $iteration timed out. Logs: $logDir"
        }
        if (($iteration % 20) -eq 0 -or $iteration -eq $FullscreenRuns) {
            Write-Host "$iteration/$FullscreenRuns fullscreen captures complete"
            & (Join-Path $PSScriptRoot "soak.ps1") -ProcessId $Primary.Id -DurationSeconds 0 -LogDirectory $logDir
        }
    }
}

$checkpoint = if ($MessageLoop) { "message loop ready" } else { "tray ready" }
$startup = @()
for ($i = 0; $i -lt $StartupRuns; $i++) {
    $p = Start-RPin ""
    $ready = $false
    try {
        $deadline = [Diagnostics.Stopwatch]::StartNew()
        while ($deadline.Elapsed.TotalSeconds -lt 5) {
            if ($p.HasExited) { throw "RPin exited before startup, code $($p.ExitCode)" }
            $files = Get-ChildItem $logDir -Filter "*.log" -ErrorAction SilentlyContinue
            $line = $files | Select-String -Pattern ("$checkpoint pid=" + $p.Id + "$") | Select-Object -Last 1
            if ($line) {
                $readyTime = [datetime]::ParseExact($line.Line.Substring(0,23), "yyyy-MM-dd HH:mm:ss.fff", [Globalization.CultureInfo]::InvariantCulture)
                $startup += [math]::Round(($readyTime - $p.StartTime).TotalMilliseconds, 1)
                $ready = $true
                break
            }
            Start-Sleep -Milliseconds 10
        }
        if (!$ready) { throw "Timed out waiting for $checkpoint. Logs: $logDir" }
        if ($i -eq $StartupRuns - 1) {
            Start-Sleep -Seconds $IdleSeconds
            $idle = & (Join-Path $PSScriptRoot "soak.ps1") -ProcessId $p.Id -DurationSeconds 0 -LogDirectory $logDir
            if ($FullscreenRuns -gt 0) {
                Write-Host "Before fullscreen captures"
                $idle
                Measure-Fullscreen $p
            }
        }
    } finally {
        try {
            if ($ready -and !$p.HasExited) {
                $quit = Start-RPin "--exit"
                try {
                    if (!$quit.WaitForExit(3000) -or $quit.ExitCode -ne 0 -or !$p.WaitForExit(3000)) {
                        throw "RPin did not exit normally"
                    }
                } finally { $quit.Dispose() }
            }
        } finally {
            if (!$p.HasExited) { $p.Kill(); $p.WaitForExit() }
            $p.Dispose()
        }
    }
}

$sorted = @($startup | Sort-Object)
$mid = ($sorted.Count - 1) / 2
[pscustomobject]@{
    ExeBytes = (Get-Item $exePath).Length
    Checkpoint = $checkpoint
    StartupRuns = $startup.Count
    FirstLaunchMs = $startup[0]
    StartupMedianMs = ($sorted[[int][math]::Floor($mid)] + $sorted[[int][math]::Ceiling($mid)]) / 2
    StartupMinMs = $sorted[0]
    StartupMaxMs = $sorted[-1]
    IdleSeconds = $IdleSeconds
    IdlePrivateMiB = [math]::Round($idle.PrivateBytes / 1MB, 2)
    IdleWorkingSetMiB = [math]::Round($idle.WorkingSetBytes / 1MB, 2)
    IdleHandles = $idle.Handles
    IdleGdiObjects = $idle.GdiObjects
    IdleUserObjects = $idle.UserObjects
    IdleLogBytes = $idle.LogBytes
    FullscreenRuns = $FullscreenRuns
    LogDirectory = $logDir
}
