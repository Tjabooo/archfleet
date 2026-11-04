# Runs in your signed-in Windows desktop session (install.ps1 registers a logon task), never as an SSH service.
# Clipboard traffic, including Windows history, travels through a same-user named pipe.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$taskSid = [Security.Principal.WindowsIdentity]::GetCurrent().User
$taskPipeName = 'ArchfleetClipboard-' + $taskSid.Value
$taskMutex = [Threading.Mutex]::new($false, 'Local\' + $taskPipeName)
if (-not $taskMutex.WaitOne(0)) { exit 0 }
$taskLimit = 8 * 1024 * 1024
$taskUtf8 = [Text.UTF8Encoding]::new($false)

function Initialize-ClipboardHistory {
    if ($script:taskAwaitMethod) { return }
    Add-Type -AssemblyName System.Runtime.WindowsRuntime
    $null = [Windows.ApplicationModel.DataTransfer.Clipboard,Windows.ApplicationModel.DataTransfer,ContentType=WindowsRuntime]
    $null = [Windows.Storage.Streams.IRandomAccessStreamWithContentType,Windows.Storage.Streams,ContentType=WindowsRuntime]
    $script:taskAwaitMethod = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
        $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and $_.GetGenericArguments().Count -eq 1 -and
        $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
    } | Select-Object -First 1
}

function Wait-ClipboardOperation($Operation, [Type]$ResultType) {
    $taskPending = $script:taskAwaitMethod.MakeGenericMethod($ResultType).Invoke($null, @($Operation))
    if (-not $taskPending.Wait(5000)) {
        $Operation.Cancel()
        throw 'Windows clipboard history did not respond in time.'
    }
    return $taskPending.GetAwaiter().GetResult()
}

function Read-HistoryItems {
    Initialize-ClipboardHistory
    $taskResult = Wait-ClipboardOperation ([Windows.ApplicationModel.DataTransfer.Clipboard]::GetHistoryItemsAsync()) ([Windows.ApplicationModel.DataTransfer.ClipboardHistoryItemsResult,Windows.ApplicationModel.DataTransfer,ContentType=WindowsRuntime])
    if ([string]$taskResult.Status -ne 'Success') { throw ([string]$taskResult.Status) }
    return @($taskResult.Items | Sort-Object Timestamp -Descending)
}

function Get-DesktopHistory {
    try {
        Initialize-ClipboardHistory
        if (-not [Windows.ApplicationModel.DataTransfer.Clipboard]::IsHistoryEnabled()) {
            return @{ status='disabled'; entries=@() }
        }
        $taskItems = @(Read-HistoryItems)
        $taskEntries = @()
        foreach ($taskItem in $taskItems) {
            if ($taskEntries.Count -ge 25) { break }  # Windows itself keeps at most 25
            try {
                $taskContent = $taskItem.Content
                if ($taskContent.Contains('Bitmap')) {
                    $taskEntries += @{ id=$taskItem.Id; preview='Windows image'; kind='image' }
                } elseif ($taskContent.Contains('Text')) {
                    $taskText = Wait-ClipboardOperation ($taskContent.GetTextAsync()) ([string])
                    if ($taskUtf8.GetByteCount($taskText) -gt $taskLimit) { continue }
                    $taskEntries += @{ id=$taskItem.Id; preview=$taskText.Substring(0, [Math]::Min(600, $taskText.Length)); kind='text' }
                }
            } catch {
                # A removed or unreadable item must not hide the other entries or leak its contents.
            }
        }
        return @{ status='ok'; entries=@($taskEntries) }
    } catch {
        return @{ status='unavailable'; entries=@() }
    }
}

