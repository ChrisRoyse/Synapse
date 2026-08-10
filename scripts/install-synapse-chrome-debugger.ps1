param(
    [string]$SynapseNativeHostExe = "$env:USERPROFILE\.cargo\bin\synapse-chrome-native-host.exe",
    [string]$ExtensionId = "leoocgnkjnplbfdbklajepahofecgfbk",
    [string]$TokenPath = "$env:APPDATA\synapse\token.txt",
    # Maintenance/self-heal entry point: run ONLY the one-way removal of any
    # debugger/nativeMessaging blockers Synapse wrote into the Chrome
    # ExtensionSettings policy, print the result, and exit.
    [switch]$RemoveExternalDebuggerPolicyOnly,
    # Emergency/operator opt-out. Default behavior shields the normal Chrome
    # profile from layout-shifting debugger/native-host popups by adding
    # Synapse-authored blocked_permissions entries for detected hazards.
    [switch]$PreserveExternalDebuggerExtensions,
    # Default behavior auto-loads the bundled unpacked extension into the
    # already-open active Chrome profile when the profile row is absent.
    [switch]$SkipAutoInstall,
    # Maintenance/self-heal entry point: when the expected unpacked extension row
    # is already installed and ready but no daemon bridge host is registered, use
    # the already-open Chrome extensions page UI to invoke its Reload button.
    [switch]$ReloadExistingExtensionViaUi,
    # Machine-readable process boundary used by the repo-built daemon. The
    # payload is emitted as one base64-framed JSON record so PowerShell warning
    # streams cannot corrupt or impersonate the result.
    [switch]$OutputJson,
    # Cleanup-only recovery used when the replacement bridge cannot perform its
    # exact token-scoped close. The lease is the installer's own base64-framed
    # JSON readback from the immediately preceding maintenance operation.
    [switch]$CleanupOwnedMaintenanceTabViaUi,
    [string]$MaintenanceCleanupLeaseBase64 = '',
    # Durable ownership identity for the one maintenance tab used by the
    # daemon-controlled Reload/Load unpacked flow. The UI path always creates
    # a new tab and proves its unique marker before navigating it, so repair
    # never depends on the extension mutation lane that it is repairing.
    [string]$MaintenanceTabToken = '',
    [string]$MaintenanceMarkerUrl = '',
    [string]$MaintenanceMarkerTitle = '',
    [ValidateRange(5, 300)]
    [int]$AutoInstallTimeoutSeconds = 90
)

$ErrorActionPreference = 'Stop'
if ($SkipAutoInstall -and $ReloadExistingExtensionViaUi) {
    throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_RELOAD_CONFLICT skip_auto_install=true reload_existing_extension_via_ui=true remediation=choose either the read-only skip path or the existing-extension UI reload path; setup must not silently ignore one of these mutually exclusive modes"
}
if ($CleanupOwnedMaintenanceTabViaUi -and ($SkipAutoInstall -or $ReloadExistingExtensionViaUi)) {
    throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_MODE_CONFLICT cleanup_only=true skip_auto_install=$SkipAutoInstall reload_existing=$ReloadExistingExtensionViaUi remediation=cleanup-only accepts only the exact prior lease and marker identity"
}
if ($CleanupOwnedMaintenanceTabViaUi -and [string]::IsNullOrWhiteSpace($MaintenanceCleanupLeaseBase64)) {
    throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_LEASE_MISSING remediation=cleanup-only requires the base64-framed exact lease returned by the preceding installer operation"
}
if (
    -not [string]::IsNullOrWhiteSpace($MaintenanceTabToken) -and
    $MaintenanceTabToken -notmatch '^[a-f0-9]{32}$'
) {
    throw "SYNAPSE_CHROME_MAINTENANCE_TOKEN_INVALID token=$MaintenanceTabToken remediation=use the daemon-generated 32-character lowercase hexadecimal ownership token"
}
if ([string]::IsNullOrWhiteSpace($MaintenanceTabToken)) {
    $MaintenanceTabToken = [Guid]::NewGuid().ToString('N')
}
if ([string]::IsNullOrWhiteSpace($MaintenanceMarkerTitle)) {
    $MaintenanceMarkerTitle = "Synapse Bridge Maintenance $MaintenanceTabToken"
}
if ([string]::IsNullOrWhiteSpace($MaintenanceMarkerUrl)) {
    $encodedMarkerTitle = [Uri]::EscapeDataString($MaintenanceMarkerTitle)
    $MaintenanceMarkerUrl = "data:text/html,<title>$encodedMarkerTitle</title>"
}
$expectedMarkerTitle = "Synapse Bridge Maintenance $MaintenanceTabToken"
if ($MaintenanceMarkerTitle -cne $expectedMarkerTitle) {
    throw "SYNAPSE_CHROME_MAINTENANCE_MARKER_TITLE_INVALID actual=$MaintenanceMarkerTitle expected=$expectedMarkerTitle remediation=the marker title must be derived exactly from the ownership token"
}
$expectedMarkerUrl = "data:text/html,<title>$([Uri]::EscapeDataString($MaintenanceMarkerTitle))</title>"
if ($MaintenanceMarkerUrl -cne $expectedMarkerUrl) {
    throw "SYNAPSE_CHROME_MAINTENANCE_MARKER_URL_INVALID actual=$MaintenanceMarkerUrl expected=$expectedMarkerUrl remediation=the marker URL must be derived exactly from the ownership title"
}
# `Get-Acl` is diagnostic-only below. Some hidden Windows PowerShell launch
# environments have Security type data loaded while the cmdlet is not callable;
# importing then fails with duplicate type-data errors. Let the diagnostic reader
# report an acl_error instead of aborting bridge installation.

function ConvertTo-CompressedJson {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Value,
        [int]$Depth = 12
    )
    ConvertTo-Json -InputObject $Value -Depth $Depth -Compress
}

function Get-SynapseFileSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "SYNAPSE_CHROME_FILE_HASH_MISSING path=$Path remediation=write the extension artifact before hashing it"
    }
    try {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            $share = [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
            $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, $share)
            try {
                $hash = $sha.ComputeHash($stream)
            } finally {
                $stream.Dispose()
            }
        } finally {
            $sha.Dispose()
        }
        # Cryptographic hex is canonical lowercase across the extension,
        # daemon health payload, and strict equality readbacks.
        return (($hash | ForEach-Object { $_.ToString('x2') }) -join '')
    } catch {
        throw "SYNAPSE_CHROME_FILE_HASH_FAILED path=$Path error=$($_.Exception.Message) remediation=verify the extension artifact is readable and retry the Chrome bridge install"
    }
}

function Get-SynapseSha256HexLower {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $hash = $sha.ComputeHash($Bytes)
        return (($hash | ForEach-Object { $_.ToString('x2') }) -join '')
    } finally {
        $sha.Dispose()
    }
}

function Test-SynapseByteArrayExactEqual {
    param(
        [Parameter(Mandatory = $true)][byte[]]$Left,
        [Parameter(Mandatory = $true)][byte[]]$Right
    )
    if ($Left.Length -ne $Right.Length) {
        return $false
    }
    for ($index = 0; $index -lt $Left.Length; $index += 1) {
        if ($Left[$index] -ne $Right[$index]) {
            return $false
        }
    }
    return $true
}

function Read-SynapseUtf8FileStrict {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "SYNAPSE_CHROME_UTF8_SOURCE_MISSING path=$Path remediation=restore the required Chrome bridge artifact and retry"
    }
    try {
        $bytes = [System.IO.File]::ReadAllBytes($Path)
    } catch {
        throw "SYNAPSE_CHROME_UTF8_SOURCE_READ_FAILED path=$Path error=$($_.Exception.Message) remediation=verify the Chrome bridge artifact is readable and retry"
    }
    if (
        $bytes.Length -ge 3 -and
        $bytes[0] -eq 0xEF -and
        $bytes[1] -eq 0xBB -and
        $bytes[2] -eq 0xBF
    ) {
        throw "SYNAPSE_CHROME_UTF8_BOM_FORBIDDEN path=$Path byte_length=$($bytes.Length) remediation=store service_worker.js as canonical UTF-8 without a byte-order mark"
    }
    $utf8Strict = New-Object System.Text.UTF8Encoding($false, $true)
    try {
        $text = $utf8Strict.GetString($bytes)
    } catch [System.Text.DecoderFallbackException] {
        throw "SYNAPSE_CHROME_UTF8_INVALID path=$Path byte_length=$($bytes.Length) error=$($_.Exception.Message) remediation=repair service_worker.js as valid UTF-8 without lossy replacement characters"
    }
    [pscustomobject]@{
        bytes = $bytes
        text = $text
        byte_length = $bytes.Length
        sha256 = Get-SynapseSha256HexLower -Bytes $bytes
    }
}

function Write-SynapseFileBytesAtomic {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][byte[]]$ExpectedBytes
    )
    $destination = [System.IO.Path]::GetFullPath($Path)
    $parent = [System.IO.Path]::GetDirectoryName($destination)
    if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
        throw "SYNAPSE_CHROME_ATOMIC_REPLACE_PARENT_MISSING path=$destination parent=$parent remediation=create and verify the stable extension directory before injecting its host-local credential"
    }
    $targetExisted = Test-Path -LiteralPath $destination -PathType Leaf
    $tempName = '.{0}.synapse-{1}.tmp' -f [System.IO.Path]::GetFileName($destination), ([Guid]::NewGuid().ToString('N'))
    $tempPath = Join-Path $parent $tempName
    $backupPath = $null
    try {
        [System.IO.File]::WriteAllBytes($tempPath, $ExpectedBytes)
        $tempReadback = [System.IO.File]::ReadAllBytes($tempPath)
        if (-not (Test-SynapseByteArrayExactEqual -Left $ExpectedBytes -Right $tempReadback)) {
            throw "SYNAPSE_CHROME_ATOMIC_STAGE_READBACK_MISMATCH path=$tempPath expected_length=$($ExpectedBytes.Length) actual_length=$($tempReadback.Length)"
        }
        if ($targetExisted) {
            # Windows PowerShell 5.1's overload binder does not reliably pass
            # `$null` as File.Replace's optional backup path. Use an exact,
            # same-directory backup so the destination update remains atomic
            # and the previous bytes remain available until readback succeeds.
            $backupName = '.{0}.synapse-backup-{1}.tmp' -f [System.IO.Path]::GetFileName($destination), ([Guid]::NewGuid().ToString('N'))
            $backupPath = Join-Path $parent $backupName
            [System.IO.File]::Replace($tempPath, $destination, $backupPath, $true)
        } else {
            # A same-volume rename publishes a first deployment atomically.
            [System.IO.File]::Move($tempPath, $destination)
        }
    } catch {
        $backupState = if ($null -ne $backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
            "present:$backupPath"
        } else {
            'absent'
        }
        throw "SYNAPSE_CHROME_ATOMIC_REPLACE_FAILED path=$destination target_existed=$targetExisted temp_path=$tempPath backup_state=$backupState error=$($_.Exception.Message) remediation=verify the stable extension directory is writable and supports same-directory atomic replacement; if backup_state is present, preserve the named backup for exact recovery"
    } finally {
        if (Test-Path -LiteralPath $tempPath) {
            try {
                [System.IO.File]::Delete($tempPath)
            } catch {
                throw "SYNAPSE_CHROME_ATOMIC_STAGE_CLEANUP_FAILED temp_path=$tempPath error=$($_.Exception.Message) remediation=remove only the named temporary file after confirming it is not the deployed worker, then retry"
            }
            if (Test-Path -LiteralPath $tempPath) {
                throw "SYNAPSE_CHROME_ATOMIC_STAGE_CLEANUP_UNVERIFIED temp_path=$tempPath remediation=remove only the named temporary file after confirming it is not the deployed worker, then retry"
            }
        }
    }
    $actualBytes = [System.IO.File]::ReadAllBytes($destination)
    if (-not (Test-SynapseByteArrayExactEqual -Left $ExpectedBytes -Right $actualBytes)) {
        $backupState = if ($null -ne $backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
            "present:$backupPath"
        } else {
            'absent'
        }
        throw "SYNAPSE_CHROME_ATOMIC_REPLACE_READBACK_MISMATCH path=$destination backup_state=$backupState expected_length=$($ExpectedBytes.Length) actual_length=$($actualBytes.Length) expected_sha256=$(Get-SynapseSha256HexLower -Bytes $ExpectedBytes) actual_sha256=$(Get-SynapseSha256HexLower -Bytes $actualBytes) remediation=the deployed worker bytes changed after atomic replacement; preserve any named backup and refuse to load the extension"
    }
    if ($null -ne $backupPath -and (Test-Path -LiteralPath $backupPath)) {
        try {
            [System.IO.File]::Delete($backupPath)
        } catch {
            throw "SYNAPSE_CHROME_ATOMIC_BACKUP_CLEANUP_FAILED backup_path=$backupPath error=$($_.Exception.Message) remediation=remove only the named backup after confirming the deployed worker hash matches the expected hash"
        }
        if (Test-Path -LiteralPath $backupPath) {
            throw "SYNAPSE_CHROME_ATOMIC_BACKUP_CLEANUP_UNVERIFIED backup_path=$backupPath remediation=remove only the named backup after confirming the deployed worker hash matches the expected hash"
        }
    }
    [pscustomobject]@{
        atomic_replace = $true
        atomic_mode = if ($targetExisted) { 'replace_existing_with_backup' } else { 'move_new' }
        byte_exact_readback = $true
        byte_length = $actualBytes.Length
        sha256 = Get-SynapseSha256HexLower -Bytes $actualBytes
    }
}

function Get-SynapseChromeBridgeRegisterToken {
    param([Parameter(Mandatory = $true)][string]$BearerToken)
    $domainBytes = [System.Text.Encoding]::UTF8.GetBytes('synapse.chrome_bridge.register.v1')
    $tokenBytes = [System.Text.Encoding]::UTF8.GetBytes($BearerToken)
    $bytes = New-Object byte[] ($domainBytes.Length + 1 + $tokenBytes.Length)
    [System.Buffer]::BlockCopy($domainBytes, 0, $bytes, 0, $domainBytes.Length)
    $bytes[$domainBytes.Length] = 0
    [System.Buffer]::BlockCopy($tokenBytes, 0, $bytes, ($domainBytes.Length + 1), $tokenBytes.Length)
    Get-SynapseSha256HexLower -Bytes $bytes
}

function Get-SynapseChromeBridgeBearerToken {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_SOURCE_MISSING path=$Path remediation=run scripts\synapse-setup.ps1 so the daemon bearer token exists before deploying the Chrome bridge"
    }
    $raw = Get-Content -Raw -LiteralPath $Path
    $token = if ($null -eq $raw) { '' } else { $raw.Trim() }
    if ($token.Length -lt 16) {
        throw "SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_SOURCE_INVALID path=$Path length=$($token.Length) remediation=delete the invalid daemon bearer token and rerun scripts\synapse-setup.ps1"
    }
    return $token
}

function New-SynapseChromeBridgeRegisterTokenDeployment {
    param(
        [Parameter(Mandatory = $true)]
        [string]$SourceServiceWorkerPath,
        [Parameter(Mandatory = $true)]
        [string]$RegisterToken
    )
    if ($RegisterToken -notmatch '^[0-9a-f]{64}$') {
        throw "SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_INVALID remediation=derived register token must be 64 lowercase hex chars"
    }
    $source = Read-SynapseUtf8FileStrict -Path $SourceServiceWorkerPath
    $pattern = 'const\s+BRIDGE_REGISTER_TOKEN\s*=\s*"";'
    $matches = [System.Text.RegularExpressions.Regex]::Matches($source.text, $pattern)
    if ($matches.Count -ne 1) {
        throw "SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_PLACEHOLDER_COUNT_INVALID path=$SourceServiceWorkerPath count=$($matches.Count) remediation=service_worker.js must contain exactly one empty BRIDGE_REGISTER_TOKEN declaration; setup refuses ambiguous or already-stamped source bytes"
    }
    $replacement = 'const BRIDGE_REGISTER_TOKEN = "' + $RegisterToken + '";'
    $updated = [System.Text.RegularExpressions.Regex]::Replace($source.text, $pattern, $replacement, 1)
    $encoding = New-Object System.Text.UTF8Encoding($false, $true)
    $expectedBytes = $encoding.GetBytes($updated)
    [pscustomobject]@{
        expected_bytes = $expectedBytes
        source_byte_length = $source.byte_length
        source_sha256 = $source.sha256
        expected_byte_length = $expectedBytes.Length
        expected_sha256 = Get-SynapseSha256HexLower -Bytes $expectedBytes
        placeholder_count = $matches.Count
    }
}

function Set-SynapseChromeBridgeRegisterToken {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ServiceWorkerPath,
        [Parameter(Mandatory = $true)]
        [object]$PreparedDeployment
    )
    $expectedBytes = [byte[]]$PreparedDeployment.expected_bytes
    if ($expectedBytes.Length -le 0) {
        throw "SYNAPSE_CHROME_BRIDGE_PREPARED_WORKER_EMPTY path=$ServiceWorkerPath remediation=prepare and validate the host-tokenized service worker before deploying it"
    }
    $expectedSha256 = Get-SynapseSha256HexLower -Bytes $expectedBytes
    if ($expectedSha256 -ne [string]$PreparedDeployment.expected_sha256) {
        throw "SYNAPSE_CHROME_BRIDGE_PREPARED_WORKER_HASH_MISMATCH path=$ServiceWorkerPath expected_sha256=$($PreparedDeployment.expected_sha256) actual_sha256=$expectedSha256 remediation=refuse deployment because prepared worker bytes changed in memory"
    }
    $writeReadback = Write-SynapseFileBytesAtomic -Path $ServiceWorkerPath -ExpectedBytes $expectedBytes
    [pscustomobject]@{
        source_byte_length = $PreparedDeployment.source_byte_length
        source_sha256 = $PreparedDeployment.source_sha256
        expected_byte_length = $PreparedDeployment.expected_byte_length
        expected_sha256 = $PreparedDeployment.expected_sha256
        placeholder_count = $PreparedDeployment.placeholder_count
        atomic_replace = $writeReadback.atomic_replace
        atomic_mode = $writeReadback.atomic_mode
        byte_exact_readback = $writeReadback.byte_exact_readback
        actual_byte_length = $writeReadback.byte_length
        actual_sha256 = $writeReadback.sha256
    }
}

function Assert-SynapseChromeBridgeRegisterTokenReadback {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ServiceWorkerPath,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedRegisterToken,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedServiceWorkerSha256,
        [Parameter(Mandatory = $true)]
        [long]$ExpectedServiceWorkerByteLength
    )
    $readback = Read-SynapseUtf8FileStrict -Path $ServiceWorkerPath
    $matches = [System.Text.RegularExpressions.Regex]::Matches($readback.text, 'const\s+BRIDGE_REGISTER_TOKEN\s*=\s*"([^"]*)";')
    if ($matches.Count -ne 1) {
        throw "SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_READBACK_COUNT_INVALID path=$ServiceWorkerPath count=$($matches.Count) remediation=deployed service_worker.js must contain exactly one BRIDGE_REGISTER_TOKEN declaration after setup injects the host-local credential"
    }
    $match = $matches[0]
    $actual = [string]$match.Groups[1].Value
    $actualSha256 = if ($actual.Length -gt 0) {
        Get-SynapseSha256HexLower -Bytes ([System.Text.Encoding]::UTF8.GetBytes($actual))
    } else {
        ''
    }
    $expectedSha256 = Get-SynapseSha256HexLower -Bytes ([System.Text.Encoding]::UTF8.GetBytes($ExpectedRegisterToken))
    if ($actual -ne $ExpectedRegisterToken) {
        throw "SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_READBACK_MISMATCH path=$ServiceWorkerPath actual_length=$($actual.Length) actual_sha256=$actualSha256 expected_length=$($ExpectedRegisterToken.Length) expected_sha256=$expectedSha256 remediation=setup copied or rewrote the unpacked extension without the derived daemon registration credential; rerun setup after verifying the deployed worker is writable"
    }
    if ($ExpectedServiceWorkerSha256 -notmatch '^[0-9a-f]{64}$') {
        throw "SYNAPSE_CHROME_BRIDGE_EXPECTED_WORKER_HASH_INVALID expected_sha256=$ExpectedServiceWorkerSha256 remediation=the atomic token injector must return a canonical lowercase SHA-256 before readback"
    }
    if ($readback.sha256 -ne $ExpectedServiceWorkerSha256) {
        throw "SYNAPSE_CHROME_BRIDGE_WORKER_BYTE_READBACK_MISMATCH path=$ServiceWorkerPath expected_length=$ExpectedServiceWorkerByteLength actual_length=$($readback.byte_length) expected_sha256=$ExpectedServiceWorkerSha256 actual_sha256=$($readback.sha256) remediation=the deployed worker changed after atomic token injection; refuse to load the extension"
    }
    if ($readback.byte_length -ne $ExpectedServiceWorkerByteLength) {
        throw "SYNAPSE_CHROME_BRIDGE_WORKER_BYTE_LENGTH_READBACK_MISMATCH path=$ServiceWorkerPath expected_length=$ExpectedServiceWorkerByteLength actual_length=$($readback.byte_length) expected_sha256=$ExpectedServiceWorkerSha256 actual_sha256=$($readback.sha256) remediation=the deployed worker byte length changed after atomic token injection; refuse to load the extension"
    }
    [pscustomobject]@{
        length = $actual.Length
        sha256 = $actualSha256
        matches_expected = $true
        service_worker_byte_length = $readback.byte_length
        service_worker_sha256 = $readback.sha256
        service_worker_matches_expected = $true
    }
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

# Exact blocked_install_message a previous Synapse version stamped onto every
# ExtensionSettings entry it authored. It is the ONLY reliable marker that lets
# the self-heal below distinguish Synapse-authored blockers from policy an
# enterprise admin or the user set themselves. Do not change this string: it
# must match byte-for-byte what shipped installers wrote, or old installs will
# not be healed.
$script:SynapseChromeBlockedInstallMessage = 'Synapse blocked this extension on this host because debugger/nativeMessaging permissions can surface Chrome debugger or native-host popups during background automation.'

function Remove-SynapseChromeExternalDebuggerPolicy {
    param(
        [string[]]$PreserveExtensionIds = @()
    )

    # Reversible self-heal. This only undoes debugger/nativeMessaging blockers
    # that Synapse wrote into Chrome ExtensionSettings, identified strictly by
    # the Synapse blocked_install_message marker. Entries Synapse did not author
    # are left byte-for-byte untouched. Best-effort per hive: a hive we cannot
    # write is reported with ACL evidence, never silently swallowed, and never
    # fatal.
    $preserveIds = @($PreserveExtensionIds | Where-Object {
        $_ -match '^[a-p]{32}$'
    } | Sort-Object -Unique)
    $results = @()
    foreach ($hive in (Get-ChromePolicyHiveCandidates -Hive 'Auto')) {
        $policyRoot = Get-ChromePolicyRoot -Hive $hive
        if (-not (Test-Path -LiteralPath $policyRoot)) {
            $results += [pscustomobject]@{ hive = $hive; path = $policyRoot; changed = $false; reason = 'policy_root_absent' }
            continue
        }
        $raw = (Get-ItemProperty -LiteralPath $policyRoot -Name ExtensionSettings -ErrorAction SilentlyContinue).ExtensionSettings
        if ([string]::IsNullOrWhiteSpace([string]$raw)) {
            $results += [pscustomobject]@{ hive = $hive; path = $policyRoot; changed = $false; reason = 'no_extension_settings' }
            continue
        }
        try {
            $settings = Read-ChromeExtensionSettingsPolicy -Hive $hive
        } catch {
            # A policy we cannot parse is NOT ours to rewrite; surface it loudly
            # and leave it intact rather than risk corrupting admin policy.
            $results += [pscustomobject]@{ hive = $hive; path = $policyRoot; changed = $false; reason = "parse_error:$($_.Exception.Message)" }
            continue
        }
        $changed = $false
        $cleaned = [ordered]@{}
        $removedEntries = @()
        $strippedEntries = @()
        $preservedEntries = @()
        foreach ($name in @($settings.Keys)) {
            $entry = $settings[$name]
            $isSynapseAuthored = ($entry -is [System.Collections.Specialized.OrderedDictionary]) -and
                $entry.Contains('blocked_install_message') -and
                ([string]$entry['blocked_install_message'] -eq $script:SynapseChromeBlockedInstallMessage)
            if (-not $isSynapseAuthored) {
                $cleaned[$name] = $entry
                continue
            }
            if ($preserveIds -contains $name) {
                $cleaned[$name] = $entry
                $preservedEntries += $name
                continue
            }
            $changed = $true
            $blocked = @()
            if ($entry.Contains('blocked_permissions')) {
                $blocked = @($entry['blocked_permissions'] | Where-Object { $_ -ne 'debugger' -and $_ -ne 'nativeMessaging' })
            }
            $entry.Remove('blocked_install_message')
            if ($blocked.Count -gt 0) {
                $entry['blocked_permissions'] = $blocked
            } elseif ($entry.Contains('blocked_permissions')) {
                $entry.Remove('blocked_permissions')
            }
            # Drop the entry entirely only if Synapse's blockers were all it held;
            # otherwise preserve whatever else (e.g. an admin installation_mode).
            if ($entry.Keys.Count -gt 0) {
                $cleaned[$name] = $entry
                $strippedEntries += $name
            } else {
                $removedEntries += $name
            }
        }
        if (-not $changed) {
            $reason = if ($preservedEntries.Count -gt 0) {
                'only_current_synapse_popup_shields_present'
            } else {
                'no_synapse_authored_blocks'
            }
            $results += [pscustomobject]@{
                hive = $hive
                path = $policyRoot
                changed = $false
                reason = $reason
                preserved_entries = @($preservedEntries)
            }
            continue
        }
        try {
            if ($cleaned.Keys.Count -gt 0) {
                $json = ConvertTo-CompressedJson -Value $cleaned
                New-ItemProperty -LiteralPath $policyRoot -Name ExtensionSettings -PropertyType String -Value $json -Force | Out-Null
            } else {
                Remove-ItemProperty -LiteralPath $policyRoot -Name ExtensionSettings -ErrorAction Stop
            }
            $results += [pscustomobject]@{
                hive = $hive
                path = $policyRoot
                changed = $true
                reason = 'removed_synapse_authored_blocks'
                removed_entries = @($removedEntries)
                stripped_entries = @($strippedEntries)
                preserved_entries = @($preservedEntries)
                extension_settings_remaining = ($cleaned.Keys.Count -gt 0)
            }
        } catch {
            $results += [pscustomobject]@{
                hive = $hive
                path = $policyRoot
                changed = $false
                reason = "write_failed:$($_.Exception.Message)"
                acl_detail = (Get-RegistryAclDiagnostic -Path $policyRoot)
            }
        }
    }
    return @($results)
}

