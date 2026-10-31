#!/usr/bin/env pwsh
<#
.SYNOPSIS
  codesearch installer/updater for Windows.

.DESCRIPTION
  First install: asks for the install path (default ~/.local/bin).
  Existing install (marker ~/.codesearch/install-path or codesearch on PATH):
  stops the running serve from that install, updates the binaries in place,
  and never asks for a path.

  Download-then-run: the script verifies the release SHA256 before installing.

.PARAMETER Destination
  Install directory. Skips the first-install prompt (CI/automation).

.PARAMETER NoCSharp
  Install the small package without the bundled scip-csharp helper.

.PARAMETER Tag
  Release tag to install (default: latest published release).

.PARAMETER DryRun
  Show what would happen; download nothing, write nothing.
#>
[CmdletBinding()]
param(
    [string]$Destination,
    [switch]$NoCSharp,
    [string]$Tag,
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
$Repo = 'flupkede/codesearch'
$Marker = Join-Path $HOME '.codesearch/install-path'

function Resolve-LatestTag {
    $r = Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$Repo/releases/latest"
    return $r.tag_name
}

function Get-ArtifactName([bool]$CSharp) {
    # windows-x86_64 only; ARM64 Windows builds do not exist yet.
    $name = 'codesearch-windows-x86_64'
    if ($CSharp) { $name = "$name-with-csharp" }
    return "$name.zip"
}

function Get-ExistingInstallDir {
    if (Test-Path $Marker) {
        # -Encoding UTF8: Windows PowerShell 5.1 decodes BOM-less files as ANSI
        # by default, mangling non-ASCII destinations on the README one-liner.
        $p = (Get-Content $Marker -Raw -Encoding UTF8).Trim()
        if ($p -and (Test-Path (Join-Path $p 'codesearch.exe'))) { return $p }
    }
    $cmd = Get-Command codesearch.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return Split-Path $cmd.Source -Parent }
    return $null
}

function Stop-InstallServe([string]$Dir) {
    $exe = Join-Path $Dir 'codesearch.exe'
    $mine = @(Get-Process codesearch -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
    if (-not $mine) { return }
    Write-Host "Stopping $($mine.Count) running codesearch process(es) from $Dir ..."
    # The exe is file-locked while running; a stop is required to replace it.
    $mine | Stop-Process -Force
    Start-Sleep -Seconds 1
}

Write-Host '== codesearch installer (Windows) ==' -ForegroundColor Cyan
if (-not $Tag) { $Tag = Resolve-LatestTag }
$wantCSharp = -not $NoCSharp
$artifact = Get-ArtifactName $wantCSharp
$base = "https://github.com/$Repo/releases/download/$Tag"

$existing = Get-ExistingInstallDir
if ($existing) {
    $dir = $existing
    Write-Host "Existing install found: $dir — updating in place."
    if ($DryRun) { Write-Host "(dry-run) would stop serve from $dir if running" }
    else { Stop-InstallServe $dir }
} else {
    if ($Destination) {
        $dir = $Destination
    } else {
        $default = Join-Path $HOME '.local/bin'
        $answer = Read-Host "Install path [$default]"
        $dir = if ($answer) { $answer } else { $default }
    }
    Write-Host "Fresh install into: $dir"
}

$version = $null
if (-not $DryRun) {
    $tmp = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ("codesearch-install-" + [Guid]::NewGuid().ToString('N')))
    try {
        Write-Host "Downloading $artifact ($Tag) ..."
        $zip = Join-Path $tmp $artifact
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$artifact" -OutFile $zip

        $sumName = "$artifact.sha256"
        $expected = $null
        try {
            # -ErrorAction Stop so a 404 becomes a catchable exception; PS7
            # throws HttpResponseException (not WebException) for non-2xx, so
            # the catch must be typeless. Absent sidecar = skip, not fail.
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$sumName" -OutFile (Join-Path $tmp $sumName) -ErrorAction Stop
            $expected = ((Get-Content (Join-Path $tmp $sumName) -First 1) -split '\s+')[0]
        } catch {
            Write-Warning 'No checksum published for this release — skipping verification.'
        }
        if ($expected) {
            $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
            if ($actual -ne $expected) { throw "SHA256 mismatch for $artifact (expected $expected, got $actual)" }
            Write-Host 'SHA256 verified.' -ForegroundColor Green
        }

        Write-Host 'Extracting ...'
        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Copy-Item (Join-Path $tmp 'codesearch.exe') $dir -Force
        $helper = Get-ChildItem $tmp -Recurse -Filter 'scip-csharp.exe' | Select-Object -First 1
        if ($helper) { Copy-Item $helper.FullName $dir -Force }
        New-Item -ItemType Directory -Force -Path (Split-Path $Marker) | Out-Null
        # WriteAllText = UTF-8 without BOM on every PS version; -Encoding ASCII
        # would mangle non-ASCII destinations and break the marker round-trip.
        [System.IO.File]::WriteAllText($Marker, $dir)
        $version = (& (Join-Path $dir 'codesearch.exe') --version 2>$null)
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($DryRun) {
    Write-Host "(dry-run) would install $artifact from $Tag into $dir"
} else {
    Write-Host "Installed into $dir" -ForegroundColor Green
    if ($version) { Write-Host $version }
    $pathDir = (Resolve-Path $dir).Path
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $pathDir) {
        Write-Host "NOTE: $pathDir is not on your PATH. Add it with:" -ForegroundColor Yellow
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"$pathDir;`$env:Path`", 'User')"
    }
}
