# Build the Windows release: one file, dist\DiShus-<version>-windows-x86_64.exe
#
# It is dishus.exe with dishus-daemon.exe built in. Double-clicking it installs DiShus for the
# current user (no administrator rights), starts it, and shows the pairing QR code.
# Needs Rust (MSVC) and the Visual Studio Build Tools ("Desktop development with C++").
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Set-Location $root
$ver = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value

cargo build --release --locked -p dishus-daemon
if ($LASTEXITCODE -ne 0) { throw 'building dishus-daemon failed' }
$env:DISHUS_EMBED_DAEMON = Join-Path $root 'target\release\dishus-daemon.exe'
try {
    cargo build --release --locked -p dishus-cli
    if ($LASTEXITCODE -ne 0) { throw 'building dishus-cli failed' }
} finally {
    Remove-Item Env:DISHUS_EMBED_DAEMON
}

$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$out = Join-Path $dist "DiShus-$ver-windows-x86_64.exe"
Copy-Item (Join-Path $root 'target\release\dishus.exe') $out -Force
$hash = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
"$hash  $(Split-Path $out -Leaf)" | Set-Content -Encoding ascii (Join-Path $dist 'SHA256SUMS-windows')
Write-Host "Built $out"
Write-Host "SHA-256 $hash"