function Set-SynapseChromeExternalDebuggerPolicy {
    param(
        [object[]]$Extensions
    )

    $hazards = @($Extensions | Where-Object {
        $id = [string]$_.extension_id
        $id -match '^[a-p]{32}$'
    } | Sort-Object extension_id -Unique)

    $policyRoot = Get-ChromePolicyRoot -Hive 'HKCU'
    if ($hazards.Count -eq 0) {
        return [pscustomobject]@{
            hive = 'HKCU'
            path = $policyRoot
            changed = $false
            reason = 'no_debugger_or_native_hazards'
            shielded_entries = @()
            unchanged_entries = @()
        }
    }

    try {
        if (-not (Test-Path -LiteralPath $policyRoot)) {
            New-Item -Path $policyRoot -Force | Out-Null
        }
        $settings = Read-ChromeExtensionSettingsPolicy -Hive 'HKCU'
    } catch {
        $acl = Get-RegistryAclDiagnostic -Path $policyRoot
        return [pscustomobject]@{
            hive = 'HKCU'
            path = $policyRoot
            changed = $false
            reason = 'external_popup_shield_write_denied_requires_bridge_management'
            warning_code = 'SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_DENIED'
            blocking = $true
            phase = 'read_or_create'
            error = $_.Exception.Message
            acl = $acl
            remediation = 'do not weaken the admin-only HKCU\Software\Policies\Google\Chrome ACL for non-elevated setup; run scripts\synapse-setup.ps1 from an elevated PowerShell only when the optional Chrome ExtensionSettings popup shield must be applied; otherwise the installed Synapse Chrome Bridge must suppress the hazard through chrome.management or normal bridge commands fail closed before touching Chrome'
            shielded_entries = @()
            unchanged_entries = @()
        }
    }
    $changed = $false
    $shieldedEntries = @()
    $unchangedEntries = @()

    foreach ($hazard in $hazards) {
        $id = [string]$hazard.extension_id
        $existing = $null
        if ($settings.Contains($id)) {
            $existing = $settings[$id]
        }
        if (-not ($existing -is [System.Collections.Specialized.OrderedDictionary])) {
            $existing = [ordered]@{}
        }

        $blocked = @()
        if ($existing.Contains('blocked_permissions')) {
            $blocked = @($existing['blocked_permissions'])
        }
        $hazardPermissions = @($hazard.hazard_api | Where-Object {
            $_ -eq 'debugger' -or $_ -eq 'nativeMessaging'
        })
        if ($hazardPermissions.Count -eq 0) {
            $hazardPermissions = @('debugger', 'nativeMessaging')
        }
        $nextBlocked = @($blocked + $hazardPermissions | Where-Object {
            -not [string]::IsNullOrWhiteSpace([string]$_)
        } | Sort-Object -Unique)

        $beforeBlocked = @($blocked | Sort-Object -Unique)
        $beforeMessage = $null
        if ($existing.Contains('blocked_install_message')) {
            $beforeMessage = [string]$existing['blocked_install_message']
        }

        $existing['blocked_permissions'] = $nextBlocked
        $existing['blocked_install_message'] = $script:SynapseChromeBlockedInstallMessage
        $settings[$id] = $existing

        if ((($beforeBlocked -join ',') -ne ($nextBlocked -join ',')) -or
            ($beforeMessage -ne $script:SynapseChromeBlockedInstallMessage)) {
            $changed = $true
            $shieldedEntries += [pscustomobject]@{
                extension_id = $id
                name = [string]$hazard.name
                active_api = @($hazard.active_api)
                granted_api = @($hazard.granted_api)
                manifest_api = @($hazard.manifest_api)
                hazard_api = @($hazard.hazard_api)
                runtime_enabled = [bool]$hazard.runtime_enabled
                source = [string]$hazard.source
            }
        } else {
            $unchangedEntries += $id
        }
    }

    if ($changed) {
        $json = ConvertTo-CompressedJson -Value $settings
        try {
            New-ItemProperty -LiteralPath $policyRoot -Name ExtensionSettings -PropertyType String -Value $json -Force | Out-Null
        } catch {
            $acl = Get-RegistryAclDiagnostic -Path $policyRoot
            return [pscustomobject]@{
                hive = 'HKCU'
                path = $policyRoot
                changed = $false
                reason = 'external_popup_shield_write_denied_requires_bridge_management'
                warning_code = 'SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_DENIED'
                blocking = $true
                phase = 'write_extension_settings'
                error = $_.Exception.Message
                acl = $acl
                remediation = 'do not weaken the admin-only HKCU\Software\Policies\Google\Chrome ACL for non-elevated setup; run scripts\synapse-setup.ps1 from an elevated PowerShell only when the optional Chrome ExtensionSettings popup shield must be applied; otherwise the installed Synapse Chrome Bridge must suppress the hazard through chrome.management or normal bridge commands fail closed before touching Chrome'
                shielded_entries = @($shieldedEntries)
                unchanged_entries = @($unchangedEntries)
            }
        }
    }

    [pscustomobject]@{
        hive = 'HKCU'
        path = $policyRoot
        changed = $changed
        reason = if ($changed) { 'synapse_authored_popup_shield_applied' } else { 'synapse_authored_popup_shield_already_present' }
        shielded_entries = @($shieldedEntries)
        unchanged_entries = @($unchangedEntries)
    }
}

