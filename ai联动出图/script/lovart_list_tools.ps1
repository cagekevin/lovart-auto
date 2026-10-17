# ============================================================
# Lovart MCP - 列出当前可用的工具（诊断用）
#
# 什么时候用: 想确认令牌还有效没、服务端现在有哪些工具、
#             或者 CodeBuddy 里工具显示为空时排查用。
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File "G:\01画布项目\lovart\script\lovart_list_tools.ps1"
#
# 说明: 这个调用不消耗 credits。
#       如果连接返回 401, 说明令牌过期, 去跑 lovart_token_refresh.ps1。
# ============================================================

$ErrorActionPreference = 'Stop'
[System.Net.ServicePointManager]::Expect100Continue = $false

$tokenFile = Join-Path $env:USERPROFILE '.lovart\mcp_oauth.json'
if (-not (Test-Path $tokenFile)) { throw "找不到令牌文件: $tokenFile" }

$tok = (Get-Content $tokenFile -Raw | ConvertFrom-Json).access_token
if (-not $tok) { throw "令牌文件里没有 access_token: $tokenFile" }

$url = 'https://lgw.lovart.ai/v1/openapi/mcp/widgets'
$headers = @{
  Authorization = "Bearer $tok"
  Accept        = 'application/json, text/event-stream'
}

$initBody = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"lovart_list_tools","version":"1.0"}}}'

try {
  $r = Invoke-WebRequest -Uri $url -Method Post -ContentType 'application/json' `
    -Headers $headers -Body $initBody -UseBasicParsing -TimeoutSec 30
}
catch {
  $code = ''
  if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
  throw "连接失败 (HTTP $code)。如果返回 401, 说明令牌已过期, 请运行 lovart_token_refresh.ps1"
}

$sid = $r.Headers['Mcp-Session-Id']
if (-not $sid) { $sid = $r.Headers['mcp-session-id'] }
if ($sid) { $headers['Mcp-Session-Id'] = $sid }

$toolsBody = '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
$r2 = Invoke-WebRequest -Uri $url -Method Post -ContentType 'application/json' `
  -Headers $headers -Body $toolsBody -UseBasicParsing -TimeoutSec 30

$p = $r2.Content
if ($p -match '(?s)data:\s*(\{.*\})') { $p = $matches[1] }
$obj = $p.Trim() | ConvertFrom-Json

$tools = $obj.result.tools
Write-Host ("可用工具数量: " + @($tools).Count)
Write-Host ''
foreach ($t in $tools) {
  $req = @()
  if ($t.inputSchema.required) { $req = $t.inputSchema.required }
  $names = @()
  if ($t.inputSchema.properties) {
    foreach ($pr in $t.inputSchema.properties.PSObject.Properties) {
      $mark = ''
      if ($req -contains $pr.Name) { $mark = '*' }
      $names += ($pr.Name + $mark)
    }
  }
  Write-Host ('- ' + $t.name)
  if ($names.Count -gt 0) {
    $legend = ''
    if ($req.Count -gt 0) { $legend = '   (* = 必填)' }
    Write-Host ('    参数: ' + ($names -join ', ') + $legend)
  }
}
