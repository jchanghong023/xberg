#!/usr/bin/env pwsh
<#
.SYNOPSIS
Builds the Windows CLI release bundle: xberg.exe + sibling DLLs + the default
model set + a launcher, zipped as xberg-cli-<target>.zip.

.DESCRIPTION
Single source of truth for "build + package" on Windows. Used locally and by
.github/workflows/build-windows-cli.yml; the workflow continues afterwards with
artifact upload, version stamping and `gh release create`.

The cargo feature set matches the fork's development build: document formats
(no HEIC), analysis, CLI core, Tesseract OCR as a fallback, PaddleOCR
(pp-ocrv6 tiny — the default OCR backend), audio/video transcription,
layout detection with TATR table structure recognition, plus the `xberg serve`
HTTP API server (axum).
`--no-default-features` drops the heavy default stacks (embeddings,
candle-VLM). Deliberately excluded: heic (no stock Windows libheif build path),
pdfium, mcp, embedding/NER. Whisper tiny is bundled so video/audio inputs
transcribe offline; the two layout models the image/PDF path resolves
(RT-DETR 161.3 MiB + TATR 28.8 MiB) and the PaddleOCR pp-ocrv6 tiny set
(~13 MiB: det + rec + dict + textline cls) are bundled so image tables,
layout regions, and OCR work offline.

Before zipping, the staged tree must pass: a cleaned-PATH `--version` probe,
in-tree MSVC CRT deployment, the shared PE import-closure gate
(scripts/ci/verify-windows-dll-closure.ps1 -RequireImportClosure), and an
offline smoke test that proves the bundled models are what the binary loads --
the same command against an empty cache must fail instead of silently
downloading.

Every stage that can overlap does, and all of it is throttled by -Jobs: the
pinned Whisper tiny files race the cargo build into
target/package-models-<target>; the stage-directory writes
and the validation probes run concurrently. While any of that runs the script
samples system CPU and, at the end,
writes the sampling log and a "quiet window" report to
target/package-cpu-<timestamp>.csv and -summary.txt, so a stage that leaves the
machine idle can be located from its timestamps alone. The report is diagnostic
only -- it never changes the exit code (-NoCpuMonitor turns sampling off).

.EXAMPLE
pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1

Bare invocation. On GitHub Actions this stays at the 6-job runner default.
Locally it auto-selects min(30, logical cores) so a workstation is not
pinned to the pipeline's 6.

.EXAMPLE
pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1 -Target x86_64-pc-windows-msvc -Jobs 8

Local override when you want a specific parallelism instead of the auto
local/cap choice. Explicit -Jobs always wins over auto-detection.
#>

# Start-Process -Environment (validation probes) needs PowerShell 7.4; fail fast
# instead of dying in the Validate phase after the build already ran.
#Requires -Version 7.4
[CmdletBinding()]
param(
  [string]$Target = "x86_64-pc-windows-msvc",
  # 0 = auto. Resolved right after param() to either the pipeline's 6
  # (GITHUB_ACTIONS) or min(30, [Environment]::ProcessorCount) on a
  # developer machine. .github/workflows/build-windows-cli.yml still passes
  # no -Jobs, so CI behavior is unchanged.
  [int]$Jobs = 0,
  [string]$OrtVersion = "1.24.2",
  # CPU sampling: one sample every -MonitorIntervalSec, a run of samples below
  # -QuietCpuPct lasting -QuietSeconds or longer is reported as a quiet window.
  # Anything at or under 20% of the machine is treated as a stalled/serial stage.
  # -MonitorCsv defaults to target/package-cpu-<timestamp>.csv (<summary.txt>).
  [int]$MonitorIntervalSec = 5,
  [int]$QuietCpuPct = 20,
  [int]$QuietSeconds = 20,
  [string]$MonitorCsv = "",
  [switch]$NoCpuMonitor,
  # Skip the post-compress `7z t` integrity pass. CI keeps the default (test);
  # local iterations can opt out to avoid reading the whole zip back.
  [switch]$SkipZipTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Resolve -Jobs=0 (auto) before any stage consumes it. CI (GITHUB_ACTIONS)
# keeps the runner-sized 6 so build-windows-cli.yml stays on its historical
# parallelism; a local bare invocation uses min(30, logical cores). An
# explicit -Jobs still overrides both.
if ($Jobs -le 0) {
  $Jobs = if ($env:GITHUB_ACTIONS) { 6 } else { [Math]::Min(30, [Environment]::ProcessorCount) }
}

$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "../../..")).Path
$StageName = "xberg-cli-$Target"
$Stage = Join-Path $RepoRoot $StageName
$ZipPath = Join-Path $RepoRoot "$StageName.zip"
$StageExe = Join-Path $Stage "xberg.exe"
$ModelsRoot = Join-Path $Stage "models"
# Persistent local model cache (same HF layout as the bundle). Whisper + any
# already-verified OCR/layout file is reused across runs; the Stage wipe never
# touches this directory.
$ModelCacheRoot = Join-Path $RepoRoot "target/package-models-$Target"

# Keep in sync with AGENTS.md「编译」小节. Fork scope: file→Markdown + OCR +
# transcription + layout/table structure + HTTP API (`xberg serve`). No
# heic/pdfium/candle/mcp/embeddings/NER. PaddleOCR (pp-ocrv6 tiny) is the
# default OCR backend; Tesseract stays compiled as a fallback.
$Features = @(
  "formats-no-heic"
  "analysis"
  "core-cli"
  "ocr"
  "paddle-ocr"
  "transcription"
  "layout-detection"
  "api"
)

# Layout ONNX models staged from the CLI's own `cache manifest` (checksums and
# sizes come from the binary, so they cannot drift). Only the two models the
# image/PDF extraction path actually resolves are selected -- RT-DETR (layout
# regions, 161.3 MiB) and TATR (table structure, 28.8 MiB); the manifest also
# lists SLANeXT/SLANet_plus/table-classifier/pp_doclayout_v3, but those are for
# table models and layout backends this build does not use, and each spec below
# must match exactly one entry. Whisper tiny is staged separately via
# $TranscriptionFiles (not listed by `cache manifest`).
# PaddleOCR pp-ocrv6 tiny (~13 MiB): det + rec + dict + the v2 textline
# orientation classifier the v6 path still resolves. `small`/`medium`
# det/rec entries stay out of the bundle — only the default tier ships.
$RequiredModels = @(
  @{ Label = "layout RT-DETR"; Regex = 'models--xberg-io--layout-models/snapshots/[0-9a-f]+/rtdetr/model\.onnx$' }
  @{ Label = "layout TATR"; Regex = 'models--xberg-io--layout-models/snapshots/[0-9a-f]+/tatr/model\.onnx$' }
  @{ Label = "paddle det tiny"; Regex = '^v6/det/tiny/model\.onnx$' }
  @{ Label = "paddle rec tiny"; Regex = '^v6/rec/tiny/model\.onnx$' }
  @{ Label = "paddle dict tiny"; Regex = '^v6/rec/tiny/dict\.txt$' }
  @{ Label = "paddle cls"; Regex = '^v2/classifiers/PP-LCNet_x1_0_textline_ori\.onnx$' }
)

