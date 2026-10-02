$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$revision = (Get-Content -LiteralPath (Join-Path $projectRoot 'slint-revision.txt') -Raw).Trim()
$dependencyPath = Join-Path $projectRoot 'third_party/slint'
if ($revision -notmatch '^[0-9a-f]{40}$') { throw 'Invalid Slint revision' }
if (-not (Test-Path -LiteralPath $dependencyPath)) {
    git clone --no-checkout https://github.com/slint-ui/slint.git $dependencyPath
    if ($LASTEXITCODE -ne 0) { throw 'Could not clone Slint' }
    git -C $dependencyPath checkout --detach $revision
    if ($LASTEXITCODE -ne 0) { throw 'Could not check out pinned Slint revision' }
}
$actual = git -c "safe.directory=$dependencyPath" -C $dependencyPath rev-parse HEAD
if ($LASTEXITCODE -ne 0 -or $actual.Trim() -ne $revision) {
    throw "Slint checkout must be at $revision. Existing checkout was left unchanged."
}
