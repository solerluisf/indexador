param(
    [switch]$Release,
    [switch]$NoRust,
    [switch]$NoCsharp
)

$ErrorActionPreference = "Stop"
$RepoRoot = $PSScriptRoot

$ProfileName = if ($Release) { "release" } else { "debug" }
$ConfigName  = if ($Release) { "Release" } else { "Debug" }
$CargoArgs   = @("build")
if ($Release) { $CargoArgs += "--release" }
$CargoArgs  += @("-p", "pdf_extractor_capi", "-p", "pdf_extractor")
$RustDll     = "$RepoRoot\target\$ProfileName\pdf_extractor_capi.dll"

$Stopwatch = [System.Diagnostics.Stopwatch]::StartNew()

# ── 1. Build Rust ───────────────────────────────────────────────────────────
if (-not $NoRust) {
    Write-Host "=== Rust ($ProfileName) ===" -ForegroundColor Cyan

    $rustSources = Get-ChildItem -Path $RepoRoot -Recurse -Include "*.rs", "Cargo.toml", "Cargo.lock" |
        Where-Object { -not $_.FullName.Contains("\target\") }
    $newestSrc = ($rustSources | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime

    $rustNeeded = $true
    if ((Test-Path $RustDll) -and $newestSrc) {
        $dllTime = (Get-Item $RustDll).LastWriteTime
        if ($newestSrc -le $dllTime) {
            $rustNeeded = $false
            Write-Host "  Rust sources are up to date, skipping cargo build" -ForegroundColor Green
        }
    }

    if ($rustNeeded) {
        Write-Host "  Building Rust ($ProfileName)..." -ForegroundColor Yellow
        & "cargo" @CargoArgs --manifest-path "$RepoRoot\Cargo.toml"
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    }
}

# ── 2. Check whether C# build is needed ────────────────────────────────────
$csharpNeeded = $true
if (-not $NoCsharp) {
    $csharpOut = "$RepoRoot\PdfExplorer\bin\$ConfigName\net8.0-windows10.0.17763.0\PdfExplorer.exe"
    $csharpSources = Get-ChildItem -Path "$RepoRoot\PdfExplorer" -Recurse -Include "*.cs", "*.xaml", "*.csproj", "*.slnx"

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

    dotnet restore "$RepoRoot\PdfExplorer\PdfExplorer.csproj"
    if ($LASTEXITCODE -ne 0) { throw "dotnet restore failed" }

    dotnet build "$RepoRoot\PdfExplorer\PdfExplorer.csproj" --no-restore -c $ConfigName
    if ($LASTEXITCODE -ne 0) { throw "dotnet build failed" }
}

# ── 4. Ensure worker EXEs in C# output ─────────────────────────────────────
$csharpOutDir = "$RepoRoot\PdfExplorer\bin\$ConfigName\net8.0-windows10.0.17763.0"
if (-not $NoRust) {
    Write-Host "=== Copying worker binaries to C# output ===" -ForegroundColor Cyan
    foreach ($bin in @("pdf_worker.exe", "tesseract_worker.exe")) {
        $src = "$RepoRoot\target\$ProfileName\$bin"
        $dst = "$csharpOutDir\$bin"
        if (Test-Path $src) {
            Copy-Item -LiteralPath $src -Destination $dst -Force
            Write-Host "  Copied $bin" -ForegroundColor Green
        } else {
            Write-Host "  WARNING: $bin not found at $src" -ForegroundColor Yellow
        }
    }
}

# ── 5. Summary ─────────────────────────────────────────────────────────────
$elapsed = $Stopwatch.Elapsed
$total = $elapsed.TotalSeconds.ToString('0.0')
Write-Host "=== Done (${total}s) ===" -ForegroundColor Cyan
