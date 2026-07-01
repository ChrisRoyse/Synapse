<#
.SYNOPSIS
  Windows-side Synapse setup: build/install the daemon binary, deploy bundled
  profiles, generate the bearer token, register the auto-start HTTP daemon, and
  (optionally) wire the Windows-side MCP clients. Idempotent and fail-loud.

.DESCRIPTION
  Synapse has exactly ONE controlling body: the Windows-native synapse-mcp.exe
  HTTP daemon. It is the only process that can do real Win32 SendInput / UI
  Automation / WGC-DXGI capture, and it controls BOTH Windows programs (native
  windows) and WSL programs (WSLg GUI apps render as real Windows windows;
  act_run_shell / act_launch reach WSL CLIs via wsl.exe). Every MCP client — on
  Windows or in WSL — connects to this one daemon.

  This script makes that body exist and run, then points the Windows-side
  clients at it. The WSL-side entry (scripts/synapse-install.sh) calls this same
  script through interop and then wires the WSL-side clients.

  Robustness decisions baked in here (learned the hard way):
    * Build from the LOCAL source path (cd into -SourceDir). Building over a
      \\wsl.localhost / pushd-mapped drive bakes transient Z:\ paths into the
      binary (CARGO_MANIFEST_DIR) and intermittently fails cargo's dep-info
      step. -SourceDir must be a real local path.
    * Deploy the bundled profiles NEXT TO the installed exe so the daemon's
      executable-relative profile lookup always resolves, and ALSO pass
      --profile-dir explicitly. A compile-time CARGO_MANIFEST_DIR profile path
      never exists on an installed host.
    * Use a persistent CARGO_TARGET_DIR so re-installs are incremental, not a
      ~25-minute RocksDB rebuild every time.

  Nothing here silently falls back: every prerequisite is checked and throws a
  clear error naming exactly what failed and how to fix it.

.PARAMETER SourceDir
  Path to a LOCAL synapse source checkout to build from. Required unless
  -SkipBuild is set. Must be on a real local drive (not \\wsl.localhost or a
  pushd-mapped UNC drive).

.PARAMETER SkipBuild
  Do not build; require an already-installed synapse-mcp.exe at -ExePath.

.PARAMETER BuildTimeoutMinutes
  Maximum time to allow the release build to run. The build process tree is
  launched inside a Windows Job Object with kill-on-close, so Cargo/rustc
  children cannot survive if this setup process exits or is killed.

.PARAMETER ForceRestart
  Permit setup/remove to stop the shared daemon even when active HTTP MCP
  sessions, live client TCP connections, or bridge children are present. Without
  this explicit maintenance flag, setup fails closed instead of interrupting
  another agent. The normal stop path is authenticated graceful shutdown; this
  flag also permits an exact-PID forced stop only after the graceful path fails
  for a verified legacy or unresponsive synapse-mcp.exe process.

.PARAMETER Bind
  Loopback address the daemon binds. Default 127.0.0.1:7700.

.PARAMETER ChromeNativeHostExePath
  Legacy diagnostic native-host path. The normal end-user Chrome bridge uses
  direct localhost HTTP registration plus WebSocket command delivery; setup
  does not install or launch native messaging because Chrome may create a
  visible cmd.exe wrapper for native hosts.

  Synapse applies a reversible HKCU Chrome ExtensionSettings popup shield for
  external debugger/nativeMessaging hazards by default, identified by a
  Synapse-authored blocked_install_message marker. It also preserves a
  self-shield for the stable Synapse extension ID so an older loaded bridge
  build cannot retain nativeMessaging capability. Background automation is
  still achieved on Synapse's own side: the bundled bridge uses direct
  localhost WebSocket, never nativeMessaging/helper Chrome windows, and exposes
  narrow chrome.debugger lanes for target-scoped hover/tap/active-tab drag,
  dialog handling, and viewport/device/geolocation/locale/media/network
  emulation plus inactive-tab synthetic mouse drag and HTML5 DataTransfer drag
  dispatch in the already-open authenticated Chrome profile.

.PARAMETER MaintenanceLockPath
  File-lock Source of Truth that serializes setup/remove across multiple
  agents. The file contents name the owning PID and cleanup policy; the held
  FileStream is the actual lock and is released by Windows when setup exits.

.PARAMETER WireClients
  Wire the Windows-side MCP clients (Claude Code and Codex via HTTP, Claude
  Desktop via the connect bridge). Default $true.

.PARAMETER Remove
  Uninstall: stop + unregister the scheduled task. Leaves the DB, token, and
  binary in place unless -Purge is also given.

.PARAMETER Purge
  With -Remove, also delete the daemon DB, deployed profiles, and token.

.PARAMETER ActiveIssue
  Optional current GitHub issue number/ref to preserve in Codex restart
  handoffs. Defaults to SYNAPSE_ACTIVE_ISSUE when set. Accepts 1441, #1441, or
  a ChrisRoyse/Synapse issue URL.
#>
[CmdletBinding()]
param(
    [string]$SourceDir,
    [switch]$SkipBuild,
    [string]$Bind        = '127.0.0.1:7700',
    [string]$ExePath     = "$env:USERPROFILE\.cargo\bin\synapse-mcp.exe",
    [string]$ChromeNativeHostExePath = "$env:USERPROFILE\.cargo\bin\synapse-chrome-native-host.exe",
    [string]$CargoTarget = "$env:LOCALAPPDATA\synapse\build-target",
    [string]$DbPath      = "$env:LOCALAPPDATA\synapse\db-daemon",
    [string]$ProfilesDir = "$env:USERPROFILE\.cargo\bin\profiles",
    [string]$LogDir      = "$env:LOCALAPPDATA\synapse\logs",
    [string]$TokenPath   = "$env:APPDATA\synapse\token.txt",
    [string]$CodexToolSurfaceSnapshotPath = "$env:APPDATA\synapse\codex-tool-surface.json",
    [string]$ActiveIssue = $env:SYNAPSE_ACTIVE_ISSUE,
    [string]$TaskName    = 'SynapseMcpDaemon',
    [string]$MaintenanceLockPath = "$env:LOCALAPPDATA\synapse\setup-maintenance.lock.json",
    [ValidateRange(1, 1440)][int]$BuildTimeoutMinutes = 90,
    [switch]$ForceRestart,
    [switch]$SkipClientWiring,
    [switch]$Remove,
    [switch]$Purge
)

$ErrorActionPreference = 'Stop'
function Info($m)  { Write-Host "[synapse-setup] $m" }
function Step($m)  { Write-Host "`n=== $m ===" -ForegroundColor Cyan }
function Die($m)   { throw "[synapse-setup] FATAL: $m" }

function Invoke-SynapseChromeBridgeVerifier {
    param(
        [Parameter(Mandatory = $true)]
        [string]$InstallerPath,
        [Parameter(Mandatory = $true)]
        [string]$NativeHostExePath
    )

    if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) {
        Die "SYNAPSE_CHROME_BRIDGE_INSTALLER_MISSING path=$InstallerPath remediation=setup requires the repo script that verifies the direct localhost Chrome bridge and removes stale nativeMessaging registration"
    }
    $chromeBridgeArgs = @{
        SynapseNativeHostExe = $NativeHostExePath
    }
    $readback = & $InstallerPath @chromeBridgeArgs
    if (-not $readback.ok) {
        Die "SYNAPSE_CHROME_BRIDGE_INSTALLER_FAILED path=$InstallerPath remediation=installer did not return ok=true"
    }
    $autoInstall = $readback.synapse_chrome_auto_install
    if (-not $autoInstall) {
        Die "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_READBACK_MISSING path=$InstallerPath remediation=setup requires the bridge installer to report synapse_chrome_auto_install so skipped or failed active-profile installation cannot pass silently"
    }
    if ([string]$autoInstall.reason -eq 'skip_auto_install_requested') {
        Die "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_SKIPPED path=$InstallerPath remediation=setup must auto-install the bundled Chrome bridge into the already-open active profile; remove -SkipAutoInstall and rerun from the interactive Windows desktop"
    }
    $profileInstallState = $readback.synapse_chrome_profile_install_state
    if (-not $profileInstallState) {
        Die "SYNAPSE_CHROME_BRIDGE_PROFILE_INSTALL_STATE_MISSING path=$InstallerPath remediation=setup requires active Chrome profile installation readback after bridge verification"
    }
    if ($profileInstallState.active_profile_installed -ne $true) {
        Die ("SYNAPSE_CHROME_BRIDGE_ACTIVE_PROFILE_NOT_INSTALLED active_profile={0} installed_profiles={1} auto_install_attempted={2} auto_install_reason={3} remediation=setup must auto-install the deployed stable Synapse Chrome bridge directory into the already-open active Chrome profile before daemon handoff can continue" -f `
            $profileInstallState.active_profile,
            (@($profileInstallState.installed_profiles) -join ','),
            $autoInstall.attempted,
            $autoInstall.reason)
    }
    return $readback
}

function Format-SynapseChromeBridgeProfileInstallState {
    param($Readback)

    $state = $Readback.synapse_chrome_profile_install_state
    if (-not $state) {
        return 'profile_install_state=missing'
    }
    $autoInstall = $Readback.synapse_chrome_auto_install
    $autoInstallAttempted = if ($autoInstall) { [string]$autoInstall.attempted } else { 'missing' }
    $autoInstallReason = if ($autoInstall) { [string]$autoInstall.reason } else { 'missing' }
    $extensionDir = if ($Readback.extension_dir) { [string]$Readback.extension_dir } else { 'missing' }
    return ("profile_install_state=installed:{0},profile_count:{1},installed_profile_count:{2},active_profile:{3},active_profile_installed:{4},reason:{5},auto_install_attempted:{6},auto_install_reason:{7},extension_dir:{8}" -f `
        $state.installed, `
        $state.profile_count, `
        $state.installed_profile_count, `
        $state.active_profile, `
        $state.active_profile_installed, `
        $state.reason, `
        $autoInstallAttempted, `
        $autoInstallReason, `
        $extensionDir)
}

$processTokenAtStart = $env:SYNAPSE_BEARER_TOKEN
$processToolSurfaceHashAtStart = $env:SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START
$processToolSurfaceSnapshotAtStart = $env:SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START
$script:SynapseSetupMaintenanceLockStream = $null
$script:SynapseSetupMaintenanceLockPath = $null
$script:SynapseSetupMaintenanceLockReason = $null

function Get-ProcessLineage {
    param([int]$StartPid = $PID)
    $lineage = @()
    $seen = @{}
    $current = $StartPid
    $child = $null
    while ($current -and -not $seen.ContainsKey($current)) {
        $seen[$current] = $true
        $p = Get-CimInstance Win32_Process -Filter "ProcessId=$current" -ErrorAction SilentlyContinue
        if (-not $p) { break }
        # Guard against Windows PID reuse: ParentProcessId is a bare number, so
        # once a real parent exits its PID can be recycled by an unrelated, newer
        # process. A genuine parent always starts no later than its child; if the
        # candidate "parent" started after the child it is a recycled PID, not a
        # true ancestor, so stop the walk rather than climb into a phantom chain
        # (e.g. wininit.exe <- cmd.exe, which falsely tripped the cmd-ancestor guard).
        if ($child -and $p.CreationDate -and $child.CreationDate -and $p.CreationDate -gt $child.CreationDate) {
            break
        }
        $lineage += $p
        $child = $p
        $current = [int]$p.ParentProcessId
    }
    return $lineage
}

function Get-SynapseCurrentCodexAncestor {
    $lineage = Get-ProcessLineage
    return ($lineage | Where-Object {
        $_.Name -ieq 'codex.exe' -or $_.CommandLine -match '@openai[\\/]+codex|codex\.js|codex-win32'
    } | Select-Object -First 1)
}

function Read-SynapseSetupMaintenanceLockOwner {
    param([Parameter(Mandatory=$true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        return '<missing>'
    }
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8, $true, 4096, $true)
            try {
                $text = $reader.ReadToEnd().Trim()
                if ([string]::IsNullOrWhiteSpace($text)) { return '<empty>' }
                return ($text -replace '\s+', ' ')
            } finally {
                $reader.Dispose()
            }
        } finally {
            $stream.Dispose()
        }
    } catch {
        return "<unreadable error=$($_.Exception.Message)>"
    }
}

function Acquire-SynapseSetupMaintenanceLock {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::Read)
    } catch [System.IO.IOException] {
        $owner = Read-SynapseSetupMaintenanceLockOwner -Path $Path
        Die "SYNAPSE_SETUP_MAINTENANCE_LOCK_HELD reason=$Reason path=$Path owner=$owner remediation=another setup/remove process owns the maintenance lock; wait for that setup process to exit or inspect the named PID. Do not close terminal windows or broad shell processes to clear this condition."
    } catch {
        Die "SYNAPSE_SETUP_MAINTENANCE_LOCK_OPEN_FAILED reason=$Reason path=$Path error=$($_.Exception.Message) remediation=repair permissions on the synapse local appdata directory"
    }

    $script:SynapseSetupMaintenanceLockStream = $stream
    $script:SynapseSetupMaintenanceLockPath = $Path
    $script:SynapseSetupMaintenanceLockReason = $Reason
    $self = Get-CimInstance Win32_Process -Filter "ProcessId=$PID" -ErrorAction SilentlyContinue
    $lineageText = (Get-ProcessLineage | ForEach-Object { "{0}:{1}" -f $_.ProcessId, $_.Name }) -join ' <- '
    $owner = [ordered]@{
        schema = 'synapse_setup_maintenance_lock/v1'
        state = 'held'
        reason = $Reason
        pid = $PID
        parent_pid = $self.ParentProcessId
        process_name = $self.Name
        command_line = $self.CommandLine
        source_dir = $SourceDir
        bind = $Bind
        started_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        lineage = $lineageText
        cleanup_policy = 'never close terminal windows globally; only exact process IDs spawned by this setup operation or verified synapse-mcp targets may be stopped'
    }
    $json = $owner | ConvertTo-Json -Depth 6
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json + "`n")
    $stream.SetLength(0)
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush($true)
    Info "Maintenance lock acquired reason=$Reason path=$Path pid=$PID"
}

function Release-SynapseSetupMaintenanceLock {
    param(
        [Parameter(Mandatory=$true)][ValidateSet('released','failed')][string]$State,
        [string]$ErrorMessage
    )

    if ($null -eq $script:SynapseSetupMaintenanceLockStream) {
        return
    }

    $stream = $script:SynapseSetupMaintenanceLockStream
    try {
        $self = Get-CimInstance Win32_Process -Filter "ProcessId=$PID" -ErrorAction SilentlyContinue
        $owner = [ordered]@{
            schema = 'synapse_setup_maintenance_lock/v1'
            state = $State
            reason = $script:SynapseSetupMaintenanceLockReason
            pid = $PID
            parent_pid = if ($self) { $self.ParentProcessId } else { $null }
            process_name = if ($self) { $self.Name } else { $null }
            command_line = if ($self) { $self.CommandLine } else { $null }
            source_dir = $SourceDir
            bind = $Bind
            released_at_utc = (Get-Date).ToUniversalTime().ToString('o')
            cleanup_policy = 'never close terminal windows globally; only exact process IDs spawned by this setup operation or verified synapse-mcp targets may be stopped'
        }
        if (-not [string]::IsNullOrWhiteSpace($ErrorMessage)) {
            $owner.error = ($ErrorMessage -replace '\s+', ' ').Trim()
        }
        $json = $owner | ConvertTo-Json -Depth 6
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($json + "`n")
        $stream.SetLength(0)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
        Info "Maintenance lock $State path=$script:SynapseSetupMaintenanceLockPath pid=$PID"
    } catch {
        Info "WARN: could not write maintenance lock release state path=$script:SynapseSetupMaintenanceLockPath error=$($_.Exception.Message)"
    } finally {
        $stream.Dispose()
        $script:SynapseSetupMaintenanceLockStream = $null
        $script:SynapseSetupMaintenanceLockPath = $null
        $script:SynapseSetupMaintenanceLockReason = $null
    }
}

trap {
    $errorText = $_ | Out-String
    Release-SynapseSetupMaintenanceLock -State failed -ErrorMessage $errorText
    break
}

function Quote-WindowsCommandArgument {
    param([Parameter(Mandatory=$true)][string]$Value)
    if ($Value.Length -eq 0) { return '""' }
    if ($Value -notmatch '[\s"]') { return $Value }
    $escaped = $Value -replace '(\\*)"', '$1$1\"'
    $escaped = $escaped -replace '(\\+)$', '$1$1'
    return '"' + $escaped + '"'
}

function Quote-VbsString {
    param([Parameter(Mandatory=$true)][string]$Value)
    return '"' + ($Value -replace '"', '""') + '"'
}

function Vbs-Literal {
    param([Parameter(Mandatory=$true)][string]$Value)
    return '"' + ($Value -replace '"', '""') + '"'
}

function New-HiddenDaemonLauncher {
    param(
        [Parameter(Mandatory=$true)][string]$OutputPath,
        [Parameter(Mandatory=$true)][string]$ExePath,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$TokenPath
    )

    $daemonLogDir = $LogDir
    $launcherLog = Join-Path $LogDir 'daemon-launcher.log'
    $daemonCommand = @(
        (Quote-WindowsCommandArgument $ExePath),
        '--mode', 'http',
        '--bind', (Quote-WindowsCommandArgument $Bind),
        '--db', (Quote-WindowsCommandArgument $DbPath),
        '--profile-dir', (Quote-WindowsCommandArgument $ProfilesDir),
        '--log-level', 'info'
    ) -join ' '

    @"
Option Explicit
Dim shell, fso, env, tokenPath, launcherLog, daemonLogDir, daemonCommand
Dim tokenFile, token, exitCode

Set shell = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
tokenPath = $(Vbs-Literal $TokenPath)
launcherLog = $(Vbs-Literal $launcherLog)
daemonLogDir = $(Vbs-Literal $daemonLogDir)
daemonCommand = $(Vbs-Literal $daemonCommand)

Sub LogLine(message)
  Dim logFile
  Set logFile = fso.OpenTextFile(launcherLog, 8, True)
  logFile.WriteLine Now & " " & message
  logFile.Close
End Sub

On Error Resume Next
If Not fso.FolderExists(daemonLogDir) Then
  fso.CreateFolder daemonLogDir
End If
If Err.Number <> 0 Then
  WScript.Quit 1
End If
On Error GoTo 0

If Not fso.FileExists(tokenPath) Then
  LogLine "SYNAPSE_DAEMON_TOKEN_MISSING path=" & tokenPath
  WScript.Quit 1
End If

On Error Resume Next
Set tokenFile = fso.OpenTextFile(tokenPath, 1, False)
If Err.Number <> 0 Then
  LogLine "SYNAPSE_DAEMON_TOKEN_READ_FAILED path=" & tokenPath & " err_number=" & Err.Number & " err_description=" & Err.Description
  WScript.Quit 1
End If
If tokenFile.AtEndOfStream Then
  token = ""
Else
  token = Trim(tokenFile.ReadAll)
End If
If Err.Number <> 0 Then
  LogLine "SYNAPSE_DAEMON_TOKEN_READ_FAILED path=" & tokenPath & " err_number=" & Err.Number & " err_description=" & Err.Description
  tokenFile.Close
  WScript.Quit 1
End If
tokenFile.Close
On Error GoTo 0

If Len(token) < 16 Then
  LogLine "SYNAPSE_DAEMON_TOKEN_INVALID path=" & tokenPath & " length=" & Len(token)
  WScript.Quit 1
End If

Set env = shell.Environment("PROCESS")
env("SYNAPSE_BEARER_TOKEN") = token
env("SYNAPSE_LOG_DIR") = daemonLogDir

LogLine "SYNAPSE_DAEMON_LAUNCH_START command=" & daemonCommand
exitCode = shell.Run(daemonCommand, 0, True)
LogLine "SYNAPSE_DAEMON_EXIT exit_code=" & exitCode
WScript.Quit exitCode
"@ | Set-Content -Path $OutputPath -Encoding ascii
}

