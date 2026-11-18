$ErrorActionPreference = "Stop"
$built = Join-Path (Get-Location) "target\x86_64-pc-windows-msvc\release\compactrs.exe"
if (!(Test-Path $built)) { throw "Missing smoke-test EXE: $built" }
$testRoot = Join-Path $env:RUNNER_TEMP ("CompactRS_Stack_" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $testRoot -Force | Out-Null

function Get-Candidates([string]$exe) {
    @(Get-CimInstance Win32_Process -Filter "Name = 'compactrs.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -ieq $exe })
}
function Read-Trace([string]$path) {
    if (Test-Path $path) { return @(Get-Content $path -ErrorAction SilentlyContinue) }
    return @()
}

try {
    foreach ($mode in @("main", "dialog", "stack7")) {
        $root = Join-Path $testRoot $mode
        New-Item -ItemType Directory -Path $root -Force | Out-Null
        $exe = Join-Path $root "compactrs.exe"
        Copy-Item $built $exe -Force
        $n = if ($mode -eq "stack7") { 7 } else { 2 }
        $paths = @()
        for ($i = 1; $i -le $n; $i++) {
            $path = Join-Path $root "Folder $i"
            New-Item -ItemType Directory -Path $path -Force | Out-Null
            [IO.File]::WriteAllText((Join-Path $path "data.txt"), ("X" * 65536))
            $paths += $path
        }
        $trace = Join-Path $root "trace.log"
        $env:COMPACTRS_MOCK_LOG = $trace
        $isDialog = $mode -ne "main"
        $env:COMPACTRS_TEST_DIALOG = if ($isDialog) { "1" } else { "0" }
        Write-Host "TEST: $mode; $n directories"

        # Reproduce a Windows Explorer multi-selection with separate shell processes.
        foreach ($path in $paths) {
            Start-Process -FilePath $exe -ArgumentList "--path `"$path`" --algo xpress4k" | Out-Null
            Start-Sleep -Milliseconds 70
        }

        $deadline = (Get-Date).AddSeconds($(if ($mode -eq "stack7") { 80 } else { 45 }))
        $ready = $false
        while ((Get-Date) -lt $deadline) {
            $lines = @(Read-Trace $trace)
            $launches = @($lines | Where-Object { $_ -eq "UAC_LAUNCH" }).Count
            $brokers = @($lines | Where-Object { $_ -eq "BROKER_CREATED" }).Count
            if ($launches -gt 1 -or $brokers -gt 1) {
                throw "Duplicate broker/UAC for $mode : $($lines -join ' | ')"
            }

            $guiCreated = @($lines | Where-Object {
                if ($isDialog) { $_.StartsWith("DIALOG_CREATED|") }
                else { $_.StartsWith("MAIN_CREATED|") }
            }).Count -eq 1

            if ($isDialog) {
                $events = @($lines | Where-Object { $_.StartsWith("START|") -or $_.StartsWith("DONE|") })
                $complete = $events.Count -eq ($n * 2)
                if ($complete) {
                    for ($i = 0; $i -lt $n; $i++) {
                        $started = $events[$i * 2]
                        $ended = $events[$i * 2 + 1]
                        if (!$started.StartsWith("START|") -or !$ended.StartsWith("DONE|") -or
                            $started.Substring(6) -ne $ended.Substring(5) -or
                            $started.Substring(6) -ne $paths[$i]) {
                            $complete = $false
                            break
                        }
                    }
                }
                if ($mode -eq "stack7") {
                    # First 5 cards, then hide older ones. The top ellipsis
                    # starts after the sixth folder begins.
                    $complete = $complete -and ($lines -contains "STACK_LAYOUT|6|true|1") -and
                        ($lines -contains "STACK_LAYOUT|7|true|2")
                }
            } else {
                $complete = $true
                foreach ($path in $paths) {
                    if (($lines -notcontains "STARTUP|$path") -and ($lines -notcontains "MAIN_ADD|$path")) {
                        $complete = $false
                    }
                }
            }

            if ($launches -eq 1 -and $brokers -eq 1 -and $complete -and $guiCreated) {
                $ready = $true
                break
            }
            Start-Sleep -Milliseconds 250
        }
        if (!$ready) {
            $lines = @(Read-Trace $trace)
            $proc = @(Get-Candidates $exe)
            Write-Host "DEBUG process count=$($proc.Count)"
            foreach ($p in $proc) { Write-Host "DEBUG PID=$($p.ProcessId) command=$($p.CommandLine)" }
            throw "Timeout ($mode): $($lines -join ' | ')"
        }

        Start-Sleep -Seconds 4
        $procs = @(Get-Candidates $exe)
        if ($procs.Count -ne 1) {
            throw "Process leak in $mode : expected 1 GUI process, found $($procs.Count)"
        }
        Write-Host "PASS: $mode; $n folders; one simulated UAC, one broker, one GUI, all paths delivered."
        if ($isDialog) { Write-Host "PASS: $n folders serialized, no overlap." }
        if ($mode -eq "stack7") { Write-Host "PASS: rolling five-card stack, ellipsis, last-five viewport." }
        foreach ($p in @(Get-Candidates $exe)) {
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        }
        Start-Sleep -Milliseconds 500
    }
} finally {
    foreach ($mode in @("main", "dialog", "stack7")) {
        $exe = Join-Path (Join-Path $testRoot $mode) "compactrs.exe"
        foreach ($p in @(Get-Candidates $exe)) {
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item Env:\COMPACTRS_MOCK_LOG -ErrorAction SilentlyContinue
    Remove-Item Env:\COMPACTRS_TEST_DIALOG -ErrorAction SilentlyContinue
}
