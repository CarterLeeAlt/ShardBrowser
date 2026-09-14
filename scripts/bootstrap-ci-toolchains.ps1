param(
    [switch]$Node,
    [switch]$Rust,
    [switch]$Python,
    [string]$RustToolchain = "1.98.1-x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"

if (-not ($Node -or $Rust -or $Python)) {
    throw "Specify at least one of -Node, -Rust, or -Python."
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$toolsRoot = Join-Path $repoRoot ".cache\tools"
$downloadsRoot = Join-Path $toolsRoot "downloads"
$nodeVersion = "22.14.0"
$nodeRoot = Join-Path $toolsRoot "node-v$nodeVersion-win-x64"
$pythonVersion = "3.12.8"
$pythonRoot = Join-Path $toolsRoot "python-$pythonVersion-embed-amd64"
$toolchain = $RustToolchain

function Assert-Sha256 {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Expected
    )

    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Expected.ToLowerInvariant()) {
        throw "SHA-256 mismatch for $Path. Expected $Expected, received $actual."
    }
}

function Get-VerifiedFile {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$Sha256
    )

    if (-not (Test-Path -LiteralPath $Destination)) {
        Invoke-WebRequest -Uri $Uri -OutFile $Destination
    }
    Assert-Sha256 -Path $Destination -Expected $Sha256
}

New-Item -ItemType Directory -Force -Path $toolsRoot, $downloadsRoot | Out-Null
$pathEntries = [System.Collections.Generic.List[string]]::new()

if ($Node) {
    $nodeArchive = Join-Path $downloadsRoot "node-v$nodeVersion-win-x64.zip"
    Get-VerifiedFile `
        -Uri "https://nodejs.org/dist/v$nodeVersion/node-v$nodeVersion-win-x64.zip" `
        -Destination $nodeArchive `
        -Sha256 "55b639295920b219bb2acbcfa00f90393a2789095b7323f79475c9f34795f217"
    if (-not (Test-Path -LiteralPath (Join-Path $nodeRoot "node.exe"))) {
        Expand-Archive -LiteralPath $nodeArchive -DestinationPath $toolsRoot -Force
    }
    $pathEntries.Add($nodeRoot)
}

if ($Rust) {
    $env:CARGO_HOME = Join-Path $toolsRoot "cargo"
    $env:RUSTUP_HOME = Join-Path $toolsRoot "rustup"
    $rustupArchive = Join-Path $downloadsRoot "rustup-init-1.28.2-x86_64-pc-windows-msvc.exe"
    Get-VerifiedFile `
        -Uri "https://static.rust-lang.org/rustup/archive/1.28.2/x86_64-pc-windows-msvc/rustup-init.exe" `
        -Destination $rustupArchive `
        -Sha256 "88d8258dcf6ae4f7a80c7d1088e1f36fa7025a1cfd1343731b4ee6f385121fc0"
    $rustup = Join-Path $env:CARGO_HOME "bin\rustup.exe"
    if (-not (Test-Path -LiteralPath $rustup)) {
        & $rustupArchive -y --no-modify-path --profile minimal --default-toolchain $toolchain
    }
    $rustBin = Join-Path $env:RUSTUP_HOME "toolchains\$toolchain\bin"
    if (-not (Test-Path -LiteralPath (Join-Path $rustBin "cargo.exe"))) {
        & $rustup toolchain install $toolchain --profile minimal
    }
    & $rustup component add rustfmt clippy --toolchain $toolchain

    $env:RUSTC = Join-Path $rustBin "rustc.exe"
    $env:RUSTUP_TOOLCHAIN = $toolchain
    $pathEntries.Add($rustBin)
    if ($env:GITHUB_ENV) {
        Add-Content -LiteralPath $env:GITHUB_ENV -Value "RUSTC=$env:RUSTC"
        Add-Content -LiteralPath $env:GITHUB_ENV -Value "RUSTUP_TOOLCHAIN=$env:RUSTUP_TOOLCHAIN"
    }
}

if ($Python) {
    $pythonArchive = Join-Path $downloadsRoot "python-$pythonVersion-embed-amd64.zip"
    Get-VerifiedFile `
        -Uri "https://www.python.org/ftp/python/$pythonVersion/python-$pythonVersion-embed-amd64.zip" `
        -Destination $pythonArchive `
        -Sha256 "8d3f33be9eb810f23c102f08475af2854e50484b8e4e06275e937be61ce3d2fb"
    if (-not (Test-Path -LiteralPath (Join-Path $pythonRoot "python.exe"))) {
        New-Item -ItemType Directory -Force -Path $pythonRoot | Out-Null
        Expand-Archive -LiteralPath $pythonArchive -DestinationPath $pythonRoot -Force
    }
    $pathEntries.Add($pythonRoot)
}

if ($pathEntries.Count -gt 0) {
    $env:PATH = "$(($pathEntries -join ';'));$env:PATH"
}
if ($env:GITHUB_PATH) {
    foreach ($pathEntry in $pathEntries) {
        Add-Content -LiteralPath $env:GITHUB_PATH -Value $pathEntry
    }
}
