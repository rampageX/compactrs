# Generate a CompactRS release tag using China Standard Time (UTC+08:00).
# e.g. 2026-11-18 13:45:00 UTC => v2026.11.18-r2145
# The injected clock is for deterministic CI regression tests.
param(
    [datetime]$UtcNow = [datetime]::UtcNow
)
$ErrorActionPreference = 'Stop'
$beijing = $UtcNow.ToUniversalTime().AddHours(8)
'v{0}.{1}.{2}-r{3}' -f $beijing.Year, $beijing.Month, $beijing.Day, $beijing.ToString('HHmm')
