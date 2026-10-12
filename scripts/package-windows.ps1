param(
    [Parameter(Mandatory=$true)][string]$BinaryDir,
    [Parameter(Mandatory=$true)][string]$OutputDir,
    [Parameter(Mandatory=$true)][string]$DependencyDir,
    [Parameter(Mandatory=$true)][ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedCommit,
    [string]$InnoCompiler = 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?$')][string]$Version = '0.1.0-preview',
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$')][string]$VersionNumber = '0.1.0.0',
    [ValidatePattern('^ScreenSense\.[A-Za-z0-9.-]{1,80}$')][string]$AppIdentifier = 'ScreenSense.917fbe92-1d22-4f87-af47-e143e7eca3dd'
)
$ErrorActionPreference = 'Stop'
$repo = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$binaries = (Resolve-Path -LiteralPath $BinaryDir).Path
$output = [System.IO.Path]::GetFullPath($OutputDir)
$actualCommit = (& git -C $repo rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $actualCommit -ne $ExpectedCommit) { throw 'Packaging source commit mismatch' }
$dirty = & git -C $repo status --porcelain
if ($LASTEXITCODE -ne 0 -or $dirty) { throw 'Commit the packaging source before building an installer' }
if (-not (Test-Path -LiteralPath $InnoCompiler -PathType Leaf)) { throw "Inno Setup compiler missing: $InnoCompiler. See docs/WINDOWS-INSTALLER.md for official tooling." }
# ISCC's executable resource is the console frontend version, not the engine
# version. Compile a fixed no-output stdin script to ask the actual engine.
$probe = [System.Diagnostics.ProcessStartInfo]::new()
$probe.FileName=$InnoCompiler
$probe.Arguments='/O- -'
$probe.UseShellExecute=$false
$probe.CreateNoWindow=$true
$probe.RedirectStandardInput=$true
$probe.RedirectStandardOutput=$true
$probe.RedirectStandardError=$true
$probeProcess=[System.Diagnostics.Process]::Start($probe)
$probeOut=$probeProcess.StandardOutput.ReadToEndAsync()
$probeErr=$probeProcess.StandardError.ReadToEndAsync()
$probeProcess.StandardInput.WriteLine("[Setup]`nAppName=ScreenSense compiler version probe`nAppVersion=0.0.0`nCreateAppDir=no`nUninstallable=no`nOutput=no`nOutputDir=`nOutputManifestFile=`nPrivilegesRequired=lowest")
$probeProcess.StandardInput.Close()
if (-not $probeProcess.WaitForExit(30000)) { throw "Compiler version probe did not exit; PID $($probeProcess.Id). No force termination performed." }
$probeText=$probeOut.GetAwaiter().GetResult()
$probeError=$probeErr.GetAwaiter().GetResult()
if ($probeProcess.ExitCode -ne 0) { throw "Inno engine probe failed: $probeError" }
$compilerVersion=[regex]::Match($probeText,'Compiler engine version: Inno Setup ([0-9]+\.[0-9]+\.[0-9]+)').Groups[1].Value
if (-not $compilerVersion -or [version]$compilerVersion -lt [version]'6.3.0') { throw "Inno Setup engine 6.3 or newer is required; detected '$compilerVersion'" }
Write-Output "Inno Setup engine: $compilerVersion"

function ReadPe([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    if ($bytes.Length -lt 256 -or $bytes[0] -ne 77 -or $bytes[1] -ne 90) { throw "Invalid PE file: $path" }
    $pe = [BitConverter]::ToInt32($bytes,60)
    if ($pe -lt 0 -or $pe+24 -gt $bytes.Length -or [BitConverter]::ToUInt32($bytes,$pe) -ne 17744) { throw 'Invalid PE header' }
    $machine = [BitConverter]::ToUInt16($bytes,$pe+4)
    if ($machine -ne 34404) { throw "Expected Windows x64 payload: $path" }
    $count = [BitConverter]::ToUInt16($bytes,$pe+6)
    $optSize = [BitConverter]::ToUInt16($bytes,$pe+20)
    $opt = $pe+24
    if ([BitConverter]::ToUInt16($bytes,$opt) -ne 523) { throw 'Expected PE32+ payload' }
    $sections = @()
    for ($i=0; $i -lt $count; $i++) {
        $at=$opt+$optSize+$i*40
        $sections += @{ address=[BitConverter]::ToUInt32($bytes,$at+12); size=[Math]::Max([BitConverter]::ToUInt32($bytes,$at+8),[BitConverter]::ToUInt32($bytes,$at+16)); raw=[BitConverter]::ToUInt32($bytes,$at+20) }
    }
    function RvaOffset([uint32]$rva) {
        foreach ($section in $sections) { if ($rva -ge $section.address -and $rva -lt $section.address+$section.size) { return [int]($section.raw+$rva-$section.address) } }
        throw 'Invalid import RVA'
    }
    $imports = @()
    $rva = [BitConverter]::ToUInt32($bytes,$opt+120)
    if ($rva -ne 0) {
        $at = RvaOffset $rva
        while ([BitConverter]::ToUInt32($bytes,$at) -ne 0 -or [BitConverter]::ToUInt32($bytes,$at+12) -ne 0) {
            $name = RvaOffset ([BitConverter]::ToUInt32($bytes,$at+12))
            $end = $name
            while ($bytes[$end] -ne 0) { $end++ }
            $imports += [System.Text.Encoding]::ASCII.GetString($bytes,$name,$end-$name)
            $at += 20
        }
    }
    if ($imports -match '^(VCRUNTIME|MSVCP|MSVCR)[0-9].*\.dll$') { throw "Payload requires a Visual C++ redistributable. Build with -C target-feature=+crt-static: $path" }
    return @{ machine='x86_64'; imports=$imports }
}

$cli = Join-Path $binaries 'screensense.exe'
$gui = Join-Path $binaries 'screensense-gui.exe'
$cliPe = ReadPe $cli
$guiPe = ReadPe $gui
New-Item -ItemType Directory -Path $output -Force | Out-Null
$payload = Join-Path $output ('payload-'+[Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $payload | Out-Null
Copy-Item -LiteralPath $cli,$gui -Destination $payload
Copy-Item -LiteralPath (Join-Path $repo 'crates/gui/LICENSE') -Destination (Join-Path $payload 'LICENSE-GUI-GPL-3.0.txt')
Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination (Join-Path $payload 'LICENSE-CORE-MIT.txt')
Copy-Item -LiteralPath (Join-Path $repo 'THIRD-PARTY-NOTICES.md'),(Join-Path $repo 'README.md'),(Join-Path $repo 'README.zh-TW.md'),(Join-Path $repo 'docs/GUI.md'),(Join-Path $repo 'packaging/windows/INSTALL-README.txt') -Destination $payload
Copy-Item -LiteralPath (Join-Path $DependencyDir 'LICENSES') -Destination $payload -Recurse
Copy-Item -LiteralPath (Join-Path $DependencyDir 'dependency-sources.zip') -Destination $payload
& git -C $repo archive --format=tar --output="$payload/source.tar" HEAD
if ($LASTEXITCODE -ne 0) { throw 'Corresponding source archive failed' }
$buildInfo = @{ commit=$ExpectedCommit; version=$Version; app_identifier=$AppIdentifier; architecture='windows-x86_64'; profile='release + crt-static'; signed=$false; source="https://github.com/stevenke1981/screensense/tree/$ExpectedCommit"; inno_compiler_version=$compilerVersion; cli_sha256=(Get-FileHash -LiteralPath $cli -Algorithm SHA256).Hash.ToLowerInvariant(); gui_sha256=(Get-FileHash -LiteralPath $gui -Algorithm SHA256).Hash.ToLowerInvariant(); dependency_sources_sha256=(Get-FileHash -LiteralPath (Join-Path $payload 'dependency-sources.zip') -Algorithm SHA256).Hash.ToLowerInvariant(); cli_pe=$cliPe; gui_pe=$guiPe }
$buildInfo.source_sha256=(Get-FileHash -LiteralPath (Join-Path $payload 'source.tar') -Algorithm SHA256).Hash.ToLowerInvariant()
$buildInfo | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $payload 'BUILD-INFO.json') -Encoding utf8
$stem = 'ScreenSense-'+$Version+'-'+$ExpectedCommit.Substring(0,7)+'-windows-x64-setup'
& $InnoCompiler "/DPayloadDir=$payload" "/DOutputDir=$output" "/DAppVersion=$Version" "/DVersionNumber=$VersionNumber" "/DAppIdentifier=$AppIdentifier" "/DInstallerStem=$stem" (Join-Path $repo 'packaging/windows/screensense.iss')
if ($LASTEXITCODE -ne 0) { throw "Inno Setup compiler exited $LASTEXITCODE" }
$installer = Join-Path $output ($stem+'.exe')
if (-not (Test-Path -LiteralPath $installer)) { throw 'Installer output missing' }
$installerHash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
$buildInfo.installer_sha256=$installerHash
$buildInfo.installer_filename=[System.IO.Path]::GetFileName($installer)
$buildInfo | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $output ($stem+'.build.json')) -Encoding utf8
"$installerHash  $([System.IO.Path]::GetFileName($installer))" | Set-Content -LiteralPath (Join-Path $output ($stem+'.sha256')) -Encoding utf8
Write-Output "Installer: $installer"
Write-Output "SHA256: $installerHash"
