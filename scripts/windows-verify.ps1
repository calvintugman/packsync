# PackSync: verification on a real Windows installation.
#
# Run from the repository root:
#     powershell -ExecutionPolicy Bypass -File scripts\windows-verify.ps1
#
# It builds PackSync, runs the test suite, and then exercises the things that only real Windows can
# answer (NTFS long paths, sharing violations, the lock Minecraft holds on a world). It uses a plain
# folder as "the cloud": no Google account, no network, and it never touches Prism or your instances.
# Everything it creates is under %TEMP%\packsync-verify and is deleted at the next run.
#
# Result: windows-report.txt (send this back) and windows-test-full.log (only if tests failed).
# See docs/windows-checklist.md.

$ErrorActionPreference = 'Continue'
$repo = (Get-Location).Path
$report = Join-Path $repo 'windows-report.txt'
$fullLog = Join-Path $repo 'windows-test-full.log'
Set-Content -Path $report -Value "PackSync Windows verification  $(Get-Date -Format s)" -Encoding UTF8
$script:failed = 0

function Say([string]$text) {
    Write-Host $text
    Add-Content -Path $report -Value $text -Encoding UTF8
}
function Section([string]$title) { Say ""; Say "===== $title =====" }
function Check([string]$name, [bool]$ok, [string]$detail = '') {
    if ($ok) { Say "PASS  $name" } else { Say "FAIL  $name  $detail"; $script:failed++ }
}
# Run a program, return its combined output as one string; the exit code is in $LASTEXITCODE.
function Capture([string]$exe, [string[]]$arguments) {
    $o = & $exe @arguments 2>&1 | ForEach-Object { "$_" }
    return ($o -join "`n")
}

# ---------------------------------------------------------------------------------------------
Section 'System'
$os = Get-CimInstance Win32_OperatingSystem
Say "Windows: $($os.Caption) $($os.Version) build $($os.BuildNumber) $($os.OSArchitecture)"
Say "PowerShell: $($PSVersionTable.PSVersion)"
Say "rustc: $(Capture 'rustc' @('--version'))"
Say "cargo: $(Capture 'cargo' @('--version'))"
Say "git: $(Capture 'git' @('--version'))"
Say "git core.autocrlf: '$(Capture 'git' @('config', '--get', 'core.autocrlf'))'"
$lp = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -ErrorAction SilentlyContinue).LongPathsEnabled
Say "LongPathsEnabled (registry): $lp   (PackSync must work with 0 or 1)"
$drive = (Get-Item $env:TEMP).PSDrive.Name
Say "TEMP is on ${drive}: file system $((Get-Volume -DriveLetter $drive).FileSystem)"
Say "commit: $(Capture 'git' @('rev-parse', '--short', 'HEAD'))   uncommitted files: $((Capture 'git' @('status', '--porcelain')).Split("`n").Where({ $_ }).Count)"

# ---------------------------------------------------------------------------------------------
Section 'Line endings'
$crlf = (Capture 'git' @('ls-files', '--eol')).Split("`n") | Where-Object { $_ -match 'w/crlf' -and $_ -notmatch '\.(png|ico|icns)$' }
Check 'no checked-out text file has CRLF line endings' ($crlf.Count -eq 0) "($($crlf.Count) files, first: $($crlf | Select-Object -First 1))"

# ---------------------------------------------------------------------------------------------
Section 'Build'
$build = Capture 'cargo' @('build', '--release', '-p', 'packsync-cli')
Check 'cargo build --release -p packsync-cli' ($LASTEXITCODE -eq 0) ($build.Split("`n") | Select-Object -Last 5 | Out-String)
$ps = Join-Path $repo 'target\release\packsync.exe'
if (-not (Test-Path $ps)) { Say 'packsync.exe was not built; stopping here.'; exit 1 }
Say "packsync: $(Capture $ps @('--version'))"

