# Script om versie automatisch te verhogen en AGENTS.md bij te werken
# Gebruik: .\scripts\bump-version.ps1

param(
    [Parameter(Mandatory=$false)]
    [ValidateSet("major", "minor", "patch")]
    [string]$Type = "patch",

    [Parameter(Mandatory=$false)]
    [string]$Description = ""
)

# Colors
function Write-ColorOutput($ForegroundColor) {
    $fc = $host.UI.RawUI.ForegroundColor
    $host.UI.RawUI.ForegroundColor = $ForegroundColor
    if ($args) {
        Write-Output $args
    }
    $host.UI.RawUI.ForegroundColor = $fc
}

# Get current version from Cargo.toml
$cargoToml = Get-Content "Cargo.toml" | Select-String "^version = "
if (-not $cargoToml) {
    Write-ColorOutput Red "Fout: Kan versie niet vinden in Cargo.toml"
    exit 1
}

$currentVersion = $cargoToml.Line -replace 'version = "(.*)"', '$1'
Write-ColorOutput Cyan "Huidige versie: $currentVersion"

# Parse version
$versionParts = $currentVersion.Split('.')
$major = [int]$versionParts[0]
$minor = [int]$versionParts[1]
$patch = [int]$versionParts[2]

# Calculate new version
switch ($Type) {
    "major" {
        $newVersion = "$($major + 1).0.0"
        $changeType = "Major"
    }
    "minor" {
        $newVersion = "$major.$($minor + 1).0"
        $changeType = "Minor"
    }
    "patch" {
        $newVersion = "$major.$minor.$($patch + 1)"
        $changeType = "Patch"
    }
}

Write-ColorOutput Green "Nieuwe versie: $newVersion ($changeType)"

# Update Cargo.toml
Write-ColorOutput Cyan "→ Cargo.toml updaten..."
$cargoContent = Get-Content "Cargo.toml" -Raw
$cargoContent = $cargoContent -replace "version = `"$currentVersion`"", "version = `"$newVersion`""
Set-Content "Cargo.toml" -Value $cargoContent -NoNewline

# Update AGENTS.md
Write-ColorOutput Cyan "→ AGENTS.md updaten..."
$today = Get-Date -Format "yyyy-MM-dd"

if (Test-Path "AGENTS.md") {
    $agentsContent = Get-Content "AGENTS.md" -Raw

    # Find the position after the first "---" marker
    $headerEnd = $agentsContent.IndexOf("---", 10)  # Skip first "---" after title

    if ($headerEnd -gt 0) {
        $newSection = @"

## [$newVersion] - $today

### $changeType
"@

        if ($Description) {
            $newSection += @"

$Description
"@
        }

        $newSection += @"

---
"@

        # Insert new section after header
        $agentsContent = $agentsContent.Insert($headerEnd + 3, $newSection)
        Set-Content "AGENTS.md" -Value $agentsContent -NoNewline
    }
} else {
    # Create new AGENTS.md
    $newContent = @"
# DemonGrep - Agent Changelog

## [$newVersion] - $today

### $changeType

$Description

---
"@
    Set-Content "AGENTS.md" -Value $newContent -NoNewline
}

# Show git status
Write-ColorOutput Cyan "→ Git status:"
git status --short

# Ask if user wants to commit
Write-Host ""
$commit = Read-Host "Wil je deze wijzigingen commiten? (j/n)"

if ($commit -eq "j" -or $commit -eq "J" -or $commit -eq "y" -or $commit -eq "Y") {
    $branch = git branch --show-current
    Write-ColorOutput Green "→ Commiten naar branch: $branch"

    $commitMessage = "chore: Bump version to $newVersion

- $changeType update
- $newVersion"

    if ($Description) {
        $commitMessage += "`n`n$Description"
    }

    git add Cargo.toml AGENTS.md
    git commit -m $commitMessage

    Write-ColorOutput Green "✅ Versie verhoogd naar $newVersion en gecommit!"
    Write-Host ""
    Write-Host "Volgende stappen:"
    Write-Host "  1. Push: git push"
    Write-Host "  2. Of maak een PR: gh pr create"
} else {
    Write-ColorOutput Yellow "⚠️  Wijzigingen niet gecommit"
    Write-Host "  Cargo.toml en AGENTS.md zijn wel aangepast"
}
