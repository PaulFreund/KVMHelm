param(
  [Parameter(Mandatory = $true)][string]$LanHost,
  [Parameter(Mandatory = $true)][string]$Certificate,
  [Parameter(Mandatory = $true)][string]$Key,
  [int]$LanPort = 8766
)
$ErrorActionPreference = 'Stop'
$kvmRoot = Split-Path -Parent $PSScriptRoot
$kvmNode = (Get-Command node -ErrorAction Stop).Source
# Both listeners share device connections and control leases.
& $kvmNode "$kvmRoot/dist/server/cli.js" serve `
  --data "$kvmRoot/.kvmhelm/data" --secrets "$kvmRoot/.kvmhelm/secrets" `
  --lan-host $LanHost --lan-port $LanPort `
  --cert $Certificate --key $Key
