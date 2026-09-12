#!/usr/bin/env pwsh
<#
.SYNOPSIS
Builds the Windows CLI release bundle: xberg.exe + sibling DLLs + the default
model set + a launcher, zipped as xberg-cli-<target>.zip.

.DESCRIPTION
Single source of truth for "build + package" on Windows. Used locally and by
.github/workflows/build-windows-cli.yml; the workflow continues afterwards with
artifact upload, version stamping and `gh release create`.

The cargo feature set is the crate's `all` aggregate minus `embeddings`,
`ner-onnx` and `ner-llm`: full document conversion (formats, OCR, layout,
chunking, tree-sitter, URL ingestion, API/MCP servers, transcription) without
the embedding/NER model stacks. `--no-default-features` is required because
`default` carries `embeddings`. Only the default tier of each enabled model
capability ships (PaddleOCR pp-ocrv6 small + both PP-LCNet orientation
classifiers, layout RT-DETR and table TATR, plus the Whisper tiny transcription
model so video/audio inputs transcribe without network); GLiNER, embedding,
candle-VLM and SLANeXT models are deliberately not bundled, nor is tessdata.

Before zipping, the staged tree must pass: a cleaned-PATH `--version` probe,
in-tree MSVC CRT deployment, the shared PE import-closure gate
(scripts/ci/verify-windows-dll-closure.ps1 -RequireImportClosure), and an
offline smoke test that proves the bundled models are what the binary loads --
the same command against an empty cache must fail instead of silently
downloading.

Every stage that can overlap does, and all of it is throttled by -Jobs: the
pdfium download races the cargo build, the four stage-directory writes and the
five validation probes run concurrently, and the model downloads were already
parallel. While any of that runs the script samples system CPU and, at the end,
writes the sampling log and a "quiet window" report to
target/package-cpu-<timestamp>.csv and -summary.txt, so a stage that leaves the
machine idle can be located from its timestamps alone. The report is diagnostic
only -- it never changes the exit code (-NoCpuMonitor turns sampling off).

.EXAMPLE
pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1

Bare invocation, the way CI runs it: the 6-job default, sized for the runner.

.EXAMPLE
pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1 -Target x86_64-pc-windows-msvc -Jobs 32

Local package: pass the machine's core count. The pipeline's 6-job default is
sized for the runner and leaves a workstation idle.
#>
[CmdletBinding()]
param(
  [string]$Target = "x86_64-pc-windows-msvc",
  # 6 is the pipeline value: .github/workflows/build-windows-cli.yml passes no
  # -Jobs, and the runner has few cores. A developer machine is not the runner:
  # pass the local core count explicitly (-Jobs 32 here), because 6 leaves a
  # 32-thread box at ~19% load and multiplies the build time.
  [int]$Jobs = 6,
  [string]$OrtVersion = "1.24.2",
  # pdfium-binaries `chromium/<n>` tag. Pinned because it must match the binding
  # set of the xberg-pdfium-render crate this build links. Bumping it is its own
  # task (it needs the pdfium binding compatibility re-checked), not a drive-by.
  [string]$PdfiumVersion = "7881",
  # CPU sampling: one sample every -MonitorIntervalSec, a run of samples below
  # -QuietCpuPct lasting -QuietSeconds or longer is reported as a quiet window.
  # Anything at or under 20% of the machine is treated as a stalled/serial stage.
  # -MonitorCsv defaults to target/package-cpu-<timestamp>.csv (<summary.txt>).
  [int]$MonitorIntervalSec = 5,
  [int]$QuietCpuPct = 20,
  [int]$QuietSeconds = 20,
  [string]$MonitorCsv = "",
  [switch]$NoCpuMonitor
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "../../..")).Path
$StageName = "xberg-cli-$Target"
$Stage = Join-Path $RepoRoot $StageName
$ZipPath = Join-Path $RepoRoot "$StageName.zip"
$StageExe = Join-Path $Stage "xberg.exe"
$ModelsRoot = Join-Path $Stage "models"

