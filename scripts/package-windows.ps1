Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$tauriConf = Get-Content (Join-Path $repo 'apps/desktop/src-tauri/tauri.conf.json') -Raw | ConvertFrom-Json
$ver = $tauriConf.version

$cliExe = Join-Path $repo 'target/release/packsync.exe'
$desktopExe = Join-Path $repo 'apps/desktop/src-tauri/target/release/packsync-desktop.exe'

if (-not (Test-Path -LiteralPath $cliExe)) {
    Write-Error "Missing CLI binary: $cliExe"
    exit 1
}
if (-not (Test-Path -LiteralPath $desktopExe)) {
    Write-Error "Missing desktop binary: $desktopExe"
    exit 1
}

$loaderDll = Join-Path $repo 'apps/desktop/src-tauri/target/release/WebView2Loader.dll'
if (-not (Test-Path -LiteralPath $loaderDll)) {
    $foundLoader = Get-ChildItem -Path (Join-Path $repo 'apps/desktop/src-tauri/target/release/build') -Recurse -Filter 'WebView2Loader.dll' -ErrorAction SilentlyContinue |
        Where-Object { $_.Directory.Name -eq 'x64' } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if ($null -eq $foundLoader) {
        Write-Error 'Could not locate x64 WebView2Loader.dll under apps/desktop/src-tauri/target/release'
        exit 1
    }
    $loaderDll = $foundLoader.FullName
}

$outRoot = Join-Path $repo 'dist-release'
$pkgName = "packsync-$ver-windows-x86_64"
$stageDir = Join-Path $outRoot $pkgName
$zipPath = Join-Path $outRoot "$pkgName.zip"

if (Test-Path -LiteralPath $stageDir) {
    Remove-Item -LiteralPath $stageDir -Recurse -Force
}
if (Test-Path -LiteralPath $zipPath) {
    Remove-Item -LiteralPath $zipPath -Force
}
New-Item -ItemType Directory -Force -Path $stageDir | Out-Null

Copy-Item -LiteralPath $cliExe -Destination (Join-Path $stageDir 'packsync.exe') -Force
Copy-Item -LiteralPath $desktopExe -Destination (Join-Path $stageDir 'packsync-desktop.exe') -Force
Copy-Item -LiteralPath $loaderDll -Destination (Join-Path $stageDir 'WebView2Loader.dll') -Force
Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination (Join-Path $stageDir 'LICENSE.txt') -Force

$readmeText = @"
PackSync $ver (Windows, x86_64)
Continue your Minecraft modpack on any computer: syncs Prism Launcher instances (mods, configs, worlds).

Install
  Unzip this folder somewhere permanent (for example C:\PackSync) and run packsync-desktop.exe.
  Keep the files together: packsync.exe is used by Prism's launch hooks, WebView2Loader.dll by the app.
  The app is not code-signed yet, so Windows SmartScreen warns: choose "More info" > "Run anyway".
  Needs the Microsoft Edge WebView2 runtime (already installed on Windows 10 and 11).

Getting started
  First computer:   open PackSync, press "Start on this computer", then pick your instances.
  Other computers:  on the first one, Settings > Storage > "Show a pairing code";
                    on the new one, "Join from another computer" and type the code.
  Then add a passkey (Settings > Storage) so you can get back in if you ever lose every computer.

Everything is encrypted on your computer before upload. Command line: packsync.exe --help
Source: https://github.com/calvintugman/packsync    Licence: MIT (LICENSE.txt)
"@
$readmeLf = ($readmeText -replace "`r`n", "`n") + "`n"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText((Join-Path $stageDir 'README.txt'), $readmeLf, $utf8NoBom)

$tarExe = Join-Path $env:SystemRoot 'System32\tar.exe'
& $tarExe -a -C $outRoot -cf $zipPath $pkgName
if ($LASTEXITCODE -ne 0) {
    Write-Error "tar.exe failed with exit code $LASTEXITCODE"
    exit 1
}

Remove-Item -LiteralPath $stageDir -Recurse -Force

$zipInfo = Get-Item -LiteralPath $zipPath
$hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Output "Packaged: $($zipInfo.FullName) ($($zipInfo.Length) bytes)"
Write-Output "SHA-256:  $hash"