# The transcription model the runtime resolves for `transcription.model = "tiny"`
# (video/audio extraction). `xberg cache manifest` does not list it -- the CLI
# resolves it through hf-hub by repository name at use time -- so the files are
# pinned here instead: repository revision plus size and SHA256 for every file,
# staged into the HF cache layout hf-hub reads. Bundled because the requirement
# is that a shipped bundle transcribes offline: without these five files the
# first video input attempts a download instead of transcribing.
$TranscriptionRepo = "onnx-community/whisper-tiny"
$TranscriptionRevision = "ff4177021cc41f7db950912b73ea4fdf7d01d8e7"
$TranscriptionFiles = @(
  @{ Path = "config.json"; SizeBytes = 2243; Sha256 = "46aeea0a406afbeb563fc8e59ca10609203df4299af6a83f73752fef369efd2d" }
  @{ Path = "tokenizer.json"; SizeBytes = 2480466; Sha256 = "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566" }
  @{ Path = "onnx/encoder_model.onnx"; SizeBytes = 32904992; Sha256 = "6642befb640f950d4a8cbbd17834d59e7e75f575b81ccf213e06b050623ab1dd" }
  @{ Path = "onnx/decoder_model.onnx"; SizeBytes = 118397483; Sha256 = "ab79e3f2a9a3d98f159f853a3172120a38af7eb5f7863d706aa7d39c228f009e" }
  @{ Path = "onnx/decoder_with_past_model.onnx"; SizeBytes = 113638998; Sha256 = "0485135066eb1d36dcb04dbabd0cc1141c7cd8c442217abd0798d55fe2bc6bed" }
)

# MSVC runtime DLLs the loader needs beside the binary when a shipped PE imports
# them (Rust/MSVC do not embed the C++ runtime). Matched by prefix so
# msvcp140_atomic_wait.dll / vcruntime140_threads.dll are covered too.
$CrtDllPattern = '^(vcruntime140|msvcp140|concrt140|vccorlib140)'

# CPU monitoring state, wired by Start-CpuMonitor. $null while monitoring is off,
# which keeps Write-Phase a plain Write-Host.
$script:CpuMarkers = $null
$script:CpuCsv = $null

function Write-Phase([string]$Name) {
  Write-Host ""
  Write-Host "==> $Name" -ForegroundColor Cyan
  # Phase boundaries reach the sampling log through the monitor thread -- the
  # only writer of the CSV -- so a marker can never interleave with a sample.
  if ($script:CpuMarkers) {
    $script:CpuMarkers.Enqueue([pscustomobject]@{ Time = (Get-Date -Format "HH:mm:ss"); Phase = $Name })
  }
}

function Write-Step([string]$Message) {
  Write-Phase $Message
}

function Invoke-Native([string]$FilePath, [string[]]$Arguments, [string]$WorkingDirectory) {
  Push-Location $WorkingDirectory
  try {
    & $FilePath @Arguments
    $code = $LASTEXITCODE
  }
  finally {
    Pop-Location
  }
  if ($code -ne 0) {
    throw "'$FilePath $($Arguments -join ' ')' failed with exit code $code"
  }
}

