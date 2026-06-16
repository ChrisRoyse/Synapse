param(
    [string]$SynapseNativeHostExe = "$env:USERPROFILE\.cargo\bin\synapse-chrome-native-host.exe",
    [string]$ExtensionId = "leoocgnkjnplbfdbklajepahofecgfbk",
    [switch]$ApplyExternalChromeDebuggerPolicy = $false,
    [ValidateSet('Auto', 'HKCU', 'HKLM')]
    [string]$ChromePolicyHive = 'Auto',
    [ValidateSet('AllExtensions', 'DetectedExtensions')]
    [string]$ChromePolicyBlockScope = 'AllExtensions',
    [bool]$AutoElevateChromePolicy = $false,
    [switch]$AllowExternalChromeDebuggerOrNativeMessaging,
    [switch]$ChromePolicyOnly,
    [string[]]$ChromePolicyExternalExtensionIds = @(),
    [string]$ChromePolicyEvidencePath
)

$ErrorActionPreference = 'Stop'
Import-Module Microsoft.PowerShell.Security -ErrorAction SilentlyContinue

function ConvertTo-CompressedJson {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Value,
        [int]$Depth = 12
    )
    ConvertTo-Json -InputObject $Value -Depth $Depth -Compress
}

function Get-RegistryAclDiagnostic {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    $cursor = $Path
    while (-not [string]::IsNullOrWhiteSpace($cursor) -and -not (Test-Path -LiteralPath $cursor)) {
        if ($cursor -notmatch '^(.*)\\[^\\]+$') {
            break
        }
        $cursor = $Matches[1]
    }
    if ([string]::IsNullOrWhiteSpace($cursor)) {
        $cursor = $Path
    }
    try {
        $acl = Get-Acl -LiteralPath $cursor -ErrorAction Stop
        $access = @($acl.Access | ForEach-Object {
            [pscustomobject]@{
                identity = [string]$_.IdentityReference
                rights = [string]$_.RegistryRights
                type = [string]$_.AccessControlType
                inherited = [bool]$_.IsInherited
            }
        })
        return ConvertTo-CompressedJson -Value ([ordered]@{
            requested_path = $Path
            inspected_path = $cursor
            owner = [string]$acl.Owner
            access = $access
        }) -Depth 8
    } catch {
        return "requested_path=$Path inspected_path=$cursor acl_error=$($_.Exception.Message)"
    }
}

function Get-ChromePolicyRoot {
    param(
        [ValidateSet('HKCU', 'HKLM')]
        [string]$Hive
    )
    return "${Hive}:\Software\Policies\Google\Chrome"
}

function Get-ChromePolicyHiveCandidates {
    param(
        [ValidateSet('Auto', 'HKCU', 'HKLM')]
        [string]$Hive
    )

    if ($Hive -eq 'Auto') {
        return @('HKCU', 'HKLM')
    }
    return @($Hive)
}

function Quote-ProcessArgument {
    param([Parameter(Mandatory = $true)][string]$Value)
    if ($Value.Length -eq 0) {
        return '""'
    }
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    return '"' + ($Value -replace '"', '\"') + '"'
}

function Quote-PowerShellLiteral {
    param([Parameter(Mandatory = $true)][string]$Value)
    return "'" + ($Value -replace "'", "''") + "'"
}

function Read-ChromeExtensionSettingsPolicy {
    param(
        [ValidateSet('HKCU', 'HKLM')]
        [string]$Hive
    )

    $policyRoot = Get-ChromePolicyRoot -Hive $Hive
    if (-not (Test-Path -LiteralPath $policyRoot)) {
        return [ordered]@{}
    }
    $raw = (Get-ItemProperty -LiteralPath $policyRoot -Name ExtensionSettings -ErrorAction SilentlyContinue).ExtensionSettings
    if ([string]::IsNullOrWhiteSpace([string]$raw)) {
        return [ordered]@{}
    }
    try {
        $parsed = $raw | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw "SYNAPSE_CHROME_EXTENSION_SETTINGS_POLICY_INVALID hive=$Hive path=$policyRoot value_name=ExtensionSettings parse_error=$($_.Exception.Message) remediation=fix the existing Chrome ExtensionSettings policy JSON before Synapse can merge debugger/nativeMessaging blockers without overwriting policy state"
    }

    $settings = [ordered]@{}
    foreach ($property in $parsed.PSObject.Properties) {
        $entry = [ordered]@{}
        foreach ($entryProperty in $property.Value.PSObject.Properties) {
            if ($entryProperty.Value -is [System.Array]) {
                $entry[$entryProperty.Name] = @($entryProperty.Value)
            } else {
                $entry[$entryProperty.Name] = $entryProperty.Value
            }
        }
        $settings[$property.Name] = $entry
    }
    return $settings
}

