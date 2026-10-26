param(
    [Parameter(Mandatory = $true)][int]$RootProcessId,
    [Parameter(Mandatory = $true)][double]$Seconds,
    [int]$IntervalMs = 1000
)
$ErrorActionPreference = 'Stop'
if ($Seconds -lt 2 -or $Seconds -gt 300 -or $IntervalMs -lt 200) { throw 'Invalid sampling window' }
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class KVMHelmBenchmarkClock {
    [StructLayout(LayoutKind.Sequential)] public struct FileTime {
        public uint Low, High;
        public long Ticks { get { return ((long)High << 32) | Low; } }
    }
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool GetSystemTimes(out FileTime idle, out FileTime kernel, out FileTime user);
    public static long[] Read() {
        FileTime idle, kernel, user;
        if (!GetSystemTimes(out idle, out kernel, out user)) throw new System.ComponentModel.Win32Exception();
        return new long[] { idle.Ticks, kernel.Ticks + user.Ticks };
    }
}
'@
$sampleClock = [Diagnostics.Stopwatch]::StartNew()
$cpuLast = @{}
$cpuObservedMs = 0.0
$nextSampleMs = 0.0
while ($true) {
    # Read only IDs/ancestry/names; never collect process command lines or env.
    $all = @(Get-CimInstance Win32_Process -Property ProcessId,ParentProcessId,Name)
    $selected = [Collections.Generic.HashSet[int]]::new()
    [void]$selected.Add($RootProcessId)
    do {
        $changed = $false
        foreach ($item in $all) {
            if ($selected.Contains([int]$item.ParentProcessId) -and $selected.Add([int]$item.ProcessId)) { $changed = $true }
        }
    } while ($changed)
    $processes = @()
    $resident = 0L
    $private = 0L
    $rootFound = $false
    foreach ($item in $all) {
        $candidateProcessId = [int]$item.ProcessId
        if (-not $selected.Contains($candidateProcessId)) { continue }
        try {
            $process = Get-Process -Id $candidateProcessId -ErrorAction Stop
            $identity = "$candidateProcessId/$($process.StartTime.Ticks)"
            $currentCpuMs = $process.TotalProcessorTime.TotalMilliseconds
            if ($cpuLast.ContainsKey($identity)) { $cpuObservedMs += [Math]::Max(0.0, $currentCpuMs - $cpuLast[$identity]) }
            $cpuLast[$identity] = $currentCpuMs
            $resident += $process.WorkingSet64
            $private += $process.PrivateMemorySize64
            $processes += @{ name=$process.ProcessName; resident_bytes=$process.WorkingSet64; private_bytes=$process.PrivateMemorySize64 }
            if ($candidateProcessId -eq $RootProcessId) { $rootFound = $true }
            $process.Dispose()
        } catch {
            if ($candidateProcessId -eq $RootProcessId) { throw }
        }
    }
    if (-not $rootFound) { throw 'Measured daemon exited' }
    $system = [KVMHelmBenchmarkClock]::Read()
    @{ kind='sample'; elapsed_ms=$sampleClock.Elapsed.TotalMilliseconds; cpu_observed_ms=$cpuObservedMs;
       resident_bytes=$resident; private_bytes=$private; system_idle_ticks=$system[0]; system_total_ticks=$system[1];
       processes=$processes } | ConvertTo-Json -Compress -Depth 4
    if ($sampleClock.Elapsed.TotalSeconds -ge $Seconds) { break }
    $nextSampleMs += $IntervalMs
    $remainingMs = $nextSampleMs - $sampleClock.Elapsed.TotalMilliseconds
    if ($remainingMs -gt 0) { Start-Sleep -Milliseconds ([int]$remainingMs) }
}