if ($RemoveExternalDebuggerPolicyOnly) {
    $cleanup = Remove-SynapseChromeExternalDebuggerPolicy -PreserveExtensionIds @()
    [pscustomobject]@{
        ok = $true
        mode = 'chrome_policy_cleanup_only'
        chrome_policy_cleanup = $cleanup
    }
    exit 0
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
$extensionSourceDir = Join-Path $repoRoot 'extensions\synapse-chrome-debugger'
$sourceManifestPath = Join-Path $extensionSourceDir 'manifest.json'
if (-not (Test-Path -LiteralPath $sourceManifestPath -PathType Leaf)) {
    throw "SYNAPSE_CHROME_EXTENSION_MANIFEST_MISSING path=$sourceManifestPath"
}
$sourceServiceWorkerPath = Join-Path $extensionSourceDir 'service_worker.js'
if (-not (Test-Path -LiteralPath $sourceServiceWorkerPath -PathType Leaf)) {
    throw "SYNAPSE_CHROME_EXTENSION_SERVICE_WORKER_MISSING path=$sourceServiceWorkerPath"
}
$serviceWorkerSourceRead = Read-SynapseUtf8FileStrict -Path $sourceServiceWorkerPath
$serviceWorkerSource = $serviceWorkerSourceRead.text
if ($serviceWorkerSource -notmatch 'const\s+BRIDGE_BUILD_ID\s*=\s*"([^"]+)";') {
    throw "SYNAPSE_CHROME_EXTENSION_BUILD_ID_MISSING path=$sourceServiceWorkerPath remediation=service_worker.js must expose BRIDGE_BUILD_ID so setup can verify the build deployed into the constant active directory"
}
$bridgeBuildId = [string]$Matches[1]
if ($bridgeBuildId -notmatch '^[A-Za-z0-9._-]+$') {
    throw "SYNAPSE_CHROME_EXTENSION_BUILD_ID_INVALID build_id=$bridgeBuildId remediation=BRIDGE_BUILD_ID must be safe for a Windows directory name"
}
if ($serviceWorkerSource -notmatch 'const\s+BRIDGE_DECLARED_BUILD_SHA256\s*=\s*"([^"]+)";') {
    throw "SYNAPSE_CHROME_EXTENSION_BUILD_SHA_MISSING path=$sourceServiceWorkerPath remediation=service_worker.js must expose BRIDGE_DECLARED_BUILD_SHA256 for declared build metadata; physical integrity comes from service_worker_sha256"
}
$bridgeDeclaredBuildSha256 = [string]$Matches[1]
if ($serviceWorkerSource -notmatch 'const\s+BRIDGE_REGISTER_TOKEN\s*=\s*"";') {
    throw "SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_SOURCE_NOT_EMPTY path=$sourceServiceWorkerPath remediation=repo service_worker.js must keep BRIDGE_REGISTER_TOKEN empty; setup injects the host-local derived credential only into the deployed copy"
}
$bridgeBearerToken = Get-SynapseChromeBridgeBearerToken -Path $TokenPath
$bridgeRegisterToken = Get-SynapseChromeBridgeRegisterToken -BearerToken $bridgeBearerToken
$bridgeRegisterTokenPrepared = New-SynapseChromeBridgeRegisterTokenDeployment `
    -SourceServiceWorkerPath $sourceServiceWorkerPath `
    -RegisterToken $bridgeRegisterToken
if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
    throw "SYNAPSE_CHROME_EXTENSION_STABLE_ROOT_UNAVAILABLE remediation=LOCALAPPDATA is required to deploy the unpacked Chrome bridge to a checkout-independent stable directory"
}
$stableExtensionRoot = Join-Path $env:LOCALAPPDATA 'synapse\chrome-extension'
$extensionDir = Join-Path $stableExtensionRoot 'active'
$localAppDataFull = [System.IO.Path]::GetFullPath($env:LOCALAPPDATA).TrimEnd(
    [char[]]@(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
)
$stableRootFull = [System.IO.Path]::GetFullPath($stableExtensionRoot)
$extensionDirFull = [System.IO.Path]::GetFullPath($extensionDir)
if (-not $extensionDirFull.StartsWith($stableRootFull + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "SYNAPSE_CHROME_EXTENSION_STABLE_PATH_INVALID root=$stableRootFull path=$extensionDirFull"
}
foreach ($existingPath in @(
        $localAppDataFull,
        (Join-Path $localAppDataFull 'synapse'),
        $stableRootFull,
        $extensionDirFull
    )) {
    if (-not (Test-Path -LiteralPath $existingPath)) {
        continue
    }
    $existingItem = Get-Item -LiteralPath $existingPath -Force -ErrorAction Stop
    if (-not $existingItem.PSIsContainer) {
        throw "SYNAPSE_CHROME_EXTENSION_DEPLOY_PATH_NOT_DIRECTORY path=$existingPath remediation=the constant extension deployment chain must contain only real directories"
    }
    if ($existingItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "SYNAPSE_CHROME_EXTENSION_DEPLOY_REPARSE_POINT_FORBIDDEN path=$existingPath attributes=$($existingItem.Attributes) remediation=replace the link/junction with a real local directory before deploying extension bytes"
    }
}
New-Item -ItemType Directory -Force -Path $extensionDirFull | Out-Null
$extensionDirItem = Get-Item -LiteralPath $extensionDirFull -Force -ErrorAction Stop
if (
    -not $extensionDirItem.PSIsContainer -or
    ($extensionDirItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
) {
    throw "SYNAPSE_CHROME_EXTENSION_ACTIVE_DIRECTORY_INVALID path=$extensionDirFull attributes=$($extensionDirItem.Attributes) remediation=the constant active deployment path must be a real local directory, never a file, link, or junction"
}
$expectedDeployedRelativePaths = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
$staticDeployReadback = @()
foreach ($sourceFile in @(Get-ChildItem -LiteralPath $extensionSourceDir -File -Recurse -Force -ErrorAction Stop)) {
    $relativePath = $sourceFile.FullName.Substring($extensionSourceDir.Length).TrimStart(
        [char[]]@(
            [System.IO.Path]::DirectorySeparatorChar,
            [System.IO.Path]::AltDirectorySeparatorChar
        )
    )
    if ([string]::IsNullOrWhiteSpace($relativePath)) {
        throw "SYNAPSE_CHROME_EXTENSION_STATIC_RELATIVE_PATH_EMPTY source=$($sourceFile.FullName) remediation=every extension artifact must resolve below the extension source directory"
    }
    if (-not $expectedDeployedRelativePaths.Add($relativePath)) {
        throw "SYNAPSE_CHROME_EXTENSION_DUPLICATE_RELATIVE_PATH source=$($sourceFile.FullName) relative_path=$relativePath remediation=the bundled extension source must map each case-insensitive Windows relative path exactly once"
    }
    if ($sourceFile.FullName -ieq $sourceServiceWorkerPath) {
        continue
    }
    $destination = [System.IO.Path]::GetFullPath((Join-Path $extensionDirFull $relativePath))
    if (-not $destination.StartsWith($extensionDirFull + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "SYNAPSE_CHROME_EXTENSION_STATIC_PATH_ESCAPE source=$($sourceFile.FullName) destination=$destination active_dir=$extensionDirFull remediation=extension artifacts must remain inside the constant active deployment directory"
    }
    $destinationParent = [System.IO.Path]::GetDirectoryName($destination)
    New-Item -ItemType Directory -Force -Path $destinationParent | Out-Null
    $expectedBytes = [System.IO.File]::ReadAllBytes($sourceFile.FullName)
    $writeReadback = Write-SynapseFileBytesAtomic -Path $destination -ExpectedBytes $expectedBytes
    $staticDeployReadback += [pscustomobject]@{
        relative_path = $relativePath
        source_path = $sourceFile.FullName
        deployed_path = $destination
        byte_length = $writeReadback.byte_length
        sha256 = $writeReadback.sha256
        atomic_replace = $writeReadback.atomic_replace
        atomic_mode = $writeReadback.atomic_mode
        byte_exact_readback = $writeReadback.byte_exact_readback
    }
}
$extensionDir = $extensionDirFull
$manifestPath = Join-Path $extensionDir 'manifest.json'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "SYNAPSE_CHROME_EXTENSION_STABLE_MANIFEST_MISSING source=$sourceManifestPath deployed=$manifestPath"
}
$deployedServiceWorkerPath = Join-Path $extensionDir 'service_worker.js'
$bridgeRegisterTokenWrite = Set-SynapseChromeBridgeRegisterToken `
    -ServiceWorkerPath $deployedServiceWorkerPath `
    -PreparedDeployment $bridgeRegisterTokenPrepared
$bridgeRegisterTokenReadback = Assert-SynapseChromeBridgeRegisterTokenReadback `
    -ServiceWorkerPath $deployedServiceWorkerPath `
    -ExpectedRegisterToken $bridgeRegisterToken `
    -ExpectedServiceWorkerSha256 $bridgeRegisterTokenWrite.expected_sha256 `
    -ExpectedServiceWorkerByteLength $bridgeRegisterTokenWrite.expected_byte_length
$obsoleteDeployCleanup = @()
foreach ($deployedFile in @(Get-ChildItem -LiteralPath $extensionDirFull -File -Recurse -Force -ErrorAction Stop)) {
    $relativePath = $deployedFile.FullName.Substring($extensionDirFull.Length).TrimStart(
        [char[]]@(
            [System.IO.Path]::DirectorySeparatorChar,
            [System.IO.Path]::AltDirectorySeparatorChar
        )
    )
    if ($expectedDeployedRelativePaths.Contains($relativePath)) {
        continue
    }
    $deployedFullPath = [System.IO.Path]::GetFullPath($deployedFile.FullName)
    if (
        -not $deployedFullPath.StartsWith(
            $extensionDirFull + [System.IO.Path]::DirectorySeparatorChar,
            [System.StringComparison]::OrdinalIgnoreCase
        ) -or
        ($deployedFile.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
    ) {
        throw "SYNAPSE_CHROME_EXTENSION_OBSOLETE_FILE_UNSAFE path=$($deployedFile.FullName) active_dir=$extensionDirFull attributes=$($deployedFile.Attributes) remediation=cleanup refuses to follow or remove a path outside the real constant active directory"
    }
    Remove-Item -LiteralPath $deployedFullPath -Force -ErrorAction Stop
    if (Test-Path -LiteralPath $deployedFullPath) {
        throw "SYNAPSE_CHROME_EXTENSION_OBSOLETE_FILE_REMOVE_FAILED path=$deployedFullPath remediation=remove the exact obsolete file after proving it is inside the constant active extension directory"
    }
    $obsoleteDeployCleanup += $relativePath
}
$deployedDirectories = @(Get-ChildItem -LiteralPath $extensionDirFull -Directory -Recurse -Force -ErrorAction Stop |
        Sort-Object { $_.FullName.Length } -Descending)
foreach ($deployedDirectory in $deployedDirectories) {
    if ($deployedDirectory.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "SYNAPSE_CHROME_EXTENSION_DEPLOYED_DIRECTORY_REPARSE_POINT_FORBIDDEN path=$($deployedDirectory.FullName) remediation=the constant active extension tree cannot contain links or junctions"
    }
    if (@(Get-ChildItem -LiteralPath $deployedDirectory.FullName -Force -ErrorAction Stop).Count -eq 0) {
        Remove-Item -LiteralPath $deployedDirectory.FullName -Force -ErrorAction Stop
        if (Test-Path -LiteralPath $deployedDirectory.FullName) {
            throw "SYNAPSE_CHROME_EXTENSION_EMPTY_DIRECTORY_REMOVE_FAILED path=$($deployedDirectory.FullName) remediation=remove the exact empty directory after proving it is inside the constant active extension tree"
        }
    }
}
$extensionManifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
$extensionDeploy = [pscustomobject]@{
    source_dir = $extensionSourceDir
    deployed_dir = $extensionDir
    stable_root = $stableRootFull
    path_contract = 'constant_active_directory'
    static_artifacts = @($staticDeployReadback)
    expected_relative_paths = @($expectedDeployedRelativePaths | Sort-Object)
    obsolete_files_removed = @($obsoleteDeployCleanup)
    build_id = $bridgeBuildId
    declared_build_sha256 = $bridgeDeclaredBuildSha256
    build_sha256 = $bridgeDeclaredBuildSha256
    manifest_sha256 = Get-SynapseFileSha256 -Path $manifestPath
    service_worker_sha256 = Get-SynapseFileSha256 -Path $deployedServiceWorkerPath
    service_worker_source_sha256 = $bridgeRegisterTokenWrite.source_sha256
    service_worker_expected_sha256 = $bridgeRegisterTokenWrite.expected_sha256
    service_worker_byte_exact_readback = $bridgeRegisterTokenWrite.byte_exact_readback
    service_worker_atomic_replace = $bridgeRegisterTokenWrite.atomic_replace
    service_worker_atomic_mode = $bridgeRegisterTokenWrite.atomic_mode
    bridge_register_token_injected = $true
    bridge_register_token_length = $bridgeRegisterTokenReadback.length
    bridge_register_token_sha256 = $bridgeRegisterTokenReadback.sha256
    bridge_register_token_matches_expected = $bridgeRegisterTokenReadback.matches_expected
}
$requiredPermissions = @($extensionManifest.permissions)
$optionalPermissions = @($extensionManifest.optional_permissions)
$hostPermissions = @($extensionManifest.host_permissions)
if ($optionalPermissions -contains 'debugger') {
    throw "SYNAPSE_CHROME_EXTENSION_OPTIONAL_DEBUGGER_PERMISSION_FORBIDDEN path=$manifestPath remediation=the Synapse bridge must request debugger as a required permission so target-scoped CDP input support is deterministic after auto-install"
}
if (-not ($requiredPermissions -contains 'debugger')) {
    throw "SYNAPSE_CHROME_EXTENSION_DEBUGGER_PERMISSION_REQUIRED path=$manifestPath remediation=manual FSV of normal-profile hover/tap/active-tab drag and viewport emulation requires the bundled bridge to expose narrow chrome.debugger lanes for already-open Chrome tabs; inactive-tab drag uses the bundled chrome.scripting synthetic mouse path"
}
if ($requiredPermissions -contains 'nativeMessaging') {
    throw "SYNAPSE_CHROME_EXTENSION_NATIVE_MESSAGING_FORBIDDEN path=$manifestPath remediation=normal end-user bridge must use direct localhost HTTP registration plus WebSocket command delivery; nativeMessaging can launch a visible cmd.exe wrapper on Windows"
}
if ($optionalPermissions -contains 'nativeMessaging') {
    throw "SYNAPSE_CHROME_EXTENSION_OPTIONAL_NATIVE_MESSAGING_FORBIDDEN path=$manifestPath remediation=normal end-user bridge must not request nativeMessaging"
}
if (-not ($requiredPermissions -contains 'management')) {
    throw "SYNAPSE_CHROME_EXTENSION_MANAGEMENT_PERMISSION_REQUIRED path=$manifestPath remediation=normal end-user bridge must request chrome.management so it can disable external debugger/nativeMessaging extensions or fail closed with exact readback before any browser command"
}
if (-not ($requiredPermissions -contains 'alarms')) {
    throw "SYNAPSE_CHROME_EXTENSION_ALARMS_PERMISSION_MISSING path=$manifestPath remediation=normal end-user bridge requires chrome.alarms so an MV3 service worker suspended after daemon restart can wake and re-register without foreground Chrome automation"
}
if (-not ($requiredPermissions -contains 'webNavigation')) {
    throw "SYNAPSE_CHROME_EXTENSION_WEBNAVIGATION_PERMISSION_MISSING path=$manifestPath remediation=normal bridge requires chrome.webNavigation for target-scoped lifecycle and SPA route event readback without debugger attach"
}
if (-not ($requiredPermissions -contains 'webRequest')) {
    throw "SYNAPSE_CHROME_EXTENSION_WEBREQUEST_PERMISSION_MISSING path=$manifestPath remediation=normal bridge requires chrome.webRequest for target-scoped request/response wait event buffering without debugger attach"
}
if (-not ($requiredPermissions -contains 'downloads')) {
    throw "SYNAPSE_CHROME_EXTENSION_DOWNLOADS_PERMISSION_MISSING path=$manifestPath remediation=normal bridge requires chrome.downloads for real download event/readback capture and browser_downloads save/move verification"
}
if (-not ($requiredPermissions -contains 'unlimitedStorage')) {
    throw "SYNAPSE_CHROME_EXTENSION_UNLIMITED_STORAGE_PERMISSION_MISSING path=$manifestPath remediation=the durable MV3 reconnect/mutation ledger must not be evicted or quota-blocked at chrome.storage.local's default limit; declare unlimitedStorage as a required messageless permission"
}
if ($optionalPermissions -contains 'alarms') {
    throw "SYNAPSE_CHROME_EXTENSION_OPTIONAL_ALARMS_PERMISSION_FORBIDDEN path=$manifestPath remediation=alarms must be a required permission for deterministic bridge wake/readback, not an optional runtime prompt"
}
if ($hostPermissions -notcontains 'http://127.0.0.1:7700/*') {
    throw "SYNAPSE_CHROME_EXTENSION_LOCALHOST_PERMISSION_MISSING path=$manifestPath remediation=normal bridge requires host_permissions http://127.0.0.1:7700/* for direct daemon registration and message posting"
}

function Initialize-SynapseChromeBridgeAutoInstallInterop {
    try {
        Add-Type -AssemblyName UIAutomationClient -ErrorAction Stop
        Add-Type -AssemblyName UIAutomationTypes -ErrorAction Stop
    } catch {
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_UIA_UNAVAILABLE error=$($_.Exception.Message) remediation=run from an interactive Windows desktop where UIAutomationClient is available, or load the deployed stable Chrome bridge directory manually once and rerun setup"
    }

    if (-not ('SynapseChromeBridgeAutoInstall.Win32' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace SynapseChromeBridgeAutoInstall {
    public sealed class ForegroundAttachAttemptResult {
        public uint CurrentThread { get; set; }
        public uint TargetThread { get; set; }
        public uint TargetPid { get; set; }
        public uint ForegroundThread { get; set; }
        public uint ForegroundPid { get; set; }
        public bool AttachedTarget { get; set; }
        public bool AttachedForeground { get; set; }
        public bool BringWindowToTopResult { get; set; }
        public bool SetForegroundWindowResult { get; set; }
        public long SetFocusResultHwnd { get; set; }
    }

    public sealed class WindowProcessIdentityResult {
        public bool Valid { get; set; }
        public long Hwnd { get; set; }
        public uint ProcessId { get; set; }
        public ulong ProcessStartedAt100ns { get; set; }
        public string ExecutablePath { get; set; }
        public string ErrorCode { get; set; }
        public int Win32Error { get; set; }
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct FileTime {
        public uint LowDateTime;
        public uint HighDateTime;
    }

    public static class Win32 {
        [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
        [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
        [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
        [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
        [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
        [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
        [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr hWnd);
        [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
        [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
        [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, UIntPtr dwExtraInfo);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern IntPtr OpenProcess(uint processAccess, bool inheritHandle, uint processId);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit, out FileTime kernel, out FileTime user);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] private static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder path, ref uint size);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool CloseHandle(IntPtr handle);

        public static WindowProcessIdentityResult ReadWindowProcessIdentity(IntPtr hwnd) {
            WindowProcessIdentityResult result = new WindowProcessIdentityResult {
                Hwnd = hwnd.ToInt64(),
                ExecutablePath = String.Empty,
                ErrorCode = String.Empty,
            };
            if (hwnd == IntPtr.Zero) {
                result.ErrorCode = "foreground_window_null";
                return result;
            }
            uint pid = 0;
            uint threadId = GetWindowThreadProcessId(hwnd, out pid);
            result.ProcessId = pid;
            if (threadId == 0 || pid == 0) {
                result.ErrorCode = "window_process_owner_unavailable";
                result.Win32Error = Marshal.GetLastWin32Error();
                return result;
            }
            const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
            IntPtr process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
            if (process == IntPtr.Zero) {
                result.ErrorCode = "process_open_failed";
                result.Win32Error = Marshal.GetLastWin32Error();
                return result;
            }
            try {
                FileTime creation;
                FileTime exit;
                FileTime kernel;
                FileTime user;
                if (!GetProcessTimes(process, out creation, out exit, out kernel, out user)) {
                    result.ErrorCode = "process_times_read_failed";
                    result.Win32Error = Marshal.GetLastWin32Error();
                    return result;
                }
                uint capacity = 32768;
                StringBuilder path = new StringBuilder((int)capacity);
                if (!QueryFullProcessImageName(process, 0, path, ref capacity)) {
                    result.ErrorCode = "process_image_path_read_failed";
                    result.Win32Error = Marshal.GetLastWin32Error();
                    return result;
                }
                result.ProcessStartedAt100ns = ((ulong)creation.HighDateTime << 32) | creation.LowDateTime;
                result.ExecutablePath = path.ToString();
            } finally {
                if (!CloseHandle(process) && String.IsNullOrEmpty(result.ErrorCode)) {
                    result.ErrorCode = "process_handle_close_failed";
                    result.Win32Error = Marshal.GetLastWin32Error();
                }
            }
            result.Valid = String.IsNullOrEmpty(result.ErrorCode)
                && result.ProcessId != 0
                && result.ProcessStartedAt100ns != 0
                && !String.IsNullOrWhiteSpace(result.ExecutablePath);
            return result;
        }

        public static ForegroundAttachAttemptResult AttachThreadInputBringToTopSetForeground(IntPtr targetHwnd, IntPtr foregroundHwnd) {
            ForegroundAttachAttemptResult result = new ForegroundAttachAttemptResult();
            uint targetPid = 0;
            result.TargetThread = GetWindowThreadProcessId(targetHwnd, out targetPid);
            result.TargetPid = targetPid;
            if (foregroundHwnd != IntPtr.Zero) {
                uint foregroundPid = 0;
                result.ForegroundThread = GetWindowThreadProcessId(foregroundHwnd, out foregroundPid);
                result.ForegroundPid = foregroundPid;
            }
            result.CurrentThread = GetCurrentThreadId();
            try {
                if (result.TargetThread != 0 && result.TargetThread != result.CurrentThread) {
                    result.AttachedTarget = AttachThreadInput(result.CurrentThread, result.TargetThread, true);
                }
                if (result.ForegroundThread != 0 && result.ForegroundThread != result.CurrentThread && result.ForegroundThread != result.TargetThread) {
                    result.AttachedForeground = AttachThreadInput(result.CurrentThread, result.ForegroundThread, true);
                }
                result.BringWindowToTopResult = BringWindowToTop(targetHwnd);
                result.SetForegroundWindowResult = SetForegroundWindow(targetHwnd);
                result.SetFocusResultHwnd = SetFocus(targetHwnd).ToInt64();
            } finally {
                if (result.AttachedForeground) {
                    AttachThreadInput(result.CurrentThread, result.ForegroundThread, false);
                }
                if (result.AttachedTarget) {
                    AttachThreadInput(result.CurrentThread, result.TargetThread, false);
                }
            }
            return result;
        }
    }
}
'@ -ErrorAction Stop
    }
}

function Get-SynapseActiveChromeProfileName {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [string]$ExtensionId,
        [string]$ExtensionDir
    )

    $localStatePath = Join-Path $ChromeUserDataRoot 'Local State'
    $candidates = New-Object System.Collections.Generic.List[string]
    if (Test-Path -LiteralPath $localStatePath -PathType Leaf) {
        try {
            $localState = Get-Content -Raw -LiteralPath $localStatePath | ConvertFrom-Json -ErrorAction Stop
            if ($localState.profile -and $localState.profile.last_used) {
                $candidates.Add([string]$localState.profile.last_used) | Out-Null
            }
            if ($localState.profile -and $localState.profile.last_active_profiles) {
                foreach ($candidate in @($localState.profile.last_active_profiles)) {
                    $candidates.Add([string]$candidate) | Out-Null
                }
            }
        } catch {
            $candidates.Clear()
        }
    }

    $uniqueCandidates = @($candidates | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -Unique)
    if (-not [string]::IsNullOrWhiteSpace($ExtensionId) -and -not [string]::IsNullOrWhiteSpace($ExtensionDir)) {
        foreach ($candidate in $uniqueCandidates) {
            if (-not (Test-Path -LiteralPath (Join-Path $ChromeUserDataRoot $candidate) -PathType Container)) {
                continue
            }
            $row = Test-SynapseChromeBridgeProfileRow `
                -ChromeUserDataRoot $ChromeUserDataRoot `
                -ProfileName $candidate `
                -ExtensionId $ExtensionId `
                -ExtensionDir $ExtensionDir
            if ($row.installed -and $row.manifest_path_matches) {
                return $candidate
            }
        }

        $installedProfiles = @()
        foreach ($profileDir in @(Get-ChildItem -LiteralPath $ChromeUserDataRoot -Directory -ErrorAction SilentlyContinue)) {
            if ([string]$profileDir.Name -eq 'Snapshots') {
                continue
            }
            $row = Test-SynapseChromeBridgeProfileRow `
                -ChromeUserDataRoot $ChromeUserDataRoot `
                -ProfileName $profileDir.Name `
                -ExtensionId $ExtensionId `
                -ExtensionDir $ExtensionDir
            if ($row.installed -and $row.manifest_path_matches) {
                $installedProfiles += [string]$profileDir.Name
            }
        }
        $installedProfiles = @($installedProfiles | Sort-Object -Unique)
        if ($installedProfiles.Count -eq 1) {
            return [string]$installedProfiles[0]
        }
    }

    foreach ($candidate in $uniqueCandidates) {
        if (Test-Path -LiteralPath (Join-Path $ChromeUserDataRoot $candidate) -PathType Container) {
            return $candidate
        }
    }
    return $null
}

function Get-SynapseChromeBridgeManifestApiPermissions {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ExtensionDir
    )

    $manifestPath = Join-Path $ExtensionDir 'manifest.json'
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        return @()
    }
    try {
        $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json -ErrorAction Stop
    } catch {
        return @()
    }
    if (-not $manifest.permissions) {
        return @()
    }
    @($manifest.permissions | ForEach-Object { [string]$_ } | Where-Object {
            -not [string]::IsNullOrWhiteSpace($_) -and
            $_ -notmatch '://' -and
            $_ -ne '<all_urls>'
        } | Sort-Object -Unique)
}

function Test-SynapseChromeBridgeProfileRow {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [string]$ProfileName,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionId,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionDir
    )

    $profilePath = Join-Path $ChromeUserDataRoot $ProfileName
    foreach ($prefFileName in @('Preferences', 'Secure Preferences')) {
        $prefPath = Join-Path $profilePath $prefFileName
        if (-not (Test-Path -LiteralPath $prefPath -PathType Leaf)) {
            continue
        }
        try {
            $pref = Get-Content -Raw -LiteralPath $prefPath | ConvertFrom-Json -ErrorAction Stop
        } catch {
            continue
        }
        if (-not $pref.extensions -or -not $pref.extensions.settings) {
            continue
        }
        $property = $pref.extensions.settings.PSObject.Properties[$ExtensionId]
        if (-not $property) {
            continue
        }
        $setting = $property.Value
        $manifestPath = if ($setting.PSObject.Properties.Name -contains 'path') { [string]$setting.path } else { $null }
        $manifestMatches = $false
        if (-not [string]::IsNullOrWhiteSpace($manifestPath)) {
            try {
                $manifestMatches = ((Resolve-Path -LiteralPath $manifestPath -ErrorAction Stop).Path -eq (Resolve-Path -LiteralPath $ExtensionDir -ErrorAction Stop).Path)
            } catch {
                $manifestMatches = ($manifestPath -ieq $ExtensionDir)
            }
        }
        $activeApiPermissions = @()
        if ($setting.PSObject.Properties.Name -contains 'active_permissions' -and $setting.active_permissions -and $setting.active_permissions.api) {
            $activeApiPermissions = @($setting.active_permissions.api | ForEach-Object { [string]$_ } | Sort-Object -Unique)
        }
        $grantedApiPermissions = @()
        if ($setting.PSObject.Properties.Name -contains 'granted_permissions' -and $setting.granted_permissions -and $setting.granted_permissions.api) {
            $grantedApiPermissions = @($setting.granted_permissions.api | ForEach-Object { [string]$_ } | Sort-Object -Unique)
        }
        $disableReasons = @()
        if ($setting.PSObject.Properties.Name -contains 'disable_reasons' -and $null -ne $setting.disable_reasons) {
            $disableReasons = @($setting.disable_reasons | ForEach-Object { [int]$_ })
        }
        $requiredApiPermissions = Get-SynapseChromeBridgeManifestApiPermissions -ExtensionDir $ExtensionDir
        $missingActiveApiPermissions = @($requiredApiPermissions | Where-Object {
                $activeApiPermissions -notcontains $_
            })
        # enabled_and_permissioned is PATH-INDEPENDENT: it proves the loaded extension
        # is enabled (no disable_reasons) and holds every required API permission,
        # without requiring its recorded load path to equal the latest stable dir. The
        # Synapse bridge extension ID is derived from the manifest key and is therefore
        # the same regardless of which directory Chrome loaded it from, so a non-stable
        # path does not make a working bridge any less the Synapse bridge.
        $enabledAndPermissioned = ($disableReasons.Count -eq 0) -and
            ($missingActiveApiPermissions.Count -eq 0)
        $enabled = ($disableReasons.Count -eq 0)
        # manifest_dir_exists confirms the directory Chrome loaded the unpacked
        # extension from is still present on disk with a manifest. If a prior install's
        # directory was deleted, the row is stale and a genuine reinstall is required.
        $manifestDirExists = (-not [string]::IsNullOrWhiteSpace($manifestPath)) -and
            (Test-Path -LiteralPath $manifestPath -PathType Container) -and
            (Test-Path -LiteralPath (Join-Path $manifestPath 'manifest.json') -PathType Leaf)
        $ready = $manifestMatches -and $enabledAndPermissioned
        $permissionActivationPending = $manifestMatches -and $manifestDirExists -and
            $enabled -and ($missingActiveApiPermissions.Count -gt 0)
        return [pscustomobject]@{
            installed = $true
            profile = $ProfileName
            pref_file = $prefFileName
            pref_path = $prefPath
            manifest_path = $manifestPath
            manifest_path_matches = $manifestMatches
            manifest_dir_exists = $manifestDirExists
            active_api_permissions = $activeApiPermissions
            granted_api_permissions = $grantedApiPermissions
            disable_reasons = $disableReasons
            required_api_permissions = $requiredApiPermissions
            missing_active_api_permissions = $missingActiveApiPermissions
            enabled = $enabled
            enabled_and_permissioned = $enabledAndPermissioned
            permission_activation_pending = $permissionActivationPending
            ready = $ready
        }
    }

    [pscustomobject]@{
        installed = $false
        profile = $ProfileName
        pref_file = $null
        pref_path = $null
        manifest_path = $null
        manifest_path_matches = $false
        manifest_dir_exists = $false
        active_api_permissions = @()
        granted_api_permissions = @()
        disable_reasons = @()
        required_api_permissions = Get-SynapseChromeBridgeManifestApiPermissions -ExtensionDir $ExtensionDir
        missing_active_api_permissions = Get-SynapseChromeBridgeManifestApiPermissions -ExtensionDir $ExtensionDir
        enabled = $false
        enabled_and_permissioned = $false
        permission_activation_pending = $false
        ready = $false
    }
}

function ConvertTo-SynapseComparablePath {
    param(
        [AllowNull()]
        [string]$Path
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $null
    }
    $expanded = [System.Environment]::ExpandEnvironmentVariables($Path.Trim())
    try {
        $full = [System.IO.Path]::GetFullPath($expanded)
    } catch {
        $full = $expanded
    }
    $trimChars = [char[]]@([System.IO.Path]::DirectorySeparatorChar, [System.IO.Path]::AltDirectorySeparatorChar)
    return $full.TrimEnd($trimChars)
}

function Get-SynapseChromeBridgeReferencedManifestPaths {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionId
    )

    $paths = @()
    if (-not (Test-Path -LiteralPath $ChromeUserDataRoot -PathType Container)) {
        throw "SYNAPSE_CHROME_EXTENSION_REFERENCE_ROOT_MISSING user_data_root=$ChromeUserDataRoot remediation=cleanup refuses to classify any deployed build as unreferenced without the physical Chrome profile root"
    }
    try {
        $profileDirs = @(Get-ChildItem -LiteralPath $ChromeUserDataRoot -Directory -ErrorAction Stop)
    } catch {
        throw "SYNAPSE_CHROME_EXTENSION_REFERENCE_PROFILE_ENUMERATION_FAILED user_data_root=$ChromeUserDataRoot error=$($_.Exception.Message) remediation=cleanup refuses to delete any build until every Chrome profile directory can be enumerated"
    }
    foreach ($profileDir in $profileDirs) {
        if ([string]$profileDir.Name -eq 'Snapshots') {
            continue
        }
        $profileRowCount = 0
        $profilePaths = @()
        foreach ($prefFileName in @('Preferences', 'Secure Preferences')) {
            $prefPath = Join-Path $profileDir.FullName $prefFileName
            if (-not (Test-Path -LiteralPath $prefPath -PathType Leaf)) {
                continue
            }
            try {
                $pref = Get-Content -Raw -LiteralPath $prefPath -ErrorAction Stop |
                    ConvertFrom-Json -ErrorAction Stop
            } catch {
                throw "SYNAPSE_CHROME_EXTENSION_REFERENCE_PROFILE_READ_FAILED profile=$($profileDir.Name) pref_file=$prefFileName path=$prefPath error=$($_.Exception.Message) remediation=cleanup refuses to delete any build while a Chrome profile source of truth is unreadable or invalid JSON"
            }
            if (-not $pref.extensions -or -not $pref.extensions.settings) {
                continue
            }
            $property = $pref.extensions.settings.PSObject.Properties[$ExtensionId]
            if (-not $property) {
                continue
            }
            $profileRowCount += 1
            $setting = $property.Value
            if ($setting.PSObject.Properties.Name -notcontains 'path') {
                continue
            }
            $manifestPath = ConvertTo-SynapseComparablePath -Path ([string]$setting.path)
            if (-not [string]::IsNullOrWhiteSpace($manifestPath)) {
                $profilePaths += $manifestPath
            }
        }
        $profilePaths = @($profilePaths | Sort-Object -Unique)
        if ($profileRowCount -gt 0 -and $profilePaths.Count -eq 0) {
            throw "SYNAPSE_CHROME_EXTENSION_REFERENCE_PATH_MISSING profile=$($profileDir.Name) extension_id=$ExtensionId row_count=$profileRowCount remediation=Chrome has extension metadata without a resolvable unpacked path; migrate the active profile to the constant active directory before deleting any deployed build"
        }
        $paths += $profilePaths
    }
    @($paths | Sort-Object -Unique)
}

function Remove-SynapseStaleChromeBridgeBuildDirs {
    param(
        [Parameter(Mandatory = $true)]
        [string]$StableRoot,
        [Parameter(Mandatory = $true)]
        [string]$CurrentExtensionDir,
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionId
    )

    if (-not (Test-Path -LiteralPath $StableRoot -PathType Container)) {
        return [pscustomobject]@{
            attempted = $false
            reason = 'stable_root_missing'
            stable_root = $StableRoot
            current_extension_dir = $CurrentExtensionDir
            referenced_paths = @()
            preserved_dirs = @()
            removed_dirs = @()
            failed_dirs = @()
        }
    }

    $current = ConvertTo-SynapseComparablePath -Path $CurrentExtensionDir
    $referencedPaths = @(Get-SynapseChromeBridgeReferencedManifestPaths `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -ExtensionId $ExtensionId)
    $preserved = @()
    $removed = @()
    $failed = @()

    try {
        $candidateDirs = @(Get-ChildItem -LiteralPath $StableRoot -Directory -ErrorAction Stop)
    } catch {
        throw "SYNAPSE_CHROME_STALE_BUILD_ENUMERATION_FAILED stable_root=$StableRoot error=$($_.Exception.Message) remediation=stale-build cleanup refuses to mutate disk without an exact directory inventory"
    }
    foreach ($dir in $candidateDirs) {
        $dirPath = ConvertTo-SynapseComparablePath -Path $dir.FullName
        if ($dirPath -ieq $current) {
            $preserved += [pscustomobject]@{ path = $dir.FullName; reason = 'current_active_directory' }
            continue
        }
        if ([string]$dir.Name -notlike 'synapse-chrome-bridge-*') {
            continue
        }
        $stableRootComparable = ConvertTo-SynapseComparablePath -Path $StableRoot
        if (
            -not $dirPath.StartsWith(
                $stableRootComparable + [System.IO.Path]::DirectorySeparatorChar,
                [System.StringComparison]::OrdinalIgnoreCase
            ) -or
            ($dir.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
        ) {
            $failed += [pscustomobject]@{
                path = $dir.FullName
                error = 'candidate_path_outside_stable_root_or_reparse_point'
            }
            continue
        }
        if (@($referencedPaths | Where-Object { $_ -ieq $dirPath }).Count -gt 0) {
            $preserved += [pscustomobject]@{ path = $dir.FullName; reason = 'referenced_by_chrome_profile' }
            continue
        }
        try {
            Remove-Item -LiteralPath $dir.FullName -Recurse -Force -ErrorAction Stop
            if (Test-Path -LiteralPath $dir.FullName) {
                throw 'directory_still_exists_after_remove'
            }
            $removed += $dir.FullName
        } catch {
            $failed += [pscustomobject]@{ path = $dir.FullName; error = $_.Exception.Message }
        }
    }

    if ($failed.Count -gt 0) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            stable_root = $StableRoot
            current_extension_dir = $CurrentExtensionDir
            referenced_paths = @($referencedPaths)
            preserved_dirs = @($preserved)
            removed_dirs = @($removed)
            failed_dirs = @($failed)
        }) -Depth 10
        throw "SYNAPSE_CHROME_STALE_BUILD_CLEANUP_FAILED detail=$detail remediation=inspect every failed path and remove the exact stale non-reparse build directory only after proving no Chrome profile references it"
    }

    [pscustomobject]@{
        attempted = $true
        reason = 'removed_unreferenced_stale_build_dirs'
        stable_root = $StableRoot
        current_extension_dir = $CurrentExtensionDir
        referenced_paths = @($referencedPaths)
        preserved_dirs = @($preserved)
        removed_dirs = @($removed)
        failed_dirs = @($failed)
    }
}

function Get-SynapseCommandLineSwitchValue {
    param(
        [AllowNull()]
        [string]$CommandLine,
        [Parameter(Mandatory = $true)]
        [string]$SwitchName
    )

    if ([string]::IsNullOrWhiteSpace($CommandLine)) {
        return $null
    }
    $pattern = '(?i)(?:^|\s)' + [regex]::Escape($SwitchName) + '(?:=|\s+)(?:"([^"]+)"|''([^'']+)''|([^\s"]+))'
    $match = [regex]::Match($CommandLine, $pattern)
    if (-not $match.Success) {
        return $null
    }
    foreach ($index in 1..3) {
        if ($match.Groups[$index].Success -and -not [string]::IsNullOrWhiteSpace($match.Groups[$index].Value)) {
            return $match.Groups[$index].Value
        }
    }
    return $null
}

function Get-SynapseChromeProcessProfileMatch {
    param(
        [AllowNull()]
        [string]$CommandLine,
        [AllowNull()]
        [string]$ChromeUserDataRoot
    )

    $commandLineReadable = -not [string]::IsNullOrWhiteSpace($CommandLine)
    $userDataDir = Get-SynapseCommandLineSwitchValue -CommandLine $CommandLine -SwitchName '--user-data-dir'
    $normalizedUserDataDir = ConvertTo-SynapseComparablePath -Path $userDataDir
    $expectedRoot = ConvertTo-SynapseComparablePath -Path $ChromeUserDataRoot
    $hasMsPlaywrightMcpDir = $commandLineReadable -and $CommandLine -match 'ms-playwright-mcp'
    $hasRemoteDebuggingPipe = $commandLineReadable -and $CommandLine -match '(^|\s)--remote-debugging-pipe(\s|=|$)'
    $hasRemoteDebuggingPort = $commandLineReadable -and $CommandLine -match '(^|\s)--remote-debugging-port(\s|=|$)'
    $hasAutomationControlledFlag = $commandLineReadable -and $CommandLine -match '(^|\s)--disable-blink-features=([^\s]*,)?AutomationControlled(,|\s|$)'

    $matches = $true
    $reason = 'not_checked'
    if (-not [string]::IsNullOrWhiteSpace($expectedRoot)) {
        if (-not $commandLineReadable) {
            $matches = $false
            $reason = 'command_line_unreadable'
        } elseif ($hasMsPlaywrightMcpDir) {
            $matches = $false
            $reason = 'dedicated_ms_playwright_mcp_user_data_dir'
        } elseif ([string]::IsNullOrWhiteSpace($normalizedUserDataDir)) {
            $matches = $true
            $reason = 'default_chrome_user_data_root'
        } elseif ([string]::Equals($normalizedUserDataDir, $expectedRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            $matches = $true
            $reason = 'user_data_root_matches_active_profile_root'
        } else {
            $matches = $false
            $reason = 'user_data_root_mismatch'
        }
    }

    [pscustomobject]@{
        command_line_readable = $commandLineReadable
        user_data_dir = $userDataDir
        user_data_dir_normalized = $normalizedUserDataDir
        expected_user_data_root = $expectedRoot
        matches_active_profile_root = $matches
        match_reason = $reason
        has_ms_playwright_mcp_dir = $hasMsPlaywrightMcpDir
        has_remote_debugging_pipe = $hasRemoteDebuggingPipe
        has_remote_debugging_port = $hasRemoteDebuggingPort
        has_automation_controlled_flag = $hasAutomationControlledFlag
    }
}

function Format-SynapseChromeWindowRejectSummary {
    param(
        [object[]]$Windows
    )

    $rows = @($Windows | Select-Object -First 8 | ForEach-Object {
        'hwnd={0},pid={1},title="{2}",eligible={3},reason={4},user_data_dir="{5}",remote_pipe={6},remote_port={7},ms_playwright_mcp={8}' -f `
            $_.hwnd,
            $_.pid,
            (([string]$_.title) -replace '"', '\"'),
            $_.chrome_profile_eligible,
            $_.chrome_profile_match_reason,
            (([string]$_.chrome_user_data_dir) -replace '"', '\"'),
            $_.has_remote_debugging_pipe,
            $_.has_remote_debugging_port,
            $_.has_ms_playwright_mcp_dir
    })
    $extra = [Math]::Max(0, @($Windows).Count - $rows.Count)
    $suffix = if ($extra -gt 0) { " | +$extra more" } else { '' }
    return (($rows -join ' | ') + $suffix)
}

function Get-SynapseChromeTopLevelWindows {
    param(
        [AllowNull()]
        [string]$ChromeUserDataRoot
    )

    Initialize-SynapseChromeBridgeAutoInstallInterop
    $chromeProcessRows = @(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" -ErrorAction SilentlyContinue |
        ForEach-Object {
            $commandLine = [string]$_.CommandLine
            $profileMatch = Get-SynapseChromeProcessProfileMatch -CommandLine $commandLine -ChromeUserDataRoot $ChromeUserDataRoot
            [pscustomobject]@{
                pid = [int]$_.ProcessId
                command_line_readable = $profileMatch.command_line_readable
                chrome_user_data_dir = $profileMatch.user_data_dir
                chrome_user_data_dir_normalized = $profileMatch.user_data_dir_normalized
                chrome_profile_eligible = $profileMatch.matches_active_profile_root
                chrome_profile_match_reason = $profileMatch.match_reason
                has_ms_playwright_mcp_dir = $profileMatch.has_ms_playwright_mcp_dir
                has_remote_debugging_pipe = $profileMatch.has_remote_debugging_pipe
                has_remote_debugging_port = $profileMatch.has_remote_debugging_port
                has_automation_controlled_flag = $profileMatch.has_automation_controlled_flag
            }
        })
    if ($chromeProcessRows.Count -eq 0) {
        return @()
    }
    $chromeProcesses = @($chromeProcessRows | ForEach-Object { [int]$_.pid })
    $chromeProcessByPid = @{}
    foreach ($row in $chromeProcessRows) {
        $chromeProcessByPid[[int]$row.pid] = $row
    }
    $foreground = [SynapseChromeBridgeAutoInstall.Win32]::GetForegroundWindow().ToInt64()
    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $children = $root.FindAll(
        [System.Windows.Automation.TreeScope]::Children,
        [System.Windows.Automation.Condition]::TrueCondition
    )
    $windows = @()
    foreach ($child in $children) {
        $current = $child.Current
        $processId = [int]$current.ProcessId
        if ($chromeProcesses -notcontains $processId) {
            continue
        }
        $hwnd = [int64]$current.NativeWindowHandle
        $title = [string]$current.Name
        if ($hwnd -eq 0 -or [string]::IsNullOrWhiteSpace($title)) {
            continue
        }
        $windowIdentity = [SynapseChromeBridgeAutoInstall.Win32]::ReadWindowProcessIdentity([IntPtr]$hwnd)
        $processInfo = $chromeProcessByPid[$processId]
        $windows += [pscustomobject]@{
            hwnd = $hwnd
            pid = $processId
            process_started_at_100ns = [uint64]$windowIdentity.ProcessStartedAt100ns
            executable_path = [string]$windowIdentity.ExecutablePath
            identity_valid = [bool]$windowIdentity.Valid
            identity_error = [string]$windowIdentity.ErrorCode
            identity_win32_error = [int]$windowIdentity.Win32Error
            title = $title
            class_name = [string]$current.ClassName
            is_foreground = ($hwnd -eq $foreground)
            command_line_readable = $processInfo.command_line_readable
            chrome_user_data_dir = $processInfo.chrome_user_data_dir
            chrome_user_data_dir_normalized = $processInfo.chrome_user_data_dir_normalized
            chrome_profile_eligible = $processInfo.chrome_profile_eligible
            chrome_profile_match_reason = $processInfo.chrome_profile_match_reason
            has_ms_playwright_mcp_dir = $processInfo.has_ms_playwright_mcp_dir
            has_remote_debugging_pipe = $processInfo.has_remote_debugging_pipe
            has_remote_debugging_port = $processInfo.has_remote_debugging_port
            has_automation_controlled_flag = $processInfo.has_automation_controlled_flag
            element = $child
        }
    }
    return @($windows)
}

function Find-SynapseAutomationElementByName {
    param(
        [Parameter(Mandatory = $true)]
        [System.Windows.Automation.AutomationElement]$Root,
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [AllowNull()]
        [System.Windows.Automation.ControlType]$ControlType
    )

    $nameCondition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::NameProperty,
        $Name
    )
    $condition = $nameCondition
    if ($null -ne $ControlType) {
        $typeCondition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            $ControlType
        )
        $condition = [System.Windows.Automation.AndCondition]::new($nameCondition, $typeCondition)
    }
    $Root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
}

function Find-SynapseAutomationElementByAutomationId {
    param(
        [Parameter(Mandatory = $true)]
        [System.Windows.Automation.AutomationElement]$Root,
        [Parameter(Mandatory = $true)]
        [string]$AutomationId,
        [AllowNull()]
        [System.Windows.Automation.ControlType]$ControlType
    )

    $idCondition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::AutomationIdProperty,
        $AutomationId
    )
    $condition = $idCondition
    if ($null -ne $ControlType) {
        $typeCondition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            $ControlType
        )
        $condition = [System.Windows.Automation.AndCondition]::new($idCondition, $typeCondition)
    }
    $Root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
}

function Invoke-SynapseAutomationElement {
    param(
        [Parameter(Mandatory = $true)]
        [System.Windows.Automation.AutomationElement]$Element,
        [Parameter(Mandatory = $true)]
        [string]$Description
    )

    try {
        $invoke = $Element.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $invoke.Invoke()
        return
    } catch {
        try {
            $toggle = $Element.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
            $toggle.Toggle()
            return
        } catch {
            throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_INVOKE_FAILED control=$Description error=$($_.Exception.Message)"
        }
    }
}

function Invoke-SynapseAutomationElementMouseClick {
    param(
        [Parameter(Mandatory = $true)]
        [System.Windows.Automation.AutomationElement]$Element,
        [Parameter(Mandatory = $true)]
        [string]$Description
    )

    $rect = $Element.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) {
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_MOUSE_CLICK_FAILED control=$Description reason=empty_bounding_rect"
    }
    $x = [int]($rect.Left + ($rect.Width / 2))
    $y = [int]($rect.Top + ($rect.Height / 2))
    [SynapseChromeBridgeAutoInstall.Win32]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 80
    [SynapseChromeBridgeAutoInstall.Win32]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 120
    [SynapseChromeBridgeAutoInstall.Win32]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
}

function Set-SynapseAutomationEditValue {
    param(
        [Parameter(Mandatory = $true)]
        [System.Windows.Automation.AutomationElement]$Element,
        [Parameter(Mandatory = $true)]
        [string]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Description
    )

    try {
        $valuePattern = $Element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
        $valuePattern.SetValue($Value)
    } catch {
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_SET_FIELD_FAILED field=$Description error=$($_.Exception.Message)"
    }
}

function Read-SynapseForegroundWindow {
    Initialize-SynapseChromeBridgeAutoInstallInterop
    $foregroundHwnd = [SynapseChromeBridgeAutoInstall.Win32]::GetForegroundWindow().ToInt64()
    $nativeIdentity = [SynapseChromeBridgeAutoInstall.Win32]::ReadWindowProcessIdentity([IntPtr]$foregroundHwnd)
    if ($foregroundHwnd -eq 0) {
        return [pscustomobject]@{
            hwnd = 0
            pid = 0
            process_started_at_100ns = 0
            executable_path = ''
            identity_valid = $false
            identity_error = [string]$nativeIdentity.ErrorCode
            identity_win32_error = [int]$nativeIdentity.Win32Error
            title = '<none>'
            class_name = '<none>'
        }
    }

    try {
        $root = [System.Windows.Automation.AutomationElement]::RootElement
        $children = $root.FindAll(
            [System.Windows.Automation.TreeScope]::Children,
            [System.Windows.Automation.Condition]::TrueCondition
        )
        foreach ($child in $children) {
            $current = $child.Current
            if ([int64]$current.NativeWindowHandle -ne $foregroundHwnd) {
                continue
            }
            return [pscustomobject]@{
                hwnd = $foregroundHwnd
                pid = [int]$nativeIdentity.ProcessId
                process_started_at_100ns = [uint64]$nativeIdentity.ProcessStartedAt100ns
                executable_path = [string]$nativeIdentity.ExecutablePath
                identity_valid = [bool]$nativeIdentity.Valid
                identity_error = [string]$nativeIdentity.ErrorCode
                identity_win32_error = [int]$nativeIdentity.Win32Error
                title = [string]$current.Name
                class_name = [string]$current.ClassName
            }
        }
    } catch {
        return [pscustomobject]@{
            hwnd = $foregroundHwnd
            pid = [int]$nativeIdentity.ProcessId
            process_started_at_100ns = [uint64]$nativeIdentity.ProcessStartedAt100ns
            executable_path = [string]$nativeIdentity.ExecutablePath
            identity_valid = [bool]$nativeIdentity.Valid
            identity_error = [string]$nativeIdentity.ErrorCode
            identity_win32_error = [int]$nativeIdentity.Win32Error
            title = '<uia-read-failed>'
            class_name = $_.Exception.Message
        }
    }

    [pscustomobject]@{
        hwnd = $foregroundHwnd
        pid = [int]$nativeIdentity.ProcessId
        process_started_at_100ns = [uint64]$nativeIdentity.ProcessStartedAt100ns
        executable_path = [string]$nativeIdentity.ExecutablePath
        identity_valid = [bool]$nativeIdentity.Valid
        identity_error = [string]$nativeIdentity.ErrorCode
        identity_win32_error = [int]$nativeIdentity.Win32Error
        title = '<not-found>'
        class_name = '<not-found>'
    }
}

