$ErrorActionPreference = 'Stop'
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$name = "IP-KVM OS Services $($identity.User.Value)"

# Stops the task and waits for its process to exit, so the installer can
# replace the executable.
function Stop-Task {
    if (-not (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue)) {
        return $false
    }
    Stop-ScheduledTask -TaskName $name
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-ScheduledTask -TaskName $name).State -eq 'Running' -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 200
    }
    return $true
}

if ($env:OS_SERVICES_INSTALL -eq '1') {
    Stop-Task | Out-Null
    if (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $name -Confirm:$false
    }
    $user = $identity.Name
    $action = New-ScheduledTaskAction -Execute $env:OS_SERVICES_EXECUTABLE -Argument '--run'
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
        -RestartCount 255 -RestartInterval (New-TimeSpan -Minutes 1) `
        -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings | Out-Null
    # A task that an elevated installer registers gives the user only read
    # access. Full access lets a later update or removal without elevation
    # change it.
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $scheduler.GetFolder('\').GetTask($name).SetSecurityDescriptor(
        "D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;$($identity.User.Value))", 0)
    Start-ScheduledTask -TaskName $name
} elseif (Stop-Task) {
    Unregister-ScheduledTask -TaskName $name -Confirm:$false
}
