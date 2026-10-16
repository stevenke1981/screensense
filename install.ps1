# ScreenSense installer for Windows 10/11 (x64).
#
# Downloads the latest published release for this supported target. If no
# compatible release is published yet, use the source-build instructions in
# README.md.
#
# What it does:
#   1. downloads the latest release archive and SHA256SUMS from GitHub;
#   2. verifies the archive against SHA256SUMS and stops on any mismatch;
#   3. unpacks it to %LOCALAPPDATA%\Programs\screensense;
#   4. adds that directory to your USER PATH if it is not there yet.
#
# What it does NOT do: start the daemon, edit any AI-tool configuration, or
# enable autostart. Those are separate commands you run yourself.
#
# Works on Windows PowerShell 5.1 and PowerShell 7.

$ErrorActionPreference = 'Stop'

$Repo  = 'stevenke1981/screensense'
$Asset = 'screensense-x86_64-pc-windows-msvc.zip'
$Base  = "https://github.com/$Repo/releases/latest/download"
$Dest  = Join-Path $env:LOCALAPPDATA 'Programs\screensense'

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
    throw "No prebuilt binary for $($env:PROCESSOR_ARCHITECTURE). Build from source (see README.md)."
}

# Windows PowerShell 5.1 may still default to TLS 1.0.
[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("screensense-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Tmp | Out-Null

try {
    $Zip  = Join-Path $Tmp $Asset
    $Sums = Join-Path $Tmp 'SHA256SUMS'

    Write-Host "Downloading $Asset ..."
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset" -OutFile $Zip
        Invoke-WebRequest -UseBasicParsing -Uri "$Base/SHA256SUMS" -OutFile $Sums
    } catch {
        throw "Download failed from $Base (has a release been published yet?): $($_.Exception.Message)"
    }

    # Lines look like "<64 hex>  <file name>" (a "*" may precede the name).
    $Want = $null
    foreach ($Line in Get-Content -LiteralPath $Sums) {
        $Parts = $Line.Trim() -split '\s+', 2
        if ($Parts.Count -eq 2 -and $Parts[1].TrimStart('*') -eq $Asset) {
            $Want = $Parts[0].ToLowerInvariant()
            break
        }
    }
    if (-not $Want) {
        throw "SHA256SUMS has no entry for $Asset. Nothing was installed."
    }
    $Got = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Got -ne $Want) {
        throw "Checksum mismatch for $Asset (expected $Want, got $Got). Nothing was installed."
    }
    Write-Host 'Checksum OK.'

    $Unpacked = Join-Path $Tmp 'unpacked'
    Expand-Archive -LiteralPath $Zip -DestinationPath $Unpacked
    if (-not (Test-Path -LiteralPath (Join-Path $Unpacked 'screensense.exe'))) {
        throw 'The archive does not contain screensense.exe.'
    }

    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    try {
        Copy-Item -Path (Join-Path $Unpacked '*') -Destination $Dest -Recurse -Force
    } catch {
        throw "Could not write to $Dest. If ScreenSense is running, stop it first (screensense daemon stop) and run this script again. $($_.Exception.Message)"
    }
    Write-Host "Installed to $Dest"
} finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

# Add to the USER PATH (not the machine PATH) if missing.
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($null -eq $UserPath) { $UserPath = '' }
$OnPath = $false
foreach ($Entry in ($UserPath -split ';')) {
    if ($Entry.Trim().TrimEnd('\') -ieq $Dest.TrimEnd('\')) { $OnPath = $true }
}
if (-not $OnPath) {
    if ($UserPath -eq '') {
        $NewPath = $Dest
    } else {
        $NewPath = $UserPath.TrimEnd(';') + ';' + $Dest
    }
    [Environment]::SetEnvironmentVariable('Path', $NewPath, 'User')
    $env:Path = $env:Path.TrimEnd(';') + ';' + $Dest
    Write-Host "Added $Dest to your user PATH. Open a new terminal for it to take effect elsewhere."
}

Write-Host ''
Write-Host 'Nothing has been started or configured. Next steps:'
Write-Host '  screensense doctor --capture   # check what works on this machine'
Write-Host '  screensense daemon start       # start recording'
Write-Host '  screensense setup              # connect your AI coding tools'