function Test-SynapseForegroundIdentityExact {
    param(
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)]$Actual
    )
    return (
        $Expected.identity_valid -eq $true -and
        $Actual.identity_valid -eq $true -and
        [int64]$Expected.hwnd -eq [int64]$Actual.hwnd -and
        [int]$Expected.pid -eq [int]$Actual.pid -and
        [uint64]$Expected.process_started_at_100ns -eq [uint64]$Actual.process_started_at_100ns -and
        [string]$Expected.executable_path -ieq [string]$Actual.executable_path
    )
}

function ConvertTo-SynapseForegroundIdentityEvidence {
    param([Parameter(Mandatory = $true)]$Identity)
    [pscustomobject]@{
        hwnd = [int64]$Identity.hwnd
        pid = [int]$Identity.pid
        process_started_at_100ns = [uint64]$Identity.process_started_at_100ns
        executable_path = [string]$Identity.executable_path
        identity_valid = [bool]$Identity.identity_valid
        identity_error = [string]$Identity.identity_error
        identity_win32_error = [int]$Identity.identity_win32_error
    }
}

function Read-SynapseWindowProcessIdentity {
    param([Parameter(Mandatory = $true)][int64]$Hwnd)
    Initialize-SynapseChromeBridgeAutoInstallInterop
    $identity = [SynapseChromeBridgeAutoInstall.Win32]::ReadWindowProcessIdentity([IntPtr]$Hwnd)
    [pscustomobject]@{
        hwnd = [int64]$Hwnd
        pid = [int]$identity.ProcessId
        process_started_at_100ns = [uint64]$identity.ProcessStartedAt100ns
        executable_path = [string]$identity.ExecutablePath
        identity_valid = [bool]$identity.Valid
        identity_error = [string]$identity.ErrorCode
        identity_win32_error = [int]$identity.Win32Error
    }
}

function Read-SynapseChromeAddressBarValue {
    param(
        [Parameter(Mandatory = $true)]
        $Window
    )

    try {
        $addressBar = Find-SynapseAutomationElementByName `
            -Root $Window.element `
            -Name 'Address and search bar' `
            -ControlType ([System.Windows.Automation.ControlType]::Edit)
        if (-not $addressBar) {
            return [pscustomobject]@{
                present = $false
                value = $null
                error = 'address_bar_not_found'
            }
        }
        $valuePattern = $addressBar.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
        [pscustomobject]@{
            present = $true
            value = [string]$valuePattern.Current.Value
            error = $null
        }
    } catch {
        [pscustomobject]@{
            present = $false
            value = $null
            error = $_.Exception.Message
        }
    }
}

function Read-SynapseChromeNavigationState {
    param(
        [Parameter(Mandatory = $true)]
        [int64]$Hwnd,
        [AllowNull()]
        [string]$ChromeUserDataRoot
    )

    $windows = @(Get-SynapseChromeTopLevelWindows -ChromeUserDataRoot $ChromeUserDataRoot | Where-Object {
        $_.hwnd -eq $Hwnd
    } | Select-Object -First 1)
    if ($windows.Count -eq 0) {
        return [pscustomobject]@{
            hwnd = $Hwnd
            pid = 0
            title = '<missing>'
            address_bar_present = $false
            address_bar_value = $null
            address_bar_error = 'window_not_found'
        }
    }

    $address = Read-SynapseChromeAddressBarValue -Window $windows[0]
    [pscustomobject]@{
        hwnd = [int64]$windows[0].hwnd
        pid = [int]$windows[0].pid
        title = [string]$windows[0].title
        address_bar_present = [bool]$address.present
        address_bar_value = $address.value
        address_bar_error = $address.error
    }
}

function Wait-SynapseChromeForegroundReadback {
    param(
        [Parameter(Mandatory = $true)]
        $TargetIdentity,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline,
        [Parameter(Mandatory = $true)]
        [int]$MaxWaitMilliseconds
    )

    $waitDeadline = (Get-Date).AddMilliseconds($MaxWaitMilliseconds)
    if ($waitDeadline -gt $Deadline) {
        $waitDeadline = $Deadline
    }
    do {
        $foregroundNow = Read-SynapseForegroundWindow
        if (Test-SynapseForegroundIdentityExact -Expected $TargetIdentity -Actual $foregroundNow) {
            return $foregroundNow
        }
        if ((Get-Date) -ge $waitDeadline) {
            return $null
        }
        Start-Sleep -Milliseconds 100
    } while ($true)
}

function Wait-SynapseChromeForegroundAcquisition {
    param(
        [Parameter(Mandatory = $true)]
        $Window,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline
    )

    $stage = 'initializing'
    try {
        $stage = 'convert_target_hwnd'
        $targetHwnd = [IntPtr]$Window.hwnd
        $targetIdentity = [pscustomobject]@{
            hwnd = [int64]$Window.hwnd
            pid = [int]$Window.pid
            process_started_at_100ns = [uint64]$Window.process_started_at_100ns
            executable_path = [string]$Window.executable_path
            identity_valid = [bool]$Window.identity_valid
        }
        if ($targetIdentity.identity_valid -ne $true -or
            $targetIdentity.pid -le 0 -or
            $targetIdentity.process_started_at_100ns -eq 0 -or
            [string]::IsNullOrWhiteSpace($targetIdentity.executable_path)) {
            throw 'target_exact_process_identity_invalid'
        }
        $foregroundBefore = Read-SynapseForegroundWindow
        if ($foregroundBefore.identity_valid -ne $true) {
            throw 'prior_foreground_exact_process_identity_invalid'
        }
        $attempts = New-Object System.Collections.Generic.List[object]
        $stage = 'show_window_async'
        $showResult = [SynapseChromeBridgeAutoInstall.Win32]::ShowWindowAsync($targetHwnd, 9)

        $stage = 'initial_set_foreground'
        $initialSetForeground = [SynapseChromeBridgeAutoInstall.Win32]::SetForegroundWindow($targetHwnd)
        $stage = 'record_initial_attempt'
        $attempts.Add([pscustomobject]@{
            method = 'show_window_async_set_foreground'
            show_window_async_result = [bool]$showResult
            set_foreground_window_result = [bool]$initialSetForeground
            foreground_after = Read-SynapseForegroundWindow
        }) | Out-Null
        $stage = 'wait_initial_foreground'
        $acquired = Wait-SynapseChromeForegroundReadback -TargetIdentity $targetIdentity -Deadline $Deadline -MaxWaitMilliseconds 900
        if ($acquired) {
            return [pscustomobject]@{
                acquired = $true
                method = 'show_window_async_set_foreground'
                show_window_async_result = [bool]$showResult
                initial_set_foreground_window_result = [bool]$initialSetForeground
                attempts = @($attempts.ToArray())
                foreground = $acquired
            }
        }

        $stage = 'read_foreground_before_alt_unlock'
        $foregroundBeforeAlt = Read-SynapseForegroundWindow
        if ($foregroundBeforeAlt.identity_valid -ne $true) {
            throw 'foreground_before_alt_unlock_identity_invalid'
        }
        if (-not (Test-SynapseForegroundIdentityExact -Expected $foregroundBefore -Actual $foregroundBeforeAlt)) {
            return [pscustomobject]@{
                acquired = $false
                operator_superseded = $true
                method = 'none_operator_superseded'
                show_window_async_result = [bool]$showResult
                initial_set_foreground_window_result = [bool]$initialSetForeground
                alt_unlock_attempted = $false
                alt_unlock_set_foreground_window_result = $null
                attempts = @($attempts.ToArray())
                foreground = $null
                final_foreground = $foregroundBeforeAlt
            }
        }

        $stage = 'alt_unlock_key'
        Send-SynapseNativeAltUnlock
        Start-Sleep -Milliseconds 80
        $stage = 'read_foreground_before_alt_retry'
        $foregroundBeforeAltRetry = Read-SynapseForegroundWindow
        if ($foregroundBeforeAltRetry.identity_valid -ne $true) {
            throw 'foreground_before_alt_retry_identity_invalid'
        }
        if (-not (Test-SynapseForegroundIdentityExact -Expected $foregroundBefore -Actual $foregroundBeforeAltRetry)) {
            return [pscustomobject]@{
                acquired = $false
                operator_superseded = $true
                method = 'none_operator_superseded'
                show_window_async_result = [bool]$showResult
                initial_set_foreground_window_result = [bool]$initialSetForeground
                alt_unlock_attempted = $true
                alt_unlock_set_foreground_window_result = $null
                attempts = @($attempts.ToArray())
                foreground = $null
                final_foreground = $foregroundBeforeAltRetry
            }
        }
        $stage = 'alt_unlock_set_foreground'
        $altUnlockSetForeground = [SynapseChromeBridgeAutoInstall.Win32]::SetForegroundWindow($targetHwnd)
        $stage = 'record_alt_attempt'
        $attempts.Add([pscustomobject]@{
            method = 'alt_key_foreground_unlock_set_foreground'
            set_foreground_window_result = [bool]$altUnlockSetForeground
            foreground_after = Read-SynapseForegroundWindow
        }) | Out-Null
        $stage = 'wait_alt_foreground'
        $acquired = Wait-SynapseChromeForegroundReadback -TargetIdentity $targetIdentity -Deadline $Deadline -MaxWaitMilliseconds 1500
        if ($acquired) {
            return [pscustomobject]@{
                acquired = $true
                method = 'alt_key_foreground_unlock_set_foreground'
                show_window_async_result = [bool]$showResult
                initial_set_foreground_window_result = [bool]$initialSetForeground
                alt_unlock_attempted = $true
                alt_unlock_set_foreground_window_result = [bool]$altUnlockSetForeground
                attempts = @($attempts.ToArray())
                foreground = $acquired
            }
        }

        [pscustomobject]@{
            acquired = $false
            method = 'unacquired'
            show_window_async_result = [bool]$showResult
            initial_set_foreground_window_result = [bool]$initialSetForeground
            alt_unlock_attempted = $true
            alt_unlock_set_foreground_window_result = [bool]$altUnlockSetForeground
            attempts = @($attempts.ToArray())
            foreground = $null
        }
    } catch {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            stage = $stage
            exception_type = $_.Exception.GetType().FullName
            error = $_.Exception.Message
            inner_error = if ($_.Exception.InnerException) { $_.Exception.InnerException.Message } else { $null }
            invocation = if ($_.InvocationInfo) { $_.InvocationInfo.PositionMessage } else { $null }
            script_stack_trace = $_.ScriptStackTrace
            target_hwnd = $Window.hwnd
            target_hwnd_type = if ($null -ne $Window.hwnd) { $Window.hwnd.GetType().FullName } else { '<null>' }
            target_pid = $Window.pid
            target_title = $Window.title
            foreground_now = Read-SynapseForegroundWindow
        }) -Depth 8
        throw "SYNAPSE_CHROME_FOREGROUND_ACQUISITION_FAILED detail=$detail remediation=fix the typed Win32 foreground acquisition stage named in detail; setup refuses to send Chrome navigation keys without a separately verified foreground readback"
    }
}

function Invoke-SynapseChromeAddressBarNavigation {
    param(
        [Parameter(Mandatory = $true)]
        $Window,
        [Parameter(Mandatory = $true)]
        $ForegroundLease,
        [AllowNull()]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [string]$Url,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedTitlePattern,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline,
        [Parameter(Mandatory = $true)]
        [string]$Purpose
    )

    $before = Read-SynapseChromeNavigationState -Hwnd $Window.hwnd -ChromeUserDataRoot $ChromeUserDataRoot
    $foregroundBefore = Read-SynapseForegroundWindow
    $leasedChrome = $ForegroundLease.acquired_chrome_foreground
    if (-not $leasedChrome -or $leasedChrome.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            purpose = $Purpose
            foreground_lease = $ForegroundLease
            foreground_now = $foregroundBefore
        }) -Depth 8
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_LEASE_INVALID_BEFORE_NAVIGATION detail=$detail remediation=maintenance navigation requires the exact acquired Chrome HWND/PID/process-start/image identity; no new foreground acquisition is attempted inside an existing transaction"
    }
    if (-not (Test-SynapseForegroundIdentityExact -Expected $leasedChrome -Actual $foregroundBefore)) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            outcome = 'operator_superseded'
            operator_superseded = $true
            purpose = $Purpose
            acquired_chrome_foreground = $leasedChrome
            current_foreground = $foregroundBefore
            navigation_before = $before
        }) -Depth 8
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_OPERATOR_SUPERSEDED_BEFORE_NAVIGATION detail=$detail remediation=the operator selected another exact foreground window after Chrome maintenance began; cleanup will preserve that window and the operation must be retried explicitly"
    }
    $foregroundReadback = [pscustomobject]@{
        acquired = $true
        method = 'existing_maintenance_foreground_lease'
        show_window_async_result = $false
        initial_set_foreground_window_result = $false
        alt_unlock_attempted = $false
        alt_unlock_set_foreground_window_result = $null
        attempts = @()
        foreground = $foregroundBefore
    }
    $foregroundAcquired = $foregroundReadback.foreground

    $foregroundWindow = Get-SynapseChromeWindowByHwnd `
        -Hwnd $Window.hwnd `
        -ChromeUserDataRoot $ChromeUserDataRoot
    if (-not $foregroundWindow) {
        throw "SYNAPSE_CHROME_NAVIGATION_WINDOW_LOST_AFTER_FOREGROUND hwnd=$($Window.hwnd) expected_pid=$($Window.pid) purpose=$Purpose remediation=the exact Chrome window disappeared after foreground acquisition; setup refuses navigation in another window"
    }
    if (
        [int]$foregroundWindow.pid -ne [int]$Window.pid -or
        $foregroundWindow.chrome_profile_eligible -ne $true -or
        [string]$foregroundWindow.chrome_user_data_dir_normalized -cne [string]$Window.chrome_user_data_dir_normalized
    ) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            purpose = $Purpose
            expected_hwnd = $Window.hwnd
            expected_pid = $Window.pid
            expected_user_data_dir = $Window.chrome_user_data_dir_normalized
            actual_hwnd = $foregroundWindow.hwnd
            actual_pid = $foregroundWindow.pid
            actual_user_data_dir = $foregroundWindow.chrome_user_data_dir_normalized
            actual_profile_eligible = $foregroundWindow.chrome_profile_eligible
            actual_profile_match_reason = $foregroundWindow.chrome_profile_match_reason
        }) -Depth 8
        throw "SYNAPSE_CHROME_NAVIGATION_WINDOW_IDENTITY_CHANGED_AFTER_FOREGROUND detail=$detail remediation=the foreground HWND must still belong to the exact eligible Chrome PID and user-data root selected before navigation"
    }
    $Window = $foregroundWindow
    $addressBar = Find-SynapseAutomationElementByName `
        -Root $Window.element `
        -Name 'Address and search bar' `
        -ControlType ([System.Windows.Automation.ControlType]::Edit)
    if (-not $addressBar) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            purpose = $Purpose
            target_url = $Url
            chrome_window_hwnd = $Window.hwnd
            chrome_window_pid = $Window.pid
            foreground_acquired = $foregroundAcquired
            navigation_before = $before
        }) -Depth 8
        throw "SYNAPSE_CHROME_NAVIGATION_ADDRESS_BAR_NOT_FOUND detail=$detail remediation=the selected Chrome window did not expose its address edit through UI Automation; setup refuses navigation without an exact element target"
    }

    try {
        $valuePattern = $addressBar.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
        if ($valuePattern.Current.IsReadOnly) {
            throw 'address_bar_value_pattern_read_only'
        }
        # ValuePattern targets this exact UIA element. Global Ctrl+A/Ctrl+V and
        # clipboard mutation let concurrent operator typing redirect the URL
        # even though foreground ownership had just been verified (#2035).
        # Set the value locally, then read it back before committing navigation.
        $valuePattern.SetValue($Url)
        $fieldValueAfterSet = [string]$valuePattern.Current.Value
        if ($fieldValueAfterSet.Trim().TrimEnd('/') -ine $Url.Trim().TrimEnd('/')) {
            throw "address_bar_value_readback_mismatch_after_set actual=$fieldValueAfterSet expected=$Url"
        }
        $addressBar.SetFocus()
        if (-not $addressBar.Current.HasKeyboardFocus) {
            throw 'address_bar_keyboard_focus_not_observed'
        }
        $foregroundAtCommit = Read-SynapseForegroundWindow
        $fieldValueAtCommit = [string]$valuePattern.Current.Value
        if (
            [int64]$foregroundAtCommit.hwnd -ne [int64]$Window.hwnd -or
            [int]$foregroundAtCommit.pid -ne [int]$Window.pid -or
            -not $addressBar.Current.HasKeyboardFocus -or
            $fieldValueAtCommit.Trim().TrimEnd('/') -ine $Url.Trim().TrimEnd('/')
        ) {
            throw "address_bar_commit_identity_changed foreground_hwnd=$($foregroundAtCommit.hwnd) foreground_pid=$($foregroundAtCommit.pid) keyboard_focus=$($addressBar.Current.HasKeyboardFocus) actual_value=$fieldValueAtCommit expected_value=$Url"
        }
        # Chrome's omnibox exposes ValuePattern and TextPattern, but no
        # InvokePattern. Enter is the only global input left in this flow; it is
        # emitted only after the exact HWND/PID/focus/value boundary readback.
        Send-SynapseNativeKeyTap -VirtualKey 0x0D
    } catch {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            purpose = $Purpose
            target_url = $Url
            chrome_window_hwnd = $Window.hwnd
            chrome_window_pid = $Window.pid
            foreground_acquired = $foregroundAcquired
            navigation_before = $before
            error = $_.Exception.Message
        }) -Depth 8
        throw "SYNAPSE_CHROME_NAVIGATION_EXACT_UIA_INPUT_FAILED detail=$detail remediation=the exact Chrome omnibox must expose a writable ValuePattern, retain the requested value, retain keyboard focus, and remain in the exact foreground HWND/PID through the navigation commit boundary"
    }

    # Chrome normalises what the omnibox displays for internal pages: navigating to
    # chrome://extensions/?id=<id>&synapse_maintenance_token=<token> leaves the
    # address bar reading exactly "chrome://extensions". Requiring the omnibox to
    # echo a query string back is therefore not a durable confirmation signal — it
    # can never hold for a chrome:// URL. Accept the query/fragment-stripped form
    # as well, and compensate for the weaker address evidence by additionally
    # requiring the document title to match, which the caller supplies and which
    # was previously recorded but never actually checked.
    $expectedAddresses = New-Object System.Collections.Generic.List[string]
    $expectedAddresses.Add($Url.Trim().TrimEnd('/')) | Out-Null
    $strippedUrl = ($Url -split '[?#]')[0]
    if (-not [string]::IsNullOrWhiteSpace($strippedUrl)) {
        $strippedUrl = $strippedUrl.Trim().TrimEnd('/')
        if (-not $expectedAddresses.Contains($strippedUrl)) {
            $expectedAddresses.Add($strippedUrl) | Out-Null
        }
    }
    $after = Wait-SynapseUntil -Deadline $Deadline -SleepMilliseconds 250 -Probe {
        $state = Read-SynapseChromeNavigationState -Hwnd $Window.hwnd -ChromeUserDataRoot $ChromeUserDataRoot
        $addressMatches = $false
        if (-not [string]::IsNullOrWhiteSpace([string]$state.address_bar_value)) {
            $actualAddress = ([string]$state.address_bar_value).Trim().TrimEnd('/')
            foreach ($expectedAddress in $expectedAddresses) {
                if ($actualAddress -ieq $expectedAddress) {
                    $addressMatches = $true
                    break
                }
            }
        }
        $titleMatches = ([string]$state.title) -match $ExpectedTitlePattern
        if ($addressMatches -and $titleMatches) {
            return $state
        }
        return $null
    }
    if (-not $after) {
        $latest = Read-SynapseChromeNavigationState -Hwnd $Window.hwnd -ChromeUserDataRoot $ChromeUserDataRoot
        $foregroundAfter = Read-SynapseForegroundWindow
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            purpose = $Purpose
            target_url = $Url
            expected_title_pattern = $ExpectedTitlePattern
            accepted_addresses = @($expectedAddresses)
            observed_address = ([string]$latest.address_bar_value).Trim().TrimEnd('/')
            observed_title = [string]$latest.title
            observed_title_matches = (([string]$latest.title) -match $ExpectedTitlePattern)
            chrome_window_hwnd = $Window.hwnd
            chrome_window_pid = $Window.pid
            foreground_before = $foregroundBefore
            foreground_acquired = $foregroundAcquired
            foreground_after = $foregroundAfter
            navigation_before = $before
            navigation_after = $latest
        }) -Depth 8
        throw "SYNAPSE_CHROME_NAVIGATION_NOT_CONFIRMED detail=$detail remediation=Chrome did not visibly reach the requested internal page after exact UIA value assignment and verified commit; compare observed_address against accepted_addresses and observed_title against expected_title_pattern; setup refuses to search extension controls on a stale page"
    }

    [pscustomobject]@{
        method = 'foreground_uia_value_set_enter_exact_readback'
        purpose = $Purpose
        target_url = $Url
        expected_title_pattern = $ExpectedTitlePattern
        show_window_async_result = [bool]$foregroundReadback.show_window_async_result
        set_foreground_window_result = [bool]$foregroundReadback.initial_set_foreground_window_result
        foreground_acquisition = $foregroundReadback
        foreground_before = $foregroundBefore
        foreground_acquired = $foregroundAcquired
        before = $before
        after = $after
    }
}

function Send-SynapseNativeKeyDown {
    param([byte]$VirtualKey)
    [SynapseChromeBridgeAutoInstall.Win32]::keybd_event($VirtualKey, 0, 0, [UIntPtr]::Zero)
}

function Send-SynapseNativeKeyUp {
    param([byte]$VirtualKey)
    [SynapseChromeBridgeAutoInstall.Win32]::keybd_event($VirtualKey, 0, 2, [UIntPtr]::Zero)
}

function Send-SynapseNativeKeyTap {
    param([byte]$VirtualKey)
    Send-SynapseNativeKeyDown -VirtualKey $VirtualKey
    Start-Sleep -Milliseconds 60
    Send-SynapseNativeKeyUp -VirtualKey $VirtualKey
}

function Send-SynapseNativeAltUnlock {
    $pressed = $false
    try {
        Send-SynapseNativeKeyDown -VirtualKey 0x12
        $pressed = $true
    } finally {
        # The release is a finally obligation. A stranded ALT key changes every
        # later keystroke into a menu accelerator and is never acceptable.
        Send-SynapseNativeKeyUp -VirtualKey 0x12
        if (-not $pressed) {
            # A redundant release is safe and makes the pre-down failure path
            # explicit in the physical input stream as well.
            Send-SynapseNativeKeyUp -VirtualKey 0x12
        }
    }
}

function Wait-SynapseForegroundIdentityReadback {
    param(
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)][datetime]$Deadline,
        [int]$MaxWaitMilliseconds = 1200
    )
    $waitDeadline = (Get-Date).AddMilliseconds($MaxWaitMilliseconds)
    if ($waitDeadline -gt $Deadline) {
        $waitDeadline = $Deadline
    }
    do {
        $current = Read-SynapseForegroundWindow
        if (Test-SynapseForegroundIdentityExact -Expected $Expected -Actual $current) {
            return $current
        }
        if ((Get-Date) -ge $waitDeadline) {
            return $null
        }
        Start-Sleep -Milliseconds 50
    } while ($true)
}

function Restore-SynapseChromeMaintenanceForeground {
    param(
        [Parameter(Mandatory = $true)]$ForegroundLease,
        [Parameter(Mandatory = $true)][datetime]$Deadline
    )

    $prior = $ForegroundLease.prior_foreground
    $chrome = $ForegroundLease.acquired_chrome_foreground
    if (-not $prior -or -not $chrome -or $prior.identity_valid -ne $true -or $chrome.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
        }) -Depth 8
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_LEASE_INVALID detail=$detail remediation=maintenance must capture exact HWND/PID/process-start/image identities before and after Chrome foreground acquisition"
    }

    $current = Read-SynapseForegroundWindow
    if ($current.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{ current = $current }) -Depth 6
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_READ_FAILED phase=before_restore detail=$detail remediation=GetForegroundWindow and its exact process identity must be readable before cleanup may change focus"
    }
    $required = -not (Test-SynapseForegroundIdentityExact -Expected $prior -Actual $chrome)
    if (Test-SynapseForegroundIdentityExact -Expected $prior -Actual $current) {
        return [pscustomobject]@{
            required = $required
            attempted = $false
            restored = $true
            operator_superseded = $false
            outcome = if ($required) { 'already_restored' } else { 'chrome_already_foreground_noop' }
            restore_method = 'none_already_current'
            initial_set_foreground_window_result = $null
            alt_unlock_attempted = $false
            alt_unlock_set_foreground_window_result = $null
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $current
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $current
        }
    }
    if (-not (Test-SynapseForegroundIdentityExact -Expected $chrome -Actual $current)) {
        return [pscustomobject]@{
            required = $required
            attempted = $false
            restored = $false
            operator_superseded = $true
            outcome = 'operator_superseded'
            restore_method = 'none_operator_superseded'
            initial_set_foreground_window_result = $null
            alt_unlock_attempted = $false
            alt_unlock_set_foreground_window_result = $null
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $current
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $current
        }
    }

    $priorNow = Read-SynapseWindowProcessIdentity -Hwnd ([int64]$prior.hwnd)
    if ($priorNow.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{ expected = $prior; actual = $priorNow }) -Depth 8
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_TARGET_MISSING detail=$detail remediation=the exact pre-operation window/process no longer exists; cleanup refuses to focus any substitute"
    }
    if (-not (Test-SynapseForegroundIdentityExact -Expected $prior -Actual $priorNow)) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{ expected = $prior; actual = $priorNow }) -Depth 8
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_IDENTITY_CHANGED detail=$detail remediation=the prior HWND/PID/process-start/image identity was reused or replaced; cleanup refuses to focus the unrelated window"
    }

    # Re-read immediately before the focus mutation. A human-selected third
    # window wins this race and is never overwritten.
    $commitCurrent = Read-SynapseForegroundWindow
    if ($commitCurrent.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{ current = $commitCurrent }) -Depth 6
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_READ_FAILED phase=commit detail=$detail remediation=repair foreground identity readback; cleanup refuses a focus write without current authority"
    }
    if (-not (Test-SynapseForegroundIdentityExact -Expected $chrome -Actual $commitCurrent)) {
        return [pscustomobject]@{
            required = $required
            attempted = $false
            restored = $false
            operator_superseded = $true
            outcome = 'operator_superseded_during_restore'
            restore_method = 'none_operator_superseded'
            initial_set_foreground_window_result = $null
            alt_unlock_attempted = $false
            alt_unlock_set_foreground_window_result = $null
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $commitCurrent
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $commitCurrent
        }
    }

    $initialSet = [SynapseChromeBridgeAutoInstall.Win32]::SetForegroundWindow([IntPtr]$prior.hwnd)
    $restored = Wait-SynapseForegroundIdentityReadback -Expected $prior -Deadline $Deadline -MaxWaitMilliseconds 900
    if ($restored) {
        return [pscustomobject]@{
            required = $required
            attempted = $true
            restored = $true
            operator_superseded = $false
            outcome = 'restored'
            restore_method = 'set_foreground_window'
            initial_set_foreground_window_result = [bool]$initialSet
            alt_unlock_attempted = $false
            alt_unlock_set_foreground_window_result = $null
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $commitCurrent
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $restored
        }
    }

    $afterInitial = Read-SynapseForegroundWindow
    if ($afterInitial.identity_valid -eq $true -and -not (Test-SynapseForegroundIdentityExact -Expected $chrome -Actual $afterInitial)) {
        $operatorSuperseded = -not (Test-SynapseForegroundIdentityExact -Expected $prior -Actual $afterInitial)
        return [pscustomobject]@{
            required = $required
            attempted = $true
            restored = -not $operatorSuperseded
            operator_superseded = $operatorSuperseded
            outcome = if ($operatorSuperseded) { 'operator_superseded_during_restore' } else { 'restored_after_false_return' }
            restore_method = if ($operatorSuperseded) { 'none_operator_superseded' } else { 'set_foreground_window_readback' }
            initial_set_foreground_window_result = [bool]$initialSet
            alt_unlock_attempted = $false
            alt_unlock_set_foreground_window_result = $null
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $commitCurrent
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $afterInitial
        }
    }
    if ($afterInitial.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{ after_initial = $afterInitial; initial_result = [bool]$initialSet }) -Depth 6
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_READ_FAILED phase=after_initial_set detail=$detail remediation=repair physical GetForegroundWindow readback before retrying maintenance"
    }

    Send-SynapseNativeAltUnlock
    Start-Sleep -Milliseconds 80
    $beforeAltRetry = Read-SynapseForegroundWindow
    if ($beforeAltRetry.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{ before_alt_retry = $beforeAltRetry }) -Depth 6
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_READ_FAILED phase=before_alt_retry detail=$detail remediation=repair physical foreground readback; no retry was issued"
    }
    if (-not (Test-SynapseForegroundIdentityExact -Expected $chrome -Actual $beforeAltRetry)) {
        $operatorSuperseded = -not (Test-SynapseForegroundIdentityExact -Expected $prior -Actual $beforeAltRetry)
        return [pscustomobject]@{
            required = $required
            attempted = $true
            restored = -not $operatorSuperseded
            operator_superseded = $operatorSuperseded
            outcome = if ($operatorSuperseded) { 'operator_superseded_during_restore' } else { 'restored_after_alt_unlock' }
            restore_method = if ($operatorSuperseded) { 'none_operator_superseded' } else { 'alt_unlock_readback' }
            initial_set_foreground_window_result = [bool]$initialSet
            alt_unlock_attempted = $true
            alt_unlock_set_foreground_window_result = $null
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $commitCurrent
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $beforeAltRetry
        }
    }
    $altSet = [SynapseChromeBridgeAutoInstall.Win32]::SetForegroundWindow([IntPtr]$prior.hwnd)
    $restoredAfterAlt = Wait-SynapseForegroundIdentityReadback -Expected $prior -Deadline $Deadline -MaxWaitMilliseconds 1500
    if ($restoredAfterAlt) {
        return [pscustomobject]@{
            required = $required
            attempted = $true
            restored = $true
            operator_superseded = $false
            outcome = 'restored'
            restore_method = 'alt_unlock_set_foreground_window'
            initial_set_foreground_window_result = [bool]$initialSet
            alt_unlock_attempted = $true
            alt_unlock_set_foreground_window_result = [bool]$altSet
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $commitCurrent
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $restoredAfterAlt
        }
    }
    $final = Read-SynapseForegroundWindow
    if ($final.identity_valid -eq $true -and -not (Test-SynapseForegroundIdentityExact -Expected $chrome -Actual $final)) {
        $operatorSuperseded = -not (Test-SynapseForegroundIdentityExact -Expected $prior -Actual $final)
        return [pscustomobject]@{
            required = $required
            attempted = $true
            restored = -not $operatorSuperseded
            operator_superseded = $operatorSuperseded
            outcome = if ($operatorSuperseded) { 'operator_superseded_during_restore' } else { 'restored_after_false_alt_return' }
            restore_method = if ($operatorSuperseded) { 'none_operator_superseded' } else { 'alt_unlock_set_foreground_window_readback' }
            initial_set_foreground_window_result = [bool]$initialSet
            alt_unlock_attempted = $true
            alt_unlock_set_foreground_window_result = [bool]$altSet
            prior_foreground = $prior
            acquired_chrome_foreground = $chrome
            current_before_restore = ConvertTo-SynapseForegroundIdentityEvidence -Identity $commitCurrent
            final_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $final
        }
    }
    $detail = ConvertTo-CompressedJson -Value ([ordered]@{
        prior = $prior
        acquired_chrome = $chrome
        current_before_restore = $commitCurrent
        final = $final
        initial_set_foreground_window_result = [bool]$initialSet
        alt_unlock_set_foreground_window_result = [bool]$altSet
    }) -Depth 8
    throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_REFUSED detail=$detail remediation=Windows refused both exact foreground restoration attempts while the operation still owned Chrome; inspect the named HWND/process identities and foreground-lock policy, then retry"
}