function Ensure-SynapseSetupProcessJobType {
    if ('SynapseSetup.ProcessJob' -as [type]) { return }

    Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace SynapseSetup
{
    public static class ProcessJob
    {
        private const uint CREATE_SUSPENDED = 0x00000004;
        private const uint CREATE_NO_WINDOW = 0x08000000;
        private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
        private const int JobObjectExtendedLimitInformation = 9;
        private const uint WAIT_OBJECT_0 = 0x00000000;
        private const uint WAIT_TIMEOUT = 0x00000102;
        private const uint WAIT_FAILED = 0xffffffff;
        private const uint EXIT_TIMEOUT = 124;
        private const uint EXIT_ASSIGN_FAILED = 125;
        private const uint EXIT_RESUME_FAILED = 126;
        private const uint INFINITE = 0xffffffff;

        [StructLayout(LayoutKind.Sequential)]
        private struct IO_COUNTERS
        {
            public ulong ReadOperationCount;
            public ulong WriteOperationCount;
            public ulong OtherOperationCount;
            public ulong ReadTransferCount;
            public ulong WriteTransferCount;
            public ulong OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
        {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize;
            public UIntPtr MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
        {
            public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            public IO_COUNTERS IoInfo;
            public UIntPtr ProcessMemoryLimit;
            public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed;
            public UIntPtr PeakJobMemoryUsed;
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct STARTUPINFO
        {
            public uint cb;
            public string lpReserved;
            public string lpDesktop;
            public string lpTitle;
            public uint dwX;
            public uint dwY;
            public uint dwXSize;
            public uint dwYSize;
            public uint dwXCountChars;
            public uint dwYCountChars;
            public uint dwFillAttribute;
            public uint dwFlags;
            public ushort wShowWindow;
            public ushort cbReserved2;
            public IntPtr lpReserved2;
            public IntPtr hStdInput;
            public IntPtr hStdOutput;
            public IntPtr hStdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct PROCESS_INFORMATION
        {
            public IntPtr hProcess;
            public IntPtr hThread;
            public uint dwProcessId;
            public uint dwThreadId;
        }

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern IntPtr CreateJobObject(IntPtr lpJobAttributes, string lpName);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(
            IntPtr hJob,
            int jobObjectInfoClass,
            IntPtr lpJobObjectInfo,
            uint cbJobObjectInfoLength);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern bool CreateProcess(
            string lpApplicationName,
            StringBuilder lpCommandLine,
            IntPtr lpProcessAttributes,
            IntPtr lpThreadAttributes,
            bool bInheritHandles,
            uint dwCreationFlags,
            IntPtr lpEnvironment,
            string lpCurrentDirectory,
            ref STARTUPINFO lpStartupInfo,
            out PROCESS_INFORMATION lpProcessInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint ResumeThread(IntPtr hThread);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint WaitForSingleObject(IntPtr hHandle, uint dwMilliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetExitCodeProcess(IntPtr hProcess, out uint lpExitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateJobObject(IntPtr hJob, uint uExitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateProcess(IntPtr hProcess, uint uExitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CloseHandle(IntPtr hObject);

        public static int Run(
            string applicationName,
            string commandLine,
            string workingDirectory,
            uint timeoutMilliseconds,
            out string failure)
        {
            failure = "";
            IntPtr job = IntPtr.Zero;
            IntPtr limitPointer = IntPtr.Zero;
            PROCESS_INFORMATION processInfo = new PROCESS_INFORMATION();
            try
            {
                job = CreateJobObject(IntPtr.Zero, null);
                if (job == IntPtr.Zero)
                {
                    failure = "PROCESS_JOB_CREATE_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    return 127;
                }

                JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                int limitSize = Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
                limitPointer = Marshal.AllocHGlobal(limitSize);
                Marshal.StructureToPtr(limits, limitPointer, false);
                if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, limitPointer, (uint)limitSize))
                {
                    failure = "PROCESS_JOB_LIMIT_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    return 127;
                }

                STARTUPINFO startupInfo = new STARTUPINFO();
                startupInfo.cb = (uint)Marshal.SizeOf(typeof(STARTUPINFO));
                StringBuilder mutableCommandLine = new StringBuilder(commandLine);
                bool created = CreateProcess(
                    applicationName,
                    mutableCommandLine,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    false,
                    CREATE_SUSPENDED | CREATE_NO_WINDOW,
                    IntPtr.Zero,
                    workingDirectory,
                    ref startupInfo,
                    out processInfo);
                if (!created)
                {
                    failure = "PROCESS_JOB_CREATE_PROCESS_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    return 127;
                }

                if (!AssignProcessToJobObject(job, processInfo.hProcess))
                {
                    failure = "PROCESS_JOB_ASSIGN_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    TerminateProcess(processInfo.hProcess, EXIT_ASSIGN_FAILED);
                    return (int)EXIT_ASSIGN_FAILED;
                }

                if (ResumeThread(processInfo.hThread) == 0xffffffff)
                {
                    failure = "PROCESS_JOB_RESUME_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    TerminateJobObject(job, EXIT_RESUME_FAILED);
                    return (int)EXIT_RESUME_FAILED;
                }

                uint wait = WaitForSingleObject(
                    processInfo.hProcess,
                    timeoutMilliseconds == 0 ? INFINITE : timeoutMilliseconds);
                if (wait == WAIT_TIMEOUT)
                {
                    TerminateJobObject(job, EXIT_TIMEOUT);
                    WaitForSingleObject(processInfo.hProcess, 15000);
                    failure = "PROCESS_JOB_TIMEOUT: child process tree exceeded timeout_ms=" + timeoutMilliseconds;
                    return (int)EXIT_TIMEOUT;
                }
                if (wait == WAIT_FAILED)
                {
                    failure = "PROCESS_JOB_WAIT_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    TerminateJobObject(job, 127);
                    return 127;
                }
                if (wait != WAIT_OBJECT_0)
                {
                    failure = "PROCESS_JOB_WAIT_UNEXPECTED: wait_result=" + wait;
                    TerminateJobObject(job, 127);
                    return 127;
                }

                uint exitCode;
                if (!GetExitCodeProcess(processInfo.hProcess, out exitCode))
                {
                    failure = "PROCESS_JOB_EXIT_CODE_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    return 127;
                }
                return unchecked((int)exitCode);
            }
            finally
            {
                if (limitPointer != IntPtr.Zero)
                {
                    Marshal.FreeHGlobal(limitPointer);
                }
                if (processInfo.hThread != IntPtr.Zero)
                {
                    CloseHandle(processInfo.hThread);
                }
                if (processInfo.hProcess != IntPtr.Zero)
                {
                    CloseHandle(processInfo.hProcess);
                }
                if (job != IntPtr.Zero)
                {
                    CloseHandle(job);
                }
            }
        }
    }
}
'@ | Out-Null
}

function Invoke-SynapseProcessInKillOnCloseJob {
    param(
        [Parameter(Mandatory=$true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [Parameter(Mandatory=$true)][string]$WorkingDirectory,
        [Parameter(Mandatory=$true)][int]$TimeoutMinutes,
        [string]$LogPath
    )

    Ensure-SynapseSetupProcessJobType
    if (-not (Test-Path $FilePath)) { Die "Process job target missing: $FilePath" }
    if (-not (Test-Path $WorkingDirectory)) { Die "Process job working directory missing: $WorkingDirectory" }

    $argumentText = (($ArgumentList | ForEach-Object { Quote-WindowsCommandArgument $_ }) -join ' ').Trim()
    $targetCommand = (Quote-WindowsCommandArgument $FilePath)
    if (-not [string]::IsNullOrWhiteSpace($argumentText)) {
        $targetCommand = "$targetCommand $argumentText"
    }

    $applicationPath = $FilePath
    $commandLine = $targetCommand
    if (-not [string]::IsNullOrWhiteSpace($LogPath)) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LogPath) | Out-Null
        if (Test-Path $LogPath) { Remove-Item $LogPath -Force }
        $cmdPath = Join-Path $env:SystemRoot 'System32\cmd.exe'
        $redirectCommand = "$targetCommand > $(Quote-WindowsCommandArgument $LogPath) 2>&1"
        $applicationPath = $cmdPath
        $commandLine = "$(Quote-WindowsCommandArgument $cmdPath) /d /s /c `"$redirectCommand`""
    }

    $timeoutMilliseconds = [uint32]([math]::Min([int64]$TimeoutMinutes * 60 * 1000, [uint32]::MaxValue))
    $failure = ''
    $exitCode = [SynapseSetup.ProcessJob]::Run(
        $applicationPath,
        $commandLine,
        $WorkingDirectory,
        $timeoutMilliseconds,
        [ref]$failure)
    if (-not [string]::IsNullOrWhiteSpace($failure)) {
        $tail = ''
        if ($LogPath -and (Test-Path $LogPath)) {
            $tail = (Get-Content -Path $LogPath -Tail 40 -ErrorAction SilentlyContinue) -join "`n"
        }
        Die "PROCESS_JOB_FAILED command=$targetCommand exit=$exitCode reason=$failure log=$LogPath tail=`n$tail"
    }
    return $exitCode
}

function Install-CodexSynapseTokenLoader {
    param(
        [Parameter(Mandatory=$true)][string]$CodexCommandPath,
        [Parameter(Mandatory=$true)][string]$TokenPath
    )

    $npmDir = Split-Path -Parent $CodexCommandPath
    if (-not $npmDir -or -not (Test-Path $npmDir)) {
        Die "Cannot resolve Codex launcher directory from '$CodexCommandPath'."
    }

    $ps1Path = Join-Path $npmDir 'codex.ps1'
    $cmdPath = Join-Path $npmDir 'codex.cmd'
    $shPath = Join-Path $npmDir 'codex'

    if (Test-Path $ps1Path) {
        $ps1 = @'
#!/usr/bin/env pwsh
$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent

# Synapse MCP token loader: begin
$synapseConfigPath = Join-Path $env:USERPROFILE '.codex\config.toml'
$synapseTokenPath = Join-Path $env:APPDATA 'synapse\token.txt'
$synapseHasConfig = $false
if (Test-Path $synapseConfigPath) {
  try {
    $synapseHasConfig = ((Get-Content -Raw $synapseConfigPath) -match '(?m)^\[mcp_servers\.synapse\]')
  } catch {
    Write-Error "SYNAPSE_CODEX_CONFIG_UNREADABLE path=$synapseConfigPath remediation=repair Codex config permissions or rerun scripts\synapse-setup.ps1"
    exit 1
  }
}
if ($synapseHasConfig) {
  if (-not (Test-Path $synapseTokenPath)) {
    Write-Error "SYNAPSE_CODEX_TOKEN_MISSING path=$synapseTokenPath remediation=run scripts\synapse-setup.ps1 to generate the bearer token"
    exit 1
  }
  $synapseTokenRaw = Get-Content -Raw $synapseTokenPath
  $synapseToken = if ($null -eq $synapseTokenRaw) { '' } else { $synapseTokenRaw.Trim() }
  if ([string]::IsNullOrWhiteSpace($synapseToken)) {
    Write-Error "SYNAPSE_CODEX_TOKEN_EMPTY path=$synapseTokenPath remediation=delete the empty token and rerun scripts\synapse-setup.ps1"
    exit 1
  }
  if ($env:SYNAPSE_BEARER_TOKEN -ne $synapseToken) {
    $env:SYNAPSE_BEARER_TOKEN = $synapseToken
  }
  $synapseToolSurfacePath = Join-Path $env:APPDATA 'synapse\codex-tool-surface.json'
  if (-not (Test-Path $synapseToolSurfacePath)) {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING path=$synapseToolSurfacePath remediation=run scripts\synapse-setup.ps1 to write the current daemon tools/list fingerprint before starting Codex"
    exit 1
  }
  try {
    $synapseToolSurface = Get-Content -Raw $synapseToolSurfacePath | ConvertFrom-Json
  } catch {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_UNREADABLE path=$synapseToolSurfacePath error=$($_.Exception.Message) remediation=repair the snapshot file or rerun scripts\synapse-setup.ps1"
    exit 1
  }
  $synapseToolSurfaceHash = [string]$synapseToolSurface.tool_surface_sha256
  if ([string]::IsNullOrWhiteSpace($synapseToolSurfaceHash)) {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=$synapseToolSurfacePath remediation=delete the invalid snapshot and rerun scripts\synapse-setup.ps1"
    exit 1
  }
  $synapseStartSnapshotDir = Join-Path $env:LOCALAPPDATA 'synapse\codex-start-snapshots'
  $synapseStartSnapshotPath = Join-Path $synapseStartSnapshotDir ("codex-tool-surface-{0}-{1}.json" -f $PID, [Guid]::NewGuid().ToString('N'))
  try {
    New-Item -ItemType Directory -Force -Path $synapseStartSnapshotDir | Out-Null
    Copy-Item -LiteralPath $synapseToolSurfacePath -Destination $synapseStartSnapshotPath -Force
  } catch {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=$synapseStartSnapshotPath error=$($_.Exception.Message) remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-start-snapshots before starting Codex"
    exit 1
  }
  $env:SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START = $synapseToolSurfaceHash
  $env:SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START = [string]$synapseToolSurface.tool_count
  $env:SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START = $synapseStartSnapshotPath
}
Remove-Variable synapseConfigPath,synapseTokenPath,synapseHasConfig -ErrorAction SilentlyContinue
Remove-Variable synapseTokenRaw,synapseToken -ErrorAction SilentlyContinue
Remove-Variable synapseToolSurfacePath,synapseToolSurface,synapseToolSurfaceHash -ErrorAction SilentlyContinue
Remove-Variable synapseStartSnapshotDir,synapseStartSnapshotPath -ErrorAction SilentlyContinue
# Synapse MCP token loader: end

$exe=""
if ($PSVersionTable.PSVersion -lt "6.0" -or $IsWindows) {
  # Fix case when both the Windows and Linux builds of Node
  # are installed in the same directory
  $exe=".exe"
}
$ret=0
if (Test-Path "$basedir/node$exe") {
  # Support pipeline input
  if ($MyInvocation.ExpectingInput) {
    $input | & "$basedir/node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  } else {
    & "$basedir/node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  }
  $ret=$LASTEXITCODE
} else {
  # Support pipeline input
  if ($MyInvocation.ExpectingInput) {
    $input | & "node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  } else {
    & "node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  }
  $ret=$LASTEXITCODE
}
exit $ret
'@
        Copy-Item $ps1Path "$ps1Path.synapse-bak" -Force
        Set-Content -Path $ps1Path -Value $ps1 -Encoding utf8
        Info "Installed Synapse token loader in Codex PowerShell launcher: $ps1Path"
    } else {
        Info "WARN: Codex PowerShell launcher not found at $ps1Path; cannot install ps1 token loader."
    }

    if (Test-Path $cmdPath) {
        $cmd = @'
@ECHO off
GOTO start
:find_dp0
SET dp0=%~dp0
EXIT /b
:start
SETLOCAL EnableExtensions EnableDelayedExpansion
CALL :find_dp0

REM Synapse MCP token loader: begin
SET "_synapse_cfg=%USERPROFILE%\.codex\config.toml"
SET "_synapse_tok=%APPDATA%\synapse\token.txt"
SET "_synapse_surface=%APPDATA%\synapse\codex-tool-surface.json"
SET "_synapse_has_cfg="
IF EXIST "%_synapse_cfg%" (
  %SystemRoot%\System32\findstr.exe /R /C:"^\[mcp_servers\.synapse\]" "%_synapse_cfg%" >NUL 2>NUL
  IF NOT ERRORLEVEL 1 SET "_synapse_has_cfg=1"
)
IF DEFINED _synapse_has_cfg (
  IF NOT EXIST "%_synapse_tok%" (
    ECHO SYNAPSE_CODEX_TOKEN_MISSING path=%_synapse_tok% remediation=run scripts\synapse-setup.ps1 to generate the bearer token 1>&2
    EXIT /B 1
  )
  SET /P _synapse_file_token=<"%_synapse_tok%"
  IF NOT DEFINED _synapse_file_token (
    ECHO SYNAPSE_CODEX_TOKEN_EMPTY path=%_synapse_tok% remediation=delete the empty token and rerun scripts\synapse-setup.ps1 1>&2
    EXIT /B 1
  )
  IF NOT "%SYNAPSE_BEARER_TOKEN%"=="!_synapse_file_token!" SET "SYNAPSE_BEARER_TOKEN=!_synapse_file_token!"
  IF NOT EXIST "%_synapse_surface%" (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING path=%_synapse_surface% remediation=run scripts\synapse-setup.ps1 to write the current daemon tools/list fingerprint before starting Codex 1>&2
    EXIT /B 1
  )
  SET "_synapse_surface_hash="
  SET "_synapse_surface_count="
  FOR /F "tokens=2 delims=:" %%A IN ('%SystemRoot%\System32\findstr.exe /C:tool_surface_sha256 "%_synapse_surface%"') DO SET "_synapse_surface_hash=%%~A"
  FOR /F "tokens=2 delims=:" %%A IN ('%SystemRoot%\System32\findstr.exe /C:tool_count "%_synapse_surface%"') DO SET "_synapse_surface_count=%%~A"
  SET "_synapse_surface_hash=!_synapse_surface_hash:"=!"
  SET "_synapse_surface_hash=!_synapse_surface_hash:,=!"
  SET "_synapse_surface_hash=!_synapse_surface_hash: =!"
  SET "_synapse_surface_count=!_synapse_surface_count:"=!"
  SET "_synapse_surface_count=!_synapse_surface_count:,=!"
  SET "_synapse_surface_count=!_synapse_surface_count: =!"
  IF NOT DEFINED _synapse_surface_hash (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=%_synapse_surface% remediation=delete the invalid snapshot and rerun scripts\synapse-setup.ps1 1>&2
    EXIT /B 1
  )
  SET "_synapse_start_dir=%LOCALAPPDATA%\synapse\codex-start-snapshots"
  SET "_synapse_start_surface=!_synapse_start_dir!\codex-tool-surface-!RANDOM!-!RANDOM!.json"
  IF NOT EXIST "!_synapse_start_dir!" MD "!_synapse_start_dir!" >NUL 2>NUL
  IF NOT EXIST "!_synapse_start_dir!" (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=!_synapse_start_surface! remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-start-snapshots before starting Codex 1>&2
    EXIT /B 1
  )
  COPY /Y "%_synapse_surface%" "!_synapse_start_surface!" >NUL
  IF ERRORLEVEL 1 (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=!_synapse_start_surface! remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-start-snapshots before starting Codex 1>&2
    EXIT /B 1
  )
  SET "SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START=!_synapse_surface_hash!"
  SET "SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START=!_synapse_surface_count!"
  SET "SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START=!_synapse_start_surface!"
)
SET "_synapse_cfg="
SET "_synapse_tok="
SET "_synapse_surface="
SET "_synapse_has_cfg="
SET "_synapse_file_token="
SET "_synapse_surface_hash="
SET "_synapse_surface_count="
SET "_synapse_start_dir="
SET "_synapse_start_surface="
REM Synapse MCP token loader: end

IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
  SET PATHEXT=%PATHEXT:;.JS;=;%
)

endLocal & SET "SYNAPSE_BEARER_TOKEN=%SYNAPSE_BEARER_TOKEN%" & SET "SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START=%SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START%" & SET "SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START=%SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START%" & SET "SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START=%SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START%" & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*
'@
        Copy-Item $cmdPath "$cmdPath.synapse-bak" -Force
        Set-Content -Path $cmdPath -Value $cmd -Encoding ascii
        Info "Installed Synapse token loader in Codex CMD launcher: $cmdPath"
    } else {
        Info "WARN: Codex CMD launcher not found at $cmdPath; cannot install cmd token loader."
    }

    if (Test-Path $shPath) {
        $sh = @'
#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\,/,g')")

# Synapse MCP token loader: begin
synapse_cfg="$USERPROFILE/.codex/config.toml"
synapse_tok="$APPDATA/synapse/token.txt"
case `uname` in
    *CYGWIN*|*MINGW*|*MSYS*)
        if command -v cygpath > /dev/null 2>&1; then
            synapse_cfg=$(cygpath -u "$synapse_cfg")
            synapse_tok=$(cygpath -u "$synapse_tok")
        fi
    ;;
esac
if [ -f "$synapse_cfg" ] && grep -Eq '^\[mcp_servers\.synapse\]' "$synapse_cfg"; then
    if [ ! -r "$synapse_tok" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOKEN_MISSING path=$synapse_tok remediation=run scripts/synapse-setup.ps1 to generate the bearer token" >&2
        exit 1
    fi
    synapse_file_token=$(tr -d '\r\n' < "$synapse_tok")
    if [ -z "$synapse_file_token" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOKEN_EMPTY path=$synapse_tok remediation=delete the empty token and rerun scripts/synapse-setup.ps1" >&2
        exit 1
    fi
    if [ "${SYNAPSE_BEARER_TOKEN:-}" != "$synapse_file_token" ]; then
        SYNAPSE_BEARER_TOKEN="$synapse_file_token"
        export SYNAPSE_BEARER_TOKEN
    fi
    synapse_surface="$APPDATA/synapse/codex-tool-surface.json"
    if [ ! -r "$synapse_surface" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING path=$synapse_surface remediation=run scripts/synapse-setup.ps1 to write the current daemon tools/list fingerprint before starting Codex" >&2
        exit 1
    fi
    synapse_surface_hash=$(sed -n 's/.*"tool_surface_sha256"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F][0-9a-fA-F]*\)".*/\1/p' "$synapse_surface" | head -n 1)
    synapse_surface_count=$(sed -n 's/.*"tool_count"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$synapse_surface" | head -n 1)
    if [ -z "$synapse_surface_hash" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=$synapse_surface remediation=delete the invalid snapshot and rerun scripts/synapse-setup.ps1" >&2
        exit 1
    fi
    synapse_start_dir="$LOCALAPPDATA/synapse/codex-start-snapshots"
    synapse_start_surface="$synapse_start_dir/codex-tool-surface-$$-$(date +%s).json"
    if ! mkdir -p "$synapse_start_dir" || ! cp "$synapse_surface" "$synapse_start_surface"; then
        printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=$synapse_start_surface remediation=repair permissions on $synapse_start_dir before starting Codex" >&2
        exit 1
    fi
    SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START="$synapse_surface_hash"
    SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START="$synapse_surface_count"
    SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START="$synapse_start_surface"
    export SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START
fi
unset synapse_cfg synapse_tok synapse_file_token synapse_surface synapse_surface_hash synapse_surface_count synapse_start_dir synapse_start_surface
# Synapse MCP token loader: end

case `uname` in
    *CYGWIN*|*MINGW*|*MSYS*)
        if command -v cygpath > /dev/null 2>&1; then
            basedir=`cygpath -w "$basedir"`
        fi
    ;;
esac

if [ -x "$basedir/node" ]; then
  exec "$basedir/node"  "$basedir/node_modules/@openai/codex/bin/codex.js" "$@"
else
  exec node  "$basedir/node_modules/@openai/codex/bin/codex.js" "$@"
fi
'@
        Copy-Item $shPath "$shPath.synapse-bak" -Force
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($shPath, ($sh -replace "`r?`n", "`n"), $utf8NoBom)
        Info "Installed Synapse token loader in Codex shell launcher: $shPath"
    } else {
        Info "WARN: Codex shell launcher not found at $shPath; cannot install shell token loader."
    }

    $loaderTokenRaw = if (Test-Path $TokenPath) { Get-Content -Raw $TokenPath } else { $null }
    $loaderToken = if ($null -eq $loaderTokenRaw) { '' } else { $loaderTokenRaw.Trim() }
    if ((Test-Path $TokenPath) -and [string]::IsNullOrWhiteSpace($loaderToken)) {
        Die "Installed Codex token loaders, but token at $TokenPath is empty."
    }
}

function Test-CodexSynapseHttpConfig {
    param(
        [Parameter(Mandatory=$true)][string]$ConfigPath,
        [Parameter(Mandatory=$true)][string]$Bind
    )

    if (-not (Test-Path $ConfigPath)) {
        return $false
    }
    try {
        $content = Get-Content -Raw $ConfigPath
    } catch {
        return $false
    }
    $section = [regex]::Match(
        $content,
        '(?ms)^\[mcp_servers\.synapse\]\s*(?<body>.*?)(?=^\[|\z)'
    )
    if (-not $section.Success) {
        return $false
    }
    $body = [string]$section.Groups['body'].Value
    $bindUrlRegex = [regex]::Escape("http://$Bind/mcp")
    return ($body -match "url\s*=\s*`"$bindUrlRegex`"" -and
        $body -match 'bearer_token_env_var\s*=\s*"SYNAPSE_BEARER_TOKEN"')
}

function Get-SynapseMcpProcessSnapshot {
    @(Get-CimInstance Win32_Process -Filter "Name='synapse-mcp.exe'" -ErrorAction SilentlyContinue |
        Sort-Object ProcessId |
        Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine)
}

function Format-SynapseMcpProcessSnapshot {
    param([object[]]$Snapshot)
    if (-not $Snapshot -or $Snapshot.Count -eq 0) {
        return '<none>'
    }
    return (($Snapshot | ForEach-Object {
        $matchRules = if ($_.PSObject.Properties.Name -contains 'DeployTargetRules') { $_.DeployTargetRules } else { '<unclassified>' }
        $bindArg = if ($_.PSObject.Properties.Name -contains 'DeployTargetBindArg') { $_.DeployTargetBindArg } else { '<unclassified>' }
        $dbArg = if ($_.PSObject.Properties.Name -contains 'DeployTargetDbArg') { $_.DeployTargetDbArg } else { '<unclassified>' }
        "pid=$($_.ProcessId) ppid=$($_.ParentProcessId) path=$($_.ExecutablePath) target_match=$matchRules bind_arg=$bindArg db_arg=$dbArg cmd=$($_.CommandLine)"
    }) -join "`n")
}

function Normalize-SynapseSetupPathForCompare {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) { return '' }
    try {
        $full = [System.IO.Path]::GetFullPath($Path)
    } catch {
        $full = $Path.Trim()
    }
    return $full.TrimEnd([char[]]@([char]92, [char]47))
}

function Get-SynapseCommandLineArgumentValue {
    param(
        [string]$CommandLine,
        [Parameter(Mandatory=$true)][string]$Name
    )
    if ([string]::IsNullOrWhiteSpace($CommandLine)) { return $null }
    $escapedName = [regex]::Escape($Name)
    $pattern = "(?i)(?:^|\s)$escapedName(?:\s+|=)(?:""(?<quoted>[^""]*)""|(?<bare>\S+))"
    $match = [regex]::Match($CommandLine, $pattern)
    if (-not $match.Success) { return $null }
    if ($match.Groups['quoted'].Success) { return $match.Groups['quoted'].Value }
    return $match.Groups['bare'].Value
}

function Get-SynapseMcpDeployTargetMatch {
    param(
        [Parameter(Mandatory=$true)]$Process,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath
    )

    $rules = @()
    $bindArg = Get-SynapseCommandLineArgumentValue -CommandLine $Process.CommandLine -Name '--bind'
    if (-not [string]::IsNullOrWhiteSpace($bindArg) -and $bindArg.Trim() -ieq $Bind) {
        $rules += "bind=$Bind"
    }

    $expectedDb = Normalize-SynapseSetupPathForCompare -Path $DbPath
    $dbArg = Get-SynapseCommandLineArgumentValue -CommandLine $Process.CommandLine -Name '--db'
    $actualDb = Normalize-SynapseSetupPathForCompare -Path $dbArg
    if (-not [string]::IsNullOrWhiteSpace($actualDb) -and $actualDb -ieq $expectedDb) {
        $rules += "db=$expectedDb"
    }

    [pscustomobject]@{
        IsMatch = ($rules.Count -gt 0)
        Rules = $rules
        BindArg = if ($null -eq $bindArg) { '<missing>' } else { $bindArg }
        DbArg = if ($null -eq $dbArg) { '<missing>' } else { $dbArg }
        ExpectedDb = $expectedDb
    }
}

function Add-SynapseMcpDeployTargetMetadata {
    param(
        [Parameter(Mandatory=$true)]$Process,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath
    )

    $match = Get-SynapseMcpDeployTargetMatch -Process $Process -Bind $Bind -DbPath $DbPath
    $rules = if ($match.Rules.Count -gt 0) { $match.Rules -join ',' } else { '<none>' }
    $Process | Add-Member -NotePropertyName DeployTargetMatched -NotePropertyValue $match.IsMatch -Force
    $Process | Add-Member -NotePropertyName DeployTargetRules -NotePropertyValue $rules -Force
    $Process | Add-Member -NotePropertyName DeployTargetBindArg -NotePropertyValue $match.BindArg -Force
    $Process | Add-Member -NotePropertyName DeployTargetDbArg -NotePropertyValue $match.DbArg -Force
    return $Process
}

function Select-SynapseMcpDeployTargetProcesses {
    param(
        [object[]]$Snapshot,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [switch]$Invert
    )

    @($Snapshot | ForEach-Object {
        $process = Add-SynapseMcpDeployTargetMetadata -Process $_ -Bind $Bind -DbPath $DbPath
        if ($Invert) {
            if (-not $process.DeployTargetMatched) { $process }
        } else {
            if ($process.DeployTargetMatched) { $process }
        }
    })
}

function Get-SynapseBindEndpoint {
    param([Parameter(Mandatory=$true)][string]$Bind)

    $lastColon = $Bind.LastIndexOf(':')
    if ($lastColon -lt 1 -or $lastColon -eq ($Bind.Length - 1)) {
        Die "SYNAPSE_BIND_PARSE_FAILED bind=$Bind remediation=use host:port, for example 127.0.0.1:7700"
    }

    $address = $Bind.Substring(0, $lastColon)
    $portText = $Bind.Substring($lastColon + 1)
    $port = 0
    if (-not [int]::TryParse($portText, [ref]$port) -or $port -lt 1 -or $port -gt 65535) {
        Die "SYNAPSE_BIND_PARSE_FAILED bind=$Bind port=$portText remediation=use a TCP port from 1 through 65535"
    }

    [pscustomobject]@{ Address = $address; Port = $port }
}

function Get-SynapseTcpClientSnapshot {
    param([Parameter(Mandatory=$true)][string]$Bind)

    $endpoint = Get-SynapseBindEndpoint -Bind $Bind
    $allTcp = @(Get-NetTCPConnection -ErrorAction SilentlyContinue)
    $serverConnections = @($allTcp |
        Where-Object {
            $_.LocalAddress -eq $endpoint.Address -and
            $_.LocalPort -eq $endpoint.Port -and
            "$($_.State)" -ne 'Listen'
        } |
        Sort-Object LocalPort, RemotePort, OwningProcess)

    foreach ($connection in $serverConnections) {
        $peer = @($allTcp | Where-Object {
            $_.LocalAddress -eq $connection.RemoteAddress -and
            $_.LocalPort -eq $connection.RemotePort -and
            $_.RemoteAddress -eq $connection.LocalAddress -and
            $_.RemotePort -eq $connection.LocalPort
        } | Select-Object -First 1)
        $peerOwnerPid = if ($peer.Count -gt 0) { [int]$peer[0].OwningProcess } else { 0 }
        $peerOwner = if ($peerOwnerPid -gt 0) { Get-Process -Id $peerOwnerPid -ErrorAction SilentlyContinue } else { $null }
        $peerOwnerLine = if ($peerOwnerPid -gt 0) {
            (Get-CimInstance Win32_Process -Filter "ProcessId=$peerOwnerPid" -ErrorAction SilentlyContinue).CommandLine
        } else {
            $null
        }
        [pscustomobject]@{
            State = $connection.State
            LocalAddress = $connection.LocalAddress
            LocalPort = $connection.LocalPort
            RemoteAddress = $connection.RemoteAddress
            RemotePort = $connection.RemotePort
            OwningProcess = $connection.OwningProcess
            OwnerName = (Get-Process -Id $connection.OwningProcess -ErrorAction SilentlyContinue).ProcessName
            OwnerCommandLine = (Get-CimInstance Win32_Process -Filter "ProcessId=$($connection.OwningProcess)" -ErrorAction SilentlyContinue).CommandLine
            PeerOwningProcess = $peerOwnerPid
            PeerOwnerName = $peerOwner.ProcessName
            PeerOwnerCommandLine = $peerOwnerLine
            HasLivePeer = ($peerOwnerPid -gt 0)
        }
    }
}

function Get-SynapseTcpBindListenerSnapshot {
    param([Parameter(Mandatory=$true)][string]$Bind)

    $endpoint = Get-SynapseBindEndpoint -Bind $Bind
    $listeners = @(Get-NetTCPConnection -LocalAddress $endpoint.Address -LocalPort $endpoint.Port -State Listen -ErrorAction SilentlyContinue |
        Sort-Object LocalAddress, LocalPort, OwningProcess)

    foreach ($listener in $listeners) {
        $owner = if ($listener.OwningProcess -gt 0) {
            Get-CimInstance Win32_Process -Filter "ProcessId=$($listener.OwningProcess)" -ErrorAction SilentlyContinue
        } else {
            $null
        }
        [pscustomobject]@{
            LocalAddress = $listener.LocalAddress
            LocalPort = $listener.LocalPort
            State = $listener.State
            OwningProcess = $listener.OwningProcess
            CreationTime = $listener.CreationTime
            OwnerExists = ($null -ne $owner)
            OwnerName = $owner.Name
            OwnerCommandLine = $owner.CommandLine
        }
    }
}

function Format-SynapseTcpClientSnapshot {
    param([object[]]$Snapshot)
    if (-not $Snapshot -or $Snapshot.Count -eq 0) {
        return '<none>'
    }
    return (($Snapshot | ForEach-Object {
        "state=$($_.State) local=$($_.LocalAddress):$($_.LocalPort) remote=$($_.RemoteAddress):$($_.RemotePort) owner_pid=$($_.OwningProcess) owner=$($_.OwnerName) peer_pid=$($_.PeerOwningProcess) peer=$($_.PeerOwnerName) has_live_peer=$($_.HasLivePeer) peer_cmd=$($_.PeerOwnerCommandLine)"
    }) -join "`n")
}

function Format-SynapseTcpBindListenerSnapshot {
    param([object[]]$Snapshot)
    if (-not $Snapshot -or $Snapshot.Count -eq 0) {
        return '<none>'
    }
    return (($Snapshot | ForEach-Object {
        "state=$($_.State) local=$($_.LocalAddress):$($_.LocalPort) owner_pid=$($_.OwningProcess) owner_exists=$($_.OwnerExists) owner=$($_.OwnerName) created=$($_.CreationTime) owner_cmd=$($_.OwnerCommandLine)"
    }) -join "`n")
}

function Wait-SynapseBindReleased {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [int]$TimeoutSeconds = 15
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        if ($listeners.Count -eq 0) {
            Info "Synapse bind release verified reason=$Reason bind=$Bind listener_count=0"
            return
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)

    $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
    $tcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
    $processes = @(Get-SynapseMcpProcessSnapshot)
    Die ("SYNAPSE_BIND_STILL_LISTENING reason={0} bind={1} timeout_s={2} listener_count={3} process_count={4}`nlisteners:`n{5}`ntcp_clients:`n{6}`nprocesses:`n{7}`nremediation=the configured HTTP bind is still occupied after daemon shutdown. Do not start another daemon or switch ports. Close/restart the exact live MCP client peer listed here if it owns the remaining connection, or restart the current Codex process when it is the peer; never close terminal/IDE/WSL processes globally." -f `
        $Reason,
        $Bind,
        $TimeoutSeconds,
        $listeners.Count,
        $processes.Count,
        (Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners),
        (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients),
        (Format-SynapseMcpProcessSnapshot -Snapshot $processes))
}

function Read-SynapseSetupTokenForRestartGuard {
    param([Parameter(Mandatory=$true)][string]$TokenPath)

    if (-not (Test-Path -LiteralPath $TokenPath)) {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_RESTART_GUARD_TOKEN_MISSING'; Token = $null; Detail = "path=$TokenPath" }
    }

    try {
        $raw = Get-Content -Raw -LiteralPath $TokenPath
        $token = if ($null -eq $raw) { '' } else { $raw.Trim() }
    } catch {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_RESTART_GUARD_TOKEN_READ_FAILED'; Token = $null; Detail = "path=$TokenPath error=$($_.Exception.Message)" }
    }

    if ($token.Length -lt 16) {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_RESTART_GUARD_TOKEN_INVALID'; Token = $null; Detail = "path=$TokenPath length=$($token.Length)" }
    }

    [pscustomobject]@{ Ok = $true; Code = 'OK'; Token = $token; Detail = "path=$TokenPath length=$($token.Length)" }
}

function Read-SynapseHealthForRestartGuard {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token
    )

    try {
        $health = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $Token" } -TimeoutSec 4
        [pscustomobject]@{ Ok = $true; Health = $health; Error = $null }
    } catch {
        [pscustomobject]@{ Ok = $false; Health = $null; Error = $_.Exception.Message }
    }
}

function ConvertTo-SynapseCanonicalValue {
    param([AllowNull()][object]$Value)

    if ($null -eq $Value) {
        return $null
    }

    if ($Value -is [System.Collections.IDictionary]) {
        $ordered = [ordered]@{}
        foreach ($key in @($Value.Keys | Sort-Object { [string]$_ })) {
            $ordered[[string]$key] = ConvertTo-SynapseCanonicalValue -Value $Value[$key]
        }
        return $ordered
    }

    if ($Value -is [System.Management.Automation.PSCustomObject]) {
        $ordered = [ordered]@{}
        foreach ($prop in @($Value.PSObject.Properties | Sort-Object Name)) {
            $ordered[$prop.Name] = ConvertTo-SynapseCanonicalValue -Value $prop.Value
        }
        return $ordered
    }

    if ($Value -is [System.Collections.IEnumerable] -and $Value -isnot [string]) {
        $items = New-Object System.Collections.ArrayList
        foreach ($item in $Value) {
            [void]$items.Add((ConvertTo-SynapseCanonicalValue -Value $item))
        }
        return ,($items.ToArray())
    }

    return $Value
}

function Get-SynapseCanonicalJson {
    param([AllowNull()][object]$Value)

    $canonical = ConvertTo-SynapseCanonicalValue -Value $Value
    return ($canonical | ConvertTo-Json -Depth 100 -Compress)
}

function Get-SynapseSha256Hex {
    param([Parameter(Mandatory=$true)][string]$Text)

    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        return (($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString('x2') }) -join '')
    } finally {
        $sha.Dispose()
    }
}

function Get-SynapseObjectPropertyValue {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory=$true)][string[]]$Names
    )

    if ($null -eq $Object) {
        return $null
    }
    foreach ($name in $Names) {
        $property = $Object.PSObject.Properties[$name]
        if ($property) {
            return $property.Value
        }
    }
    return $null
}

function Read-SynapseMcpSseJsonResponse {
    param(
        [Parameter(Mandatory=$true)][string]$Content,
        [Parameter(Mandatory=$true)][string]$Operation,
        [int]$ExpectedId = 0
    )

    $trimmed = $Content.Trim()
    if ($trimmed.StartsWith('{')) {
        $message = $trimmed | ConvertFrom-Json
    } else {
        $normalized = ($Content -replace "`r`n", "`n") -replace "`r", "`n"
        $message = $null
        foreach ($frame in @($normalized -split "`n`n")) {
            $dataLines = @()
            foreach ($line in @($frame -split "`n")) {
                if ($line.StartsWith('data:')) {
                    $dataLines += $line.Substring(5).TrimStart()
                }
            }
            $data = ($dataLines -join "`n").Trim()
            if ($data.StartsWith('{')) {
                $message = $data | ConvertFrom-Json
                break
            }
        }
        if ($null -eq $message) {
            $prefix = if ($Content.Length -gt 240) { $Content.Substring(0, 240) } else { $Content }
            Die "SYNAPSE_MCP_SSE_PARSE_FAILED operation=$Operation content_prefix=$prefix remediation=streamable HTTP returned no JSON data frame; inspect daemon logs and MCP transport compatibility"
        }
    }

    if ($ExpectedId -ne 0 -and [int]$message.id -ne $ExpectedId) {
        Die "SYNAPSE_MCP_JSONRPC_ID_MISMATCH operation=$Operation expected_id=$ExpectedId actual_id=$($message.id) remediation=the daemon returned an unexpected JSON-RPC response; inspect streamable HTTP session handling"
    }
    if ($null -ne $message.error) {
        $errorJson = $message.error | ConvertTo-Json -Compress -Depth 8
        Die "SYNAPSE_MCP_JSONRPC_ERROR operation=$Operation error=$errorJson remediation=repair the daemon MCP endpoint before accepting setup"
    }

    return $message
}

function Get-SynapseWebResponseUtf8Content {
    param([Parameter(Mandatory=$true)]$Response)

    $streamProperty = $Response.PSObject.Properties['RawContentStream']
    if ($streamProperty -and $null -ne $streamProperty.Value) {
        $stream = $streamProperty.Value
        if ($stream.CanSeek) {
            $stream.Position = 0
        }
        $encoding = [System.Text.UTF8Encoding]::new($false, $true)
        $reader = [System.IO.StreamReader]::new($stream, $encoding, $true, 4096, $true)
        try {
            return $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
            if ($stream.CanSeek) {
                $stream.Position = 0
            }
        }
    }

    $content = $Response.Content
    if ($content -is [byte[]]) {
        $encoding = [System.Text.UTF8Encoding]::new($false, $true)
        return $encoding.GetString($content)
    }
    return [string]$content
}

function Invoke-SynapseMcpHttpPost {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$Method,
        [Parameter(Mandatory=$true)]$Params,
        [int]$Id = 0,
        [string]$SessionId,
        [int]$TimeoutSec = 8
    )

    $headers = @{
        Authorization = "Bearer $Token"
        Accept = 'application/json, text/event-stream'
    }
    if (-not [string]::IsNullOrWhiteSpace($SessionId)) {
        $headers['Mcp-Session-Id'] = $SessionId
    }

    $request = [ordered]@{
        jsonrpc = '2.0'
        method = $Method
        params = $Params
    }
    if ($Id -ne 0) {
        $request['id'] = $Id
    }
    $body = $request | ConvertTo-Json -Depth 30 -Compress

    try {
        $response = Invoke-WebRequest `
            -Uri "http://$Bind/mcp" `
            -Method Post `
            -Headers $headers `
            -ContentType 'application/json' `
            -Body $body `
            -TimeoutSec $TimeoutSec `
            -UseBasicParsing `
            -ErrorAction Stop
        return [pscustomobject]@{
            Content = Get-SynapseWebResponseUtf8Content -Response $response
            Headers = $response.Headers
            StatusCode = $response.StatusCode
        }
    } catch {
        Die "SYNAPSE_MCP_TOOL_SURFACE_READ_FAILED stage=$Method bind=$Bind error=$($_.Exception.Message) remediation=repair streamable HTTP MCP before accepting setup"
    }
}

function Close-SynapseMcpSetupSession {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$SessionId
    )

    $headers = @{
        Authorization = "Bearer $Token"
        Accept = 'application/json, text/event-stream'
        'Mcp-Session-Id' = $SessionId
    }

    try {
        Invoke-WebRequest -Uri "http://$Bind/mcp" -Method Delete -Headers $headers -TimeoutSec 5 -UseBasicParsing -ErrorAction Stop | Out-Null
    } catch {
        Info "WARN: SYNAPSE_MCP_TOOL_SURFACE_SESSION_DELETE_FAILED bind=$Bind session_id=$SessionId error=$($_.Exception.Message) remediation=inspect health active_sessions and daemon logs; setup did not leave a process behind"
    }
}

function Invoke-SynapseSetupMcpTool {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$Name,
        [Parameter(Mandatory=$true)]$Arguments,
        [int]$TimeoutSec = 8
    )

    $sessionId = $null
    try {
        $initParams = [ordered]@{
            protocolVersion = '2025-06-18'
            capabilities = @{}
            clientInfo = [ordered]@{ name = 'synapse-setup'; version = '0' }
        }
        $initResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -Method 'initialize' -Params $initParams -Id 1
        $sessionId = @($initResponse.Headers['Mcp-Session-Id'])[0]
        if ([string]::IsNullOrWhiteSpace($sessionId)) {
            Die "SYNAPSE_MCP_TOOL_SESSION_MISSING bind=$Bind tool=$Name remediation=streamable HTTP initialize did not return Mcp-Session-Id"
        }
        $initMessage = Read-SynapseMcpSseJsonResponse -Content $initResponse.Content -Operation 'initialize' -ExpectedId 1
        if ($null -eq $initMessage.result -or $null -eq $initMessage.result.capabilities) {
            Die "SYNAPSE_MCP_TOOL_INITIALIZE_INVALID bind=$Bind session_id=$sessionId tool=$Name remediation=daemon initialize response is missing capabilities"
        }

        Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'notifications/initialized' -Params @{} | Out-Null

        $requestId = 2
        $callParams = @{ name = $Name; arguments = $Arguments }
        $callResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $callParams -Id $requestId -TimeoutSec $TimeoutSec
        $callMessage = Read-SynapseMcpSseJsonResponse -Content $callResponse.Content -Operation "tools/call $Name" -ExpectedId $requestId
        if ($callMessage.result.isError -eq $true) {
            $errorText = @($callMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
            Die "SYNAPSE_MCP_TOOL_CALL_ERROR bind=$Bind session_id=$sessionId tool=$Name error=$errorText remediation=repair the live daemon/bridge before accepting setup"
        }
        $text = @($callMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
        $json = $null
        if (-not [string]::IsNullOrWhiteSpace($text)) {
            try {
                $json = $text | ConvertFrom-Json
            } catch {
                $json = $null
            }
        }
        return [pscustomobject]@{
            SessionId = $sessionId
            Message = $callMessage
            Text = $text
            Json = $json
        }
    } finally {
        if (-not [string]::IsNullOrWhiteSpace($sessionId)) {
            Close-SynapseMcpSetupSession -Bind $Bind -Token $Token -SessionId $sessionId
        }
    }
}

function Assert-SynapseChromeBridgeLiveAfterSetup {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)]$Health
    )

    $chromeBridge = $Health.subsystems.chrome_bridge
    $status = [string]$chromeBridge.status
    $detail = [string]$chromeBridge.detail
    $isClean = $status -eq 'ok' -and $detail -match 'extension_stale=false' -and $detail -match 'pageScreenshot'
    if ($isClean) {
        Info "Chrome bridge OK after daemon start: stale=false capability=pageScreenshot"
        return $Health
    }

    $currentHealth = $Health
    for ($attempt = 0; $attempt -lt 15; $attempt++) {
        if ($detail -notmatch 'no_active_chrome_bridge_host') {
            break
        }
        Info "Chrome bridge host absent after daemon start; waiting for alarmReconnect readback attempt=$($attempt + 1)/15"
        Start-Sleep -Seconds 2
        try {
            $currentHealth = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $Token" } -TimeoutSec 4
        } catch {
            Die "SYNAPSE_CHROME_BRIDGE_WAIT_HEALTH_FAILED bind=$Bind error=$($_.Exception.Message) remediation=daemon was live before Chrome bridge wait but /health failed during reconnect wait"
        }
        $chromeBridge = $currentHealth.subsystems.chrome_bridge
        $status = [string]$chromeBridge.status
        $detail = [string]$chromeBridge.detail
        $isClean = $status -eq 'ok' -and $detail -match 'extension_stale=false' -and $detail -match 'pageScreenshot'
        if ($isClean) {
            Info "Chrome bridge OK after daemon start wait: stale=false capability=pageScreenshot"
            return $currentHealth
        }
    }
    if ($detail -match 'no_active_chrome_bridge_host') {
        Die "SYNAPSE_CHROME_BRIDGE_HOST_ABSENT_AFTER_SETUP_WAIT status=$status detail=$detail remediation=setup waited for the already-installed bridge host to reconnect after daemon start; keep the active Chrome profile open and rerun scripts\\install-synapse-chrome-debugger.ps1 if the host does not register"
    }

    Info "WARN: Chrome bridge not clean after daemon start; requesting in-place cdp_bridge_reload through the new live MCP daemon. status=$status detail=$detail"
    $reload = Invoke-SynapseSetupMcpTool -Bind $Bind -Token $Token -Name 'cdp_bridge_reload' -Arguments @{ wait_timeout_ms = 30000 } -TimeoutSec 45
    $afterBuild = if ($reload.Json -and $reload.Json.after) { [string]$reload.Json.after.extension_build_id } else { 'unknown' }
    Info "Chrome bridge reload completed through MCP after_build_id=$afterBuild"

    try {
        $afterHealth = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $Token" } -TimeoutSec 4
    } catch {
        Die "SYNAPSE_CHROME_BRIDGE_POST_RELOAD_HEALTH_FAILED bind=$Bind error=$($_.Exception.Message) remediation=daemon was live before bridge reload but /health failed afterward"
    }
    $afterBridge = $afterHealth.subsystems.chrome_bridge
    $afterStatus = [string]$afterBridge.status
    $afterDetail = [string]$afterBridge.detail
    if ($afterStatus -ne 'ok' -or $afterDetail -notmatch 'extension_stale=false' -or $afterDetail -notmatch 'pageScreenshot') {
        Die "SYNAPSE_CHROME_BRIDGE_STALE_AFTER_SETUP_RELOAD status=$afterStatus detail=$afterDetail remediation=setup requires the already-open Chrome profile to load the bundled bridge build; run scripts\\install-synapse-chrome-debugger.ps1 from the interactive desktop and keep normal bridge commands failed closed until health is clean"
    }
    Info "Chrome bridge OK after setup reload: stale=false capability=pageScreenshot"
    return $afterHealth
}

function Read-SynapseDaemonToolSurface {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)]$Health
    )

    $sessionId = $null
    try {
        $initParams = [ordered]@{
            protocolVersion = '2025-06-18'
            capabilities = @{}
            clientInfo = [ordered]@{ name = 'synapse-setup'; version = '0' }
        }
        $initResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -Method 'initialize' -Params $initParams -Id 1
        $sessionId = @($initResponse.Headers['Mcp-Session-Id'])[0]
        if ([string]::IsNullOrWhiteSpace($sessionId)) {
            Die "SYNAPSE_MCP_TOOL_SURFACE_SESSION_MISSING bind=$Bind remediation=streamable HTTP initialize did not return Mcp-Session-Id; repair daemon transport"
        }
        $initMessage = Read-SynapseMcpSseJsonResponse -Content $initResponse.Content -Operation 'initialize' -ExpectedId 1
        if ($null -eq $initMessage.result -or $null -eq $initMessage.result.capabilities) {
            Die "SYNAPSE_MCP_INITIALIZE_RESULT_INVALID bind=$Bind session_id=$sessionId remediation=daemon initialize response is missing capabilities"
        }

        Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'notifications/initialized' -Params @{} | Out-Null

        $tools = @()
        $cursor = $null
        $requestId = 2
        do {
            $listParams = @{}
            if (-not [string]::IsNullOrWhiteSpace($cursor)) {
                $listParams['cursor'] = $cursor
            }
            $listResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/list' -Params $listParams -Id $requestId
            $listMessage = Read-SynapseMcpSseJsonResponse -Content $listResponse.Content -Operation 'tools/list' -ExpectedId $requestId
            if ($null -eq $listMessage.result -or $null -eq $listMessage.result.tools) {
                Die "SYNAPSE_MCP_TOOLS_LIST_RESULT_INVALID bind=$Bind session_id=$sessionId request_id=$requestId remediation=tools/list did not return a tools array"
            }
            $tools += @($listMessage.result.tools)
            $cursor = [string]$listMessage.result.nextCursor
            $requestId += 1
        } while (-not [string]::IsNullOrWhiteSpace($cursor))

        $sortedTools = @($tools | Sort-Object name)
        $toolNames = @($sortedTools | ForEach-Object { [string]$_.name })

        $healthCallParams = @{ name = 'health'; arguments = @{} }
        $healthCallResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $healthCallParams -Id $requestId
        $healthCallMessage = Read-SynapseMcpSseJsonResponse -Content $healthCallResponse.Content -Operation 'tools/call health' -ExpectedId $requestId
        $healthText = @($healthCallMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
        if ([string]::IsNullOrWhiteSpace($healthText)) {
            Die "SYNAPSE_MCP_HEALTH_TOOL_RESULT_INVALID bind=$Bind session_id=$sessionId request_id=$requestId remediation=health tools/call did not return JSON text content"
        }
        try {
            $sessionHealth = $healthText | ConvertFrom-Json
        } catch {
            Die "SYNAPSE_MCP_HEALTH_TOOL_JSON_INVALID bind=$Bind session_id=$sessionId request_id=$requestId error=$($_.Exception.Message) remediation=health tools/call returned non-JSON text"
        }
        $runtimeHash = [string]$sessionHealth.tool_surface_sha256
        $runtimeToolCount = try { [int]$sessionHealth.tool_count } catch { -1 }
        $runtimeNames = @($sessionHealth.tool_names | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object)
        $toolNamesSorted = @($toolNames | Sort-Object)
        $runtimeNamesJoined = ($runtimeNames -join "`n")
        $toolNamesJoined = ($toolNamesSorted -join "`n")
        if ([string]::IsNullOrWhiteSpace($runtimeHash) -or $runtimeToolCount -ne $toolNames.Count -or $runtimeNamesJoined -ne $toolNamesJoined) {
            Die ("SYNAPSE_MCP_HEALTH_TOOL_SURFACE_MISMATCH bind={0} session_id={1} tools_list_count={2} health_count={3} health_hash={4} tools_list_only={5} health_only={6} remediation=repair health/tool-list fingerprint agreement before writing Codex snapshot" -f `
                $Bind,
                $sessionId,
                $toolNames.Count,
                $runtimeToolCount,
                $runtimeHash,
                (Format-SynapseLimitedList -Items @($toolNamesSorted | Where-Object { $runtimeNames -notcontains $_ })),
                (Format-SynapseLimitedList -Items @($runtimeNames | Where-Object { $toolNamesSorted -notcontains $_ })))
        }

        $toolSchemas = @($sortedTools | ForEach-Object {
            $tool = $_
            $inputSchema = Get-SynapseObjectPropertyValue -Object $tool -Names @('inputSchema', 'input_schema')
            $outputSchema = Get-SynapseObjectPropertyValue -Object $tool -Names @('outputSchema', 'output_schema')
            $toolCanonical = Get-SynapseCanonicalJson -Value $tool
            [ordered]@{
                name = [string]$tool.name
                description = [string]$tool.description
                input_schema = $inputSchema
                input_schema_sha256 = Get-SynapseSha256Hex -Text (Get-SynapseCanonicalJson -Value $inputSchema)
                output_schema = $outputSchema
                output_schema_sha256 = if ($null -eq $outputSchema) { $null } else { Get-SynapseSha256Hex -Text (Get-SynapseCanonicalJson -Value $outputSchema) }
                tool_sha256 = Get-SynapseSha256Hex -Text $toolCanonical
            }
        })
        $canonical = Get-SynapseCanonicalJson -Value ([ordered]@{
            mcp_surface = 'tools/list'
            tools = $sortedTools
        })
        $setupCanonicalHash = Get-SynapseSha256Hex -Text $canonical
        $daemonPid = try { [int]$Health.pid } catch { $null }

        return [pscustomobject]([ordered]@{
            schema = 2
            created_at_utc = [DateTime]::UtcNow.ToString('o')
            bind = $Bind
            daemon_pid = $daemonPid
            tool_count = $toolNames.Count
            tool_surface_sha256 = $runtimeHash
            tool_surface_sha256_source = 'mcp_health_tool'
            tool_surface_setup_canonical_sha256 = $setupCanonicalHash
            tool_names = $toolNames
            tool_schemas = $toolSchemas
        })
    } finally {
        if (-not [string]::IsNullOrWhiteSpace($sessionId)) {
            Close-SynapseMcpSetupSession -Bind $Bind -Token $Token -SessionId $sessionId
        }
    }
}

function Get-SynapseFileSha256 {
    param([Parameter(Mandatory=$true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        Die "SYNAPSE_FILE_HASH_MISSING path=$Path remediation=build or install the daemon binary before hashing it"
    }
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash
}

function New-SynapseSetupRunDirectory {
    param(
        [Parameter(Mandatory=$true)][string]$Root,
        [Parameter(Mandatory=$true)][string]$Purpose
    )

    $safePurpose = $Purpose -replace '[^A-Za-z0-9_.-]', '_'
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssfffZ')
    $path = Join-Path $Root "$safePurpose-$stamp-$PID"
    New-Item -ItemType Directory -Force -Path $path | Out-Null
    return $path
}

function New-SynapseStagedDaemonBinary {
    param(
        [Parameter(Mandatory=$true)][string]$BuiltPath,
        [Parameter(Mandatory=$true)][string]$LogDir
    )

    $stagingRoot = Join-Path $LogDir 'setup-staging'
    $stagingDir = New-SynapseSetupRunDirectory -Root $stagingRoot -Purpose 'daemon-binary'
    $builtHash = Get-SynapseFileSha256 -Path $BuiltPath
    $stagedPath = Join-Path $stagingDir "synapse-mcp-$builtHash.exe"
    Copy-Item -LiteralPath $BuiltPath -Destination $stagedPath -Force
    $stagedHash = Get-SynapseFileSha256 -Path $stagedPath
    if ($stagedHash -ne $builtHash) {
        Die "SYNAPSE_STAGED_BINARY_HASH_MISMATCH built=$BuiltPath staged=$stagedPath built_hash=$builtHash staged_hash=$stagedHash remediation=inspect disk/storage; refusing to install an unverified binary"
    }
    Info "Staged daemon binary path=$stagedPath sha256=$stagedHash"
    return [pscustomobject]@{
        Path = $stagedPath
        Sha256 = $stagedHash
        SourcePath = $BuiltPath
    }
}

function New-SynapseCandidateBind {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Parse('127.0.0.1'), 0)
    try {
        $listener.Start()
        $port = [int]$listener.LocalEndpoint.Port
    } finally {
        $listener.Stop()
    }
    return "127.0.0.1:$port"
}

function Stop-SynapseExactCandidateProcess {
    param(
        [Parameter(Mandatory=$true)][int]$ProcessId,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [string]$Reason = 'candidate_health'
    )

    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
    if (-not $current) {
        Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds 5
        return
    }

    $shutdown = Request-SynapseGracefulShutdown -Bind $Bind -Token $Token -ExpectedPids @($ProcessId) -Reason $Reason
    if ($shutdown.Ok) {
        $deadline = (Get-Date).AddSeconds(10)
        do {
            Start-Sleep -Milliseconds 250
            $current = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
            if (-not $current) {
                Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds 5
                Info "Candidate daemon graceful shutdown verified pid=$ProcessId bind=$Bind"
                return
            }
        } while ((Get-Date) -lt $deadline)
    } else {
        Info "WARN: candidate graceful shutdown failed pid=$ProcessId bind=$Bind code=$($shutdown.Code) error=$($shutdown.Error); falling back to exact spawned PID stop"
    }

    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
    if ($current) {
        $exeLeaf = if ($current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
        if ($current.Name -inotlike 'synapse-mcp*.exe' -and $exeLeaf -inotlike 'synapse-mcp*.exe') {
            Die "SYNAPSE_CANDIDATE_STOP_TARGET_MISMATCH pid=$ProcessId actual_name=$($current.Name) actual_path=$($current.ExecutablePath) remediation=PID was reused before candidate cleanup; refusing to stop it"
        }
        Stop-Process -Id $ProcessId -Force -ErrorAction Stop
        Info "Candidate daemon exact spawned PID stop issued pid=$ProcessId bind=$Bind"
    }
    Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds 5
}

function Test-SynapseCandidateDaemon {
    param(
        [Parameter(Mandatory=$true)][string]$CandidateExePath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$LogDir
    )

    if (-not (Test-Path -LiteralPath $CandidateExePath)) {
        Die "SYNAPSE_CANDIDATE_BINARY_MISSING path=$CandidateExePath remediation=build or provide a real synapse-mcp.exe before setup can touch the live daemon"
    }
    if (-not (Test-Path -LiteralPath $ProfilesDir)) {
        Die "SYNAPSE_CANDIDATE_PROFILES_MISSING path=$ProfilesDir remediation=build/deploy profiles before validating the daemon candidate"
    }
    $tokenRead = Read-SynapseSetupTokenForRestartGuard -TokenPath $TokenPath
    if (-not $tokenRead.Ok) {
        Die "$($tokenRead.Code) stage=candidate_health $($tokenRead.Detail) remediation=setup must have a valid bearer token before candidate health can be proven"
    }

    $candidateRoot = New-SynapseSetupRunDirectory -Root (Join-Path $LogDir 'setup-candidates') -Purpose 'candidate'
    $candidateDb = Join-Path $candidateRoot 'db'
    New-Item -ItemType Directory -Force -Path $candidateDb | Out-Null
    $candidateBind = New-SynapseCandidateBind
    $candidateHash = Get-SynapseFileSha256 -Path $CandidateExePath
    Info "Candidate daemon health preflight starting exe=$CandidateExePath sha256=$candidateHash bind=$candidateBind db=$candidateDb profiles=$ProfilesDir"

    $candidate = $null
    $health = $null
    $surface = $null
    try {
        $candidate = Start-Process `
            -FilePath $CandidateExePath `
            -ArgumentList @('--mode','http','--bind',$candidateBind,'--db',$candidateDb,'--profile-dir',$ProfilesDir,'--log-level','info') `
            -WindowStyle Hidden `
            -PassThru
        $deadline = (Get-Date).AddSeconds(25)
        do {
            Start-Sleep -Milliseconds 500
            $read = Read-SynapseHealthForRestartGuard -Bind $candidateBind -Token $tokenRead.Token
            if ($read.Ok -and $read.Health.ok) {
                $health = $read.Health
                break
            }
        } while ((Get-Date) -lt $deadline)

        if ($null -eq $health) {
            $alive = [bool](Get-Process -Id $candidate.Id -ErrorAction SilentlyContinue)
            $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $candidateBind)
            Die ("SYNAPSE_CANDIDATE_HEALTH_FAILED exe={0} sha256={1} pid={2} alive={3} bind={4} listeners={5} remediation=the newly built daemon was not healthy on an isolated DB/port; old live daemon was not touched. Inspect candidate logs and setup-build.log." -f `
                $CandidateExePath,
                $candidateHash,
                $candidate.Id,
                $alive,
                $candidateBind,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners))
        }

        $healthPid = [int]$health.pid
        if ($healthPid -ne [int]$candidate.Id) {
            Die "SYNAPSE_CANDIDATE_PID_MISMATCH expected_pid=$($candidate.Id) health_pid=$healthPid bind=$candidateBind remediation=health came from an unexpected process; refusing handoff"
        }
        $surface = Read-SynapseDaemonToolSurface -Bind $candidateBind -Token $tokenRead.Token -Health $health
        if ($surface.tool_count -lt 1) {
            Die "SYNAPSE_CANDIDATE_TOOL_SURFACE_EMPTY pid=$healthPid bind=$candidateBind remediation=tools/list returned no tools; refusing handoff"
        }
        Info "Candidate daemon health preflight passed pid=$healthPid bind=$candidateBind tool_count=$($surface.tool_count) tool_surface_sha256=$($surface.tool_surface_sha256)"
        return [pscustomobject]@{
            Ok = $true
            Pid = $healthPid
            Bind = $candidateBind
            DbPath = $candidateDb
            ExePath = $CandidateExePath
            Sha256 = $candidateHash
            ToolCount = $surface.tool_count
            ToolSurfaceSha256 = $surface.tool_surface_sha256
            ToolNames = $surface.tool_names
            ToolSchemas = $surface.tool_schemas
            ToolSurface = $surface
            tool_count = $surface.tool_count
            tool_surface_sha256 = $surface.tool_surface_sha256
            tool_names = $surface.tool_names
            tool_schemas = $surface.tool_schemas
            daemon_pid = $healthPid
        }
    } finally {
        if ($candidate -and (Get-Process -Id $candidate.Id -ErrorAction SilentlyContinue)) {
            Stop-SynapseExactCandidateProcess -ProcessId ([int]$candidate.Id) -Bind $candidateBind -Token $tokenRead.Token -Reason 'candidate_health'
        } elseif ($candidateBind) {
            Wait-SynapseBindReleased -Reason 'candidate_health' -Bind $candidateBind -TimeoutSeconds 5
        }
    }
}

function Write-SynapseCodexToolSurfaceSnapshot {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)]$Surface
    )

    try {
        $dir = Split-Path -Parent $Path
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $json = $Surface | ConvertTo-Json -Depth 20
        $encoding = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($Path, $json, $encoding)
    } catch {
        Die "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_WRITE_FAILED path=$Path error=$($_.Exception.Message) remediation=repair permissions on the Synapse appdata directory before starting Codex"
    }
    Info "Codex tool-surface snapshot written path=$Path daemon_pid=$($Surface.daemon_pid) tool_count=$($Surface.tool_count) tool_surface_sha256=$($Surface.tool_surface_sha256)"
}

