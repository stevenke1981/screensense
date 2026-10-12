param(
    [Parameter(Mandatory=$true)][string]$InitialInstaller,
    [Parameter(Mandatory=$true)][string]$UpgradeInstaller,
    [Parameter(Mandatory=$true)][string]$OutputRoot,
    [Parameter(Mandatory=$true)][ValidatePattern('^ScreenSense\.fixture\.[0-9a-f-]{36}$')][string]$FixtureAppId,
    [Parameter(Mandatory=$true)][ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedCommit,
    [string]$Python = 'python'
)
$ErrorActionPreference = 'Stop'
$repo = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$root = [System.IO.Path]::GetFullPath($OutputRoot)
New-Item -ItemType Directory -Path $root -Force | Out-Null
$fixture = Join-Path $root ('installer-fixture-'+[Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$install = Join-Path $fixture 'app'
$registry = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\'+$FixtureAppId+'_is1'
if (Test-Path -LiteralPath $registry) { throw 'Fixture registration already exists; refusing to touch an existing install' }
$script:ownedProcessPending=$false

function RunOwned([string]$file,[string]$arguments,[bool]$isolatedApp=$false) {
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName=[System.IO.Path]::GetFullPath($file)
    $start.Arguments=$arguments
    $start.WorkingDirectory=$fixture
    $start.UseShellExecute=$false
    $start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true
    $start.RedirectStandardError=$true
    if ($isolatedApp) {
        $start.EnvironmentVariables['SCREENSENSE_NO_SCREEN']='1'
        $start.EnvironmentVariables['SCREENSENSE_DATA']=(Join-Path $fixture 'isolated-data')
        $start.EnvironmentVariables['SCREENSENSE_CONFIG']=(Join-Path $fixture 'config.toml')
    }
    $p=[System.Diagnostics.Process]::Start($start)
    $stdout=$p.StandardOutput.ReadToEndAsync()
    $stderr=$p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit(120000)) { $script:ownedProcessPending=$true; throw "Owned process did not exit; PID $($p.Id). No force termination performed; fixture cleanup deferred." }
    $out=$stdout.GetAwaiter().GetResult()
    $err=$stderr.GetAwaiter().GetResult()
    if ($p.ExitCode -ne 0) { throw "Owned process exited $($p.ExitCode): $err" }
    return $out
}
function FixtureData([string]$mode) {
    $json = & $Python (Join-Path $PSScriptRoot 'installer-fixture-data.py') $fixture $mode
    if ($LASTEXITCODE -ne 0) { throw 'Synthetic installer database check failed' }
    return ($json | ConvertFrom-Json)
}
function Install([string]$installer,[string]$log) {
    $stem=[System.IO.Path]::Combine([System.IO.Path]::GetDirectoryName($installer),[System.IO.Path]::GetFileNameWithoutExtension($installer))
    $info=Get-Content -LiteralPath ($stem+'.build.json') -Raw | ConvertFrom-Json
    if ($info.commit -ne $ExpectedCommit) { throw 'Installer commit mismatch' }
    if ($info.app_identifier -ne $FixtureAppId) { throw 'Refusing production AppId during fixture test' }
    if ((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant() -ne $info.installer_sha256) { throw 'Installer hash mismatch' }
    RunOwned $installer "/VERYSILENT /SP- /NORESTART /NOICONS /TASKS=`"`" /DIR=`"$install`" /LOG=`"$(Join-Path $fixture $log)`"" | Out-Null
    if (-not (Test-Path -LiteralPath $registry)) { throw 'Per-user uninstall registration missing' }
    $registration=Get-ItemProperty -LiteralPath $registry
    if ([System.IO.Path]::GetFullPath($registration.InstallLocation.TrimEnd('\')) -ne [System.IO.Path]::GetFullPath($install)) { throw 'Fixture install location mismatch' }
    if ($registration.DisplayVersion -ne $info.version) { throw 'Uninstall display version mismatch' }
    foreach ($entry in @(@('screensense.exe',$info.cli_sha256),@('screensense-gui.exe',$info.gui_sha256),@('source/source.tar',$info.source_sha256),@('source/dependency-sources.zip',$info.dependency_sources_sha256))) {
        if ((Get-FileHash -LiteralPath (Join-Path $install $entry[0]) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry[1]) { throw "Installed payload hash mismatch: $($entry[0])" }
    }
    if (-not (Test-Path -LiteralPath (Join-Path $install 'licenses/dependencies/index.json') -PathType Leaf)) { throw 'Original dependency license index missing' }
}

$checks=[System.Collections.Generic.List[string]]::new()
try {
    Install ([System.IO.Path]::GetFullPath($InitialInstaller)) 'install.log'
    $checks.Add('Per-user installation and uninstall metadata in new isolated directory')
    $cli=Join-Path $install 'screensense.exe'
    $gui=Join-Path $install 'screensense-gui.exe'
    if ((RunOwned $cli '--version' $true) -notmatch 'screensense 0.1.0') { throw 'Installed CLI version mismatch' }
    $guiOutput=Join-Path $fixture 'gui-check'
    RunOwned $gui "--verify-ui `"$guiOutput`"" $true | Out-Null
    $verification=Get-Content -LiteralPath (Join-Path $guiOutput 'verification.json') -Raw | ConvertFrom-Json
    if ($verification.result -ne 'passed' -or $verification.screen_observation -ne 'disabled') { throw 'Installed GUI synthetic startup verification failed' }
    $checks.Add('Installed matching CLI starts; GUI software verification starts/exits successfully with observation disabled')
    $config=Join-Path $fixture 'config.toml'
    [System.IO.File]::WriteAllText($config,"language='en'`n[capture]`nenabled=false`n[analyzer]`nenabled=false`n# Synthetic retained configuration`n")
    $configHash=(Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash
    $before=FixtureData 'seed'
    $sentinel=Join-Path $install 'user-created-sentinel.txt'
    [System.IO.File]::WriteAllText($sentinel,'Synthetic user-created file; installer must not delete it.')
    Install ([System.IO.Path]::GetFullPath($UpgradeInstaller)) 'upgrade.log'
    if ((Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash -ne $configHash) { throw 'Upgrade changed configuration' }
    $after=FixtureData 'read'
    if ($after.frames -ne 1 -or $after.insights -ne 1 -or ($after.controls | ConvertTo-Json -Compress) -ne ($before.controls | ConvertTo-Json -Compress)) { throw 'Upgrade lost synthetic data or controls' }
    if (-not (Test-Path -LiteralPath $sentinel)) { throw 'Upgrade removed a user-created file' }
    $checks.Add('Fixture version upgrade preserves config, SQLite frame/insight, controls and extra file')
    $uninstaller=Join-Path $install 'unins000.exe'
    RunOwned $uninstaller "/VERYSILENT /NORESTART /LOG=`"$(Join-Path $fixture 'uninstall.log')`"" | Out-Null
    if (Test-Path -LiteralPath $registry) { throw 'Own uninstall registration remained' }
    if ((Test-Path -LiteralPath $cli) -or (Test-Path -LiteralPath $gui)) { throw 'Installed binaries remained' }
    if (-not (Test-Path -LiteralPath $sentinel)) { throw 'Uninstall removed a user-created file' }
    if ((Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash -ne $configHash) { throw 'Uninstall changed configuration' }
    $remaining=FixtureData 'read'
    if ($remaining.frames -ne 1 -or $remaining.insights -ne 1) { throw 'Uninstall deleted retained history' }
    $checks.Add('Uninstall removes only owned binaries/registration and preserves configuration/history/extra file')
    @{ result='passed'; commit=$ExpectedCommit; fixture=$fixture; app_id=$FixtureAppId; unsigned=$true; screen_observation='disabled'; gui_input='internal Slint checks, no OS activation attempted'; upgrade_scope='installer version transition with identical-source synthetic payload; historical schema migrations not tested'; checks=$checks } |
        ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $fixture 'installer-verification.json') -Encoding utf8
    Write-Output "PASS $fixture"
} finally {
    if (-not $script:ownedProcessPending -and (Test-Path -LiteralPath $registry)) {
        $registration=Get-ItemProperty -LiteralPath $registry
        if ([System.IO.Path]::GetFullPath($registration.InstallLocation.TrimEnd('\')) -eq [System.IO.Path]::GetFullPath($install)) {
            $uninstaller=Join-Path $install 'unins000.exe'
            if (Test-Path -LiteralPath $uninstaller) { RunOwned $uninstaller "/VERYSILENT /NORESTART /LOG=`"$(Join-Path $fixture 'cleanup-uninstall.log')`"" | Out-Null }
        }
    }
}
