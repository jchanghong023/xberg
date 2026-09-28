#Requires -Version 7.0
<#
.SYNOPSIS
Provision the pinned media/snapshot assets for package-cli-windows.ps1.

.DESCRIPTION
Populates -MediaAssetRoot (default <repo>/.tmp/assets) with every byte-pinned
asset the packaging script stages into the bundle: the TextSnap snapshot OCR
model set, SenseVoice INT8 + tokens + Silero VAD, the sherpa-onnx v1.13.6
dynamic libraries (extracted from the official tar.bz2) and the FFmpeg n9.0.2
shared LGPL DLLs (extracted from the BtbN autobuild zip).

Every file is verified against a pinned size + SHA-256 -- already-present
verified files are reused, everything else is downloaded/extracted from the
same public sources the JchTools release pipeline pinned, and any mismatch
fails the run. The packaging script re-verifies every byte on staging, so this
provisioner can never weaken verification; its pins exist to fail fast and to
make CI provisioning reproducible.

Used by .github/workflows/build-windows-cli.yml before packaging; also safe to
run locally (the reuse path makes repeat runs a no-op hash check).
#>
[CmdletBinding()]
param(
  [string]$MediaAssetRoot = "",
  [string]$RepoRoot = ""
)

$ErrorActionPreference = "Stop"
if (-not $RepoRoot) { $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../../..")).Path }
if (-not $MediaAssetRoot) { $MediaAssetRoot = Join-Path $RepoRoot ".tmp/assets" }
New-Item -ItemType Directory -Path $MediaAssetRoot -Force | Out-Null

# Direct-file assets: label, url, destination under the asset root, pinned size/sha.
$DirectAssets = @(
  @{ Label = "snapshot det";  Url = "https://github.com/jchanghong023/JchTools/releases/download/optional-components-v0.1.0/snap-det.onnx";  Dest = "snapshot-models/det.onnx"; SizeBytes = 9891707;  Sha256 = "3914f972d833af87d23bb2338bd09238f978a48f3c4dbb8e1a4ee26a93869940" }
  @{ Label = "snapshot rec";  Url = "https://github.com/jchanghong023/JchTools/releases/download/optional-components-v0.1.0/snap-rec.onnx";  Dest = "snapshot-models/rec.onnx"; SizeBytes = 21148338; Sha256 = "3e3def686ac9a1676b59bc9749ad896263d8f68b53f352060774de359a2e23ed" }
  @{ Label = "snapshot dict"; Url = "https://github.com/jchanghong023/JchTools/releases/download/optional-components-v0.1.0/snap-dict.txt"; Dest = "snapshot-models/dict/dict.txt"; SizeBytes = 74947; Sha256 = "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d" }
  @{ Label = "sensevoice model"; Url = "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/2365baeacb507f821a0c8120fcee3d484dba7a07/model.int8.onnx"; Dest = "media-models/models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx"; SizeBytes = 239233841; Sha256 = "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51" }
  @{ Label = "sensevoice tokens"; Url = "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/2365baeacb507f821a0c8120fcee3d484dba7a07/tokens.txt"; Dest = "media-models/models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt"; SizeBytes = 315894; Sha256 = "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc" }
  @{ Label = "silero vad"; Url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"; Dest = "media-models/models/vad/silero_vad.onnx"; SizeBytes = 643854; Sha256 = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6" }
  @{ Label = "sherpa license"; Url = "https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/v1.13.6/LICENSE"; Dest = "media-dlls/sherpa-onnx/LICENSE"; SizeBytes = 11358; Sha256 = "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30" }
)

# Archive assets: the archive itself is pinned, and each member is extracted
# to its destination and re-verified before being accepted.
$ArchiveAssets = @(
  @{ Label = "sherpa-onnx v1.13.6"; Url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.6/sherpa-onnx-v1.13.6-win-x64-shared-MD-Release-no-tts.tar.bz2"; Kind = "tarbz2"; SizeBytes = 18748740; Sha256 = "071d6641efd737a1f60de48c9c4cd596f78d5b0980815e8ad3798c95785d2b26";
     Members = @(
       @{ Entry = "sherpa-onnx-v1.13.6-win-x64-shared-MD-Release-no-tts/lib/sherpa-onnx-c-api.dll"; Dest = "media-dlls/sherpa-onnx-c-api.dll"; SizeBytes = 2866688; Sha256 = "c372657098ced2cac7d0a54d2926f3ce61542b06ad0a6eb71c8aee8c186db6c9" },
       @{ Entry = "sherpa-onnx-v1.13.6-win-x64-shared-MD-Release-no-tts/lib/sherpa-onnx-cxx-api.dll"; Dest = "media-dlls/sherpa-onnx-cxx-api.dll"; SizeBytes = 128512; Sha256 = "a90a5e2065a23e52996b5aed2e296f6d71c85b3966ec77fbda57604d1defcbdc" },
       @{ Entry = "sherpa-onnx-v1.13.6-win-x64-shared-MD-Release-no-tts/bin/onnxruntime.dll"; Dest = "media-dlls/onnxruntime.dll"; SizeBytes = 16720896; Sha256 = "4ee0ae76cbf51bde6999f36829939b2b06d340ab57867ef82b61a0b674111efa" },
       @{ Entry = "sherpa-onnx-v1.13.6-win-x64-shared-MD-Release-no-tts/bin/onnxruntime_providers_shared.dll"; Dest = "media-dlls/onnxruntime_providers_shared.dll"; SizeBytes = 10752; Sha256 = "cd7245821ad7054d1904ac221ca3d6b913c0e8977f0b408787f3bc2c430a5403" }
     ) }
  @{ Label = "ffmpeg n9.0.2 shared"; Url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-09-24-14-14/ffmpeg-n9.0.2-3-ga5923073bf-win64-lgpl-shared-9.0.zip"; Kind = "zip"; SizeBytes = 76974101; Sha256 = "735bae484ba2c3342bfb34df477b9c6b0f43f9819f4d2fde011be293ee1b6517";
     Members = @(
       @{ Entry = "ffmpeg-n9.0.2-3-ga5923073bf-win64-lgpl-shared-9.0/bin/avutil-61.dll"; Dest = "media-dlls/ffmpeg/avutil-61.dll"; SizeBytes = 3016704; Sha256 = "26ee298e9e71ec1d018d0394bc8f967f6742fc73380b9a196fa95737a1ac3a9b" },
       @{ Entry = "ffmpeg-n9.0.2-3-ga5923073bf-win64-lgpl-shared-9.0/bin/swresample-7.dll"; Dest = "media-dlls/ffmpeg/swresample-7.dll"; SizeBytes = 734720; Sha256 = "aea51fb6a87787276334d77fa3d472154641dad9fc7815245f690fdca41ec572" },
       @{ Entry = "ffmpeg-n9.0.2-3-ga5923073bf-win64-lgpl-shared-9.0/bin/avcodec-63.dll"; Dest = "media-dlls/ffmpeg/avcodec-63.dll"; SizeBytes = 91182080; Sha256 = "9d82d5c867632d3c0758edb6273b47ad8f3e97aeda66d4c4ac784f00320597f8" },
       @{ Entry = "ffmpeg-n9.0.2-3-ga5923073bf-win64-lgpl-shared-9.0/bin/avformat-63.dll"; Dest = "media-dlls/ffmpeg/avformat-63.dll"; SizeBytes = 22764032; Sha256 = "9b41797f2a749765f8c811b015461e737d09ef99bdf344c4c9bb5e4da30fe320" },
       @{ Entry = "ffmpeg-n9.0.2-3-ga5923073bf-win64-lgpl-shared-9.0/LICENSE.txt"; Dest = "media-dlls/ffmpeg/LICENSE.txt"; SizeBytes = 7651; Sha256 = "da7eabb7bafdf7d3ae5e9f223aa5bdc1eece45ac569dc21b3b037520b4464768" }
     ) }
)

function Test-Pinned {
  param([string]$Path, [long]$SizeBytes, [string]$Sha256)
  if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
  if ((Get-Item -LiteralPath $Path).Length -ne $SizeBytes) { return $false }
  return ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() -eq $Sha256)
}