function Write-ChromeExternalDebuggerPolicy {
    param(
        [ValidateSet('HKCU', 'HKLM')]
        [string]$Hive,
        [string[]]$ExternalExtensionIds,
        [ValidateSet('AllExtensions', 'DetectedExtensions')]
        [string]$BlockScope = 'AllExtensions'
    )

    $ids = @($ExternalExtensionIds | Where-Object { $_ -match '^[a-p]{32}$' } | Sort-Object -Unique)
    $policyEntries = @()
    if ($BlockScope -eq 'AllExtensions') {
        $policyEntries += '*'
    }
    $policyEntries += $ids
    $policyEntries = @($policyEntries | Sort-Object -Unique)

    if ($policyEntries.Count -eq 0) {
        return [pscustomobject]@{
            hive = $Hive
            path = Get-ChromePolicyRoot -Hive $Hive
            value_name = 'ExtensionSettings'
            blocked_extension_ids = @()
            block_scope = $BlockScope
            policy_entries = @()
            readback_ok = $true
            policy_json = ConvertTo-CompressedJson -Value ([ordered]@{})
        }
    }

    $policyRoot = Get-ChromePolicyRoot -Hive $Hive
    try {
        New-Item -ItemType Directory -Force -Path $policyRoot | Out-Null
        $settings = Read-ChromeExtensionSettingsPolicy -Hive $Hive
        foreach ($policyEntry in $policyEntries) {
            if (-not $settings.Contains($policyEntry)) {
                $settings[$policyEntry] = [ordered]@{}
            }
            $entry = $settings[$policyEntry]
            $blocked = @()
            if ($entry.Contains('blocked_permissions')) {
                $blocked = @($entry['blocked_permissions'])
            }
            $blocked = @($blocked + @('debugger', 'nativeMessaging') | Sort-Object -Unique)
            $entry['blocked_permissions'] = $blocked
            if (-not $entry.Contains('blocked_install_message')) {
                $entry['blocked_install_message'] = 'Synapse blocked this extension on this host because debugger/nativeMessaging permissions can surface Chrome debugger or native-host popups during background automation.'
            }
        }
        $json = ConvertTo-CompressedJson -Value $settings
        New-ItemProperty -LiteralPath $policyRoot -Name ExtensionSettings -PropertyType String -Value $json -Force | Out-Null
        $readback = Read-ChromeExtensionSettingsPolicy -Hive $Hive
        $missing = @()
        foreach ($policyEntry in $policyEntries) {
            if (-not $readback.Contains($policyEntry)) {
                $missing += "${policyEntry}:missing_entry"
                continue
            }
            $blocked = @($readback[$policyEntry]['blocked_permissions'])
            foreach ($permission in @('debugger', 'nativeMessaging')) {
                if ($blocked -notcontains $permission) {
                    $missing += "${policyEntry}:missing_$permission"
                }
            }
        }
        if ($missing.Count -gt 0) {
            throw "SYNAPSE_CHROME_POLICY_READBACK_MISMATCH hive=$Hive path=$policyRoot missing=$($missing -join ',') remediation=Chrome ExtensionSettings policy write did not persist the required blocked_permissions"
        }
        return [pscustomobject]@{
            hive = $Hive
            path = $policyRoot
            value_name = 'ExtensionSettings'
            blocked_extension_ids = $ids
            block_scope = $BlockScope
            policy_entries = $policyEntries
            readback_ok = $true
            policy_json = ConvertTo-CompressedJson -Value $readback
        }
    } catch {
        $aclDiagnostic = Get-RegistryAclDiagnostic -Path $policyRoot
        throw "SYNAPSE_CHROME_POLICY_REMEDIATION_WRITE_FAILED hive=$Hive path=$policyRoot block_scope=$BlockScope policy_entries=$($policyEntries -join ',') blocked_extension_ids=$($ids -join ',') detail=$($_.Exception.Message) acl_detail=$aclDiagnostic remediation=run setup from a principal that can write Chrome policy or disable/remove the named external Chrome extension, then refresh/restart Chrome and rerun this verifier"
    }
}

