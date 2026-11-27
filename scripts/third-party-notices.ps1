#!/usr/bin/env pwsh
# 生成 THIRD_PARTY_NOTICES.md：随 dshr（桌面版与命令行版，Windows / macOS）分发的第三方组件及其许可证。
# 范围：工作区全部成员在下列目标上的普通依赖；dev / build 依赖不进最终程序，不列入。
# 许可证全文取自各 crate 发布包里自带的 LICENSE / COPYING / NOTICE 文件，相同文本只保留一份。
# 用法：pwsh scripts/third-party-notices.ps1          依赖变化（Cargo.lock 改动）后重新运行并提交结果
#       pwsh scripts/third-party-notices.ps1 -Check   只检查文件是否最新，过期时退出码为 1（CI 用）
param([switch]$Check)
$ErrorActionPreference = 'Stop'
# cargo 输出为 UTF-8，避免按系统代码页解码损坏非 ASCII 字符（如 crate 描述里的特殊字符）。
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Text.UTF8Encoding]::new($false)

$root = Split-Path -Parent $PSScriptRoot
$outFile = Join-Path $root 'THIRD_PARTY_NOTICES.md'
$targets = 'x86_64-pc-windows-msvc', 'aarch64-apple-darwin', 'x86_64-apple-darwin'

# 带 C 源码或数据、需要单独说明的 crate（只在实际用到时列出）。
$bundled = [ordered]@{
    'rquickjs-sys'        = @('QuickJS-ng', 'https://github.com/quickjs-ng/quickjs', 'MIT',
                              'JS 插件引擎，C 源码随 crate 编译进程序')
    'libsqlite3-sys'      = @('SQLite', 'https://sqlite.org/copyright.html', 'Public Domain',
                              '插件的数据库（`host.db`），C 源码随 crate 编译进程序')
    'zstd-sys'            = @('Zstandard', 'https://github.com/facebook/zstd', 'BSD-3-Clause',
                              '会话日志的 zstd 压缩，C 源码编译进程序；上游为 BSD / GPLv2 双许可，本项目按 BSD 使用')
    'aws-lc-sys'          = @('AWS-LC', 'https://github.com/aws/aws-lc', 'ISC、Apache-2.0、OpenSSL 等',
                              'TLS 加密库（经 rustls），C 源码编译进程序；含源自 BoringSSL / OpenSSL 的代码，各部分的许可见下文全文')
    'gpui-kit-assets'     = @('Lucide 图标', 'https://lucide.dev', 'ISC',
                              '界面图标（SVG）嵌入程序')
    'icu_properties_data' = @('Unicode 字符数据', 'https://www.unicode.org/license.txt', 'Unicode-3.0',
                              'ICU4X 的 Unicode 属性、分词与规范化数据嵌入程序')
}
# 主许可证文件之外，还要收录的内嵌源码许可证（相对 crate 根目录）。
$extraLicenseFiles = @{
    'rquickjs-sys' = @('quickjs/LICENSE')
    'zstd-sys'     = @('zstd/LICENSE')
    'aws-lc-sys'   = @('aws-lc/LICENSE')
}
# 发布包没有许可证文件的 crate：按其 SPDX 声明从 spdx.org 取标准文本，缓存在此目录（提交进仓库，
# 之后生成不再需要联网）。抓取失败的才列进“未附带许可证文本的 crate”。
$spdxCache = Join-Path $PSScriptRoot 'license-texts'
$licenseFileName = '^(licen[cs]e|copying|copyright|notice|unlicense)([-._].*)?$'

function Get-Normalized([string]$text) {
    $lines = ($text -replace "`r`n?", "`n") -split "`n" | ForEach-Object { $_.TrimEnd() }
    ($lines -join "`n").Trim("`n", ' ', "`t", [char]0xFEFF)
}

function Get-Fence([string]$text) {
    $longest = 0
    foreach ($m in [regex]::Matches($text, '`+')) { $longest = [Math]::Max($longest, $m.Length) }
    '`' * [Math]::Max(3, $longest + 1)
}

function Format-Cell([string]$s) { $s -replace '\|', '\|' }

# 从 SPDX 表达式里取出 `A OR B` 中的许可证 id（多份许可证都收录；不支持 AND 与例外写法）。
function Get-SpdxIds([string]$license) {
    $ids = [System.Collections.Generic.List[string]]::new()
    if (-not $license) { return $ids }
    foreach ($part in [regex]::Split($license, '\s+OR\s+|\s+or\s+|\s*/\s*')) {
        $id = $part.Trim() -replace '^LicenseRef-', ''
        if ($id -match '^[A-Za-z0-9.+-]+$' -and $id -notmatch '^(WITH|AND|OR|MIT-style)$') { $ids.Add($id) }
    }
    $ids
}