function Read-SynapseCodexToolSurfaceSnapshotOrNull {
    param([AllowNull()][string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path)) {
        return $null
    }
    try {
        return Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
    } catch {
        return [pscustomobject]@{
            unreadable = $true
            path = $Path
            error = $_.Exception.Message
        }
    }
}

function New-SynapseToolRecordMap {
    param([AllowNull()]$Surface)

    $map = @{}
    if ($null -eq $Surface -or $Surface.unreadable) {
        return $map
    }
    foreach ($record in @($Surface.tool_schemas)) {
        $name = [string]$record.name
        if (-not [string]::IsNullOrWhiteSpace($name)) {
            $map[$name] = $record
        }
    }
    return $map
}

function Get-SynapseNullableSchemaHash {
    param([AllowNull()]$Schema)

    if ($null -eq $Schema) {
        return '<null>'
    }
    return Get-SynapseSha256Hex -Text (Get-SynapseCanonicalJson -Value $Schema)
}

function Get-SynapseStoredHashOrEmpty {
    param(
        [AllowNull()]$Record,
        [Parameter(Mandatory=$true)][string]$Name
    )

    if ($null -eq $Record) {
        return ''
    }
    if ($Record -is [System.Collections.IDictionary] -and $Record.Contains($Name)) {
        if ($null -eq $Record[$Name]) {
            return ''
        }
        return [string]$Record[$Name]
    }
    $property = $Record.PSObject.Properties[$Name]
    if (-not $property -or $null -eq $property.Value) {
        return ''
    }
    return [string]$property.Value
}

