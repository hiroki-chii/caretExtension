param([switch]$Offline)

$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $workspacePath
try {
    $buildArguments = @('build', '--release', '--locked')
    if ($Offline) { $buildArguments += '--offline' }
    & cargo @buildArguments
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }

    $distributionPath = Join-Path $workspacePath 'dist'
    New-Item -ItemType Directory -Path $distributionPath -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $workspacePath 'target\x86_64-pc-windows-msvc\release\caret-mode.exe') -Destination $distributionPath
    foreach ($documentName in @('README.md', 'VALIDATION.md', 'IMPLEMENTATION_PLAN.md')) {
        Copy-Item -LiteralPath (Join-Path $workspacePath $documentName) -Destination $distributionPath
    }
    $fixturePath = Join-Path $distributionPath 'tests\fixtures'
    New-Item -ItemType Directory -Path $fixturePath -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $workspacePath 'tests\fixtures\input.html') -Destination $fixturePath
    Get-Item -LiteralPath (Join-Path $distributionPath 'caret-mode.exe') | Select-Object FullName,Length
} finally {
    Pop-Location
}