function Wait-SynapseUntil {
    param(
        [Parameter(Mandatory = $true)]
        [scriptblock]$Probe,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline,
        [int]$SleepMilliseconds = 250
    )

    do {
        $value = & $Probe
        if ($value) {
            return $value
        }
        Start-Sleep -Milliseconds $SleepMilliseconds
    } while ((Get-Date) -lt $Deadline)
    return $null
}

function Find-SynapseChromeFolderDialog {
    param(
        [Parameter(Mandatory = $true)]
        [int64]$ChromeWindowHwnd,
        [Parameter(Mandatory = $true)]
        [int]$ChromeWindowPid
    )

    $chromeWindows = @(Get-SynapseChromeTopLevelWindows)
    foreach ($window in $chromeWindows) {
        if ($window.title -match '^Select the extension directory\.?$' -and $window.pid -eq $ChromeWindowPid) {
            return $window
        }
        if ($window.hwnd -ne $ChromeWindowHwnd) {
            continue
        }
        $dialog = Find-SynapseAutomationElementByName `
            -Root $window.element `
            -Name 'Select the extension directory.' `
            -ControlType ([System.Windows.Automation.ControlType]::Window)
        if ($dialog) {
            $current = $dialog.Current
            return [pscustomobject]@{
                hwnd = [int64]$current.NativeWindowHandle
                pid = $window.pid
                title = [string]$current.Name
                class_name = [string]$current.ClassName
                is_foreground = $false
                element = $dialog
            }
        }
    }
    return $null
}

function Get-SynapseChromeWindowByHwnd {
    param(
        [Parameter(Mandatory = $true)]
        [int64]$Hwnd,
        [AllowNull()]
        [string]$ChromeUserDataRoot
    )

    @(Get-SynapseChromeTopLevelWindows -ChromeUserDataRoot $ChromeUserDataRoot | Where-Object { $_.hwnd -eq $Hwnd } | Select-Object -First 1)[0]
}

function Read-SynapseChromeExtensionDetailsUiState {
    param(
        [Parameter(Mandatory = $true)]
        $Window
    )

    $reloadButton = Find-SynapseAutomationElementByAutomationId `
        -Root $Window.element `
        -AutomationId 'dev-reload-button' `
        -ControlType ([System.Windows.Automation.ControlType]::Button)
    $enableToggle = Find-SynapseAutomationElementByAutomationId `
        -Root $Window.element `
        -AutomationId 'enableToggle' `
        -ControlType ([System.Windows.Automation.ControlType]::Button)
    $serviceWorker = Find-SynapseAutomationElementByName `
        -Root $Window.element `
        -Name 'service worker' `
        -ControlType ([System.Windows.Automation.ControlType]::Hyperlink)
    $extensionName = Find-SynapseAutomationElementByName `
        -Root $Window.element `
        -Name 'Synapse Chrome Bridge' `
        -ControlType $null

    $enableToggleName = if ($enableToggle) { [string]$enableToggle.Current.Name } else { '<missing>' }
    [pscustomobject]@{
        hwnd = [int64]$Window.hwnd
        pid = [int]$Window.pid
        title = [string]$Window.title
        class_name = [string]$Window.class_name
        reload_button_present = ($null -ne $reloadButton)
        enable_toggle_present = ($null -ne $enableToggle)
        enable_toggle_name = $enableToggleName
        enable_toggle_on = ($enableToggleName -match '^On\b|extension enabled')
        service_worker_link_present = ($null -ne $serviceWorker)
        extension_name_present = ($null -ne $extensionName)
    }
}

# A Chrome window opened without --profile-directory after a non-clean exit is a
# ProfilePickerView, not a browser window. It legitimately has no tab strip, so
# reporting "tab strip unreadable" sends the operator to investigate Chrome
# accessibility internals when the required action is simply to pick a profile.
# Classification matters as much as detection (AGENTS.md), so this is read
# explicitly rather than inferred from a zero tab count.
function Read-SynapseChromeProfilePickerState {
    param(
        [Parameter(Mandatory = $true)]
        $Window
    )

    $present = $false
    $rootViewName = $null
    $profileNames = @()
    try {
        $pickerCondition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ClassNameProperty,
            'ProfilePickerView'
        )
        $picker = $Window.element.FindFirst(
            [System.Windows.Automation.TreeScope]::Descendants,
            $pickerCondition
        )
        $rootCondition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ClassNameProperty,
            'RootView'
        )
        $rootView = $Window.element.FindFirst(
            [System.Windows.Automation.TreeScope]::Descendants,
            $rootCondition
        )
        if ($rootView) {
            $rootViewName = [string]$rootView.Current.Name
        }
        # Chrome localises the picker heading, so the class name is the primary
        # witness and the heading only corroborates it.
        $present = ($null -ne $picker) -or ($rootViewName -match "^Who's using Chrome\?$")
        if ($present) {
            $buttonCondition = [System.Windows.Automation.PropertyCondition]::new(
                [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
                [System.Windows.Automation.ControlType]::Button
            )
            $buttons = $Window.element.FindAll(
                [System.Windows.Automation.TreeScope]::Descendants,
                $buttonCondition
            )
            foreach ($button in $buttons) {
                $name = [string]$button.Current.Name
                if (-not [string]::IsNullOrWhiteSpace($name)) {
                    $profileNames += $name
                }
            }
        }
    } catch {
        return [pscustomobject]@{
            present = $false
            root_view_name = $rootViewName
            profile_buttons = @()
            error = $_.Exception.Message
        }
    }
    [pscustomobject]@{
        present = $present
        root_view_name = $rootViewName
        profile_buttons = @($profileNames)
        error = $null
    }
}

function Read-SynapseChromeTabStripState {
    param(
        [Parameter(Mandatory = $true)]
        $Window
    )

    $tabContainerCondition = [System.Windows.Automation.AndCondition]::new(
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Tab
        ),
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ClassNameProperty,
            'TabContainerImpl'
        )
    )
    $tabItemCondition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::TabItem
    )
    try {
        $containers = $Window.element.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            $tabContainerCondition
        )
        if ($containers.Count -ne 1) {
            throw "chrome_tab_container_count=$($containers.Count)"
        }
        $items = $containers[0].FindAll(
            [System.Windows.Automation.TreeScope]::Children,
            $tabItemCondition
        )
    } catch {
        $tabStripError = $_.Exception.Message
        $picker = Read-SynapseChromeProfilePickerState -Window $Window
        if ($picker.present) {
            $detail = ConvertTo-CompressedJson -Value ([ordered]@{
                hwnd = $Window.hwnd
                pid = $Window.pid
                window_title = [string]$Window.title
                window_class_name = [string]$Window.class_name
                root_view_name = $picker.root_view_name
                offered_profiles = @($picker.profile_buttons)
                tab_strip_error = $tabStripError
            }) -Depth 8
            throw "SYNAPSE_CHROME_MAINTENANCE_PROFILE_PICKER_ONLY detail=$detail remediation=this Chrome window is the profile picker, not a browser window, so it correctly has no tab strip; relaunch chrome.exe with --profile-directory set to the profile that has the Synapse bridge extension installed, or pick that profile in the open picker, then retry"
        }
        throw "SYNAPSE_CHROME_MAINTENANCE_TABSTRIP_READ_FAILED hwnd=$($Window.hwnd) pid=$($Window.pid) error=$tabStripError profile_picker_present=false remediation=Chrome must expose one exact TabContainerImpl browser tab strip; page-content ARIA tabs are intentionally excluded"
    }
    $rows = @()
    foreach ($item in $items) {
        $runtimeId = '<unavailable>'
        try {
            $runtimeId = (@($item.GetRuntimeId()) -join '.')
        } catch {
            $runtimeId = "<error:$($_.Exception.Message)>"
        }
        $selected = $null
        try {
            $selection = $item.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
            $selected = [bool]$selection.Current.IsSelected
        } catch {
            $selected = $null
        }
        $rows += [pscustomobject]@{
            runtime_id = $runtimeId
            automation_id = [string]$item.Current.AutomationId
            name = [string]$item.Current.Name
            selected = $selected
        }
    }
    [pscustomobject]@{
        hwnd = [int64]$Window.hwnd
        pid = [int]$Window.pid
        count = $rows.Count
        tabs = @($rows)
    }
}

function Enter-SynapseOwnedChromeMaintenanceTab {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline
    )

    Initialize-SynapseChromeBridgeAutoInstallInterop
    $candidateWindows = @(Get-SynapseChromeTopLevelWindows -ChromeUserDataRoot $ChromeUserDataRoot | Where-Object {
        $_.title -notmatch '^Select the extension directory\.?$'
    })
    $eligibleWindows = @($candidateWindows | Where-Object { $_.chrome_profile_eligible -eq $true })
    if ($eligibleWindows.Count -eq 0) {
        $rejected = Format-SynapseChromeWindowRejectSummary -Windows $candidateWindows
        throw "SYNAPSE_CHROME_MAINTENANCE_NO_ELIGIBLE_WINDOW user_data_root=$ChromeUserDataRoot rejected_windows=$rejected remediation=open the already-authenticated Chrome profile for this user-data root; setup refuses to launch a second Chrome window/profile"
    }

    $chromeWindow = @($eligibleWindows | Sort-Object @{ Expression = 'is_foreground'; Descending = $true }, @{ Expression = 'title'; Descending = $false } | Select-Object -First 1)[0]
    if ($chromeWindow.identity_valid -ne $true) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            hwnd = $chromeWindow.hwnd
            pid = $chromeWindow.pid
            identity_error = $chromeWindow.identity_error
            identity_win32_error = $chromeWindow.identity_win32_error
        }) -Depth 6
        throw "SYNAPSE_CHROME_MAINTENANCE_CHROME_IDENTITY_UNREADABLE detail=$detail remediation=the selected Chrome HWND must expose exact PID, process creation time, and executable path before any foreground mutation"
    }
    $captureDeadline = (Get-Date).AddMilliseconds(1200)
    if ($captureDeadline -gt $Deadline) {
        $captureDeadline = $Deadline
    }
    $priorForeground = Wait-SynapseUntil -Deadline $captureDeadline -SleepMilliseconds 50 -Probe {
        $candidate = Read-SynapseForegroundWindow
        if ($candidate.identity_valid -eq $true) { return $candidate }
        return $null
    }
    if (-not $priorForeground) {
        $lastForeground = Read-SynapseForegroundWindow
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{ last_foreground = $lastForeground }) -Depth 6
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_CAPTURE_FAILED detail=$detail remediation=GetForegroundWindow and the owning process identity must settle before maintenance may acquire Chrome"
    }
    $chromeIdentity = [pscustomobject]@{
        hwnd = [int64]$chromeWindow.hwnd
        pid = [int]$chromeWindow.pid
        process_started_at_100ns = [uint64]$chromeWindow.process_started_at_100ns
        executable_path = [string]$chromeWindow.executable_path
        identity_valid = [bool]$chromeWindow.identity_valid
        identity_error = [string]$chromeWindow.identity_error
        identity_win32_error = [int]$chromeWindow.identity_win32_error
    }
    $foregroundLease = [pscustomobject]@{
        prior_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $priorForeground
        acquired_chrome_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $chromeIdentity
    }
    # Publish foreground ownership before the first focus attempt. Even a failure
    # between acquisition and tab creation must restore or explicitly preserve a
    # superseding operator window.
    $script:SynapseChromeMaintenanceForegroundLease = $foregroundLease
    $foregroundDeadline = (Get-Date).AddSeconds(5)
    if ($foregroundDeadline -gt $Deadline) {
        $foregroundDeadline = $Deadline
    }
    $foreground = Wait-SynapseChromeForegroundAcquisition -Window $chromeWindow -Deadline $foregroundDeadline
    if (-not $foreground.acquired) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            maintenance_tab_token = $MaintenanceTabToken
            chrome_window_hwnd = $chromeWindow.hwnd
            chrome_window_pid = $chromeWindow.pid
            foreground_acquisition = $foreground
        }) -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_TAB_CREATE_FOREGROUND_FAILED detail=$detail remediation=Windows did not grant verified foreground ownership needed to realize and read the exact Chrome UIA tab strip before New Tab invocation"
    }
    $acquiredForeground = Read-SynapseForegroundWindow
    if (-not (Test-SynapseForegroundIdentityExact -Expected $chromeIdentity -Actual $acquiredForeground)) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            expected_chrome = $chromeIdentity
            actual_foreground = $acquiredForeground
            foreground_acquisition = $foreground
        }) -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_IDENTITY_MISMATCH_AFTER_ACQUIRE detail=$detail remediation=the exact Chrome HWND/PID/process-start/image identity must own foreground before any tab mutation"
    }
    $foregroundLease.acquired_chrome_foreground = ConvertTo-SynapseForegroundIdentityEvidence -Identity $acquiredForeground

    # UIA elements are provider-backed snapshots, not durable window handles.
    # The failed Chrome 150 setup captured the element while its HWND was in the
    # background and observed zero TabContainerImpl descendants; a later fresh
    # element for that same physical HWND exposed exactly one container and all
    # 16 tabs. Reading that snapshot before foreground acquisition therefore
    # aborted bridge repair before setup created any tab (#2035).
    # Reacquire the element from the exact HWND only after foreground ownership
    # is physically read back, then pin both process and profile identity before
    # taking the pre-mutation tab-strip SoT snapshot.
    $foregroundWindow = Get-SynapseChromeWindowByHwnd `
        -Hwnd $chromeWindow.hwnd `
        -ChromeUserDataRoot $ChromeUserDataRoot
    if (-not $foregroundWindow) {
        throw "SYNAPSE_CHROME_MAINTENANCE_WINDOW_LOST_AFTER_FOREGROUND hwnd=$($chromeWindow.hwnd) expected_pid=$($chromeWindow.pid) remediation=the exact Chrome window disappeared after foreground acquisition; setup refuses to create a tab in another window"
    }
    if ([int]$foregroundWindow.pid -ne [int]$chromeWindow.pid -or
        [uint64]$foregroundWindow.process_started_at_100ns -ne [uint64]$chromeWindow.process_started_at_100ns -or
        [string]$foregroundWindow.executable_path -ine [string]$chromeWindow.executable_path -or
        $foregroundWindow.identity_valid -ne $true -or
        $foregroundWindow.chrome_profile_eligible -ne $true -or
        [string]$foregroundWindow.chrome_user_data_dir_normalized -cne [string]$chromeWindow.chrome_user_data_dir_normalized) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            expected_hwnd = $chromeWindow.hwnd
            expected_pid = $chromeWindow.pid
            expected_process_started_at_100ns = $chromeWindow.process_started_at_100ns
            expected_executable_path = $chromeWindow.executable_path
            expected_user_data_dir = $chromeWindow.chrome_user_data_dir_normalized
            actual_hwnd = $foregroundWindow.hwnd
            actual_pid = $foregroundWindow.pid
            actual_process_started_at_100ns = $foregroundWindow.process_started_at_100ns
            actual_executable_path = $foregroundWindow.executable_path
            actual_identity_valid = $foregroundWindow.identity_valid
            actual_identity_error = $foregroundWindow.identity_error
            actual_user_data_dir = $foregroundWindow.chrome_user_data_dir_normalized
            actual_profile_eligible = $foregroundWindow.chrome_profile_eligible
            actual_profile_match_reason = $foregroundWindow.chrome_profile_match_reason
            foreground_acquisition = $foreground
        }) -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_WINDOW_IDENTITY_CHANGED_AFTER_FOREGROUND detail=$detail remediation=the foreground HWND no longer belongs to the exact eligible Chrome profile selected before acquisition; setup refuses cross-profile tab creation"
    }
    $chromeWindow = $foregroundWindow
    $tabStripBefore = Read-SynapseChromeTabStripState -Window $chromeWindow

    $newTabCondition = [System.Windows.Automation.AndCondition]::new(
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Button
        ),
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ClassNameProperty,
            'TabStripControlButton'
        ),
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::NameProperty,
            'New Tab'
        )
    )
    $newTabButtons = $chromeWindow.element.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        $newTabCondition
    )
    if ($newTabButtons.Count -ne 1) {
        throw "SYNAPSE_CHROME_MAINTENANCE_NEW_TAB_BUTTON_NOT_UNIQUE hwnd=$($chromeWindow.hwnd) pid=$($chromeWindow.pid) match_count=$($newTabButtons.Count) remediation=the exact eligible Chrome HWND must expose one UIA Button named New Tab with class TabStripControlButton"
    }
    try {
        $newTabPattern = $newTabButtons[0].GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $newTabPattern.Invoke()
    } catch {
        throw "SYNAPSE_CHROME_MAINTENANCE_NEW_TAB_INVOKE_FAILED hwnd=$($chromeWindow.hwnd) pid=$($chromeWindow.pid) error=$($_.Exception.Message) remediation=repair the exact Chrome New Tab button InvokePattern; setup does not use global Ctrl+T"
    }
    $createdWindow = Wait-SynapseUntil -Deadline $Deadline -SleepMilliseconds 100 -Probe {
        $current = Get-SynapseChromeWindowByHwnd `
            -Hwnd $chromeWindow.hwnd `
            -ChromeUserDataRoot $ChromeUserDataRoot
        if (-not $current) {
            return $null
        }
        $tabs = Read-SynapseChromeTabStripState -Window $current
        if ($tabs.count -eq ($tabStripBefore.count + 1)) {
            $beforeRuntimeIds = @($tabStripBefore.tabs | ForEach-Object { [string]$_.runtime_id })
            $createdCandidates = @($tabs.tabs | Where-Object {
                [string]$_.runtime_id -notin $beforeRuntimeIds
            })
            if ($createdCandidates.Count -eq 1) {
                return [pscustomobject]@{
                    window = $current
                    tab_strip = $tabs
                    owned_tab_runtime_id = [string]$createdCandidates[0].runtime_id
                    owned_tab_selected = ($createdCandidates[0].selected -eq $true)
                }
            }
        }
        return $null
    }
    if (-not $createdWindow) {
        $latest = Get-SynapseChromeWindowByHwnd `
            -Hwnd $chromeWindow.hwnd `
            -ChromeUserDataRoot $ChromeUserDataRoot
        $latestTabs = if ($latest) { Read-SynapseChromeTabStripState -Window $latest } else { $null }
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            maintenance_tab_token = $MaintenanceTabToken
            chrome_window_hwnd = $chromeWindow.hwnd
            chrome_window_pid = $chromeWindow.pid
            tab_strip_before = $tabStripBefore
            tab_strip_after = $latestTabs
        }) -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_TAB_CREATE_NOT_CONFIRMED detail=$detail remediation=the exact UIA New Tab invocation did not produce exactly one new runtime identity in the selected Chrome HWND; setup refuses to repurpose an existing operator tab"
    }
    if (-not $createdWindow.owned_tab_selected) {
        $tabItemCondition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::TabItem
        )
        $items = $createdWindow.window.element.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            $tabItemCondition
        )
        $matches = @()
        foreach ($item in $items) {
            $runtimeId = '<unavailable>'
            try {
                $runtimeId = (@($item.GetRuntimeId()) -join '.')
            } catch {
                $runtimeId = "<error:$($_.Exception.Message)>"
            }
            if ($runtimeId -ceq [string]$createdWindow.owned_tab_runtime_id) {
                $matches += $item
            }
        }
        if ($matches.Count -ne 1) {
            throw "SYNAPSE_CHROME_MAINTENANCE_CREATED_TAB_ELEMENT_NOT_UNIQUE hwnd=$($chromeWindow.hwnd) owned_runtime_id=$($createdWindow.owned_tab_runtime_id) match_count=$($matches.Count) remediation=the exact browser tab created by UIA New Tab invocation must remain uniquely selectable"
        }
        try {
            $selection = $matches[0].GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
            $selection.Select()
        } catch {
            throw "SYNAPSE_CHROME_MAINTENANCE_CREATED_TAB_SELECT_FAILED hwnd=$($chromeWindow.hwnd) owned_runtime_id=$($createdWindow.owned_tab_runtime_id) error=$($_.Exception.Message) remediation=repair exact UIA browser-tab selection before maintenance navigation"
        }
        $selectedCreated = Wait-SynapseUntil -Deadline $Deadline -SleepMilliseconds 100 -Probe {
            $current = Get-SynapseChromeWindowByHwnd `
                -Hwnd $chromeWindow.hwnd `
                -ChromeUserDataRoot $ChromeUserDataRoot
            if (-not $current) {
                return $null
            }
            $tabs = Read-SynapseChromeTabStripState -Window $current
            $selected = @($tabs.tabs | Where-Object { $_.selected -eq $true })
            if (
                $selected.Count -eq 1 -and
                [string]$selected[0].runtime_id -ceq [string]$createdWindow.owned_tab_runtime_id
            ) {
                return $tabs
            }
            return $null
        }
        if (-not $selectedCreated) {
            throw "SYNAPSE_CHROME_MAINTENANCE_CREATED_TAB_SELECT_NOT_CONFIRMED hwnd=$($chromeWindow.hwnd) owned_runtime_id=$($createdWindow.owned_tab_runtime_id) remediation=the exact browser tab created by UIA New Tab invocation was not independently observed selected before navigation"
        }
        $createdWindow.tab_strip = $selectedCreated
        $createdWindow.owned_tab_selected = $true
        $createdWindow.window = Get-SynapseChromeWindowByHwnd `
            -Hwnd $chromeWindow.hwnd `
            -ChromeUserDataRoot $ChromeUserDataRoot
    }

    $lease = [pscustomobject]@{
        token = $MaintenanceTabToken
        ownership_source = 'verified_uia_new_tab_marker'
        owned_tab_runtime_id = [string]$createdWindow.owned_tab_runtime_id
        chrome_tab_id = $null
        chrome_window_id = $null
        chrome_window_hwnd = [int64]$createdWindow.window.hwnd
        chrome_window_pid = [int]$createdWindow.window.pid
        marker_url = $MaintenanceMarkerUrl
        marker_title = $MaintenanceMarkerTitle
        marker_navigation = $null
        tab_strip_before_create = $tabStripBefore
        tab_strip_after_marker = $null
        expected_tab_count_after_cleanup = $tabStripBefore.count
        created_via_ui = $true
        foreground_lease = $foregroundLease
    }
    # Publish ownership before navigation so every later failure, including
    # concurrent address-bar input, is cleaned by exact UIA runtime identity.
    $script:SynapseChromeMaintenanceTabLease = $lease

    $markerNavigation = Invoke-SynapseChromeAddressBarNavigation `
        -Window $createdWindow.window `
        -ForegroundLease $foregroundLease `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -Url $MaintenanceMarkerUrl `
        -ExpectedTitlePattern ("^" + [regex]::Escape($MaintenanceMarkerTitle) + "( - Google Chrome)?$") `
        -Deadline $Deadline `
        -Purpose 'owned_bridge_maintenance_marker'

    $createdWindow.window = Get-SynapseChromeWindowByHwnd `
        -Hwnd $chromeWindow.hwnd `
        -ChromeUserDataRoot $ChromeUserDataRoot
    if (-not $createdWindow.window) {
        throw "SYNAPSE_CHROME_MAINTENANCE_WINDOW_LOST_AFTER_MARKER hwnd=$($chromeWindow.hwnd) pid=$($chromeWindow.pid) owned_runtime_id=$($lease.owned_tab_runtime_id) remediation=the exact Chrome window disappeared after marker navigation; cleanup retains the published runtime-id lease"
    }
    $tabStripAfterMarker = Read-SynapseChromeTabStripState -Window $createdWindow.window
    $beforeRuntimeIds = @($tabStripBefore.tabs | ForEach-Object { [string]$_.runtime_id })
    $createdTabRows = @($tabStripAfterMarker.tabs | Where-Object {
        [string]$_.runtime_id -notin $beforeRuntimeIds
    })
    if (
        $tabStripAfterMarker.count -ne ($tabStripBefore.count + 1) -or
        $createdTabRows.Count -ne 1 -or
        [string]$createdTabRows[0].runtime_id -cne [string]$createdWindow.owned_tab_runtime_id -or
        $createdTabRows[0].selected -ne $true
    ) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            maintenance_tab_token = $MaintenanceTabToken
            chrome_window_hwnd = $chromeWindow.hwnd
            tab_strip_before = $tabStripBefore
            tab_strip_after_marker = $tabStripAfterMarker
            created_tab_candidates = $createdTabRows
        }) -Depth 12
        throw "SYNAPSE_CHROME_MAINTENANCE_MARKER_TAB_IDENTITY_AMBIGUOUS detail=$detail remediation=the new marker tab must be the one exact newly-created and selected UIA runtime identity"
    }
    $lease.marker_navigation = $markerNavigation
    $lease.tab_strip_after_marker = $tabStripAfterMarker
    [pscustomobject]@{
        window = $createdWindow.window
        lease = $lease
    }
}

function Close-SynapseOwnedChromeMaintenanceTabViaUiCore {
    param(
        [Parameter(Mandatory = $true)]
        $Lease,
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline
    )

    $window = Get-SynapseChromeWindowByHwnd `
        -Hwnd $Lease.chrome_window_hwnd `
        -ChromeUserDataRoot $ChromeUserDataRoot
    if (-not $window) {
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_WINDOW_MISSING hwnd=$($Lease.chrome_window_hwnd) pid=$($Lease.chrome_window_pid) token=$($Lease.token) remediation=read chrome.tabs by the exact ownership token before deciding whether the tab was already removed"
    }
    $tabsBefore = Read-SynapseChromeTabStripState -Window $window
    $ownedRuntimeId = [string]$Lease.owned_tab_runtime_id
    $ownedRows = @($tabsBefore.tabs | Where-Object {
        [string]$_.runtime_id -ceq $ownedRuntimeId
    })
    if ($ownedRows.Count -ne 1) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            lease = $Lease
            tabs_before_cleanup = $tabsBefore
            owned_runtime_id_match_count = $ownedRows.Count
        }) -Depth 12
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_RUNTIME_ID_NOT_UNIQUE detail=$detail remediation=cleanup requires the one exact UIA tab identity created by this operation"
    }
    if ($ownedRows[0].selected -ne $true) {
        $tabItemCondition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::TabItem
        )
        $items = $window.element.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            $tabItemCondition
        )
        $ownedElements = @()
        foreach ($item in $items) {
            $runtimeId = '<unavailable>'
            try {
                $runtimeId = (@($item.GetRuntimeId()) -join '.')
            } catch {
                $runtimeId = "<error:$($_.Exception.Message)>"
            }
            if ($runtimeId -ceq $ownedRuntimeId) {
                $ownedElements += $item
            }
        }
        if ($ownedElements.Count -ne 1) {
            throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_RUNTIME_ELEMENT_NOT_UNIQUE hwnd=$($window.hwnd) owned_runtime_id=$ownedRuntimeId match_count=$($ownedElements.Count) token=$($Lease.token) remediation=repair Chrome UI Automation identity exposure before cleanup"
        }
        try {
            $selection = $ownedElements[0].GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
            $selection.Select()
        } catch {
            throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_RUNTIME_SELECT_FAILED hwnd=$($window.hwnd) owned_runtime_id=$ownedRuntimeId token=$($Lease.token) error=$($_.Exception.Message) remediation=repair Chrome UI Automation tab selection before cleanup"
        }
        $selectedOwned = Wait-SynapseUntil -Deadline $Deadline -SleepMilliseconds 100 -Probe {
            $current = Get-SynapseChromeWindowByHwnd `
                -Hwnd $window.hwnd `
                -ChromeUserDataRoot $ChromeUserDataRoot
            if (-not $current) {
                return $null
            }
            $tabs = Read-SynapseChromeTabStripState -Window $current
            $selected = @($tabs.tabs | Where-Object { $_.selected -eq $true })
            if ($selected.Count -eq 1 -and [string]$selected[0].runtime_id -ceq $ownedRuntimeId) {
                return $tabs
            }
            return $null
        }
        if (-not $selectedOwned) {
            throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_RUNTIME_SELECT_NOT_CONFIRMED hwnd=$($window.hwnd) owned_runtime_id=$ownedRuntimeId token=$($Lease.token) remediation=the exact operation-created tab was not independently observed selected"
        }
        $window = Get-SynapseChromeWindowByHwnd `
            -Hwnd $Lease.chrome_window_hwnd `
            -ChromeUserDataRoot $ChromeUserDataRoot
    }
    $navigationBefore = Read-SynapseChromeNavigationState -Hwnd $window.hwnd -ChromeUserDataRoot $ChromeUserDataRoot
    $addressBefore = ([string]$navigationBefore.address_bar_value).Trim().TrimEnd('/')
    $markerAddress = ([string]$Lease.marker_url).Trim().TrimEnd('/')
    $tokenPattern = 'synapse_maintenance_token=' + [regex]::Escape([string]$Lease.token)
    $ownedAddress = ($addressBefore -ceq $markerAddress -or $addressBefore -match $tokenPattern)
    # The address bar is not a sound ownership proof on its own: Chrome strips the
    # query string from chrome:// URLs, so the token this lease was built around is
    # erased from the omnibox by the very navigation the operation performs. The
    # durable identity is the UIA runtime id of the exact tab this operation
    # created, which is already what selection is verified against above. Refusing
    # Refusing any close against a tab we cannot prove we own stays correct;
    # this only stops a correctly-owned tab being disowned by URL normalisation.
    # Read the strip again here rather than reusing the pre-selection snapshot, so
    # the selected identity is the one that exists at the moment of the check.
    $tabsAtOwnershipCheck = Read-SynapseChromeTabStripState -Window $window
    $selectedTabs = @($tabsAtOwnershipCheck.tabs | Where-Object { $_.selected -eq $true })
    $ownedRuntime = (
        $selectedTabs.Count -eq 1 -and
        [string]$selectedTabs[0].runtime_id -ceq [string]$Lease.owned_tab_runtime_id
    )
    if (-not ($ownedAddress -or $ownedRuntime)) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            lease = $Lease
            navigation_before = $navigationBefore
            owned_by_address = $ownedAddress
            owned_by_runtime_id = $ownedRuntime
            expected_runtime_id = [string]$Lease.owned_tab_runtime_id
            selected_runtime_ids = @($selectedTabs | ForEach-Object { [string]$_.runtime_id })
        }) -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_OWNERSHIP_LOST detail=$detail remediation=the operation-created tab is neither the active exact marker/token tab nor the exact UIA runtime identity this operation created; setup refuses any close against an operator tab"
    }
    # Close the exact owned UIA tab element directly. Ctrl+W depends on the
    # global foreground at the instant the key arrives, so concurrent operator
    # input can redirect it even after a correct foreground readback. The
    # TabCloseButton is a descendant of the runtime-id-pinned tab and exposes an
    # InvokePattern; using it makes the destructive target local and immutable
    # at the invocation boundary.
    $tabItemCondition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::TabItem
    )
    $ownedElementsAtClose = @()
    foreach ($item in $window.element.FindAll([System.Windows.Automation.TreeScope]::Descendants, $tabItemCondition)) {
        $runtimeId = '<unavailable>'
        try {
            $runtimeId = (@($item.GetRuntimeId()) -join '.')
        } catch {
            $runtimeId = "<error:$($_.Exception.Message)>"
        }
        if ($runtimeId -ceq $ownedRuntimeId) {
            $ownedElementsAtClose += $item
        }
    }
    if ($ownedElementsAtClose.Count -ne 1) {
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_CLOSE_TARGET_NOT_UNIQUE hwnd=$($window.hwnd) owned_runtime_id=$ownedRuntimeId match_count=$($ownedElementsAtClose.Count) token=$($Lease.token) remediation=the exact operation-created UIA tab must remain uniquely present at the close boundary"
    }
    $closeButtonCondition = [System.Windows.Automation.AndCondition]::new(
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Button
        ),
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ClassNameProperty,
            'TabCloseButton'
        )
    )
    $closeButtons = $ownedElementsAtClose[0].FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        $closeButtonCondition
    )
    if ($closeButtons.Count -ne 1) {
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_CLOSE_BUTTON_NOT_UNIQUE hwnd=$($window.hwnd) owned_runtime_id=$ownedRuntimeId match_count=$($closeButtons.Count) token=$($Lease.token) remediation=the exact selected operation-created tab must expose one TabCloseButton before cleanup mutates Chrome"
    }
    try {
        $closePattern = $closeButtons[0].GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $closePattern.Invoke()
    } catch {
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_CLOSE_INVOKE_FAILED hwnd=$($window.hwnd) owned_runtime_id=$ownedRuntimeId token=$($Lease.token) error=$($_.Exception.Message) remediation=repair the exact Chrome TabCloseButton InvokePattern before retrying cleanup"
    }
    $after = Wait-SynapseUntil -Deadline $Deadline -SleepMilliseconds 100 -Probe {
        $current = Get-SynapseChromeWindowByHwnd `
            -Hwnd $window.hwnd `
            -ChromeUserDataRoot $ChromeUserDataRoot
        if (-not $current) {
            return $null
        }
        $tabs = Read-SynapseChromeTabStripState -Window $current
        $navigation = Read-SynapseChromeNavigationState -Hwnd $current.hwnd -ChromeUserDataRoot $ChromeUserDataRoot
        $address = ([string]$navigation.address_bar_value).Trim().TrimEnd('/')
        # Absence of the exact owned runtime id is the positive proof that this
        # operation's own tab is gone. The address checks alone would pass simply
        # because Chrome stripped the token out of the omnibox.
        $ownedRuntimeStillPresent = @(
            $tabs.tabs | Where-Object {
                [string]$_.runtime_id -ceq [string]$Lease.owned_tab_runtime_id
            }
        ).Count -gt 0
        $baselineRuntimeIds = @($Lease.tab_strip_before_create.tabs | ForEach-Object { [string]$_.runtime_id })
        $currentRuntimeIds = @($tabs.tabs | ForEach-Object { [string]$_.runtime_id })
        $missingBaselineRuntimeIds = @($baselineRuntimeIds | Where-Object { $_ -notin $currentRuntimeIds })
        if (-not $ownedRuntimeStillPresent -and $missingBaselineRuntimeIds.Count -eq 0) {
            return [pscustomobject]@{
                tab_strip = $tabs
                navigation = $navigation
                missing_baseline_runtime_ids = @()
                concurrent_runtime_ids = @($currentRuntimeIds | Where-Object {
                    $_ -notin $baselineRuntimeIds -and $_ -cne [string]$Lease.owned_tab_runtime_id
                })
            }
        }
        return $null
    }
    if (-not $after) {
        $latest = Get-SynapseChromeWindowByHwnd `
            -Hwnd $window.hwnd `
            -ChromeUserDataRoot $ChromeUserDataRoot
        $latestTabs = if ($latest) { Read-SynapseChromeTabStripState -Window $latest } else { $null }
        $latestNavigation = if ($latest) { Read-SynapseChromeNavigationState -Hwnd $latest.hwnd -ChromeUserDataRoot $ChromeUserDataRoot } else { $null }
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            lease = $Lease
            tabs_before = $tabsBefore
            tabs_after = $latestTabs
            navigation_after = $latestNavigation
        }) -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_NOT_CONFIRMED detail=$detail remediation=the exact operation-created runtime id must be absent and every pre-operation tab runtime id must remain present; concurrent tabs are retained and reported, never closed"
    }
    $selectionRestore = [pscustomobject]@{
        attempted = $false
        restored = $false
        reason = 'no_precreate_tab_strip_snapshot'
        expected_runtime_id = $null
        before_runtime_id = $null
        after_runtime_id = $null
    }
    if ($Lease.tab_strip_before_create) {
        $expectedSelected = @($Lease.tab_strip_before_create.tabs | Where-Object { $_.selected -eq $true })
        if ($expectedSelected.Count -ne 1) {
            throw "SYNAPSE_CHROME_MAINTENANCE_SELECTION_SNAPSHOT_INVALID hwnd=$($window.hwnd) selected_count=$($expectedSelected.Count) token=$($Lease.token) remediation=the UI-created maintenance path requires exactly one previously-selected Chrome tab identity"
        }
        $expectedRuntimeId = [string]$expectedSelected[0].runtime_id
        $selectedBeforeRestore = @($after.tab_strip.tabs | Where-Object { $_.selected -eq $true })
        $beforeRuntimeId = if ($selectedBeforeRestore.Count -eq 1) { [string]$selectedBeforeRestore[0].runtime_id } else { '<ambiguous>' }
        if ($beforeRuntimeId -cne $expectedRuntimeId) {
            $currentWindow = Get-SynapseChromeWindowByHwnd -Hwnd $window.hwnd
            $tabItemCondition = [System.Windows.Automation.PropertyCondition]::new(
                [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
                [System.Windows.Automation.ControlType]::TabItem
            )
            $items = $currentWindow.element.FindAll(
                [System.Windows.Automation.TreeScope]::Descendants,
                $tabItemCondition
            )
            $matches = @()
            foreach ($item in $items) {
                $runtimeId = '<unavailable>'
                try {
                    $runtimeId = (@($item.GetRuntimeId()) -join '.')
                } catch {
                    $runtimeId = "<error:$($_.Exception.Message)>"
                }
                if ($runtimeId -ceq $expectedRuntimeId) {
                    $matches += $item
                }
            }
            if ($matches.Count -ne 1) {
                throw "SYNAPSE_CHROME_MAINTENANCE_SELECTION_RESTORE_TARGET_NOT_UNIQUE hwnd=$($window.hwnd) expected_runtime_id=$expectedRuntimeId match_count=$($matches.Count) token=$($Lease.token) remediation=setup closed its maintenance tab but refuses to guess which operator tab was previously selected"
            }
            try {
                $selection = $matches[0].GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
                $selection.Select()
            } catch {
                throw "SYNAPSE_CHROME_MAINTENANCE_SELECTION_RESTORE_FAILED hwnd=$($window.hwnd) expected_runtime_id=$expectedRuntimeId error=$($_.Exception.Message) remediation=repair Chrome UI Automation tab selection before accepting maintenance cleanup"
            }
            $restored = Wait-SynapseUntil -Deadline $Deadline -SleepMilliseconds 100 -Probe {
                $current = Get-SynapseChromeWindowByHwnd -Hwnd $window.hwnd
                if (-not $current) {
                    return $null
                }
                $tabs = Read-SynapseChromeTabStripState -Window $current
                $selected = @($tabs.tabs | Where-Object { $_.selected -eq $true })
                if ($selected.Count -eq 1 -and [string]$selected[0].runtime_id -ceq $expectedRuntimeId) {
                    return $tabs
                }
                return $null
            }
            if (-not $restored) {
                throw "SYNAPSE_CHROME_MAINTENANCE_SELECTION_RESTORE_NOT_CONFIRMED hwnd=$($window.hwnd) expected_runtime_id=$expectedRuntimeId token=$($Lease.token) remediation=the previously-selected operator tab was not independently observed selected after cleanup"
            }
            $after.tab_strip = $restored
        }
        $selectedAfterRestore = @($after.tab_strip.tabs | Where-Object { $_.selected -eq $true })
        $afterRuntimeId = if ($selectedAfterRestore.Count -eq 1) { [string]$selectedAfterRestore[0].runtime_id } else { '<ambiguous>' }
        $selectionRestore = [pscustomobject]@{
            attempted = ($beforeRuntimeId -cne $expectedRuntimeId)
            restored = ($afterRuntimeId -ceq $expectedRuntimeId)
            reason = if ($beforeRuntimeId -ceq $expectedRuntimeId) { 'already_restored_by_chrome' } else { 'uia_selection_item_exact_runtime_id' }
            expected_runtime_id = $expectedRuntimeId
            before_runtime_id = $beforeRuntimeId
            after_runtime_id = $afterRuntimeId
        }
    }
    [pscustomobject]@{
        attempted = $true
        closed = $true
        absent_verified = $true
        method = 'exact_uia_runtime_id_tab_close_button_invoke'
        token = $Lease.token
        owned_tab_runtime_id = $Lease.owned_tab_runtime_id
        chrome_tab_id = $Lease.chrome_tab_id
        chrome_window_id = $Lease.chrome_window_id
        chrome_window_hwnd = $Lease.chrome_window_hwnd
        tabs_before = $tabsBefore
        tabs_after = $after.tab_strip
        navigation_before = $navigationBefore
        navigation_after = $after.navigation
        concurrent_runtime_ids = @($after.concurrent_runtime_ids)
        missing_baseline_runtime_ids = @($after.missing_baseline_runtime_ids)
        prior_selection_restore = $selectionRestore
    }
}

function Close-SynapseOwnedChromeMaintenanceTabViaUi {
    param(
        [Parameter(Mandatory = $true)]$Lease,
        [Parameter(Mandatory = $true)][string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)][datetime]$Deadline
    )

    $cleanup = $null
    $cleanupError = $null
    try {
        $cleanup = Close-SynapseOwnedChromeMaintenanceTabViaUiCore `
            -Lease $Lease `
            -ChromeUserDataRoot $ChromeUserDataRoot `
            -Deadline $Deadline
    } catch {
        $cleanupError = $_.Exception.Message
    }

    $foregroundLease = $Lease.foreground_lease
    if (-not $foregroundLease) {
        $foregroundLease = $script:SynapseChromeMaintenanceForegroundLease
    }
    $foreground = $null
    $foregroundError = $null
    if ($foregroundLease) {
        try {
            $foreground = Restore-SynapseChromeMaintenanceForeground `
                -ForegroundLease $foregroundLease `
                -Deadline $Deadline
        } catch {
            $foregroundError = $_.Exception.Message
        }
    } else {
        $foregroundError = 'SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_RESTORE_LEASE_MISSING remediation=cleanup cannot prove whether it still owns the foreground without the pre-operation and acquired-Chrome identities'
    }

    if ($cleanupError -and $foregroundError) {
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_AND_FOREGROUND_RESTORE_FAILED cleanup_error=$cleanupError foreground_error=$foregroundError remediation=repair both exact tab cleanup and foreground transaction conditions; neither failure is hidden"
    }
    if ($cleanupError) {
        $foregroundJson = ConvertTo-CompressedJson -Value $foreground -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_FAILED cleanup_error=$cleanupError foreground_transaction=$foregroundJson remediation=the exact foreground transaction completed, but tab cleanup did not prove its postconditions"
    }
    if ($foregroundError) {
        throw $foregroundError
    }
    $cleanup | Add-Member -NotePropertyName foreground_transaction -NotePropertyValue $foreground -Force
    return $cleanup
}

