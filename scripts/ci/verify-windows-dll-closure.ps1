# verify-windows-dll-closure.ps1
#
# Windows analogue of scripts/ci/verify-glibc-floor.sh: proves a Windows native
# artifact (wheel, PHP/Elixir/C-FFI archive, or CLI zip) is loadable on a clean
# Windows host by inspecting the PE import table of every native library it
# ships, rather than trusting that a build succeeded.
#
# It checks three independent things, all required to fix xberg-io/xberg#1456
# (the third only when -RequireImportClosure is passed):
#   1. ABSENCE: no shipped .pyd/.dll/.exe statically imports $ForbiddenDll
#      (default DirectML.dll). A hard PE import is resolved by the Windows
#      loader before a single instruction of the module runs, so an unshipped
#      import here is fatal at `import xberg` / DllMain, not a runtime error
#      you can catch.
#   2. PRESENCE: $RequiredDll (default onnxruntime.dll) is shipped in the same
#      directory as each matched native file, because moving off the pyke
#      static-link strategy (which baked ORT in) onto system dynamic linking
#      makes that a real runtime dependency that must be vendored. (The check
#      is per-directory, not a tree-wide search: the loader resolves an import
#      next to the importing module first, so that is where the DLL must be.)
#   3. CLOSURE (opt-in): every DLL imported by every shipped .exe/.dll is either
#      shipped in the artifact or present in %SystemRoot%\System32. Self-contained
#      bundles (e.g. the Windows CLI zip) use this to prove they load on a host
#      with nothing on PATH.
#
# Usage:
#   verify-windows-dll-closure.ps1 <artifact> <NativeGlob> [RequiredDll] [ForbiddenDll] [-RequireImportClosure]
#
#   <artifact>     Path to a .whl, .zip, .tar.gz, or an already-extracted directory.
#   <NativeGlob>   Filename glob identifying the native library to inspect,
#                  e.g. "*.pyd", "php_xberg.dll", "xberg_ffi.dll", "xberg.exe".
#
# Exit 0 only if every matched native file passes both checks. Exit 1 with a
# specific reason otherwise.

param(
  [Parameter(Mandatory = $true)][string]$Artifact,
  [Parameter(Mandatory = $true)][string]$NativeGlob,
  [string]$RequiredDll = "onnxruntime.dll",
  [string]$ForbiddenDll = "DirectML.dll",
  # Check 3: every DLL imported by any shipped .exe/.dll is either shipped
  # inside the artifact or lives in %SystemRoot%\System32. Bundled CLI zips
  # (scripts/publish/cli/package-cli-windows.ps1) use this to prove the
  # extracted tree loads on a machine with no ORT/vcpkg/pdfium on PATH.
  [switch]$RequireImportClosure
)

$ErrorActionPreference = "Stop"

function Write-Log([string]$Message) {
  Write-Host "verify-windows-dll-closure: $Message"
}

. (Join-Path $PSScriptRoot "lib/pe-imports.ps1")

function Assert-PeImportClosureUnderRoot([string]$Root) {
  # Every PE in the tree must resolve each import either to a shipped file or to
  # a system DLL. `api-ms-win-*`/`ext-ms-*` are API-set virtual names resolved by
  # the loader's apiset schema, never files on disk, so they cannot be probed
  # with Test-Path and are taken as system-provided.
  $shipped = @{}
  Get-ChildItem -Path $Root -Recurse -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Extension -in @(".dll", ".exe") } |
    ForEach-Object { $shipped[$_.Name] = $true }

  $system32 = Join-Path ([Environment]::GetFolderPath("Windows")) "System32"
  $peFiles = Get-ChildItem -Path $Root -Recurse -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Extension -in @(".dll", ".exe") }

  $failures = @()
  foreach ($pe in $peFiles) {
    foreach ($import in (Get-PeImportedDllNames $pe.FullName)) {
      if ($shipped.ContainsKey($import)) { continue }
      if ($import -match '^(api|ext)-ms-win-') { continue }
      if (Test-Path -LiteralPath (Join-Path $system32 $import)) { continue }
      $relative = [System.IO.Path]::GetRelativePath($Root, $pe.FullName)
      $failures += "$relative imports $import, which is neither shipped in the artifact nor present in $system32"
    }
  }
  return $failures
}

function Expand-Artifact([string]$ArtifactPath, [string]$Dest) {
  New-Item -ItemType Directory -Path $Dest -Force | Out-Null
  switch -Regex ($ArtifactPath) {
    '\.(whl|zip)$' {
      Expand-Archive -Path $ArtifactPath -DestinationPath $Dest -Force
    }
    '\.tar\.gz$|\.tgz$' {
      & tar -xzf $ArtifactPath -C $Dest
      if ($LASTEXITCODE -ne 0) { throw "tar extraction failed for '$ArtifactPath'" }
    }
    default {
      throw "unsupported artifact type: '$ArtifactPath'"
    }
  }
}

$root = $Artifact
$workDir = $null
if (Test-Path -PathType Container $Artifact) {
  $root = (Resolve-Path $Artifact).Path
}
else {
  $workDir = Join-Path ([System.IO.Path]::GetTempPath()) ("verify-windows-dll-closure-" + [Guid]::NewGuid().ToString("N"))
  Expand-Artifact -ArtifactPath $Artifact -Dest $workDir
  $root = $workDir
}

try {
  $natives = @(Get-ChildItem -Path $root -Recurse -File -Filter $NativeGlob -ErrorAction SilentlyContinue)
  if ($natives.Count -eq 0) {
    Write-Error "no native file matching '$NativeGlob' found under '$root' (artifact: '$Artifact')"
    exit 1
  }

  $failures = @()
  foreach ($native in $natives) {
    Write-Log "inspecting $($native.FullName)"
    $imports = Get-PeImportedDllNames $native.FullName
    $forbidden = $imports | Where-Object { $_ -ieq $ForbiddenDll }
    if ($forbidden) {
      $failures += "$($native.Name) imports $ForbiddenDll, which is never shipped -- the Windows loader will refuse to load this module (xberg-io/xberg#1456)"
      continue
    }

    $requiredPresent = @(Get-ChildItem -Path $native.Directory.FullName -Filter $RequiredDll -File -ErrorAction SilentlyContinue).Count -gt 0
    if (-not $requiredPresent) {
      $failures += "$($native.Name) is dynamically linked but $RequiredDll is not shipped alongside it in $($native.Directory.FullName)"
    }
    else {
      Write-Log "OK $($native.Name) (no $ForbiddenDll import, $RequiredDll present)"
    }
  }

  if ($RequireImportClosure) {
    Write-Log "checking PE import closure for every .exe/.dll under $root"
    $closureFailures = Assert-PeImportClosureUnderRoot -Root $root
    if ($closureFailures.Count -gt 0) {
      $failures += $closureFailures
    }
    else {
      Write-Log "OK import closure (every imported DLL is shipped or provided by Windows)"
    }
  }

  if ($failures.Count -gt 0) {
    # Write-Error throws under $ErrorActionPreference = "Stop", so raising the
    # failures inside the loop would report the first one and drop the rest;
    # print every failure first, then throw once with the count.
    foreach ($f in $failures) { Write-Log "FAIL: $f" }
    throw "$($failures.Count) DLL closure check failure(s) (artifact: '$Artifact')"
  }
  Write-Log "artifact passes the Windows DLL closure gate"
  exit 0
}
finally {
  if ($workDir -and (Test-Path $workDir)) { Remove-Item -Recurse -Force $workDir }
}
