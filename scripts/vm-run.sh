#!/usr/bin/env bash
# Run a program inside the logged-in desktop session (SSH sessions can't capture the screen or
# send input). A scheduled task set to "run only when user is logged on" does it, as the normal
# (non-elevated) user. Output goes to C:\dishus\run-<time>.log (printed); vm-logs.sh fetches it.
# usage: vm-run.sh 'C:\dishus\DiShus-0.1.4-windows-x86_64.exe' [args...]
# DISHUS_VM_USER=name runs it as another user, who must be the one logged in.
. "$(dirname "$0")/vm-common.sh"
[[ $# -ge 1 ]] || { echo "usage: $0 <windows path> [args...]" >&2; exit 1; }
exe=$1; shift
# The whole line goes to cmd /c, so cmd does the redirect.
# One log per run: a program still running (an installer waiting for Enter) keeps its log locked.
log="C:\\dishus\\run-$(date +%H%M%S).log"
line="\"$exe\" $* > $log 2>&1"
vm_ssh "\$a = New-ScheduledTaskAction -Execute cmd.exe -Argument '/c $line'
\$p = New-ScheduledTaskPrincipal -UserId ${DISHUS_VM_USER:-dishus} -LogonType Interactive -RunLevel Limited
# Parallel: by default a new start is ignored while an earlier run (say, an installer waiting
# for Enter) is still open.
\$s = New-ScheduledTaskSettingsSet -MultipleInstances Parallel
Register-ScheduledTask -TaskName dishus-run -Action \$a -Principal \$p -Settings \$s -Force | Out-Null
Start-ScheduledTask -TaskName dishus-run
Start-Sleep 2
(Get-ScheduledTask -TaskName dishus-run).State"
echo "$log"
