# pe-imports.ps1
#
# Shared PE (COFF) import-table reader used by the Windows native-artifact gates
# (`scripts/ci/verify-windows-dll-closure.ps1` and the CLI packaging script under
# `scripts/publish/cli/`). Dot-source it; it defines functions only and has no
# side effects.
#
#   . "$PSScriptRoot/../lib/pe-imports.ps1"
#   $names = Get-PeImportedDllNames "path/to/xberg.exe"

function Get-PeImportedDllNames([string]$Path) {
  # Minimal COFF/PE import-directory parser. Returns the literal DLL name
  # strings the file's import table references (case as stored, usually
  # mixed-case as written by the linker that produced the .lib).
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  $stream = New-Object System.IO.MemoryStream(, $bytes)
  $br = New-Object System.IO.BinaryReader($stream)
  try {
    $stream.Seek(0x3C, [System.IO.SeekOrigin]::Begin) | Out-Null
    $peOffset = $br.ReadInt32()
    $stream.Seek($peOffset, [System.IO.SeekOrigin]::Begin) | Out-Null
    $sig = $br.ReadUInt32()
    if ($sig -ne 0x00004550) { throw "not a PE file (bad signature) at '$Path'" }

    $machine = $br.ReadUInt16()
    $numberOfSections = $br.ReadUInt16()
    $stream.Seek(12, [System.IO.SeekOrigin]::Current) | Out-Null # TimeDateStamp, PointerToSymbolTable, NumberOfSymbols
    $sizeOfOptionalHeader = $br.ReadUInt16()
    $stream.Seek(2, [System.IO.SeekOrigin]::Current) | Out-Null # Characteristics
    $optionalHeaderStart = $stream.Position

    $magic = $br.ReadUInt16()
    $isPe32Plus = ($magic -eq 0x20B)
    # DataDirectory[0] starts at offset 0x60 (PE32, IMAGE_OPTIONAL_HEADER32) or
    # 0x70 (PE32+, IMAGE_OPTIONAL_HEADER64) from the optional header start.
    # The +16 comes from SizeOfStackReserve/StackCommit/HeapReserve/HeapCommit,
    # which are 4 bytes each in PE32 and 8 each in PE32+ (4 fields x +4 = +16).
    # ImageBase widening (4 -> 8) and the dropped 4-byte BaseOfData field cancel
    # each other out and contribute nothing to the shift -- do not "correct" this
    # back, the two effects are genuinely independent.
    # NumberOfRvaAndSizes is the 4-byte field immediately before DataDirectory[0].
    $dataDirectoryStart = $optionalHeaderStart + $(if ($isPe32Plus) { 0x70 } else { 0x60 })
    $stream.Position = $dataDirectoryStart - 4
    $numberOfRvaAndSizes = $br.ReadUInt32()
    if ($numberOfRvaAndSizes -lt 2) { return @() } # no import directory at all

    # DataDirectory[1] = Import Table; each entry is 8 bytes (VirtualAddress, Size).
    $stream.Position = $dataDirectoryStart + 8 # entry index 1
    $importTableRva = $br.ReadUInt32()
    $importTableSize = $br.ReadUInt32()
    if ($importTableRva -eq 0 -or $importTableSize -eq 0) { return @() }

    $sectionHeadersStart = $optionalHeaderStart + $sizeOfOptionalHeader
    $sections = @()
    $stream.Position = $sectionHeadersStart
    for ($i = 0; $i -lt $numberOfSections; $i++) {
      $nameBytes = $br.ReadBytes(8)
      $virtualSize = $br.ReadUInt32()
      $virtualAddress = $br.ReadUInt32()
      $stream.Seek(4, [System.IO.SeekOrigin]::Current) | Out-Null # SizeOfRawData
      $pointerToRawData = $br.ReadUInt32()
      $stream.Seek(16, [System.IO.SeekOrigin]::Current) | Out-Null # remaining fields to next 40-byte header
      $sections += [PSCustomObject]@{
        VirtualAddress   = $virtualAddress
        VirtualSize      = $virtualSize
        PointerToRawData = $pointerToRawData
      }
    }

    function Rva2Offset([uint32]$Rva) {
      foreach ($s in $sections) {
        if ($Rva -ge $s.VirtualAddress -and $Rva -lt ($s.VirtualAddress + [Math]::Max($s.VirtualSize, 1))) {
          return [int64]($Rva - $s.VirtualAddress + $s.PointerToRawData)
        }
      }
      throw "RVA 0x$($Rva.ToString('X')) not found in any section of '$Path'"
    }

    function ReadAsciiZ([int64]$Offset) {
      $stream.Position = $Offset
      $sb = New-Object System.Text.StringBuilder
      while ($true) {
        $b = $br.ReadByte()
        if ($b -eq 0) { break }
        [void]$sb.Append([char]$b)
      }
      return $sb.ToString()
    }

    $importDirOffset = Rva2Offset $importTableRva
    $names = @()
    $descriptorOffset = $importDirOffset
    while ($true) {
      $stream.Position = $descriptorOffset
      $originalFirstThunk = $br.ReadUInt32()
      $stream.Seek(8, [System.IO.SeekOrigin]::Current) | Out-Null # TimeDateStamp, ForwarderChain
      $nameRva = $br.ReadUInt32()
      $stream.Seek(4, [System.IO.SeekOrigin]::Current) | Out-Null # FirstThunk
      # An all-zero 20-byte descriptor terminates the array.
      if ($originalFirstThunk -eq 0 -and $nameRva -eq 0) { break }
      if ($nameRva -ne 0) {
        $names += ReadAsciiZ (Rva2Offset $nameRva)
      }
      $descriptorOffset += 20
    }
    return $names
  }
  finally {
    $br.Dispose()
    $stream.Dispose()
  }
}