function Get-DesktopHistoryEntry($Id) {
    if ($Id -isnot [string] -or $Id.Length -lt 1 -or $Id.Length -gt 256 -or $Id -match '[\x00-\x1f\x7f]') {
        throw 'Invalid Windows clipboard history entry.'
    }
    $taskItem = @(Read-HistoryItems) | Where-Object { $_.Id -ceq $Id } | Select-Object -First 1
    if (-not $taskItem) { throw 'That Windows clipboard history entry is gone. Reopen SUPER+V to refresh.' }
    $taskContent = $taskItem.Content
    if ($taskContent.Contains('Bitmap')) {
        $taskReference = Wait-ClipboardOperation ($taskContent.GetBitmapAsync()) ([Windows.Storage.Streams.RandomAccessStreamReference,Windows.Storage.Streams,ContentType=WindowsRuntime])
        $taskRandom = Wait-ClipboardOperation ($taskReference.OpenReadAsync()) ([Windows.Storage.Streams.IRandomAccessStreamWithContentType,Windows.Storage.Streams,ContentType=WindowsRuntime])
        $taskInput = $null
        $taskImage = $null
        $taskOutput = [IO.MemoryStream]::new()
        try {
            # The interface-only WinRT result is a COM projection in Windows PowerShell.
            # Invoke the typed adapter explicitly so PowerShell's overload binder does not
            # mistake it for an untyped __ComObject.
            $taskAdapter = [IO.WindowsRuntimeStreamExtensions].GetMethod('AsStream', [Type[]]@([Windows.Storage.Streams.IRandomAccessStream,Windows.Storage.Streams,ContentType=WindowsRuntime]))
            $taskInput = $taskAdapter.Invoke($null, @($taskRandom))
            if ($taskInput.Length -gt $taskLimit) { throw 'Clipboard exceeds the 8 MiB transfer limit.' }
            $taskImage = [Drawing.Image]::FromStream($taskInput)
            if ([long]$taskImage.Width * $taskImage.Height -gt 64000000) { throw 'Clipboard image dimensions exceed the supported limit.' }
            $taskImage.Save($taskOutput, [Drawing.Imaging.ImageFormat]::Png)
            $taskBytes = $taskOutput.ToArray()
            if ($taskBytes.Length -gt $taskLimit) { throw 'Clipboard exceeds the 8 MiB transfer limit.' }
            return @{ kind='image'; mime='image/png'; data=[Convert]::ToBase64String($taskBytes); bytes=$taskBytes.Length; width=$taskImage.Width; height=$taskImage.Height }
        } finally {
            if ($taskImage) { $taskImage.Dispose() }
            if ($taskInput) { $taskInput.Dispose() }
            # AsStreamForRead owns and closes the WinRT stream. Calling Dispose directly on
            # PowerShell's COM projection can fail and hide an otherwise valid image result.
            if (-not $taskInput) { try { ([IDisposable]$taskRandom).Dispose() } catch {} }
            $taskOutput.Dispose()
        }
    }
    if ($taskContent.Contains('Text')) {
        $taskText = Wait-ClipboardOperation ($taskContent.GetTextAsync()) ([string])
        $taskSize = $taskUtf8.GetByteCount($taskText)
        if ($taskSize -gt $taskLimit) { throw 'Clipboard exceeds the 8 MiB transfer limit.' }
        return @{ kind='text'; mime='text/plain;charset=utf-8'; text=$taskText; bytes=$taskSize }
    }
    throw 'This Windows history entry is not plain text or an image.'
}

function Get-DesktopClipboard {
    # Browsers and most apps also offer the exact PNG; the bitmap fallback below loses transparency.
    $taskObject = [Windows.Forms.Clipboard]::GetDataObject()
    if ($taskObject -and $taskObject.GetDataPresent('PNG')) {
        $taskPng = $taskObject.GetData('PNG')
        if ($taskPng -is [IO.Stream]) {
            $taskStream = [IO.MemoryStream]::new()
            try {
                $taskPng.CopyTo($taskStream)
                $taskBytes = $taskStream.ToArray()
            } finally { $taskStream.Dispose(); $taskPng.Dispose() }
            if ($taskBytes.Length -gt $taskLimit) { throw 'Clipboard exceeds the 8 MiB transfer limit.' }
            if ($taskBytes.Length -ge 24 -and [BitConverter]::ToString($taskBytes, 0, 8) -eq '89-50-4E-47-0D-0A-1A-0A') {
                $taskWidth = [BitConverter]::ToUInt32([byte[]]($taskBytes[19..16]), 0)
                $taskHeight = [BitConverter]::ToUInt32([byte[]]($taskBytes[23..20]), 0)
                return @{ kind='image'; mime='image/png'; data=[Convert]::ToBase64String($taskBytes); bytes=$taskBytes.Length; width=$taskWidth; height=$taskHeight }
            }
        }
    }
    if ([Windows.Forms.Clipboard]::ContainsImage()) {
        $taskImage = [Windows.Forms.Clipboard]::GetImage()
        $taskStream = [IO.MemoryStream]::new()
        try {
            if ([long]$taskImage.Width * $taskImage.Height -gt 64000000) { throw 'Clipboard image dimensions exceed the supported limit.' }
            $taskImage.Save($taskStream, [Drawing.Imaging.ImageFormat]::Png)
            $taskBytes = $taskStream.ToArray()
            if ($taskBytes.Length -gt $taskLimit) { throw 'Clipboard exceeds the 8 MiB transfer limit.' }
            return @{ kind='image'; mime='image/png'; data=[Convert]::ToBase64String($taskBytes); bytes=$taskBytes.Length; width=$taskImage.Width; height=$taskImage.Height }
        } finally { $taskStream.Dispose(); $taskImage.Dispose() }
    }
    if ([Windows.Forms.Clipboard]::ContainsText()) {
        $taskText = [Windows.Forms.Clipboard]::GetText([Windows.Forms.TextDataFormat]::UnicodeText)
        $taskSize = $taskUtf8.GetByteCount($taskText)
        if ($taskSize -gt $taskLimit) { throw 'Clipboard exceeds the 8 MiB transfer limit.' }
        return @{ kind='text'; mime='text/plain;charset=utf-8'; text=$taskText; bytes=$taskSize }
    }
    $taskObject = [Windows.Forms.Clipboard]::GetDataObject()
    $taskFormats = if ($taskObject) { @($taskObject.GetFormats()) } else { @() }
    if ($taskFormats.Count) { return @{ kind='unsupported'; types=$taskFormats; bytes=0 } }
    return @{ kind='empty'; mime=''; bytes=0 }
}

