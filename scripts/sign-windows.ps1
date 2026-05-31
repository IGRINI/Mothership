param(
  [Parameter(Mandatory = $true)]
  [string] $Path
)

$ErrorActionPreference = "Stop"

function Require-Signing {
  return $env:MOTHERSHIP_REQUIRE_WINDOWS_SIGNING -eq "1"
}

function Resolve-SignTool {
  if ($env:MOTHERSHIP_SIGNTOOL) {
    return $env:MOTHERSHIP_SIGNTOOL
  }

  $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($command) {
    return $command.Source
  }

  return $null
}

if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
  throw "Signing target does not exist: $Path"
}

$signTool = Resolve-SignTool
$timestampUrl = if ($env:MOTHERSHIP_WINDOWS_TIMESTAMP_URL) {
  $env:MOTHERSHIP_WINDOWS_TIMESTAMP_URL
} else {
  "http://timestamp.digicert.com"
}

if (-not $signTool) {
  if (Require-Signing) {
    throw "signtool.exe was not found. Set MOTHERSHIP_SIGNTOOL or install Windows SDK signing tools."
  }
  Write-Host "windows signing skipped: signtool.exe not found"
  exit 0
}

if ($env:MOTHERSHIP_WINDOWS_CERT_THUMBPRINT) {
  & $signTool sign /fd SHA256 /tr $timestampUrl /td SHA256 /sha1 $env:MOTHERSHIP_WINDOWS_CERT_THUMBPRINT $Path
  if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE"
  }
  exit 0
}

if ($env:MOTHERSHIP_WINDOWS_CERT_PFX) {
  $args = @("sign", "/fd", "SHA256", "/tr", $timestampUrl, "/td", "SHA256", "/f", $env:MOTHERSHIP_WINDOWS_CERT_PFX)
  if ($env:MOTHERSHIP_WINDOWS_CERT_PASSWORD) {
    $args += @("/p", $env:MOTHERSHIP_WINDOWS_CERT_PASSWORD)
  }
  $args += $Path
  & $signTool @args
  if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE"
  }
  exit 0
}

if (Require-Signing) {
  throw "Windows signing is required but no certificate was configured. Set MOTHERSHIP_WINDOWS_CERT_THUMBPRINT or MOTHERSHIP_WINDOWS_CERT_PFX."
}

Write-Host "windows signing skipped: no certificate configured"
