# ============================================================
# Lovart MCP - generation runner (one folder per task)
#
# A task folder holds everything: the prompt, the reference image, the
# output image, and the call record. The task .md is the single source:
#   ## 提示词      <- fenced block, Chinese, edit this
#   ## 本次调用    <- rewritten by this script on every run
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File "...\lovart_run.ps1" `
#     -PromptMd "...\生成图\参考1_草地帆布包\参考1_草地帆布包.md" `
#     -ArgsFile "...\script\_args_model.json" `
#     -RefImage "...\参考图\HKH产品.jpg"
#
# Parameters:
#   -PromptMd   task md: folder = output folder, basename = output name
#   -ArgsFile   tool arguments JSON (model / aspect_ratio / ref_images / params)
#   -RefImage   local reference image copied into the folder as 垫图.<ext>
#   -Proxy      proxy url; empty = auto (env HTTPS_PROXY, then WinINET settings)
#   -MaxWait    max seconds to wait for completion, default 900
#
# Notes:
#   - polls get_task_status every poll_after_seconds returned by the server
#   - reads and writes UTF-8 explicitly (PS 5.1 guesses ANSI otherwise)
#   - the request body is ASCII-escaped (\uXXXX) so Chinese never gets mangled
# ============================================================
param(
  [Parameter(Mandatory = $true)][string]$PromptMd,
  [Parameter(Mandatory = $true)][string]$ArgsFile,
  [string]$RefImage = '',
  [string]$Proxy = '',
  [int]$MaxWait = 900
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[System.Net.ServicePointManager]::Expect100Continue = $false

$utf8 = New-Object System.Text.UTF8Encoding($false)

function Read-Utf8([string]$path) {
  return [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)
}

# JSON must reach the server intact whatever the console codepage is:
# escape every non-ASCII char to \uXXXX so the body is pure ASCII.
function ConvertTo-AsciiJson([string]$json) {
  $sb = New-Object System.Text.StringBuilder
  foreach ($ch in $json.ToCharArray()) {
    $code = [int]$ch
    if ($code -gt 126) { [void]$sb.Append(('\u{0:x4}' -f $code)) }
    else { [void]$sb.Append($ch) }
  }
  return $sb.ToString()
}

# ---- task md: read prompt, remember everything up to the prompt block ----
if (-not (Test-Path $PromptMd)) { throw "task md not found: $PromptMd" }
$PromptMd = [System.IO.Path]::GetFullPath($PromptMd)
$outDir = Split-Path $PromptMd -Parent
$name = [System.IO.Path]::GetFileNameWithoutExtension($PromptMd)

$mdLines = (Read-Utf8 $PromptMd) -split "`r?`n"
$keepEnd = -1
$promptText = ''
for ($i = 0; $i -lt $mdLines.Count; $i++) {
  if ($mdLines[$i].Trim() -eq '## 提示词') {
    $open = -1; $close = -1
    for ($j = $i + 1; $j -lt $mdLines.Count; $j++) {
      if ($mdLines[$j].Trim() -eq '```') {
        if ($open -lt 0) { $open = $j } else { $close = $j; break }
      }
    }
    if ($open -lt 0 -or $close -lt 0) { throw "no fenced block under '## 提示词': $PromptMd" }
    $promptText = (($mdLines[($open + 1)..($close - 1)]) -join "`n").Trim()
    $keepEnd = $close
    break
  }
}
if ($keepEnd -lt 0) { throw "no '## 提示词' section: $PromptMd" }
if (-not $promptText) { throw "prompt block is empty: $PromptMd" }
$keepLines = $mdLines[0..$keepEnd]

# ---- proxy ----
$proxyUrl = $Proxy
if (-not $proxyUrl) { $proxyUrl = $env:HTTPS_PROXY }
if (-not $proxyUrl) {
  $ie = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' -ErrorAction SilentlyContinue
  if ($ie -and $ie.ProxyEnable -eq 1 -and $ie.ProxyServer) { $proxyUrl = 'http://' + $ie.ProxyServer }
}
if ($proxyUrl) {
  [System.Net.WebRequest]::DefaultWebProxy = New-Object System.Net.WebProxy($proxyUrl)
  Write-Host "[proxy] $proxyUrl"
}
else {
  Write-Host "[proxy] none"
}

# ---- token ----
$tokenFile = Join-Path $env:USERPROFILE '.lovart\mcp_oauth.json'
if (-not (Test-Path $tokenFile)) { throw "token file not found: $tokenFile" }
$tok = (Read-Utf8 $tokenFile | ConvertFrom-Json).access_token
if (-not $tok) { throw "no access_token in $tokenFile" }

$url = 'https://lgw.lovart.ai/v1/openapi/mcp/widgets'
$headers = @{
  Authorization = "Bearer $tok"
  Accept        = 'application/json, text/event-stream'
}

function Invoke-Mcp {
  param([string]$Method, $Params)
  $json = @{ jsonrpc = '2.0'; id = 1; method = $Method; params = $Params } | ConvertTo-Json -Depth 30 -Compress
  $bytes = [System.Text.Encoding]::ASCII.GetBytes((ConvertTo-AsciiJson $json))
  try {
    $r = Invoke-WebRequest -Uri $url -Method Post -ContentType 'application/json; charset=utf-8' `
      -Headers $headers -Body $bytes -UseBasicParsing -TimeoutSec 120
  }
  catch {
    $code = ''
    if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
    throw "MCP call failed ($Method, HTTP $code): $($_.Exception.Message)"
  }
  $sid = $r.Headers['Mcp-Session-Id']
  if (-not $sid) { $sid = $r.Headers['mcp-session-id'] }
  if ($sid) { $headers['Mcp-Session-Id'] = $sid }
  $c = $r.Content
  if ($c -match '(?s)data:\s*(\{.*\})') { $c = $matches[1] }
  return ($c | ConvertFrom-Json)
}

function Get-Payload($resp) {
  if ($resp.error) { throw "MCP returned an error: $($resp.error | ConvertTo-Json -Compress)" }
  if ($resp.result -and $resp.result.structuredContent) { return $resp.result.structuredContent }
  if ($resp.result -and $resp.result.content) {
    foreach ($c in $resp.result.content) {
      if ($c.type -eq 'text') {
        try { return ($c.text | ConvertFrom-Json) } catch { }
      }
    }
  }
  return $null
}

function Invoke-Tool {
  param([string]$Name, $Arguments)
  $resp = Invoke-Mcp -Method 'tools/call' -Params @{ name = $Name; arguments = $Arguments }
  return Get-Payload $resp
}

# ---- handshake ----
Invoke-Mcp -Method 'initialize' -Params @{
  protocolVersion = '2024-11-05'
  capabilities    = @{}
  clientInfo      = @{ name = 'lovart_run'; version = '1.0' }
} | Out-Null

# ---- arguments ----
if (-not (Test-Path $ArgsFile)) { throw "args file not found: $ArgsFile" }
$toolArgs = (Read-Utf8 $ArgsFile | ConvertFrom-Json)
$toolArgs | Add-Member -NotePropertyName prompt -NotePropertyValue $promptText -Force

# ---- reference image into the folder ----
$refCopy = ''
if ($RefImage) {
  if (-not (Test-Path $RefImage)) { throw "ref image not found: $RefImage" }
  $refExt = [System.IO.Path]::GetExtension($RefImage)
  $refCopy = Join-Path $outDir "垫图$refExt"
  Copy-Item -Path $RefImage -Destination $refCopy -Force
  Write-Host "[ref] $refCopy"
}

# ---- submit ----
Write-Host "[submit] generate_image"
$sub = Invoke-Tool -Name 'generate_image' -Arguments $toolArgs

$taskIds = @()
if ($sub -and $sub.task_ids) { $taskIds = @($sub.task_ids) }
$projectId = $null
if ($sub -and $sub.project_id) { $projectId = $sub.project_id }
if ($taskIds.Count -eq 0) { throw "no task_ids returned: $($sub | ConvertTo-Json -Compress)" }

$pollSec = 20
if ($sub.poll_after_seconds) { $pollSec = [int]$sub.poll_after_seconds }
Write-Host "[task] $($taskIds -join ', ')  project=$projectId  poll=${pollSec}s"

# ---- poll ----
$deadline = (Get-Date).AddSeconds($MaxWait)
$final = $null
while ($true) {
  if ((Get-Date) -gt $deadline) {
    throw "timed out after ${MaxWait}s, task still running: $($taskIds -join ', ')"
  }
  Start-Sleep -Seconds $pollSec
  $st = Invoke-Tool -Name 'get_task_status' -Arguments @{ task_ids = $taskIds; project_id = $projectId }
  $status = $st.status
  if (-not $status -and $st.tasks) { $status = ($st.tasks | Select-Object -First 1).status }
  Write-Host "[poll] $status"
  if ($status -eq 'completed') { $final = $st; break }
  if ($status -eq 'failed' -or $status -eq 'error' -or $status -eq 'canceled') {
    throw "task $status : $($taskIds -join ', ')"
  }
}

# ---- artifacts ----
$res = Invoke-Tool -Name 'get_task' -Arguments @{ task_ids = $taskIds; project_id = $projectId; include_images = $false }
if (-not $res.artifacts -and $final.artifacts) { $res = $final }
$artifacts = @()
if ($res.artifacts) { $artifacts = @($res.artifacts) }
if ($artifacts.Count -eq 0) { throw "task completed but no artifacts: $($taskIds -join ', ')" }

# ---- save into the task folder ----
if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir -Force | Out-Null }
$paths = @()
$savedNames = @()
for ($i = 0; $i -lt $artifacts.Count; $i++) {
  $a = $artifacts[$i]
  $ext = 'png'
  if ($a.format) { $ext = $a.format }
  $suffix = ''
  if ($artifacts.Count -gt 1) { $suffix = "_$($i + 1)" }
  $fileName = "$name$suffix.$ext"
  $path = Join-Path $outDir $fileName
  Invoke-WebRequest -Uri $a.url -OutFile $path -UseBasicParsing -TimeoutSec 300
  $paths += $path
  $savedNames += "$fileName（$($a.width)×$($a.height)）"
  Write-Host "[saved] $path"
}

# ---- rewrite the task md: prompt block kept, call record refreshed ----
$spec = $res.spec
$specJson = ''
if ($spec) { $specJson = ($spec | ConvertTo-Json -Compress) }
$refLine = '（未指定）'
$refJson = ''
if ($toolArgs.ref_images) { $refJson = (@($toolArgs.ref_images) | ConvertTo-Json -Compress) }
if ($refCopy) {
  $refLine = "``$([System.IO.Path]::GetFileName($refCopy))``　发送用 ref_images: ``$refJson``"
}
elseif ($refJson) {
  $refLine = "``$refJson``"
}

$out = @()
$out += $keepLines
$out += ''
$out += '## 本次调用'
$out += ''
$out += "- 模型: ``$($res.model)``　比例: ``$($spec.aspect_ratio)``　参数: ``$specJson``"
$out += "- 垫图: $refLine"
$out += "- project_id: ``$projectId``　task_id: ``$($taskIds -join ', ')``"
$out += "- 出图: $($savedNames -join '、')"
[System.IO.File]::WriteAllText($PromptMd, ($out -join "`r`n"), $utf8)
Write-Host "[record] $PromptMd"

[pscustomobject]@{
  folder     = $outDir
  files      = $paths
  record     = $PromptMd
  ref        = $refCopy
  task_ids   = $taskIds
  project_id = $projectId
  artifacts  = @($artifacts | ForEach-Object { $_.url })
} | ConvertTo-Json -Depth 5
