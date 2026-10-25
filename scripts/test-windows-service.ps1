param([Parameter(Mandatory = $true)][string]$PackageDirectory)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or -not $env:RUNNER_TEMP) { throw 'This temporary SCM integration test is only for an ephemeral GitHub Actions runner.' }
$runnerRoot = [IO.Path]::GetFullPath($env:RUNNER_TEMP).TrimEnd('\') + '\'
$name = 'KVMHelmCI-' + [Guid]::NewGuid().ToString('N')
$testRoot = [IO.Path]::GetFullPath((Join-Path $runnerRoot $name))
if (-not $testRoot.StartsWith($runnerRoot, [StringComparison]::OrdinalIgnoreCase)) { throw 'Service test state must stay in RUNNER_TEMP.' }
$binary = Join-Path ([IO.Path]::GetFullPath($PackageDirectory)) 'kvmhelm.exe'
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw 'Extract a Windows native package first.' }
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$port = $listener.LocalEndpoint.Port
$listener.Stop()
$data = Join-Path $testRoot 'data'
$secrets = Join-Path $testRoot 'secrets'
$created = $false
try {
    # The isolated test starts with empty state, owned by LocalSystem. It never
    # opens a user installation, issues a PAT or enables a physical device.
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $path = '"' + $binary + '" service run --host 127.0.0.1 --port ' + $port + ' --data "' + $data + '" --secrets "' + $secrets + '"'
    New-Service -Name $name -BinaryPathName $path -StartupType Manual | Out-Null
    $created = $true
    foreach ($cycle in 1..2) {
        Start-Service -Name $name
        (Get-Service -Name $name).WaitForStatus('Running', [TimeSpan]::FromSeconds(45))
        $ready = Invoke-RestMethod -Uri "http://127.0.0.1:$port/readyz" -TimeoutSec 4
        if (-not $ready.ok) { throw 'SCM Running must mean the native listener/store is ready.' }
        $processId = (Get-CimInstance Win32_Service -Filter "Name='$name'").ProcessId
        if (-not $processId) { throw 'No service process' }
        Stop-Service -Name $name
        (Get-Service -Name $name).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(90))
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        while ((Get-Process -Id $processId -ErrorAction SilentlyContinue) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
        if (Get-Process -Id $processId -ErrorAction SilentlyContinue) { throw 'SCM Stopped was reported before process cleanup completed.' }
    }
    Write-Output 'PASS: isolated Windows SCM start/readiness/stop/restart and store reopening.'
} finally {
    if ($created) {
        Stop-Service -Name $name -ErrorAction SilentlyContinue
        & sc.exe delete $name | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Could not remove the isolated test service.' }
    }
    # Native state deliberately belongs to LocalSystem. Leave its private files
    # in this disposable runner's temp area rather than changing their ACLs.
}
