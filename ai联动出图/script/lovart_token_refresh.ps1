# ============================================================
# Lovart MCP - 令牌续期（日常用这个）
#
# 什么时候跑: 令牌有效期 30 天。过期后 CodeBuddy 里 lovart 的工具
#             会变空 / 报 401, 跑一次这个脚本即可。
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File "G:\01画布项目\lovart\script\lovart_token_refresh.ps1"
#
# 做了什么: 用 refresh_token 换新 access_token -> 存回
#           ~/.lovart/mcp_oauth.json -> 调用 lovart_apply_config.ps1
#           同步写入 G:\01画布项目\lovart\.mcp.json
# ============================================================

$ErrorActionPreference = 'Stop'
[System.Net.ServicePointManager]::Expect100Continue = $false

$TokenFile = Join-Path $env:USERPROFILE '.lovart\mcp_oauth.json'

if (-not (Test-Path $TokenFile)) { throw "找不到令牌文件: $TokenFile" }
$d = Get-Content $TokenFile -Raw | ConvertFrom-Json
if (-not $d.refresh_token) { throw '令牌文件里没有 refresh_token, 请改用 lovart_reauth.ps1 重新授权。' }

$body = "grant_type=refresh_token" +
  "&refresh_token=" + [Uri]::EscapeDataString($d.refresh_token) +
  "&client_id=" + $d.client_id +
  "&resource=" + [Uri]::EscapeDataString($d.resource)

$r = Invoke-WebRequest -Uri $d.token_endpoint -Method Post `
  -ContentType 'application/x-www-form-urlencoded' `
  -Headers @{ Accept = 'application/json' } `
  -Body $body -UseBasicParsing -TimeoutSec 30

$t = $r.Content | ConvertFrom-Json
if (-not $t.access_token) { throw ("刷新失败: " + $r.Content) }

$d.access_token = $t.access_token
if ($t.refresh_token) { $d.refresh_token = $t.refresh_token }
$d.expires_in = $t.expires_in
$d.saved_at = (Get-Date).ToString('o')

[System.IO.File]::WriteAllText(
  $TokenFile,
  ($d | ConvertTo-Json -Depth 6),
  (New-Object System.Text.UTF8Encoding($false))
)
Write-Host '令牌已刷新。'

& (Join-Path $PSScriptRoot 'lovart_apply_config.ps1')
