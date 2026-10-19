<#
.SYNOPSIS
Fetches what cw-audio is built against: the sherpa-onnx static libraries without TTS, and the
Silero voice activity model.

.DESCRIPTION
Leaves the libraries in <Destination>\sherpa-onnx\lib, where .cargo/config.toml points
SHERPA_ONNX_LIB_DIR, and silero_vad.onnx in <Destination>. Each download is checked against a
pinned SHA-256. Does nothing when every file is already in place.
#>
[CmdletBinding()]
param(
    # Defaults to target\vendor of this repository.
    [string]$Destination
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# The version is the one Cargo.toml pins sherpa-onnx to.
$ArchiveName = 'sherpa-onnx-v1.13.8-win-x64-static-MT-Release-no-tts-lib'
$ArchiveUrl = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.8/$ArchiveName.tar.bz2"
$ArchiveSha256 = 'f0aa074ddc39553b30208b53e84c12275d09702911c9faa8d34fa932428e4641'
$Libraries = @{
    'kaldi-decoder-core.lib'        = 4308664
    'kaldi-native-fbank-core.lib'   = 1852382
    'kissfft-float.lib'             = 38212
    'onnxruntime.lib'               = 944135846
    'sherpa-onnx-c-api.lib'         = 565810
    'sherpa-onnx-core.lib'          = 51585312
    'sherpa-onnx-cxx-api.lib'       = 629172
    'sherpa-onnx-fst.lib'           = 13558300
    'sherpa-onnx-fstfar.lib'        = 601326
    'sherpa-onnx-kaldifst-core.lib' = 23055672
    'ssentencepiece_core.lib'       = 823206
}

# sherpa-onnx-sys links these three whether or not TTS is used, and the build without TTS does not
# ship them. An archive with no members satisfies the linker.
$Stubs = 'espeak-ng.lib', 'piper_phonemize.lib', 'ucd.lib'
$EmptyArchive = [Text.Encoding]::ASCII.GetBytes("!<arch>`n")

$VadName = 'silero_vad.onnx'
$VadUrl = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/$VadName"
$VadSha256 = '9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6'

# By full path: a bare `tar` can be Git's GNU tar, which misreads `C:\` paths, and Windows
# PowerShell aliases a bare `curl` to Invoke-WebRequest.
$Tar = Join-Path $env:SystemRoot 'System32\tar.exe'
$Curl = Join-Path $env:SystemRoot 'System32\curl.exe'

# Not Get-FileHash: Windows PowerShell cannot load it when it inherits PowerShell 7's module path.
function Test-Sha256([string]$Path, [string]$Sha256) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $false
    }
    $stream = [IO.File]::OpenRead($Path)
    try {
        $algorithm = [Security.Cryptography.SHA256]::Create()
        try {
            $actual = [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-', '')
        }
        finally {
            $algorithm.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
    $actual -eq $Sha256
}

# Called with the incoming folder as the working directory, like tar below: tar cannot open a
# path that has characters outside the ANSI code page, so both work with names relative to it.
function Save-Verified([string]$Url, [string]$Sha256, [string]$Name) {
    Write-Host "Downloading $Url"
    & $Curl --fail --location --silent --show-error --retry 3 --connect-timeout 30 --max-time 1800 --output $Name $Url
    if ($LASTEXITCODE -ne 0) {
        throw "curl exited with $LASTEXITCODE for $Url"
    }
    if (-not (Test-Sha256 (Join-Path $incoming $Name) $Sha256)) {
        throw "$Url does not have the SHA-256 $Sha256"
    }
}

function Get-MissingLibraries([string]$Directory) {
    $expected = $Libraries.Clone()
    foreach ($stub in $Stubs) {
        $expected[$stub] = $EmptyArchive.Length
    }
    foreach ($name in $expected.Keys) {
        $path = Join-Path $Directory $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or (Get-Item -LiteralPath $path).Length -ne $expected[$name]) {
            $name
        }
    }
}

if (-not $Destination) {
    $Destination = Join-Path $PSScriptRoot '..\target\vendor'
}
$Destination = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Destination)
$libraryDirectory = Join-Path $Destination 'sherpa-onnx\lib'
$vad = Join-Path $Destination $VadName

if (Test-Path -LiteralPath $Destination) {
    Get-ChildItem -LiteralPath $Destination -Directory -Filter '.incoming-*' |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force }
}

$missingLibraries = @(Get-MissingLibraries $libraryDirectory)
$vadPresent = Test-Sha256 $vad $VadSha256
if ($missingLibraries.Count -eq 0 -and $vadPresent) {
    Write-Host "Nothing to fetch: $Destination is complete."
    return
}

$incoming = Join-Path $Destination ".incoming-$PID"
New-Item -ItemType Directory -Force -Path $incoming | Out-Null
Push-Location -LiteralPath $incoming
try {
    if (-not $vadPresent) {
        Save-Verified $VadUrl $VadSha256 $VadName
        Move-Item -LiteralPath (Join-Path $incoming $VadName) -Destination $vad -Force
    }

    if ($missingLibraries.Count -ne 0) {
        Write-Host "Missing or incomplete in ${libraryDirectory}: $($missingLibraries -join ', ')"
        Save-Verified $ArchiveUrl $ArchiveSha256 "$ArchiveName.tar.bz2"
        & $Tar -xf "$ArchiveName.tar.bz2"
        if ($LASTEXITCODE -ne 0) {
            throw "tar exited with $LASTEXITCODE for $ArchiveName.tar.bz2"
        }
        $unpacked = Join-Path $incoming "$ArchiveName\lib"

        # Before the stubs go in: a library by one of their names would be the build with TTS.
        $unexpected = @(Get-ChildItem -LiteralPath $unpacked |
                ForEach-Object { $_.Name } |
                Where-Object { -not $Libraries.ContainsKey($_) })
        if ($unexpected.Count -ne 0) {
            throw "The archive holds libraries this script does not expect: $($unexpected -join ', ')"
        }
        foreach ($stub in $Stubs) {
            [IO.File]::WriteAllBytes((Join-Path $unpacked $stub), $EmptyArchive)
        }
        $incomplete = @(Get-MissingLibraries $unpacked)
        if ($incomplete.Count -ne 0) {
            throw "Missing from the archive or unpacked with another size: $($incomplete -join ', ')"
        }

        if (Test-Path -LiteralPath $libraryDirectory) {
            Remove-Item -LiteralPath $libraryDirectory -Recurse -Force
        }
        New-Item -ItemType Directory -Force -Path (Split-Path $libraryDirectory) | Out-Null
        # Unlike Move-Item, this fails when the target exists instead of moving into it.
        [IO.Directory]::Move($unpacked, $libraryDirectory)
    }
}
finally {
    Pop-Location
    Remove-Item -LiteralPath $incoming -Recurse -Force -ErrorAction SilentlyContinue
}

$missingLibraries = @(Get-MissingLibraries $libraryDirectory)
if ($missingLibraries.Count -ne 0 -or -not (Test-Sha256 $vad $VadSha256)) {
    throw "$Destination is still incomplete after fetching: $($missingLibraries -join ', ')"
}
Write-Host "Fetched into $Destination."
