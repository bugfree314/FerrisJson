param(
    [Parameter(Mandatory = $true)][string]$File,
    [ValidateRange(3, 100)][int]$Runs = 3,
    [ValidateSet("open", "switch", "tree", "export", "backend")][string]$Scenario = "open",
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
$modePair = switch ($Scenario) {
    "switch" { @("switch-normal", "switch-low") }
    "tree" { @("tree") }
    "export" { @("export-string", "export-stream") }
    "backend" { @("memory-open", "indexed-open") }
    default { @("retain", "release") }
}
for ($iteration = 0; $iteration -lt $Runs; $iteration++) {
    # Alternate order to reduce systematic file-cache/order bias; no cache flush is attempted.
    $modes = @($modePair)
    if ($iteration % 2 -ne 0) { [array]::Reverse($modes) }
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
foreach ($mode in $modePair) {
    $group = @($samples | Where-Object mode -eq $mode)
    $summary[$mode] = @{
        peak_resident_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.peak_resident_bytes })
        ready_resident_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.resident_bytes })
        peak_commit_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.peak_commit_bytes })
        ready_private_bytes = Get-Stats @($group | ForEach-Object { $_.memory.ready.private_bytes })
    }
    foreach ($field in @("total_ms", "load_ms", "read_ms", "parse_ms", "release_ms", "count_and_index_ms", "preview_ms", "previous_load_ms", "previous_release_ms", "new_load_ms", "switch_ms", "rows_ms", "search_all_ms", "rebuild_rows_ms", "export_ms")) {
        $values = @($group | ForEach-Object { $_.$field } | Where-Object { $null -ne $_ })
        if ($values.Count -gt 0) { $summary[$mode][$field] = Get-Stats $values }
    }
}
$report = @{
    measured_at = [DateTimeOffset]::Now.ToString("o")
    rustc = (& rustc --version | Out-String).Trim()
    processor = $env:PROCESSOR_IDENTIFIER
    logical_processors = $env:NUMBER_OF_PROCESSORS
    git_revision = (& git rev-parse HEAD | Out-String).Trim()
    working_tree_modified = [bool](& git status --porcelain --untracked-files=no | Out-String).Trim()
    scenario = $Scenario
    input_sha256 = (Get-FileHash -LiteralPath $inputFile -Algorithm SHA256).Hash
    cache_policy = "OS cache uncontrolled; fresh child process per sample; alternating mode order"
    scope = switch ($Scenario) {
        "switch" { "Document switching engine; same file loaded twice; no active queries or GUI" }
        "tree" { "Engine with all nodes expanded and regex .* matching all nodes; excludes GUI/JSONPath" }
        "export" { "Engine exporting full root; legacy formatted String versus stream with flush/sync/atomic rename; excludes GUI" }
        "backend" { "Memory Value backend versus validated file-snapshot/range backend, including initial root preview; excludes GUI/search/query/export" }
        default { "Document engine only; excludes GUI, fonts, tree rows, search, query, export and document switching" }
    }
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
