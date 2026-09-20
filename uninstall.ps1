#Requires -Version 5.1
<#
os-watcher 卸载脚本（Windows）

用法（在「以管理员身份运行」的 PowerShell 中执行）：
    .\uninstall.ps1                      # 交互确认，备份 config.toml
    .\uninstall.ps1 -Yes                 # 跳过确认
    .\uninstall.ps1 -NoBackup            # 不备份，直接删除
    .\uninstall.ps1 -BackupDir D:\bak    # 指定备份目录
    .\uninstall.ps1 -KeepConfig          # 保留 config.toml，仅删除程序文件

也可以直接运行 uninstall.cmd（会自动提权，无需手动开管理员终端）。

参数：
    -ServiceName <NAME>   服务名，默认 os-watcher
    -BackupDir <DIR>      备份目录，默认 <安装目录同级>\os-watcher-backup-<时间戳>
    -NoBackup             不备份
    -KeepConfig           保留 config.toml
    -Yes                  跳过交互确认
    -RegisterOnly         内部使用：只做服务注销，不删除文件

说明：卸载会停止并注销服务、删除安装目录。正在运行的本脚本自身、以及被
占用的二进制，无法立即删除，会登记为「重启时删除」。
#>
[CmdletBinding()]
param(
    [string]$ServiceName = 'os-watcher',
    [string]$BackupDir = '',
    [switch]$NoBackup,
    [switch]$KeepConfig,
    [switch]$Yes,
    [switch]$RegisterOnly,
    # 内部使用：由 uninstall.cmd 传入，非管理员时经 UAC 重新拉起自身。
    [switch]$Elevate
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ScriptPath = $MyInvocation.MyCommand.Path
$ScriptDir = Split-Path -Parent $ScriptPath

# 刻意不 Set-Location 到安装目录：Windows 拒绝删除任何进程的当前工作目录，
# 而卸载的最后一步正是删除安装目录本身。脚本内所有路径都基于 $ScriptDir。

function Write-Info([string]$Message) { Write-Host "[INFO] $Message" -ForegroundColor Cyan }
function Write-Ok([string]$Message) { Write-Host "[ OK ] $Message" -ForegroundColor Green }
function Write-Warn([string]$Message) { Write-Host "[WARN] $Message" -ForegroundColor Yellow }
function Write-Err([string]$Message) { Write-Host "[FAIL] $Message" -ForegroundColor Red }
function Fail([string]$Message) { Write-Err $Message; exit 1 }

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        return
    }
    if (-not $Elevate) {
        Fail 'Windows 卸载需要管理员权限，请以管理员身份运行终端，或直接运行 uninstall.cmd'
    }

    # 逐项转成 -Name 值 / -Switch 形式。含空格的值（如 -BackupDir）必须加引号，
    # 否则提权后的进程会把一个参数拆成两个。
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"")
    foreach ($key in $PSBoundParameters.Keys) {
        if ($key -eq 'Elevate') { continue }
        $value = $PSBoundParameters[$key]
        if ($value -is [switch]) {
            if ($value.IsPresent) { $argList += "-$key" }
        } elseif ($null -ne $value -and "$value" -ne '') {
            $argList += "-$key"
            $argList += "`"$value`""
        }
    }

    Write-Info '正在请求管理员权限...'
    try {
        $proc = Start-Process -FilePath (Get-Process -Id $PID).Path `
            -ArgumentList $argList -Verb RunAs -Wait -PassThru -ErrorAction Stop
    } catch {
        Fail "提权失败：$($_.Exception.Message)"
    }
    exit $proc.ExitCode
}

# 服务名会进 sc.exe 与注册表，限制字符集避免被当成选项。
function Assert-ServiceName {
    if ($ServiceName -notmatch '^[A-Za-z0-9_.@-]+$') {
        Fail "服务名只能包含字母、数字、点、下划线、@ 和连字符：$ServiceName"
    }
}

function Remove-InstalledService {
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if (-not $service) {
        Write-Info "服务未注册：$ServiceName"
        return
    }

    Write-Info "停止并注销服务：$ServiceName"
    if ($service.Status -ne 'Stopped') {
        Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
        try { $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30)) } catch {
            Write-Warn '服务未能及时停止，继续注销'
        }
    }

    & sc.exe delete $ServiceName | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Warn "服务注销返回 $LASTEXITCODE，可能已不存在"
    }

    # 删除是异步的，确认名字已释放，否则重装会撞上「已存在」。
    for ($i = 0; $i -lt 40; $i++) {
        if (-not (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 250
    }
    if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
        Write-Warn "服务 $ServiceName 仍在注销中，重装前请确认已消失"
    }
    Write-Ok "服务已注销：$ServiceName"
}

# 默认只备份用户配置：它含口令、推送渠道与节点拓扑，重装后可直接复用。
# 数据库（可能数百 MB）与 web-dist（发布包自带）不备份，需要时手工拷贝。
#
# 备份目录默认放在安装目录的**同级**位置：卸载会删除安装目录，备份若放在
# 目录内会被同一次卸载一并删掉。
function Backup-Install {
    $target = $BackupDir
    if ([string]::IsNullOrWhiteSpace($target)) {
        $stamp = Get-Date -Format 'yyyyMMddHHmmss'
        $parent = Split-Path -Parent $ScriptDir
        if ([string]::IsNullOrWhiteSpace($parent)) { $parent = (Get-Location).Path }
        $target = Join-Path $parent "os-watcher-backup-$stamp"
    }

    $config = Join-Path $ScriptDir 'config.toml'
    if (-not (Test-Path -LiteralPath $config -PathType Leaf)) {
        Write-Warn "没有可备份的文件（$config 不存在）"
        return
    }

    New-Item -ItemType Directory -Path $target -Force | Out-Null
    Copy-Item -LiteralPath $config -Destination (Join-Path $target 'config.toml') -Force
    Write-Ok "已备份到：$target"
    Write-Info '重新安装后把 config.toml 放回安装目录即可恢复设置'
}

