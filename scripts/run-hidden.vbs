' Windowless runner for scheduled tasks:  wscript.exe //B //Nologo run-hidden.vbs <program> [args...]
'
' A scheduled task must never start a console program (powershell.exe, pwsh.exe, cmd.exe, ...)
' directly. Task Scheduler gives it a visible console (an OpenConsole window under Windows
' Terminal) before -WindowStyle Hidden can apply, so every run flashes a terminal and steals
' focus. wscript.exe is a GUI-subsystem host: no console exists until Run() creates the
' program with window style 0 (SW_HIDE).
'
' Run(..., True) waits, so MultipleInstances=IgnoreNew still sees the task as running, and its
' return value is the program's own exit code, which WScript.Quit hands to Task Scheduler
' unchanged (conhost.exe --headless, by contrast, recorded 0 even when the program failed).
'
' Arguments are re-quoted with the Windows command-line rules. WScript.Arguments cannot carry
' an embedded double quote, so a command that needs one gets its own .vbs launcher instead.
Option Explicit
Dim shell, cmd, i

Function QuoteArg(value)
  Dim n
  If Len(value) > 0 And InStr(value, " ") = 0 And InStr(value, vbTab) = 0 Then
    QuoteArg = value
    Exit Function
  End If
  ' Backslashes before the closing quote must be doubled or they would escape it.
  ' (VBScript's And does not short-circuit, so the bounds check is a separate test.)
  n = 0
  Do While n < Len(value)
    If Mid(value, Len(value) - n, 1) <> "\" Then Exit Do
    n = n + 1
  Loop
  QuoteArg = """" & value & String(n, "\") & """"
End Function

Dim exitCode
If WScript.Arguments.Count = 0 Then WScript.Quit 2
Set shell = CreateObject("WScript.Shell")
cmd = QuoteArg(WScript.Arguments(0))
For i = 1 To WScript.Arguments.Count - 1
  cmd = cmd & " " & QuoteArg(WScript.Arguments(i))
Next
' Under //B a runtime error ends the script with exit code 0, which would record a program
' that never started as a success. Catch it and fail with a distinct code instead.
On Error Resume Next
exitCode = shell.Run(cmd, 0, True)
If Err.Number <> 0 Then WScript.Quit 3
On Error GoTo 0
WScript.Quit exitCode
