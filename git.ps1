param(
    [Parameter(Mandatory, Position = 0)]
    [string]$Message,
    [Parameter(Position = 1)]
    [string]$Branch = "main"
)

$ErrorActionPreference = "Stop"

git add .
if ($LASTEXITCODE -ne 0) { throw "git add failed" }

git commit -m $Message
if ($LASTEXITCODE -ne 0) { throw "git commit failed" }

git push origin $Branch
if ($LASTEXITCODE -ne 0) { throw "git push failed" }