# 取某 SPDX 许可证的标准文本：先读缓存，没有则从 spdx.org 下载并写入缓存；失败返回 $null。
# 文本统一规范化为 LF，避免缓存文件的换行符随平台不同导致生成结果不一致。
function Get-SpdxText([string]$id) {
    $path = Join-Path $spdxCache "$id.txt"
    if (Test-Path -LiteralPath $path) {
        return ([IO.File]::ReadAllText($path) -replace "`r`n?", "`n")
    }
    try {
        $text = (Invoke-WebRequest -Uri "https://spdx.org/licenses/$id.txt" -UseBasicParsing -TimeoutSec 30).Content
    } catch { return $null }
    if (-not ($text -and $text.Length -gt 200)) { return $null }
    $text = $text -replace "`r`n?", "`n"
    [IO.Directory]::CreateDirectory($spdxCache) | Out-Null
    [IO.File]::WriteAllText($path, $text, [Text.UTF8Encoding]::new($false))
    Write-Host "已从 spdx.org 缓存许可证文本：$id"
    $text
}

Push-Location $root
try {
    $meta = cargo metadata --format-version 1 --locked | ConvertFrom-Json
    if ($LASTEXITCODE) { throw 'cargo metadata 失败' }
    # 只排除工作区自身的 crate：按 workspace_members 判断，不能看 source 是否为空——
    # `[patch.crates-io]` 指向 vendor/ 的依赖（如 gpui-pre-windows、gpui-pre-macos）也是本地路径，source 同样为空。
    $members = [System.Collections.Generic.HashSet[string]]::new([string[]]$meta.workspace_members)
    $byKey = @{}
    foreach ($p in $meta.packages) {
        if (-not $members.Contains($p.id)) { $byKey["$($p.name) $($p.version)"] = $p }
    }

    $shipped = [System.Collections.Generic.HashSet[string]]::new()
    foreach ($t in $targets) {
        $lines = cargo tree --workspace --locked -e normal --target $t --prefix none -f '{p}'
        if ($LASTEXITCODE) { throw "cargo tree 失败：$t" }
        foreach ($l in $lines) {
            if ($l -match '^(\S+) v(\S+)' -and $byKey.ContainsKey("$($Matches[1]) $($Matches[2])")) {
                [void]$shipped.Add("$($Matches[1]) $($Matches[2])")
            }
        }
    }
    $keys = [string[]]@($shipped)
    [Array]::Sort($keys, [StringComparer]::Ordinal)
    $packages = $keys | ForEach-Object { $byKey[$_] }
} finally {
    Pop-Location
}

# 收集许可证文本，按内容去重。
$groups = [ordered]@{}
$missing = [System.Collections.Generic.List[object]]::new()
foreach ($p in $packages) {
    $dir = Split-Path -Parent $p.manifest_path
    $files = @(Get-ChildItem -LiteralPath $dir -File |
        Where-Object { $_.Name -match $licenseFileName -and $_.Extension -ne '.rs' } |
        Sort-Object { $_.Name.ToLowerInvariant() } |
        ForEach-Object { $_.Name })
    if ($extraLicenseFiles.ContainsKey($p.name)) {
        $files += $extraLicenseFiles[$p.name] | Where-Object { Test-Path -LiteralPath (Join-Path $dir $_) }
    }
    # crate 没有自带许可证文件时，按其 SPDX 声明收录标准许可证文本（来源标注在“使用者”里）。
    $texts = foreach ($f in $files) {
        [pscustomobject]@{ Text = [IO.File]::ReadAllText((Join-Path $dir $f)); Source = $f -replace '\\', '/' }
    }
    if ($files.Count -eq 0) {
        $texts = foreach ($id in Get-SpdxIds $p.license) {
            $t = Get-SpdxText $id
            if ($t) { [pscustomobject]@{ Text = $t; Source = "$id 标准文本（spdx.org）" } }
        }
        if (-not $texts) { $missing.Add($p); continue }
    }
    foreach ($entry in $texts) {
        $text = Get-Normalized $entry.Text
        if (-not $text) { continue }
        # 只差在换行、缩进的文本视为同一份，保留最先遇到的排版。
        $key = $text -replace '\s+', ' '
        $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($key)))
        if (-not $groups.Contains($hash)) {
            $groups[$hash] = [pscustomobject]@{ Text = $text; Users = [System.Collections.Generic.List[string]]::new() }
        }
        $groups[$hash].Users.Add("$($p.name) $($p.version)（$($entry.Source)）")
    }
}

$md = [System.Collections.Generic.List[string]]::new()
function Add([string]$line = '') { $md.Add($line) }

