#Requires -Version 5.1
<#
.SYNOPSIS
    os-watcher Release 包部署脚本（Windows 原生）。

.DESCRIPTION
    下载 Release 包、校验 SHA256、备份现有安装、覆盖安装，并把 os-watcher
    注册为 Windows 服务（原生 sc.exe / SCM，无需 nssm）。

    脚本需要管理员权限。可直接调用，也可用 deploy.cmd 自动提权。

.EXAMPLE
    .\deploy.ps1 -Package full
.EXAMPLE
    .\deploy.ps1 -Package node -Port 7980 -GossipPort 7979 -Peers 10.0.0.1:7979,10.0.0.2:7979
.EXAMPLE
    .\deploy.ps1 -Package full -Version v0.1.4 -Force
#>
[CmdletBinding()]
param(
    # 安装包类型：node（仅二进制）或 full（含 Web 面板）
    [ValidateSet('node', 'full')]
    [string]$Package = 'node',

    # Release 版本：latest 或具体 tag（如 v0.1.4）
    [string]$Version = 'latest',

    # GitHub 仓库，owner/repo
    [string]$Repo = $(if ($env:GITHUB_REPO) { $env:GITHUB_REPO } else { 'yous1r/os-watcher' }),

    # 覆盖平台检测，默认 windows-x86_64
    [string]$Platform = '',

    # REST API / Web 面板端口
    [int]$Port = 7980,

    # Gossip UDP 端口
    [int]$GossipPort = 7979,

    # 对等节点列表，逗号分隔 host:port
    [string]$Peers = '',

    # 服务名
    [string]$ServiceName = 'os-watcher',

    # 下载代理；也可使用 HTTP_PROXY / HTTPS_PROXY 环境变量
    [string]$Proxy = '',

    # 跳过确认直接覆盖安装
    [switch]$Force,

    # 只注册/重启服务，不下载也不覆盖安装（deploy.sh 的 Windows 分支用它）
    [switch]$RegisterOnly,

    # deploy.cmd 用：非管理员时经 UAC 重新拉起自己
    [switch]$Elevate,

    # 显示帮助
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 控制台默认代码页（本机 437）渲染不了中文，中文提示会变成问号。
try {
    [Console]::OutputEncoding = [Text.Encoding]::UTF8
    $OutputEncoding = [Text.Encoding]::UTF8
} catch {
    # 重定向到文件时可能没有控制台，忽略即可。
}

# ---------------------------------------------------------------- 输出工具

function Write-Info([string]$Message) { Write-Host "[INFO] $Message" -ForegroundColor Cyan }
function Write-Ok([string]$Message) { Write-Host "[ OK ] $Message" -ForegroundColor Green }
function Write-Warn([string]$Message) { Write-Host "[WARN] $Message" -ForegroundColor Yellow }
function Write-Err([string]$Message) { Write-Host "[FAIL] $Message" -ForegroundColor Red }
function Fail([string]$Message) {
    Write-Err $Message
    exit 1
}

function Show-Usage {
    @"
os-watcher Release 包部署脚本（Windows）

用法：
  .\deploy.ps1 -Package node
  .\deploy.ps1 -Package full -Port 7980 -GossipPort 7979
  .\deploy.ps1 -Package full -Version v0.1.4 -Force
  .\deploy.cmd -Package full            # 自动提权

选项：
  -Package <node|full>       安装包类型，默认 node
  -Version <tag|latest>      Release 版本，默认 latest
  -Repo <owner/repo>         GitHub 仓库，默认 yous1r/os-watcher
  -Platform <name>           覆盖自动平台检测
  -Port <PORT>               REST API / Web 面板端口，默认 7980
  -GossipPort <PORT>         Gossip UDP 端口，默认 7979
  -Peers <LIST>              对等节点列表，逗号分隔 host:port
  -ServiceName <NAME>        服务名，默认 os-watcher
  -Proxy <URL>               下载代理；也可使用 HTTP_PROXY/HTTPS_PROXY
  -Force                     跳过确认直接覆盖安装
  -RegisterOnly              只注册/重启服务，不下载也不覆盖安装
  -Help                      显示帮助
"@ | Write-Host
}

# ---------------------------------------------------------------- 前置检查

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-Platform {
    if (-not [Environment]::Is64BitOperatingSystem) {
        Fail "仅提供 64 位 Windows 的 Release 包"
    }
    return 'windows-x86_64'
}

function Get-ReleaseAssetUrl([string]$Asset) {
    if ($Version -eq 'latest') {
        return "https://github.com/$Repo/releases/latest/download/$Asset"
    }
    return "https://github.com/$Repo/releases/download/$Version/$Asset"
}

# 下载失败返回 $false，由调用方决定是致命还是仅告警。
function Save-File([string]$Url, [string]$Destination) {
    $params = @{
        Uri             = $Url
        OutFile         = $Destination
        UseBasicParsing = $true
        TimeoutSec      = 300
    }
    if ($Proxy) {
        $params['Proxy'] = $Proxy
    }
    try {
        Invoke-WebRequest @params
        return $true
    } catch {
        Write-Verbose "下载失败：$Url —— $_"
        return $false
    }
}

# Get-FileHash 在精简过的 PS 5.1 上可能缺失，直接用 .NET 更稳。
function Get-Sha256Hex([string]$Path) {
    $sha = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::OpenRead($Path)
    try {
        return (($sha.ComputeHash($stream) | ForEach-Object { $_.ToString('x2') }) -join '')
    } finally {
        $stream.Dispose()
        $sha.Dispose()
    }
}

function Confirm-Install {
    if ($Force) { return }
    @"
即将安装 os-watcher：
  仓库：$Repo
  版本：$Version
  平台：$Platform
  包型：$Package
  目录：$ScriptDir
  服务：$ServiceName
  端口：$Port（API）/ $GossipPort（Gossip）
"@ | Write-Host
    $answer = Read-Host "继续？[y/N]"
    if ($answer -notmatch '^(y|yes)$') {
        Fail '已取消'
    }
}

# ---------------------------------------------------------------- 安装步骤

# 运行中的服务占着 exe，覆盖安装会因共享冲突失败，所以先停。
function Stop-InstalledService {
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if (-not $service) { return }

    Write-Info "停止现有服务：$ServiceName"
    if ($service.Status -ne 'Stopped') {
        Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
        $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
    }
}

function Backup-CurrentInstall([string]$BinName) {
    $stamp = Get-Date -Format 'yyyyMMddHHmmss'
    $backupDir = Join-Path $ScriptDir "backups\deploy-$stamp"
    New-Item -ItemType Directory -Path $backupDir -Force | Out-Null

    $items = @($BinName, 'config.toml', 'config.example.toml')
    foreach ($item in $items) {
        $src = Join-Path $ScriptDir $item
        if (Test-Path -LiteralPath $src -PathType Leaf) {
            Copy-Item -LiteralPath $src -Destination $backupDir -Force
        }
    }
    $webDist = Join-Path $ScriptDir 'web-dist'
    if (Test-Path -LiteralPath $webDist -PathType Container) {
        Copy-Item -LiteralPath $webDist -Destination $backupDir -Recurse -Force
    }

    Write-Ok "已备份当前安装到：$backupDir"
}

function Install-Payload([string]$Root, [string]$BinName) {
    $sourceExe = Join-Path $Root $BinName
    if (-not (Test-Path -LiteralPath $sourceExe -PathType Leaf)) {
        Fail "Release 包缺少可执行文件：$BinName"
    }

    Copy-Item -LiteralPath $sourceExe -Destination (Join-Path $ScriptDir $BinName) -Force

    foreach ($item in @('README.md', 'deploy.sh', 'deploy.ps1', 'deploy.cmd', 'config.example.toml')) {
        $src = Join-Path $Root $item
        if (Test-Path -LiteralPath $src -PathType Leaf) {
            Copy-Item -LiteralPath $src -Destination (Join-Path $ScriptDir $item) -Force
        }
    }

    $configPath = Join-Path $ScriptDir 'config.toml'
    $examplePath = Join-Path $ScriptDir 'config.example.toml'
    if (-not (Test-Path -LiteralPath $configPath) -and (Test-Path -LiteralPath $examplePath)) {
        Copy-Item -LiteralPath $examplePath -Destination $configPath
        Write-Ok "已创建默认配置：$configPath"
    } else {
        Write-Info '保留现有 config.toml，仅更新 config.example.toml'
    }

    $sourceWeb = Join-Path $Root 'web-dist'
    if (Test-Path -LiteralPath $sourceWeb -PathType Container) {
        $targetWeb = Join-Path $ScriptDir 'web-dist'
        if (Test-Path -LiteralPath $targetWeb) {
            Remove-Item -LiteralPath $targetWeb -Recurse -Force
        }
        Copy-Item -LiteralPath $sourceWeb -Destination $targetWeb -Recurse -Force
    }

    Write-Ok "Release 包已安装到：$ScriptDir"
}

function Install-Service([string]$BinName) {
    $exePath = Join-Path $ScriptDir $BinName
    $configPath = Join-Path $ScriptDir 'config.toml'
    $serviceArgs = "--config `"$configPath`" start --api-port $Port --gossip-port $GossipPort"
    if ($Peers) {
        $serviceArgs += " --peers $Peers"
    }
    # 服务进程的工作目录是 System32，因此 exe 与配置都用绝对路径。
    $binPath = "`"$exePath`" $serviceArgs"

    $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($existing) {
        Stop-InstalledService
        Write-Info "注销现有服务：$ServiceName"
        & sc.exe delete $ServiceName | Out-Null
        # 删除是异步的，名字没释放前无法重新创建。
        for ($i = 0; $i -lt 40; $i++) {
            if (-not (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue)) { break }
            Start-Sleep -Milliseconds 250
        }
        if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
            Fail "服务 $ServiceName 未能注销，请手动执行：sc.exe delete $ServiceName"
        }
    }

    Write-Info "注册服务：$ServiceName"
    # New-Service 会把 binPath 原样写入注册表；sc.exe create 在 PowerShell 下
    # 会重新解析引号，带空格的路径会被拆错。
    New-Service -Name $ServiceName -BinaryPathName $binPath `
        -DisplayName $ServiceName -StartupType Automatic | Out-Null

    Write-Info "启动服务：$ServiceName"
    Start-Service -Name $ServiceName

    $service = Get-Service -Name $ServiceName
    $service.WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
    Write-Ok "Windows 服务已启动：$ServiceName"
    Write-Info "日志：$(Join-Path $ScriptDir 'os-watcher.log')"
}

# ---------------------------------------------------------------- 主流程

if ($Help) {
    Show-Usage
    exit 0
}

if (-not (Test-Administrator)) {
    if (-not $Elevate) {
        Fail '需要管理员权限：请用「以管理员身份运行」的 PowerShell 执行，或直接运行 deploy.cmd'
    }

    # 逐项转成 -Name 值 / -Switch 形式。含空格的值（如 -Peers）必须加引号，
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

if ($Repo -notmatch '/') {
    Fail '-Repo 必须是 owner/repo 格式'
}
if ([string]::IsNullOrWhiteSpace($Version)) {
    Fail '-Version 不能为空'
}

$ScriptDir = $PSScriptRoot
Set-Location -LiteralPath $ScriptDir

if (-not $Platform) {
    $Platform = Get-Platform
}

$binName = 'os-watcher.exe'

# 只注册服务：Release 包已经由调用方解压到本目录，这里不再联网。
if ($RegisterOnly) {
    Install-Service -BinName $binName
    exit 0
}

$ext = 'zip'
$asset = "os-watcher-$Platform-$Package.$ext"
$assetUrl = Get-ReleaseAssetUrl $asset
$shaUrl = Get-ReleaseAssetUrl "$asset.sha256"

Confirm-Install

# PS 5.1 默认可能不启用 TLS 1.2，GitHub 只接受 1.2 以上。
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$tempDir = Join-Path ([IO.Path]::GetTempPath()) ("os-watcher-deploy-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

try {
    $archive = Join-Path $tempDir $asset
    $shaFile = Join-Path $tempDir "$asset.sha256"
    $extractDir = Join-Path $tempDir 'extract'

    Write-Info "下载 Release 包：$assetUrl"
    if (-not (Save-File -Url $assetUrl -Destination $archive)) {
        Fail "下载失败：$assetUrl"
    }

    if (Save-File -Url $shaUrl -Destination $shaFile) {
        $expected = ((Get-Content -LiteralPath $shaFile -First 1) -split '\s+')[0].ToLower()
        $actual = Get-Sha256Hex $archive
        if ($expected -ne $actual) {
            Fail "SHA256 校验失败：期望 $expected，实际 $actual"
        }
        Write-Ok 'SHA256 校验通过'
    } else {
        Write-Warn '未下载到 SHA256 文件，继续安装'
    }

    Write-Info '解压 Release 包'
    Expand-Archive -LiteralPath $archive -DestinationPath $extractDir -Force

    $root = Get-ChildItem -LiteralPath $extractDir -Directory |
        Select-Object -First 1 -ExpandProperty FullName
    if (-not $root) {
        Fail 'Release 包结构异常：未找到顶层目录'
    }

    Stop-InstalledService
    Backup-CurrentInstall -BinName $binName
    Install-Payload -Root $root -BinName $binName
    Install-Service -BinName $binName
} finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
