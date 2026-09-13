#!/usr/bin/env pwsh
<#
.SYNOPSIS
Offline smoke test for a staged Windows CLI bundle: proves the bundled models
are the models the binary loads.

.DESCRIPTION
Runs `xberg.exe extract <Fixture> --format json` with -CleanPath on PATH,
HF_HUB_OFFLINE=1 and HF_HUB_CACHE set to -CacheDir, and asserts one of two
outcomes:

  default                      exit 0, parseable JSON on stdout, non-empty
                               result.content -- extraction works offline.
  -ExpectEmptyCacheFailure     non-zero exit whose stderr mentions offline mode
                               -- the same command against an empty cache has to
                               fail rather than silently download. Only valid
                               when the bundle actually stages HF models for
                               this extract path (paddle/layout); the lean
                               package omits this probe.

One `smoke=ok ...` line on stdout and exit 0 on success; otherwise the reason
goes to stderr and the exit code is 1. package-cli-windows.ps1 runs it twice, as
two of its five parallel validation probes.

.EXAMPLE
pwsh -NoProfile -File scripts/publish/cli/offline-smoke.ps1 -Exe xberg-cli-x86_64-pc-windows-msvc/xberg.exe -Fixture fixtures/images/test_hello_world.png -CacheDir xberg-cli-x86_64-pc-windows-msvc/models -CleanPath $env:PATH
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [Parameter(Mandatory = $true)][string]$Fixture,
  [Parameter(Mandatory = $true)][string]$CacheDir,
  # PATH the binary runs with: the caller's cleaned PATH, without the workspace
  # or the ORT directory, so a DLL missing from the bundle cannot be satisfied
  # by the host.
  [Parameter(Mandatory = $true)][string]$CleanPath,
  [switch]$ExpectEmptyCacheFailure
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

function Fail([string]$Message) {
  [Console]::Error.WriteLine("offline-smoke: $Message")
  exit 1
}

function Invoke-Captured([string]$FilePath, [string[]]$Arguments) {
  $stdoutFile = New-TemporaryFile
  $stderrFile = New-TemporaryFile
  try {
    & $FilePath @Arguments 1>$stdoutFile.FullName 2>$stderrFile.FullName
    $code = $LASTEXITCODE
    return [pscustomobject]@{
      ExitCode = $code
      Stdout   = (Get-Content -LiteralPath $stdoutFile.FullName -Raw -ErrorAction SilentlyContinue)
      Stderr   = (Get-Content -LiteralPath $stderrFile.FullName -Raw -ErrorAction SilentlyContinue)
    }
  }
  finally {
    Remove-Item -LiteralPath $stdoutFile.FullName, $stderrFile.FullName -Force -ErrorAction SilentlyContinue
  }
}

if (-not (Test-Path -LiteralPath $Exe)) { Fail "no binary at $Exe" }
if (-not (Test-Path -LiteralPath $Fixture)) { Fail "no fixture at $Fixture" }

# Process-wide on purpose: the binary under test has to see exactly this
# environment, and this script is the only thing running in this process.
$env:PATH = $CleanPath
$env:HF_HUB_CACHE = $CacheDir
$env:HF_HUB_OFFLINE = "1"
$env:HUGGINGFACE_HUB_OFFLINE = "1"

$extractArgs = @("extract", $Fixture, "--format", "json")
$result = Invoke-Captured -FilePath $Exe -Arguments $extractArgs

if ($ExpectEmptyCacheFailure) {
  if ($result.ExitCode -eq 0) {
    Fail "the bundled models are not what the smoke test loaded: with HF_HUB_CACHE=$CacheDir and offline mode forced, the extraction still succeeded"
  }
  if ($result.Stderr -notmatch "offline mode") {
    Fail "cache-miss probe failed as expected (exit $($result.ExitCode)) but stderr does not mention offline mode: $($result.Stderr)"
  }
  Write-Host "smoke=ok kind=empty-cache exit=$($result.ExitCode) cache=$CacheDir"
  exit 0
}

if ($result.ExitCode -ne 0) {
  Fail "extraction failed (exit $($result.ExitCode)): $($result.Stderr)"
}
if ([string]::IsNullOrEmpty($result.Stdout)) {
  Fail "extraction wrote nothing to stdout (exit 0), so the JSON result is unreadable"
}
$text = $result.Stdout
$start = $text.IndexOf('{')
$end = $text.LastIndexOf('}')
if ($start -lt 0 -or $end -le $start) {
  Fail "no JSON on stdout: $text"
}
try {
  $document = $text.Substring($start, $end - $start + 1) | ConvertFrom-Json
  $content = $document.result.content
}
catch {
  Fail "could not parse the JSON on stdout: $($_.Exception.Message)"
}
if ([string]::IsNullOrWhiteSpace($content)) {
  Fail "extraction produced empty content with HF_HUB_CACHE=$CacheDir"
}
Write-Host "smoke=ok kind=positive content_len=$($content.Length) cache=$CacheDir"
exit 0