function Set-DesktopClipboard($Value) {
    switch ($Value.kind) {
        'text' {
            if ($null -eq $Value.text -or $Value.text -isnot [string]) { throw 'Clipboard text must be a string.' }
            $taskSize = $taskUtf8.GetByteCount($Value.text)
            if ($taskSize -gt $taskLimit) { throw 'Clipboard exceeds the 8 MiB transfer limit.' }
            if ($Value.text.Length -eq 0) { [Windows.Forms.Clipboard]::Clear() }
            else { [Windows.Forms.Clipboard]::SetText($Value.text, [Windows.Forms.TextDataFormat]::UnicodeText) }
            return @{ written=$true; kind='text'; bytes=$taskSize }
        }
        'image' {
            if ($Value.data.Length -gt 11184812) { throw 'Clipboard image exceeds the 8 MiB transfer limit.' }
            $taskBytes = [Convert]::FromBase64String($Value.data)
            if ($taskBytes.Length -gt $taskLimit -or $taskBytes.Length -lt 24 -or [BitConverter]::ToString($taskBytes,0,8) -ne '89-50-4E-47-0D-0A-1A-0A') { throw 'Expected PNG clipboard data of at most 8 MiB.' }
            $taskStream = [IO.MemoryStream]::new($taskBytes, $false)
            $taskImage = $null
            try {
                $taskImage = [Drawing.Image]::FromStream($taskStream)
                if ([long]$taskImage.Width * $taskImage.Height -gt 64000000) { throw 'Clipboard image dimensions exceed the supported limit.' }
                # The exact PNG for apps that take it (keeps transparency), plus a bitmap for those that don't.
                $taskData = [Windows.Forms.DataObject]::new()
                $taskData.SetData('PNG', $false, [IO.MemoryStream]::new($taskBytes, $false))
                $taskData.SetImage($taskImage)
                [Windows.Forms.Clipboard]::SetDataObject($taskData, $true, 5, 100)
            } finally { if ($taskImage) { $taskImage.Dispose() }; $taskStream.Dispose() }
            return @{ written=$true; kind='image'; bytes=$taskBytes.Length }
        }
        'empty' { [Windows.Forms.Clipboard]::Clear(); return @{ written=$true; kind='empty'; bytes=0 } }
        default { throw 'Supported clipboard formats are plain text and PNG.' }
    }
}

try {
    while ($true) {
        $taskSecurity = [IO.Pipes.PipeSecurity]::new()
        $taskSecurity.SetAccessRuleProtection($true, $false)
        $taskSecurity.SetOwner($taskSid)
        $taskSecurity.AddAccessRule([IO.Pipes.PipeAccessRule]::new($taskSid, [IO.Pipes.PipeAccessRights]::FullControl, [Security.AccessControl.AccessControlType]::Allow))
        $taskPipe = [IO.Pipes.NamedPipeServerStream]::new($taskPipeName, [IO.Pipes.PipeDirection]::InOut, 1, [IO.Pipes.PipeTransmissionMode]::Byte, [IO.Pipes.PipeOptions]::Asynchronous, 16384, 16384, $taskSecurity)
        try {
            $taskPipe.WaitForConnection()
            $taskReader = [IO.StreamReader]::new($taskPipe, $taskUtf8, $false, 16384, $true)
            $taskWriter = [IO.StreamWriter]::new($taskPipe, $taskUtf8, 16384, $true)
            $taskWriter.AutoFlush = $true
            $taskRead = $taskReader.ReadLineAsync()
            if (-not $taskRead.Wait(6000)) { continue }
            try {
                if ($taskRead.Result.Length -gt 12000000) { throw 'Clipboard request is too large.' }
                $taskRequest = $taskRead.Result | ConvertFrom-Json
                $taskData = switch ($taskRequest.action) {
                    'clipboard-get' { Get-DesktopClipboard }
                    'clipboard-set' { Set-DesktopClipboard $taskRequest.value }
                    'clipboard-history' { Get-DesktopHistory }
                    'clipboard-entry' { Get-DesktopHistoryEntry $taskRequest.value }
                    'ping' { @{ ready=$true; session=(Get-Process -Id $PID).SessionId } }
                    default { throw 'Unsupported clipboard request.' }
                }
                $taskResponse = @{ ok=$true; data=$taskData }
            } catch { $taskResponse = @{ ok=$false; error=$_.Exception.Message } }
            $taskWriter.WriteLine(($taskResponse | ConvertTo-Json -Depth 6 -Compress))
        } catch {
            # Disconnected/time-limited clients do not stop the desktop helper.
        } finally {
            if ($taskReader) { $taskReader.Dispose(); $taskReader = $null }
            if ($taskWriter) { $taskWriter.Dispose(); $taskWriter = $null }
            $taskPipe.Dispose()
        }
    }
} finally { $taskMutex.ReleaseMutex(); $taskMutex.Dispose() }
