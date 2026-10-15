#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Splits an already validated Windows CLI staging tree into independently downloadable components.
.DESCRIPTION
No compilation, provisioning or runtime probes. The full offline package remains the responsibility
of package-cli-windows.ps1. Each of the three feature ZIPs includes the identical common runtime,
preserves the xberg-cli-<target>/ archive root and can be used independently. A separate audio-assets
ZIP contains only FFmpeg and speech assets, without samples or the common runtime; it is not standalone.
The manifest records hashes read back from the actual ZIPs; shared paths must have the same bytes.
.EXAMPLE
pwsh -NoProfile -File scripts/publish/cli/package-cli-components.ps1 -Stage ./xberg-cli-x86_64-pc-windows-msvc -OutputDirectory .
.EXAMPLE
python scripts/tests/test_cli_components.py

Build-free smoke tests: create small synthetic staging trees under .tmp/, invoke this script,
and verify standalone package ownership, licenses, byte hashes and lossless recombination.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Stage,
  [Parameter(Mandatory)][string]$OutputDirectory,
  [ValidateSet('x86_64-pc-windows-msvc')][string]$Target = 'x86_64-pc-windows-msvc'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$stageRoot = (Get-Item -LiteralPath $Stage -Force).FullName
$stageName = "xberg-cli-$Target"
if (-not (Test-Path -LiteralPath $stageRoot -PathType Container) -or
    (Split-Path -Leaf $stageRoot) -cne $stageName) {
  throw "Stage must be a directory named '$stageName'"
}
$outputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
$stagePrefix = $stageRoot.TrimEnd('\', '/') + [System.IO.Path]::DirectorySeparatorChar
if ($outputRoot.Equals($stageRoot, [StringComparison]::OrdinalIgnoreCase) -or
    $outputRoot.StartsWith($stagePrefix, [StringComparison]::OrdinalIgnoreCase)) {
  throw 'OutputDirectory must be outside Stage'
}
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$manifestPath = Join-Path $outputRoot "xberg-components-$Target.json"
# A failed replacement must never leave an old manifest advertising partially replaced archives.
Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue

function Get-ComponentOwner([string]$Path) {
  if ($Path.StartsWith('models/snapshot-ocr/', [StringComparison]::Ordinal)) { return 'snapshot' }
  if ($Path.StartsWith('models/models--', [StringComparison]::Ordinal)) { return 'document' }
  if ($Path.StartsWith('models/sense_voice_zh_en_ja_ko_yue_2024_07_17/', [StringComparison]::Ordinal) -or
      $Path.StartsWith('models/vad/', [StringComparison]::Ordinal) -or
      $Path.StartsWith('ffmpeg/', [StringComparison]::Ordinal) -or
      $Path.StartsWith('samples/', [StringComparison]::Ordinal)) { return 'media' }
  # Both document and snapshot OCR need this license; include it with every common runtime.
  if ($Path -ceq 'models/paddleocr-onnx-models-LICENSE.txt') { return 'common' }
  if ($Path.StartsWith('models/', [StringComparison]::Ordinal)) {
    throw "No component ownership rule for staged model '$Path'"
  }
  return 'common'
}

$componentIds = @('document', 'snapshot', 'media')
$owned = [ordered]@{ common = [System.Collections.Generic.List[object]]::new() }
foreach ($id in $componentIds) { $owned[$id] = [System.Collections.Generic.List[object]]::new() }
$seen = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$entries = @(Get-Item -LiteralPath $stageRoot -Force) + @(Get-ChildItem -LiteralPath $stageRoot -Recurse -Force)
foreach ($entry in $entries) {
  if (($entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw "Staged links/reparse points are not supported: '$($entry.FullName)'"
  }
  if ($entry.PSIsContainer) { continue }
  $relative = [System.IO.Path]::GetRelativePath($stageRoot, $entry.FullName).Replace('\', '/')
  # Names must also be safe for consumers extracting onto a Windows runtime directory.
  if ($relative -match '(^|/)(\.|\.\.)(/|$)|[\x00-\x1f:<>"|?*]|[. ](/|$)' -or
      $relative.StartsWith('/') -or -not $seen.Add($relative)) {
    throw "Unsafe or duplicate staged member '$relative'"
  }
  $id = Get-ComponentOwner $relative
  $owned[$id].Add([pscustomobject]@{
      path = $relative
      source = $entry.FullName
      size_bytes = [long]$entry.Length
      sha256 = (Get-FileHash -LiteralPath $entry.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    })
}
foreach ($id in $componentIds) {
  if ($owned[$id].Count -eq 0) { throw "Component '$id' has no staged files" }
}
# These are shared or component-local redistribution notices, not optional support files.
foreach ($required in @('xberg.exe', 'LICENSE', 'THIRD_PARTY_LICENSES.md',
    'models/paddleocr-onnx-models-LICENSE.txt', 'ffmpeg/LICENSE.txt', 'sherpa-onnx/LICENSE')) {
  if (-not $seen.Contains($required)) { throw "Required staged runtime/license file missing: '$required'" }
}

$components = [System.Collections.Generic.List[object]]::new()
$audioAssets = $null
foreach ($id in ($componentIds + @('audio-assets'))) {
  $archiveName = "xberg-$id-$Target.zip"
  $archivePath = Join-Path $outputRoot $archiveName
  $files = if ($id -ceq 'audio-assets') {
    @($owned['media'].ToArray() | Where-Object {
        -not $_.path.StartsWith('samples/', [StringComparison]::Ordinal)
      } | Sort-Object -Property path -CaseSensitive)
  }
  else {
    @(@($owned['common'].ToArray()) + @($owned[$id].ToArray()) | Sort-Object -Property path -CaseSensitive)
  }
  $stream = [System.IO.File]::Open($archivePath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
  try {
    $zip = [System.IO.Compression.ZipArchive]::new($stream, [System.IO.Compression.ZipArchiveMode]::Create, $true)
    try {
      foreach ($file in $files) {
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
          $zip, $file.source, "$stageName/$($file.path)", [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
      }
    }
    finally { $zip.Dispose() }
  }
  finally { $stream.Dispose() }

  $members = [System.Collections.Generic.List[object]]::new()
  $zip = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
  try {
    if ($zip.Entries.Count -ne $files.Count) { throw "Archive member count mismatch: '$archiveName'" }
    foreach ($file in $files) {
      $entry = $zip.GetEntry("$stageName/$($file.path)")
      if ($null -eq $entry) { throw "Archive member missing: '$($file.path)'" }
      $memberStream = $entry.Open()
      try {
        $sha256 = [Convert]::ToHexString([System.Security.Cryptography.SHA256]::HashData($memberStream)).ToLowerInvariant()
      }
      finally { $memberStream.Dispose() }
      if ($entry.Length -ne $file.size_bytes -or $sha256 -cne $file.sha256) {
        throw "Archive bytes differ from staging: '$($file.path)'"
      }
      $members.Add([ordered]@{ path = $file.path; size_bytes = [long]$entry.Length; sha256 = $sha256 })
    }
  }
  finally { $zip.Dispose() }
  $record = [ordered]@{
      id = $id
      archive = $archiveName
      requires = @()
      size_bytes = [long](Get-Item -LiteralPath $archivePath).Length
      sha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
      members = @($members.ToArray())
    }
  if ($id -ceq 'audio-assets') { $audioAssets = $record }
  else { $components.Add($record) }
  Write-Host ("component: {0} ({1:N1} MB, {2} members)" -f $archiveName, ((Get-Item -LiteralPath $archivePath).Length / 1MB), $members.Count)
}
$manifest = [ordered]@{
  schema_version = 1
  target = $Target
  components = @($components.ToArray())
  audio_assets = $audioAssets
}
[System.IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 8) + "`n", [System.Text.UTF8Encoding]::new($false))
Write-Host "manifest: $manifestPath"
