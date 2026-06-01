param(
    [int]$PdfCount = 100,
    [int]$SearchIterations = 50,
    [string]$BinaryPath = "target\release\pdf_extractor.exe"
)

$ErrorActionPreference = "Stop"
$BenchDir = "$env:TEMP\pdf_extractor_bench_$(Get-Date -Format 'yyyyMMdd_HHmmss')"
$PdfDir = "$BenchDir\pdfs"
$IndexDir = "$BenchDir\index"
$DbPath = "$BenchDir\jobs.db"
$JsonlPath = "$BenchDir\documents.jsonl"
$LogPath = "$BenchDir\benchmark.log"

New-Item -ItemType Directory -Path $PdfDir -Force | Out-Null
Write-Host "=== pdf_extractor Benchmark ===" -ForegroundColor Cyan
Write-Host "Binary: $BinaryPath"
Write-Host "PDF count: $PdfCount"
Write-Host "Output: $BenchDir`n"

# ---------------------------------------------------------------
# 1. Generate test PDFs using bench_pdfs binary
# ---------------------------------------------------------------
Write-Host "=== Phase 1: Generating $PdfCount test PDFs ===" -ForegroundColor Yellow
$genStart = Get-Date

$benchBin = Join-Path (Split-Path $BinaryPath -Parent) "bench_pdfs.exe"
if (-not (Test-Path $benchBin)) {
    Write-Host "  bench_pdfs not found at $benchBin" -ForegroundColor Red
    Write-Host "  Run: cargo build --release --bin bench_pdfs" -ForegroundColor Yellow
    exit 1
}

$genOutput = & $benchBin --count $PdfCount --dir $PdfDir 2>&1
$genElapsed = (Get-Date) - $genStart
Write-Host "  Generated $PdfCount PDFs in $($genElapsed.TotalSeconds.ToString('0.0'))s`n"

# ---------------------------------------------------------------
# 2. Extraction benchmark
# ---------------------------------------------------------------
Write-Host "=== Phase 2: Extraction benchmark ===" -ForegroundColor Yellow
Remove-Item -Path $IndexDir -Recurse -ErrorAction SilentlyContinue
Remove-Item -Path $DbPath -ErrorAction SilentlyContinue
Remove-Item -Path $JsonlPath -ErrorAction SilentlyContinue

$extStart = Get-Date
$extOutput = & $BinaryPath extract -i $PdfDir -o $JsonlPath -d $DbPath -l $LogPath --index-path $IndexDir 2>&1
$extElapsed = (Get-Date) - $extStart

$docsProcessed = if ($extOutput -match 'docs_processed\s*=\s*(\d+)') { $Matches[1] } else { "?" }
$docsErrored = if ($extOutput -match 'docs_errored\s*=\s*(\d+)') { $Matches[1] } else { "?" }
$throughput = if ($extOutput -match 'avg_throughput\s*=\s*"([\d.]+)"') { $Matches[1] } else { "?" }

Write-Host "  Time: $($extElapsed.TotalSeconds.ToString('0.0'))s"
Write-Host "  Processed: $docsProcessed"
Write-Host "  Errored: $docsErrored"
Write-Host "  Throughput: $throughput docs/s`n"

# ---------------------------------------------------------------
# 3. Search benchmarks
# ---------------------------------------------------------------
$queryWords = @("quantum", "machine", "neural", "gradient", "optimization", "transformer", "reinforcement")
$rng = [Random]::new()

function Run-SearchBenchmark {
    param([string]$Label, [string[]]$ExtraArgs, [int]$Iterations)

    Write-Host "=== Phase: $Label search benchmark ===" -ForegroundColor Yellow
    $times = @()
    $totalStart = Get-Date
    for ($i = 0; $i -lt $Iterations; $i++) {
        $q = $queryWords[$rng.Next(0, $queryWords.Length)]
        $qStart = Get-Date
        $null = & $BinaryPath search --index-path $IndexDir @ExtraArgs $q 2>&1
        $qElapsed = (Get-Date) - $qStart
        $times += $qElapsed.TotalMilliseconds
    }
    $totalElapsed = (Get-Date) - $totalStart
    $times = $times | Sort-Object
    $avg = ($times | Measure-Object -Average).Average
    $p50 = $times[[math]::Floor($times.Count * 0.50)]
    $p95 = $times[[math]::Floor($times.Count * 0.95)]
    $p99 = $times[[math]::Floor($times.Count * 0.99)]

    Write-Host "  Iterations: $Iterations"
    Write-Host "  Total time: $($totalElapsed.TotalSeconds.ToString('0.0'))s"
    Write-Host "  Avg latency: $($avg.ToString('0.0'))ms"
    Write-Host "  P50 latency: $($p50.ToString('0.0'))ms"
    Write-Host "  P95 latency: $($p95.ToString('0.0'))ms"
    Write-Host "  P99 latency: $($p99.ToString('0.0'))ms`n"

    return @{ Label = $Label; Avg = $avg; P50 = $p50; P95 = $p95; P99 = $p99 }
}

$basicResult = Run-SearchBenchmark -Label "basic" -ExtraArgs @() -Iterations $SearchIterations

$fuzzyQueryWords = $queryWords | ForEach-Object {
    if ($_.Length -gt 3) { $_.Substring(0, $_.Length - 2) } else { $_ }
}
$fuzzyResult = Run-SearchBenchmark -Label "fuzzy" -ExtraArgs @("--fuzzy", "2") -Iterations $SearchIterations

$stemResult = Run-SearchBenchmark -Label "stem" -ExtraArgs @("--stem") -Iterations $SearchIterations

$fieldResult = Run-SearchBenchmark -Label "field" -ExtraArgs @("--field", "normalized_text") -Iterations $SearchIterations

# ---------------------------------------------------------------
# 7. Index stats
# ---------------------------------------------------------------
Write-Host "=== Index statistics ===" -ForegroundColor Yellow
$statsOutput = & $BinaryPath index-stats --index-path $IndexDir 2>&1
$statsOutput | ForEach-Object { Write-Host "  $_" }
Write-Host ""

# ---------------------------------------------------------------
# Summary
# ---------------------------------------------------------------
Write-Host "=== Summary ===" -ForegroundColor Green
Write-Host "PDF count         : $PdfCount"
Write-Host "Extraction time   : $($extElapsed.TotalSeconds.ToString('0.0'))s"
Write-Host "Extraction rate   : $throughput docs/s"
Write-Host ""
Write-Host "Search (basic)    : avg=$($basicResult.Avg.ToString('0.0'))ms  P50=$($basicResult.P50.ToString('0.0'))ms  P95=$($basicResult.P95.ToString('0.0'))ms"
Write-Host "Search (fuzzy)    : avg=$($fuzzyResult.Avg.ToString('0.0'))ms  P50=$($fuzzyResult.P50.ToString('0.0'))ms  P95=$($fuzzyResult.P95.ToString('0.0'))ms"
Write-Host "Search (stem)     : avg=$($stemResult.Avg.ToString('0.0'))ms  P50=$($stemResult.P50.ToString('0.0'))ms  P95=$($stemResult.P95.ToString('0.0'))ms"
Write-Host "Search (field)    : avg=$($fieldResult.Avg.ToString('0.0'))ms  P50=$($fieldResult.P50.ToString('0.0'))ms  P95=$($fieldResult.P95.ToString('0.0'))ms"
Write-Host ""
Write-Host "Error rate        : $docsErrored / $docsProcessed"
Write-Host "Benchmark dir     : $BenchDir"