function Get-SynapseRecordSchemaHash {
    param(
        [AllowNull()]$Record,
        [Parameter(Mandatory=$true)][string]$StoredHashName,
        [Parameter(Mandatory=$true)][string]$SchemaPropertyName
    )

    $stored = Get-SynapseStoredHashOrEmpty -Record $Record -Name $StoredHashName
    if (-not [string]::IsNullOrWhiteSpace($stored)) {
        return $stored
    }
    if ($null -eq $Record) {
        return '<null>'
    }
    if ($Record -is [System.Collections.IDictionary] -and $Record.Contains($SchemaPropertyName)) {
        return Get-SynapseNullableSchemaHash -Schema $Record[$SchemaPropertyName]
    }
    $property = $Record.PSObject.Properties[$SchemaPropertyName]
    $schema = if ($property) { $property.Value } else { $null }
    return Get-SynapseNullableSchemaHash -Schema $schema
}

function Format-SynapseLimitedList {
    param(
        [AllowNull()]$Items,
        [int]$Limit = 20
    )

    $values = @($Items | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) } | Sort-Object -Unique)
    if ($values.Count -eq 0) {
        return 'none'
    }
    $shown = @($values | Select-Object -First $Limit)
    $suffix = if ($values.Count -gt $Limit) { ",+$($values.Count - $Limit)_more" } else { '' }
    return (($shown -join ',') + $suffix)
}