function Confirm-Uninstall {
    if ($Yes) { return }
    $backupText = if ($NoBackup) { '否' } else { '是（仅 config.toml）' }
    $keepText = if ($KeepConfig) { '是' } else { '否' }
    Write-Host @"
即将卸载 os-watcher：
  目录：$ScriptDir
  服务：$ServiceName
  备份：$backupText
  保留 config.toml：$keepText

该操作会删除安装目录，且不可撤销。
"@
    $answer = Read-Host '继续？[y/N]'
    if ($answer -notmatch '^(y|Y|yes|YES)$') {
        Fail '已取消'
    }
}

# 用 MoveFileEx 的 PENDING_DELETE 把被占用的路径登记为「重启时删除」。
function Register-RebootCleanup([string[]]$Targets) {
    if ($Targets.Count -eq 0) { return }

    # 必须走 IntPtr 重载：PowerShell 把 $null 传给 [string] 参数时会 marshal 成
    # 空字符串而不是 NULL 指针，MoveFileEx 的删除语义要求真正的 NULL，否则
    # 返回 ERROR_PATH_NOT_FOUND 且什么都不登记。
    Add-Type -Namespace Omp -Name Native -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode, EntryPoint = "MoveFileExW")]
public static extern bool MoveFileEx(string lpExistingFileName, IntPtr lpNewFileName, int dwFlags);
'@ -ErrorAction SilentlyContinue

    Write-Warn '以下文件正被占用，已登记为重启时删除：'
    foreach ($target in $Targets) {
        Write-Host "       $target"
        try {
            if (Test-Path -LiteralPath $target) {
                [Omp.Native]::MoveFileEx($target, [IntPtr]::Zero, 4) | Out-Null
            }
        } catch {
            Write-Warn "登记失败，请手动删除：$target"
        }
    }
}

function Remove-InstallFiles {
    # 脚本自身路径在函数内不能用 $MyInvocation.MyCommand.Path（那是函数自己的
    # 调用信息，严格模式下访问 .Path 会报错），用脚本级变量。
    $self = $ScriptPath
    $selfName = Split-Path -Leaf $self
    $pending = New-Object System.Collections.Generic.List[string]

    # uninstall.cmd 正在被 cmd.exe 逐行读取。立刻删掉它，cmd.exe 就读不到后续
    # 行，脚本会以「系统找不到指定的路径」中止并丢掉退出码。它与脚本自身一样，
    # 只能登记为重启时删除。
    $cmdEntry = Join-Path $ScriptDir 'uninstall.cmd'

    $entries = Get-ChildItem -LiteralPath $ScriptDir -Force | Where-Object {
        $_.Name -ne $selfName -and $_.FullName -ne $cmdEntry
    }

    # 先删目录再删文件：web-dist / backups 这类子目录可能很大。
    foreach ($entry in ($entries | Sort-Object { -not $_.PSIsContainer })) {
        if ($KeepConfig -and $entry.Name -eq 'config.toml') { continue }
        try {
            Remove-Item -LiteralPath $entry.FullName -Recurse -Force -ErrorAction Stop
        } catch {
            # 正在运行的服务二进制、被日志句柄占用的文件会走到这里。
            $pending.Add($entry.FullName)
        }
    }

    # 脚本自身：能删就删，删不掉（PowerShell 正读取它）就登记重启清理。
    try {
        Remove-Item -LiteralPath $self -Force -ErrorAction Stop
    } catch {
        $pending.Add($self)
    }

    if (Test-Path -LiteralPath $cmdEntry) {
        $pending.Add($cmdEntry)
    }

    # 目录只有在空了以后才能删。被占用文件挡着时把目录也登记上：重启时按登记
    # 顺序先删文件、再删目录，否则会剩一个空壳安装目录。
    #
    # 但 -KeepConfig 时绝不能登记目录：config.toml 要留在里面，登记目录等于
    # 让下次重启把用户明确要求保留的配置一起删掉。
    if (Test-Path -LiteralPath $ScriptDir) {
        if (-not (Get-ChildItem -LiteralPath $ScriptDir -Force)) {
            Remove-Item -LiteralPath $ScriptDir -Force -ErrorAction SilentlyContinue
        } elseif (-not $KeepConfig) {
            $pending.Add($ScriptDir)
        }
    }

    Register-RebootCleanup $pending.ToArray()
}

# ---------------------------------------------------------------- 主流程

Assert-Administrator
Assert-ServiceName

if ($RegisterOnly) {
    Remove-InstalledService
    exit 0
}

Write-Info "安装目录：$ScriptDir"

Confirm-Uninstall

# 备份在停服务之前：配置是静态文件，此时服务仍能正常写入，不会读到半截状态。
if ($NoBackup) {
    Write-Warn '已跳过备份（-NoBackup），config.toml 将随目录一起删除'
} else {
    Backup-Install
}

Remove-InstalledService
Remove-InstallFiles

Write-Ok 'os-watcher 已卸载'
