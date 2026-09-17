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
                               result.content, a `Layout detection completed`
                               line on stderr (the bundled RT-DETR/TATR loaded
                               from -CacheDir) and no model-cache miss --
                               extraction works offline off the bundle's cache.
  -ExpectEmptyCacheFailure     stderr reports the offline model-cache miss
                               ("Hugging Face offline mode is enabled and ...
                               is not available in the local cache"). Whether
                               the process also fails is not asserted: the
                               layout path falls back to whole-image OCR, and
                               whether that fallback needs its own HF model
                               depends on the OCR backend the build compiles in
                               (bundled PaddleOCR does, Tesseract does not). The
                               diagnostic, not the exit code, is the contract --
                               a run that never reports a miss is not reading
                               the cache under test.

The fixture is extracted with layout detection enabled (an inline config that
also sets `ocr`, so `should_use_layout_ocr` is true and the RT-DETR/TATR files
are part of the extract path). Without that config the PNG smoke needs no
HF-cached model and neither probe proves anything about the bundled models.

One `smoke=ok ...` line on stdout and exit 0 on success; otherwise the reason
goes to stderr and the exit code is 1. package-cli-windows.ps1 runs it twice, as
two of its parallel validation probes.

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

function Invoke-Captured([string]$FilePath, [string[]]$Arguments, [int]$TimeoutMs = 900000) {
  $stdoutFile = New-TemporaryFile
  $stderrFile = New-TemporaryFile
  try {
    # Start-Process rather than `&`: the probe must be killable when it hangs (a model
    # session deadlocked on load), or a stuck binary would block the packaging gate until
    # a human intervenes. -ArgumentList joins with spaces without quoting, so arguments
    # carrying spaces quote themselves. taskkill is called by absolute path: this script
    # replaces PATH with the clean path before probing, and System32 is not on it.
    $quoted = foreach ($argument in $Arguments) {
      if ($argument -match '\s') { '"' + $argument + '"' } else { $argument }
    }
    $process = Start-Process -FilePath $FilePath -ArgumentList $quoted -PassThru -NoNewWindow `
      -RedirectStandardOutput $stdoutFile.FullName -RedirectStandardError $stderrFile.FullName
    if (-not $process.WaitForExit($TimeoutMs)) {
      & "$env:SystemRoot\System32\taskkill.exe" /PID $process.Id /T /F | Out-Null
      # Bounded: if the kill itself failed, giving up on the wait beats hanging the caller.
      $process.WaitForExit(10000) | Out-Null
      Fail "probe did not finish within $($TimeoutMs / 1000)s and was killed: $FilePath $($Arguments -join ' ')"
    }
    return [pscustomobject]@{
      ExitCode = $process.ExitCode
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

# base64 of {"layout":{},"ocr":{}} -- inline JSON would need quoting through the native
# argv handoff; the base64 form is exactly one argument with no escaping. The `ocr`
# section is what enables the layout path for an image (`should_use_layout_ocr`), and
# therefore what makes the bundled RT-DETR/TATR files part of the extract path.
$layoutConfigB64 = "eyJsYXlvdXQiOnt9LCJvY3IiOnt9fQ=="
$extractArgs = @("extract", $Fixture, "--format", "json",
                 "--config-json-base64", $layoutConfigB64)
$result = Invoke-Captured -FilePath $Exe -Arguments $extractArgs

if ($ExpectEmptyCacheFailure) {
  # An empty cache does not have to make the process fail: the image extractor falls back to
  # whole-image OCR after a layout-model failure, and that fallback fails too only when its own
  # backend needs an HF model (bundled PaddleOCR does, Tesseract does not). The missing-model
  # diagnostic is the assertion, because a run that never reports a miss is not reading the
  # cache under test.
  if ($result.Stderr -notmatch "offline mode") {
    Fail "the bundled models are not what the smoke test loaded: with HF_HUB_CACHE=$CacheDir and offline mode forced, the run exited $($result.ExitCode) without reporting a model-cache miss"
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
# The config enables the layout path, so this run resolved RT-DETR/TATR through the cache
# under test. A miss here means the bundled models did not satisfy the extract path; no
# `Layout detection completed` line means the path never ran and nothing was proven.
if ($result.Stderr -match "offline mode") {
  Fail "the staged cache at $CacheDir did not satisfy the extract path: stderr reports a model-cache miss: $($result.Stderr)"
}
if ($result.Stderr -notmatch "Layout detection completed") {
  Fail "the layout path never ran, so the bundled models were not exercised (no 'Layout detection completed' on stderr): $($result.Stderr)"
}
Write-Host "smoke=ok kind=positive content_len=$($content.Length) cache=$CacheDir"
exit 0
