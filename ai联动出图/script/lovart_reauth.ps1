# ============================================================
# Lovart MCP - 完整重新授权（兜底用）
#
# 什么时候跑: refresh_token 也失效的时候（比如超过 30 天没用、
#             或在 Lovart 侧撤销过授权）。
#             表现是 lovart_token_refresh.ps1 报错、刷不出新令牌。
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File "G:\01画布项目\lovart\script\lovart_reauth.ps1"
#
# 做了什么: 在本地起一个临时回调服务 -> 打开浏览器走 OAuth 授权码
#           流程(PKCE) -> 换取全新的 access_token + refresh_token ->
#           存回 ~/.lovart/mcp_oauth.json -> 同步写入 .mcp.json
#
# 注意: 需要你在浏览器里手动点授权。请确保浏览器已登录 Lovart。
# ============================================================

$ErrorActionPreference = 'Stop'

# Windows PowerShell 默认发送 Expect: 100-continue,
# 会被 lovart 的网关判为 invalid_request, 必须关掉
[System.Net.ServicePointManager]::Expect100Continue = $false
[System.Net.ServicePointManager]::SecurityProtocol = [System.Net.SecurityProtocolType]::Tls12

$resource = 'https://lgw.lovart.ai/v1/openapi/mcp/widgets'
$clientId = 'codex-lovart'
$authz    = 'https://api.lovart.ai/api/www/oauth/authorize'
$tokenEp  = 'https://api.lovart.ai/api/www/oauth/token'

function New-B64Url([byte[]]$b) {
  return ([Convert]::ToBase64String($b)).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Parse('127.0.0.1'), 0)
$listener.Start()
$port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
$redirect = "http://127.0.0.1:$port/callback"

$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$vb = New-Object byte[] 32
$rng.GetBytes($vb)
$verifier = New-B64Url $vb

$sha = [System.Security.Cryptography.SHA256]::Create()
$challenge = New-B64Url ($sha.ComputeHash([System.Text.Encoding]::ASCII.GetBytes($verifier)))

$sb = New-Object byte[] 16
$rng.GetBytes($sb)
$state = New-B64Url $sb

$url = "$authz" +
  "?response_type=code" +
  "&client_id=$clientId" +
  "&state=$state" +
  "&code_challenge=$challenge" +
  "&code_challenge_method=S256" +
  "&redirect_uri=" + [Uri]::EscapeDataString($redirect) +
  "&resource=" + [Uri]::EscapeDataString($resource)

Write-Host "本地回调地址: $redirect"
Start-Process $url
Write-Host '已打开浏览器, 请在浏览器里完成 Lovart 授权, 等待回调...'

$code = $null
$deadline = (Get-Date).AddMinutes(5)
while ((Get-Date) -lt $deadline -and -not $code) {
  if ($listener.Pending()) {
    $client = $listener.AcceptTcpClient()
    try {
      $stream = $client.GetStream()
      $reader = New-Object System.IO.StreamReader($stream)
      $requestLine = $reader.ReadLine()
      while ($true) {
        $line = $reader.ReadLine()
        if ($null -eq $line -or $line -eq '') { break }
      }
      $path = '/'
      if ($requestLine -match '^GET\s+(\S+)') { $path = $matches[1] }

      $page = "<html><head><meta charset='utf-8'></head><body style='font-family:system-ui,sans-serif;padding:48px;text-align:center'><h2>Lovart 授权已完成</h2><p>可以关闭此页面, 回到终端查看结果。</p></body></html>"
      $head = "HTTP/1.1 200 OK`r`nContent-Type: text/html; charset=utf-8`r`nContent-Length: $([System.Text.Encoding]::UTF8.GetByteCount($page))`r`nConnection: close`r`n`r`n"
      $buf = [System.Text.Encoding]::UTF8.GetBytes($head + $page)
      $stream.Write($buf, 0, $buf.Length)
      $stream.Flush()

      if ($path -match '[?&]code=([^&]+)') { $code = [Uri]::UnescapeDataString($matches[1]) }
      if ($path -match '[?&]error=([^&]+)') { throw ("OAuth 返回错误: " + [Uri]::UnescapeDataString($matches[1])) }
    }
    finally {
      $client.Close()
    }
  }
  else {
    Start-Sleep -Milliseconds 300
  }
}
$listener.Stop()

if (-not $code) { throw '等待授权回调超时(5分钟)。' }
Write-Host '已拿到授权码, 正在换取令牌...'

function Invoke-TokenRequest([string]$bodyString, [string]$label) {
  Write-Host "== 尝试换取令牌: $label"
  try {
    $r = Invoke-WebRequest -Uri $tokenEp -Method Post `
      -ContentType 'application/x-www-form-urlencoded' `
      -Headers @{ Accept = 'application/json' } `
      -Body $bodyString -UseBasicParsing -TimeoutSec 30
    Write-Host ("   HTTP " + $r.StatusCode)
    return ($r.Content | ConvertFrom-Json)
  }
  catch {
    $raw = ''
    if ($_.Exception.Response) {
      try {
        $s = New-Object IO.StreamReader($_.Exception.Response.GetResponseStream())
        $raw = $s.ReadToEnd()
      }
      catch { }
    }
    Write-Host ("   失败: " + $_.Exception.Message)
    if ($raw) { Write-Host ("   响应: " + $raw) }
    return $null
  }
}

$encCode  = [Uri]::EscapeDataString($code)
$encRedir = [Uri]::EscapeDataString($redirect)
$encRes   = [Uri]::EscapeDataString($resource)

$variants = @(
  @{ label = '带 resource'; body = "grant_type=authorization_code&code=$encCode&redirect_uri=$encRedir&client_id=$clientId&code_verifier=$verifier&resource=$encRes" },
  @{ label = '不带 resource'; body = "grant_type=authorization_code&code=$encCode&redirect_uri=$encRedir&client_id=$clientId&code_verifier=$verifier" }
)

$tokenResp = $null
foreach ($v in $variants) {
  $tokenResp = Invoke-TokenRequest $v.body $v.label
  if ($tokenResp -and $tokenResp.access_token) { break }
}

if (-not $tokenResp -or -not $tokenResp.access_token) {
  throw '所有换取令牌的尝试都失败了。'
}

$out = [ordered]@{
  access_token   = $tokenResp.access_token
  refresh_token  = $tokenResp.refresh_token
  token_type     = $tokenResp.token_type
  expires_in     = $tokenResp.expires_in
  scope          = $tokenResp.scope
  saved_at       = (Get-Date).ToString('o')
  token_endpoint = $tokenEp
  client_id      = $clientId
  resource       = $resource
}

$dir = Join-Path $env:USERPROFILE '.lovart'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$target = Join-Path $dir 'mcp_oauth.json'
[System.IO.File]::WriteAllText(
  $target,
  ($out | ConvertTo-Json -Depth 5),
  (New-Object System.Text.UTF8Encoding($false))
)

Write-Host ("令牌已保存到: " + $target)
Write-Host ("access_token 长度: " + $tokenResp.access_token.Length)
Write-Host ("是否含 refresh_token: " + [bool]$tokenResp.refresh_token)
Write-Host ("有效期(秒): " + $tokenResp.expires_in)

& (Join-Path $PSScriptRoot 'lovart_apply_config.ps1')
