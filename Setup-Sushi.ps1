[CmdletBinding()]
param(
    [string] $GamePath,
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'Sushi'),
    [string] $ShortcutDirectory = (Join-Path ([Environment]::GetFolderPath('Programs')) 'Sushi')
)

$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT' -or ![Environment]::Is64BitOperatingSystem) {
    throw 'Sushi requires 64-bit Windows.'
}
if (!(Get-Command rustup -ErrorAction SilentlyContinue) -or !(Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Install Rust for Windows from https://rustup.rs, then reopen Setup.cmd.'
}

$InstallDir = [IO.Path]::GetFullPath($InstallDir)
# Refuse a fresh default installation when the prior custom folder still
# exists: preserve its cache, saves and saved game path during migration.
$previous = Join-Path $env:LOCALAPPDATA ('VIctory ' + 'Pascal')
$newDefault = Join-Path $env:LOCALAPPDATA 'Sushi'
if ($InstallDir -ieq $newDefault -and !(Test-Path -LiteralPath $InstallDir) -and (Test-Path -LiteralPath $previous -PathType Container)) {
    throw "Close the runtime and move the previous installation directory to $newDefault before running setup again, to preserve saves and shaders. Previous directory: $previous"
}
for ($parent = [IO.DirectoryInfo]::new($InstallDir); $parent; $parent = $parent.Parent) {
    if (Test-Path -LiteralPath (Join-Path $parent.FullName '.victory-root')) {
        throw 'Choose an install directory outside the source checkout so the launcher can write saves and shader caches.'
    }
}
$config = Join-Path $InstallDir 'game-path.txt'
if (!$GamePath -and (Test-Path -LiteralPath $config -PathType Leaf)) {
    $saved = [IO.File]::ReadAllText($config).Trim()
    if ($saved -and (Test-Path -LiteralPath $saved -PathType Leaf)) { $GamePath = $saved }
}
if (!$GamePath) {
    Add-Type -AssemblyName System.Windows.Forms
    $picker = [Windows.Forms.OpenFileDialog]::new()
    try {
        $picker.Title = 'Select the Series X game executable from your extracted game installation'
        $picker.Filter = 'Game executable (*.exe)|*.exe'
        if ($picker.ShowDialog() -ne [Windows.Forms.DialogResult]::OK) {
            Write-Host 'Setup cancelled.'
            return
        }
        $GamePath = $picker.FileName
    } finally { $picker.Dispose() }
}
if (!(Test-Path -LiteralPath $GamePath -PathType Leaf)) {
    throw "Game executable not found: $GamePath. Select the correct .exe in Setup.cmd."
}
$game = Get-Item -LiteralPath $GamePath
if ($game.PSIsContainer -or $game.Extension -ine '.exe' -or $game.Length -lt 2) {
    throw "Select a nonempty game executable in its original installation directory: $GamePath"
}
$stream = [IO.File]::OpenRead($game.FullName)
try {
    if ($stream.ReadByte() -ne 0x4d -or $stream.ReadByte() -ne 0x5a) {
        throw "The selected file is not a readable PE executable: $GamePath"
    }
} finally { $stream.Dispose() }
if ($game.Name -ieq 'game_scarlett_master.exe') {
    $missing = @(
        foreach ($name in 'MicrosoftGame.Config','common.rpf','data.rpf','levels.rpf','shaders.rpf') {
            $asset = Get-Item -LiteralPath (Join-Path $game.DirectoryName $name) -ErrorAction SilentlyContinue
            if (!$asset -or $asset.PSIsContainer -or $asset.Length -eq 0) { $name }
        }
    )
    if ($missing.Count -gt 0) {
        throw "Game installation is incomplete in $($game.DirectoryName): $($missing -join ', '). Select the complete installation."
    }
}
$gameRoot = $game.DirectoryName.TrimEnd([char]92, [char]47) + [IO.Path]::DirectorySeparatorChar
$installRoot = $InstallDir.TrimEnd([char]92, [char]47) + [IO.Path]::DirectorySeparatorChar
if ($gameRoot.StartsWith($installRoot, [StringComparison]::OrdinalIgnoreCase) -or $installRoot.StartsWith($gameRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The game and runtime installation must be separate directories.'
}
Write-Host "Game executable: $($game.FullName)"
Write-Host "Install directory: $InstallDir"
Write-Host 'Game path checks passed. Building the runtime...'

Push-Location -LiteralPath $PSScriptRoot
try {
    & (Join-Path $PSScriptRoot 'Initialize-FormatTables.ps1')
    $targetDir = Join-Path $PSScriptRoot 'target'
    $target = 'x86_64-pc-windows-msvc'
    & cargo build --locked --release -p victory --features diagnostics --bins --target $target --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) {
        throw 'Build failed. Check the compiler output and the C++ workload and Windows SDK listed in the README.'
    }
    $release = Join-Path $targetDir "$target/release"
    & (Join-Path $release 'victory.exe') inspect $game.FullName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'The runtime could not read the selected game executable.' }
} finally { Pop-Location }

# Hold the same lock as Play and Preload during an install or update.
$state = [IO.Directory]::CreateDirectory((Join-Path $InstallDir 'state'))
try {
    $lock = [IO.File]::Open((Join-Path $state.FullName 'launcher.lock'), 'OpenOrCreate', 'Write', 'None')
} catch [IO.IOException] {
    throw "Could not lock the install directory. Close Sushi and shader preload before updating. $($_.Exception.Message)"
}
try {
    foreach ($name in 'victory.exe','victory-play.exe','victory-warm.exe') {
        $destination = Join-Path $InstallDir $name
        $temp = Join-Path $InstallDir ([IO.Path]::GetRandomFileName())
        try {
            [IO.File]::Copy((Join-Path $release $name), $temp)
            if ([IO.File]::Exists($destination)) {
                [IO.File]::Replace($temp, $destination, [System.Management.Automation.Language.NullString]::Value)
            } else {
                [IO.File]::Move($temp, $destination)
            }
        } finally { if ([IO.File]::Exists($temp)) { [IO.File]::Delete($temp) } }
    }
    [IO.File]::WriteAllText($config, $game.FullName, [Text.UTF8Encoding]::new($false))
} finally { $lock.Dispose() }

$launcher = Join-Path $InstallDir 'victory-play.exe'
Write-Host "Installed Sushi in $InstallDir"
[IO.Directory]::CreateDirectory($ShortcutDirectory) | Out-Null
$shell = New-Object -ComObject WScript.Shell
foreach ($entry in @(
    @{ Name = 'Sushi'; Arguments = ''; Target = $launcher }
    @{ Name = 'Sushi (Compile shaders and play)'; Arguments = ''; Target = (Join-Path $InstallDir 'victory-warm.exe') }
    @{ Name = 'Sushi (Experimental RT)'; Arguments = '--rt'; Target = $launcher }
)) {
    $shortcut = $shell.CreateShortcut((Join-Path $ShortcutDirectory ($entry.Name + '.lnk')))
    $shortcut.TargetPath = $entry.Target
    $shortcut.Arguments = $entry.Arguments
    $shortcut.WorkingDirectory = $InstallDir
    $shortcut.Save()
}
Write-Host "Open Sushi from $ShortcutDirectory to play, or select Sushi (Compile shaders and play) to prepare the cache and start the game automatically."
