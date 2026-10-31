$ErrorActionPreference = 'Stop'

function Write-Step {
    param([string]$Message)
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Get-VsWherePath {
    $programFilesX86 = [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
    $programFiles = $env:ProgramFiles

    $candidates = @()
    if ($programFilesX86) {
        $candidates += Join-Path $programFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'
    }
    if ($programFiles) {
        $candidates += Join-Path $programFiles 'Microsoft Visual Studio\Installer\vswhere.exe'
    }

    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path $candidate)) {
            return $candidate
        }
    }

    return $null
}

function Get-MsvcInstallPath {
    $vswhere = Get-VsWherePath
    if (-not $vswhere) {
        return $null
    }

    $path = & $vswhere -latest -products '*' `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath 2>$null | Select-Object -First 1

    if ($LASTEXITCODE -ne 0 -or -not $path) {
        return $null
    }

    return $path.Trim()
}

function Import-MsvcEnvironment {
    param([string]$InstallPath)

    $devCmd = Join-Path $InstallPath 'Common7\Tools\VsDevCmd.bat'
    if (-not (Test-Path $devCmd)) {
        return
    }

    cmd /c "`"$devCmd`" -arch=x64 -host_arch=x64 >nul && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            Set-Item -Path ("Env:" + $matches[1]) -Value $matches[2]
        }
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "Rust is not installed." -ForegroundColor Red
    Write-Host "Install it from https://rustup.rs/ and run this script again."
    exit 1
}

$msvcPath = Get-MsvcInstallPath

if (-not $msvcPath) {
    Write-Step "Microsoft C++ Build Tools not found; installing them"

    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        Write-Host "winget is not available on this system." -ForegroundColor Red
        Write-Host "Install Visual Studio Build Tools with the 'Desktop development with C++' workload:"
        Write-Host "https://visualstudio.microsoft.com/visual-cpp-build-tools/"
        Write-Host "Then run: cargo install --path . --locked"
        exit 1
    }

    Write-Host "This downloads several GB and may take a while."
    & winget install --id Microsoft.VisualStudio.2022.BuildTools -e `
        --accept-source-agreements --accept-package-agreements `
        --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"

    if ($LASTEXITCODE -ne 0) {
        Write-Host "Build Tools installation failed." -ForegroundColor Red
        Write-Host "Install them manually from https://visualstudio.microsoft.com/visual-cpp-build-tools/"
        exit 1
    }

    $msvcPath = Get-MsvcInstallPath
    if (-not $msvcPath) {
        Write-Host "Build Tools were installed but MSVC could not be located." -ForegroundColor Red
        Write-Host "Open a new terminal and run: cargo install --path . --locked"
        exit 1
    }
}

Write-Step "Configuring the MSVC build environment"
Import-MsvcEnvironment -InstallPath $msvcPath

Write-Step "Installing netpulse"
& cargo install --path $PSScriptRoot --locked --force
if ($LASTEXITCODE -ne 0) {
    Write-Host "cargo install failed." -ForegroundColor Red
    exit $LASTEXITCODE
}

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) {
    $userPath = ''
}

if ($userPath -notlike "*$cargoBin*") {
    $newPath = if ($userPath.TrimEnd(';')) { "$($userPath.TrimEnd(';'));$cargoBin" } else { $cargoBin }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "Added $cargoBin to your user PATH."
}

$binary = Join-Path $cargoBin 'netpulse.exe'
if (Test-Path $binary) {
    Write-Step "Done"
    Write-Host "Open a new terminal and run: netpulse"
} else {
    Write-Host "netpulse.exe was not found in $cargoBin." -ForegroundColor Red
    exit 1
}
