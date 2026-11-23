param(
    [Parameter(Mandatory = $true)][int]$ProcessId,
    [ValidateRange(0, 86400)][int]$DurationSeconds = 28800,
    [ValidateRange(1, 3600)][int]$IntervalSeconds = 60,
    [string]$LogDirectory = (Join-Path $env:LOCALAPPDATA 'RPin\logs')
)

$ErrorActionPreference = 'Stop'
if (-not ('RPinDiagnostics.GuiResources' -as [type])) {
    Add-Type -Namespace RPinDiagnostics -Name GuiResources -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true)]
public static extern uint GetGuiResources(System.IntPtr process, uint flags);
[System.Runtime.InteropServices.DllImport("kernel32.dll")]
public static extern void SetLastError(uint error);
'@
}
function Get-GuiCount([IntPtr]$ProcessHandle, [uint32]$Flags) {
    [RPinDiagnostics.GuiResources]::SetLastError(0)
    $count = [RPinDiagnostics.GuiResources]::GetGuiResources($ProcessHandle, $Flags)
    $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    if ($count -eq 0 -and $errorCode -ne 0) {
        throw "GetGuiResources($Flags) failed with Win32 error $errorCode"
    }
    $count
}

$process = Get-Process -Id $ProcessId
if ($process.ProcessName -ne 'rpin') {
    throw "Process $ProcessId is not RPin."
}
$clock = [Diagnostics.Stopwatch]::StartNew()
try {
    do {
        if ($process.HasExited) {
            throw "RPin exited after $($clock.Elapsed.TotalSeconds) seconds."
        }
        $process.Refresh()
        $elapsed = $clock.Elapsed.TotalSeconds
        $logs = Get-ChildItem -LiteralPath $LogDirectory -File -ErrorAction SilentlyContinue |
            Where-Object Name -Match '^rpin-[0-9]{8}\.log(\.[123])?$' |
            Measure-Object Length -Sum
        [pscustomobject]@{
            Timestamp = [DateTime]::Now.ToString('o')
            Seconds = [math]::Round($elapsed, 2)
            PrivateBytes = $process.PrivateMemorySize64
            WorkingSetBytes = $process.WorkingSet64
            Handles = $process.HandleCount
            GdiObjects = Get-GuiCount $process.Handle 0
            UserObjects = Get-GuiCount $process.Handle 1
            LogBytes = [long]$logs.Sum
        }
        $remaining = $DurationSeconds - $elapsed
        if ($remaining -gt 0) {
            Start-Sleep -Milliseconds ([int]([math]::Min($IntervalSeconds, $remaining) * 1000))
        }
    } while ($remaining -gt 0)
} finally {
    $process.Dispose()
}