# `all` (crates/xberg-cli/Cargo.toml) minus embeddings, ner-onnx, ner-llm. Keep
# in sync with that aggregate; check-feature-parity.py does not cover this list.
$Features = @(
  "formats-no-heic"
  "analysis"
  "core-cli"
  "html"
  "url-ingestion"
  "liter-llm"
  "ocr"
  "paddle-ocr"
  "sceptre-ocr"
  "candle-vlm-ocr"
  "layout-detection"
  "chunking-tokenizers"
  "tree-sitter"
  "api"
  "heic"
  "mcp"
  "mcp-http"
  "classification"
  "captioning"
  "summarization"
  "summarization-llm"
  "transcription"
  "pdf-pdfium-surface"
)

# Manifest entries that must exist for the bundle to be usable offline, matched
# by `relative_path` from `xberg cache manifest --format json`. Exact paths for
# PaddleOCR (repository-relative), revision-agnostic for the layout repo (its
# snapshot directory embeds the commit). A miss here means upstream renamed or
# dropped an artifact: fail loudly instead of shipping a bundle that silently
# downloads at runtime.
$RequiredModels = @(
  @{ Regex = '^v6/det/small/model\.onnx$'; Label = "PaddleOCR pp-ocrv6 det small" }
  @{ Regex = '^v6/rec/small/model\.onnx$'; Label = "PaddleOCR pp-ocrv6 rec small" }
  @{ Regex = '^v6/rec/small/dict\.txt$'; Label = "PaddleOCR pp-ocrv6 rec small dictionary" }
  @{ Regex = '^v2/classifiers/PP-LCNet_x1_0_textline_ori\.onnx$'; Label = "text line orientation classifier" }
  @{ Regex = '^v2/classifiers/PP-LCNet_x1_0_doc_ori\.onnx$'; Label = "document orientation classifier" }
  @{ Regex = '^models--xberg-io--layout-models/snapshots/[0-9a-f]{40}/rtdetr/model\.onnx$'; Label = "layout RT-DETR" }
  @{ Regex = '^models--xberg-io--layout-models/snapshots/[0-9a-f]{40}/tatr/model\.onnx$'; Label = "table TATR" }
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
  $manifest = Invoke-Captured -FilePath $Exe -Arguments @("cache", "manifest", "--format", "json")
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

function Install-Models([object[]]$Edges, [int]$Throttle) {
  # [object[]], not [string[]]: the edges are PSCustomObjects and string
  # coercion would drop Target/Sha256/Url before any download happens.
  $plan = @($Edges | Where-Object { -not (Test-ModelFile $_.Target $_.Sha256 $_.SizeBytes) })
  $skipped = $Edges.Count - $plan.Count
  if ($skipped -gt 0) { Write-Host "  $skipped model file(s) already present and verified" }

  if ($plan.Count -gt 0) {
    Write-Host "  downloading $($plan.Count) model file(s) with $Throttle parallel worker(s)"
    $results = $plan | ForEach-Object -Parallel {
      $ProgressPreference = "SilentlyContinue"
      $edge = $_
      $target = $edge.Target
      $dir = Split-Path -Parent $target
      New-Item -ItemType Directory -Path $dir -Force | Out-Null
      $tmp = "$target.partial-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
      $error_ = "unknown failure"
      for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
          Invoke-WebRequest -Uri $edge.Url -OutFile $tmp -UseBasicParsing
          $hash = (Get-FileHash -LiteralPath $tmp -Algorithm SHA256).Hash
          $length = (Get-Item -LiteralPath $tmp).Length
          if ($hash -ne $edge.Sha256) { throw "sha256 mismatch (expected $($edge.Sha256), got $hash)" }
          if ($edge.SizeBytes -gt 0 -and $length -ne $edge.SizeBytes) {
            throw "size mismatch (expected $($edge.SizeBytes) bytes, got $length)"
          }
          Move-Item -LiteralPath $tmp -Destination $target -Force
          return [pscustomobject]@{ Label = $edge.Label; Ok = $true; Bytes = $length; Error = $null }
        }
        catch {
          $error_ = $_.Exception.Message
          Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
          if ($attempt -lt 3) { Start-Sleep -Seconds (3 * $attempt) }
        }
      }
      Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
      [pscustomobject]@{ Label = $edge.Label; Ok = $false; Bytes = 0; Error = $error_ }
    } -ThrottleLimit $Throttle

    foreach ($result in $results | Where-Object { -not $_.Ok }) {
      Write-Warning "  download failed: $($result.Label): $($result.Error)"
    }
  }

  $invalid = @($Edges | Where-Object { -not (Test-ModelFile $_.Target $_.Sha256 $_.SizeBytes) })
  if ($invalid.Count -gt 0) {
    throw "model staging failed checksum/size verification for: $($invalid.Label -join ', ')"
  }
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
  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio/Installer/vswhere.exe"
  if (Test-Path -LiteralPath $vswhere) {
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
$pdfiumJob = $null
$pdfiumWork = $null
$emptyCache = $null
$validateRoot = $null
try {
  if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot "Cargo.toml"))) {
    throw "repo root expected at $RepoRoot (no Cargo.toml)"
  }
  Write-Host "repo:   $RepoRoot"
  Write-Host "target: $Target"
  Write-Host "jobs:   $Jobs of $([Environment]::ProcessorCount) logical cores"

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

  # pdfium is downloaded rather than built and staging only needs the file after
  # the build, so its download races the build instead of extending the critical
  # path. The temp tree path is computed here and passed in so the finally block
  # can delete it no matter what happens to the job.
  $pdfiumWork = Join-Path ([System.IO.Path]::GetTempPath()) ("xberg-pdfium-" + [Guid]::NewGuid().ToString("N"))
  $pdfiumJob = Start-ThreadJob -ArgumentList $PdfiumVersion, $pdfiumWork -ScriptBlock {
    param([string]$Version, [string]$Work)
    # The download/extract/locate flow is duplicated here on purpose: a thread
    # job cannot dot-source this script (the body would run again) and a
    # function cannot be handed across runspaces.
    $ProgressPreference = "SilentlyContinue"
    $url = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/$Version/pdfium-win-x64.tgz"
    New-Item -ItemType Directory -Path $Work -Force | Out-Null
    $archive = Join-Path $Work "pdfium-win-x64.tgz"
    $lastError = $null
    for ($attempt = 1; $attempt -le 5; $attempt++) {
      try {
        Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
        $lastError = $null
        break
      }
      catch {
        $lastError = $_.Exception.Message
        if ($attempt -lt 5) { Start-Sleep -Seconds (5 * $attempt) }
      }
    }
    if ($lastError) { throw "could not download pdfium $Version from $url : $lastError" }

    & tar -xzf $archive -C $Work
    if ($LASTEXITCODE -ne 0) { throw "tar failed to extract $archive" }

    $pdfium = Get-ChildItem -Path $Work -Recurse -File -Filter "pdfium.dll" | Select-Object -First 1
    if (-not $pdfium) { throw "pdfium.dll not found in the extracted pdfium $Version archive" }
    return $pdfium.FullName
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

  # Collected with its own message: a pdfium download failure is a network
  # failure and would otherwise read as part of the build.
  $pdfiumErrors = @()
  $pdfiumPath = Receive-Job -Job $pdfiumJob -Wait -ErrorVariable pdfiumErrors -ErrorAction SilentlyContinue
  $pdfiumState = $pdfiumJob.State
  Remove-Job -Job $pdfiumJob -Force -ErrorAction SilentlyContinue
  $pdfiumJob = $null
  if ($pdfiumState -ne "Completed" -or [string]::IsNullOrWhiteSpace($pdfiumPath)) {
    throw "pdfium $PdfiumVersion download failed (job state $pdfiumState): $($pdfiumErrors -join '; ')"
  }
  $pdfiumPath = @($pdfiumPath)[-1]
  Write-Host "  pdfium.dll $PdfiumVersion ready"

  Write-Phase "Stage"
  Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Path $Stage -Force | Out-Null
  # The binary first: the model manifest and the CRT scan both need it in place.
  Copy-Item -LiteralPath $builtExe -Destination $StageExe -Force
  Write-Host "  staged xberg.exe"

  # Four independent writes into the stage directory, in flight at once and
  # bounded by -Jobs. Each reports its own outcome instead of throwing, so one
  # failure cannot hide another; the parent summarizes and throws once.
  $ortLib = $env:ORT_LIB_LOCATION
  $stageResults = @("ort-dlls", "pdfium", "licenses", "launcher") | ForEach-Object -Parallel {
    $ProgressPreference = "SilentlyContinue"
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
        "pdfium" {
          Copy-Item -LiteralPath $using:pdfiumPath -Destination (Join-Path $using:Stage "pdfium.dll") -Force
          "pdfium.dll $using:PdfiumVersion"
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
            'set "PDFIUM_DYNAMIC_LIB_PATH=%XBERG_ROOT%"'
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
  foreach ($name in @("ort-dlls", "pdfium", "licenses", "launcher")) {
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

  # Deploy the MSVC runtime before the first run of the staged binary: the
  # freshly built xberg.exe imports vcruntime140.dll, and a host without the
  # redistributable somewhere on its search path would fail to launch it during
  # model staging, before validation ever runs. That dependency is the reason
  # this stage cannot overlap the model stage, which opens by running the staged
  # binary for `cache manifest`.
  Write-Phase "CRT"
  Install-CrtDlls -Stage $Stage -RepoRoot $RepoRoot

  Write-Phase "Models"
  $selected = @(Get-RequiredModelEntries -Exe $StageExe -ModelsRoot $ModelsRoot)
  # The transcription model rides the same parallel download + verification pass
  # as the OCR/layout set; it is not in the CLI manifest, so it is pinned in
  # $TranscriptionFiles.
  $selected += @(Get-TranscriptionModelEntries -ModelsRoot $ModelsRoot)
  Install-Models -Edges $selected -Throttle $Jobs
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
  # Five read-only probes against the staged tree, all in flight at once. Every
  # probe that runs the binary installs the cleaned PATH itself rather than
  # inheriting it: Start-Process -Environment replaces ordinary variables, but
  # for PATH it always puts $PSHOME first and appends the Machine/User scope
  # afterwards, which is not the PATH Get-CleanPath computed. -CleanPath does
  # that for the smoke script, XBERG_PROBE_PATH for the --version wrapper.
  $checks = @(
    @{
      Name = "version"
      Exe = $verifyPwsh
      Args = @("-NoProfile", "-Command", '$env:PATH = $env:XBERG_PROBE_PATH; & $env:XBERG_PROBE_EXE --version; exit $LASTEXITCODE')
      Env = @{ XBERG_PROBE_PATH = $cleanPath; XBERG_PROBE_EXE = $StageExe }
    }
    @{
      Name = "dll-closure"
      Exe = $verifyPwsh
      Args = @("-NoProfile", "-File", $verifyScript, "-Artifact", $Stage, "-NativeGlob", "xberg.exe")
      Env = @{}
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
    @{
      Name = "smoke-empty-cache"
      Exe = $verifyPwsh
      Args = @("-NoProfile", "-File", $smokeScript, "-Exe", $StageExe, "-Fixture", $fixture, "-CacheDir", $emptyCache, "-CleanPath", $cleanPath, "-ExpectEmptyCacheFailure")
      Env = @{}
    }
  )

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
    [pscustomobject]@{ Name = $check.Name; Process = $process; StdoutFile = $stdoutFile; StderrFile = $stderrFile }
  }
  foreach ($item in $running) { $item.Process.WaitForExit() }

  $failedChecks = [System.Collections.Generic.List[string]]::new()
  foreach ($item in $running) {
    $stdout = Get-Content -LiteralPath $item.StdoutFile -Raw -ErrorAction SilentlyContinue
    $stderr = Get-Content -LiteralPath $item.StderrFile -Raw -ErrorAction SilentlyContinue
    if ($item.Process.ExitCode -ne 0) {
      $failedChecks.Add("$($item.Name) exited $($item.Process.ExitCode)`n--- stdout ---`n$stdout`n--- stderr ---`n$stderr")
      continue
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
  Invoke-Native -FilePath $sevenZip -Arguments @("t", $ZipPath) -WorkingDirectory $RepoRoot | Out-Null

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
  if ($pdfiumJob) {
    Stop-Job -Job $pdfiumJob -ErrorAction SilentlyContinue
    Remove-Job -Job $pdfiumJob -Force -ErrorAction SilentlyContinue
  }
  if ($pdfiumWork) { Remove-Item -Recurse -Force $pdfiumWork -ErrorAction SilentlyContinue }
  if ($emptyCache) { Remove-Item -Recurse -Force $emptyCache -ErrorAction SilentlyContinue }
  if ($validateRoot) { Remove-Item -Recurse -Force $validateRoot -ErrorAction SilentlyContinue }
  Pop-Location
}