function Get-SynapseToolSurfaceDiff {
    param(
        [AllowNull()]$StartSurface,
        [Parameter(Mandatory=$true)]$CurrentSurface
    )

    if ($null -eq $StartSurface) {
        return [pscustomobject]([ordered]@{
            Summary = 'start_snapshot=missing added=unknown removed=unknown callable_schema_changed=unknown'
            SchemaDetail = 'missing'
            HasRestartRequired = $true
            HasNameDelta = $true
            HasCallableSchemaChange = $true
        })
    }
    if ($StartSurface.unreadable) {
        return [pscustomobject]([ordered]@{
            Summary = "start_snapshot=unreadable error=$($StartSurface.error) added=unknown removed=unknown callable_schema_changed=unknown"
            SchemaDetail = 'unreadable'
            HasRestartRequired = $true
            HasNameDelta = $true
            HasCallableSchemaChange = $true
        })
    }

    $startNames = @($StartSurface.tool_names | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)
    $currentNames = @($CurrentSurface.tool_names | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)
    $added = @($currentNames | Where-Object { $startNames -notcontains $_ })
    $removed = @($startNames | Where-Object { $currentNames -notcontains $_ })

    $startMap = New-SynapseToolRecordMap -Surface $StartSurface
    $currentMap = New-SynapseToolRecordMap -Surface $CurrentSurface
    $inputChanged = @()
    $outputChanged = @()
    $descriptionChanged = @()
    $storedHashChanged = @()
    $storedSchemaHashOnlyChanged = @()
    if ($startMap.Count -gt 0 -and $currentMap.Count -gt 0) {
        foreach ($name in $currentNames) {
            if ($startMap.ContainsKey($name) -and $currentMap.ContainsKey($name)) {
                $startRecord = $startMap[$name]
                $currentRecord = $currentMap[$name]
                $startInputHash = Get-SynapseRecordSchemaHash -Record $startRecord -StoredHashName 'input_schema_sha256' -SchemaPropertyName 'input_schema'
                $currentInputHash = Get-SynapseRecordSchemaHash -Record $currentRecord -StoredHashName 'input_schema_sha256' -SchemaPropertyName 'input_schema'
                $startOutputHash = Get-SynapseRecordSchemaHash -Record $startRecord -StoredHashName 'output_schema_sha256' -SchemaPropertyName 'output_schema'
                $currentOutputHash = Get-SynapseRecordSchemaHash -Record $currentRecord -StoredHashName 'output_schema_sha256' -SchemaPropertyName 'output_schema'
                if ($startInputHash -ne $currentInputHash) {
                    $inputChanged += $name
                }
                if ($startOutputHash -ne $currentOutputHash) {
                    $outputChanged += $name
                }
                if ([string]$startRecord.description -ne [string]$currentRecord.description) {
                    $descriptionChanged += $name
                }
                $storedInputChanged = (Get-SynapseStoredHashOrEmpty -Record $startRecord -Name 'input_schema_sha256') -ne (Get-SynapseStoredHashOrEmpty -Record $currentRecord -Name 'input_schema_sha256')
                $storedOutputChanged = (Get-SynapseStoredHashOrEmpty -Record $startRecord -Name 'output_schema_sha256') -ne (Get-SynapseStoredHashOrEmpty -Record $currentRecord -Name 'output_schema_sha256')
                $storedToolChanged = (Get-SynapseStoredHashOrEmpty -Record $startRecord -Name 'tool_sha256') -ne (Get-SynapseStoredHashOrEmpty -Record $currentRecord -Name 'tool_sha256')
                if ($storedToolChanged) {
                    $storedHashChanged += $name
                }
                if (($storedInputChanged -or $storedOutputChanged) -and $startInputHash -eq $currentInputHash -and $startOutputHash -eq $currentOutputHash) {
                    $storedSchemaHashOnlyChanged += $name
                }
            }
        }
    }
    $schemaDetail = if ($startMap.Count -gt 0 -and $currentMap.Count -gt 0) { 'present' } else { 'missing' }
    $callableSchemaChanged = @($inputChanged + $outputChanged | Sort-Object -Unique)
    $hasNameDelta = ($added.Count -gt 0 -or $removed.Count -gt 0)
    $hasCallableSchemaChange = ($schemaDetail -ne 'present' -or $callableSchemaChanged.Count -gt 0)
    $summary = ("start_snapshot_schema_detail={0} added={1} removed={2} callable_schema_changed={3} input_schema_changed={4} output_schema_changed={5} description_changed={6} stored_tool_hash_changed={7} stored_schema_hash_only_changed={8}" -f `
        $schemaDetail,
        (Format-SynapseLimitedList -Items $added),
        (Format-SynapseLimitedList -Items $removed),
        (Format-SynapseLimitedList -Items $callableSchemaChanged),
        (Format-SynapseLimitedList -Items $inputChanged),
        (Format-SynapseLimitedList -Items $outputChanged),
        (Format-SynapseLimitedList -Items $descriptionChanged),
        (Format-SynapseLimitedList -Items $storedHashChanged),
        (Format-SynapseLimitedList -Items $storedSchemaHashOnlyChanged))
    return [pscustomobject]([ordered]@{
        Summary = $summary
        SchemaDetail = $schemaDetail
        Added = $added
        Removed = $removed
        InputSchemaChanged = $inputChanged
        OutputSchemaChanged = $outputChanged
        CallableSchemaChanged = $callableSchemaChanged
        DescriptionChanged = $descriptionChanged
        StoredToolHashChanged = $storedHashChanged
        StoredSchemaHashOnlyChanged = $storedSchemaHashOnlyChanged
        HasDescriptionChange = ($descriptionChanged.Count -gt 0)
        HasNameDelta = $hasNameDelta
        HasCallableSchemaChange = $hasCallableSchemaChange
        HasRestartRequired = ($hasNameDelta -or $hasCallableSchemaChange -or $descriptionChanged.Count -gt 0)
    })
}

function Get-SynapseToolSurfaceDiffSummary {
    param(
        [AllowNull()]$StartSurface,
        [Parameter(Mandatory=$true)]$CurrentSurface
    )

    $diff = Get-SynapseToolSurfaceDiff -StartSurface $StartSurface -CurrentSurface $CurrentSurface
    return $diff.Summary
}

function Write-SynapseUtf8NoBomFile {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Text
    )

    $dir = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Text, $encoding)
}

function Get-SynapseHandoffGitReadback {
    param([AllowNull()][string]$SourceDir)

    if ([string]::IsNullOrWhiteSpace($SourceDir) -or -not (Test-Path -LiteralPath $SourceDir)) {
        return [ordered]@{
            available = $false
            reason = 'source_dir_missing_or_not_supplied'
            source_dir = $SourceDir
        }
    }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        return [ordered]@{
            available = $false
            reason = 'git_not_found'
            source_dir = $SourceDir
        }
    }

    try {
        $status = @(& git -C $SourceDir status --short --branch 2>&1)
        $head = @(& git -C $SourceDir rev-parse HEAD 2>&1)
        $origin = @(& git -C $SourceDir rev-parse origin/main 2>&1)
        $branch = @(& git -C $SourceDir branch --show-current 2>&1)
        return [ordered]@{
            available = $true
            source_dir = $SourceDir
            branch = (($branch | Select-Object -First 1) -join '').Trim()
            head = (($head | Select-Object -First 1) -join '').Trim()
            origin_main = (($origin | Select-Object -First 1) -join '').Trim()
            status_short_branch = @($status)
        }
    } catch {
        return [ordered]@{
            available = $false
            reason = 'git_readback_failed'
            source_dir = $SourceDir
            error = $_.Exception.Message
        }
    }
}

function Get-SynapseRecoveryNotesPath {
    param([AllowNull()][string]$SourceDir)

    $candidates = @()
    if (-not [string]::IsNullOrWhiteSpace($SourceDir)) {
        $candidates += (Join-Path $SourceDir 'STATE\RECOVERY_NOTES.md')
    }
    $repoRootFromScript = Split-Path -Parent $PSScriptRoot
    if (-not [string]::IsNullOrWhiteSpace($repoRootFromScript)) {
        $candidates += (Join-Path $repoRootFromScript 'STATE\RECOVERY_NOTES.md')
    }

    foreach ($candidate in @($candidates | Select-Object -Unique)) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }
    $first = @($candidates | Select-Object -First 1)
    if ($first.Count -gt 0) {
        return [string]$first[0]
    }
    return $null
}

function Get-SynapseNormalizedIssueRef {
    param([AllowNull()][string]$Issue)

    if ([string]::IsNullOrWhiteSpace($Issue)) {
        return $null
    }

    $trimmed = $Issue.Trim()
    if ($trimmed -match '^#?(?<number>[0-9]+)$') {
        return "#$($Matches['number'])"
    }
    if ($trimmed -match '^https://github\.com/ChrisRoyse/Synapse/issues/(?<number>[0-9]+)(?:[/?#].*)?$') {
        return "#$($Matches['number'])"
    }

    Die "SYNAPSE_ACTIVE_ISSUE_INVALID value=$trimmed remediation=pass an issue number like 1441, #1441, or https://github.com/ChrisRoyse/Synapse/issues/1441"
}

function Get-SynapseIssueNumberFromRef {
    param([AllowNull()][string]$IssueRef)

    if ([string]::IsNullOrWhiteSpace($IssueRef)) {
        return $null
    }
    if ($IssueRef -match '^#(?<number>[0-9]+)$') {
        return $Matches['number']
    }
    return $null
}

function ConvertTo-SynapseHandoffDiffObject {
    param([AllowNull()]$Diff)

    if ($null -eq $Diff) {
        return [ordered]@{
            available = $false
            reason = 'diff_not_supplied'
        }
    }

    return [ordered]@{
        available = $true
        summary = [string]$Diff.Summary
        schema_detail = [string]$Diff.SchemaDetail
        has_restart_required = [bool]$Diff.HasRestartRequired
        has_name_delta = [bool]$Diff.HasNameDelta
        has_callable_schema_change = [bool]$Diff.HasCallableSchemaChange
        has_description_change = [bool]$Diff.HasDescriptionChange
        added = @($Diff.Added | ForEach-Object { [string]$_ })
        removed = @($Diff.Removed | ForEach-Object { [string]$_ })
        input_schema_changed = @($Diff.InputSchemaChanged | ForEach-Object { [string]$_ })
        output_schema_changed = @($Diff.OutputSchemaChanged | ForEach-Object { [string]$_ })
        callable_schema_changed = @($Diff.CallableSchemaChanged | ForEach-Object { [string]$_ })
        description_changed = @($Diff.DescriptionChanged | ForEach-Object { [string]$_ })
        stored_tool_hash_changed = @($Diff.StoredToolHashChanged | ForEach-Object { [string]$_ })
        stored_schema_hash_only_changed = @($Diff.StoredSchemaHashOnlyChanged | ForEach-Object { [string]$_ })
    }
}

function Write-SynapseCodexRestartHandoff {
    param(
        [Parameter(Mandatory=$true)][string]$Phase,
        [Parameter(Mandatory=$true)][string]$Reason,
        [AllowNull()]$CodexAncestor,
        [AllowNull()]$Surface,
        [AllowNull()]$Diff,
        [AllowNull()][string]$ProcessHashAtStart,
        [AllowNull()][string]$ProcessSnapshotAtStart,
        [AllowNull()][string]$CurrentSnapshotPath,
        [AllowNull()][string]$SourceDir,
        [AllowNull()][string]$Bind,
        [AllowNull()][string]$TokenPath,
        [AllowNull()][string]$ActiveIssue
    )

    $root = Join-Path $env:LOCALAPPDATA 'synapse\codex-restart-handoffs'
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
    $codexPid = if ($CodexAncestor) { [int]$CodexAncestor.ProcessId } else { 0 }
    $baseName = "codex-restart-handoff-$codexPid-$stamp"
    $jsonPath = Join-Path $root "$baseName.json"
    $mdPath = Join-Path $root "$baseName.md"
    $recoveryNotesPath = Get-SynapseRecoveryNotesPath -SourceDir $SourceDir
    $startSnapshotStatus = if ([string]::IsNullOrWhiteSpace($ProcessSnapshotAtStart)) {
        'missing_env'
    } elseif (Test-Path -LiteralPath $ProcessSnapshotAtStart) {
        'readable'
    } else {
        'missing_file'
    }

    $daemon = [ordered]@{
        bind = $Bind
        pid = if ($Surface -and $Surface.PSObject.Properties['daemon_pid']) { $Surface.daemon_pid } else { $null }
        tool_count = if ($Surface -and $Surface.PSObject.Properties['tool_count']) { $Surface.tool_count } else { $null }
        tool_surface_sha256 = if ($Surface -and $Surface.PSObject.Properties['tool_surface_sha256']) { [string]$Surface.tool_surface_sha256 } else { $null }
        snapshot_path = $CurrentSnapshotPath
    }
    $codexProcess = if ($CodexAncestor) {
        [ordered]@{
            pid = [int]$CodexAncestor.ProcessId
            name = [string]$CodexAncestor.Name
            command_line = [string]$CodexAncestor.CommandLine
        }
    } else {
        [ordered]@{
            pid = $null
            name = $null
            command_line = $null
        }
    }
    $diffObject = ConvertTo-SynapseHandoffDiffObject -Diff $Diff
    $gitReadback = Get-SynapseHandoffGitReadback -SourceDir $SourceDir
    $activeIssueRef = Get-SynapseNormalizedIssueRef -Issue $ActiveIssue
    $activeIssueNumber = Get-SynapseIssueNumberFromRef -IssueRef $activeIssueRef
    $activeIssueRead = if ([string]::IsNullOrWhiteSpace($activeIssueNumber)) {
        $null
    } else {
        "gh issue view $activeIssueNumber --repo ChrisRoyse/Synapse --comments"
    }
    $resumeInstruction = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) {
        'Resume the active GitHub issue from the caller/session context; if unknown, read the open issue queue and choose the issue that produced this setup handoff. Perform manual real-MCP FSV; do not use direct helper calls as acceptance.'
    } else {
        "Resume $activeIssueRef and perform manual real-MCP FSV; do not use direct helper calls as acceptance."
    }
    $restartCommandHint = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) {
        "Close this Codex session completely, start a new Codex session through the patched Codex launcher, verify the active codex.exe PID is not $codexPid, then resume the active issue from the caller/session context."
    } else {
        "Close this Codex session completely, start a new Codex session through the patched Codex launcher, verify the active codex.exe PID is not $codexPid, then resume $activeIssueRef."
    }
    $postRestartRequiredReads = @(
        'C:\Users\hotra\Downloads\AICodingAgentSuperPrompt.md',
        'C:\code\Synapse\docs\compressionprompt.md',
        'C:\code\Synapse\AGENTS.md',
        $recoveryNotesPath
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $githubReads = @(
        'gh issue view 351 --repo ChrisRoyse/Synapse --comments',
        $activeIssueRead,
        'gh issue list --repo ChrisRoyse/Synapse --state open --limit 100'
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }

    $record = [ordered]@{
        schema_version = 2
        artifact_kind = 'synapse_codex_restart_handoff'
        created_at_utc = [DateTime]::UtcNow.ToString('o')
        reason_code = 'SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE'
        reason = $Reason
        phase = $Phase
        required_restart = $true
        no_in_process_hot_refresh = $true
        explanation = 'The already-running Codex process has process-local MCP callable metadata that does not match the current daemon tools/list surface. Restart through the patched Codex launcher is the same-agent recovery boundary.'
        codex_process = $codexProcess
        daemon = $daemon
        current_process_start_surface = [ordered]@{
            env_hash_present = (-not [string]::IsNullOrWhiteSpace($ProcessHashAtStart))
            env_hash = $ProcessHashAtStart
            env_snapshot_path = $ProcessSnapshotAtStart
            snapshot_status = $startSnapshotStatus
        }
        diff = $diffObject
        post_restart_required_reads = $postRestartRequiredReads
        active_issue = [ordered]@{
            issue_ref = $activeIssueRef
            issue_number = $activeIssueNumber
            source = 'ActiveIssue parameter or SYNAPSE_ACTIVE_ISSUE environment variable'
            status = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown' } else { 'provided' }
        }
        stale_schema_context_issue = [ordered]@{
            issue_ref = '#1398'
            role = 'background context for the stale-schema bug class; not the resume target'
        }
        github_reads = $githubReads
        post_restart_verification = @(
            'Run git status --short --branch and confirm the working tree matches the handoff/recovery notes.',
            "Read the active Codex process parent chain and confirm the active codex.exe PID is not stale PID $codexPid from this handoff.",
            'Call real mcp__synapse.health and verify daemon pid/tool_surface_sha256 matches or intentionally supersedes this handoff.',
            'Run tool discovery for the previously missing Synapse tools. If metadata is still stale, rerun scripts\synapse-setup.ps1 and keep the issue open.',
            $resumeInstruction
        )
        restart_command_hint = $restartCommandHint
        repo_readback = $gitReadback
        token_path = $TokenPath
        recovery_notes_path = $recoveryNotesPath
    }

    try {
        New-Item -ItemType Directory -Force -Path $root | Out-Null
        Write-SynapseUtf8NoBomFile -Path $jsonPath -Text (($record | ConvertTo-Json -Depth 40) + "`n")
        $md = @(
            '# Synapse Codex Restart Handoff',
            '',
            "- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE ($Reason)",
            "- Phase: $Phase",
            "- Created UTC: $($record.created_at_utc)",
            "- Codex PID: $codexPid",
            "- Daemon: pid=$($daemon.pid) bind=$($daemon.bind) tool_count=$($daemon.tool_count) tool_surface_sha256=$($daemon.tool_surface_sha256)",
            "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
            "- Stale-schema context issue: #1398 (background only; not the resume target)",
            "- Current process start snapshot: status=$startSnapshotStatus hash=$ProcessHashAtStart path=$ProcessSnapshotAtStart",
            "- Current daemon snapshot: $CurrentSnapshotPath",
            "- Diff: $($diffObject.summary)",
            '',
            '## Required Restart',
            "The running Codex process cannot hot-add changed MCP tools or mutate cached tool schemas. Close stale Codex PID $codexPid completely, restart Codex through the patched launcher, and prove the active codex.exe PID changed before continuing. Typing continue into the same PID is not a restart.",
            '',
            '## Read After Restart'
        )
        foreach ($item in $postRestartRequiredReads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## GitHub Reads'
        )
        foreach ($item in $record.github_reads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## Verification'
        )
        foreach ($item in $record.post_restart_verification) {
            $md += "- $item"
        }
        $md += @(
            '',
            "JSON artifact: $jsonPath",
            ''
        )
        Write-SynapseUtf8NoBomFile -Path $mdPath -Text (($md -join "`n") + "`n")

        if (-not [string]::IsNullOrWhiteSpace($recoveryNotesPath)) {
            $notes = @(
                '# Synapse Recovery Notes',
                '',
                '## Latest Codex Restart Handoff',
                '',
                "- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE ($Reason)",
                "- Phase: $Phase",
                "- Created UTC: $($record.created_at_utc)",
                "- JSON: $jsonPath",
                "- Markdown: $mdPath",
                "- Stale Codex PID: $codexPid",
                "- Daemon bind: $Bind",
                "- Daemon tool surface: $($daemon.tool_surface_sha256)",
                "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
                "- Stale-schema context issue: #1398 (background only; not the resume target)",
                '',
                "After restart, re-read AGENTS.md, #351, $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'the active issue from the caller/session context' } else { $activeIssueRef }), git status, and this file before resuming. #1398 is stale-schema background context only. Use the real mcp__synapse client for FSV; direct helper calls are diagnostics only.",
                ''
            )
            Write-SynapseUtf8NoBomFile -Path $recoveryNotesPath -Text (($notes -join "`n") + "`n")
        }
    } catch {
        Die "SYNAPSE_CODEX_RESTART_HANDOFF_WRITE_FAILED phase=$Phase reason=$Reason path=$jsonPath error=$($_.Exception.Message) remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-restart-handoffs and the repo STATE directory, then rerun setup"
    }

    Info "SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE handoff_written phase=$Phase reason=$Reason json=$jsonPath markdown=$mdPath recovery_notes=$recoveryNotesPath"
    return [pscustomobject]@{
        JsonPath = $jsonPath
        MarkdownPath = $mdPath
        RecoveryNotesPath = $recoveryNotesPath
    }
}