# ---------------------------------------------------------------------------------------------
Section 'Test suite (cargo test --workspace) - this takes several minutes'
$tests = Capture 'cargo' @('test', '--workspace', '--no-fail-fast')
$testExit = $LASTEXITCODE
Set-Content -Path $fullLog -Value $tests -Encoding UTF8
$lines = $tests.Split("`n")
$passed = 0; $failedTests = 0; $ignored = 0
foreach ($l in $lines) {
    if ($l -match '^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored') {
        $passed += [int]$Matches[1]; $failedTests += [int]$Matches[2]; $ignored += [int]$Matches[3]
    }
}
Say "totals: $passed passed, $failedTests failed, $ignored ignored (cargo exit code $testExit)"
$lines | Where-Object { $_ -match '^test .* FAILED$' -or $_ -match 'panicked at' -or $_ -match '^error' } |
    Select-Object -First 40 | ForEach-Object { Say "  $_" }
Check 'the whole test suite passes' ($testExit -eq 0 -and $failedTests -eq 0) "see windows-test-full.log"
$lockTest = $lines | Where-Object { $_ -match 'lock_probe_sees_a_byte_range_lock_and_an_exclusive_open' } | Select-Object -First 1
Say "lock probe test line: $lockTest"
Check 'the session.lock probe test ran and passed (it cannot be checked under Wine)' ($lockTest -match '\.\.\. ok')

