param(
    [Parameter(Mandatory = $true)][string]$File,
    [ValidateRange(3, 100)][int]$Runs = 3,
    [string]$Output = "target/memory-benchmark.json"
)
$ErrorActionPreference = "Stop"
if (-not ($IsWindows -or $env:OS -eq "Windows_NT")) {
    throw "This measurement script requires Windows process memory counters."
}
$inputFile = (Resolve-Path -LiteralPath $File).Path
cargo build --release --offline --locked --example memory_bench
if ($LASTEXITCODE -ne 0) { throw "Benchmark build failed" }
$binaryName = if ($IsWindows -or $env:OS -eq "Windows_NT") { "memory_bench.exe" } else { "memory_bench" }
$binary = Join-Path "target/release/examples" $binaryName
$samples = @()
for ($iteration = 0; $iteration -lt $Runs; $iteration++) {
    # Alternate order to reduce systematic file-cache/order bias; no cache flush is attempted.
    $modes = if ($iteration % 2 -eq 0) { @("retain", "release") } else { @("release", "retain") }
    foreach ($mode in $modes) {
        Write-Host "Run $($iteration + 1)/$Runs : $mode"
        $result = & $binary $inputFile $mode
        if ($LASTEXITCODE -ne 0) { throw "Benchmark failed: $mode" }
        $sample = $result | ConvertFrom-Json
        if ($null -eq $sample.memory.ready.peak_resident_bytes) { throw "Process memory counters unavailable" }
        $sample | Add-Member -NotePropertyName run -NotePropertyValue ($iteration + 1)
        $samples += $sample
    }
}
function Get-Stats($values) {
    $ordered = @($values | Sort-Object)
    $middle = [int][Math]::Floor($ordered.Count / 2)
    $median = if ($ordered.Count % 2) { $ordered[$middle] } else { ($ordered[$middle - 1] + $ordered[$middle]) / 2 }
    @{ median = $median; min = $ordered[0]; max = $ordered[-1] }
}
$summary = @{}
foreach ($mode in @("retain", "release")) {
    $group = @($samples | Where-Object mode -eq $mode)
    $summary[$mode] = @{
        total_ms = Get-Stats @($group | ForEach-Object total_ms)
        read_ms = Get-Stats @($group | ForEach-Object read_ms)
        parse_ms = Get-Stats @($group | ForEach-Object parse_ms)
        release_ms = Get-Stats @($group | ForEach-Object release_ms)
        count_and_index_ms = Get-Stats @($group | ForEach-Object count_and_index_ms)
        preview_ms = Get-Stats @($group | ForEach-Object preview_ms)
        peak_resident_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.peak_resident_bytes })
        ready_resident_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.resident_bytes })
        peak_commit_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.peak_commit_bytes })
        ready_private_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.private_bytes })
    }
}
$report = @{
    measured_at = [DateTimeOffset]::Now.ToString("o")
    rustc = (& rustc --version | Out-String).Trim()
    processor = $env:PROCESSOR_IDENTIFIER
    logical_processors = $env:NUMBER_OF_PROCESSORS
    git_revision = (& git rev-parse HEAD | Out-String).Trim()
    input_sha256 = (Get-FileHash -LiteralPath $inputFile -Algorithm SHA256).Hash
    cache_policy = "OS cache uncontrolled; fresh child process per sample; alternating mode order"
    scope = "Document engine only; excludes GUI, fonts, tree rows, search, query, export and document switching"
    summary = $summary
    samples = $samples
}
$outputDirectory = Split-Path -Parent $Output
if ($outputDirectory -and -not (Test-Path -LiteralPath $outputDirectory)) {
    New-Item -ItemType Directory -Path $outputDirectory | Out-Null
}
$report | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $Output -Encoding utf8
$summary | ConvertTo-Json -Depth 5 | Write-Host
Write-Host "Saved $Output"