function Assert-CodexCandidateHandoffPreservesCurrentProcess {
    param(
        [AllowNull()]$CodexAncestor,
        [Parameter(Mandatory=$true)]$CandidateSurface,
        [AllowNull()][string]$ProcessHashAtStart,
        [AllowNull()][string]$ProcessSnapshotAtStart,
        [AllowNull()][string]$SourceDir,
        [AllowNull()][string]$Bind,
        [AllowNull()][string]$TokenPath,
        [AllowNull()][string]$ActiveIssue
    )

    if ($null -eq $CodexAncestor) {
        return
    }

    $candidateHash = [string]$CandidateSurface.tool_surface_sha256
    if ([string]::IsNullOrWhiteSpace($candidateHash)) {
        Die "SYNAPSE_CODEX_CANDIDATE_TOOL_SURFACE_HASH_MISSING codex_pid=$($CodexAncestor.ProcessId) remediation=candidate tools/list preflight did not produce a usable tool_surface_sha256; refusing to touch the live daemon"
    }

    if ($ProcessHashAtStart -eq $candidateHash) {
        Info "Codex current-process tool surface will survive candidate handoff codex_pid=$($CodexAncestor.ProcessId) tool_surface_sha256=$candidateHash tool_count=$($CandidateSurface.tool_count)"
        return
    }

    $startSurface = Read-SynapseCodexToolSurfaceSnapshotOrNull -Path $ProcessSnapshotAtStart
    $diff = Get-SynapseToolSurfaceDiff -StartSurface $startSurface -CurrentSurface $CandidateSurface
    $diffSummary = $diff.Summary

    if ([string]::IsNullOrWhiteSpace($ProcessHashAtStart)) {
        $handoff = Write-SynapseCodexRestartHandoff `
            -Phase 'pre_handoff_candidate' `
            -Reason 'start_snapshot_missing_before_candidate_handoff' `
            -CodexAncestor $CodexAncestor `
            -Surface $CandidateSurface `
            -Diff $diff `
            -ProcessHashAtStart $ProcessHashAtStart `
            -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
            -CurrentSnapshotPath $null `
            -SourceDir $SourceDir `
            -Bind $Bind `
            -TokenPath $TokenPath `
            -ActiveIssue $ActiveIssue
        Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE_PRE_HANDOFF codex_pid={0} tool_surface_at_process_start=missing candidate_tool_surface_sha256={1} candidate_tool_count={2} candidate_pid={3} start_snapshot={4} handoff={5} {6} remediation=setup will continue only to install the verified daemon; final setup must fail closed if this Codex process remains stale." -f `
            $CodexAncestor.ProcessId,
            $candidateHash,
            $CandidateSurface.tool_count,
            $CandidateSurface.daemon_pid,
            $ProcessSnapshotAtStart,
            $handoff.JsonPath,
            $diffSummary)
        return
    }

    if (-not $diff.HasRestartRequired) {
        Info ("Codex current-process tool surface hash will change after candidate handoff but callable schema is unchanged; continuing codex_pid={0} start_tool_surface_sha256={1} candidate_tool_surface_sha256={2} candidate_tool_count={3} candidate_pid={4} start_snapshot={5} {6}" -f `
            $CodexAncestor.ProcessId,
            $ProcessHashAtStart,
            $candidateHash,
            $CandidateSurface.tool_count,
            $CandidateSurface.daemon_pid,
            $ProcessSnapshotAtStart,
            $diffSummary)
        return
    }

    $handoff = Write-SynapseCodexRestartHandoff `
        -Phase 'pre_handoff_candidate' `
        -Reason 'start_snapshot_hash_mismatch_before_candidate_handoff' `
        -CodexAncestor $CodexAncestor `
        -Surface $CandidateSurface `
        -Diff $diff `
        -ProcessHashAtStart $ProcessHashAtStart `
        -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
        -CurrentSnapshotPath $null `
        -SourceDir $SourceDir `
        -Bind $Bind `
        -TokenPath $TokenPath `
        -ActiveIssue $ActiveIssue
    Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE_PRE_HANDOFF codex_pid={0} start_tool_surface_sha256={1} candidate_tool_surface_sha256={2} candidate_tool_count={3} candidate_pid={4} start_snapshot={5} handoff={6} {7} remediation=setup will continue only to install the verified daemon; final setup must fail closed if this Codex process remains stale." -f `
        $CodexAncestor.ProcessId,
        $ProcessHashAtStart,
        $candidateHash,
        $CandidateSurface.tool_count,
        $CandidateSurface.daemon_pid,
        $ProcessSnapshotAtStart,
        $handoff.JsonPath,
        $diffSummary)
    return
}

function Assert-CodexCurrentProcessToolSurfaceFresh {
    param(
        [AllowNull()]$CodexAncestor,
        [Parameter(Mandatory=$true)]$CurrentSurface,
        [AllowNull()][string]$ProcessHashAtStart,
        [AllowNull()][string]$ProcessSnapshotAtStart,
        [Parameter(Mandatory=$true)][string]$SnapshotPath,
        [AllowNull()][string]$SourceDir,
        [AllowNull()][string]$Bind,
        [AllowNull()][string]$TokenPath,
        [AllowNull()][string]$ActiveIssue
    )

    if ($null -eq $CodexAncestor) {
        return
    }

    $currentHash = [string]$CurrentSurface.tool_surface_sha256
    $startSurface = Read-SynapseCodexToolSurfaceSnapshotOrNull -Path $ProcessSnapshotAtStart
    $diff = Get-SynapseToolSurfaceDiff -StartSurface $startSurface -CurrentSurface $CurrentSurface
    $diffSummary = $diff.Summary
    if ([string]::IsNullOrWhiteSpace($ProcessHashAtStart)) {
        $handoff = Write-SynapseCodexRestartHandoff `
            -Phase 'post_handoff_current_daemon' `
            -Reason 'start_snapshot_missing_after_daemon_handoff' `
            -CodexAncestor $CodexAncestor `
            -Surface $CurrentSurface `
            -Diff $diff `
            -ProcessHashAtStart $ProcessHashAtStart `
            -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
            -CurrentSnapshotPath $SnapshotPath `
            -SourceDir $SourceDir `
            -Bind $Bind `
            -TokenPath $TokenPath `
            -ActiveIssue $ActiveIssue
        Die ("SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE codex_pid={0} tool_surface_at_process_start=missing current_tool_surface_sha256={1} tool_count={2} daemon_pid={3} snapshot={4} start_snapshot={5} handoff={6} {7} remediation=restart Codex through the patched launcher, read the handoff plus STATE\\RECOVERY_NOTES.md, then resume the active issue named in the handoff and verify real mcp__synapse metadata." -f `
            $CodexAncestor.ProcessId,
            $currentHash,
            $CurrentSurface.tool_count,
            $CurrentSurface.daemon_pid,
            $SnapshotPath,
            $ProcessSnapshotAtStart,
            $handoff.JsonPath,
            $diffSummary)
    }

    if ($ProcessHashAtStart -ne $currentHash) {
        if (-not $diff.HasRestartRequired) {
            Info ("Codex current-process tool surface hash changed but callable schema is unchanged; continuing without restart handoff codex_pid={0} start_tool_surface_sha256={1} current_tool_surface_sha256={2} tool_count={3} daemon_pid={4} snapshot={5} start_snapshot={6} {7}" -f `
                $CodexAncestor.ProcessId,
                $ProcessHashAtStart,
                $currentHash,
                $CurrentSurface.tool_count,
                $CurrentSurface.daemon_pid,
                $SnapshotPath,
                $ProcessSnapshotAtStart,
                $diffSummary)
            return
        }
        $handoff = Write-SynapseCodexRestartHandoff `
            -Phase 'post_handoff_current_daemon' `
            -Reason 'start_snapshot_hash_mismatch_after_daemon_handoff' `
            -CodexAncestor $CodexAncestor `
            -Surface $CurrentSurface `
            -Diff $diff `
            -ProcessHashAtStart $ProcessHashAtStart `
            -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
            -CurrentSnapshotPath $SnapshotPath `
            -SourceDir $SourceDir `
            -Bind $Bind `
            -TokenPath $TokenPath `
            -ActiveIssue $ActiveIssue
        Die ("SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE codex_pid={0} tool_surface_at_process_start=mismatch start_tool_surface_sha256={1} current_tool_surface_sha256={2} tool_count={3} daemon_pid={4} snapshot={5} start_snapshot={6} handoff={7} {8} remediation=restart Codex through the patched launcher, read the handoff plus STATE\\RECOVERY_NOTES.md, then resume the active issue named in the handoff and verify real mcp__synapse metadata." -f `
            $CodexAncestor.ProcessId,
            $ProcessHashAtStart,
            $currentHash,
            $CurrentSurface.tool_count,
            $CurrentSurface.daemon_pid,
            $SnapshotPath,
            $ProcessSnapshotAtStart,
            $handoff.JsonPath,
            $diffSummary)
    }

    Info "Codex current-process tool surface matches daemon snapshot codex_pid=$($CodexAncestor.ProcessId) tool_surface_sha256=$currentHash tool_count=$($CurrentSurface.tool_count)"
}

function Request-SynapseGracefulShutdown {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][int[]]$ExpectedPids,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    try {
        $response = Invoke-RestMethod `
            -Method Post `
            -Uri "http://$Bind/shutdown" `
            -Headers @{ Authorization = "Bearer $Token" } `
            -UserAgent "synapse-setup/$Reason" `
            -TimeoutSec 4
    } catch {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_REQUEST_FAILED'; Response = $null; Error = $_.Exception.Message }
    }

    $responsePid = 0
    try {
        $responsePid = [int]$response.pid
    } catch {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_PID_UNREADABLE'; Response = $response; Error = $_.Exception.Message }
    }

    if ($ExpectedPids -notcontains $responsePid) {
        return [pscustomobject]@{
            Ok = $false
            Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_PID_MISMATCH'
            Response = $response
            Error = "response_pid=$responsePid expected_pids=$($ExpectedPids -join ',')"
        }
    }

    if ($response.ok -ne $true -or "$($response.shutdown)" -ne 'requested') {
        return [pscustomobject]@{
            Ok = $false
            Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_RESPONSE_INVALID'
            Response = $response
            Error = "response=$($response | ConvertTo-Json -Compress -Depth 6)"
        }
    }

    [pscustomobject]@{ Ok = $true; Code = 'OK'; Response = $response; Error = $null }
}

function Get-SynapseActiveSessionCount {
    param([Parameter(Mandatory=$true)]$Health)

    $value = $Health.subsystems.http.active_sessions
    if ($null -eq $value) {
        return $null
    }

    try {
        return [int]$value
    } catch {
        return $null
    }
}

function Assert-SynapseRestartAllowed {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [switch]$ForceRestart,
        [switch]$AllowActiveClientDrain
    )

    $allProcesses = @(Get-SynapseMcpProcessSnapshot)
    $processes = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allProcesses -Bind $Bind -DbPath $DbPath)
    $ignoredProcesses = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allProcesses -Bind $Bind -DbPath $DbPath -Invert)
    if ($ignoredProcesses.Count -gt 0) {
        Info ("Synapse restart guard reason={0} ignored_non_target_process_count={1}`nignored:`n{2}" -f `
            $Reason,
            $ignoredProcesses.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $ignoredProcesses))
    }
    if ($processes.Count -eq 0) {
        Info "Synapse restart guard reason=$Reason target_process_count=0 ignored_non_target_process_count=$($ignoredProcesses.Count) verdict=clear"
        return
    }

    $null = Get-SynapseBindEndpoint -Bind $Bind
    $nonHttpProcesses = @($processes | Where-Object { $_.CommandLine -notmatch '(?i)--mode\s+http' })
    $tokenRead = Read-SynapseSetupTokenForRestartGuard -TokenPath $TokenPath
    if (-not $tokenRead.Ok) {
        $message = "$($tokenRead.Code) reason=$Reason process_count=$($processes.Count) $($tokenRead.Detail) remediation=do not restart blindly while the daemon may have clients; repair token state or rerun with -ForceRestart after coordinating a maintenance window"
        if ($ForceRestart) {
            Info "FORCE_RESTART: $message"
        } else {
            Die $message
        }
    }

    $activeSessions = $null
    $healthRead = $null
    if ($tokenRead.Ok) {
        $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $tokenRead.Token
        if (-not $healthRead.Ok) {
            $message = "SYNAPSE_RESTART_GUARD_HEALTH_UNREADABLE reason=$Reason bind=$Bind error=$($healthRead.Error) remediation=do not restart blindly; repair the daemon/token or rerun with -ForceRestart after coordinating a maintenance window"
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } else {
                Die $message
            }
        }
    }

    if ($tokenRead.Ok -and $healthRead.Ok) {
        $activeSessions = Get-SynapseActiveSessionCount -Health $healthRead.Health
        if ($null -eq $activeSessions) {
            $message = "SYNAPSE_RESTART_GUARD_ACTIVE_SESSIONS_UNREADABLE reason=$Reason bind=$Bind remediation=health did not expose subsystems.http.active_sessions; do not restart blindly"
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } else {
                Die $message
            }
        }
    }

    $tcpConnections = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
    $setupLineagePids = @((Get-ProcessLineage -StartPid $PID) | ForEach-Object { [int]$_.ProcessId })
    $selfProbeTcpClients = @($tcpConnections | Where-Object {
        $_.HasLivePeer -and $setupLineagePids -contains [int]$_.PeerOwningProcess
    })
    $tcpClients = @($tcpConnections | Where-Object {
        $_.HasLivePeer -and $setupLineagePids -notcontains [int]$_.PeerOwningProcess
    })
    $staleTcpConnections = @($tcpConnections | Where-Object { -not $_.HasLivePeer })
    $blockers = @()
    $clientDrainBlockers = @()
    if ($nonHttpProcesses.Count -gt 0) { $blockers += "non_http_synapse_processes=$($nonHttpProcesses.Count)" }
    if ($tcpClients.Count -gt 0) {
        $blockers += "live_tcp_clients=$($tcpClients.Count)"
        $clientDrainBlockers += "live_tcp_clients=$($tcpClients.Count)"
    }
    if ($null -ne $activeSessions -and $activeSessions -gt 0 -and $tcpClients.Count -gt 0) {
        $blockers += "active_sessions=$activeSessions"
        $clientDrainBlockers += "active_sessions=$activeSessions"
    }
    if ($null -ne $activeSessions -and $activeSessions -gt 0 -and $tcpClients.Count -eq 0) {
        Info "Synapse restart guard reason=$Reason idle_session_map_entries=$activeSessions live_tcp_clients=0 verdict=not_blocking_idle_sessions"
    }
    if ($staleTcpConnections.Count -gt 0) {
        Info ("Synapse restart guard reason={0} stale_tcp_connections={1}`nstale_tcp:`n{2}" -f `
            $Reason,
            $staleTcpConnections.Count,
            (Format-SynapseTcpClientSnapshot -Snapshot $staleTcpConnections))
    }
    if ($selfProbeTcpClients.Count -gt 0) {
        Info ("Synapse restart guard reason={0} self_probe_tcp_connections={1}`nself_probe_tcp:`n{2}" -f `
            $Reason,
            $selfProbeTcpClients.Count,
            (Format-SynapseTcpClientSnapshot -Snapshot $selfProbeTcpClients))
    }

    if ($blockers.Count -gt 0) {
        $message = ("SYNAPSE_ACTIVE_CLIENTS_PRESENT reason={0} blockers={1} process_count={2} active_sessions={3} live_tcp_clients={4} idle_session_map_entries={5} stale_tcp_connections={6}`nprocesses:`n{7}`ntcp_clients:`n{8}`nstale_tcp:`n{9}`nremediation=wait for MCP clients to disconnect, close only the exact owner-known helper process listed here, or rerun with -ForceRestart only after coordinating a maintenance window. Do not close terminal windows or broad shell processes." -f `
            $Reason,
            ($blockers -join ','),
            $processes.Count,
            ($(if ($null -eq $activeSessions) { 'unknown' } else { $activeSessions })),
            $tcpClients.Count,
            ($(if ($null -eq $activeSessions) { 'unknown' } else { $activeSessions })),
            $staleTcpConnections.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $processes),
            (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients),
            (Format-SynapseTcpClientSnapshot -Snapshot $staleTcpConnections))
        if ($nonHttpProcesses.Count -gt 0 -and -not $ForceRestart) {
            Die $message
        } elseif ($AllowActiveClientDrain -and $clientDrainBlockers.Count -gt 0 -and $nonHttpProcesses.Count -eq 0) {
            Info ("Synapse restart guard reason={0} verdict=drain_permitted blockers={1} active_sessions={2} live_tcp_clients={3} stale_tcp_connections={4} process_count={5} drain=authenticated_http_shutdown" -f `
                $Reason,
                ($clientDrainBlockers -join ','),
                ($(if ($null -eq $activeSessions) { 'unknown' } else { $activeSessions })),
                $tcpClients.Count,
                $staleTcpConnections.Count,
                $processes.Count)
        } elseif ($ForceRestart) {
            Info "FORCE_RESTART: $message"
        } else {
            Die $message
        }
    } else {
        Info "Synapse restart guard reason=$Reason verdict=clear active_sessions=$activeSessions live_tcp_clients=0 stale_tcp_connections=$($staleTcpConnections.Count) process_count=$($processes.Count)"
    }
}

function Assert-SynapseProcessStopTarget {
    param(
        [Parameter(Mandatory=$true)]$SnapshotProcess,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath
    )

    $pidValue = [int]$SnapshotProcess.ProcessId
    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
    if (-not $current) {
        Info "Synapse process stop target already exited pid=$pidValue"
        return $null
    }

    $protectedNames = @(
        'cmd.exe',
        'powershell.exe',
        'pwsh.exe',
        'WindowsTerminal.exe',
        'OpenConsole.exe',
        'conhost.exe',
        'wsl.exe',
        'wslhost.exe',
        'Code.exe'
    )
    if ($protectedNames -contains $current.Name) {
        Die ("SYNAPSE_PROTECTED_PROCESS_STOP_REFUSED pid={0} name={1} command_line={2} remediation=terminal/IDE/WSL host processes are operator and agent workspaces; never close them from setup, tests, or FSV. Stop only exact owner-known helper PIDs." -f `
            $pidValue,
            $current.Name,
            $current.CommandLine)
    }

    $exeLeaf = if ($current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
    if ($current.Name -ine 'synapse-mcp.exe' -and $exeLeaf -ine 'synapse-mcp.exe') {
        Die ("SYNAPSE_PROCESS_STOP_TARGET_MISMATCH pid={0} expected=synapse-mcp.exe actual_name={1} actual_path={2} command_line={3} remediation=PID was reused or snapshot was not a Synapse process; refusing exact-PID stop" -f `
            $pidValue,
            $current.Name,
            $current.ExecutablePath,
            $current.CommandLine)
    }
    if ($current.CommandLine -notmatch '(?i)synapse-mcp(\.exe)?') {
        Die ("SYNAPSE_PROCESS_STOP_TARGET_UNVERIFIED pid={0} name={1} command_line={2} remediation=command line does not prove a Synapse MCP target; refusing exact-PID stop" -f `
            $pidValue,
            $current.Name,
            $current.CommandLine)
    }

    $targetMatch = Get-SynapseMcpDeployTargetMatch -Process $current -Bind $Bind -DbPath $DbPath
    if (-not $targetMatch.IsMatch) {
        Die ("SYNAPSE_PROCESS_STOP_TARGET_SCOPE_MISMATCH pid={0} bind={1} db={2} actual_bind_arg={3} actual_db_arg={4} command_line={5} remediation=setup only stops synapse-mcp.exe processes whose command line targets the deployed --bind or --db; refusing collateral stop" -f `
            $pidValue,
            $Bind,
            $targetMatch.ExpectedDb,
            $targetMatch.BindArg,
            $targetMatch.DbArg,
            $current.CommandLine)
    }
    $rules = $targetMatch.Rules -join ','
    $current | Add-Member -NotePropertyName DeployTargetMatched -NotePropertyValue $true -Force
    $current | Add-Member -NotePropertyName DeployTargetRules -NotePropertyValue $rules -Force
    $current | Add-Member -NotePropertyName DeployTargetBindArg -NotePropertyValue $targetMatch.BindArg -Force
    $current | Add-Member -NotePropertyName DeployTargetDbArg -NotePropertyValue $targetMatch.DbArg -Force
    Info "Synapse process stop target verified pid=$pidValue match_rules=$rules path=$($current.ExecutablePath) cmd=$($current.CommandLine)"

    return $current
}

function Stop-SynapseMcpProcesses {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [switch]$ForceRestart,
        [int]$TimeoutSeconds = 15
    )

    $allBefore = @(Get-SynapseMcpProcessSnapshot)
    $before = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allBefore -Bind $Bind -DbPath $DbPath)
    $ignoredBefore = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allBefore -Bind $Bind -DbPath $DbPath -Invert)
    Info "Synapse process stop requested reason=$Reason before_all_count=$($allBefore.Count) target_count=$($before.Count) ignored_non_target_count=$($ignoredBefore.Count) bind=$Bind db=$DbPath"
    Info ("Synapse process stop target candidates:`n{0}" -f (Format-SynapseMcpProcessSnapshot -Snapshot $before))
    if ($ignoredBefore.Count -gt 0) {
        Info ("Synapse process stop ignored non-target processes:`n{0}" -f (Format-SynapseMcpProcessSnapshot -Snapshot $ignoredBefore))
    }
    if ($before.Count -eq 0) {
        Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds
        return
    }

    foreach ($proc in $before) {
        $null = Assert-SynapseProcessStopTarget -SnapshotProcess $proc -Bind $Bind -DbPath $DbPath
    }

    $httpProcesses = @($before | Where-Object { $_.CommandLine -match '(?i)--mode\s+http' })
    $nonHttpProcesses = @($before | Where-Object { $_.CommandLine -notmatch '(?i)--mode\s+http' })
    if ($nonHttpProcesses.Count -gt 0 -and -not $ForceRestart) {
        Die ("SYNAPSE_GRACEFUL_SHUTDOWN_NON_HTTP_PROCESS reason={0} count={1}`nprocesses:`n{2}`nremediation=setup will not force-stop stdio/bridge/non-http Synapse processes without -ForceRestart; run synapse-mcp --mode doctor to inspect ownership, or coordinate a maintenance window before forcing exact verified PIDs. Do not close terminal windows." -f `
            $Reason,
            $nonHttpProcesses.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $nonHttpProcesses))
    }

    if ($httpProcesses.Count -gt 0) {
        $tokenRead = Read-SynapseSetupTokenForRestartGuard -TokenPath $TokenPath
        if (-not $tokenRead.Ok) {
            $message = ("{0} reason={1} process_count={2} {3} remediation=graceful shutdown requires the daemon bearer token; repair token state before setup, or use -ForceRestart only after manually verifying no held inputs and no live clients." -f `
                $tokenRead.Code,
                $Reason,
                $httpProcesses.Count,
                $tokenRead.Detail)
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } else {
                Die $message
            }
        } else {
            $expectedPids = @($httpProcesses | ForEach-Object { [int]$_.ProcessId })
            $shutdown = Request-SynapseGracefulShutdown -Bind $Bind -Token $tokenRead.Token -ExpectedPids $expectedPids -Reason $Reason
            if (-not $shutdown.Ok) {
                $message = ("{0} reason={1} bind={2} error={3} response={4} remediation=the running daemon did not accept authenticated graceful shutdown; inspect daemon logs and token/bind state. Use -ForceRestart only for a coordinated legacy-runtime transition after manual input-state readback." -f `
                    $shutdown.Code,
                    $Reason,
                    $Bind,
                    $shutdown.Error,
                    ($(if ($null -eq $shutdown.Response) { '<none>' } else { $shutdown.Response | ConvertTo-Json -Compress -Depth 6 })))
                if ($ForceRestart) {
                    Info "FORCE_RESTART: $message"
                } else {
                    Die $message
                }
            } else {
                Info ("Synapse graceful shutdown requested reason={0} pid={1} active_sessions_before_shutdown={2}" -f `
                    $Reason,
                    $shutdown.Response.pid,
                    $shutdown.Response.active_sessions_before_shutdown)
            }
        }

        $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
        do {
            Start-Sleep -Milliseconds 250
            $remainingHttpPids = @($httpProcesses | Where-Object {
                $pidValue = [int]$_.ProcessId
                $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
                $exeLeaf = if ($current -and $current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
                $null -ne $current -and ($current.Name -ieq 'synapse-mcp.exe' -or $exeLeaf -ieq 'synapse-mcp.exe')
            })
            if ($remainingHttpPids.Count -eq 0) {
                Info "Synapse graceful shutdown verified reason=$Reason http_process_count=0"
                Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds
                break
            }
        } while ((Get-Date) -lt $deadline)

        $remainingHttpPids = @($httpProcesses | Where-Object {
            $pidValue = [int]$_.ProcessId
            $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
            $exeLeaf = if ($current -and $current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
            $null -ne $current -and ($current.Name -ieq 'synapse-mcp.exe' -or $exeLeaf -ieq 'synapse-mcp.exe')
        })
        if ($remainingHttpPids.Count -gt 0) {
            $message = ("SYNAPSE_GRACEFUL_SHUTDOWN_TIMEOUT reason={0} timeout_s={1} remaining_count={2}`nremaining:`n{3}" -f `
                $Reason,
                $TimeoutSeconds,
                $remainingHttpPids.Count,
                (Format-SynapseMcpProcessSnapshot -Snapshot $remainingHttpPids))
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } else {
                Die $message
            }
        }
    }

    $remaining = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    if ($remaining.Count -eq 0) {
        Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds
        $ignoredAfter = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath -Invert)
        Info "Synapse process stop verified reason=$Reason target_after_count=0 ignored_non_target_after_count=$($ignoredAfter.Count)"
        return
    }

    if (-not $ForceRestart) {
        Die ("SYNAPSE_PROCESS_STOP_INCOMPLETE reason={0} remaining_count={1} remaining=`n{2}" -f `
            $Reason,
            $remaining.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $remaining))
    }

    Info ("FORCE_RESTART: exact-PID stop for remaining verified Synapse processes reason={0} remaining_count={1}`nremaining:`n{2}" -f `
        $Reason,
        $remaining.Count,
        (Format-SynapseMcpProcessSnapshot -Snapshot $remaining))
    foreach ($proc in $remaining) {
        $verified = Assert-SynapseProcessStopTarget -SnapshotProcess $proc -Bind $Bind -DbPath $DbPath
        if (-not $verified) { continue }
        $pidValue = [int]$verified.ProcessId
        try {
            Stop-Process -Id $pidValue -Force -ErrorAction Stop
            Info "Synapse process exact-PID force stop issued pid=$pidValue reason=$Reason match_rules=$($verified.DeployTargetRules) path=$($verified.ExecutablePath) cmd=$($verified.CommandLine)"
        } catch {
            Die ("SYNAPSE_PROCESS_STOP_FAILED pid={0} reason={1} error={2} remediation=setup only stops verified synapse-mcp.exe PIDs; inspect process ownership and retry after the daemon exits" -f `
                $pidValue,
                $Reason,
                $_.Exception.Message)
        }
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        Start-Sleep -Milliseconds 250
        $after = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
        if ($after.Count -eq 0) {
            Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds
            $ignoredAfter = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath -Invert)
            Info "Synapse process stop verified reason=$Reason target_after_count=0 ignored_non_target_after_count=$($ignoredAfter.Count)"
            return
        }
    } while ((Get-Date) -lt $deadline)

    $remaining = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    Die ("SYNAPSE_PROCESS_STOP_FAILED reason={0} timeout_s={1} remaining_count={2} remaining=`n{3}" -f `
        $Reason, $TimeoutSeconds, $remaining.Count, (Format-SynapseMcpProcessSnapshot -Snapshot $remaining))
}

function Get-SynapseChromeNativeHostProcessSnapshot {
    param([Parameter(Mandatory=$true)][string]$NativeHostExePath)

    $expectedPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($NativeHostExePath)
    @(Get-CimInstance Win32_Process -Filter "Name='synapse-chrome-native-host.exe'" -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ExecutablePath -and
            ($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($_.ExecutablePath) -ieq $expectedPath)
        } |
        Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,CommandLine)
}

function Format-SynapseChromeNativeHostProcessSnapshot {
    param($Snapshot)
    $rows = @($Snapshot | ForEach-Object {
        "pid=$($_.ProcessId) ppid=$($_.ParentProcessId) path=$($_.ExecutablePath) cmd=$($_.CommandLine)"
    })
    if ($rows.Count -eq 0) { return '<none>' }
    return ($rows -join "`n")
}

