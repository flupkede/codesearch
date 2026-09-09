# Installs the codesearch <-> Claude Code enforcement hooks.
#
# What this does:
#   1. Copies hooks/*.ps1 into ~/.claude/hooks/codesearch/
#   2. Merges the guard registrations into ~/.claude/settings.json:
#      PreToolUse: Grep -> grep-guard, Agent -> subagent-preamble,
#      WebSearch|WebFetch -> web-guard, Edit|Write|MultiEdit -> edit-guard;
#      PostToolUse: find_impact|find -> edit-guard-post
#   3. Backs up settings.json before touching it
#
# Safe to re-run: registrations are matched by command string and skipped if
# already present (no duplicates). Does not touch any other hook, matcher, or
# setting already in your settings.json.
#
# Usage:
#   pwsh -File integrations/claude-code/install.ps1
#   pwsh -File integrations/claude-code/install.ps1 -Scope project   # writes to .claude/settings.json in cwd instead

param(
    [ValidateSet('user', 'project')]
    [string]$Scope = 'user'
)

$ErrorActionPreference = 'Stop'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$hooksSrc  = Join-Path $scriptDir 'hooks'

if ($Scope -eq 'user') {
    $claudeDir = Join-Path $HOME '.claude'
} else {
    $claudeDir = Join-Path (Get-Location) '.claude'
}

$hooksDest    = Join-Path $claudeDir 'hooks\codesearch'
$settingsPath = Join-Path $claudeDir 'settings.json'

New-Item -ItemType Directory -Force -Path $hooksDest | Out-Null

# NOTE: `codesearch hooks claude install` is the preferred, self-contained way
# to install these hooks (no source tree needed). This script remains as the
# from-source equivalent and must stay in sync with src/cli/claude_hooks.rs.
Copy-Item -Path (Join-Path $hooksSrc 'grep-guard.ps1')        -Destination $hooksDest -Force
Copy-Item -Path (Join-Path $hooksSrc 'subagent-preamble.ps1') -Destination $hooksDest -Force
Copy-Item -Path (Join-Path $hooksSrc 'web-guard.ps1')         -Destination $hooksDest -Force
Copy-Item -Path (Join-Path $hooksSrc 'edit-guard.ps1')        -Destination $hooksDest -Force
Copy-Item -Path (Join-Path $hooksSrc 'edit-guard-post.ps1')   -Destination $hooksDest -Force
Copy-Item -Path (Join-Path $hooksSrc 'codesearch-common.ps1') -Destination $hooksDest -Force

$grepGuardCmd     = "pwsh -NoProfile -NonInteractive -File `"$($hooksDest -replace '\\','/')/grep-guard.ps1`""
$preambleCmd      = "pwsh -NoProfile -NonInteractive -File `"$($hooksDest -replace '\\','/')/subagent-preamble.ps1`""
$webGuardCmd      = "pwsh -NoProfile -NonInteractive -File `"$($hooksDest -replace '\\','/')/web-guard.ps1`""
$editGuardCmd     = "pwsh -NoProfile -NonInteractive -File `"$($hooksDest -replace '\\','/')/edit-guard.ps1`""
$editGuardPostCmd = "pwsh -NoProfile -NonInteractive -File `"$($hooksDest -replace '\\','/')/edit-guard-post.ps1`""

# Load or initialize settings.json
if (Test-Path $settingsPath) {
    $backup = "$settingsPath.bak-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
    Copy-Item $settingsPath $backup
    Write-Host "Backed up existing settings to $backup"
    $settings = Get-Content $settingsPath -Raw | ConvertFrom-Json -AsHashtable
} else {
    New-Item -ItemType Directory -Force -Path $claudeDir | Out-Null
    $settings = @{}
}

if (-not $settings.ContainsKey('hooks'))                { $settings['hooks'] = @{} }
if (-not $settings['hooks'].ContainsKey('PreToolUse'))  { $settings['hooks']['PreToolUse'] = @() }
if (-not $settings['hooks'].ContainsKey('PostToolUse')) { $settings['hooks']['PostToolUse'] = @() }

$preToolUse  = [System.Collections.ArrayList]$settings['hooks']['PreToolUse']
$postToolUse = [System.Collections.ArrayList]$settings['hooks']['PostToolUse']

function Add-MatcherHook {
    param([string]$HookEvent, $HookList, [string]$Matcher, [string]$Command)
    # Skip if a hook with this exact command already exists anywhere in the list
    foreach ($entry in $HookList) {
        foreach ($h in $entry.hooks) {
            if ($h.command -eq $Command) {
                Write-Host "Already registered: $HookEvent $Matcher (skipping)"
                return
            }
        }
    }
    [void]$HookList.Add(@{
        matcher = $Matcher
        hooks   = @(@{ type = 'command'; command = $Command })
    })
    Write-Host "Registered $HookEvent $Matcher hook -> $Command"
}

Add-MatcherHook -HookEvent 'PreToolUse'  -HookList $preToolUse  -Matcher 'Grep'                 -Command $grepGuardCmd
Add-MatcherHook -HookEvent 'PreToolUse'  -HookList $preToolUse  -Matcher 'Agent'                -Command $preambleCmd
Add-MatcherHook -HookEvent 'PreToolUse'  -HookList $preToolUse  -Matcher 'WebSearch|WebFetch'   -Command $webGuardCmd
Add-MatcherHook -HookEvent 'PreToolUse'  -HookList $preToolUse  -Matcher 'Edit|Write|MultiEdit' -Command $editGuardCmd
Add-MatcherHook -HookEvent 'PostToolUse' -HookList $postToolUse -Matcher 'mcp__codesearch__find_impact|mcp__codesearch__find' -Command $editGuardPostCmd

$settings['hooks']['PreToolUse']  = @($preToolUse)
$settings['hooks']['PostToolUse'] = @($postToolUse)

$settings | ConvertTo-Json -Depth 20 | Set-Content $settingsPath -Encoding utf8

Write-Host ""
Write-Host "Done. Hooks installed to: $hooksDest"
Write-Host "Settings updated: $settingsPath"
Write-Host ""
Write-Host "Restart Claude Code (or start a new session) for the hooks to take effect."
