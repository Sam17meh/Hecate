$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$distDir = Join-Path $repoRoot 'dist'
if (Test-Path -LiteralPath $distDir) {
    throw "Refusing to overwrite existing output directory: $distDir"
}
$runnerTemp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$buildRoot = Join-Path $runnerTemp "hecate-build-$([guid]::NewGuid().ToString('N'))"
$metadataJson = & cargo metadata --locked --no-deps --format-version 1
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
$metadata = $metadataJson | ConvertFrom-Json
$package = @($metadata.packages | Where-Object { $_.name -eq 'hecate' }) | Select-Object -First 1
if ($null -eq $package) { throw 'Cargo metadata did not contain the hecate package.' }
$version = [string]$package.version
if ($version -ne '0.1.0') { throw "Expected package version 0.1.0, found $version." }
if ($env:GITHUB_REF -and $env:GITHUB_REF -ne "refs/tags/v$version") {
    throw "Workflow tag '$env:GITHUB_REF' does not match package version v$version."
}

$target = 'x86_64-pc-windows-msvc'
$rustVersion = (& rustc --version).Trim()
if ($LASTEXITCODE -ne 0) { throw 'rustc --version failed.' }
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { throw 'vswhere.exe is required to locate dumpbin.exe.' }
$dumpbinCandidates = @(& $vswhere -latest -products '*' -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe')
if ($LASTEXITCODE -ne 0 -or $dumpbinCandidates.Count -eq 0) { throw 'Could not locate the MSVC x64 dumpbin.exe.' }
$dumpbin = $dumpbinCandidates[-1]

New-Item -ItemType Directory -Path $buildRoot | Out-Null
$reportPath = Join-Path $buildRoot 'build-inspection.txt'
@("HECATE Windows release candidate inspection", "Package version: $version", "Target: $target", "Compiler: $rustVersion", 'PE inspection: MSVC dumpbin /dependents', '') | Set-Content -LiteralPath $reportPath -Encoding utf8

$candidates = @{}
try {
foreach ($crt in @('dynamic', 'static')) {
    $targetDir = Join-Path $buildRoot "target-$crt"
    $exe = Join-Path $targetDir "$target\release\hecate.exe"
    $previousRustFlags = $env:RUSTFLAGS
    $previousEncodedRustFlags = $env:CARGO_ENCODED_RUSTFLAGS
    try {
        Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
        if ($crt -eq 'static') {
            $env:RUSTFLAGS = '-C target-feature=+crt-static'
        } else {
            Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
        }
        & cargo build --locked --release --target $target --target-dir $targetDir
        if ($LASTEXITCODE -ne 0) { throw "The $crt CRT release build failed." }
    } finally {
        if ($null -eq $previousRustFlags) {
            Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
        } else {
            $env:RUSTFLAGS = $previousRustFlags
        }
        if ($null -eq $previousEncodedRustFlags) {
            Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
        } else {
            $env:CARGO_ENCODED_RUSTFLAGS = $previousEncodedRustFlags
        }
    }

    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Missing $crt candidate executable: $exe" }
    $versionOutput = (& $exe --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $versionOutput -ne "hecate $version") {
        throw "$crt candidate version check failed: '$versionOutput' (expected 'hecate $version')."
    }
    $null = & $exe --help 2>&1
    if ($LASTEXITCODE -ne 0) { throw "$crt candidate --help failed." }

    $dependencies = (& $dumpbin /dependents $exe 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($dependencies) -or
        $dependencies -notmatch '(?im)^\s*Image has the following dependencies:\s*$') {
        throw "PE dependency inspection failed for $crt candidate."
    }
    $importedDlls = @([regex]::Matches($dependencies, '(?im)^\s+([A-Z0-9_.-]+\.dll)\s*$') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique)
    if ($importedDlls.Count -eq 0) {
        throw "PE dependency inspection found no imported DLL names for the $crt candidate."
    }
    $msvcCrtDllPattern = '(?i)^(?:VCRUNTIME[^\\/]*\.dll|MSVCP[^\\/]*\.dll|api-ms-win-crt-[^\\/]*\.dll)$'
    $msvcCrtDlls = @($importedDlls | Where-Object { $_ -match $msvcCrtDllPattern })
    if ($crt -eq 'dynamic' -and $msvcCrtDlls.Count -eq 0) {
        throw 'Dynamic CRT candidate is missing expected MSVC CRT imports (VCRUNTIME*.dll, MSVCP*.dll, or api-ms-win-crt-*.dll).'
    }
    if ($crt -eq 'static' -and $msvcCrtDlls.Count -gt 0) {
        throw "Static CRT candidate unexpectedly imports MSVC CRT DLL(s): $($msvcCrtDlls -join ', ')"
    }
    $dependencies = $dependencies.Replace($exe, 'hecate.exe')
    $size = (Get-Item -LiteralPath $exe).Length
    $candidates[$crt] = $exe
    @("[$crt CRT]", 'Executable: hecate.exe', "Version: $versionOutput", "Size bytes: $size", 'PE dependencies:', $dependencies, '') |
        Add-Content -LiteralPath $reportPath -Encoding utf8
    Write-Host "$crt CRT: $size bytes; $versionOutput"
}
} catch {
    Remove-Item -LiteralPath $buildRoot -Recurse -Force -ErrorAction SilentlyContinue
    throw
}

New-Item -ItemType Directory -Path $distDir | Out-Null
Copy-Item -LiteralPath $reportPath -Destination $distDir
$outputReportPath = Join-Path $distDir 'build-inspection.txt'

$stageDir = Join-Path $distDir 'package'
New-Item -ItemType Directory -Path $stageDir | Out-Null
$packageFiles = @(
    @{ Source = $candidates['static']; Name = 'hecate.exe' },
    @{ Source = (Join-Path $repoRoot 'README.md'); Name = 'README.md' },
    @{ Source = (Join-Path $repoRoot 'WINDOWS.md'); Name = 'WINDOWS.md' },
    @{ Source = (Join-Path $repoRoot 'LICENSE-MIT'); Name = 'LICENSE-MIT' },
    @{ Source = (Join-Path $repoRoot 'LICENSE-APACHE'); Name = 'LICENSE-APACHE' }
)
foreach ($file in $packageFiles) {
    if (-not (Test-Path -LiteralPath $file.Source -PathType Leaf)) { throw "Missing package input: $($file.Source)" }
    Copy-Item -LiteralPath $file.Source -Destination (Join-Path $stageDir $file.Name)
}

$archiveName = "hecate-v$version-$target.zip"
$archivePath = Join-Path $distDir $archiveName
Compress-Archive -Path (Join-Path $stageDir '*') -DestinationPath $archivePath -CompressionLevel Optimal
$expectedEntries = @('LICENSE-APACHE', 'LICENSE-MIT', 'README.md', 'WINDOWS.md', 'hecate.exe') | Sort-Object
$archive = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
$extractedDir = Join-Path $runnerTemp "hecate-verify-$([guid]::NewGuid().ToString('N'))"
try {
    $actualEntries = @($archive.Entries | ForEach-Object { $_.FullName } | Sort-Object)
    if (Compare-Object -ReferenceObject $expectedEntries -DifferenceObject $actualEntries) {
        throw "Archive contents mismatch. Found: $($actualEntries -join ', ')"
    }
    [System.IO.Compression.ZipFileExtensions]::ExtractToDirectory($archive, $extractedDir)
} finally {
    $archive.Dispose()
}
try {
    $packagedVersion = (& (Join-Path $extractedDir 'hecate.exe') --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $packagedVersion -ne "hecate $version") {
        throw "Packaged executable version check failed: '$packagedVersion'."
    }
} finally {
    Remove-Item -LiteralPath $extractedDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $buildRoot -Recurse -Force -ErrorAction SilentlyContinue
}

$checksum = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
"$checksum  $archiveName" | Set-Content -LiteralPath "$archivePath.sha256" -Encoding ascii
Remove-Item -LiteralPath $stageDir -Recurse -Force
Write-Host "Archive: $archivePath"
Write-Host "SHA-256: $checksum"
Write-Host "Verified archive entries: $($expectedEntries -join ', ')"
Write-Host "Inspection report: $outputReportPath"