function Get-SynapseChromeDaemonBridgeHealth {
    param(
        [Parameter(Mandatory = $true)]
        [string]$BearerToken,
        [ValidateRange(1, 60)]
        [int]$TimeoutSec = 5
    )

    try {
        $health = Invoke-RestMethod `
            -Uri 'http://127.0.0.1:7700/health' `
            -Method Get `
            -Headers @{ Authorization = "Bearer $BearerToken" } `
            -TimeoutSec $TimeoutSec `
            -ErrorAction Stop
        $subsystem = $health.subsystems.chrome_bridge
        if (-not $subsystem) {
            throw 'health response omitted subsystems.chrome_bridge'
        }
        $bridge = $subsystem.chrome_bridge
        if (-not $bridge) {
            throw 'health response omitted subsystems.chrome_bridge.chrome_bridge'
        }
        $detail = [string]$subsystem.detail
        $readDetail = {
            param([string]$Name)
            $match = [regex]::Match(
                $detail,
                ('(?:^|\s)' + [regex]::Escape($Name) + '=(?<value>[^\s]+)')
            )
            if ($match.Success) {
                return [string]$match.Groups['value'].Value
            }
            return $null
        }
        $activeHostId = & $readDetail 'active_host_id'
        $buildId = & $readDetail 'extension_build_id'
        $workerSha256 = & $readDetail 'extension_service_worker_sha256'
        $staleText = & $readDetail 'extension_stale'
        $extensionId = & $readDetail 'extension_id'
        [pscustomobject]@{
            ok = $true
            daemon_pid = [int64]$health.pid
            subsystem_status = [string]$subsystem.status
            host_count = [int]$bridge.host_count
            tab_control_available = [bool]$bridge.tab_control_available
            active_host_id = $activeHostId
            extension_id = $extensionId
            extension_build_id = $buildId
            extension_service_worker_sha256 = $workerSha256
            extension_stale = if ($staleText -eq 'true') { $true } elseif ($staleText -eq 'false') { $false } else { $null }
            probe_timeout_s = $TimeoutSec
            error = $null
        }
    } catch {
        [pscustomobject]@{
            ok = $false
            daemon_pid = $null
            subsystem_status = $null
            host_count = $null
            tab_control_available = $null
            active_host_id = $null
            extension_id = $null
            extension_build_id = $null
            extension_service_worker_sha256 = $null
            extension_stale = $null
            probe_timeout_s = $TimeoutSec
            error = $_.Exception.Message
        }
    }
}

# --- #2031 -----------------------------------------------------------------
# Bounded startup-probe gate in front of the pre-Chrome daemon health readback.
#
# Both daemon-controlled Chrome paths below must read authenticated /health
# BEFORE they create a maintenance tab, so the replacement bridge host can be
# diffed against a proven "before" identity. That read used to be a single
# 5-second request. A daemon that has only just been handed off by setup is
# already accepting connections while it is still inside cold-start work
# (Calyx vault/index open, embedded model load, first facade registration), so
# one unlucky sample timed out and the whole reload failed closed even though
# an identical read moments later succeeded (#2031).
#
# A single short request is a liveness probe. What this path needs is a
# startup probe: the same check, given an explicitly bounded runway, polled
# with backoff, and still failing closed at the end with the exact evidence.
# The runway is carved out of the caller's existing operation deadline so the
# physical Chrome work that follows keeps its budget; nothing here retries
# forever, and nothing here proceeds on an unproven daemon.
function Wait-SynapseChromeDaemonBridgeHealth {
    param(
        [Parameter(Mandatory = $true)]
        [string]$BearerToken,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline,
        [Parameter(Mandatory = $true)]
        [ValidateSet('before_reload', 'before_load')]
        [string]$Purpose,
        [ValidateRange(1, 120)]
        [int]$BudgetSeconds = 15,
        [ValidateRange(1, 60)]
        [int]$ProbeTimeoutSeconds = 5
    )

    $started = Get-Date
    $budgetDeadline = $started.AddSeconds($BudgetSeconds)
    $effectiveDeadline = if ($budgetDeadline -lt $Deadline) { $budgetDeadline } else { $Deadline }
    $attempt = 0
    $backoffMs = 250
    $maxBackoffMs = 2000
    $last = $null
    $observedErrors = New-Object System.Collections.Generic.List[string]

    while ($true) {
        $attempt += 1
        $last = Get-SynapseChromeDaemonBridgeHealth -BearerToken $BearerToken -TimeoutSec $ProbeTimeoutSeconds
        if ($last.ok) {
            break
        }
        $lastError = [string]$last.error
        if (-not $observedErrors.Contains($lastError)) {
            [void]$observedErrors.Add($lastError)
        }
        # A credential rejection is never a startup race: the daemon answered
        # and refused this token. Waiting cannot repair it, so stop immediately
        # and let the caller report the physical condition.
        if ($lastError -match '(?i)\(401\)|\(403\)|Unauthorized|Forbidden') {
            break
        }
        if ((Get-Date) -ge $effectiveDeadline) {
            break
        }
        $remainingMs = [int][Math]::Max(0, ($effectiveDeadline - (Get-Date)).TotalMilliseconds)
        $sleepMs = [Math]::Min($backoffMs, $remainingMs)
        if ($sleepMs -gt 0) {
            Start-Sleep -Milliseconds $sleepMs
        }
        $backoffMs = [Math]::Min($maxBackoffMs, $backoffMs * 2)
    }

    $readiness = [ordered]@{
        purpose = $Purpose
        attempts = $attempt
        elapsed_ms = [int]((Get-Date) - $started).TotalMilliseconds
        budget_s = $BudgetSeconds
        probe_timeout_s = $ProbeTimeoutSeconds
        operation_deadline_utc = $Deadline.ToUniversalTime().ToString('o')
        effective_deadline_utc = $effectiveDeadline.ToUniversalTime().ToString('o')
        distinct_errors = @($observedErrors.ToArray())
        last_error = [string]$last.error
    }
    [pscustomobject]@{
        ok = [bool]$last.ok
        health = $last
        error = [string]$last.error
        readiness = $readiness
    }
}

function Wait-SynapseChromeDaemonBridgeReplacement {
    param(
        [Parameter(Mandatory = $true)]
        [string]$BearerToken,
        [Parameter(Mandatory = $true)]
        $Before,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedExtensionId,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedBuildId,
        [Parameter(Mandatory = $true)]
        [string]$ExpectedServiceWorkerSha256,
        [Parameter(Mandatory = $true)]
        [datetime]$Deadline
    )

    $last = $null
    do {
        $last = Get-SynapseChromeDaemonBridgeHealth -BearerToken $BearerToken
        $replacementHost = (
            $last.ok -and
            -not [string]::IsNullOrWhiteSpace([string]$last.active_host_id) -and
            (
                [string]::IsNullOrWhiteSpace([string]$Before.active_host_id) -or
                [string]$last.active_host_id -cne [string]$Before.active_host_id
            )
        )
        if (
            $last.ok -and
            [int64]$last.daemon_pid -eq [int64]$Before.daemon_pid -and
            [string]$last.subsystem_status -ceq 'ok' -and
            [int]$last.host_count -eq 1 -and
            $last.tab_control_available -eq $true -and
            $replacementHost -and
            [string]$last.extension_id -ceq $ExpectedExtensionId -and
            [string]$last.extension_build_id -ceq $ExpectedBuildId -and
            [string]$last.extension_service_worker_sha256 -ceq $ExpectedServiceWorkerSha256 -and
            $last.extension_stale -eq $false
        ) {
            return $last
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $Deadline)

    $detail = ConvertTo-CompressedJson -Value ([ordered]@{
        before = $Before
        last = $last
        expected_extension_id = $ExpectedExtensionId
        expected_build_id = $ExpectedBuildId
        expected_service_worker_sha256 = $ExpectedServiceWorkerSha256
    }) -Depth 8
    throw "SYNAPSE_CHROME_BRIDGE_REPLACEMENT_HOST_NOT_CONFIRMED detail=$detail remediation=the Reload control completed but the same daemon did not expose exactly one clean replacement bridge host with the deployed build/worker identity before the deadline"
}

function Invoke-SynapseChromeBridgeExistingUiReload {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionId,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionDir,
        [Parameter(Mandatory = $true)]
        [string]$ActiveProfile,
        [Parameter(Mandatory = $true)]
        $Before,
        [Parameter(Mandatory = $true)]
        [int]$TimeoutSeconds
    )

    $expectedReady = ($Before.installed -and $Before.manifest_path_matches -and $Before.ready)
    $nonstableReady = ($Before.installed -and $Before.enabled_and_permissioned -and $Before.manifest_dir_exists)
    $permissionActivationEligible = (
        $Before.installed -and
        $Before.manifest_path_matches -and
        $Before.manifest_dir_exists -and
        $Before.enabled -eq $true -and
        $Before.disable_reasons.Count -eq 0 -and
        $Before.missing_active_api_permissions.Count -gt 0
    )
    if (-not $expectedReady -and -not $nonstableReady -and -not $permissionActivationEligible) {
        $missing = if ($Before.missing_active_api_permissions.Count -eq 0) { '<none>' } else { $Before.missing_active_api_permissions -join ',' }
        $disableReasons = if ($Before.disable_reasons.Count -eq 0) { '<none>' } else { $Before.disable_reasons -join ',' }
        throw "SYNAPSE_CHROME_BRIDGE_UI_RELOAD_ROW_NOT_ELIGIBLE active_profile=$ActiveProfile installed=$($Before.installed) manifest_path_matches=$($Before.manifest_path_matches) ready=$($Before.ready) enabled=$($Before.enabled) enabled_and_permissioned=$($Before.enabled_and_permissioned) manifest_dir_exists=$($Before.manifest_dir_exists) permission_activation_eligible=$permissionActivationEligible missing_active_api_permissions=$missing disable_reasons=$disableReasons remediation=UI reload requires an installed enabled row from the exact expected path (or an enabled fully-permissioned extant nonstable path); disabled, missing-path, and ambiguous rows are never reloaded"
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $daemonBridgeBefore = $null
    if ($OutputJson) {
        $daemonBridgeReadiness = Wait-SynapseChromeDaemonBridgeHealth `
            -BearerToken $bridgeBearerToken `
            -Deadline $deadline `
            -Purpose 'before_reload' `
            -BudgetSeconds ([Math]::Max(5, [Math]::Min(15, [int]($TimeoutSeconds / 3))))
        $daemonBridgeBefore = $daemonBridgeReadiness.health
        if (-not $daemonBridgeReadiness.ok) {
            $readinessDetail = ConvertTo-CompressedJson -Value $daemonBridgeReadiness.readiness -Depth 6
            throw "SYNAPSE_CHROME_BRIDGE_DAEMON_HEALTH_BEFORE_RELOAD_FAILED error=$($daemonBridgeReadiness.error) readiness=$readinessDetail remediation=daemon-controlled UI reload requires authenticated /health before creating a maintenance tab so replacement-host identity can be proven. The bounded startup-probe runway in readiness above already expired, so this is not a single unlucky sample: read last_error and repair the named daemon condition (cold-start work still holding /health, credential rejection, or a busy chrome_bridge state lock) before retrying browser_debugger reload_bridge"
        }
    }
    $maintenance = Enter-SynapseOwnedChromeMaintenanceTab `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -Deadline $deadline
    $chromeWindow = $maintenance.window
    $extensionDetailsUrl = "chrome://extensions/?id=$ExtensionId&synapse_maintenance_token=$MaintenanceTabToken"

    $navigation = Invoke-SynapseChromeAddressBarNavigation `
        -Window $chromeWindow `
        -ForegroundLease $maintenance.lease.foreground_lease `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -Url $extensionDetailsUrl `
        -ExpectedTitlePattern '^Extensions( - Synapse Chrome Bridge)?( - Google Chrome)?$' `
        -Deadline $deadline `
        -Purpose 'existing_bridge_extension_details'

    $details = Wait-SynapseUntil -Deadline $deadline -Probe {
        $currentWindow = @(Get-SynapseChromeTopLevelWindows -ChromeUserDataRoot $ChromeUserDataRoot | Where-Object {
            $_.hwnd -eq $chromeWindow.hwnd
        } | Select-Object -First 1)
        if ($currentWindow.Count -eq 0) {
            return $null
        }
        $reloadButton = Find-SynapseAutomationElementByAutomationId `
            -Root $currentWindow[0].element `
            -AutomationId 'dev-reload-button' `
            -ControlType ([System.Windows.Automation.ControlType]::Button)
        if (-not $reloadButton) {
            return $null
        }
        [pscustomobject]@{
            window = $currentWindow[0]
            reload_button = $reloadButton
            ui_before = Read-SynapseChromeExtensionDetailsUiState -Window $currentWindow[0]
        }
    }
    if (-not $details) {
        $latestWindow = Get-SynapseChromeWindowByHwnd -Hwnd $chromeWindow.hwnd
        $latestTitle = if ($latestWindow) { [string]$latestWindow.title } else { '<missing>' }
        $navigationReadback = ConvertTo-CompressedJson -Value $navigation -Depth 10
        throw "SYNAPSE_CHROME_BRIDGE_UI_RELOAD_BUTTON_NOT_FOUND active_profile=$ActiveProfile timeout_s=$TimeoutSeconds chrome_window_hwnd=$($chromeWindow.hwnd) chrome_window_pid=$($chromeWindow.pid) latest_title=$latestTitle user_data_dir=$($chromeWindow.chrome_user_data_dir) match_reason=$($chromeWindow.chrome_profile_match_reason) navigation=$navigationReadback remediation=Chrome reached the extension details page but did not expose the Synapse extension details Reload button through UI Automation; setup refuses to click arbitrary browser coordinates or launch another Chrome profile"
    }

    if (-not $details.ui_before.enable_toggle_present -or -not $details.ui_before.enable_toggle_on) {
        throw "SYNAPSE_CHROME_BRIDGE_UI_RELOAD_EXTENSION_DISABLED active_profile=$ActiveProfile chrome_window_hwnd=$($details.window.hwnd) chrome_window_pid=$($details.window.pid) enable_toggle_present=$($details.ui_before.enable_toggle_present) enable_toggle_name=$($details.ui_before.enable_toggle_name) remediation=the existing Synapse Chrome Bridge must be enabled before setup may reload it"
    }

    Invoke-SynapseAutomationElement -Element $details.reload_button -Description 'Synapse Chrome Bridge Reload'
    # Reloading an unpacked extension can briefly rebuild Chrome's UIA tree
    # while the physical HWND and process remain alive. Require the same HWND
    # to reappear within the existing bounded operation deadline.
    $afterWindow = Wait-SynapseUntil -Deadline $deadline -SleepMilliseconds 100 -Probe {
        Get-SynapseChromeWindowByHwnd -Hwnd $chromeWindow.hwnd
    }
    if (-not $afterWindow) {
        throw "SYNAPSE_CHROME_BRIDGE_UI_RELOAD_WINDOW_LOST active_profile=$ActiveProfile chrome_window_hwnd=$($chromeWindow.hwnd) chrome_window_pid=$($chromeWindow.pid) remediation=Chrome window disappeared after reload; keep the already-open authenticated profile available and rerun setup"
    }
    $uiAfter = Read-SynapseChromeExtensionDetailsUiState -Window $afterWindow
    # Extension activation and Chrome's durable Preferences flush are distinct
    # transitions. Poll the physical profile row to the existing operation
    # deadline instead of sampling once after a fixed sleep.
    $profileReadStarted = Get-Date
    $profileReadAttempts = 0
    $profileReadDelayMs = 100
    $after = $null
    $lastAfter = $null
    do {
        $profileReadAttempts += 1
        $lastAfter = Test-SynapseChromeBridgeProfileRow `
            -ChromeUserDataRoot $ChromeUserDataRoot `
            -ProfileName $ActiveProfile `
            -ExtensionId $ExtensionId `
            -ExtensionDir $ExtensionDir
        $afterExpectedReady = ($lastAfter.installed -and $lastAfter.manifest_path_matches -and $lastAfter.ready)
        $afterNonstableReady = ($lastAfter.installed -and $lastAfter.enabled_and_permissioned -and $lastAfter.manifest_dir_exists)
        if ($afterExpectedReady -or $afterNonstableReady) {
            $after = $lastAfter
            break
        }
        if ((Get-Date) -ge $deadline) {
            break
        }
        $remainingMs = [Math]::Max(1, [int](($deadline - (Get-Date)).TotalMilliseconds))
        Start-Sleep -Milliseconds ([Math]::Min($profileReadDelayMs, $remainingMs))
        $profileReadDelayMs = [Math]::Min(1000, $profileReadDelayMs * 2)
    } while ((Get-Date) -lt $deadline)
    $profileReadElapsedMs = [Math]::Max(0, [int](((Get-Date) - $profileReadStarted).TotalMilliseconds))
    if (-not $after) {
        $after = $lastAfter
    }
    $afterExpectedReady = ($after.installed -and $after.manifest_path_matches -and $after.ready)
    $afterNonstableReady = ($after.installed -and $after.enabled_and_permissioned -and $after.manifest_dir_exists)
    if (-not $afterExpectedReady -and -not $afterNonstableReady) {
        $missingAfter = if ($after.missing_active_api_permissions.Count -eq 0) { '<none>' } else { $after.missing_active_api_permissions -join ',' }
        $disableAfter = if ($after.disable_reasons.Count -eq 0) { '<none>' } else { $after.disable_reasons -join ',' }
        throw "SYNAPSE_CHROME_BRIDGE_UI_RELOAD_PROFILE_ROW_NOT_READY_AFTER active_profile=$ActiveProfile attempts=$profileReadAttempts elapsed_ms=$profileReadElapsedMs installed=$($after.installed) manifest_path_matches=$($after.manifest_path_matches) ready=$($after.ready) enabled=$($after.enabled) enabled_and_permissioned=$($after.enabled_and_permissioned) manifest_dir_exists=$($after.manifest_dir_exists) missing_active_api_permissions=$missingAfter disable_reasons=$disableAfter remediation=the bounded durable Chrome Preferences read never reached exact path/permission parity after Reload; inspect the active profile row and any permission-warning disable reason before retrying"
    }
    $daemonBridgeAfter = $null
    if ($OutputJson) {
        $daemonBridgeAfter = Wait-SynapseChromeDaemonBridgeReplacement `
            -BearerToken $bridgeBearerToken `
            -Before $daemonBridgeBefore `
            -ExpectedExtensionId $ExtensionId `
            -ExpectedBuildId $bridgeBuildId `
            -ExpectedServiceWorkerSha256 $extensionDeploy.service_worker_sha256 `
            -Deadline $deadline
    }

    $reloadReason = if ($permissionActivationEligible) {
        'existing_extension_permission_activation_ui_reload_invoked'
    } elseif ($Before.manifest_path_matches) {
        'existing_ready_extension_ui_reload_invoked'
    } else {
        'existing_ready_extension_nonstable_path_ui_reload_invoked'
    }
    return [pscustomobject]@{
        attempted = $true
        changed = $true
        reason = $reloadReason
        active_profile = $ActiveProfile
        required_foreground = $true
        chrome_window_hwnd = $chromeWindow.hwnd
        chrome_window_pid = $chromeWindow.pid
        chrome_window_user_data_dir = $chromeWindow.chrome_user_data_dir
        chrome_window_profile_match_reason = $chromeWindow.chrome_profile_match_reason
        extension_details_url = $extensionDetailsUrl
        maintenance_tab = $maintenance.lease
        navigation = $navigation
        ui_before = $details.ui_before
        ui_after = $uiAfter
        durable_profile_readback = [pscustomobject]@{
            attempts = $profileReadAttempts
            elapsed_ms = $profileReadElapsedMs
            source_of_truth = [string]$after.pref_path
            permission_activation_pending_before = [bool]$permissionActivationEligible
            permission_activation_complete_after = [bool]$after.ready
        }
        daemon_bridge_before = $daemonBridgeBefore
        daemon_bridge_after = $daemonBridgeAfter
        before = $Before
        after = $after
    }
}

function Invoke-SynapseChromeBridgeAutoInstall {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ChromeUserDataRoot,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionId,
        [Parameter(Mandatory = $true)]
        [string]$ExtensionDir,
        [Parameter(Mandatory = $true)]
        [int]$TimeoutSeconds
    )

    $activeProfile = Get-SynapseActiveChromeProfileName `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -ExtensionId $ExtensionId `
        -ExtensionDir $ExtensionDir
    if ([string]::IsNullOrWhiteSpace($activeProfile)) {
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_ACTIVE_PROFILE_UNKNOWN user_data_root=$ChromeUserDataRoot remediation=open the intended authenticated Chrome profile, then rerun setup"
    }
    $before = Test-SynapseChromeBridgeProfileRow `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -ProfileName $activeProfile `
        -ExtensionId $ExtensionId `
        -ExtensionDir $ExtensionDir
    if ($SkipAutoInstall) {
        return [pscustomobject]@{
            attempted = $false
            changed = $false
            reason = 'skip_auto_install_requested'
            active_profile = $activeProfile
            before = $before
            after = $before
        }
    }

    if ($before.installed -and $before.manifest_path_matches) {
        $reloadEligible = $before.ready -or $before.permission_activation_pending
        if ($ReloadExistingExtensionViaUi -and $reloadEligible) {
            return Invoke-SynapseChromeBridgeExistingUiReload `
                -ChromeUserDataRoot $ChromeUserDataRoot `
                -ExtensionId $ExtensionId `
                -ExtensionDir $ExtensionDir `
                -ActiveProfile $activeProfile `
                -Before $before `
                -TimeoutSeconds $TimeoutSeconds
        }
        if ($before.ready) {
            return [pscustomobject]@{
                attempted = $true
                changed = $false
                reason = 'existing_ready_extension_host_ui_reload_deferred'
                active_profile = $activeProfile
                required_foreground = $false
                bridge_host_reload_command = 'browser_debugger.reload_bridge'
                before = $before
                after = $before
            }
        }
        if ($before.permission_activation_pending -and -not $ReloadExistingExtensionViaUi) {
            return [pscustomobject]@{
                attempted = $true
                changed = $false
                reason = 'existing_extension_permission_activation_host_ui_reload_deferred'
                active_profile = $activeProfile
                required_foreground = $false
                bridge_host_reload_command = 'browser_debugger.reload_bridge'
                before = $before
                after = $before
            }
        }

        $missing = if ($before.missing_active_api_permissions.Count -eq 0) { '<none>' } else { $before.missing_active_api_permissions -join ',' }
        $disableReasons = if ($before.disable_reasons.Count -eq 0) { '<none>' } else { $before.disable_reasons -join ',' }
        $remediation = if ($before.permission_activation_pending) {
            'invoke browser_debugger operation=reload_bridge against the handed-off daemon; the exact enabled/path-matching row is eligible for UI Reload and strict durable permission readback'
        } else {
            'repair the named disabled/missing-path profile condition before retrying; setup never reloads an ineligible extension row'
        }
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_EXISTING_EXTENSION_NOT_READY active_profile=$activeProfile ready=$($before.ready) enabled=$($before.enabled) permission_activation_pending=$($before.permission_activation_pending) missing_active_api_permissions=$missing disable_reasons=$disableReasons remediation=$remediation"
    }

    # Same extension ID already loaded, enabled, and fully permissioned, but registered
    # from a directory other than the current stable dir (e.g. a repo-checkout install).
    # Registration is now credentialed with a host-local token injected only into the
    # deployed stable copy. Reloading a non-stable source copy would keep an
    # uncredentialed worker alive, so normal setup must migrate the loaded unpacked
    # extension to the stable directory through Chrome's Load unpacked flow.
    if ($before.installed -and $before.enabled_and_permissioned -and $before.manifest_dir_exists) {
        Write-Warning ("SYNAPSE_CHROME_BRIDGE_NONSTABLE_PATH_REQUIRES_STABLE_RELOAD active_profile={0} loaded_manifest_path={1} expected_stable_path={2} reason=credentialed_register_token_only_injected_into_stable_extension" -f `
            $activeProfile, $before.manifest_path, $ExtensionDir)
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $daemonBridgeBefore = $null
    if ($OutputJson) {
        $daemonBridgeReadiness = Wait-SynapseChromeDaemonBridgeHealth `
            -BearerToken $bridgeBearerToken `
            -Deadline $deadline `
            -Purpose 'before_load' `
            -BudgetSeconds ([Math]::Max(5, [Math]::Min(15, [int]($TimeoutSeconds / 3))))
        $daemonBridgeBefore = $daemonBridgeReadiness.health
        if (-not $daemonBridgeReadiness.ok) {
            $readinessDetail = ConvertTo-CompressedJson -Value $daemonBridgeReadiness.readiness -Depth 6
            throw "SYNAPSE_CHROME_BRIDGE_DAEMON_HEALTH_BEFORE_LOAD_FAILED error=$($daemonBridgeReadiness.error) readiness=$readinessDetail remediation=daemon-controlled Load unpacked requires authenticated /health before creating a maintenance tab so replacement-host identity can be proven. The bounded startup-probe runway in readiness above already expired, so this is not a single unlucky sample: read last_error and repair the named daemon condition before retrying"
        }
    }
    $maintenance = Enter-SynapseOwnedChromeMaintenanceTab `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -Deadline $deadline
    $chromeWindow = $maintenance.window

    # The explicit empty query prevents Chrome omnibox history completion from
    # expanding the management root to the previously visited extension-details
    # URL before Enter.
    $navigation = Invoke-SynapseChromeAddressBarNavigation `
        -Window $chromeWindow `
        -ForegroundLease $maintenance.lease.foreground_lease `
        -ChromeUserDataRoot $ChromeUserDataRoot `
        -Url "chrome://extensions/?synapse_maintenance_token=$MaintenanceTabToken" `
        -ExpectedTitlePattern '^Extensions( - Google Chrome)?$' `
        -Deadline $deadline `
        -Purpose 'load_unpacked_extensions_page'

    $loadUnpacked = Wait-SynapseUntil -Deadline $deadline -Probe {
        $currentWindow = @(Get-SynapseChromeTopLevelWindows -ChromeUserDataRoot $ChromeUserDataRoot | Where-Object {
            $_.hwnd -eq $chromeWindow.hwnd
        } | Select-Object -First 1)
        if ($currentWindow.Count -eq 0) {
            return $null
        }
        if ($currentWindow[0].title -notmatch '^Extensions( - Google Chrome)?$') {
            return $null
        }
        $button = Find-SynapseAutomationElementByName `
            -Root $currentWindow[0].element `
            -Name 'Load unpacked' `
            -ControlType ([System.Windows.Automation.ControlType]::Button)
        if ($button) {
            return [pscustomobject]@{ window = $currentWindow[0]; button = $button }
        }
        $developerMode = Find-SynapseAutomationElementByAutomationId `
            -Root $currentWindow[0].element `
            -AutomationId 'devMode' `
            -ControlType ([System.Windows.Automation.ControlType]::Button)
        if (-not $developerMode) {
            throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_DEVELOPER_MODE_CONTROL_NOT_FOUND hwnd=$($currentWindow[0].hwnd) remediation=chrome://extensions did not expose the exact devMode button through UI Automation"
        }
        try {
            $developerModeToggle = $developerMode.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
            $developerModeState = $developerModeToggle.Current.ToggleState
            if ($developerModeState -eq [System.Windows.Automation.ToggleState]::Off) {
                $developerModeToggle.Toggle()
            } elseif ($developerModeState -ne [System.Windows.Automation.ToggleState]::On) {
                throw "unexpected_toggle_state=$developerModeState"
            }
        } catch {
            throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_DEVELOPER_MODE_ENABLE_FAILED hwnd=$($currentWindow[0].hwnd) error=$($_.Exception.Message) remediation=the exact devMode toggle must become On before Load unpacked can exist"
        }
        return $null
    }
    if (-not $loadUnpacked) {
        $latestWindow = Get-SynapseChromeWindowByHwnd -Hwnd $chromeWindow.hwnd
        $latestTitle = if ($latestWindow) { [string]$latestWindow.title } else { '<missing>' }
        $navigationReadback = ConvertTo-CompressedJson -Value $navigation -Depth 10
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_LOAD_UNPACKED_NOT_FOUND active_profile=$activeProfile timeout_s=$TimeoutSeconds chrome_window_hwnd=$($chromeWindow.hwnd) chrome_window_pid=$($chromeWindow.pid) latest_title=$latestTitle user_data_dir=$($chromeWindow.chrome_user_data_dir) match_reason=$($chromeWindow.chrome_profile_match_reason) navigation=$navigationReadback remediation=Chrome reached chrome://extensions but did not expose the Load unpacked button in the selected active-profile window; setup refuses to hunt controls in any other Chrome window"
    }
    Invoke-SynapseAutomationElement -Element $loadUnpacked.button -Description 'Load unpacked'

    $dialog = Wait-SynapseUntil -Deadline $deadline -Probe {
        Find-SynapseChromeFolderDialog -ChromeWindowHwnd $chromeWindow.hwnd -ChromeWindowPid $chromeWindow.pid
    }
    if (-not $dialog) {
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_FOLDER_DIALOG_NOT_FOUND active_profile=$activeProfile timeout_s=$TimeoutSeconds remediation=Chrome did not open the folder picker after Load unpacked"
    }
    [SynapseChromeBridgeAutoInstall.Win32]::SetForegroundWindow([IntPtr]$dialog.hwnd) | Out-Null
    Start-Sleep -Milliseconds 200
    $folderEdit = Find-SynapseAutomationElementByName `
        -Root $dialog.element `
        -Name 'Folder:' `
        -ControlType ([System.Windows.Automation.ControlType]::Edit)
    if (-not $folderEdit) {
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_FOLDER_FIELD_NOT_FOUND active_profile=$activeProfile dialog_hwnd=$($dialog.hwnd) remediation=folder picker did not expose the Folder field through UI Automation"
    }
    Set-SynapseAutomationEditValue -Element $folderEdit -Value $ExtensionDir -Description 'Folder:'
    Start-Sleep -Milliseconds 200
    $selectFolder = Find-SynapseAutomationElementByName `
        -Root $dialog.element `
        -Name 'Select Folder' `
        -ControlType ([System.Windows.Automation.ControlType]::Button)
    if (-not $selectFolder) {
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_SELECT_FOLDER_NOT_FOUND active_profile=$activeProfile dialog_hwnd=$($dialog.hwnd) remediation=folder picker did not expose Select Folder through UI Automation"
    }
    Invoke-SynapseAutomationElement -Element $selectFolder -Description 'Select Folder'

    $after = Wait-SynapseUntil -Deadline $deadline -Probe {
        $row = Test-SynapseChromeBridgeProfileRow `
            -ChromeUserDataRoot $ChromeUserDataRoot `
            -ProfileName $activeProfile `
            -ExtensionId $ExtensionId `
            -ExtensionDir $ExtensionDir
        if ($row.installed -and $row.manifest_path_matches) {
            return $row
        }
        return $null
    }
    if (-not $after) {
        $latest = Test-SynapseChromeBridgeProfileRow `
            -ChromeUserDataRoot $ChromeUserDataRoot `
            -ProfileName $activeProfile `
            -ExtensionId $ExtensionId `
            -ExtensionDir $ExtensionDir
        throw "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_PROFILE_ROW_MISSING active_profile=$activeProfile timeout_s=$TimeoutSeconds installed=$($latest.installed) manifest_path=$($latest.manifest_path) expected_path=$ExtensionDir remediation=Chrome did not persist the Synapse unpacked extension row after Select Folder"
    }
    $daemonBridgeAfter = $null
    if ($OutputJson) {
        $daemonBridgeAfter = Wait-SynapseChromeDaemonBridgeReplacement `
            -BearerToken $bridgeBearerToken `
            -Before $daemonBridgeBefore `
            -ExpectedExtensionId $ExtensionId `
            -ExpectedBuildId $bridgeBuildId `
            -ExpectedServiceWorkerSha256 $extensionDeploy.service_worker_sha256 `
            -Deadline $deadline
    }
    $installReason = if ($before.installed -and -not $before.manifest_path_matches) {
        'migrated_existing_extension_to_credentialed_stable_path'
    } else {
        'installed_unpacked_extension_in_active_profile'
    }
    return [pscustomobject]@{
        attempted = $true
        changed = $true
        reason = $installReason
        active_profile = $activeProfile
        required_foreground = $true
        chrome_window_hwnd = $chromeWindow.hwnd
        chrome_window_pid = $chromeWindow.pid
        chrome_window_user_data_dir = $chromeWindow.chrome_user_data_dir
        chrome_window_profile_match_reason = $chromeWindow.chrome_profile_match_reason
        folder_dialog_hwnd = $dialog.hwnd
        maintenance_tab = $maintenance.lease
        navigation = $navigation
        daemon_bridge_before = $daemonBridgeBefore
        daemon_bridge_after = $daemonBridgeAfter
        before = $before
        after = $after
    }
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
    $hasRemoteDebuggingPipe = $commandLine -match '(^|\s)--remote-debugging-pipe(\s|=|$)'
    $hasRemoteDebuggingPort = $commandLine -match '(^|\s)--remote-debugging-port(\s|=|$)'
    $hasSilentDebuggerSwitch = $commandLine -match '(^|\s)--silent-debugger-extension-api(\s|=|$)'
    $hasAutomationControlledFlag = $commandLine -match '(^|\s)--disable-blink-features=([^\s]*,)?AutomationControlled(,|\s|$)'
    $hasMsPlaywrightMcpDir = $commandLine -match 'ms-playwright-mcp'
    $layoutInfobarReasons = @()
    if ($hasAutomationControlledFlag) {
        $layoutInfobarReasons += 'unsupported_flag_disable_blink_features_automation_controlled'
    }
    if (($hasRemoteDebuggingPipe -or $hasRemoteDebuggingPort) -and -not $hasSilentDebuggerSwitch) {
        $layoutInfobarReasons += 'remote_debugging_without_silent_debugger_extension_api'
    }
    if ($hasMsPlaywrightMcpDir -and $hasAutomationControlledFlag) {
        $layoutInfobarReasons += 'headed_ms_playwright_mcp_layout_banner'
    }
    [pscustomobject]@{
        pid = [int]$_.ProcessId
        parent_pid = [int]$_.ParentProcessId
        creation_date = [string]$_.CreationDate
        command_line_readable = -not [string]::IsNullOrWhiteSpace($commandLine)
        has_silent_debugger_switch = $hasSilentDebuggerSwitch
        has_remote_debugging_pipe = $hasRemoteDebuggingPipe
        has_remote_debugging_port = $hasRemoteDebuggingPort
        has_automation_controlled_flag = $hasAutomationControlledFlag
        has_ms_playwright_mcp_dir = $hasMsPlaywrightMcpDir
        layout_infobar_risk = ($layoutInfobarReasons.Count -gt 0)
        layout_infobar_reasons = $layoutInfobarReasons
    }
})

$chromeUserDataRoot = Join-Path $env:LOCALAPPDATA 'Google\Chrome\User Data'
if ($CleanupOwnedMaintenanceTabViaUi) {
    try {
        $leaseBytes = [Convert]::FromBase64String($MaintenanceCleanupLeaseBase64)
        $strictUtf8 = New-Object System.Text.UTF8Encoding($false, $true)
        $leaseJson = $strictUtf8.GetString($leaseBytes)
        $cleanupLease = $leaseJson | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_LEASE_INVALID error=$($_.Exception.Message) remediation=pass the unmodified base64 JSON lease emitted by the immediately preceding installer operation"
    }
    if (
        [string]$cleanupLease.token -cne $MaintenanceTabToken -or
        [string]$cleanupLease.marker_url -cne $MaintenanceMarkerUrl -or
        [string]$cleanupLease.marker_title -cne $MaintenanceMarkerTitle -or
        [string]$cleanupLease.ownership_source -cne 'verified_uia_new_tab_marker' -or
        [string]::IsNullOrWhiteSpace([string]$cleanupLease.owned_tab_runtime_id) -or
        $cleanupLease.created_via_ui -ne $true -or
        [int64]$cleanupLease.chrome_window_hwnd -le 0 -or
        [int]$cleanupLease.chrome_window_pid -le 0 -or
        [int]$cleanupLease.expected_tab_count_after_cleanup -lt 0 -or
        $null -eq $cleanupLease.foreground_lease -or
        $null -eq $cleanupLease.foreground_lease.prior_foreground -or
        $cleanupLease.foreground_lease.prior_foreground.identity_valid -ne $true -or
        [int64]$cleanupLease.foreground_lease.prior_foreground.hwnd -le 0 -or
        [int]$cleanupLease.foreground_lease.prior_foreground.pid -le 0 -or
        [uint64]$cleanupLease.foreground_lease.prior_foreground.process_started_at_100ns -eq 0 -or
        [string]::IsNullOrWhiteSpace([string]$cleanupLease.foreground_lease.prior_foreground.executable_path) -or
        $null -eq $cleanupLease.foreground_lease.acquired_chrome_foreground -or
        $cleanupLease.foreground_lease.acquired_chrome_foreground.identity_valid -ne $true -or
        [int64]$cleanupLease.foreground_lease.acquired_chrome_foreground.hwnd -ne [int64]$cleanupLease.chrome_window_hwnd -or
        [int]$cleanupLease.foreground_lease.acquired_chrome_foreground.pid -ne [int]$cleanupLease.chrome_window_pid -or
        [uint64]$cleanupLease.foreground_lease.acquired_chrome_foreground.process_started_at_100ns -eq 0 -or
        [string]::IsNullOrWhiteSpace([string]$cleanupLease.foreground_lease.acquired_chrome_foreground.executable_path)
    ) {
        $detail = ConvertTo-CompressedJson -Value ([ordered]@{
            supplied_token = $MaintenanceTabToken
            supplied_marker_url = $MaintenanceMarkerUrl
            supplied_marker_title = $MaintenanceMarkerTitle
            lease = $cleanupLease
        }) -Depth 12
        throw "SYNAPSE_CHROME_MAINTENANCE_CLEANUP_LEASE_MISMATCH detail=$detail remediation=cleanup-only refuses any lease whose token, marker, UI ownership source, tab count, or exact prior/acquired HWND/PID/process-start/image identity differs"
    }
    $cleanupDeadline = (Get-Date).AddSeconds([Math]::Max(5, [Math]::Min(30, $AutoInstallTimeoutSeconds)))
    $cleanup = Close-SynapseOwnedChromeMaintenanceTabViaUi `
        -Lease $cleanupLease `
        -ChromeUserDataRoot $chromeUserDataRoot `
        -Deadline $cleanupDeadline
    $cleanupOnlyResult = [pscustomobject]@{
        ok = $true
        operation = 'cleanup_owned_maintenance_tab_via_ui'
        extension_id = $ExtensionId
        maintenance_cleanup = $cleanup
    }
    if ($OutputJson) {
        $json = ConvertTo-CompressedJson -Value $cleanupOnlyResult -Depth 24
        $payload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))
        Write-Output "SYNAPSE_CHROME_BRIDGE_INSTALLER_JSON_V1=$payload"
    } else {
        $cleanupOnlyResult
    }
    return
}

$script:SynapseChromeMaintenanceTabLease = $null
$script:SynapseChromeMaintenanceForegroundLease = $null
try {
    $chromeBridgeAutoInstall = Invoke-SynapseChromeBridgeAutoInstall `
        -ChromeUserDataRoot $chromeUserDataRoot `
        -ExtensionId $ExtensionId `
        -ExtensionDir $extensionDir `
        -TimeoutSeconds $AutoInstallTimeoutSeconds
} catch {
    $operationError = $_.Exception.Message
    if (
        $operationError -like 'SYNAPSE_CHROME_MAINTENANCE_FOREGROUND_OPERATOR_SUPERSEDED_BEFORE_NAVIGATION *' -and
        $script:SynapseChromeMaintenanceTabLease -and
        $script:SynapseChromeMaintenanceForegroundLease
    ) {
        $foregroundDeadline = (Get-Date).AddSeconds([Math]::Max(5, [Math]::Min(30, $AutoInstallTimeoutSeconds)))
        $foreground = Restore-SynapseChromeMaintenanceForeground `
            -ForegroundLease $script:SynapseChromeMaintenanceForegroundLease `
            -Deadline $foregroundDeadline
        $foregroundJson = ConvertTo-CompressedJson -Value $foreground -Depth 10
        throw "SYNAPSE_CHROME_MAINTENANCE_OPERATION_FAILED operation_error=$operationError cleanup=delegated_to_daemon_exact_bridge_ownership_cleanup foreground_transaction=$foregroundJson token=$($script:SynapseChromeMaintenanceTabLease.token) remediation=the operator superseded the maintenance foreground before navigation; the installer performed no further Chrome UI mutation, preserved the exact current foreground, and the daemon must close only the exact marker URL/title tab through the pre-operation bridge lease"
    }
    if ($script:SynapseChromeMaintenanceTabLease) {
        $cleanupDeadline = (Get-Date).AddSeconds([Math]::Max(5, [Math]::Min(30, $AutoInstallTimeoutSeconds)))
        try {
            $cleanup = Close-SynapseOwnedChromeMaintenanceTabViaUi `
                -Lease $script:SynapseChromeMaintenanceTabLease `
                -ChromeUserDataRoot $chromeUserDataRoot `
                -Deadline $cleanupDeadline
            $foregroundJson = ConvertTo-CompressedJson -Value $cleanup.foreground_transaction -Depth 10
            throw "SYNAPSE_CHROME_MAINTENANCE_OPERATION_FAILED operation_error=$operationError cleanup=verified_absent cleanup_method=$($cleanup.method) foreground_transaction=$foregroundJson token=$($cleanup.token)"
        } catch {
            if ($_.Exception.Message -like 'SYNAPSE_CHROME_MAINTENANCE_OPERATION_FAILED *') {
                throw
            }
            $cleanupError = $_.Exception.Message
            throw "SYNAPSE_CHROME_MAINTENANCE_OPERATION_AND_CLEANUP_FAILED operation_error=$operationError cleanup_error=$cleanupError token=$($script:SynapseChromeMaintenanceTabLease.token) hwnd=$($script:SynapseChromeMaintenanceTabLease.chrome_window_hwnd) chrome_tab_id=$($script:SynapseChromeMaintenanceTabLease.chrome_tab_id) remediation=inspect the exact ownership identity and remove only that tab after proving its marker/token URL"
        }
    }
    if ($script:SynapseChromeMaintenanceForegroundLease) {
        $foregroundDeadline = (Get-Date).AddSeconds([Math]::Max(5, [Math]::Min(30, $AutoInstallTimeoutSeconds)))
        try {
            $foreground = Restore-SynapseChromeMaintenanceForeground `
                -ForegroundLease $script:SynapseChromeMaintenanceForegroundLease `
                -Deadline $foregroundDeadline
            $foregroundJson = ConvertTo-CompressedJson -Value $foreground -Depth 10
            throw "SYNAPSE_CHROME_MAINTENANCE_OPERATION_FAILED operation_error=$operationError foreground_transaction=$foregroundJson"
        } catch {
            if ($_.Exception.Message -like 'SYNAPSE_CHROME_MAINTENANCE_OPERATION_FAILED *') {
                throw
            }
            throw "SYNAPSE_CHROME_MAINTENANCE_OPERATION_AND_FOREGROUND_RESTORE_FAILED operation_error=$operationError foreground_error=$($_.Exception.Message) remediation=repair the named operation error and exact foreground transaction error; no substitute window was focused"
        }
    }
    throw
}
if ($script:SynapseChromeMaintenanceTabLease -and -not $OutputJson) {
    $cleanupDeadline = (Get-Date).AddSeconds([Math]::Max(5, [Math]::Min(30, $AutoInstallTimeoutSeconds)))
    $standaloneCleanup = Close-SynapseOwnedChromeMaintenanceTabViaUi `
        -Lease $script:SynapseChromeMaintenanceTabLease `
        -ChromeUserDataRoot $chromeUserDataRoot `
        -Deadline $cleanupDeadline
    $chromeBridgeAutoInstall | Add-Member -NotePropertyName maintenance_cleanup -NotePropertyValue $standaloneCleanup -Force
}
$profileDirs = @()
$synapseChromeProfileReadback = @()
$staleSynapseActivePermissions = @()
$staleSynapseGrantedPermissions = @()
$externalDebuggerOrNativeExtensions = @()
$externalDisabledDebuggerOrNativeExtensions = @()
$externalDebuggerExtensions = @()
function Get-ChromeExtensionRuntimeState {
    param(
        [Parameter(Mandatory = $true)]
        $Setting
    )

    $state = $null
    if ($Setting.PSObject.Properties.Name -contains 'state' -and $null -ne $Setting.state) {
        $state = [int]$Setting.state
    }
    $activeBit = $null
    if ($Setting.PSObject.Properties.Name -contains 'active_bit') {
        $activeBit = [bool]$Setting.active_bit
    }
    $disableReasons = @()
    if ($Setting.PSObject.Properties.Name -contains 'disable_reasons' -and $null -ne $Setting.disable_reasons) {
        $disableReasons = @($Setting.disable_reasons)
    }
    # Chromium persists extension state as DISABLED=0, ENABLED=1. Stale
    # permission rows can remain without state; the live chrome.management
    # bridge readback is the stronger authority for enabled hazards.
    $runtimeEnabled = (($state -eq 1) -and ($disableReasons.Count -eq 0))

    [pscustomobject]@{
        state = $state
        active_bit = $activeBit
        disable_reasons = $disableReasons
        runtime_enabled = $runtimeEnabled
    }
}

function Test-ExternalPopupRiskEnabled {
    param(
        [Parameter(Mandatory = $true)]
        [object]$RuntimeState,
        [Parameter(Mandatory = $true)]
        [bool]$HasActiveOrManifestHazard,
        [Parameter(Mandatory = $true)]
        [bool]$HasGrantedHazard
    )

    if ($RuntimeState.disable_reasons.Count -gt 0 -or $RuntimeState.state -eq 0) {
        return $false
    }
    if ($RuntimeState.state -eq 1) {
        return ($HasActiveOrManifestHazard -or $HasGrantedHazard)
    }
    # Stale granted-only residue is advisory, but an external active/manifest
    # debugger permission with no disable reason can still surface Chrome's
    # layout-shifting "started debugging this browser" infobar.
    return $HasActiveOrManifestHazard
}
if (Test-Path -LiteralPath $chromeUserDataRoot -PathType Container) {
    $profileDirs = @(Get-ChildItem -LiteralPath $chromeUserDataRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Name -ne 'Snapshots' -and (
                (Test-Path -LiteralPath (Join-Path $_.FullName 'Preferences') -PathType Leaf) -or
                (Test-Path -LiteralPath (Join-Path $_.FullName 'Secure Preferences') -PathType Leaf)
            )
        })
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
                $manifestApi = @()
                if ($setting.manifest -and $setting.manifest.permissions) {
                    $manifestApi = @($setting.manifest.permissions | Where-Object {
                        $_ -is [string]
                    })
                }
                if ($extensionProperty.Name -eq $ExtensionId) {
                    $synapseActiveHazardApi = @(
                        @($activeApi)
                        @($manifestApi)
                    ) | Where-Object {
                        $_ -eq 'nativeMessaging'
                    } | Sort-Object -Unique
                    $synapseGrantedHazardApi = @($grantedApi | Where-Object {
                        $_ -eq 'nativeMessaging'
                    } | Sort-Object -Unique)
                    $row = [pscustomobject]@{
                        profile = $profileDir.Name
                        pref_file = $prefFileName
                        path = $prefPath
                        manifest_path = $setting.path
                        active_api = $activeApi
                        granted_api = $grantedApi
                        manifest_api = $manifestApi
                        active_or_manifest_hazard_api = $synapseActiveHazardApi
                        granted_hazard_api = $synapseGrantedHazardApi
                        state = $runtimeState.state
                        active_bit = $runtimeState.active_bit
                        disable_reasons = $runtimeState.disable_reasons
                        runtime_enabled = $runtimeState.runtime_enabled
                    }
                    $synapseChromeProfileReadback += $row
                    if ($synapseActiveHazardApi.Count -gt 0) {
                        $staleSynapseActivePermissions += $row
                    } elseif ($synapseGrantedHazardApi.Count -gt 0) {
                        $staleSynapseGrantedPermissions += $row
                    }
                } else {
                    $activeOrManifestHazardApi = @(
                        @($activeApi)
                        @($manifestApi)
                    ) | Where-Object {
                        $_ -eq 'debugger' -or $_ -eq 'nativeMessaging'
                    } | Sort-Object -Unique
                    $grantedHazardApi = @($grantedApi | Where-Object {
                        $_ -eq 'debugger' -or $_ -eq 'nativeMessaging'
                    } | Sort-Object -Unique)
                    $hazardApi = @(
                        @($activeOrManifestHazardApi)
                        @($grantedApi)
                    ) | Where-Object {
                        $_ -eq 'debugger' -or $_ -eq 'nativeMessaging'
                    } | Sort-Object -Unique
                    if ($hazardApi.Count -eq 0) {
                        continue
                    }
                    $popupRiskEnabled = Test-ExternalPopupRiskEnabled `
                        -RuntimeState $runtimeState `
                        -HasActiveOrManifestHazard ($activeOrManifestHazardApi.Count -gt 0) `
                        -HasGrantedHazard ($grantedHazardApi.Count -gt 0)
                    $externalRow = [pscustomobject]@{
                        profile = $profileDir.Name
                        pref_file = $prefFileName
                        extension_id = $extensionProperty.Name
                        name = $setting.manifest.name
                        location = $setting.location
                        manifest_path = $setting.path
                        active_api = $activeApi
                        granted_api = $grantedApi
                        manifest_api = $manifestApi
                        active_or_manifest_hazard_api = $activeOrManifestHazardApi
                        granted_hazard_api = $grantedHazardApi
                        hazard_api = $hazardApi
                        state = $runtimeState.state
                        active_bit = $runtimeState.active_bit
                        disable_reasons = $runtimeState.disable_reasons
                        runtime_enabled = $runtimeState.runtime_enabled
                        popup_risk_enabled = $popupRiskEnabled
                        risk_basis = if ($popupRiskEnabled) {
                            if ($runtimeState.state -eq 1) {
                                'state_enabled_hazard'
                            } elseif ($activeOrManifestHazardApi.Count -gt 0) {
                                'active_or_manifest_hazard_without_disable_reason'
                            } else {
                                'state_enabled_granted_hazard'
                            }
                        } else {
                            'disabled_or_granted_only_stale'
                        }
                    }
                    if ($popupRiskEnabled) {
                        $externalDebuggerOrNativeExtensions += $externalRow
                        if ($hazardApi -contains 'debugger') {
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
$activeChromeProfile = Get-SynapseActiveChromeProfileName `
    -ChromeUserDataRoot $chromeUserDataRoot `
    -ExtensionId $ExtensionId `
    -ExtensionDir $extensionDir
$synapseChromeInstalledProfiles = @(
    $synapseChromeProfileReadback |
        Where-Object { $_.PSObject.Properties.Name -contains 'manifest_path' } |
        ForEach-Object { [string]$_.profile } |
        Sort-Object -Unique
)
$synapseChromeActiveProfileInstalled = $null
if (-not [string]::IsNullOrWhiteSpace($activeChromeProfile)) {
    $synapseChromeActiveProfileInstalled = @($synapseChromeInstalledProfiles) -contains $activeChromeProfile
}
$synapseChromeProfileInstallReason = if ($profileDirs.Count -eq 0) {
    'no_profile_dirs'
} elseif ($synapseChromeInstalledProfiles.Count -eq 0) {
    'extension_id_absent_from_preferences_and_secure_preferences'
} elseif ($synapseChromeActiveProfileInstalled -eq $false) {
    'active_profile_missing_extension'
} else {
    'extension_profile_row_present'
}
$synapseChromeProfileInstallState = [pscustomobject]@{
    scanned = $true
    installed = ($synapseChromeInstalledProfiles.Count -gt 0)
    auto_install = $chromeBridgeAutoInstall
    chrome_user_data_root = $chromeUserDataRoot
    profile_count = $profileDirs.Count
    installed_profile_count = $synapseChromeInstalledProfiles.Count
    installed_profiles = @($synapseChromeInstalledProfiles)
    active_profile = $activeChromeProfile
    active_profile_installed = $synapseChromeActiveProfileInstalled
    reason = $synapseChromeProfileInstallReason
    browser_debugger_reload_bridge_can_install_absent_extension = $true
    remediation = 'call browser_debugger operation=reload_bridge with profile=browser_debugger; the host-controlled path deploys the bundled bridge, invokes the exact Reload or Load unpacked control in the already-open active profile, and separately verifies the physical profile row plus a new authenticated daemon host'
}
$staleBridgeBuildCleanup = Remove-SynapseStaleChromeBridgeBuildDirs `
    -StableRoot $stableRootFull `
    -CurrentExtensionDir $extensionDir `
    -ChromeUserDataRoot $chromeUserDataRoot `
    -ExtensionId $ExtensionId
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

$externalNativeMessagingProcessRows = @($externalNativeMessagingProcesses | ForEach-Object {
    if ($_.ExtensionId -match '^[a-p]{32}$') {
        [pscustomobject]@{
            profile = 'process'
            pref_file = 'process'
            extension_id = $_.ExtensionId
            name = 'external nativeMessaging process'
            location = $null
            manifest_path = $null
            active_api = @('nativeMessaging')
            active_bit = $null
            disable_reasons = @()
            runtime_enabled = $true
            source = 'native_messaging_process'
        }
    }
})
$externalLayoutInfobarProcesses = @($chromeProcesses | Where-Object {
    $_.layout_infobar_risk
} | ForEach-Object {
    [pscustomobject]@{
        pid = $_.pid
        parent_pid = $_.parent_pid
        reasons = @($_.layout_infobar_reasons)
        has_remote_debugging_pipe = $_.has_remote_debugging_pipe
        has_remote_debugging_port = $_.has_remote_debugging_port
        has_silent_debugger_switch = $_.has_silent_debugger_switch
        has_automation_controlled_flag = $_.has_automation_controlled_flag
        has_ms_playwright_mcp_dir = $_.has_ms_playwright_mcp_dir
    }
})
$allExternalDebuggerOrNativeExtensions = @(
    @($externalDebuggerOrNativeExtensions | ForEach-Object {
        $_ | Add-Member -NotePropertyName source -NotePropertyValue 'chrome_profile_active' -Force -PassThru
    })
    @($externalDisabledDebuggerOrNativeExtensions | ForEach-Object {
        $_ | Add-Member -NotePropertyName source -NotePropertyValue 'chrome_profile_disabled_or_inactive' -Force -PassThru
    })
    @($externalNativeMessagingProcessRows)
)
$externalHazardExtensionIds = @(
    @($allExternalDebuggerOrNativeExtensions | ForEach-Object { $_.extension_id })
    @($externalNativeMessagingProcesses | ForEach-Object { $_.ExtensionId })
) | Where-Object { $_ -match '^[a-p]{32}$' } | Sort-Object -Unique

$synapseSelfShieldRow = [pscustomobject]@{
    extension_id = $ExtensionId
    name = 'Synapse Chrome Bridge'
    active_api = @()
    granted_api = @()
    manifest_api = @()
    hazard_api = @('nativeMessaging')
    runtime_enabled = $true
    source = 'synapse_self_bridge_invariant'
}

# External extensions and native-messaging hosts that use debugger/nativeMessaging
# can surface layout-changing Chrome popups independently of Synapse's tabs-only
# bridge. Setup tries to apply a reversible HKCU ExtensionSettings shield for
# those permissions by default. If this host denies that policy write, the
# installed bridge must suppress the hazards through chrome.management or normal
# tabs/scripting commands fail closed before queueing any Chrome command.
#
# As a one-way remediation we remove stale Synapse-authored blockers from prior
# builds, then write the current self-shield for the Synapse extension ID. The
# current bridge intentionally requests debugger for a narrow target-scoped CDP
# input lane, so the self-shield blocks nativeMessaging only.
$chromePolicyCleanup = Remove-SynapseChromeExternalDebuggerPolicy -PreserveExtensionIds @(
    @($externalHazardExtensionIds)
)
$policyShieldExtensions = @($synapseSelfShieldRow)
if (-not $PreserveExternalDebuggerExtensions) {
    $policyShieldExtensions += @($allExternalDebuggerOrNativeExtensions)
}
$chromePolicyPopupShield = Set-SynapseChromeExternalDebuggerPolicy -Extensions $policyShieldExtensions
# The popup shield writes HKCU\Software\Policies\Google\Chrome\ExtensionSettings — a
# Chrome MANAGED-POLICY key. On a correctly-secured Windows install that key is
# admin-only (owner SYSTEM, standard users get ReadKey) BY DESIGN, precisely so a
# non-admin process cannot grant itself Chrome policy. A non-elevated setup therefore
# cannot write it, and that denial is expected hardening — not a misconfiguration to
# "repair". Crucially, the popup shield is NOT the hazard enforcement boundary: the
# bridge manifest is REQUIRED to hold chrome.management (enforced earlier in this
# script), and the daemon uses it to suppress the same debugger/nativeMessaging hazards
# at runtime and to FAIL CLOSED before any Chrome command if suppression is not
# confirmed — this is verified post-handoff by the live /health chrome_bridge check in
# synapse-setup.ps1. Making an admin-only, defense-in-depth policy write a hard
# dependency would brick `synapse-update` on every properly-secured machine. So a
# write-denied shield is surfaced LOUDLY (with exact failure + remediation) but is
# non-fatal; only an UNEXPECTED shield failure (not the known admin-only policy-root
# denial) still aborts setup.
$popupShieldWriteDeniedRows = @($chromePolicyPopupShield | Where-Object {
    [string]$_.warning_code -eq 'SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_DENIED'
})
$unexpectedBlockingShieldRows = @($chromePolicyPopupShield | Where-Object {
    $_.blocking -eq $true -and [string]$_.warning_code -ne 'SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_DENIED'
})
if ($unexpectedBlockingShieldRows.Count -gt 0) {
    $detail = ConvertTo-CompressedJson -Value $unexpectedBlockingShieldRows -Depth 10
    throw "SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_FAILED detail=$detail remediation=the Synapse popup shield failed for an unexpected reason (not the admin-only HKCU managed-policy-root write denial); inspect detail.error/detail.acl and repair before rerunning setup"
}
if ($popupShieldWriteDeniedRows.Count -gt 0) {
    $deniedDetail = ConvertTo-CompressedJson -Value $popupShieldWriteDeniedRows -Depth 10
    Write-Warning "SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_DENIED_NONBLOCKING the HKCU Chrome managed-policy key is admin-only on this host, so the non-elevated popup shield could not be written. This is not fatal: the bridge's required chrome.management permission enforces the same debugger/nativeMessaging hazard suppression at runtime and fails closed if suppression is not confirmed (verified post-handoff by /health chrome_bridge). To also apply the policy-level defense-in-depth, run setup once from an elevated PowerShell. detail=$deniedDetail"
}

if ($staleSynapseActivePermissions.Count -gt 0) {
    $detail = [ordered]@{
        extension_id = $ExtensionId
        stale_active_permissions = $staleSynapseActivePermissions
        chrome_policy_popup_shield = $chromePolicyPopupShield
    } | ConvertTo-Json -Depth 8 -Compress
    throw "SYNAPSE_CHROME_EXTENSION_STALE_ACTIVE_NATIVE_MESSAGING_PERMISSION extension_id=$ExtensionId detail=$detail remediation=Synapse attempted to apply/preserve the HKCU ExtensionSettings self-shield for nativeMessaging and included the physical policy write result in detail.chrome_policy_popup_shield; call browser_debugger operation=reload_bridge through the real Synapse MCP public facade so the host controls the exact Chrome extension management Reload action and verifies the replacement profile/host state"
}

$result = [pscustomobject]@{
    ok = $true
    native_host = $hostName
    native_manifest = $null
    registry_key = $registryPath
    binary = $null
    extension_id = $ExtensionId
    extension_source_dir = $extensionSourceDir
    extension_dir = $extensionDir
    extension_deploy = $extensionDeploy
    daemon_bridge_transport = 'direct_localhost_websocket'
    daemon_bridge_origin = "chrome-extension://$ExtensionId"
    bridge_host_reload_command = 'browser_debugger.reload_bridge'
    bridge_host_reload_control_surface = 'chrome_extensions_exact_reload_button'
    bridge_build_id_expected = $bridgeBuildId
    bridge_declared_build_sha256_expected = $bridgeDeclaredBuildSha256
    bridge_build_sha256_expected = $bridgeDeclaredBuildSha256
    bridge_service_worker_sha256_expected = $extensionDeploy.service_worker_sha256
    bridge_required_capabilities = @('alarmReconnect', 'activateTab', 'ariaSnapshot', 'assertPoll', 'cdpInput', 'evaluateScript', 'initScript', 'exposeBinding', 'handleDialog', 'fileUpload', 'operatorPanicDisable', 'operatorPanicCleanup', 'operatorPanicReadback', 'operatorPanicEnable', 'viewportEmulation', 'deviceEmulation', 'geolocationEmulation', 'localeEmulation', 'mediaEmulation', 'networkConditions', 'closeTab', 'clock', 'coordinateClick', 'cookies', 'downloads', 'domAction', 'keyDispatch', 'externalPopupRiskSuppression', 'frameLocators', 'frames', 'inspectElement', 'listTabs', 'locateElements', 'navigateTab', 'openTab', 'pageEvents', 'pageVitals', 'pageContent', 'pageScreenshot', 'pagePdf', 'scrollIntoView', 'setContent', 'storageState', 'waitForFunction', 'waitForLoadState', 'waitForUrl', 'waitForRequest', 'waitForResponse', 'waitForSelector', 'waitForText', 'targetInfo', 'targetInfoPageText', 'typeActiveElement', 'setFieldValue')
    background_navigation_backend = 'chrome.tabs_plus_chrome.scripting_executeScript_plus_chrome.cookies_plus_chrome.downloads_plus_chrome.webNavigation_plus_chrome.webRequest_plus_chrome_tabs_captureVisibleTab_for_typed_dom_actions_storage_cookies_downloads_waits_page_screenshots_and_chrome_debugger_runtime_evaluate_init_scripts_handle_dialog_file_upload_cdp_input_hover_tap_drag_page_print_to_pdf_viewport_emulation_device_emulation_geolocation_emulation_locale_emulation_media_emulation_and_network_conditions_no_native_messaging_plus_chrome.management_external_popup_suppression'
    reconnect_driver = 'bounded_websocket_reconnect_with_persisted_read_verified_chrome_alarms_mv3_wake'
    attach_popup_prevention = 'normal_bridge_debugger_permission_scoped_to_Runtime_evaluate_Page_addScriptToEvaluateOnNewDocument_Runtime_addBinding_Page_handleJavaScriptDialog_DOM_setFileInputFiles_Page_fileChooserOpened_cdpInput_hover_tap_active_drag_pagePdf_printToPDF_viewportEmulation_deviceEmulation_geolocationEmulation_localeEmulation_mediaEmulation_and_networkConditions_inactive_synthetic_drag_no_helper_windows_no_nativeMessaging_permission_plus_external_popup_risk_suppression'
    normal_bridge_attach_commands_available = $true
    normal_bridge_debugger_api_calls_present = $true
    expected_extension_id_guard_present = $true
    required_alarms_permission_present = ($requiredPermissions -contains 'alarms')
    recurring_wakeup_permission_present = ($requiredPermissions -contains 'alarms')
    required_cookies_permission_present = ($requiredPermissions -contains 'cookies')
    required_downloads_permission_present = ($requiredPermissions -contains 'downloads')
    required_debugger_permission_present = ($requiredPermissions -contains 'debugger')
    optional_debugger_permission_present = $false
    required_management_permission_present = ($requiredPermissions -contains 'management')
    required_web_navigation_permission_present = ($requiredPermissions -contains 'webNavigation')
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
    chrome_policy_cleanup = $chromePolicyCleanup
    chrome_policy_popup_shield = $chromePolicyPopupShield
    external_popup_risk_blocks_popup_free_commands = ($allExternalDebuggerOrNativeExtensions.Count -gt 0)
    external_popup_risk_scope = 'runtime_bridge_management_suppression_or_fail_closed'
    external_popup_risk_block_reason = if ($allExternalDebuggerOrNativeExtensions.Count -gt 0) { 'external_debugger_or_native_hazards_require_chrome_management_suppression_or_policy_shield' } else { 'none' }
    external_popup_risk_bridge_management_required = ($allExternalDebuggerOrNativeExtensions.Count -gt 0)
    external_popup_risk_bridge_management_permission_present = ($requiredPermissions -contains 'management')
    synapse_chrome_auto_install = $chromeBridgeAutoInstall
    synapse_chrome_profile_install_state = $synapseChromeProfileInstallState
    stale_bridge_build_cleanup = $staleBridgeBuildCleanup
    synapse_chrome_profile_readback = $synapseChromeProfileReadback
    synapse_stale_granted_permission_warning = [pscustomobject]@{
        warning = ($staleSynapseGrantedPermissions.Count -gt 0)
        scope = 'profile_granted_permissions_only_not_runtime_active'
        rows = $staleSynapseGrantedPermissions
    }
    external_hazard_extension_ids = $externalHazardExtensionIds
    external_debugger_or_native_extensions = $externalDebuggerOrNativeExtensions
    external_disabled_debugger_or_native_extensions = $externalDisabledDebuggerOrNativeExtensions
    external_debugger_extensions = $externalDebuggerExtensions
    external_native_messaging_processes = $externalNativeMessagingProcesses
    external_layout_infobar_processes = $externalLayoutInfobarProcesses
}

if ($OutputJson) {
    $json = ConvertTo-CompressedJson -Value $result -Depth 24
    $payload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))
    Write-Output "SYNAPSE_CHROME_BRIDGE_INSTALLER_JSON_V1=$payload"
} else {
    $result
}
