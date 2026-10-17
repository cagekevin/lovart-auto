# ============================================================
# Lovart MCP - 通用工具调用器
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File "G:\01画布项目\lovart\script\lovart_call.ps1" -Tool list_models
#   powershell -NoProfile -ExecutionPolicy Bypass -File "G:\01画布项目\lovart\script\lovart_call.ps1" -Tool get_credits
#   powershell -NoProfile -ExecutionPolicy Bypass -File "G:\01画布项目\lovart\script\lovart_call.ps1" -Tool generate_image -Args '{\"prompt\":\"...\",\"model\":\"...\"}'
#
# 说明:
#   -Tool      工具名, 见 lovart_list_tools.ps1 的输出
#   -Args      工具参数的 JSON 字符串, 默认 {}
#   -ArgsFile  改为从 JSON 文件读取工具参数 (参数太长时用, 如 upload_asset 的 base64)
#
#   查询类 (list_models / describe_model / get_credits / list_projects / get_task_status)
#   不消耗 credits; 生成类 (generate_image / edit_image / generate_video / upscale 等) 会消耗。
# ============================================================

param(
  [Parameter(Mandatory = $true)][string]$Tool,
  [string]$Args = '{}',
  [string]$ArgsFile
)

$ErrorActionPreference = 'Stop'
[System.Net.ServicePointManager]::Expect100Continue = $false

$tokenFile = Join-Path $env:USERPROFILE '.lovart\mcp_oauth.json'
if (-not (Test-Path $tokenFile)) { throw "找不到令牌文件: $tokenFile" }
$tok = (Get-Content $tokenFile -Raw | ConvertFrom-Json).access_token
if (-not $tok) { throw "令牌文件里没有 access_token" }

$url = 'https://lgw.lovart.ai/v1/openapi/mcp/widgets'
$headers = @{
  Authorization = "Bearer $tok"
  Accept        = 'application/json, text/event-stream'
}

$init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"lovart_call","version":"1.0"}}}'

try {
  $r = Invoke-WebRequest -Uri $url -Method Post -ContentType 'application/json' `
    -Headers $headers -Body $init -UseBasicParsing -TimeoutSec 30
}
catch {
  $code = ''
  if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
  throw "连接失败 (HTTP $code)。若为 401, 令牌已过期, 请运行 lovart_token_refresh.ps1"
}

$sid = $r.Headers['Mcp-Session-Id']
if (-not $sid) { $sid = $r.Headers['mcp-session-id'] }
if ($sid) { $headers['Mcp-Session-Id'] = $sid }

if ($ArgsFile) {
  if (-not (Test-Path $ArgsFile)) { throw "找不到参数文件: $ArgsFile" }
  $rawArgs = Get-Content $ArgsFile -Raw
}
else {
  $rawArgs = $Args
}

$payloadObj = @{
  jsonrpc = '2.0'
  id      = 2
  method  = 'tools/call'
  params  = @{
    name      = $Tool
    arguments = ($rawArgs | ConvertFrom-Json)
  }
}
$payload = $payloadObj | ConvertTo-Json -Depth 30 -Compress

$r2 = Invoke-WebRequest -Uri $url -Method Post -ContentType 'application/json' `
  -Headers $headers -Body $payload -UseBasicParsing -TimeoutSec 600

$c = $r2.Content
if ($c -match '(?s)data:\s*(\{.*\})') { $c = $matches[1] }
$c.Trim()
