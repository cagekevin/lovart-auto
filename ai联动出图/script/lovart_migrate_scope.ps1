# ============================================================
# Lovart MCP - 【一次性脚本, 已于 2026-09-24 执行完毕, 留档备查】
#
# 作用: 把 lovart MCP 配置从全局作用域迁移到工作区作用域。
#   - 写入 G:\01画布项目\lovart\.mcp.json   (工作区级, 只对该文件夹生效)
#   - 从 ~/.codebuddy/mcp.json 里移除 lovart (全局级, 对所有项目生效)
#
# 什么时候才需要再跑: 只有当配置又被写回全局、你想重新限定作用域时。
#                     正常情况不需要动它。
# ============================================================

$ErrorActionPreference = 'Stop'

function Write-JsonNoBom([string]$path, $obj, [int]$depth = 10) {
  $json = $obj | ConvertTo-Json -Depth $depth
  $enc = New-Object System.Text.UTF8Encoding($false)
  [System.IO.File]::WriteAllText($path, $json, $enc)
}

$tokenFile = Join-Path $env:USERPROFILE '.lovart\mcp_oauth.json'
$tok = (Get-Content $tokenFile -Raw | ConvertFrom-Json).access_token
if (-not $tok) { throw 'No access_token in ' + $tokenFile }

$ws = 'G:\01画布项目\lovart'
$wsFile = Join-Path $ws '.mcp.json'

$server = [ordered]@{
  type        = 'http'
  url         = 'https://lgw.lovart.ai/v1/openapi/mcp/widgets'
  headers     = [ordered]@{ Authorization = "Bearer $tok" }
  description = 'Lovart AI 设计 / 图像生成 MCP'
}

Write-JsonNoBom $wsFile ([ordered]@{ mcpServers = [ordered]@{ lovart = $server } })
Write-Host "WORKSPACE CONFIG -> $wsFile"

$globalFile = Join-Path $env:USERPROFILE '.codebuddy\mcp.json'
$servers = [ordered]@{}
if (Test-Path $globalFile) {
  $existing = Get-Content $globalFile -Raw | ConvertFrom-Json
  foreach ($p in $existing.mcpServers.PSObject.Properties) {
    if ($p.Name -ne 'lovart') { $servers[$p.Name] = $p.Value }
  }
}
Write-JsonNoBom $globalFile ([ordered]@{ mcpServers = $servers })
Write-Host ("GLOBAL CONFIG -> " + $globalFile + "  (remaining: " + (($servers.Keys -join ', ') + '') + ")")
