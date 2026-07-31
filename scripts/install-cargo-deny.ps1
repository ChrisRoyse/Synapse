<#
.SYNOPSIS
  Installs the pinned `cargo-deny` used by scripts/lint.ps1 gate 5. Issue #1930.

.DESCRIPTION
  #1930's finding was that `deny.toml` declared a real policy, two docs described
  it as an active gate, and nothing had ever run it — because the binary was not
  installed anywhere. "Install it by hand once" would leave the next machine in
  exactly the state this issue was filed about, so the install is a script.

  Downloads the official release archive from GitHub, VERIFIES ITS SHA-256
  against the `.sha256` published beside it, and only then extracts the binary
  into the cargo bin directory. An unverified supply-chain tool is a strange
  thing to gate a supply-chain check with, so the hash check is not optional and
  a mismatch aborts before anything is written.

  `cargo install cargo-deny --locked` is the other option and is not used here:
  it builds ~250 crates from source (minutes), and on this host that is paid
  again on every fresh checkout for a tool whose output is identical either way.

.PARAMETER Version
  cargo-deny release to install. Defaults to the version gate 5 requires.

.PARAMETER Force
  Reinstall even when the requested version is already present.

.EXAMPLE
  pwsh -File scripts/install-cargo-deny.ps1
#>
[CmdletBinding()]
param(
    [string]$Version = '0.20.2',
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Target = 'x86_64-pc-windows-msvc'
$Asset = "cargo-deny-$Version-$Target.tar.gz"
$BaseUrl = "https://github.com/EmbarkStudios/cargo-deny/releases/download/$Version"

$CargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
$Installed = Join-Path $CargoBin 'cargo-deny.exe'

if ((Test-Path -LiteralPath $Installed) -and -not $Force) {
    $current = (& cargo deny --version 2>&1 | Out-String).Trim()
    if ($current -eq "cargo-deny $Version") {
        Write-Host "cargo-deny $Version already installed at $Installed" -ForegroundColor Green
        exit 0
    }
    Write-Host "found '$current', want 'cargo-deny $Version' — replacing" -ForegroundColor Yellow
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("cargo-deny-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null

try {
    $archive = Join-Path $work $Asset
    $shaFile = "$archive.sha256"

    Write-Host "-> $BaseUrl/$Asset"
    Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $archive -UseBasicParsing
    Invoke-WebRequest -Uri "$BaseUrl/$Asset.sha256" -OutFile $shaFile -UseBasicParsing

    # The published .sha256 is upper-case hex; Get-FileHash also returns
    # upper-case, but normalise both rather than depend on that.
    $expected = ((Get-Content -LiteralPath $shaFile -Raw) -replace '\s', '').ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "SYNAPSE_CARGO_DENY_SHA256_MISMATCH: $Asset expected $expected but downloaded $actual. Nothing was installed."
    }
    Write-Host "   sha256 OK $actual" -ForegroundColor Green

    tar -xzf $archive -C $work
    if ($LASTEXITCODE -ne 0) { throw "SYNAPSE_CARGO_DENY_EXTRACT_FAILED: tar exited $LASTEXITCODE" }

    $exe = Get-ChildItem -Path $work -Recurse -Filter 'cargo-deny.exe' | Select-Object -First 1
    if ($null -eq $exe) { throw "SYNAPSE_CARGO_DENY_BINARY_ABSENT: no cargo-deny.exe inside $Asset" }

    if (-not (Test-Path -LiteralPath $CargoBin)) {
        throw "SYNAPSE_CARGO_BIN_MISSING: $CargoBin does not exist; is the Rust toolchain installed for this user?"
    }
    Copy-Item -LiteralPath $exe.FullName -Destination $Installed -Force

    $check = (& cargo deny --version 2>&1 | Out-String).Trim()
    if ($check -ne "cargo-deny $Version") {
        throw "SYNAPSE_CARGO_DENY_VERIFY_FAILED: installed to $Installed but 'cargo deny --version' reports '$check'. Another cargo-deny may shadow it earlier on PATH."
    }
    Write-Host "installed $check -> $Installed" -ForegroundColor Green
    Write-Host "   binary sha256 $((Get-FileHash -LiteralPath $Installed -Algorithm SHA256).Hash.ToLowerInvariant())"
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