# ---------------------------------------------------------------------------------------------
Section 'Functional run on NTFS (local folder as the cloud)'
$root = Join-Path $env:TEMP 'packsync-verify'
if (Test-Path $root) { cmd /c "rmdir /s /q `"\\?\$root`"" | Out-Null }
New-Item -ItemType Directory -Force -Path $root | Out-Null
$cloud = Join-Path $root 'cloud'
$instA = Join-Path $root 'pcA\instances\Verify Pack'
$dataB = Join-Path $root 'pcB'
New-Item -ItemType Directory -Force -Path $cloud, "$dataB\instances" | Out-Null

# .NET file APIs with the \\?\ prefix, so the script itself is not limited to 260 characters.
function LP([string]$p) { return "\\?\$p" }
function WriteFile([string]$path, [byte[]]$bytes) {
    [System.IO.Directory]::CreateDirectory((LP (Split-Path $path -Parent))) | Out-Null
    [System.IO.File]::WriteAllBytes((LP $path), $bytes)
}
function RandomBytes([int]$n, [int]$seed) { $b = New-Object byte[] $n; (New-Object System.Random $seed).NextBytes($b); return ,$b }
function Text([string]$s) { return ,[System.Text.Encoding]::UTF8.GetBytes($s) }
# relative path -> SHA-256 for every file under <instance>\.minecraft (session.lock is never synced)
function TreeHash([string]$inst) {
    $base = LP (Join-Path $inst '.minecraft')
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $map = @{}
    foreach ($f in [System.IO.Directory]::EnumerateFiles($base, '*', [System.IO.SearchOption]::AllDirectories)) {
        $rel = $f.Substring($base.Length + 1)
        if ($rel -like '*session.lock') { continue }
        $map[$rel] = [System.BitConverter]::ToString($sha.ComputeHash([System.IO.File]::ReadAllBytes($f)))
    }
    return $map
}
function SameTree($a, $b) {
    if ($a.Count -ne $b.Count) { return $false }
    foreach ($k in $a.Keys) { if ($a[$k] -ne $b[$k]) { return $false } }
    return $true
}

$mc = Join-Path $instA '.minecraft'
WriteFile (Join-Path $instA 'instance.cfg') (Text "InstanceType=OneSix`nname=Verify Pack`n")
WriteFile (Join-Path $instA 'mmc-pack.json') (Text '{"components":[],"formatVersion":1}')
WriteFile (Join-Path $mc 'saves\World\level.dat') (RandomBytes 2000 1)
WriteFile (Join-Path $mc 'saves\World\session.lock') (Text ([string][char]0x2603))
WriteFile (Join-Path $mc 'saves\World\region\r.0.0.mca') (RandomBytes 2000000 2)
WriteFile (Join-Path $mc 'saves\World\region\r.0.1.mca') (RandomBytes 2000000 3)
WriteFile (Join-Path $mc 'mods\example-mod.jar') (RandomBytes 500000 4)
WriteFile (Join-Path $mc 'config\plain.cfg') (Text "a=1`n")
WriteFile (Join-Path $mc 'config\crlf.cfg') (Text "line one`r`nline two`r`n")
WriteFile (Join-Path $mc 'config\empty.cfg') (New-Object byte[] 0)
$uni = "config\" + [char]0x65E5 + [char]0x672C + [char]0x8A9E + " with spaces\" + [char]0x0444 + [char]0x0430 + [char]0x0439 + [char]0x043B + ".json"
WriteFile (Join-Path $mc $uni) (Text '{}')
$deepRel = 'config\' + ((0..24 | ForEach-Object { "deep-directory-$_" }) -join '\') + '\a-file-far-beyond-the-260-character-limit.cfg'
$deep = Join-Path $mc $deepRel
WriteFile $deep (Text "deep`n")
Say "deep path length: $($deep.Length) characters"

$a = @('--config-dir', "$root\pcA\config", '--state-dir', "$root\pcA\state")
$b = @('--config-dir', "$root\pcB\config", '--state-dir', "$root\pcB\state")
function PS-A([string[]]$more) { return Capture $ps ($a + $more) }
function PS-B([string[]]$more) { return Capture $ps ($b + $more) }

$o = PS-A @('storage', 'use-local', $cloud); Check 'A: use a local folder as the cloud' ($LASTEXITCODE -eq 0) $o
$o = PS-B @('storage', 'use-local', $cloud); Check 'B: use the same folder' ($LASTEXITCODE -eq 0) $o
PS-B @('device', 'pcB') | Out-Null
$o = PS-A @('--yes', 'enable', $instA); Check 'A: enable (first snapshot and upload)' ($LASTEXITCODE -eq 0) $o
$o = PS-B @('--yes', 'restore-new', '--data-dir', $dataB, '--name', 'Verify Pack'); Check 'B: restore-new' ($LASTEXITCODE -eq 0) $o
$instB = Join-Path $dataB 'instances\Verify Pack'
Check 'B: restored files are byte-identical to A (long path, Unicode, CRLF and empty file included)' (SameTree (TreeHash $instA) (TreeHash $instB))
Check 'B: the file behind the 260+ character path exists' ([System.IO.File]::Exists((LP (Join-Path $instB ".minecraft\$deepRel"))))
$crlfBytes = [System.IO.File]::ReadAllBytes((LP (Join-Path $instB '.minecraft\config\crlf.cfg')))
Check 'B: CRLF line endings inside a synced file were not converted' ($crlfBytes.Length -eq 20)

# an in-place edit of one region file, like the game does
$fs = [System.IO.File]::Open((LP (Join-Path $mc 'saves\World\region\r.0.1.mca')), 'Open', 'ReadWrite')
$fs.Position = 1000000; $fs.Write((RandomBytes 4096 9), 0, 4096); $fs.Close()
[System.IO.File]::WriteAllBytes((LP $deep), (Text "deep, edited`n"))
$o = PS-A @('sync', 'Verify Pack'); Check 'A: sync after an in-place edit' ($LASTEXITCODE -eq 0) $o
Say ($o.Split("`n") | Select-Object -Last 3 | Out-String).Trim()
$o = PS-B @('sync', 'Verify Pack'); Check 'B: catches up' ($LASTEXITCODE -eq 0) $o
Check 'B: identical to A after catching up' (SameTree (TreeHash $instA) (TreeHash $instB))
$o = PS-B @('verify', 'Verify Pack'); Check 'B: verify reports nothing changed or missing' ($LASTEXITCODE -eq 0 -and $o -match '0 changed, 0 missing') $o

# ---------------------------------------------------------------------------------------------
Section 'File locking'
# 1. The lock Minecraft (1.16+) holds on a world: a byte-range lock on session.lock, file still openable.
$lockPath = LP (Join-Path $mc 'saves\World\session.lock')
$game = [System.IO.File]::Open($lockPath, 'Open', 'ReadWrite', 'ReadWrite')
$game.Lock(0, 1024)
[System.IO.File]::WriteAllBytes((LP (Join-Path $mc 'config\plain.cfg')), (Text "a=1.5`n"))
$o = PS-A @('snapshot', 'Verify Pack'); $code = $LASTEXITCODE
$o2 = PS-A @('sync', 'Verify Pack'); $code2 = $LASTEXITCODE
$game.Unlock(0, 1024); $game.Close()
Say "snapshot while session.lock is locked: exit code $code"
Say ($o.Split("`n") | Select-Object -Last 2 | Out-String).Trim()
Check 'A: snapshot refuses while a world is locked (exit code 14, "game is using the instance")' ($code -eq 14) "exit $code"
Say "sync while session.lock is locked: exit code $code2"
Say ($o2.Split("`n") | Select-Object -Last 3 | Out-String).Trim()
Check 'A: sync saves nothing while the world is locked and says why' ($code2 -ne 0 -and $o2 -match 'in use') "exit $code2"
$o = PS-A @('sync', 'Verify Pack'); Check 'A: sync works again after the lock is released' ($LASTEXITCODE -eq 0) $o

# 2. A changed file that another program holds open without sharing (antivirus, an editor, the game).
[System.IO.File]::WriteAllBytes((LP (Join-Path $mc 'config\plain.cfg')), (Text "a=2`n"))
$held = [System.IO.File]::Open((LP (Join-Path $mc 'config\plain.cfg')), 'Open', 'ReadWrite', 'None')
$o = PS-A @('sync', 'Verify Pack'); $code = $LASTEXITCODE
$held.Close()
Say "sync while a changed file is opened exclusively: exit code $code"
Say ($o.Split("`n") | Select-Object -Last 2 | Out-String).Trim()
Check 'A: sync fails cleanly (non-zero exit, no crash) when a changed file cannot be read' ($code -ne 0 -and $o -notmatch 'panicked') "exit $code"
$o = PS-A @('sync', 'Verify Pack'); Check 'A: the same sync succeeds once the file is released' ($LASTEXITCODE -eq 0) $o

# 3. Restore on B while a file that must be replaced is held open: nothing may be half-applied.
$before = TreeHash $instB
$heldB = [System.IO.File]::Open((LP (Join-Path $instB '.minecraft\config\plain.cfg')), 'Open', 'ReadWrite', 'None')
$o = PS-B @('sync', 'Verify Pack'); $code = $LASTEXITCODE
$heldB.Close()
Say "B sync (restore) while the target file is opened exclusively: exit code $code"
Say ($o.Split("`n") | Select-Object -Last 2 | Out-String).Trim()
Check 'B: restore fails cleanly while a target file is in use' ($code -ne 0 -and $o -notmatch 'panicked') "exit $code"
Check 'B: nothing was changed by the failed restore (rolled back)' (SameTree $before (TreeHash $instB))
$o = PS-B @('sync', 'Verify Pack'); Check 'B: the restore succeeds once the file is released' ($LASTEXITCODE -eq 0) $o
Check 'B: identical to A at the end' (SameTree (TreeHash $instA) (TreeHash $instB))

# ---------------------------------------------------------------------------------------------
Section 'Doctor'
Say (PS-A @('doctor'))

Section 'Summary'
if ($script:failed -eq 0) { Say 'ALL CHECKS PASSED' } else { Say "$($script:failed) CHECK(S) FAILED" }
Say "Report written to $report"
exit $script:failed
