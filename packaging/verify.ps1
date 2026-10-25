$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($PSScriptRoot)
$separator = [IO.Path]::DirectorySeparatorChar
$prefix = $root.TrimEnd($separator) + $separator
$count = 0
foreach ($line in Get-Content -LiteralPath (Join-Path $root 'SHA256SUMS')) {
    if ($line -notmatch '^([0-9a-f]{64})  (.+)$') { throw 'Invalid checksum record' }
    $expected = $Matches[1]
    $relative = $Matches[2]
    if (($relative -split '/') -contains '..' -or $relative.Contains('\')) { throw 'Invalid package path' }
    $path = [IO.Path]::GetFullPath((Join-Path $root $relative))
    if (-not $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Package path escapes its directory' }
    $item = Get-Item -LiteralPath $path -Force
    if ($item.PSIsContainer) { throw 'Expected a package file' }
    while ($item.FullName -ne $root) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Package links are not supported' }
        $item = Get-Item -LiteralPath (Split-Path -Parent $item.FullName) -Force
    }
    $algorithm = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::OpenRead($path)
    try { $actual = [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
    finally { $stream.Dispose(); $algorithm.Dispose() }
    if ($actual -ne $expected) { throw "Checksum mismatch: $relative" }
    $count++
}
Write-Output "Verified $count package files."
