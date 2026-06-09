param(
    [switch]$Release,
    [switch]$NoRust,
    [switch]$NoCsharp
)

$ErrorActionPreference = "Stop"

$ProfileName = if ($Release) { "release" } else { "debug" }
$ConfigName = if ($Release) { "Release" } else { "Debug" }
$CargoArgs  = @("build")
if ($Release) { $CargoArgs += "--release" }
$CargoArgs += @("-p", "pdf_extractor_capi")
$RustDll    = "target\$ProfileName\pdf_extractor_capi.dll"

$Stopwatch = [System.Diagnostics.Stopwatch]::StartNew()

# ── 1. Build Rust ───────────────────────────────────────────────────────────
if (-not $NoRust) {
    Write-Host "=== Rust ($ProfileName) ===" -ForegroundColor Cyan

    if (Test-Path $RustDll) {
        $dllTime = (Get-Item $RustDll).LastWriteTime
        $rustSources = Get-ChildItem -Recurse -Include "*.rs", "Cargo.toml", "Cargo.lock" |
            Where-Object { -not $_.FullName.Contains("\target\") }
        $newestSrc = ($rustSources | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime

        if ($newestSrc -le $dllTime) {
            Write-Host "  Rust DLL is up to date, skipping cargo build" -ForegroundColor Green
        } else {
            Write-Host "  Rust sources changed, rebuilding..." -ForegroundColor Yellow
            & "cargo" @CargoArgs
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        }
    } else {
        Write-Host "  Building Rust..." -ForegroundColor Yellow
        & "cargo" @CargoArgs
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    }
}

# ── 2. Check whether C# build is needed ────────────────────────────────────
$csharpNeeded = $true
if (-not $NoCsharp) {
    $csprojDir = "PdfExplorer"
    $csharpOut = "PdfExplorer\bin\$ConfigName\net8.0-windows10.0.17763.0\PdfExplorer.exe"
    $csharpSources = Get-ChildItem -Recurse -Include "*.cs", "*.xaml", "*.csproj", "*.slnx" -Path $csprojDir

    if ((Test-Path $csharpOut) -and (Test-Path $RustDll)) {
        $outTime = (Get-Item $csharpOut).LastWriteTime
        $dllTime = (Get-Item $RustDll).LastWriteTime
        $newestSource = ($csharpSources | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime

        if ($newestSource -le $outTime -and $dllTime -le $outTime) {
            $csharpNeeded = $false
            Write-Host "  C# is up to date, skipping dotnet build" -ForegroundColor Green
        } else {
            Write-Host "  C# sources or native DLL changed, building..." -ForegroundColor Yellow
        }
    } else {
        Write-Host "  Building C#..." -ForegroundColor Yellow
    }
} else {
    $csharpNeeded = $false
}

# ── 3. Build C# ────────────────────────────────────────────────────────────
if ($csharpNeeded) {
    Write-Host "=== C# ($ConfigName) ===" -ForegroundColor Cyan

    dotnet restore PdfExplorer\PdfExplorer.csproj
    if ($LASTEXITCODE -ne 0) { throw "dotnet restore failed" }

    dotnet build PdfExplorer\PdfExplorer.csproj --no-restore -c $ConfigName
    if ($LASTEXITCODE -ne 0) { throw "dotnet build failed" }
}

# ── 4. Summary ─────────────────────────────────────────────────────────────
$elapsed = $Stopwatch.Elapsed
$total = $elapsed.TotalSeconds.ToString('0.0')
Write-Host "=== Done (${total}s) ===" -ForegroundColor Cyan
