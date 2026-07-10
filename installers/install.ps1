#!/usr/bin/env pwsh
param(
    [string]$InstallDir = $env:INSTALL_DIR,
    [string]$Version = $env:VERSION,
    [switch]$BuildFromSource,
    [switch]$Force,
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$BinaryName = 'wolfie'
$ExecutableName = "$BinaryName.exe"
$GitHubRepo = if ([string]::IsNullOrWhiteSpace($env:GITHUB_REPO)) { 'ToneAr/wolfie' } else { $env:GITHUB_REPO }
$ConfigSchemaUrl = 'https://raw.githubusercontent.com/ToneAr/wolfie/main/schemas/config.schema.json'

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = 'latest'
}

function Show-Usage {
    @"
Install wolfie on Windows.

Usage:
  .\install.ps1 [options]

Options:
  -InstallDir DIR    Install the binary into DIR.
                     Defaults to `%LOCALAPPDATA%\Programs\wolfie\bin,
                     unless `%USERPROFILE%\.local\bin is writable and already on PATH.
  -Version TAG       Install a specific GitHub release tag, such as v0.2.0.
                     Defaults to the latest release.
  -BuildFromSource   Build this checkout with cargo and install the result.
  -Force             Replace an existing binary at the destination.
  -Help              Show this help.

Environment:
  INSTALL_DIR         Same as -InstallDir.
  VERSION             Same as -Version.
  GITHUB_REPO         GitHub repo to download from. Defaults to ToneAr/wolfie.
  WOLFRAM_CLI_SHA256  Optional expected SHA-256 checksum for the release archive.
"@
}

function Write-Log {
    param([string]$Message)
    Write-Host $Message
}

function Fail {
    param([string]$Message)
    throw "install.ps1: $Message"
}

function Test-HasCommand {
    param([string]$Command)
    $null -ne (Get-Command $Command -ErrorAction SilentlyContinue)
}

function Require-Command {
    param([string]$Command)
    if (-not (Test-HasCommand $Command)) {
        Fail "required command not found: $Command"
    }
}

function Test-LibraryAvailable {
    param([string]$LibraryName)

    foreach ($entry in (($env:LIB -split ';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })) {
        if (Test-Path -LiteralPath (Join-Path $entry $LibraryName) -PathType Leaf) {
            return $true
        }
    }

    return $false
}

function Get-VisualStudioDevShell {
    $candidates = @()

    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $installationPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not [string]::IsNullOrWhiteSpace($installationPath)) {
            $candidates += (Join-Path $installationPath 'Common7\Tools\Launch-VsDevShell.ps1')
        }
    }

    foreach ($edition in @('Community', 'Professional', 'Enterprise', 'BuildTools')) {
        $candidates += (Join-Path $env:ProgramFiles "Microsoft Visual Studio\2022\$edition\Common7\Tools\Launch-VsDevShell.ps1")
    }

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return $candidate
        }
    }

    return $null
}

function Initialize-MsvcBuildEnvironment {
    if (-not (Test-IsWindows)) {
        return
    }

    if (Test-LibraryAvailable 'msvcrt.lib') {
        return
    }

    $devShell = Get-VisualStudioDevShell
    if ($null -ne $devShell) {
        Write-Log "Initializing Visual Studio build environment from $devShell"
        . $devShell -Arch amd64 -HostArch amd64
    }

    if (-not (Test-LibraryAvailable 'msvcrt.lib')) {
        Fail "MSVC build environment is not configured; could not find msvcrt.lib. Install Visual Studio's 'Desktop development with C++' workload, including MSVC v143 x64/x86 build tools and a Windows SDK, then run from 'Developer PowerShell for VS 2022'."
    }
}

function Test-IsWindows {
    $isWindowsVariable = Get-Variable -Name IsWindows -Scope Global -ErrorAction SilentlyContinue
    if ($null -ne $isWindowsVariable) {
        return [bool]$isWindowsVariable.Value
    }

    return $env:OS -eq 'Windows_NT'
}

