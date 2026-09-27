#!/usr/bin/env pwsh
<#
.SYNOPSIS
Offline smoke test for a staged Windows CLI bundle: proves the bundled models
and native libraries are what the binary actually loads.

.DESCRIPTION
Runs against the staged bundle with -CleanPath on PATH, HF_HUB_OFFLINE=1 and
HF_HUB_CACHE set to -CacheDir. One mode, three probes each; every probe's
environment is applied per probe and restored afterwards.

Default (positive) mode -- the bundle must work offline:
  extract-ocr      `xberg extract <Fixture> --format json` with the inline
                   `ocr` config. Asserts exit 0, parseable JSON, non-empty
                   result.content, a `PaddleOCR engine initialized
                   successfully` line on stderr and no offline model-cache
                   miss -- document extraction works off the bundle's HF cache.
  snapshot-ocr     `xberg snapshot-ocr --image <Fixture> --json`. Asserts exit
                   0 and non-empty layout text. Models are resolved through the
                   packaged `<exe dir>/models/snapshot-ocr` layout (no flag, no
                   env override), ORT_DYLIB_PATH points at the bundle root
                   onnxruntime.dll -- so the probe proves the staged snapshot
                   model set and the shipped ONNX Runtime load offline.
  transcription    `xberg extract <AudioFixture> --format json` with the inline
                   `transcription` config. Asserts exit 0 and SV-06 structure
                   in result.content reporting "no audio track" or "no speech
                   detected" -- the bundled sample is 1s of silence, so the
                   full FFmpeg DLL decode -> Silero VAD -> SenseVoice chain
                   runs end to end without producing a fake transcript.
                   XBERG_SENSEVOICE_MODEL_DIR / XBERG_SHERPA_DLL_DIR /
                   XBERG_FFMPEG_DLL_DIR point into the bundle; the sherpa DLL
                   loads its same-directory onnxruntime.dll, so ORT_DYLIB_PATH
                   is deliberately not set for this probe.

-ExpectEmptyCacheFailure -- negative probes; each must report its missing-model
diagnostic (the exit code is deliberately not asserted: the diagnostic, not the
exit code, is the contract -- a run that never reports a miss is not reading
the cache under test):
  extract-ocr      the HF offline model-cache miss ("offline mode").
  snapshot-ocr     XBERG_SNAPSHOT_MODEL_DIR pointed at a directory that does
                   not exist -> the snapshot missing-model diagnostic naming
                   the model set and the expected det/rec/dict layout.
  transcription    XBERG_SENSEVOICE_MODEL_DIR pointed at a directory that does
                   not exist -> the SenseVoice missing-model diagnostic.

One `smoke=ok ...` line per passed probe on stdout and exit 0 when all pass;
otherwise the reason goes to stderr and the exit code is 1.
package-cli-windows.ps1 runs it twice, as two of its parallel validation
probes.

.EXAMPLE
pwsh -NoProfile -File scripts/publish/cli/offline-smoke.ps1 -Exe xberg-cli-x86_64-pc-windows-msvc/xberg.exe -Fixture fixtures/images/test_hello_world.png -AudioFixture xberg-cli-x86_64-pc-windows-msvc/samples/silence-1s.wav -CacheDir xberg-cli-x86_64-pc-windows-msvc/models -CleanPath $env:PATH
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [Parameter(Mandatory = $true)][string]$Fixture,
  # Bundled 1s silence wav the transcription probe extracts (staged to
  # <bundle>/samples/silence-1s.wav by package-cli-windows.ps1).
  [Parameter(Mandatory = $true)][string]$AudioFixture,
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

# Run one probe with per-probe environment variables layered over the
# process-wide offline environment (applied before, restored after, so a
# variable meant for one channel cannot leak into the next probe).
function Invoke-Probe([string]$Name, [string[]]$ProbeArguments, [hashtable]$ProbeEnv) {
  $saved = @{}
  foreach ($key in $ProbeEnv.Keys) {
    $saved[$key] = [System.Environment]::GetEnvironmentVariable($key, "Process")
    [System.Environment]::SetEnvironmentVariable($key, $ProbeEnv[$key], "Process")
  }
  try {
    $result = Invoke-Captured -FilePath $Exe -Arguments $ProbeArguments
  }
  finally {
    foreach ($key in $saved.Keys) {
      # A $null value deletes the variable: probes that only clear a var (the
      # transcription probe must not inherit ORT_DYLIB_PATH) restore that state.
      [System.Environment]::SetEnvironmentVariable($key, $saved[$key], "Process")
    }
  }
  Write-Host "  probe $Name exit=$($result.ExitCode)"
  return $result
}

# Extract the outermost JSON object from a probe's stdout and parse it.
function Get-JsonPayload([string]$Stdout, [string]$ProbeName) {
  if ([string]::IsNullOrEmpty($Stdout)) {
    Fail "$ProbeName wrote nothing to stdout (exit 0), so the JSON result is unreadable"
  }
  $start = $Stdout.IndexOf('{')
  $end = $Stdout.LastIndexOf('}')
  if ($start -lt 0 -or $end -le $start) {
    Fail "$ProbeName produced no JSON on stdout: $Stdout"
  }
  try {
    return ($Stdout.Substring($start, $end - $start + 1) | ConvertFrom-Json)
  }
  catch {
    Fail "could not parse the $ProbeName JSON on stdout: $($_.Exception.Message)"
  }
}

