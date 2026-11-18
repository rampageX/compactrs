$ErrorActionPreference = "Stop"
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CompactRSWin {
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr FindWindowW(string className, string title);
}
'@

$built = Join-Path (Get-Location) "target\x86_64-pc-windows-msvc\release\compactrs.exe"
if (!(Test-Path $built)) { throw "Missing smoke-test exe: $built" }
$testRoot = Join-Path $env:RUNNER_TEMP ("CompactRS_Broker_" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $testRoot -Force | Out-Null

function Get-Candidates([string]$exe) {
    @(Get-CimInstance Win32_Process -Filter "Name = 'compactrs.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -ieq $exe })
}

try {
    foreach ($mode in @("main", "dialog")) {
        $root = Join-Path $testRoot $mode
        New-Item -ItemType Directory -Path $root -Force | Out-Null
        $exe = Join-Path $root "compactrs.exe"
        Copy-Item $built $exe -Force
        $a = Join-Path $root "A first"
        $b = Join-Path $root "B second"
        New-Item -ItemType Directory -Path $a,$b -Force | Out-Null
        [IO.File]::WriteAllText((Join-Path $a "a.txt"), ("A" * 65536))
        [IO.File]::WriteAllText((Join-Path $b "b.txt"), ("B" * 65536))
        $trace = Join-Path $root "trace.log"
        $env:COMPACTRS_MOCK_LOG = $trace
        $env:COMPACTRS_TEST_DIALOG = if ($mode -eq "dialog") { "1" } else { "0" }
        $windowClass = if ($mode -eq "dialog") { "CompactRS_CtxDialog" } else { "CompactRS_Class" }
        Write-Host "TEST: mode=$mode; exe=$exe"

        # Explorer's static verb starts one invocation per selected directory.
        Start-Process -FilePath $exe -ArgumentList "--path `"$a`" --algo xpress4k" | Out-Null
        Start-Sleep -Milliseconds 100
        Start-Process -FilePath $exe -ArgumentList "--path `"$b`" --algo xpress4k" | Out-Null

        $deadline = (Get-Date).AddSeconds(35)
        $ready = $false
        while ((Get-Date) -lt $deadline) {
            $lines = if (Test-Path $trace) { @(Get-Content $trace -ErrorAction SilentlyContinue) } else { @() }
            $launches = @($lines | Where-Object { $_ -eq "UAC_LAUNCH" }).Count
            $brokers = @($lines | Where-Object { $_ -eq "BROKER_CREATED" }).Count
            if ($launches -gt 1 -or $brokers -gt 1) {
                throw "Duplicate broker/UAC for $mode. TRACE: $($lines -join ' | ')"
            }
            $hwnd = [CompactRSWin]::FindWindowW($windowClass, $null)
            if ($mode -eq "dialog") {
                $events = @($lines | Where-Object { $_.StartsWith("START|") -or $_.StartsWith("DONE|") })
                $complete = $events.Count -ge 4 -and
                    $events[0].StartsWith("START|") -and $events[1].StartsWith("DONE|") -and
                    $events[2].StartsWith("START|") -and $events[3].StartsWith("DONE|") -and
                    $events[0].Substring(6) -eq $events[1].Substring(5) -and
                    $events[2].Substring(6) -eq $events[3].Substring(5) -and
                    $events[0].Substring(6) -ne $events[2].Substring(6)
            } else {
                $seenA = ($lines -contains "STARTUP|$a") -or ($lines -contains "MAIN_ADD|$a")
                $seenB = ($lines -contains "STARTUP|$b") -or ($lines -contains "MAIN_ADD|$b")
                $complete = $seenA -and $seenB
            }
            if ($launches -eq 1 -and $brokers -eq 1 -and $complete -and $hwnd -ne [IntPtr]::Zero) {
                $ready = $true
                break
            }
            Start-Sleep -Milliseconds 250
        }
        if (!$ready) {
            $lines = if (Test-Path $trace) { @(Get-Content $trace) } else { @() }
            throw "Timeout ($mode): expected one broker, one simulated UAC, correct routing and a GUI. TRACE: $($lines -join ' | ')"
        }

        # The short-lived broker and per-item launchers must all exit.
        Start-Sleep -Seconds 4
        $procs = @(Get-Candidates $exe)
        if ($procs.Count -ne 1) {
            $detail = @($procs | ForEach-Object { "${($_.ProcessId)}: $($_.CommandLine)" })
            throw "Process leak in $mode : expected 1 active GUI, found $($procs.Count). $($detail -join ' | ')"
        }
        Write-Host "PASS: $mode; one simulated UAC, one broker, one GUI process, both paths delivered."
        if ($mode -eq "dialog") { Write-Host "PASS: folder boundaries are sequential (START, DONE, START, DONE)." }
        foreach ($p in @(Get-Candidates $exe)) { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
        Start-Sleep -Milliseconds 500
    }
} finally {
    foreach ($mode in @("main", "dialog")) {
        $exe = Join-Path (Join-Path $testRoot $mode) "compactrs.exe"
        foreach ($p in @(Get-Candidates $exe)) {
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item Env:\COMPACTRS_MOCK_LOG -ErrorAction SilentlyContinue
    Remove-Item Env:\COMPACTRS_TEST_DIALOG -ErrorAction SilentlyContinue
}