function Start-CpuMonitor([int]$IntervalSec, [string]$CsvPath) {
  # Samples system CPU from a background thread job and drains the phase-marker
  # queue written by Write-Phase, so the CSV has exactly one writer. Returns the
  # job; Stop-CpuMonitor takes it down.
  $script:CpuMarkers = [System.Collections.Concurrent.ConcurrentQueue[object]]::new()
  # The CSV lands under target\, which does not exist yet on a cold checkout or
  # a CI run whose cache restore missed: Set-Content would not create the
  # directory, and with $ErrorActionPreference = "Stop" the whole package run
  # would die here, before cargo even starts.
  $csvDir = Split-Path -Parent $CsvPath
  if ($csvDir -and -not (Test-Path -LiteralPath $csvDir)) {
    New-Item -ItemType Directory -Path $csvDir -Force | Out-Null
  }
  Set-Content -LiteralPath $CsvPath -Value "kind,time,phase,load_pct,rustc_n,cargo_n,sevenzip_n,pwsh_n"
  $queue = $script:CpuMarkers
  return Start-ThreadJob -ArgumentList $IntervalSec, $CsvPath, $queue -ScriptBlock {
    param([int]$Interval, [string]$Path, [object]$Markers)
    $ProgressPreference = "SilentlyContinue"
    while ($true) {
      # The WMI query below costs about a second, so the loop sleeps only what
      # is left of the interval: otherwise the samples drift to ~2x the
      # requested spacing and Get-CpuReport's "one sample = one interval"
      # arithmetic understates every quiet window.
      $tick = [System.Diagnostics.Stopwatch]::StartNew()
      $marker = $null
      while ($Markers.TryDequeue([ref]$marker)) {
        Add-Content -LiteralPath $Path -Value ("marker,{0},{1},,,,," -f $marker.Time, $marker.Phase)
      }
      # A failed WMI query must not end the sampling: an empty load cell keeps the
      # timeline (and therefore every timestamp after it) intact.
      $load = ""
      try {
        $load = [int](Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average
      }
      catch {
      }
      $counts = @(foreach ($name in @("rustc", "cargo", "7z", "pwsh")) {
          @(Get-Process -Name $name -ErrorAction SilentlyContinue).Count
        })
      Add-Content -LiteralPath $Path -Value (
        "sample,{0},,{1},{2},{3},{4},{5}" -f (Get-Date -Format "HH:mm:ss"), $load, $counts[0], $counts[1], $counts[2], $counts[3]
      )
      $remaining = $Interval - [int]$tick.Elapsed.TotalSeconds
      if ($remaining -gt 0) { Start-Sleep -Seconds $remaining }
    }
  }
}

function Stop-CpuMonitor([object]$Job) {
  if ($Job) {
    Stop-Job -Job $Job -ErrorAction SilentlyContinue
    Remove-Job -Job $Job -Force -ErrorAction SilentlyContinue
  }
  # Markers enqueued after the monitor's last tick still deserve a line, and the
  # job that owned the file is gone by now, so writing them here is safe.
  if (-not $script:CpuMarkers -or [string]::IsNullOrWhiteSpace($script:CpuCsv)) { return }
  if (-not (Test-Path -LiteralPath $script:CpuCsv)) { return }
  $marker = $null
  while ($script:CpuMarkers.TryDequeue([ref]$marker)) {
    Add-Content -LiteralPath $script:CpuCsv -Value ("marker,{0},{1},,,,," -f $marker.Time, $marker.Phase)
  }
}

function Get-CpuReport([string]$CsvPath, [string]$SummaryPath, [int]$IntervalSec, [int]$QuietPct, [int]$QuietSeconds) {
  # Diagnostic only: every failure path here warns and leaves the exit code
  # alone, including a missing log or a log with no usable sample in it.
  try {
    if (-not (Test-Path -LiteralPath $CsvPath)) {
      Write-Warning "cpu report: no sampling log at $CsvPath"
      return
    }
    $rows = @(Import-Csv -LiteralPath $CsvPath)

    # Attribute each sample to the phase whose marker precedes it, then read the
    # report off that timeline instead of the raw rows.
    $timeline = [System.Collections.Generic.List[object]]::new()
    $phase = "(before first phase)"
    foreach ($row in $rows) {
      if ($row.kind -eq "marker") { $phase = $row.phase; continue }
      if ($row.kind -ne "sample") { continue }
      $load = 0
      if (-not [int]::TryParse($row.load_pct, [ref]$load)) { continue }
      $rustc = 0
      [void][int]::TryParse($row.rustc_n, [ref]$rustc)
      $timeline.Add([pscustomobject]@{ Time = $row.time; Phase = $phase; Load = $load; Rustc = $rustc })
    }
    if ($timeline.Count -eq 0) {
      Write-Warning "cpu report: $CsvPath has no samples"
      return
    }

    $stats = [ordered]@{}
    foreach ($sample in $timeline) {
      if (-not $stats.Contains($sample.Phase)) {
        $stats[$sample.Phase] = [pscustomobject]@{ Samples = 0; LoadSum = 0.0; Peak = 0; RustcSum = 0.0 }
      }
      $phaseStats = $stats[$sample.Phase]
      $phaseStats.Samples++
      $phaseStats.LoadSum += $sample.Load
      $phaseStats.RustcSum += $sample.Rustc
      if ($sample.Load -gt $phaseStats.Peak) { $phaseStats.Peak = $sample.Load }
    }

    # A quiet window is a run of consecutive samples below the threshold; each
    # sample stands for one -MonitorIntervalSec of wall clock.
    $quiet = [System.Collections.Generic.List[object]]::new()
    $run = $null
    foreach ($sample in $timeline) {
      if ($sample.Load -ge $QuietPct) { $run = $null; continue }
      if (-not $run) {
        $run = [pscustomobject]@{
          Start  = $sample.Time
          End    = $sample.Time
          Count  = 0
          Phases = [System.Collections.Generic.List[string]]::new()
        }
        $quiet.Add($run)
      }
      $run.Count++
      $run.End = $sample.Time
      if (-not $run.Phases.Contains($sample.Phase)) { $run.Phases.Add($sample.Phase) }
    }

    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add("cpu report: $CsvPath (one sample every ${IntervalSec}s; quiet = load < ${QuietPct}% for >= ${QuietSeconds}s)")
    $lines.Add("")
    $lines.Add(("  {0,-24} {1,7} {2,8} {3,8} {4,10}" -f "phase", "samples", "avg %", "peak %", "avg rustc"))
    foreach ($key in $stats.Keys) {
      $phaseStats = $stats[$key]
      $lines.Add(("  {0,-24} {1,7} {2,8:N1} {3,8} {4,10:N2}" -f $key, $phaseStats.Samples, ($phaseStats.LoadSum / $phaseStats.Samples), $phaseStats.Peak, ($phaseStats.RustcSum / $phaseStats.Samples)))
    }
    $lines.Add("")
    $windowLines = foreach ($window in $quiet) {
      $seconds = $window.Count * $IntervalSec
      $kind = "under"
      if ($seconds -ge $QuietSeconds) { $kind = "QUIET" }
      ("  {0,-5} {1,4}s  {2}-{3}  {4}" -f $kind, $seconds, $window.Start, $window.End, ($window.Phases -join ", "))
    }
    if ($windowLines) {
      $lines.Add("  windows below ${QuietPct}% load:")
      foreach ($line in $windowLines) { $lines.Add($line) }
    }
    else {
      $lines.Add("  windows below ${QuietPct}% load: none")
    }

    foreach ($line in $lines) { Write-Host $line }
    if ($SummaryPath) {
      Set-Content -LiteralPath $SummaryPath -Value $lines
      Write-Host "cpu summary: $SummaryPath"
    }
  }
  catch {
    Write-Warning "cpu report failed: $($_.Exception.Message)"
  }
}

function Get-CleanPath([string]$DirectoryToDrop) {
  # Same rule as the workflow used inline: PATH entries inside the workspace
  # (ORT staging, target dirs) or providing onnxruntime.dll must not mask a DLL
  # missing from the bundle.
  $kept = foreach ($entry in ($env:PATH -split ';' | Where-Object { $_ -ne '' })) {
    $resolved = try { [System.IO.Path]::GetFullPath($entry) } catch { $entry }
    if ($resolved -eq $DirectoryToDrop -or
        $resolved.StartsWith($DirectoryToDrop + [System.IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
      Write-Host "  dropping PATH entry inside the repo: $entry"
      continue
    }
    if (Test-Path -LiteralPath (Join-Path $entry "onnxruntime.dll")) {
      Write-Host "  dropping PATH entry providing onnxruntime.dll: $entry"
      continue
    }
    $entry
  }
  return ($kept -join ';')
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

function Get-ModelTargetPath([string]$RelativePath, [string]$SourceUrl, [string]$ModelsRoot) {
  # Hugging Face content-addressed cache layout. Layout/NER manifests already
  # return the snapshot-relative path; PaddleOCR returns a repository-relative
  # path whose snapshot location has to come from the pinned resolve URL.
  if ($RelativePath.StartsWith("models--", [StringComparison]::Ordinal)) {
    return (Join-Path $ModelsRoot $RelativePath.Replace('/', '\'))
  }
  $match = [regex]::Match($SourceUrl, '^https://huggingface\.co/(?<owner>[^/]+)/(?<repo>[^/]+)/resolve/(?<rev>[0-9a-f]{40})/(?<path>.+)$')
  if (-not $match.Success) {
    throw "cannot derive the Hugging Face cache layout for '$RelativePath' from source_url '$SourceUrl'"
  }
  $relative = "models--$($match.Groups['owner'].Value)--$($match.Groups['repo'].Value)/snapshots/$($match.Groups['rev'].Value)/$($match.Groups['path'].Value)"
  return (Join-Path $ModelsRoot $relative.Replace('/', '\'))
}

function Test-ModelFile([string]$Path, [string]$Sha256, [int64]$SizeBytes) {
  if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
  if ($SizeBytes -gt 0 -and (Get-Item -LiteralPath $Path).Length -ne $SizeBytes) { return $false }
  return ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -eq $Sha256)
}

function Get-RequiredModelEntries([string]$Exe, [string]$ModelsRoot) {
  # No layout models requested (lean feature set); Whisper is handled by
  # Get-TranscriptionModelEntries. Skip the cache-manifest probe entirely.
  if ($RequiredModels.Count -eq 0) {
    return @()
  }
  # This may run against the target-dir exe (before Stage copies ORT dlls next
  # to it). Put ORT_LIB_LOCATION on PATH so the binary can load onnxruntime.
  $savedPath = $env:PATH
  if (-not [string]::IsNullOrWhiteSpace($env:ORT_LIB_LOCATION)) {
    $env:PATH = $env:ORT_LIB_LOCATION + [System.IO.Path]::PathSeparator + $env:PATH
  }
  try {
    $manifest = Invoke-Captured -FilePath $Exe -Arguments @("cache", "manifest", "--format", "json")
  }
  finally {
    $env:PATH = $savedPath
  }
  if ($manifest.ExitCode -ne 0) {
    throw "'$Exe cache manifest --format json' failed with exit code $($manifest.ExitCode): $($manifest.Stderr)"
  }
  $parsed = $manifest.Stdout | ConvertFrom-Json
  $models = @($parsed.models)
  if ($models.Count -eq 0) { throw "the model manifest is empty" }

  $selected = foreach ($spec in $RequiredModels) {
    $hits = @($models | Where-Object { $_.relative_path -match $spec.Regex })
    if ($hits.Count -ne 1) {
      throw "expected exactly one model manifest entry matching '$($spec.Regex)' ($($spec.Label)), found $($hits.Count). Re-run '$Exe cache manifest --format json' and update scripts/publish/cli/package-cli-windows.ps1."
    }
    $entry = $hits[0]
    [pscustomobject]@{
      Label        = $spec.Label
      RelativePath = $entry.relative_path
      Sha256       = $entry.sha256
      SizeBytes    = [int64]$entry.size_bytes
      Url          = $entry.source_url
      Target       = (Get-ModelTargetPath -RelativePath $entry.relative_path -SourceUrl $entry.source_url -ModelsRoot $ModelsRoot)
    }
  }
  return @($selected)
}

function Get-TranscriptionModelEntries([string]$ModelsRoot) {
  # Same shape as Get-RequiredModelEntries, but the file list comes from the
  # pinned $TranscriptionFiles instead of the CLI manifest. Target paths are the
  # Hugging Face cache layout: models--<owner>--<repo>/snapshots/<rev>/<file>.
  $cacheRepo = "models--" + $TranscriptionRepo.Replace('/', '--')
  $snapshot = Join-Path (Join-Path $ModelsRoot $cacheRepo) ("snapshots/" + $TranscriptionRevision)
  $entries = foreach ($file in $TranscriptionFiles) {
    [pscustomobject]@{
      Label        = "Whisper tiny $($file.Path)"
      RelativePath = "$cacheRepo/snapshots/$TranscriptionRevision/$($file.Path)"
      Sha256       = $file.Sha256
      SizeBytes    = [int64]$file.SizeBytes
      Url          = "https://huggingface.co/$TranscriptionRepo/resolve/$TranscriptionRevision/$($file.Path)"
      Target       = Join-Path $snapshot ($file.Path.Replace('/', '\'))
    }
  }
  return @($entries)
}

# Shared download worker. A ThreadJob cannot dot-source this script or call its
# functions, so the install body lives here once; the pipeline drives it with
# Start-ModelDownloadJob + Complete-ModelInstall directly. (The synchronous
# Install-Models wrapper below is kept as a utility but has no call site.)
function Start-ModelDownloadJob([object[]]$Plan, [int]$Throttle, [string]$LocalCache) {
  return Start-ThreadJob -ArgumentList $Plan, $Throttle, $LocalCache -ScriptBlock {
    param([object[]]$Plan, [int]$Throttle, [string]$LocalCache)
    $ProgressPreference = "SilentlyContinue"
    if (-not $Plan -or $Plan.Count -eq 0) { return @() }
    # Copy into a local for $using: — nested ForEach-Object -Parallel cannot
    # reliably bind $using: to a ThreadJob param across runspaces.
    $cacheRootShared = $LocalCache
    return @($Plan | ForEach-Object -Parallel {
      $ProgressPreference = "SilentlyContinue"
      $edge = $_
      $target = $edge.Target
      $dir = Split-Path -Parent $target
      New-Item -ItemType Directory -Path $dir -Force | Out-Null

      $cacheRoot = $using:cacheRootShared
      if ($cacheRoot) {
        $cached = Join-Path $cacheRoot ($edge.RelativePath -replace '/', '\')
        if (Test-Path -LiteralPath $cached -PathType Leaf) {
          $cacheHash = (Get-FileHash -LiteralPath $cached -Algorithm SHA256).Hash
          $cacheLen = (Get-Item -LiteralPath $cached).Length
          if ($cacheHash -eq $edge.Sha256 -and ($edge.SizeBytes -le 0 -or $cacheLen -eq $edge.SizeBytes)) {
            Copy-Item -LiteralPath $cached -Destination $target -Force
            return [pscustomobject]@{ Label = $edge.Label; Ok = $true; Bytes = $cacheLen; Error = $null; Source = "cache" }
          }
        }
      }

      $tmp = "$target.partial-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
      $error_ = "unknown failure"
      for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
          # -TimeoutSec: PS7 默认无限等待；半开/停滞连接不报错，会让整个打包无限挂起。
          # 600s 对最大的 ~161 MiB 模型留足余量，超时进 catch 走已有的 3 次退避重试。
          Invoke-WebRequest -Uri $edge.Url -OutFile $tmp -UseBasicParsing -TimeoutSec 600
          $hash = (Get-FileHash -LiteralPath $tmp -Algorithm SHA256).Hash
          $length = (Get-Item -LiteralPath $tmp).Length
          if ($hash -ne $edge.Sha256) { throw "sha256 mismatch (expected $($edge.Sha256), got $hash)" }
          if ($edge.SizeBytes -gt 0 -and $length -ne $edge.SizeBytes) {
            throw "size mismatch (expected $($edge.SizeBytes) bytes, got $length)"
          }
          Move-Item -LiteralPath $tmp -Destination $target -Force
          if ($cacheRoot) {
            $cacheDest = Join-Path $cacheRoot ($edge.RelativePath -replace '/', '\')
            $cacheDir = Split-Path -Parent $cacheDest
            New-Item -ItemType Directory -Path $cacheDir -Force | Out-Null
            Copy-Item -LiteralPath $target -Destination $cacheDest -Force
          }
          return [pscustomobject]@{ Label = $edge.Label; Ok = $true; Bytes = $length; Error = $null; Source = "download" }
        }
        catch {
          $error_ = $_.Exception.Message
          Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
          if ($attempt -lt 3) { Start-Sleep -Seconds (3 * $attempt) }
        }
      }
      Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
      [pscustomobject]@{ Label = $edge.Label; Ok = $false; Bytes = 0; Error = $error_; Source = "none" }
    } -ThrottleLimit $Throttle)
  }
}

function Complete-ModelInstall([object[]]$Plan, [object]$Job) {
  $results = @()
  if ($Job) {
    $jobErrors = @()
    $results = @(Receive-Job -Job $Job -Wait -ErrorVariable jobErrors -ErrorAction SilentlyContinue)
    $state = $Job.State
    Remove-Job -Job $Job -Force -ErrorAction SilentlyContinue
    if ($state -ne "Completed" -and $state -ne "CompletedWithWarnings") {
      throw "model install job ended in state ${state}: $($jobErrors -join '; ')"
    }
  }
  foreach ($result in $results | Where-Object { $_ -and -not $_.Ok }) {
    Write-Warning "  download failed: $($result.Label): $($result.Error)"
  }

  # Re-verify only the files this run installed. Already-verified edges were
  # checksummed once in the plan filter; hashing the full ~650MB set again is
  # pure SATA tax on every package.
  $invalid = @($Plan | Where-Object { -not (Test-ModelFile $_.Target $_.Sha256 $_.SizeBytes) })
  if ($invalid.Count -gt 0) {
    throw "model staging failed checksum/size verification for: $($invalid.Label -join ', ')"
  }
  return $results
}

function Install-Models([object[]]$Edges, [int]$Throttle, [string]$LocalCache = "") {
  # [object[]], not [string[]]: the edges are PSCustomObjects and string
  # coercion would drop Target/Sha256/Url before any download happens.
  $plan = @($Edges | Where-Object { -not (Test-ModelFile $_.Target $_.Sha256 $_.SizeBytes) })
  $skipped = $Edges.Count - $plan.Count
  if ($skipped -gt 0) { Write-Host "  $skipped model file(s) already present and verified" }

  $job = $null
  if ($plan.Count -gt 0) {
    Write-Host "  installing $($plan.Count) model file(s) with $Throttle parallel worker(s)"
    $job = Start-ModelDownloadJob -Plan $plan -Throttle $Throttle -LocalCache $LocalCache
  }
  return Complete-ModelInstall -Plan $plan -Job $job
}

function Install-CrtDlls([string]$Stage, [string]$RepoRoot) {
  . (Join-Path $RepoRoot "scripts/ci/lib/pe-imports.ps1")
  $peFiles = Get-ChildItem -Path $Stage -Recurse -File |
    Where-Object { $_.Extension -in @(".exe", ".dll") }
  $imported = @($peFiles | ForEach-Object { Get-PeImportedDllNames $_.FullName } |
      Where-Object { $_ -match $CrtDllPattern } | Sort-Object -Unique)
  if ($imported.Count -eq 0) {
    Write-Host "  no MSVC CRT imports in the staged tree; nothing to deploy"
    return
  }

  $redistDirs = @()
  if ($env:VCToolsRedistDir) {
    $x64 = Join-Path $env:VCToolsRedistDir "x64"
    if (Test-Path -LiteralPath $x64) {
      $redistDirs += @(Get-ChildItem -Path $x64 -Directory -Filter "Microsoft.VC*.CRT" -ErrorAction SilentlyContinue |
          ForEach-Object { $_.FullName })
    }
  }
  # ProgramFiles(x86) is missing in some local pwsh sandboxes; Join-Path $null
  # used to abort CRT staging before any DLL was copied. Probe the usual roots.
  $vswhereCandidates = [System.Collections.Generic.List[string]]::new()
  foreach ($root in @(${env:ProgramFiles(x86)}, $env:ProgramFiles, "C:\Program Files (x86)", "C:\Program Files")) {
    if ([string]::IsNullOrWhiteSpace($root)) { continue }
    $candidate = Join-Path $root "Microsoft Visual Studio/Installer/vswhere.exe"
    if ($vswhereCandidates -notcontains $candidate) { $vswhereCandidates.Add($candidate) }
  }
  $vswhere = $vswhereCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
  if ($vswhere) {
    $installation = (& $vswhere -latest -products "*" -property installationPath 2>$null | Select-Object -First 1)
    if ($installation) {
      $redist = Join-Path $installation.Trim() "VC/Redist/MSVC"
      if (Test-Path -LiteralPath $redist) {
        $redistDirs += @(Get-ChildItem -Path $redist -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "x64" } |
            Where-Object { Test-Path -LiteralPath $_ } |
            ForEach-Object {
              Get-ChildItem -Path $_ -Directory -Filter "Microsoft.VC*.CRT" -ErrorAction SilentlyContinue |
                ForEach-Object { $_.FullName }
            })
      }
    }
  }
  $redistDirs = @($redistDirs | Select-Object -Unique)
  if (-not $env:SystemRoot) {
    throw "SystemRoot is not set; cannot fall back to System32 for CRT DLLs"
  }
  $system32 = Join-Path $env:SystemRoot "System32"

  $missing = @()
  foreach ($name in $imported) {
    $source = $null
    foreach ($dir in $redistDirs) {
      $candidate = Join-Path $dir $name
      if (Test-Path -LiteralPath $candidate) { $source = $candidate; break }
    }
    if (-not $source) {
      $candidate = Join-Path $system32 $name
      if (Test-Path -LiteralPath $candidate) {
        $source = $candidate
        Write-Warning "  falling back to $candidate for $name; a host without that system copy will fail to start the CLI"
      }
    }
    if (-not $source) {
      $missing += $name
      continue
    }
    Copy-Item -LiteralPath $source -Destination (Join-Path $Stage $name) -Force
    Write-Host "  staged $name from $(Split-Path -Parent $source)"
  }
  if ($missing.Count -gt 0) {
    throw "no source found for imported CRT DLL(s): $($missing -join ', '). Install the MSVC redistributable files (VCToolsRedistDir or a Visual Studio installation) and re-run."
  }
}

Push-Location $RepoRoot
$cpuJob = $null
$whisperJob = $null
$modelJob = $null
$emptyCache = $null
$validateRoot = $null
try {
  if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot "Cargo.toml"))) {
    throw "repo root expected at $RepoRoot (no Cargo.toml)"
  }
  Write-Host "repo:   $RepoRoot"
  Write-Host "target: $Target"
  $jobsSource = if ($MyInvocation.BoundParameters.ContainsKey('Jobs')) { "explicit" } elseif ($env:GITHUB_ACTIONS) { "ci-default" } else { "local-auto" }
  Write-Host "jobs:   $Jobs of $([Environment]::ProcessorCount) logical cores ($jobsSource)"

  if (-not $NoCpuMonitor) {
    # Resolved before the first Write-Phase so the marker queue has a sink.
    $script:CpuCsv = if ([string]::IsNullOrWhiteSpace($MonitorCsv)) {
      Join-Path $RepoRoot ("target/package-cpu-" + (Get-Date -Format "yyyyMMdd-HHmmss") + ".csv")
    }
    else {
      $MonitorCsv
    }
    $cpuSummary = Join-Path (Split-Path -Parent $script:CpuCsv) ([System.IO.Path]::GetFileNameWithoutExtension($script:CpuCsv) + "-summary.txt")
    $cpuJob = Start-CpuMonitor -IntervalSec $MonitorIntervalSec -CsvPath $script:CpuCsv
    Write-Host "cpu:    sampling every ${MonitorIntervalSec}s into $script:CpuCsv"
  }

  if (-not $env:VCPKG_ROOT -and (Test-Path -LiteralPath "C:\vcpkg")) {
    # libheif-sys (the `heic` feature) resolves libheif/boost/zlib through vcpkg.
    $env:VCPKG_ROOT = "C:\vcpkg"
    Write-Host "VCPKG_ROOT not set; defaulting to $env:VCPKG_ROOT"
  }

  # Local reuse: a previous run stages ORT under target/ort-staging and leaves
  # it there. Point ORT_LIB_LOCATION at it so a bare local invocation does not
  # re-download the archive every time. CI exports ORT_LIB_LOCATION itself.
  if ([string]::IsNullOrWhiteSpace($env:ORT_LIB_LOCATION)) {
    $ortStaging = Join-Path $RepoRoot "target/ort-staging"
    if (Test-Path -LiteralPath (Join-Path $ortStaging "onnxruntime.dll")) {
      $env:ORT_LIB_LOCATION = $ortStaging
      Write-Host "ORT_LIB_LOCATION not set; reusing $ortStaging"
    }
  }

  if ([string]::IsNullOrWhiteSpace($env:ORT_LIB_LOCATION)) {
    Write-Phase "ORT"
    Write-Host "  provisioning ONNX Runtime $OrtVersion (local run; CI exports ORT_LIB_LOCATION itself)"
    $ortScript = Join-Path $RepoRoot "scripts/ci/actions/setup-onnx-runtime/windows.ps1"
    $envFile = [System.IO.Path]::GetTempFileName()
    $savedWorkspace = $env:GITHUB_WORKSPACE
    $savedGithubEnv = $env:GITHUB_ENV
    $savedRunnerArch = $env:RUNNER_ARCH
    try {
      $env:GITHUB_WORKSPACE = $RepoRoot
      $env:GITHUB_ENV = $envFile
      # windows.ps1 resolves its empty arch argument through $env:RUNNER_ARCH --
      # a GitHub-runner variable, so on a local host it hits a null
      # ToLowerInvariant() before falling back to x64. Derive it from the cargo
      # target so the helper is never left guessing about the ORT archive.
      if ([string]::IsNullOrWhiteSpace($env:RUNNER_ARCH)) {
        $env:RUNNER_ARCH = if ($Target -like "aarch64-*") { "arm64" } else { "x64" }
      }
      # Stage into target/ (gitignored) instead of the CI action's default
      # crates/xberg-node so a local run does not dirty the working tree.
      & pwsh -NoProfile -File $ortScript $OrtVersion "target/ort-staging" "" "system"
      if ($LASTEXITCODE -ne 0) { throw "ONNX Runtime setup failed with exit code $LASTEXITCODE" }
      foreach ($line in (Get-Content -LiteralPath $envFile)) {
        if ($line -match '^([^=]+)=(.*)$') {
          [System.Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], "Process")
          Write-Host "  export $($Matches[1])"
        }
      }
    }
    finally {
      [System.Environment]::SetEnvironmentVariable("GITHUB_WORKSPACE", $savedWorkspace, "Process")
      [System.Environment]::SetEnvironmentVariable("GITHUB_ENV", $savedGithubEnv, "Process")
      [System.Environment]::SetEnvironmentVariable("RUNNER_ARCH", $savedRunnerArch, "Process")
      Remove-Item -LiteralPath $envFile -Force -ErrorAction SilentlyContinue
    }
    if ([string]::IsNullOrWhiteSpace($env:ORT_LIB_LOCATION)) {
      throw "ONNX Runtime setup did not export ORT_LIB_LOCATION"
    }
  }
  else {
    Write-Phase "ORT"
    Write-Host "  using the ONNX Runtime staged by the environment (ORT_LIB_LOCATION=$env:ORT_LIB_LOCATION)"
  }

  # Whisper is fully pinned here (URL + SHA + size) and does not need the built
  # exe. Prefetch it into the persistent model cache so a cold
  # run does not wait until after CRT to pull ~265MB.
  New-Item -ItemType Directory -Path $ModelCacheRoot -Force | Out-Null
  $whisperCacheEdges = @(Get-TranscriptionModelEntries -ModelsRoot $ModelCacheRoot |
    Where-Object { -not (Test-ModelFile $_.Target $_.Sha256 $_.SizeBytes) })
  if ($whisperCacheEdges.Count -gt 0) {
    Write-Host "whisper: prefetching $($whisperCacheEdges.Count) file(s) into $ModelCacheRoot (races cargo build)"
    $whisperJob = Start-ModelDownloadJob -Plan $whisperCacheEdges -Throttle ([Math]::Min(4, $Jobs)) -LocalCache ""
  }

  Write-Phase "Build"
  Write-Host "  xberg-cli, $($Features.Count) features, $Jobs jobs"
  # A cargo from another session (or a stale run) holds target/'s artifact lock
  # for the whole of its build, and this one then sits at "Blocking waiting for
  # file lock on artifact directory" at 0% CPU -- 45s of that was measured. Say
  # so up front instead of stalling silently; the CPU report shows the same
  # signature as cargo_n=2 with a small rustc_n.
  $busyBuilders = @(Get-Process -Name cargo, rustc -ErrorAction SilentlyContinue)
  if ($busyBuilders.Count -gt 0) {
    Write-Warning "  cargo/rustc already running (pid $($busyBuilders.Id -join ', ')); this build will block on the target/ artifact lock until they exit"
  }
  $cargoArgs = @(
    "build", "--locked", "--release", "--target", $Target, "--package", "xberg-cli",
    "--no-default-features", "--features", ($Features -join ","), "--jobs", "$Jobs"
  )
  Write-Host "  cargo $($cargoArgs -join ' ')"
  Invoke-Native -FilePath "cargo" -Arguments $cargoArgs -WorkingDirectory $RepoRoot

  $builtExe = Join-Path $RepoRoot "target/$Target/release/xberg.exe"
  if (-not (Test-Path -LiteralPath $builtExe)) {
    throw "expected binary at $builtExe after the build"
  }

  if ($whisperJob) {
    Complete-ModelInstall -Plan @() -Job $whisperJob | Out-Null
    $whisperJob = $null
    Write-Host "  whisper prefetch into $ModelCacheRoot done"
  }

  # Manifest only needs the freshly built target exe (normal PATH / MSVC CRT),
  # not the staged tree. Resolve OCR/layout edges here so downloads can overlap
  # Stage + CRT instead of waiting for both.
  $selected = @(Get-RequiredModelEntries -Exe $builtExe -ModelsRoot $ModelsRoot)
  $selected += @(Get-TranscriptionModelEntries -ModelsRoot $ModelsRoot)

  Write-Phase "Stage"
  Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Path $Stage -Force | Out-Null
  New-Item -ItemType Directory -Path $ModelsRoot -Force | Out-Null
  # The binary first: the CRT scan and smoke tests need it in place.
  Copy-Item -LiteralPath $builtExe -Destination $StageExe -Force
  Write-Host "  staged xberg.exe"

  # Start model installs into the freshly created Stage/models while the rest
  # of Stage and CRT run. Cache hits (Whisper + previous OCR/layout) copy
  # locally; misses download.
  $modelPlan = @($selected | Where-Object { -not (Test-ModelFile $_.Target $_.Sha256 $_.SizeBytes) })
  $modelSkipped = $selected.Count - $modelPlan.Count
  if ($modelSkipped -gt 0) { Write-Host "  $modelSkipped model file(s) already present and verified" }
  if ($modelPlan.Count -gt 0) {
    Write-Host "  installing $($modelPlan.Count) model file(s) with $Jobs parallel worker(s) (races Stage/CRT)"
    $modelJob = Start-ModelDownloadJob -Plan $modelPlan -Throttle $Jobs -LocalCache $ModelCacheRoot
  }

  # Independent writes into the stage directory, in flight at once and
  # bounded by -Jobs. Each reports its own outcome instead of throwing, so one
  # failure cannot hide another; the parent summarizes and throws once.
  $ortLib = $env:ORT_LIB_LOCATION
  $stageTasks = @("ort-dlls", "licenses", "launcher")
  $stageResults = @($stageTasks) | ForEach-Object -Parallel {
    $ProgressPreference = "SilentlyContinue"
    # A parallel block runs in a fresh runspace: the caller's
    # $ErrorActionPreference = "Stop" does not carry over, and without it a
    # failed Copy-Item (missing LICENSE, locked target) is a non-terminating
    # error that never reaches the catch below and the task reports Ok.
    $ErrorActionPreference = "Stop"
    $task = $_
    try {
      $detail = switch ($task) {
        "ort-dlls" {
          $dlls = @(Get-ChildItem -Path $using:ortLib -File -Filter "*.dll" -ErrorAction SilentlyContinue)
          if ($dlls.Count -eq 0) { throw "no DLLs found under ORT_LIB_LOCATION=$using:ortLib" }
          foreach ($dll in $dlls) {
            Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $using:Stage $dll.Name) -Force
          }
          if (-not (Test-Path -LiteralPath (Join-Path $using:Stage "onnxruntime.dll"))) {
            throw "onnxruntime.dll is not in ORT_LIB_LOCATION=$using:ortLib"
          }
          "$($dlls.Count) ONNX Runtime DLL(s)"
        }
        "licenses" {
          foreach ($file in @("LICENSE", "THIRD_PARTY_LICENSES.md")) {
            Copy-Item -LiteralPath (Join-Path $using:RepoRoot $file) -Destination (Join-Path $using:Stage $file) -Force
          }
          "LICENSE, THIRD_PARTY_LICENSES.md"
        }
        "launcher" {
          $launcher = @(
            "@echo off"
            "setlocal"
            'set "XBERG_ROOT=%~dp0"'
            'set "HF_HUB_CACHE=%XBERG_ROOT%models"'
            '"%XBERG_ROOT%xberg.exe" %*'
            "exit /b %ERRORLEVEL%"
          ) -join "`r`n"
          [System.IO.File]::WriteAllText((Join-Path $using:Stage "xberg.cmd"), $launcher + "`r`n", [System.Text.UTF8Encoding]::new($false))
          "xberg.cmd"
        }
      }
      [pscustomobject]@{ Name = $task; Ok = $true; Detail = $detail; Error = $null }
    }
    catch {
      [pscustomobject]@{ Name = $task; Ok = $false; Detail = $null; Error = $_.Exception.Message }
    }
  } -ThrottleLimit $Jobs

  # Reported in the declared order, not in completion order, so two runs of the
  # same failure read the same way.
  foreach ($name in $stageTasks) {
    $result = @($stageResults | Where-Object { $_.Name -eq $name })[0]
    if (-not $result) { continue }
    if ($result.Ok) {
      Write-Host "  staged $($result.Detail)"
    }
    else {
      Write-Host "  failed $($result.Name): $($result.Error)" -ForegroundColor Red
    }
  }
  $stageFailed = @($stageResults | Where-Object { -not $_.Ok })
  if ($stageFailed.Count -gt 0) {
    $summary = ($stageFailed | ForEach-Object { "$($_.Name): $($_.Error)" }) -join "; "
    throw "staging the bundle failed: $summary"
  }

  # Deploy the MSVC runtime before the first run of the staged binary (smoke /
  # --version). `cache manifest` already ran off the target exe above, so CRT
  # no longer blocks model resolution -- only the in-flight download job does.
  Write-Phase "CRT"
  Install-CrtDlls -Stage $Stage -RepoRoot $RepoRoot

  Write-Phase "Models"
  Complete-ModelInstall -Plan $modelPlan -Job $modelJob | Out-Null
  $modelJob = $null
  $modelTotal = 0
  foreach ($edge in $selected) {
    $length = (Get-Item -LiteralPath $edge.Target).Length
    $modelTotal += $length
    Write-Host ("  {0,-14} {1,10:N1} MB  {2}" -f $edge.Label, ($length / 1MB), $edge.RelativePath)
  }

  Write-Phase "Validate"
  $fixture = Join-Path $RepoRoot "fixtures/images/test_hello_world.png"
  if (-not (Test-Path -LiteralPath $fixture)) { throw "smoke fixture missing: $fixture" }
  $cleanPath = Get-CleanPath -DirectoryToDrop $RepoRoot
  $emptyCache = Join-Path ([System.IO.Path]::GetTempPath()) ("xberg-empty-cache-" + [Guid]::NewGuid().ToString("N"))
  New-Item -ItemType Directory -Path $emptyCache -Force | Out-Null
  $validateRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("xberg-validate-" + [Guid]::NewGuid().ToString("N"))
  New-Item -ItemType Directory -Path $validateRoot -Force | Out-Null

  $verifyScript = Join-Path $RepoRoot "scripts/ci/verify-windows-dll-closure.ps1"
  $smokeScript = Join-Path $RepoRoot "scripts/publish/cli/offline-smoke.ps1"
  $verifyPwsh = Join-Path $PSHOME "pwsh.exe"
  # Four read-only probes against the staged tree, all in flight at once.
  # dll-closure + -RequireImportClosure is one invocation: the verify script
  # always runs ABSENCE/PRESENCE and only adds the import walk when asked, so
  # the previous un-flagged duplicate probe was pure PE re-walk cost.
  $checks = @(
    @{
      Name = "version"
      Exe = $verifyPwsh
      # `if (-not $? ...)` is load-bearing: when CreateProcess itself fails (bad
      # architecture, truncated PE — exactly the rot this gate exists to catch),
      # `&` writes a non-terminating error and `$LASTEXITCODE` is never assigned,
      # so a bare `exit $LASTEXITCODE` exits 0 and the gate waved corrupt binaries
      # through to compress/upload/publish.
      Args = @("-NoProfile", "-Command", '$env:PATH = $env:XBERG_PROBE_PATH; & $env:XBERG_PROBE_EXE --version; if (-not $? -or $null -eq $LASTEXITCODE) { exit 1 }; exit $LASTEXITCODE')
      Env = @{ XBERG_PROBE_PATH = $cleanPath; XBERG_PROBE_EXE = $StageExe }
    }
    @{
      Name = "dll-import-closure"
      Exe = $verifyPwsh
      Args = @("-NoProfile", "-File", $verifyScript, "-Artifact", $Stage, "-NativeGlob", "xberg.exe", "-RequireImportClosure")
      Env = @{}
    }
    @{
      Name = "smoke-positive"
      Exe = $verifyPwsh
      Args = @("-NoProfile", "-File", $smokeScript, "-Exe", $StageExe, "-Fixture", $fixture, "-CacheDir", $ModelsRoot, "-CleanPath", $cleanPath)
      Env = @{}
    }
  )
  # Empty-cache probe: with HF models bundled, offline-smoke.ps1 runs the
  # fixture with the layout path enabled (`{"layout":{},"ocr":{}}`), so the
  # extract resolves RT-DETR/TATR through the cache under test. Against an empty
  # cache the run must report the offline model-cache miss instead of silently
  # downloading. The exit code is deliberately not asserted: the layout path
  # falls back to whole-image OCR, whose own model needs depend on the compiled
  # OCR backend, so the diagnostic is what proves the cache was consulted.
  if ($RequiredModels.Count -gt 0) {
    $checks += @(
      @{
        Name = "smoke-empty-cache"
        Exe = $verifyPwsh
        Args = @("-NoProfile", "-File", $smokeScript, "-Exe", $StageExe, "-Fixture", $fixture, "-CacheDir", $emptyCache, "-CleanPath", $cleanPath, "-ExpectEmptyCacheFailure")
        Env = @{}
      }
    )
  }

  $running = foreach ($check in $checks) {
    $stdoutFile = Join-Path $validateRoot ($check.Name + ".stdout.txt")
    $stderrFile = Join-Path $validateRoot ($check.Name + ".stderr.txt")
    # -ArgumentList is joined with spaces and nothing is quoted for us, so an
    # argument holding a space has to carry its own quotes.
    $quotedArgs = foreach ($argument in $check.Args) {
      if ($argument -match '\s') { '"' + $argument + '"' } else { $argument }
    }
    $spawn = @{
      FilePath               = $check.Exe
      ArgumentList           = $quotedArgs
      PassThru               = $true
      NoNewWindow            = $true
      WorkingDirectory       = $RepoRoot
      RedirectStandardOutput = $stdoutFile
      RedirectStandardError  = $stderrFile
    }
    if ($check.Env.Count -gt 0) { $spawn.Environment = $check.Env }
    $process = Start-Process @spawn
    [pscustomobject]@{ Name = $check.Name; Process = $process; StdoutFile = $stdoutFile; StderrFile = $stderrFile; Hung = $false }
  }
  # A probe that hangs (a model session deadlocked on load, a missing DLL stuck mid-load)
  # must fail the gate instead of blocking the packaging run until a human kills it. The
  # window is generous: the smoke probes load RT-DETR/TATR/PaddleOCR from the staged cache.
  # All probes share one deadline: per-probe full windows handed out serially would let
  # k simultaneously hung probes stall the gate for k times the timeout and eat the
  # workflow's own budget before any diagnostics get written.
  $probeTimeoutMs = 15 * 60 * 1000
  $probeDeadline = [DateTime]::UtcNow.AddMilliseconds($probeTimeoutMs)
  foreach ($item in $running) {
    $remainingMs = [int]([Math]::Max(0, ($probeDeadline - [DateTime]::UtcNow).TotalMilliseconds))
    if (-not $item.Process.WaitForExit($remainingMs)) {
      & "$env:SystemRoot\System32\taskkill.exe" /PID $item.Process.Id /T /F | Out-Null
      # Bounded: if the kill itself failed, giving up on the wait beats hanging the gate.
      $item.Process.WaitForExit(10000) | Out-Null
      $item.Hung = $true
    }
  }

  $failedChecks = [System.Collections.Generic.List[string]]::new()
  foreach ($item in $running) {
    $stdout = Get-Content -LiteralPath $item.StdoutFile -Raw -ErrorAction SilentlyContinue
    $stderr = Get-Content -LiteralPath $item.StderrFile -Raw -ErrorAction SilentlyContinue
    if ($item.Hung) {
      $failedChecks.Add("$($item.Name) did not finish within $($probeTimeoutMs / 60000) minutes and was killed`n--- stdout ---`n$stdout`n--- stderr ---`n$stderr")
      continue
    }
    if ($item.Process.ExitCode -ne 0) {
      $failedChecks.Add("$($item.Name) exited $($item.Process.ExitCode)`n--- stdout ---`n$stdout`n--- stderr ---`n$stderr")
      continue
    }
    # Exit code 0 alone proves less than it seems: the child is a pwsh wrapper
    # around the staged exe, so assert the `--version` payload itself — the
    # first non-empty line must be `xberg <semver>`. A wrapper bug or a hijacked
    # exe that prints anything else must not pass this gate.
    if ($item.Name -eq "version") {
      $versionLine = @("$stdout" -split "`r?`n" | Where-Object { $_ -ne "" } | Select-Object -First 1)
      if (-not $versionLine -or $versionLine -notmatch '^xberg[ -]?\d+(\.\d+)+') {
        $failedChecks.Add("version probe printed an unexpected banner (expected 'xberg <semver>'):`n$stdout`n--- stderr ---`n$stderr")
        continue
      }
    }
    Write-Host "  $($item.Name): exit 0"
    foreach ($line in @("$stdout" -split "`r?`n" | Where-Object { $_ -ne "" })) {
      Write-Host "    $line"
    }
  }
  if ($failedChecks.Count -gt 0) {
    throw ("the staged tree failed validation:`n" + ($failedChecks -join "`n"))
  }

  Write-Phase "Compress"
  Remove-Item -LiteralPath $ZipPath -Force -ErrorAction SilentlyContinue
  $sevenZip = (Get-Command 7z -ErrorAction Stop).Source
  Invoke-Native -FilePath $sevenZip -Arguments @("a", "-mmt=$Jobs", $ZipPath, $StageName) -WorkingDirectory $RepoRoot | Out-Null
  if (-not $SkipZipTest) {
    Invoke-Native -FilePath $sevenZip -Arguments @("t", $ZipPath) -WorkingDirectory $RepoRoot | Out-Null
  }
  else {
    Write-Host "  skipped 7z t (-SkipZipTest)"
  }

  Write-Phase "Done"
  Write-Host "staging dir: $Stage"
  Write-Host ("zip:         {0} ({1:N1} MB)" -f $ZipPath, ((Get-Item -LiteralPath $ZipPath).Length / 1MB))
  Write-Host ("models:      {0} files, {1:N1} MB" -f $selected.Count, ($modelTotal / 1MB))

  if ($cpuJob) {
    Stop-CpuMonitor -Job $cpuJob
    $cpuJob = $null
    Get-CpuReport -CsvPath $script:CpuCsv -SummaryPath $cpuSummary -IntervalSec $MonitorIntervalSec `
      -QuietPct $QuietCpuPct -QuietSeconds $QuietSeconds
  }
  Write-Host "usage:       .\xberg.cmd extract <document> --content-format markdown --format text"
}
finally {
  if ($cpuJob) { Stop-CpuMonitor -Job $cpuJob }
  foreach ($job in @($whisperJob, $modelJob)) {
    if ($job) {
      Stop-Job -Job $job -ErrorAction SilentlyContinue
      Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
    }
  }
  if ($emptyCache) { Remove-Item -Recurse -Force $emptyCache -ErrorAction SilentlyContinue }
  if ($validateRoot) { Remove-Item -Recurse -Force $validateRoot -ErrorAction SilentlyContinue }
  Pop-Location
}
