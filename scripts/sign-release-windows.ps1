$ErrorActionPreference = "Stop"

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$targets = @(
  (Join-Path $root "target-tauri-build\release\mothership-app.exe"),
  (Join-Path $root "target-tauri-build\release\bundle\msi\Mothership_0.1.0_x64_en-US.msi"),
  (Join-Path $root "target-tauri-build\release\bundle\nsis\Mothership_0.1.0_x64-setup.exe")
)

foreach ($target in $targets) {
  if (Test-Path -LiteralPath $target -PathType Leaf) {
    & (Join-Path $PSScriptRoot "sign-windows.ps1") $target
  } else {
    Write-Host "windows signing skipped missing target: $target"
  }
}