function Save-Pinned {
  param([string]$Url, [string]$Path, [long]$SizeBytes, [string]$Sha256)
  $parent = Split-Path -Parent $Path
  if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
  $tmp = "$Path.download"
  try {
    Invoke-WebRequest -Uri $Url -OutFile $tmp -UseBasicParsing
    if (-not (Test-Pinned -Path $tmp -SizeBytes $SizeBytes -Sha256 $Sha256)) {
      throw "downloaded bytes do not match the pinned size/SHA-256: $Url"
    }
    Move-Item -Force -LiteralPath $tmp -Destination $Path
  }
  finally {
    if (Test-Path -LiteralPath $tmp) { Remove-Item -Force -LiteralPath $tmp }
  }
}

$downloaded = 0
$reused = 0
foreach ($asset in $DirectAssets) {
  $dest = Join-Path $MediaAssetRoot $asset.Dest
  if (Test-Pinned -Path $dest -SizeBytes $asset.SizeBytes -Sha256 $asset.Sha256) {
    $reused++
    Write-Host "  reuse $($asset.Label)"
    continue
  }
  Write-Host "  fetch $($asset.Label)"
  Save-Pinned -Url $asset.Url -Path $dest -SizeBytes $asset.SizeBytes -Sha256 $asset.Sha256
  $downloaded++
}

