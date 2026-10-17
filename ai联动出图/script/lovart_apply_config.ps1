# ============================================================
# Lovart MCP - 把令牌同步写入工作区级 MCP 配置
#
# 作用: 读取 ~/.lovart/mcp_oauth.json 里的 access_token,
#       写入 G:\01画布项目\lovart\.mcp.json
#       (即 CodeBuddy 的工作区级 MCP 配置, 只对该文件夹生效)
#
# 说明: 一般不需要手动运行。
#       由 lovart_token_refresh.ps1 和 lovart_reauth.ps1 自动调用。
# ============================================================

$ErrorActionPreference = 'Stop'

$TokenFile = Join-Path $env:USERPROFILE '.lovart\mcp_oauth.json'
$McpJson   = 'G:\01画布项目\lovart\.mcp.json'
$McpUrl    = 'https://lgw.lovart.ai/v1/openapi/mcp/widgets'

if (-not (Test-Path $TokenFile)) { throw "找不到令牌文件: $TokenFile" }
$token = (Get-Content $TokenFile -Raw | ConvertFrom-Json).access_token
if (-not $token) { throw "令牌文件里没有 access_token: $TokenFile" }

$servers = [ordered]@{}
if (Test-Path $McpJson) {
  $existing = Get-Content $McpJson -Raw | ConvertFrom-Json
  if ($existing.mcpServers) {
    foreach ($p in $existing.mcpServers.PSObject.Properties) { $servers[$p.Name] = $p.Value }
  }
}

$servers['lovart'] = [ordered]@{
  type        = 'http'
  url         = $McpUrl
  headers     = [ordered]@{ Authorization = "Bearer $token" }
  description = 'Lovart AI 设计 / 图像生成 MCP'
}

# 必须无 BOM 写入, 否则 CodeBuddy 解析 JSON 会失败
$json = [ordered]@{ mcpServers = $servers } | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($McpJson, $json, (New-Object System.Text.UTF8Encoding($false)))

Write-Host "已写入工作区 MCP 配置: $McpJson"
Write-Host '重启或重载 CodeBuddy 后生效。'
