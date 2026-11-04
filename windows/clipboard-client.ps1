# SSH-safe client for the interactive desktop helper. JSON is read from stdin.
$ErrorActionPreference = 'Stop'
$taskUtf8 = [Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = $taskUtf8
[Console]::OutputEncoding = $taskUtf8
$taskPipeName = 'ArchfleetClipboard-' + [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$taskPipe = [IO.Pipes.NamedPipeClientStream]::new('.', $taskPipeName, [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous)
try {
    $taskInput = [Console]::In.ReadToEnd()
    if ($taskInput.Length -gt 12000000) { throw 'Clipboard request is too large.' }
    $taskRequest = $taskInput | ConvertFrom-Json
    try { $taskPipe.Connect(4000) } catch { throw 'Windows desktop clipboard helper is unavailable. Sign in on this PC (the Archfleet Clipboard Bridge task starts it at logon), or run windows\install.ps1 again.' }
    $taskWriter = [IO.StreamWriter]::new($taskPipe, $taskUtf8, 16384, $true)
    $taskWriter.AutoFlush = $true
    $taskReader = [IO.StreamReader]::new($taskPipe, $taskUtf8, $false, 16384, $true)
    $taskWriter.WriteLine(($taskRequest | ConvertTo-Json -Depth 6 -Compress))
    $taskRead = $taskReader.ReadLineAsync()
    if (-not $taskRead.Wait(10000)) { throw 'The Windows clipboard helper did not respond in time.' }
    [Console]::Out.WriteLine($taskRead.Result)
} catch {
    [Console]::Out.WriteLine((@{ ok=$false; error=$_.Exception.Message } | ConvertTo-Json -Compress))
    exit 1
} finally {
    if ($taskReader) { $taskReader.Dispose() }
    if ($taskWriter) { $taskWriter.Dispose() }
    $taskPipe.Dispose()
}
