param(
    [Parameter(Mandatory)][ValidateSet('Configure', 'Reset', 'Close')][string]$Action,
    [int]$OwnedProcessId,
    [string]$BinaryPath
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Native smoke support is restricted to disposable GitHub-hosted runners'
}
$key = 'HKLM:\SOFTWARE\Policies\Microsoft\Edge\WebView2\AdditionalBrowserArguments'
switch ($Action) {
    'Configure' {
        if (Get-ItemProperty -LiteralPath $key -Name 'LRCGET.exe' -ErrorAction SilentlyContinue) {
            throw 'Existing smoke policy must not be overwritten'
        }
        New-Item -Path $key -Force | Out-Null
        New-ItemProperty -LiteralPath $key -Name 'LRCGET.exe' -Value '--remote-debugging-port=0' -PropertyType String | Out-Null
    }
    'Reset' {
        Remove-ItemProperty -LiteralPath $key -Name 'LRCGET.exe' -ErrorAction Stop
    }
    'Close' {
        $process = Get-Process -Id $OwnedProcessId -ErrorAction Stop
        if ($process.Path -ne $BinaryPath) { throw 'Smoke process identity does not match' }
        if (-not $process.CloseMainWindow()) { throw 'Cannot close smoke-test window' }
        $process.WaitForExit()
    }
}