function Write-ChromeExternalDebuggerPolicyAuto {
    param(
        [ValidateSet('Auto', 'HKCU', 'HKLM')]
        [string]$Hive,
        [string[]]$ExternalExtensionIds,
        [ValidateSet('AllExtensions', 'DetectedExtensions')]
        [string]$BlockScope = 'AllExtensions',
        [bool]$AutoElevate = $true
    )

    $attempts = @()
    foreach ($candidateHive in (Get-ChromePolicyHiveCandidates -Hive $Hive)) {
        try {
            $result = Write-ChromeExternalDebuggerPolicy `
                -Hive $candidateHive `
                -ExternalExtensionIds $ExternalExtensionIds `
                -BlockScope $BlockScope
            $attempts += [pscustomobject]@{
                hive = $candidateHive
                ok = $true
                path = $result.path
                policy_entries = $result.policy_entries
            }
            $result | Add-Member -NotePropertyName requested_hive -NotePropertyValue $Hive -Force
            $result | Add-Member -NotePropertyName attempted_hives -NotePropertyValue $attempts -Force
            return $result
        } catch {
            $attempts += [pscustomobject]@{
                hive = $candidateHive
                ok = $false
                path = Get-ChromePolicyRoot -Hive $candidateHive
                error = $_.Exception.Message
            }
        }
    }

    if ($AutoElevate -and ($Hive -eq 'Auto' -or $Hive -eq 'HKLM')) {
        try {
            $result = Invoke-ElevatedChromeExternalDebuggerPolicy `
                -ExternalExtensionIds $ExternalExtensionIds `
                -BlockScope $BlockScope
            $attempts += [pscustomobject]@{
                hive = 'HKLM'
                ok = $true
                elevated = $true
                path = $result.path
                policy_entries = $result.policy_entries
                evidence_path = $result.elevation_evidence_path
            }
            $result | Add-Member -NotePropertyName requested_hive -NotePropertyValue $Hive -Force
            $result | Add-Member -NotePropertyName attempted_hives -NotePropertyValue $attempts -Force
            return $result
        } catch {
            $attempts += [pscustomobject]@{
                hive = 'HKLM'
                ok = $false
                elevated = $true
                path = Get-ChromePolicyRoot -Hive 'HKLM'
                error = $_.Exception.Message
            }
        }
    }

    $attemptDetail = ConvertTo-CompressedJson -Value ([object[]]@($attempts)) -Depth 10
    throw "SYNAPSE_CHROME_POLICY_REMEDIATION_WRITE_FAILED_ALL_HIVES requested_hive=$Hive block_scope=$BlockScope auto_elevate=$AutoElevate attempts=$attemptDetail remediation=setup could not persist Chrome ExtensionSettings blocked_permissions=[debugger,nativeMessaging] in any allowed hive; approve the one-time elevated HKLM policy writer, rerun setup elevated, repair HKCU Software\Policies ACL so the current user can write, or disable/remove the named external Chrome extension/native host before certifying popup-free state"
}

function Test-ChromePolicyReadbackBlocksPopupSurfaces {
    param(
        [object]$PolicyReadback,
        [string[]]$ExternalExtensionIds
    )

    if ($null -eq $PolicyReadback -or -not $PolicyReadback.readback_ok) {
        return $false
    }
    $policyJson = [string]$PolicyReadback.policy_json
    if ([string]::IsNullOrWhiteSpace($policyJson)) {
        return $false
    }
    try {
        $settings = $policyJson | ConvertFrom-Json -ErrorAction Stop
    } catch {
        return $false
    }

    function Test-PolicyEntryBlocksPopupPermissions {
        param(
            [Parameter(Mandatory = $true)]
            [object]$Settings,
            [Parameter(Mandatory = $true)]
            [string]$EntryName
        )

        $property = @($Settings.PSObject.Properties | Where-Object { $_.Name -eq $EntryName } | Select-Object -First 1)
        if ($property.Count -eq 0) {
            return $false
        }
        $blocked = @($property[0].Value.blocked_permissions)
        return ($blocked -contains 'debugger' -and $blocked -contains 'nativeMessaging')
    }

    if (Test-PolicyEntryBlocksPopupPermissions -Settings $settings -EntryName '*') {
        return $true
    }

    $ids = @($ExternalExtensionIds | Where-Object { $_ -match '^[a-p]{32}$' } | Sort-Object -Unique)
    if ($ids.Count -eq 0) {
        return $false
    }
    foreach ($id in $ids) {
        if (-not (Test-PolicyEntryBlocksPopupPermissions -Settings $settings -EntryName $id)) {
            return $false
        }
    }
    return $true
}

function Read-ExistingChromeExternalDebuggerPolicy {
    param(
        [ValidateSet('Auto', 'HKCU', 'HKLM')]
        [string]$Hive,
        [string[]]$ExternalExtensionIds,
        [ValidateSet('AllExtensions', 'DetectedExtensions')]
        [string]$BlockScope = 'AllExtensions'
    )

    $ids = @($ExternalExtensionIds | Where-Object { $_ -match '^[a-p]{32}$' } | Sort-Object -Unique)
    $attempts = @()
    foreach ($candidateHive in (Get-ChromePolicyHiveCandidates -Hive $Hive)) {
        try {
            $settings = Read-ChromeExtensionSettingsPolicy -Hive $candidateHive
            $readback = [pscustomobject]@{
                hive = $candidateHive
                path = Get-ChromePolicyRoot -Hive $candidateHive
                value_name = 'ExtensionSettings'
                blocked_extension_ids = $ids
                block_scope = $BlockScope
                policy_entries = @()
                readback_ok = $true
                policy_json = ConvertTo-CompressedJson -Value $settings
                existing_policy = $true
                requested_hive = $Hive
                attempted_hives = $null
            }
            $blocks = Test-ChromePolicyReadbackBlocksPopupSurfaces `
                -PolicyReadback $readback `
                -ExternalExtensionIds $ids
            $attempts += [pscustomobject]@{
                hive = $candidateHive
                ok = $blocks
                existing_policy = $true
                path = $readback.path
            }
            if ($blocks) {
                $entries = @()
                $parsed = $readback.policy_json | ConvertFrom-Json
                foreach ($entryName in @('*') + $ids) {
                    $property = @($parsed.PSObject.Properties | Where-Object { $_.Name -eq $entryName } | Select-Object -First 1)
                    if ($property.Count -gt 0) {
                        $entries += $entryName
                    }
                }
                $readback.policy_entries = @($entries | Sort-Object -Unique)
                $readback.attempted_hives = $attempts
                return $readback
            }
        } catch {
            $attempts += [pscustomobject]@{
                hive = $candidateHive
                ok = $false
                existing_policy = $true
                path = Get-ChromePolicyRoot -Hive $candidateHive
                error = $_.Exception.Message
            }
        }
    }
    return $null
}

function Invoke-ElevatedChromeExternalDebuggerPolicy {
    param(
        [string[]]$ExternalExtensionIds,
        [ValidateSet('AllExtensions', 'DetectedExtensions')]
        [string]$BlockScope = 'AllExtensions'
    )

    if ([string]::IsNullOrWhiteSpace($PSCommandPath) -or -not (Test-Path -LiteralPath $PSCommandPath -PathType Leaf)) {
        throw "SYNAPSE_CHROME_POLICY_ELEVATION_SCRIPT_PATH_MISSING path=$PSCommandPath remediation=run the verifier from a real .ps1 file so the elevated helper can execute the same audited policy writer"
    }
    $powershellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $powershellExe -PathType Leaf)) {
        throw "SYNAPSE_CHROME_POLICY_ELEVATION_POWERSHELL_MISSING path=$powershellExe remediation=repair Windows PowerShell or rerun setup from an elevated PowerShell host"
    }

    $evidenceRoot = Join-Path $env:TEMP 'synapse-chrome-policy-elevation'
    New-Item -ItemType Directory -Force -Path $evidenceRoot | Out-Null
    $evidencePath = Join-Path $evidenceRoot ("policy-{0}.json" -f ([guid]::NewGuid().ToString('n')))
    $runnerPath = Join-Path $evidenceRoot ("policy-runner-{0}.ps1" -f ([guid]::NewGuid().ToString('n')))
    $ids = @($ExternalExtensionIds | Where-Object { $_ -match '^[a-p]{32}$' } | Sort-Object -Unique)
    $idsLiteral = '@(' + (($ids | ForEach-Object { Quote-PowerShellLiteral $_ }) -join ',') + ')'
    $policyCommand = @(
        '&',
        (Quote-PowerShellLiteral $PSCommandPath),
        '-ChromePolicyOnly',
        '-ChromePolicyHive', 'HKLM',
        '-ChromePolicyBlockScope', (Quote-PowerShellLiteral $BlockScope),
        '-AutoElevateChromePolicy', '$false',
        '-ChromePolicyEvidencePath', (Quote-PowerShellLiteral $evidencePath),
        '-ChromePolicyExternalExtensionIds', $idsLiteral
    ) -join ' '
    $runner = @"
`$ErrorActionPreference = 'Stop'
try {
    $policyCommand
    exit `$LASTEXITCODE
} catch {
    `$payload = [ordered]@{
        ok = `$false
        mode = 'chrome_policy_elevated_runner'
        error = `$_.Exception.Message
    }
    Set-Content -LiteralPath $(Quote-PowerShellLiteral $evidencePath) -Value (`$payload | ConvertTo-Json -Depth 12 -Compress) -Encoding UTF8
    Write-Error `$_.Exception.Message
    exit 1
}
"@
    Set-Content -LiteralPath $runnerPath -Value $runner -Encoding UTF8
    $argumentTokens = @(
        '-NoProfile',
        '-ExecutionPolicy', 'Bypass',
        '-File', (Quote-ProcessArgument $runnerPath)
    )

    try {
        $process = Start-Process `
            -FilePath $powershellExe `
            -ArgumentList ($argumentTokens -join ' ') `
            -Verb RunAs `
            -WindowStyle Hidden `
            -Wait `
            -PassThru `
            -ErrorAction Stop
    } catch {
        Remove-Item -LiteralPath $runnerPath -Force -ErrorAction SilentlyContinue
        throw "SYNAPSE_CHROME_POLICY_ELEVATION_START_FAILED path=$powershellExe evidence_path=$evidencePath detail=$($_.Exception.Message) remediation=approve the one-time UAC prompt, rerun setup from an elevated shell, or disable/remove the named external Chrome extension/native host"
    }
    Remove-Item -LiteralPath $runnerPath -Force -ErrorAction SilentlyContinue

    if (-not (Test-Path -LiteralPath $evidencePath -PathType Leaf)) {
        throw "SYNAPSE_CHROME_POLICY_ELEVATION_NO_EVIDENCE exit_code=$($process.ExitCode) evidence_path=$evidencePath remediation=elevated helper did not write its policy evidence file; inspect Windows event logs/UAC denial and rerun setup elevated"
    }
    try {
        $payload = Get-Content -Raw -LiteralPath $evidencePath | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw "SYNAPSE_CHROME_POLICY_ELEVATION_EVIDENCE_INVALID exit_code=$($process.ExitCode) evidence_path=$evidencePath detail=$($_.Exception.Message) remediation=elevated helper evidence was not valid JSON"
    }
    if ($process.ExitCode -ne 0 -or -not $payload.ok) {
        $detail = ConvertTo-CompressedJson -Value $payload -Depth 10
        throw "SYNAPSE_CHROME_POLICY_ELEVATION_FAILED exit_code=$($process.ExitCode) evidence_path=$evidencePath detail=$detail remediation=elevated HKLM Chrome policy write failed; rerun setup elevated or repair policy registry permissions"
    }
    $result = $payload.result
    $result | Add-Member -NotePropertyName elevated -NotePropertyValue $true -Force
    $result | Add-Member -NotePropertyName elevation_evidence_path -NotePropertyValue $evidencePath -Force
    return $result
}

if ($ChromePolicyOnly) {
    try {
        # Policy-only is the elevated child contract; never recurse into elevation.
        $policyOnlyReadback = Write-ChromeExternalDebuggerPolicyAuto `
            -Hive $ChromePolicyHive `
            -ExternalExtensionIds $ChromePolicyExternalExtensionIds `
            -BlockScope $ChromePolicyBlockScope `
            -AutoElevate:$false
        $payload = [ordered]@{
            ok = $true
            mode = 'chrome_policy_only'
            result = $policyOnlyReadback
        }
        if (-not [string]::IsNullOrWhiteSpace($ChromePolicyEvidencePath)) {
            New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ChromePolicyEvidencePath) | Out-Null
            Set-Content -LiteralPath $ChromePolicyEvidencePath -Value (ConvertTo-CompressedJson -Value $payload -Depth 12) -Encoding UTF8
        }
        [pscustomobject]$payload
        exit 0
    } catch {
        $payload = [ordered]@{
            ok = $false
            mode = 'chrome_policy_only'
            error = $_.Exception.Message
        }
        if (-not [string]::IsNullOrWhiteSpace($ChromePolicyEvidencePath)) {
            New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ChromePolicyEvidencePath) | Out-Null
            Set-Content -LiteralPath $ChromePolicyEvidencePath -Value (ConvertTo-CompressedJson -Value $payload -Depth 12) -Encoding UTF8
        }
        Write-Error $_.Exception.Message
        exit 1
    }
}

function Get-SynapseNativeHostRegistryTargets {
    param(
        [Parameter(Mandatory = $true)]
        [string]$HostName
    )

    @(
        [pscustomobject]@{
            hive = 'HKCU'
            registry_view = '64'
            path = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
        },
        [pscustomobject]@{
            hive = 'HKLM'
            registry_view = '64'
            path = "HKLM:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
        },
        [pscustomobject]@{
            hive = 'HKCU'
            registry_view = '32'
            path = "HKCU:\Software\Wow6432Node\Google\Chrome\NativeMessagingHosts\$HostName"
        },
        [pscustomobject]@{
            hive = 'HKLM'
            registry_view = '32'
            path = "HKLM:\Software\Wow6432Node\Google\Chrome\NativeMessagingHosts\$HostName"
        }
    )
}

function Read-SynapseNativeHostRegistryEntries {
    param(
        [Parameter(Mandatory = $true)]
        [string]$HostName
    )

    $entries = @()
    foreach ($target in (Get-SynapseNativeHostRegistryTargets -HostName $HostName)) {
        if (-not (Test-Path -LiteralPath $target.path)) {
            continue
        }
        try {
            $key = Get-Item -LiteralPath $target.path -ErrorAction Stop
            $manifestPath = [string]$key.GetValue('')
            $entries += [pscustomobject]@{
                hive = $target.hive
                registry_view = $target.registry_view
                path = $target.path
                manifest_path = $manifestPath
                read_error = $null
            }
        } catch {
            $entries += [pscustomobject]@{
                hive = $target.hive
                registry_view = $target.registry_view
                path = $target.path
                manifest_path = $null
                read_error = $_.Exception.Message
            }
        }
    }
    return @($entries)
}

function Remove-SynapseNativeHostRegistryEntries {
    param(
        [Parameter(Mandatory = $true)]
        [string]$HostName
    )

    $before = @(Read-SynapseNativeHostRegistryEntries -HostName $HostName)
    $removed = @()
    $failures = @()

    foreach ($entry in $before) {
        try {
            Remove-Item -LiteralPath $entry.path -Force -ErrorAction Stop
            $removed += [pscustomobject]@{
                hive = $entry.hive
                registry_view = $entry.registry_view
                path = $entry.path
                manifest_path = $entry.manifest_path
            }
        } catch {
            $failures += [pscustomobject]@{
                hive = $entry.hive
                registry_view = $entry.registry_view
                path = $entry.path
                manifest_path = $entry.manifest_path
                error = $_.Exception.Message
                acl_detail = Get-RegistryAclDiagnostic -Path $entry.path
            }
        }
    }

    $after = @(Read-SynapseNativeHostRegistryEntries -HostName $HostName)
    if ($failures.Count -gt 0 -or $after.Count -gt 0) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            host_name = $HostName
            before = $before
            removed = $removed
            after = $after
            failures = $failures
        }) -Depth 10
        throw "SYNAPSE_CHROME_NATIVE_HOST_REGISTRY_REMOVE_FAILED_ALL_HIVES detail=$detail remediation=normal bridge must not leave Synapse nativeMessaging host registration in any Chrome lookup hive because Chrome can launch visible native-host wrappers; remove the listed registry keys from a principal that can write them and rerun this verifier"
    }

    [pscustomobject]@{
        host_name = $HostName
        before = $before
        removed = $removed
        after = $after
    }
}

if ($ExtensionId -notmatch '^[a-p]{32}$') {
    throw "SYNAPSE_CHROME_EXTENSION_ID_INVALID extension_id=$ExtensionId remediation=Chrome extension IDs are 32 lowercase characters in the range a-p; refusing to inspect profiles with an ambiguous extension identity"
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$extensionDir = Join-Path $repoRoot 'extensions\synapse-chrome-debugger'
$manifestPath = Join-Path $extensionDir 'manifest.json'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "SYNAPSE_CHROME_EXTENSION_MANIFEST_MISSING path=$manifestPath"
}
$extensionManifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
$requiredPermissions = @($extensionManifest.permissions)
$optionalPermissions = @($extensionManifest.optional_permissions)
$hostPermissions = @($extensionManifest.host_permissions)
if ($requiredPermissions -contains 'debugger') {
    throw "SYNAPSE_CHROME_EXTENSION_REQUIRED_DEBUGGER_PERMISSION_FORBIDDEN path=$manifestPath remediation=normal end-user bridge must use chrome.tabs without required debugger permission"
}
if ($optionalPermissions -contains 'debugger') {
    throw "SYNAPSE_CHROME_EXTENSION_OPTIONAL_DEBUGGER_PERMISSION_FORBIDDEN path=$manifestPath remediation=normal end-user bridge must use chrome.tabs without optional debugger permission; use raw CDP from a Synapse-launched automation profile for DOM/action CDP"
}
if ($requiredPermissions -contains 'nativeMessaging') {
    throw "SYNAPSE_CHROME_EXTENSION_NATIVE_MESSAGING_FORBIDDEN path=$manifestPath remediation=normal end-user bridge must use direct localhost HTTP registration plus WebSocket command delivery; nativeMessaging can launch a visible cmd.exe wrapper on Windows"
}
if ($optionalPermissions -contains 'nativeMessaging') {
    throw "SYNAPSE_CHROME_EXTENSION_OPTIONAL_NATIVE_MESSAGING_FORBIDDEN path=$manifestPath remediation=normal end-user bridge must not request nativeMessaging"
}
if ($requiredPermissions -contains 'alarms' -or $optionalPermissions -contains 'alarms') {
    throw "SYNAPSE_CHROME_EXTENSION_ALARMS_PERMISSION_FORBIDDEN path=$manifestPath remediation=normal end-user bridge must not use chrome.alarms or recurring wakeups; daemon disconnects must be handled by bounded WebSocket reconnect plus a low-frequency runtime keepalive while disconnected"
}
if ($hostPermissions -notcontains 'http://127.0.0.1:7700/*') {
    throw "SYNAPSE_CHROME_EXTENSION_LOCALHOST_PERMISSION_MISSING path=$manifestPath remediation=normal bridge requires host_permissions http://127.0.0.1:7700/* for direct daemon registration and message posting"
}

$nativeRoot = Join-Path $env:APPDATA 'synapse\chrome-debugger'
New-Item -ItemType Directory -Force -Path $nativeRoot | Out-Null

$hostName = 'com.synapse.chrome_debugger'
$hostManifestPath = Join-Path $nativeRoot "$hostName.json"
$registryPath = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$hostName"
$nativeHostRegistryCleanup = Remove-SynapseNativeHostRegistryEntries -HostName $hostName
if (Test-Path -LiteralPath $hostManifestPath -PathType Leaf) {
    Remove-Item -LiteralPath $hostManifestPath -Force
}
if (Test-Path -LiteralPath $hostManifestPath -PathType Leaf) {
    throw "SYNAPSE_CHROME_NATIVE_HOST_MANIFEST_REMOVE_FAILED path=$hostManifestPath remediation=normal bridge must use direct localhost WebSocket command delivery only"
}

$chromeProcesses = @(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" -ErrorAction SilentlyContinue | ForEach-Object {
    $commandLine = [string]$_.CommandLine
    [pscustomobject]@{
        pid = [int]$_.ProcessId
        parent_pid = [int]$_.ParentProcessId
        creation_date = [string]$_.CreationDate
        command_line_readable = -not [string]::IsNullOrWhiteSpace($commandLine)
        has_silent_debugger_switch = $commandLine -match '(^|\s)--silent-debugger-extension-api(\s|=|$)'
    }
})

$chromeUserDataRoot = Join-Path $env:LOCALAPPDATA 'Google\Chrome\User Data'
$synapseChromeProfileReadback = @()
$staleSynapseActivePermissions = @()
$staleSynapseRecurringWakePermissions = @()
$externalDebuggerOrNativeExtensions = @()
$externalDisabledDebuggerOrNativeExtensions = @()
$externalDebuggerExtensions = @()
function Get-ChromeExtensionRuntimeState {
    param(
        [Parameter(Mandatory = $true)]
        $Setting
    )

    $activeBit = $null
    if ($Setting.PSObject.Properties.Name -contains 'active_bit') {
        $activeBit = [bool]$Setting.active_bit
    }
    $disableReasons = @()
    if ($Setting.PSObject.Properties.Name -contains 'disable_reasons' -and $null -ne $Setting.disable_reasons) {
        $disableReasons = @($Setting.disable_reasons)
    }
    $runtimeEnabled = $true
    if ($activeBit -eq $false -or $disableReasons.Count -gt 0) {
        $runtimeEnabled = $false
    }

    [pscustomobject]@{
        active_bit = $activeBit
        disable_reasons = $disableReasons
        runtime_enabled = $runtimeEnabled
    }
}
if (Test-Path -LiteralPath $chromeUserDataRoot -PathType Container) {
    $profileDirs = @(Get-ChildItem -LiteralPath $chromeUserDataRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -ne 'Snapshots' })
    foreach ($profileDir in $profileDirs) {
        $extensionRuntimeById = @{}
        foreach ($prefFileName in @('Preferences', 'Secure Preferences')) {
            $prefPath = Join-Path $profileDir.FullName $prefFileName
            if (-not (Test-Path -LiteralPath $prefPath -PathType Leaf)) {
                continue
            }
            try {
                $pref = Get-Content -Raw -LiteralPath $prefPath | ConvertFrom-Json -ErrorAction Stop
            } catch {
                $synapseChromeProfileReadback += [pscustomobject]@{
                    profile = $profileDir.Name
                    pref_file = $prefFileName
                    path = $prefPath
                    parse_error = $_.Exception.Message
                }
                continue
            }
            if (-not $pref.extensions -or -not $pref.extensions.settings) {
                continue
            }
            foreach ($extensionProperty in $pref.extensions.settings.PSObject.Properties) {
                $setting = $extensionProperty.Value
                $runtimeState = Get-ChromeExtensionRuntimeState -Setting $setting
                if ($prefFileName -eq 'Preferences') {
                    $extensionRuntimeById[$extensionProperty.Name] = $runtimeState
                } elseif ($extensionRuntimeById.ContainsKey($extensionProperty.Name)) {
                    $runtimeState = $extensionRuntimeById[$extensionProperty.Name]
                }
                $activeApi = @()
                if ($setting.active_permissions -and $setting.active_permissions.api) {
                    $activeApi = @($setting.active_permissions.api)
                }
                $grantedApi = @()
                if ($setting.granted_permissions -and $setting.granted_permissions.api) {
                    $grantedApi = @($setting.granted_permissions.api)
                }
                if ($extensionProperty.Name -eq $ExtensionId) {
                    $row = [pscustomobject]@{
                        profile = $profileDir.Name
                        pref_file = $prefFileName
                        path = $prefPath
                        manifest_path = $setting.path
                        active_api = $activeApi
                        granted_api = $grantedApi
                        active_bit = $runtimeState.active_bit
                        disable_reasons = $runtimeState.disable_reasons
                        runtime_enabled = $runtimeState.runtime_enabled
                    }
                    $synapseChromeProfileReadback += $row
                    if ($activeApi -contains 'debugger' -or $activeApi -contains 'nativeMessaging') {
                        $staleSynapseActivePermissions += $row
                    }
                    if ($activeApi -contains 'alarms') {
                        $staleSynapseRecurringWakePermissions += $row
                    }
                } elseif ($activeApi -contains 'debugger' -or $activeApi -contains 'nativeMessaging') {
                    $externalRow = [pscustomobject]@{
                        profile = $profileDir.Name
                        pref_file = $prefFileName
                        extension_id = $extensionProperty.Name
                        name = $setting.manifest.name
                        location = $setting.location
                        manifest_path = $setting.path
                        active_api = $activeApi
                        active_bit = $runtimeState.active_bit
                        disable_reasons = $runtimeState.disable_reasons
                        runtime_enabled = $runtimeState.runtime_enabled
                    }
                    if ($runtimeState.runtime_enabled) {
                        $externalDebuggerOrNativeExtensions += $externalRow
                        if ($activeApi -contains 'debugger') {
                            $externalDebuggerExtensions += $externalRow
                        }
                    } else {
                        $externalDisabledDebuggerOrNativeExtensions += $externalRow
                    }
                }
            }
        }
    }
}
if ($staleSynapseActivePermissions.Count -gt 0) {
    $detail = $staleSynapseActivePermissions | ConvertTo-Json -Depth 6 -Compress
    throw "SYNAPSE_CHROME_EXTENSION_STALE_ACTIVE_DEBUGGER_PERMISSION extension_id=$ExtensionId detail=$detail remediation=if daemon health shows the live bridge advertises reloadSelf, call cdp_bridge_reload through the real Synapse MCP tool; if the loaded worker predates reloadSelf, fail closed and let Chrome reload/restart the extension out-of-band instead of automating chrome://extensions foreground UI; the normal bridge must be active with tabs only before setup can pass"
}
if ($staleSynapseRecurringWakePermissions.Count -gt 0) {
    $detail = $staleSynapseRecurringWakePermissions | ConvertTo-Json -Depth 6 -Compress
    throw "SYNAPSE_CHROME_EXTENSION_STALE_ALARMS_PERMISSION extension_id=$ExtensionId detail=$detail remediation=if daemon health shows the live bridge advertises reloadSelf, call cdp_bridge_reload through the real Synapse MCP tool; if the loaded worker predates reloadSelf, fail closed and let Chrome reload/restart the extension out-of-band instead of automating chrome://extensions foreground UI; the normal bridge must be active with tabs only and no recurring wake permission before setup can certify popup-free behavior"
}

$externalNativeMessagingProcesses = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
    Where-Object {
        $_.CommandLine -match 'chrome\.nativeMessaging' -and
        $_.CommandLine -notmatch [regex]::Escape($ExtensionId)
    } |
    Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine,
        @{Name = 'ExtensionId'; Expression = {
            if ($_.CommandLine -match 'chrome-extension://([a-p]{32})') {
                $Matches[1]
            } else {
                $null
            }
        }})

$externalHazardExtensionIds = @(
    @($externalDebuggerOrNativeExtensions | ForEach-Object { $_.extension_id })
    @($externalNativeMessagingProcesses | ForEach-Object { $_.ExtensionId })
) | Where-Object { $_ -match '^[a-p]{32}$' } | Sort-Object -Unique

$chromePolicyReadback = $null
if ($ApplyExternalChromeDebuggerPolicy -and
    ($externalHazardExtensionIds.Count -gt 0 -or $ChromePolicyBlockScope -eq 'AllExtensions')) {
    $chromePolicyReadback = Read-ExistingChromeExternalDebuggerPolicy `
        -Hive $ChromePolicyHive `
        -ExternalExtensionIds $externalHazardExtensionIds `
        -BlockScope $ChromePolicyBlockScope
    if ($null -eq $chromePolicyReadback) {
        $chromePolicyReadback = Write-ChromeExternalDebuggerPolicyAuto `
            -Hive $ChromePolicyHive `
            -ExternalExtensionIds $externalHazardExtensionIds `
            -BlockScope $ChromePolicyBlockScope
    }
}

if ($ApplyExternalChromeDebuggerPolicy -and
    -not $AllowExternalChromeDebuggerOrNativeMessaging -and
    ($externalDebuggerOrNativeExtensions.Count -gt 0 -or $externalNativeMessagingProcesses.Count -gt 0)) {
    $policyBlocksPopupSurfaces = Test-ChromePolicyReadbackBlocksPopupSurfaces `
        -PolicyReadback $chromePolicyReadback `
        -ExternalExtensionIds $externalHazardExtensionIds
    $errorCode = if ($policyBlocksPopupSurfaces) {
        'SYNAPSE_CHROME_POLICY_PENDING_CHROME_RELOAD'
    } else {
        'SYNAPSE_CHROME_EXTERNAL_DEBUGGER_OR_NATIVE_SURFACE_PRESENT'
    }
    $remediation = if ($policyBlocksPopupSurfaces) {
        'Chrome ExtensionSettings blocked_permissions=[debugger,nativeMessaging] is persisted, but the running Chrome profile/process Source of Truth still exposes the external surface; reload Chrome policies from chrome://policy or restart Chrome, then rerun this verifier. Synapse will not certify popup-free readiness until the separate profile/process readback is clean.'
    } else {
        'HKCU/HKLM Chrome ExtensionSettings wildcard "*" blocked_permissions=[debugger,nativeMessaging], or disable/remove the offending extension, then refresh/restart Chrome and rerun this verifier'
    }
    $detail = [pscustomobject]@{
        external_debugger_or_native_extensions = $externalDebuggerOrNativeExtensions
        external_disabled_debugger_or_native_extensions = $externalDisabledDebuggerOrNativeExtensions
        external_debugger_extensions = $externalDebuggerExtensions
        external_native_messaging_processes = $externalNativeMessagingProcesses
        external_hazard_extension_ids = $externalHazardExtensionIds
        current_chrome_processes = $chromeProcesses
        chrome_policy_readback = $chromePolicyReadback
        chrome_policy_blocks_popup_surfaces = $policyBlocksPopupSurfaces
        chrome_policy_pending_chrome_reload = $policyBlocksPopupSurfaces
        chrome_policy_remediation = $remediation
    } | ConvertTo-Json -Depth 8 -Compress
    throw "$errorCode detail=$detail remediation=$remediation"
}

[pscustomobject]@{
    ok = $true
    native_host = $hostName
    native_manifest = $null
    registry_key = $registryPath
    binary = $null
    extension_id = $ExtensionId
    extension_dir = $extensionDir
    daemon_bridge_transport = 'direct_localhost_websocket'
    daemon_bridge_origin = "chrome-extension://$ExtensionId"
    bridge_self_reload_command = 'cdp_bridge_reload'
    bridge_build_id_expected = 'synapse-chrome-bridge-2026-06-15-1011-reload-self-997-type-active-v1'
    bridge_build_sha256_expected = 'd3b6eba7db70928066b39e687884fd34d06da19db115026ca5e3f94b8fc62e46'
    bridge_required_capabilities = @('closeTab', 'navigateTab', 'openTab', 'reloadSelf', 'targetInfo', 'typeActiveElement')
    background_navigation_backend = 'chrome.tabs_no_debugger_permission_no_native_messaging'
    reconnect_driver = 'bounded_websocket_reconnect_with_disconnected_extension_keepalive_no_alarms'
    attach_popup_prevention = 'normal_bridge_tabs_only_no_debugger_api_no_nativeMessaging_permission_plus_daemon_side_attach_disabled'
    normal_bridge_attach_commands_available = $false
    normal_bridge_debugger_api_calls_present = $false
    expected_extension_id_guard_present = $true
    required_alarms_permission_present = $false
    recurring_wakeup_permission_present = $false
    required_debugger_permission_present = $false
    optional_debugger_permission_present = $false
    required_native_messaging_permission_present = $false
    optional_native_messaging_permission_present = $false
    localhost_host_permission_present = $true
    native_host_registry_keys = @((Get-SynapseNativeHostRegistryTargets -HostName $hostName) | ForEach-Object { $_.path })
    native_host_registry_cleanup = $nativeHostRegistryCleanup
    native_host_registry_present = ($nativeHostRegistryCleanup.after.Count -gt 0)
    native_host_manifest_present = (Test-Path -LiteralPath $hostManifestPath)
    silent_debugger_switch_required_for_attach_commands = $false
    silent_debugger_switch = $null
    current_chrome_processes = $chromeProcesses
    chrome_policy_scope = if ($chromePolicyReadback) { $chromePolicyReadback.hive } else { $ChromePolicyHive }
    chrome_policy_requested_scope = $ChromePolicyHive
    chrome_policy_block_scope = $ChromePolicyBlockScope
    chrome_policy_readback = $chromePolicyReadback
    synapse_chrome_profile_readback = $synapseChromeProfileReadback
    external_hazard_extension_ids = $externalHazardExtensionIds
    external_debugger_or_native_extensions = $externalDebuggerOrNativeExtensions
    external_disabled_debugger_or_native_extensions = $externalDisabledDebuggerOrNativeExtensions
    external_debugger_extensions = $externalDebuggerExtensions
    external_native_messaging_processes = $externalNativeMessagingProcesses
}