$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("xberg-media-provision-" + [System.IO.Path]::GetRandomFileName())
try {
  foreach ($archive in $ArchiveAssets) {
    # All members verified in place -> nothing to do for this archive.
    $missing = @($archive.Members | Where-Object {
      -not (Test-Pinned -Path (Join-Path $MediaAssetRoot $_.Dest) -SizeBytes $_.SizeBytes -Sha256 $_.Sha256)
    })
    if ($missing.Count -eq 0) {
      $reused += $archive.Members.Count
      Write-Host "  reuse $($archive.Label) (all members)"
      continue
    }
    Write-Host "  fetch $($archive.Label)"
    New-Item -ItemType Directory -Path $scratch -Force | Out-Null
    $archivePath = Join-Path $scratch "archive.$($archive.Kind)"
    Save-Pinned -Url $archive.Url -Path $archivePath -SizeBytes $archive.SizeBytes -Sha256 $archive.Sha256
    $extracted = Join-Path $scratch "extracted"
    New-Item -ItemType Directory -Path $extracted -Force | Out-Null
    if ($archive.Kind -eq "zip") {
      Expand-Archive -LiteralPath $archivePath -DestinationPath $extracted
    }
    else {
      # bsdtar ships with Windows runners and understands .tar.bz2.
      & tar -xjf $archivePath -C $extracted
      if ($LASTEXITCODE -ne 0) { throw "tar extraction failed for $($archive.Url)" }
    }
    foreach ($member in $archive.Members) {
      $source = Join-Path $extracted $member.Entry
      if (-not (Test-Pinned -Path $source -SizeBytes $member.SizeBytes -Sha256 $member.Sha256)) {
        throw "archive member '$($member.Entry)' does not match its pinned size/SHA-256 in $($archive.Url)"
      }
      $dest = Join-Path $MediaAssetRoot $member.Dest
      $parent = Split-Path -Parent $dest
      if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
      Copy-Item -LiteralPath $source -Destination $dest -Force
      if (-not (Test-Pinned -Path $dest -SizeBytes $member.SizeBytes -Sha256 $member.Sha256)) {
        throw "copied member $($member.Dest) failed verification"
      }
      $downloaded++
    }
  }
}
finally {
  if (Test-Path -LiteralPath $scratch) { Remove-Item -Recurse -Force -LiteralPath $scratch }
}

Write-Host "provisioned media assets: $downloaded fetched, $reused reused (root $MediaAssetRoot)"
