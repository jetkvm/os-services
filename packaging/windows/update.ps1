$ErrorActionPreference = 'Stop'
# The service starts this script with a staged installer and exits. The
# installer cannot replace the program while it runs, so the script waits
# for the service to exit before it installs. The installer registers and
# starts the new service; starting the task afterwards also brings the old
# version back when the installer fails.
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$name = "IP-KVM OS Services $($identity.User.Value)"
try {
    Wait-Process -Id $env:OS_SERVICES_PID -Timeout 60 -ErrorAction SilentlyContinue
    $setup = Start-Process -FilePath $env:OS_SERVICES_SETUP -ArgumentList '/S' -Wait -PassThru
    if ($setup.ExitCode -ne 0) {
        throw "the installer exited with $($setup.ExitCode)"
    }
} catch {
    Add-Content -LiteralPath $env:OS_SERVICES_LOG -Value "ipkvm-os-services: update failed: $_"
} finally {
    Remove-Item -LiteralPath $env:OS_SERVICES_SETUP -ErrorAction SilentlyContinue
    Start-ScheduledTask -TaskName $name
}