if (-not (Test-Path -LiteralPath $Exe)) { Fail "no binary at $Exe" }
if (-not (Test-Path -LiteralPath $Fixture)) { Fail "no fixture at $Fixture" }
if (-not (Test-Path -LiteralPath $AudioFixture)) { Fail "no audio fixture at $AudioFixture" }

# Process-wide on purpose: every probe is a child of this process and has to
# see exactly this offline environment. Per-probe variables are layered and
# restored by Invoke-Probe.
$env:PATH = $CleanPath
$env:HF_HUB_CACHE = $CacheDir
$env:HF_HUB_OFFLINE = "1"
$env:HUGGINGFACE_HUB_OFFLINE = "1"

# Bundle-relative locations (the staged tree is the unit under test).
$bundleRoot = Split-Path -Parent $Exe
$modelsDir = Join-Path $bundleRoot "models"
$sherpaDir = Join-Path $bundleRoot "sherpa-onnx"
$ffmpegDir = Join-Path $bundleRoot "ffmpeg"
$bundleOrtDll = Join-Path $bundleRoot "onnxruntime.dll"

# base64 of {"ocr":{"enabled":true}} / {"transcription":{"enabled":true}} --
# inline JSON would need quoting through the native argv handoff; the base64
# form is exactly one argument with no escaping. The `ocr` section pins the
# document OCR path on, which is what makes the bundled PaddleOCR files part of
# the extract path; `transcription` routes the audio fixture through the
# FFmpeg DLL -> Silero VAD -> SenseVoice chain (the fork compiles no
# layout/table models).
$ocrConfigB64 = "eyJvY3IiOnsiZW5hYmxlZCI6dHJ1ZX19"
$transcriptionConfigB64 = "eyJ0cmFuc2NyaXB0aW9uIjp7ImVuYWJsZWQiOnRydWV9fQ=="

# A path that does not exist, for the negative per-channel probes: the CLI
# treats an explicitly set but unusable model directory as a hard error with
# the missing-model diagnostic (no silent fallback).
$missingModelsDir = Join-Path ([System.IO.Path]::GetTempPath()) ("xberg-smoke-missing-models-" + [Guid]::NewGuid().ToString("N"))

if ($ExpectEmptyCacheFailure) {
  # Negative mode: each probe must report its missing-model diagnostic. Exit
  # codes are not asserted (same rationale as the empty-cache probe has always
  # had: the diagnostic is the contract).

  # 1. Document OCR: with an empty HF cache and offline mode forced, the run
  # must report the offline model-cache miss instead of silently downloading.
  $result = Invoke-Probe -Name "empty-cache-document-ocr" `
    -ProbeArguments @("extract", $Fixture, "--format", "json", "--config-json-base64", $ocrConfigB64) `
    -ProbeEnv @{}
  if ($result.Stderr -notmatch "offline mode") {
    Fail "the bundled models are not what the smoke test loaded: with HF_HUB_CACHE=$CacheDir and offline mode forced, the run exited $($result.ExitCode) without reporting a model-cache miss"
  }
  Write-Host "smoke=ok kind=empty-cache-document-ocr exit=$($result.ExitCode) cache=$CacheDir"

  # 2. Snapshot OCR: an explicitly set but missing snapshot model root must
  # produce the snapshot missing-model diagnostic naming the model set layout.
  $result = Invoke-Probe -Name "empty-cache-snapshot-ocr" `
    -ProbeArguments @("snapshot-ocr", "--image", $Fixture, "--json") `
    -ProbeEnv @{ XBERG_SNAPSHOT_MODEL_DIR = $missingModelsDir; ORT_DYLIB_PATH = $bundleOrtDll }
  if ($result.Stderr -notmatch "截图.*模型根目录") {
    Fail "the snapshot-ocr probe did not report the snapshot missing-model diagnostic (expected the XBERG_SNAPSHOT_MODEL_DIR diagnostic on stderr): $($result.Stderr)"
  }
  Write-Host "smoke=ok kind=empty-cache-snapshot-ocr exit=$($result.ExitCode)"

  # 3. Transcription: a missing SenseVoice model root must produce the
  # SenseVoice missing-model diagnostic (before any DLL or decode work).
  $result = Invoke-Probe -Name "empty-cache-transcription" `
    -ProbeArguments @("extract", $AudioFixture, "--format", "json", "--config-json-base64", $transcriptionConfigB64) `
    -ProbeEnv @{
      XBERG_SENSEVOICE_MODEL_DIR = $missingModelsDir
      XBERG_SHERPA_DLL_DIR       = $sherpaDir
      XBERG_FFMPEG_DLL_DIR       = $ffmpegDir
    }
  if ($result.Stderr -notmatch "XBERG_SENSEVOICE_MODEL_DIR") {
    Fail "the transcription probe did not report the SenseVoice missing-model diagnostic (expected the XBERG_SENSEVOICE_MODEL_DIR diagnostic on stderr): $($result.Stderr)"
  }
  Write-Host "smoke=ok kind=empty-cache-transcription exit=$($result.ExitCode)"
  exit 0
}