function Normalize-PathEntry {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return ''
    }

    try {
        return [System.IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    } catch {
        return $Path.TrimEnd('\', '/')
    }
}

function Test-PathContains {
    param([string]$Directory)

    $normalizedDirectory = Normalize-PathEntry $Directory
    foreach ($entry in (($env:PATH -split ';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })) {
        if ((Normalize-PathEntry $entry) -ieq $normalizedDirectory) {
            return $true
        }
    }

    return $false
}

function Test-WritableDirectory {
    param([string]$Directory)

    if (-not (Test-Path -LiteralPath $Directory -PathType Container)) {
        return $false
    }

    $probe = Join-Path $Directory ".wolfie-install-test-$PID"
    try {
        New-Item -ItemType File -Path $probe -Force | Out-Null
        Remove-Item -LiteralPath $probe -Force
        return $true
    } catch {
        return $false
    }
}

function Get-DefaultInstallDir {
    $homeDirectory = if ([string]::IsNullOrWhiteSpace($HOME)) { $env:USERPROFILE } else { $HOME }
    if ([string]::IsNullOrWhiteSpace($homeDirectory)) {
        Fail 'HOME or USERPROFILE is not set'
    }

    $localBin = Join-Path $homeDirectory '.local\bin'
    if ((Test-PathContains $localBin) -and (Test-WritableDirectory $localBin)) {
        return $localBin
    }

    $localAppData = $env:LOCALAPPDATA
    if ([string]::IsNullOrWhiteSpace($localAppData)) {
        $localAppData = Join-Path $homeDirectory 'AppData\Local'
    }

    return (Join-Path $localAppData 'Programs\wolfie\bin')
}

function Get-DefaultConfigPath {
    $homeDirectory = if ([string]::IsNullOrWhiteSpace($HOME)) { $env:USERPROFILE } else { $HOME }
    if ([string]::IsNullOrWhiteSpace($homeDirectory)) {
        Fail 'HOME or USERPROFILE is not set'
    }

    $appData = $env:APPDATA
    if ([string]::IsNullOrWhiteSpace($appData)) {
        $appData = Join-Path $homeDirectory 'AppData\Roaming'
    }

    return (Join-Path (Join-Path $appData 'wolfie') 'config.json')
}

function New-DefaultConfigFile {
    $configPath = Get-DefaultConfigPath
    if (Test-Path -LiteralPath $configPath) {
        return
    }

    $configDirectory = Split-Path -Parent $configPath
    New-Item -ItemType Directory -Path $configDirectory -Force | Out-Null
    $content = "{`n  `"`$schema`": `"$ConfigSchemaUrl`"`n}`n"
    Set-Content -LiteralPath $configPath -Value $content -NoNewline -Encoding utf8
    Write-Log "Created default config at $configPath"
}

function Get-ReleaseTargetName {
    if (-not (Test-IsWindows)) {
        Fail 'unsupported operating system: this installer is for Windows'
    }

    if (-not [Environment]::Is64BitOperatingSystem) {
        Fail 'unsupported CPU architecture: wolfie release builds are only available for Windows x86_64'
    }

    return 'windows-x86_64'
}

function Invoke-DownloadFile {
    param(
        [string]$Url,
        [string]$OutputPath,
        [string]$ArchiveName
    )

    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    try {
        Invoke-WebRequest -Uri $Url -OutFile $OutputPath -UseBasicParsing
    } catch {
        $statusCode = $null
        if ($_.Exception.Response) {
            $statusCode = [int]$_.Exception.Response.StatusCode
        }

        if ($statusCode -eq 404) {
            Fail "release archive not found: $ArchiveName. Publish a GitHub Release for $GitHubRepo that includes this asset, then rerun the installer."
        }

        Fail "failed to download $Url. $($_.Exception.Message)"
    }
}

function Test-Sha256 {
    param(
        [string]$FilePath,
        [string]$ExpectedHash
    )

    if ([string]::IsNullOrWhiteSpace($ExpectedHash)) {
        return
    }

    $actualHash = (Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $ExpectedHash.ToLowerInvariant()) {
        Fail 'checksum mismatch for downloaded archive'
    }
}

function Install-Binary {
    param(
        [string]$SourcePath,
        [string]$DestinationPath
    )

    if (-not (Test-Path -LiteralPath $SourcePath -PathType Leaf)) {
        Fail "binary not found at $SourcePath"
    }

    if (Test-Path -LiteralPath $DestinationPath -PathType Container) {
        Fail "$DestinationPath already exists as a directory"
    }

    if ((Test-Path -LiteralPath $DestinationPath -PathType Leaf) -and (-not $Force)) {
        Fail "$DestinationPath already exists; rerun with -Force to replace it"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $temporaryDestination = "$DestinationPath.tmp.$PID"

    try {
        Copy-Item -LiteralPath $SourcePath -Destination $temporaryDestination -Force
        if (Test-Path -LiteralPath $DestinationPath -PathType Leaf) {
            Remove-Item -LiteralPath $DestinationPath -Force
        }
        Move-Item -LiteralPath $temporaryDestination -Destination $DestinationPath
    } catch {
        if (Test-Path -LiteralPath $temporaryDestination) {
            Remove-Item -LiteralPath $temporaryDestination -Force -ErrorAction SilentlyContinue
        }
        throw
    }
}

function Install-Release {
    $target = Get-ReleaseTargetName
    $package = "$BinaryName-$target"
    $archive = "$package.zip"

    if ($Version -eq 'latest') {
        $url = "https://github.com/$GitHubRepo/releases/latest/download/$archive"
    } else {
        $url = "https://github.com/$GitHubRepo/releases/download/$Version/$archive"
    }

    $temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "$BinaryName-install-$([Guid]::NewGuid())"
    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null

    try {
        $archivePath = Join-Path $temporaryDirectory $archive
        Write-Log "Downloading $url"
        Invoke-DownloadFile -Url $url -OutputPath $archivePath -ArchiveName $archive
        Test-Sha256 -FilePath $archivePath -ExpectedHash $env:WOLFRAM_CLI_SHA256

        Expand-Archive -LiteralPath $archivePath -DestinationPath $temporaryDirectory -Force
        $binaryPath = Join-Path (Join-Path $temporaryDirectory $package) $ExecutableName

        if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
            $binary = Get-ChildItem -Path $temporaryDirectory -Recurse -File -Filter $ExecutableName | Select-Object -First 1
            if ($null -eq $binary) {
                Fail "archive did not contain $ExecutableName"
            }
            $binaryPath = $binary.FullName
        }

        Install-Binary -SourcePath $binaryPath -DestinationPath (Join-Path $InstallDir $ExecutableName)
    } finally {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Install-FromSource {
    Require-Command 'cargo'
    Initialize-MsvcBuildEnvironment

    $scriptDirectory = if (-not [string]::IsNullOrWhiteSpace($PSScriptRoot)) { $PSScriptRoot } else { (Get-Location).Path }
    if (-not (Test-Path -LiteralPath (Join-Path $scriptDirectory 'Cargo.toml') -PathType Leaf)) {
        Fail '-BuildFromSource must be run from a source checkout'
    }

    Write-Log "Building $BinaryName from source"
    Push-Location $scriptDirectory
    try {
        & cargo build --release --locked
        if ($LASTEXITCODE -ne 0) {
            Fail "cargo build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }

    Install-Binary -SourcePath (Join-Path $scriptDirectory "target\release\$ExecutableName") -DestinationPath (Join-Path $InstallDir $ExecutableName)
}

if ($Help) {
    Show-Usage
    exit 0
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Get-DefaultInstallDir
}

if (-not [System.IO.Path]::IsPathRooted($InstallDir)) {
    Fail '-InstallDir must be an absolute path'
}

$destination = Join-Path $InstallDir $ExecutableName

if ($BuildFromSource) {
    Install-FromSource
} else {
    Install-Release
}

Write-Log "Installed $BinaryName to $destination"
New-DefaultConfigFile

if (-not (Test-PathContains $InstallDir)) {
    Write-Log "Add $InstallDir to PATH to run $BinaryName without a full path."
}