function Assert-SynapseChromeNativeHostStopTarget {
    param(
        [Parameter(Mandatory=$true)]$SnapshotProcess,
        [Parameter(Mandatory=$true)][string]$NativeHostExePath
    )

    $pidValue = [int]$SnapshotProcess.ProcessId
    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
    if (-not $current) {
        Info "Chrome native host stop target already exited pid=$pidValue"
        return $null
    }

    $expectedPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($NativeHostExePath)
    $actualPath = if ($current.ExecutablePath) { $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($current.ExecutablePath) } else { '' }
    if ($current.Name -ine 'synapse-chrome-native-host.exe' -or $actualPath -ine $expectedPath) {
        Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_TARGET_MISMATCH pid={0} expected_path={1} actual_name={2} actual_path={3} command_line={4} remediation=PID was reused or snapshot was not the Synapse Chrome native host; refusing exact-PID stop" -f `
            $pidValue,
            $expectedPath,
            $current.Name,
            $current.ExecutablePath,
            $current.CommandLine)
    }
    if ($current.CommandLine -notmatch 'chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/') {
        Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_UNVERIFIED pid={0} name={1} command_line={2} remediation=command line does not prove the Synapse Chrome extension bridge target; refusing exact-PID stop" -f `
            $pidValue,
            $current.Name,
            $current.CommandLine)
    }

    return $current
}

function Stop-SynapseChromeNativeHostProcesses {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$NativeHostExePath,
        [int]$TimeoutSeconds = 10
    )

    $before = @(Get-SynapseChromeNativeHostProcessSnapshot -NativeHostExePath $NativeHostExePath)
    Info "Chrome native host process stop requested reason=$Reason before_count=$($before.Count)"
    Info ("Chrome native host process stop before:`n{0}" -f (Format-SynapseChromeNativeHostProcessSnapshot -Snapshot $before))
    foreach ($proc in $before) {
        $verified = Assert-SynapseChromeNativeHostStopTarget -SnapshotProcess $proc -NativeHostExePath $NativeHostExePath
        if (-not $verified) { continue }
        $pidValue = [int]$verified.ProcessId
        try {
            Stop-Process -Id $pidValue -Force -ErrorAction Stop
            Info "Chrome native host exact-PID stop issued pid=$pidValue reason=$Reason"
        } catch {
            Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_FAILED pid={0} reason={1} error={2} remediation=setup only stops verified synapse-chrome-native-host.exe PIDs; it never stops cmd.exe/native-messaging wrapper or terminal processes" -f `
                $pidValue,
                $Reason,
                $_.Exception.Message)
        }
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        Start-Sleep -Milliseconds 250
        $after = @(Get-SynapseChromeNativeHostProcessSnapshot -NativeHostExePath $NativeHostExePath)
        if ($after.Count -eq 0) {
            Info "Chrome native host process stop verified reason=$Reason after_count=0"
            return
        }
    } while ((Get-Date) -lt $deadline)

    $remaining = @(Get-SynapseChromeNativeHostProcessSnapshot -NativeHostExePath $NativeHostExePath)
    Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_FAILED reason={0} timeout_s={1} remaining_count={2} remaining=`n{3}" -f `
        $Reason, $TimeoutSeconds, $remaining.Count, (Format-SynapseChromeNativeHostProcessSnapshot -Snapshot $remaining))
}

# ---------------------------------------------------------------------------
# Uninstall path
# ---------------------------------------------------------------------------
$maintenanceReason = if ($Remove) { 'remove' } else { 'setup' }
Acquire-SynapseSetupMaintenanceLock -Path $MaintenanceLockPath -Reason $maintenanceReason

if ($Remove) {
    Step "Removing scheduled task '$TaskName'"
    Assert-SynapseRestartAllowed -Reason 'remove' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -ForceRestart:$ForceRestart
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask  -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Info "Unregistered '$TaskName'."
    } else { Info "Task '$TaskName' not present." }
    Stop-SynapseMcpProcesses -Reason 'remove' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -ForceRestart:$ForceRestart
    if ($Purge) {
        foreach ($p in @($DbPath, $ProfilesDir, (Split-Path -Parent $TokenPath))) {
            if (Test-Path $p) { Remove-Item -Recurse -Force $p; Info "Deleted $p" }
        }
    }
    Info "Done (remove)."
    Release-SynapseSetupMaintenanceLock -State released
    return
}

# ---------------------------------------------------------------------------
# 1. Preflight
# ---------------------------------------------------------------------------
Step "Preflight"
$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
if (-not $SkipBuild) {
    if (-not (Test-Path $cargo)) {
        Die "cargo not found at $cargo. Install the Rust toolchain (https://rustup.rs) on Windows, then re-run. Synapse builds with the current stable toolchain."
    }
    if (-not $SourceDir) { Die "-SourceDir is required unless -SkipBuild is set." }
    if (-not (Test-Path (Join-Path $SourceDir 'Cargo.toml'))) {
        Die "-SourceDir '$SourceDir' has no Cargo.toml. Point it at a synapse source checkout on a LOCAL drive."
    }
    if ($SourceDir -match '^\\\\' -or $SourceDir -match '^[Zz]:\\home\\') {
        Die "-SourceDir '$SourceDir' looks like a UNC / WSL-mapped path. Build from a real local copy: building over \\wsl.localhost bakes transient drive paths into the binary."
    }
    Info "cargo: $((& $cargo --version))"
}