# ---- Positive probe 1: document extraction + PaddleOCR off the bundle cache.
$result = Invoke-Probe -Name "extract-ocr" `
  -ProbeArguments @("extract", $Fixture, "--format", "json", "--config-json-base64", $ocrConfigB64) `
  -ProbeEnv @{}
if ($result.ExitCode -ne 0) {
  Fail "extraction failed (exit $($result.ExitCode)): $($result.Stderr)"
}
$document = Get-JsonPayload -Stdout $result.Stdout -ProbeName "extract-ocr"
$content = $document.result.content
if ([string]::IsNullOrWhiteSpace($content)) {
  Fail "extraction produced empty content with HF_HUB_CACHE=$CacheDir"
}
# The config pins the OCR path on, so this run resolved the bundled PaddleOCR models
# through the cache under test. A miss here means the staged cache did not satisfy the
# extract path; no `PaddleOCR engine initialized successfully` line means the OCR path
# never ran and nothing was proven.
if ($result.Stderr -match "offline mode") {
  Fail "the staged cache at $CacheDir did not satisfy the extract path: stderr reports a model-cache miss: $($result.Stderr)"
}
if ($result.Stderr -notmatch "PaddleOCR engine initialized successfully") {
  Fail "the OCR path never ran, so the bundled models were not exercised (no 'PaddleOCR engine initialized successfully' on stderr): $($result.Stderr)"
}
Write-Host "smoke=ok kind=positive-extract-ocr content_len=$($content.Length) cache=$CacheDir"

# ---- Positive probe 2: snapshot OCR off the packaged models/snapshot-ocr
# layout (resolved exe-adjacent: no --models flag, no env override) with the
# bundle root onnxruntime.dll (api-18 compatible, the same DLL paddle loads).
if (-not (Test-Path -LiteralPath $bundleOrtDll)) {
  Fail "the bundle root onnxruntime.dll is missing ($bundleOrtDll); the snapshot channel loads ORT through ORT_DYLIB_PATH"
}
$result = Invoke-Probe -Name "snapshot-ocr" `
  -ProbeArguments @("snapshot-ocr", "--image", $Fixture, "--json") `
  -ProbeEnv @{ ORT_DYLIB_PATH = $bundleOrtDll }
if ($result.ExitCode -ne 0) {
  Fail "snapshot-ocr failed (exit $($result.ExitCode)): $($result.Stderr)"
}
$payload = Get-JsonPayload -Stdout $result.Stdout -ProbeName "snapshot-ocr"
if ([string]::IsNullOrWhiteSpace($payload.text)) {
  Fail "snapshot-ocr produced empty layout text for the fixture, so the packaged models/snapshot-ocr set was not proven usable: $($result.Stderr)"
}
Write-Host "smoke=ok kind=positive-snapshot-ocr text_len=$(([string]$payload.text).Length) records=$(@($payload.records).Count)"

# ---- Positive probe 3: media transcription off the bundled models/ +
# sherpa-onnx/ + ffmpeg/ layout. The sample is silence, so the asserted
# outcome is the SV-06 structure reporting "no speech detected" (or "no audio
# track"); a transcript here would mean the sample was swapped, and a
# missing-asset diagnostic fails the exit-code check first. ORT_DYLIB_PATH is
# deliberately cleared: the sherpa loader pins its same-directory
# onnxruntime.dll, and this probe must prove that path works.
$result = Invoke-Probe -Name "transcription" `
  -ProbeArguments @("extract", $AudioFixture, "--format", "json", "--config-json-base64", $transcriptionConfigB64) `
  -ProbeEnv @{
    XBERG_SENSEVOICE_MODEL_DIR = $modelsDir
    XBERG_SHERPA_DLL_DIR       = $sherpaDir
    XBERG_FFMPEG_DLL_DIR       = $ffmpegDir
    ORT_DYLIB_PATH             = $null
  }
if ($result.ExitCode -ne 0) {
  Fail "transcription extraction failed (exit $($result.ExitCode)): $($result.Stderr)"
}
$document = Get-JsonPayload -Stdout $result.Stdout -ProbeName "transcription"
$content = $document.result.content
if ([string]::IsNullOrWhiteSpace($content)) {
  Fail "transcription extraction produced empty content with the bundled media assets: $($result.Stderr)"
}
if ($content -notmatch '音频时长') {
  Fail "transcription output is missing the SV-06 duration line (content did not contain 音频时长): $content"
}
if ($content -notmatch '未检测到语音|无音频轨道') {
  Fail "transcription on the bundled silence sample produced neither '未检测到语音' nor '无音频轨道' (unexpected transcript?): $content"
}
Write-Host "smoke=ok kind=positive-transcription content_len=$($content.Length)"
exit 0
