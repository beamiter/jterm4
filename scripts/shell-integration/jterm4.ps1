# jterm4 shell integration for Windows PowerShell 5+ and pwsh 7+.
# Source from $PROFILE, for example:
#   if ($env:TERM_PROGRAM -eq 'jterm4') { . /path/to/jterm4.ps1 }

if ($script:__jterm4_loaded) { return }
$script:__jterm4_loaded = $true
$script:__jterm4_in_cmd = $false
$script:__jterm4_orig_prompt = ${function:prompt}

function __jterm4_osc($payload) {
    "$([char]27)]${payload}$([char]7)"
}

function __jterm4_report_cwd_seq {
    $path = (Get-Location).ProviderPath
    $hostName = if ($env:COMPUTERNAME) {
        $env:COMPUTERNAME
    } else {
        [System.Net.Dns]::GetHostName()
    }
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($path)
    $sb = [System.Text.StringBuilder]::new()
    foreach ($b in $bytes) {
        $c = [char]$b
        if (($b -ge 0x41 -and $b -le 0x5a) -or
            ($b -ge 0x61 -and $b -le 0x7a) -or
            ($b -ge 0x30 -and $b -le 0x39) -or
            $c -eq '/' -or $c -eq '.' -or $c -eq '_' -or
            $c -eq '-' -or $c -eq '~') {
            [void]$sb.Append($c)
        } else {
            [void]$sb.AppendFormat('%{0:X2}', $b)
        }
    }
    __jterm4_osc "7;file://${hostName}$($sb.ToString())"
}

function global:prompt {
    $dollarQ = $?
    $lastEC = $LASTEXITCODE
    $ec = if ($dollarQ) { 0 } elseif ($lastEC) { $lastEC } else { 1 }

    $pre = ''
    if ($script:__jterm4_in_cmd) {
        $pre += __jterm4_osc "133;D;$ec"
        $script:__jterm4_in_cmd = $false
    }
    $pre += __jterm4_report_cwd_seq
    $pre += __jterm4_osc "133;A"
    $title = "$($env:USERNAME)@$([System.Net.Dns]::GetHostName()):$(Get-Location)"
    $pre += "$([char]27)]2;${title}$([char]7)"

    $orig = ''
    try {
        $orig = & $script:__jterm4_orig_prompt
    } catch {
        $orig = "PS $(Get-Location)> "
    }
    $post = __jterm4_osc "133;B"

    $global:LASTEXITCODE = $lastEC
    if (-not $dollarQ) {
        & { Write-Error -ErrorAction SilentlyContinue 'preserve-dollar-q' } 2>$null
    }
    return "${pre}${orig}${post}"
}

if (Get-Module -ListAvailable PSReadLine) {
    Set-PSReadLineKeyHandler -Chord Enter -ScriptBlock {
        param($key, $arg)
        $line = $null
        $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        if ($line -and $line.Trim().Length -gt 0) {
            [Console]::Write($(__jterm4_osc "133;C"))
            $script:__jterm4_in_cmd = $true
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine($key, $arg)
    }
    Set-PSReadLineKeyHandler -Chord NumPadEnter -ScriptBlock {
        param($key, $arg)
        $line = $null
        $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        if ($line -and $line.Trim().Length -gt 0) {
            [Console]::Write($(__jterm4_osc "133;C"))
            $script:__jterm4_in_cmd = $true
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine($key, $arg)
    }
}

$env:TERM_PROGRAM = 'jterm4'
