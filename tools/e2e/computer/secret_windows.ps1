# The clipboard-secret check's helper on Windows: puts the text on the
# clipboard with the format ExcludeClipboardContentFromMonitorProcessing,
# as password managers mark a password, appends "ready" to the window log,
# and runs until the task ends.
param([string]$Text, [string]$Log)
Add-Type -AssemblyName System.Windows.Forms
$data = New-Object System.Windows.Forms.DataObject
$data.SetText($Text)
$data.SetData('ExcludeClipboardContentFromMonitorProcessing', [System.IO.MemoryStream]::new([byte[]]@(0)))
[System.Windows.Forms.Clipboard]::SetDataObject($data, $true)
Add-Content -Path $Log -Value 'ready'
while ($true) { Start-Sleep -Seconds 60 }
