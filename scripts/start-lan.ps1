param(
  [Parameter(Mandatory = $true)][string]$LanHost,
  [Parameter(Mandatory = $true)][string]$Certificate,
  [Parameter(Mandatory = $true)][string]$Key,
  [int]$LanPort = 8766,
  [string]$Binary,
  [string]$Data,
  [string]$Secrets
)
$ErrorActionPreference = 'Stop'
$kvmRoot = Split-Path -Parent $PSScriptRoot
if (-not $Data) { $Data = Join-Path $kvmRoot '.kvmhelm/data' }
if (-not $Secrets) { $Secrets = Join-Path $kvmRoot '.kvmhelm/secrets' }
if (-not $Binary) { $Binary = Join-Path $kvmRoot 'target/x86_64-pc-windows-msvc/release/kvmhelm.exe' }
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) { throw 'Build the native release or supply -Binary with an extracted package executable.' }
# Both listeners share device connections and control leases.
& $Binary serve `
  --data $Data --secrets $Secrets `
  --lan-host $LanHost --lan-port $LanPort `
  --cert $Certificate --key $Key
