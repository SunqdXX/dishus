# First-logon setup for the DiShus test VM (run once by autounattend.xml, as the admin user).
# virtio drivers, OpenSSH Server with key login, no sleep/lock, then a reboot.
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\dishus-setup.log -Append

$cfg = Split-Path -Parent $MyInvocation.MyCommand.Path
$virtio = (Get-PSDrive -PSProvider FileSystem | Where-Object { Test-Path "$($_.Root)virtio-win-guest-tools.exe" } | Select-Object -First 1).Root

# virtio drivers (display, balloon, net, ...) and the QEMU guest agent.
if ($virtio) {
    $p = Start-Process -Wait -PassThru "$($virtio)virtio-win-guest-tools.exe" -ArgumentList '/install', '/quiet', '/norestart'
    "virtio guest tools: exit $($p.ExitCode)"
} else { 'virtio CD not found' }

# OpenSSH Server from the MSI on the config CD (Add-WindowsCapability needs Windows Update).
Start-Process -Wait msiexec.exe -ArgumentList '/i', "$cfg\OpenSSH-Win64.msi", '/qn', 'ADDLOCAL=Server'
Set-Service sshd -StartupType Automatic
Start-Service sshd   # first start creates C:\ProgramData\ssh
# Admin users read their keys from here, never from ~\.ssh. Only Administrators and SYSTEM may
# have access, or sshd ignores the file.
$keys = 'C:\ProgramData\ssh\administrators_authorized_keys'
Copy-Item "$cfg\authorized_keys" $keys -Force
icacls $keys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F'
New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -PropertyType String -Force `
    -Value 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' | Out-Null
Restart-Service sshd
# The VM's network may come up as Public: allow SSH on every profile.
New-NetFirewallRule -Name dishus-vm-ssh -DisplayName 'SSH (DiShus test VM)' -Direction Inbound `
    -Protocol TCP -LocalPort 22 -Action Allow -Profile Any | Out-Null
Get-NetConnectionProfile | Set-NetConnectionProfile -NetworkCategory Private

# Never sleep, blank or lock: the desktop session must stay alive for capture tests.
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /hibernate off
reg add HKLM\SOFTWARE\Policies\Microsoft\Windows\Personalization /v NoLockScreen /t REG_DWORD /d 1 /f
reg add 'HKCU\Control Panel\Desktop' /v ScreenSaveActive /t REG_SZ /d 0 /f
reg add HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System /v InactivityTimeoutSecs /t REG_DWORD /d 0 /f

New-Item -ItemType File -Force C:\dishus-setup.done | Out-Null
Stop-Transcript
shutdown /r /t 10