# ---------------------------------------------------------------------------
# 2. Build (local source -> persistent target) and verify the binary
# ---------------------------------------------------------------------------
if (-not $SkipBuild) {
    Step "Building synapse-mcp (release) from $SourceDir"
    if (-not $PSBoundParameters.ContainsKey('CargoTarget')) {
        # Key the persistent target by the source checkout so two checkouts
        # can never share (and poison) one fingerprint database. Cargo
        # freshness is mtime-based against the dep-info file list: a build
        # from checkout A marks its units fresh for checkout A's files, and
        # a later build from checkout B silently reuses crates whose sources
        # differ (observed live 2026-06-12: a synapse-core compiled from a
        # sibling clone shadowed new modules and broke the deploy build).
        $resolvedSource = (Resolve-Path $SourceDir).Path.TrimEnd('\')
        $sourceBytes = [System.Text.Encoding]::UTF8.GetBytes($resolvedSource.ToLowerInvariant())
        $sourceHash = ([System.Security.Cryptography.SHA1]::Create().ComputeHash($sourceBytes) |
            ForEach-Object { $_.ToString('x2') }) -join ''
        $sourceLeaf = (Split-Path $resolvedSource -Leaf) -replace '[^A-Za-z0-9._-]', '_'
        $CargoTarget = Join-Path $CargoTarget "$sourceLeaf-$($sourceHash.Substring(0,12))"
        Info "Per-checkout build target: $CargoTarget (source: $resolvedSource)"
    }
    New-Item -ItemType Directory -Force -Path $CargoTarget, $LogDir | Out-Null
    $env:CARGO_TARGET_DIR = $CargoTarget
    if (-not $env:CARGO_BUILD_JOBS) {
        # Full-machine parallelism even when setup runs directly (not via
        # synapse-update.ps1). The env var outranks any `[build] jobs = N` cap
        # in a user/repo cargo config.toml that would otherwise silently
        # serialize the build; cargo forwards it to build scripts as NUM_JOBS,
        # which parallelizes the RocksDB C++ compile. RAM-guarded (~1.5 GB per
        # heavy rustc/cl.exe job) so low-memory machines don't swap.
        $logicalCpus = [Environment]::ProcessorCount
        $ramGb = [math]::Floor((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
        $env:CARGO_BUILD_JOBS = [string][int][math]::Min($logicalCpus, [math]::Max(1, [math]::Floor($ramGb / 1.5)))
    }
    Info "Build parallelism: CARGO_BUILD_JOBS=$($env:CARGO_BUILD_JOBS) (logical CPUs: $([Environment]::ProcessorCount))"
    $buildLog = Join-Path $LogDir 'setup-build.log'
    Info "Build process tree is job-owned; log: $buildLog"
    $buildExit = Invoke-SynapseProcessInKillOnCloseJob `
        -FilePath $cargo `
        -ArgumentList @('build','--release','-p','synapse-mcp') `
        -WorkingDirectory $SourceDir `
        -TimeoutMinutes $BuildTimeoutMinutes `
        -LogPath $buildLog
    if ($buildExit -ne 0) {
        $tail = if (Test-Path $buildLog) { (Get-Content -Path $buildLog -Tail 80 -ErrorAction SilentlyContinue) -join "`n" } else { '' }
        Die "cargo build failed (exit $buildExit). Build log: $buildLog. Tail:`n$tail"
    }
    $built = Join-Path $CargoTarget 'release\synapse-mcp.exe'
    if (-not (Test-Path $built)) { Die "Build reported success but $built is missing." }
    Info "Built: $built ($([math]::Round((Get-Item $built).Length/1MB,1)) MB)"
}

# ---------------------------------------------------------------------------
# 3. Token, data dirs, and profile source resolution
# ---------------------------------------------------------------------------
Step "Bearer token + data dirs"
$tokDir = Split-Path -Parent $TokenPath
New-Item -ItemType Directory -Force -Path $tokDir, $DbPath, $LogDir | Out-Null
if (-not (Test-Path $TokenPath)) {
    $bytes = New-Object byte[] 32
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    ($bytes | ForEach-Object { $_.ToString('x2') }) -join '' | Set-Content -Path $TokenPath -NoNewline -Encoding ascii
    Info "Generated token -> $TokenPath"
} else { Info "Reusing token -> $TokenPath" }
$tokenRaw = Get-Content -Raw $TokenPath
$token = if ($null -eq $tokenRaw) { '' } else { $tokenRaw.Trim() }
if ($token.Length -lt 16) { Die "Token at $TokenPath is too short ($($token.Length) chars); delete it and re-run to regenerate." }
[Environment]::SetEnvironmentVariable('SYNAPSE_BEARER_TOKEN', $token, 'User')
$env:SYNAPSE_BEARER_TOKEN = $token
Info "Set Windows User SYNAPSE_BEARER_TOKEN from $TokenPath for native HTTP MCP clients that require env-based bearer auth."
try {
    $signature = '[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Auto)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);'
    $type = Add-Type -MemberDefinition $signature -Name Win32SendMessageTimeout -Namespace SynapseEnv -PassThru -ErrorAction Stop
    $broadcastResult = [UIntPtr]::Zero
    $rawReturn = $type::SendMessageTimeout([IntPtr]0xffff, 0x001A, [UIntPtr]::Zero, 'Environment', 0x0002, 5000, [ref]$broadcastResult)
    if ($rawReturn -eq [IntPtr]::Zero) {
        Info "WARN: environment broadcast returned 0; future GUI clients may need restart before seeing SYNAPSE_BEARER_TOKEN."
    }
} catch {
    Info "WARN: environment broadcast failed: $($_.Exception.Message). Future GUI clients may need restart before seeing SYNAPSE_BEARER_TOKEN."
}

$srcProfiles = if ($SourceDir) { Join-Path $SourceDir 'crates\synapse-profiles\profiles' } else { $null }
$candidateProfilesDir = $null
if ($srcProfiles -and (Test-Path $srcProfiles)) {
    $candidateProfilesDir = $srcProfiles
    Info "Candidate profile source: $candidateProfilesDir"
} elseif (Test-Path $ProfilesDir) {
    $candidateProfilesDir = $ProfilesDir
    Info "Candidate profile source: existing deployed profiles at $candidateProfilesDir"
} else {
    Die "SYNAPSE_CANDIDATE_PROFILES_MISSING source=$srcProfiles deployed=$ProfilesDir remediation=provide bundled profiles from SourceDir or an existing deployed ProfilesDir before setup can touch the live daemon"
}
$candidateProfileCount = (Get-ChildItem $candidateProfilesDir -Filter *.toml -File).Count
if ($candidateProfileCount -lt 1) {
    Die "SYNAPSE_CANDIDATE_PROFILES_EMPTY path=$candidateProfilesDir remediation=profile-dependent tools need at least one .toml profile before setup can touch the live daemon"
}
Info "Candidate profiles verified path=$candidateProfilesDir count=$candidateProfileCount"

# ---------------------------------------------------------------------------
# 4. Stage and health-check the replacement before touching the live daemon
# ---------------------------------------------------------------------------
Step "Validating candidate daemon before handoff"
$installSourcePath = $ExePath
$installSourceHash = $null
if ($SkipBuild) {
    if (-not (Test-Path -LiteralPath $ExePath)) {
        Die "SYNAPSE_SKIP_BUILD_BINARY_MISSING path=$ExePath remediation=-SkipBuild requires a real local synapse-mcp.exe at -ExePath before setup can touch the live daemon"
    }
    $installSourceHash = Get-SynapseFileSha256 -Path $ExePath
    Info "SkipBuild candidate binary path=$ExePath sha256=$installSourceHash"
} else {
    $stagedBinary = New-SynapseStagedDaemonBinary -BuiltPath $built -LogDir $LogDir
    $installSourcePath = $stagedBinary.Path
    $installSourceHash = $stagedBinary.Sha256
}
$candidatePreflight = Test-SynapseCandidateDaemon -CandidateExePath $installSourcePath -ProfilesDir $candidateProfilesDir -TokenPath $TokenPath -LogDir $LogDir
if ($candidatePreflight.Sha256 -ne $installSourceHash) {
    Die "SYNAPSE_CANDIDATE_HASH_MISMATCH expected_sha256=$installSourceHash actual_sha256=$($candidatePreflight.Sha256) path=$installSourcePath remediation=candidate preflight observed different bytes; refusing handoff"
}
Info "Candidate daemon accepted for handoff sha256=$installSourceHash tool_count=$($candidatePreflight.ToolCount) tool_surface_sha256=$($candidatePreflight.ToolSurfaceSha256)"

$codexAncestorBeforeHandoff = Get-SynapseCurrentCodexAncestor
if ($codexAncestorBeforeHandoff -and $processTokenAtStart -ne $token) {
    Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_ENV_STALE_PRE_HANDOFF_NONFATAL codex_pid={0} token_at_process_start={1} token_file={2} remediation=setup will keep the replacement handoff path available; after handoff call real mcp__synapse.health from this same Codex session before assuming reconnect failed. The patched launcher has been updated for future clients, and direct HTTP/token probes remain diagnostics only." -f `
        $codexAncestorBeforeHandoff.ProcessId,
        ($(if ([string]::IsNullOrWhiteSpace($processTokenAtStart)) { 'missing' } else { 'mismatch' })),
        $TokenPath)
}
Assert-CodexCandidateHandoffPreservesCurrentProcess `
    -CodexAncestor $codexAncestorBeforeHandoff `
    -CandidateSurface $candidatePreflight.ToolSurface `
    -ProcessHashAtStart $processToolSurfaceHashAtStart `
    -ProcessSnapshotAtStart $processToolSurfaceSnapshotAtStart `
    -SourceDir $SourceDir `
    -Bind $Bind `
    -TokenPath $TokenPath `
    -ActiveIssue $ActiveIssue

Step "Preflighting Chrome direct localhost bridge before daemon handoff"
$chromeBridgeInstaller = Join-Path $PSScriptRoot 'install-synapse-chrome-debugger.ps1'
$chromeBridgePreflight = Invoke-SynapseChromeBridgeVerifier `
    -InstallerPath $chromeBridgeInstaller `
    -NativeHostExePath $ChromeNativeHostExePath
Info ("Chrome direct bridge verifier preflight completed transport={0} extension_id={1} native_host_registry_present={2} native_host_manifest_present={3} policy_cleanup={4} popup_shield={5} {6}" -f `
    $chromeBridgePreflight.daemon_bridge_transport, `
    $chromeBridgePreflight.extension_id, `
    $chromeBridgePreflight.native_host_registry_present, `
    $chromeBridgePreflight.native_host_manifest_present, `
    (($chromeBridgePreflight.chrome_policy_cleanup | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
    (($chromeBridgePreflight.chrome_policy_popup_shield | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
    (Format-SynapseChromeBridgeProfileInstallState -Readback $chromeBridgePreflight))

# ---------------------------------------------------------------------------
# 5. Drain the running daemon, then install the proven binary
# ---------------------------------------------------------------------------
Step "Draining live daemon and installing verified binary -> $ExePath"
Assert-SynapseRestartAllowed -Reason 'install_binary' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -ForceRestart:$ForceRestart -AllowActiveClientDrain
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
}
Stop-SynapseMcpProcesses -Reason 'install_binary' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -ForceRestart:$ForceRestart
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ExePath) | Out-Null
$backupPath = $null
$oldInstalledHash = $null
if (Test-Path -LiteralPath $ExePath) {
    $oldInstalledHash = Get-SynapseFileSha256 -Path $ExePath
    $backupPath = "$ExePath.bak"
    Copy-Item -LiteralPath $ExePath -Destination $backupPath -Force
    $backupHash = Get-SynapseFileSha256 -Path $backupPath
    if ($backupHash -ne $oldInstalledHash) {
        Die "SYNAPSE_BINARY_BACKUP_HASH_MISMATCH installed=$ExePath backup=$backupPath installed_hash=$oldInstalledHash backup_hash=$backupHash remediation=backup bytes changed during copy; refusing to install candidate"
    }
    Info "Backed up old binary -> $backupPath sha256=$backupHash"
}
if (-not $SkipBuild) {
    Copy-Item -LiteralPath $installSourcePath -Destination $ExePath -Force
} else {
    Info "SkipBuild candidate already resides at install path=$ExePath"
}
if (-not (Test-Path -LiteralPath $ExePath)) {
    Die "SYNAPSE_INSTALL_BINARY_MISSING path=$ExePath remediation=setup could not find the installed daemon binary after the copy step"
}
$installedHash = Get-SynapseFileSha256 -Path $ExePath
if ($installedHash -ne $installSourceHash) {
    Die "SYNAPSE_INSTALLED_BINARY_HASH_MISMATCH path=$ExePath expected_sha256=$installSourceHash actual_sha256=$installedHash remediation=installed daemon bytes do not match the candidate that passed health preflight"
}
$ver = (& $ExePath --version) 2>&1
Info "Installed binary reports: $ver"
Info "Installed binary verified path=$ExePath sha256=$installedHash previous_sha256=$oldInstalledHash"

Step "Verifying Chrome direct localhost bridge"
$chromeBridgeReadback = Invoke-SynapseChromeBridgeVerifier `
    -InstallerPath $chromeBridgeInstaller `
    -NativeHostExePath $ChromeNativeHostExePath
Info ("Chrome direct bridge verifier completed transport={0} extension_id={1} native_host_registry_present={2} native_host_manifest_present={3} policy_cleanup={4} popup_shield={5} {6}" -f `
    $chromeBridgeReadback.daemon_bridge_transport, `
    $chromeBridgeReadback.extension_id, `
    $chromeBridgeReadback.native_host_registry_present, `
    $chromeBridgeReadback.native_host_manifest_present, `
    (($chromeBridgeReadback.chrome_policy_cleanup | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
    (($chromeBridgeReadback.chrome_policy_popup_shield | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
    (Format-SynapseChromeBridgeProfileInstallState -Readback $chromeBridgeReadback))

# ---------------------------------------------------------------------------
# 6. Deploy bundled profiles next to the exe (executable-relative lookup) +
#    keep an explicit --profile-dir for belt-and-suspenders.
# ---------------------------------------------------------------------------
Step "Deploying bundled profiles -> $ProfilesDir"
if ($srcProfiles -and (Test-Path $srcProfiles)) {
    New-Item -ItemType Directory -Force -Path $ProfilesDir | Out-Null
    Copy-Item "$srcProfiles\*" $ProfilesDir -Recurse -Force
    $n = (Get-ChildItem $ProfilesDir -Filter *.toml -File).Count
    if ($n -lt 1) { Die "SYNAPSE_PROFILES_DEPLOYED_EMPTY path=$ProfilesDir source=$srcProfiles remediation=copied profiles but found 0 .toml files in the deployed profile directory" }
    Info "Deployed $n profiles."
} elseif (-not (Test-Path $ProfilesDir)) {
    Die "SYNAPSE_PROFILES_MISSING source=$srcProfiles deployed=$ProfilesDir remediation=profile-dependent tools need bundled or deployed profiles before daemon start"
} else { Info "Reusing existing profiles at $ProfilesDir." }

# ---------------------------------------------------------------------------
# 7. Register + start the auto-start HTTP daemon (interactive desktop session)
# ---------------------------------------------------------------------------
Step "Registering auto-start daemon task '$TaskName'"
Wait-SynapseBindReleased -Reason 'pre_start' -Bind $Bind -TimeoutSeconds 1
$legacyLauncher = Join-Path $LogDir 'synapse-daemon-launch.cmd'
$hiddenLauncher = Join-Path $LogDir 'synapse-daemon-launch-hidden.vbs'
$launcherLog = Join-Path $LogDir 'daemon-launcher.log'
if (Test-Path $legacyLauncher) {
    Remove-Item -LiteralPath $legacyLauncher -Force
}
$wscriptExe = Join-Path $env:SystemRoot 'System32\wscript.exe'
if (-not (Test-Path $wscriptExe)) {
    Die "SYNAPSE_HIDDEN_LAUNCHER_MISSING path=$wscriptExe remediation=repair Windows Script Host or run the daemon manually with a hidden process supervisor"
}
New-HiddenDaemonLauncher -OutputPath $hiddenLauncher -ExePath $ExePath -Bind $Bind -DbPath $DbPath -ProfilesDir $ProfilesDir -LogDir $LogDir -TokenPath $TokenPath

$action  = New-ScheduledTaskAction -Execute $wscriptExe -Argument "//B //Nologo `"$hiddenLauncher`"" -WorkingDirectory $LogDir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$princ   = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
$set     = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
            -StartWhenAvailable -MultipleInstances IgnoreNew -RestartCount 3 `
            -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
$set.Hidden = $true
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
}
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $princ `
    -Settings $set -Description "Synapse MCP HTTP daemon (loopback) - the single body controlling Windows + WSL programs." | Out-Null
Start-ScheduledTask -TaskName $TaskName
Info "Task registered and started."

# ---------------------------------------------------------------------------
# 8. Health verify (source of truth: the live daemon)
# ---------------------------------------------------------------------------
Step "Verifying daemon health (http://$Bind/health)"
$ok = $false
$healthPid = $null
for ($i=0; $i -lt 15; $i++) {
    Start-Sleep -Seconds 2
    try {
        $h = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $token" } -TimeoutSec 4
        if ($h.ok) {
            Info ("Daemon OK: pid={0} version={1} db={2}" -f $h.pid, $h.version, $h.subsystems.storage.db_path)
            $healthPid = [int]$h.pid
            $ok = $true; break
        }
    } catch { }
}
if (-not $ok) {
    $failureListeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
    $failureProcesses = @(Get-SynapseMcpProcessSnapshot)
    $failureDetail = ("SYNAPSE_INSTALL_HEALTH_FAILED bind={0} candidate_sha256={1} installed_sha256={2} backup={3}`nlisteners:`n{4}`nprocesses:`n{5}`nremediation=inspect {6} and synapse.log.* under {7} for launch / STORAGE_* / bind errors" -f `
        $Bind,
        $installSourceHash,
        $installedHash,
        ($(if ($backupPath) { $backupPath } else { '<none>' })),
        (Format-SynapseTcpBindListenerSnapshot -Snapshot $failureListeners),
        (Format-SynapseMcpProcessSnapshot -Snapshot $failureProcesses),
        $launcherLog,
        $LogDir)

    if ($backupPath -and (Test-Path -LiteralPath $backupPath) -and $oldInstalledHash) {
        Info "WARN: $failureDetail"
        Info "Attempting rollback to previous daemon binary backup=$backupPath sha256=$oldInstalledHash"
        if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        }
        Stop-SynapseMcpProcesses -Reason 'install_health_failed_rollback' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -ForceRestart -TimeoutSeconds 10
        Copy-Item -LiteralPath $backupPath -Destination $ExePath -Force
        $rollbackHash = Get-SynapseFileSha256 -Path $ExePath
        if ($rollbackHash -ne $oldInstalledHash) {
            Die "SYNAPSE_INSTALL_HEALTH_FAILED_ROLLBACK_HASH_MISMATCH expected_sha256=$oldInstalledHash actual_sha256=$rollbackHash backup=$backupPath install_path=$ExePath original_failure=[$failureDetail]"
        }
        Start-ScheduledTask -TaskName $TaskName
        $rollbackOk = $false
        $rollbackHealth = $null
        for ($j=0; $j -lt 15; $j++) {
            Start-Sleep -Seconds 2
            try {
                $rh = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $token" } -TimeoutSec 4
                if ($rh.ok) {
                    $rollbackHealth = $rh
                    $rollbackOk = $true
                    break
                }
            } catch { }
        }
        if ($rollbackOk) {
            Die ("SYNAPSE_INSTALL_HEALTH_FAILED_ROLLED_BACK candidate_sha256={0} rollback_sha256={1} rollback_pid={2} original_failure=[{3}] remediation=old daemon is serving again; inspect candidate startup logs before retrying" -f `
                $installSourceHash,
                $rollbackHash,
                $rollbackHealth.pid,
                $failureDetail)
        }

        $rollbackListeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $rollbackProcesses = @(Get-SynapseMcpProcessSnapshot)
        Die ("SYNAPSE_INSTALL_HEALTH_FAILED_ROLLBACK_FAILED candidate_sha256={0} rollback_sha256={1} original_failure=[{2}]`nrollback_listeners:`n{3}`nrollback_processes:`n{4}`nremediation=rollback binary was restored but daemon did not become healthy; inspect {5} and synapse.log.* under {6}" -f `
            $installSourceHash,
            $rollbackHash,
            $failureDetail,
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $rollbackListeners),
            (Format-SynapseMcpProcessSnapshot -Snapshot $rollbackProcesses),
            $launcherLog,
            $LogDir)
    }

    Die $failureDetail
}
$h = Assert-SynapseChromeBridgeLiveAfterSetup -Bind $Bind -Token $token -Health $h
$healthPid = [int]$h.pid
$daemonLineage = Get-ProcessLineage -StartPid $healthPid
$cmdAncestor = $daemonLineage | Where-Object { $_.Name -ieq 'cmd.exe' } | Select-Object -First 1
if ($cmdAncestor) {
    $lineageText = ($daemonLineage | ForEach-Object { "{0}:{1}" -f $_.ProcessId, $_.Name }) -join ' <- '
    Die "SYNAPSE_DAEMON_CMD_ANCESTOR_FORBIDDEN pid=$healthPid cmd_pid=$($cmdAncestor.ProcessId) lineage=$lineageText remediation=rerun setup after removing legacy daemon launchers; daemon must not be launched through cmd.exe."
}

$toolSurface = Read-SynapseDaemonToolSurface -Bind $Bind -Token $token -Health $h
Write-SynapseCodexToolSurfaceSnapshot -Path $CodexToolSurfaceSnapshotPath -Surface $toolSurface

# ---------------------------------------------------------------------------
# 9. Wire the Windows-side MCP clients
# ---------------------------------------------------------------------------
if (-not $SkipClientWiring) {
    Step "Wiring Windows-side MCP clients"

    # Claude Code (Windows) speaks Streamable HTTP natively -> point at the daemon.
    $claude = Get-Command claude -ErrorAction SilentlyContinue
    if ($claude) {
        try {
            & $claude.Source mcp remove synapse -s user 2>$null | Out-Null
            & $claude.Source mcp add --scope user --transport http synapse "http://$Bind/mcp" --header "Authorization: Bearer $token"
            Info "Claude Code (Windows) wired via HTTP transport."
        } catch { Info "WARN: 'claude mcp add' failed: $($_.Exception.Message). Wire it manually (transport http -> http://$Bind/mcp)." }
    } else { Info "claude CLI not found on Windows PATH; skipping Claude Code wiring." }

    # Codex speaks Streamable HTTP; Claude Desktop remains stdio-only -> connect bridge.
    $bridgeArgs = @('--mode','connect','--bind',$Bind)

    $codex = Get-Command codex -ErrorAction SilentlyContinue
    $codexCfg = "$env:USERPROFILE\.codex\config.toml"
    if ($codex) {
        if (Test-CodexSynapseHttpConfig -ConfigPath $codexCfg -Bind $Bind) {
            Info "Codex MCP entry already uses the required Streamable HTTP transport."
        } else {
            & $codex.Source mcp remove synapse 2>$null | Out-Null
            & $codex.Source mcp add synapse --url "http://$Bind/mcp" --bearer-token-env-var SYNAPSE_BEARER_TOKEN
            $codexAddExit = $LASTEXITCODE
            if ($codexAddExit -ne 0 -and -not (Test-CodexSynapseHttpConfig -ConfigPath $codexCfg -Bind $Bind)) {
                Die "codex mcp add failed (exit $codexAddExit). Codex must be wired to HTTP, not the connect bridge."
            }
            if (-not (Test-CodexSynapseHttpConfig -ConfigPath $codexCfg -Bind $Bind)) {
                Die "codex mcp add completed but Codex config is not the required HTTP transport."
            }
            if ($codexAddExit -ne 0) {
                Info "WARN: codex mcp add exited $codexAddExit but Codex config now contains the required HTTP entry; continuing."
            }
        }
        Install-CodexSynapseTokenLoader -CodexCommandPath $codex.Source -TokenPath $TokenPath
        Info "Codex (Windows) wired via Streamable HTTP transport."
    } elseif (Test-Path $codexCfg) {
        $c = Get-Content -Raw $codexCfg
        if ($c -match '(?m)^\[mcp_servers\.synapse\]' -and
            -not (Test-CodexSynapseHttpConfig -ConfigPath $codexCfg -Bind $Bind)) {
            Die "Codex config exists at $codexCfg but codex CLI is not on PATH and the synapse entry is not the required HTTP transport. Install/repair Codex CLI, then re-run."
        }
        Info "Codex CLI not found; existing Codex config is already HTTP or has no synapse entry."
    } else { Info "codex CLI/config not found; skipping Codex wiring." }

    $desktopCfg = "$env:APPDATA\Claude\claude_desktop_config.json"
    if (Test-Path $desktopCfg) {
        try {
            $j = Get-Content -Raw $desktopCfg | ConvertFrom-Json
            if (-not $j.mcpServers) { $j | Add-Member -NotePropertyName mcpServers -NotePropertyValue (@{}) -Force }
            $desktopEntry = @{ command = $ExePath; args = $bridgeArgs; env = @{ SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY = '1' } }
            # $j.mcpServers is a hashtable when freshly created above, but a PSCustomObject when
            # parsed from an existing config. Dot-assigning a NEW property to a PSCustomObject throws
            # "The property 'synapse' cannot be found on this object" under Windows PowerShell 5.1,
            # so branch on type: index-assign dictionaries, Add-Member -Force PSCustomObjects (the
            # latter both adds-or-overwrites and works on PS 5.1 and 7+).
            if ($j.mcpServers -is [System.Collections.IDictionary]) {
                $j.mcpServers['synapse'] = $desktopEntry
            } else {
                $j.mcpServers | Add-Member -NotePropertyName synapse -NotePropertyValue $desktopEntry -Force
            }
            ($j | ConvertTo-Json -Depth 12) | Set-Content $desktopCfg -Encoding utf8
            Info "Claude Desktop wired -> connect bridge."
        } catch { Info "WARN: could not update $desktopCfg : $($_.Exception.Message)" }
    } else { Info "No Claude Desktop config at $desktopCfg; skipping." }
}

if (-not $SkipClientWiring) {
    $codexAncestor = Get-SynapseCurrentCodexAncestor
    if ($codexAncestor -and $processTokenAtStart -ne $token) {
        Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_ENV_STALE_NONFATAL codex_pid={0} token_at_process_start={1} token_file={2} remediation=do not assume the current Codex process is disconnected; first call real mcp__synapse.health from this same session and verify daemon PID/tool_surface readback. The patched launcher has been updated for future clients; if this already-running process has no authenticated MCP connection, use the existing live daemon and token file as diagnostics until the client can refresh its environment." -f $codexAncestor.ProcessId, ($(if ([string]::IsNullOrWhiteSpace($processTokenAtStart)) { 'missing' } else { 'mismatch' })), $TokenPath)
    }
    Assert-CodexCurrentProcessToolSurfaceFresh `
        -CodexAncestor $codexAncestor `
        -CurrentSurface $toolSurface `
        -ProcessHashAtStart $processToolSurfaceHashAtStart `
        -ProcessSnapshotAtStart $processToolSurfaceSnapshotAtStart `
        -SnapshotPath $CodexToolSurfaceSnapshotPath `
        -SourceDir $SourceDir `
        -Bind $Bind `
        -TokenPath $TokenPath `
        -ActiveIssue $ActiveIssue
} else {
    Info "Skipped Codex current-process freshness check because -SkipClientWiring was set; daemon health and tools/list were still verified."
}

Step "Done"
Info "Synapse daemon is live on http://$Bind (MCP: http://$Bind/mcp)."
Info "Token: $TokenPath   DB: $DbPath   Profiles: $ProfilesDir"
Info "WSL clients: run scripts/synapse-install.sh from WSL to wire Claude Code + Codex there."
Release-SynapseSetupMaintenanceLock -State released
