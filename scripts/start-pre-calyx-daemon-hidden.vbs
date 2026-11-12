' Windowless entry point for the SynapseMcpPreCalyx scheduled task.
'
' Task Scheduler must not start powershell.exe directly: a console host is created
' (and, with Windows Terminal as the default terminal, an OpenConsole window is shown)
' before -WindowStyle Hidden takes effect, so every 1-minute watchdog run flashed a
' terminal on screen and stole focus.
'
' wscript.exe is a GUI-subsystem host, so no console exists until Run() creates
' powershell.exe with window style 0 (SW_HIDE). Run(..., True) waits for the launcher,
' which keeps MultipleInstances=IgnoreNew meaningful, and its return value is the
' launcher's own exit code, which WScript.Quit hands to Task Scheduler unchanged.
' (conhost.exe --headless, tried before, recorded 0 even when the launcher threw.)
Option Explicit
Dim shell, fso, launcher, powershell, exitCode
Set shell = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
launcher = fso.BuildPath(fso.GetParentFolderName(WScript.ScriptFullName), "start-pre-calyx-daemon.ps1")
powershell = shell.ExpandEnvironmentStrings("%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe")
If Not fso.FileExists(launcher) Then WScript.Quit 2
' Under //B a runtime error ends the script with exit code 0; fail with a distinct code instead.
On Error Resume Next
exitCode = shell.Run("""" & powershell & """ -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File """ & launcher & """", 0, True)
If Err.Number <> 0 Then WScript.Quit 3
On Error GoTo 0
WScript.Quit exitCode
