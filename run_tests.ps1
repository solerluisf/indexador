# Run Rust tests with parallel/serial split:
#   - Tantivy indexer tests run serially (--test-threads=1) to avoid file lock contention
#   - All other lib tests run in parallel (default)
#   - C API tests run serially
#
# Usage:
#   .\run_tests.ps1                        # dev profile
#   .\run_tests.ps1 -Release               # release profile
#   .\run_tests.ps1 -CapiOnly              # C API tests only

param(
    [switch]$Release,
    [switch]$CapiOnly
)

$ErrorActionPreference = "Stop"
$profile_flag = if ($Release) { "--release" } else { "" }

function Run-Tests {
    param([string]$Desc, [string]$Cmd)
    Write-Host "`n=== $Desc ===" -ForegroundColor Cyan
    Write-Host "  $Cmd" -ForegroundColor Gray

    # Run the cargo command and capture both stdout and stderr as text
    $output = & { cmd /c "$Cmd 2>&1" } | Out-String
    $exit = $LASTEXITCODE

    if ($output) {
        # Show only the last few result lines
        $lines = $output -split "`n"
        $lines | Select-Object -Last 6 | ForEach-Object { Write-Host $_ }
    }

    if ($exit -ne 0) {
        Write-Host "FAILED: $Desc (exit code $exit)" -ForegroundColor Red
        exit $exit
    }
    Write-Host "PASSED: $Desc" -ForegroundColor Green
}

if (-not $CapiOnly) {
    Write-Host "`n============================================" -ForegroundColor Yellow
    Write-Host " pdf_extractor (lib tests)" -ForegroundColor Yellow
    Write-Host "============================================" -ForegroundColor Yellow

    # Phase 1: Tantivy indexer tests — serial (file lock contention)
    Run-Tests "indexer tests (serial)" `
        "cargo test -p pdf_extractor --lib $profile_flag -- --test-threads=1 indexer::"

    # Phase 2: All other lib tests — parallel
    Run-Tests "non-indexer tests (parallel)" `
        "cargo test -p pdf_extractor --lib $profile_flag -- --skip indexer::"

    # Phase 3: Integration tests (any tests/ files)
    Run-Tests "pdf_extractor integration tests" `
        "cargo test -p pdf_extractor $profile_flag --tests -- --test-threads=1"
}

Write-Host "`n============================================" -ForegroundColor Yellow
Write-Host " pdf_extractor_capi (C API tests)" -ForegroundColor Yellow
Write-Host "============================================" -ForegroundColor Yellow

Run-Tests "C API tests (serial)" `
    "cargo test -p pdf_extractor_capi $profile_flag -- --test-threads=1"

Write-Host "`n`nAll test suites passed!" -ForegroundColor Green
