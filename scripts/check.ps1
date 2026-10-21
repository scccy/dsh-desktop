#!/usr/bin/env pwsh
# Runs the same gates as .github/workflows/ci.yml, in the same order.
# Usage: pwsh -File scripts/check.ps1   (any failure exits non-zero)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $steps = @(
        @{ Label = 'cargo fmt --all --check'; Arguments = @('fmt', '--all', '--check') }
        @{ Label = 'cargo clippy --workspace --all-targets -- -D warnings'; Arguments = @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') }
        @{ Label = 'cargo test --workspace'; Arguments = @('test', '--workspace') }
    )

    foreach ($step in $steps) {
        Write-Host "==> $($step.Label)" -ForegroundColor Cyan
        $arguments = $step.Arguments
        & cargo @arguments
        if ($LASTEXITCODE -ne 0) {
            Write-Host "FAILED: $($step.Label)" -ForegroundColor Red
            exit $LASTEXITCODE
        }
    }

    Write-Host 'All checks passed.' -ForegroundColor Green
}
finally {
    Pop-Location
}
