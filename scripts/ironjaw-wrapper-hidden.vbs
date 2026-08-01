' Ironjaw WebSocket Gateway — hidden launcher for Task Scheduler
' Runs ironjaw-wrapper.cmd without showing a console window.
Set WshShell = CreateObject("WScript.Shell")
WshShell.Run Chr(34) & CreateObject("Scripting.FileSystemObject").GetParentFolderName(WScript.ScriptFullName) & "\ironjaw-wrapper.cmd" & Chr(34), 0, False
Set WshShell = Nothing