Add '# 第三方声明（Third-Party Notices）'
Add
Add 'dshr 本身以 MIT 许可发布，见 [LICENSE](LICENSE)。dshr 的桌面版与命令行版包含下列第三方组件，'
Add '它们各自的版权归原作者所有，按各自的许可证使用。可选双许可（如 `MIT OR Apache-2.0`）的组件，本项目按其中任一许可使用。'
Add
$targetList = ($targets | ForEach-Object { '`' + $_ + '`' }) -join '、'
Add "统计范围：工作区全部成员在 $targetList 上的普通依赖，共 $($packages.Count) 个 crate；"
Add 'dev / build 依赖不进最终程序，不列入。'
Add
Add '## 参照项目'
Add
Add '- **DeepSeek Harness**（`deepseek-ai/deepseek-harness`，MIT）：dshr 是它的非官方 Rust 重写版，只参照其信息结构、交互流程与会话日志格式，'
Add '  目前不包含它的代码或资源。今后复用其资源（文案、提示词、图标等）时，在这里列出并附上原版 LICENSE。“DeepSeek”名称与标识归其所有者，dshr 与其无隶属关系。'
Add
Add '## 内嵌的非 Rust 组件'
Add
Add '下列组件以 C 源码或数据的形式随对应的 crate 编译进程序，许可证全文见“许可证全文”一节中对应 crate 的条目。'
Add
Add '| 组件 | 随 crate | 许可证 | 用途 |'
Add '|---|---|---|---|'
foreach ($name in $bundled.Keys) {
    $p = $packages | Where-Object name -eq $name | Select-Object -First 1
    if (-not $p) { continue }
    $b = $bundled[$name]
    Add "| [$($b[0])]($($b[1])) | ``$($p.name)`` $($p.version) | $($b[2]) | $($b[3]) |"
}
Add '| Rust 标准库 | 工具链 | MIT OR Apache-2.0 | 静态链接进程序（https://github.com/rust-lang/rust） |'
Add
Add '## 依赖清单'
Add
Add '| crate | 版本 | 许可证（SPDX） |'
Add '|---|---|---|'
foreach ($p in $packages) {
    $url = if ($p.repository) { $p.repository } else { "https://crates.io/crates/$($p.name)" }
    $lic = if ($p.license) { $p.license } elseif ($p.license_file) { "见 $($p.license_file)" } else { '未声明' }
    Add "| [$($p.name)]($url) | $($p.version) | $(Format-Cell $lic) |"
}
Add
if ($missing.Count) {
    Add '## 未附带许可证文本的 crate'
    Add
    Add '下列 crate 的发布包里没有许可证文件，且未能从 spdx.org 取得其声明许可证的标准文本，标准文本见 https://spdx.org/licenses/ 。'
    Add '版权归各自的作者所有（括号内为 crate 声明的作者，未声明的见其源码仓库）。'
    Add
    foreach ($p in $missing) {
        $authors = @($p.authors | ForEach-Object { $_ -replace '\s*<[^>]*>', '' } | Where-Object { $_ })
        $by = if ($authors.Count) { "（$($authors -join '、')）" } else { '' }
        Add "- $($p.name) $($p.version)：$($p.license)$by"
    }
    Add
}
Add '## 许可证全文'
Add
Add '相同的文本只收录一次，标题下列出使用它的 crate 及文件名。'
$i = 0
foreach ($g in $groups.Values) {
    $i++
    Add
    Add "### $i. $(($g.Users | Select-Object -First 1) -replace '（.*$', '')$(if ($g.Users.Count -gt 1) { " 等 $($g.Users.Count) 项" })"
    Add
    Add "使用者：$($g.Users -join '、')"
    Add
    $fence = Get-Fence $g.Text
    Add "${fence}text"
    Add $g.Text
    Add $fence
}

$content = ($md -join "`n") + "`n"
if ($Check) {
    $current = if (Test-Path -LiteralPath $outFile) { [IO.File]::ReadAllText($outFile) -replace "`r`n", "`n" } else { '' }
    if ($current -ne $content) {
        Write-Error 'THIRD_PARTY_NOTICES.md 已过期：运行 pwsh scripts/third-party-notices.ps1 并提交结果。' -ErrorAction Continue
        exit 1
    }
    Write-Output "THIRD_PARTY_NOTICES.md 是最新的（$($packages.Count) 个 crate）。"
} else {
    [IO.File]::WriteAllText($outFile, $content, [Text.UTF8Encoding]::new($false))
    Write-Output "已生成 THIRD_PARTY_NOTICES.md：$($packages.Count) 个 crate，$($groups.Count) 份许可证文本，$($missing.Count) 个缺少许可证文件。"
}
