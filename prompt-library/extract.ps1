$out = 'G:\01画布项目\lovart-auto\prompt-library'
$kw  = '产品|商品|电商|商拍|静物|平铺|俯拍|拍摄|广告摄影|产品图|product|e-?commerce|commercial|packshot|still life|flat.?lay|lifestyle'
$nl  = [char]10

function Save-Hits($name, $hits, $srcNote) {
    $path = Join-Path $out $name
    $sw = [System.IO.File]::CreateText($path)
    $sw.WriteLine('# ' + $srcNote + ' | ' + $hits.Count + ' 条')
    $sw.WriteLine('')
    foreach ($h in $hits) {
        $sw.WriteLine($h.Trim())
        $sw.WriteLine('')
        $sw.WriteLine('---')
        $sw.WriteLine('')
    }
    $sw.Close()
    Write-Output ($name + ' -> ' + $hits.Count + ' 条')
}

# --- songguoxs: split by anchor, match on title line (2nd line of block) ---
$sg    = 'G:\01画布项目\lovart-auto\prompt-library\repos\songguoxs-gpt4o-image-prompts'
$files = Get-ChildItem $sg -Filter '*.md' | Where-Object { $_.Name -match '^\d+\.md$' }
$all   = @()
foreach ($f in $files) {
    $raw    = [System.IO.File]::ReadAllText($f.FullName)
    $blocks = [regex]::Split($raw, '(?m)^(?=<a id="prompt-\d+"></a>)')
    foreach ($b in $blocks) {
        $lines = $b -split $nl
        $title = if ($lines.Count -gt 1) { $lines[1] } else { $lines[0] }
        if ($title -match $kw) { $all += $b }
    }
}
Save-Hits '提取-songguoxs-产品电商.md' $all '提取自 songguoxs/gpt4o-image-prompts (3,854 stars)，按案例标题匹配'

# --- YouMind: split by "### No. N:", match on header line ---
$ym     = 'G:\01画布项目\lovart-auto\prompt-library\repos\youmind-awesome-nano-banana-pro-prompts\README.md'
$raw    = [System.IO.File]::ReadAllText($ym)
$blocks = [regex]::Split($raw, '(?m)^(?=### (Featured )?No\. \d+:)')
$hits   = @()
foreach ($b in $blocks) {
    $lines = $b -split $nl
    $title = $lines[0]
    if ($title -match $kw) { $hits += $b }
}
Save-Hits '提取-YouMind-产品电商.md' $hits '提取自 YouMind-OpenLab/awesome-nano-banana-pro-prompts (13,659 stars)，按条目标题匹配'

# --- xianyu110: split by "#### N.", match on header line ---
$xy     = 'G:\01画布项目\lovart-auto\prompt-library\repos\xianyu110-awesome-nanobananapro-prompts\README.md'
$raw    = [System.IO.File]::ReadAllText($xy)
$blocks = [regex]::Split($raw, '(?m)^(?=#### \d+\.)')
$hits2  = @()
foreach ($b in $blocks) {
    $lines = $b -split $nl
    if ($lines[0] -match $kw) { $hits2 += $b }
}
Save-Hits '提取-xianyu110-产品电商.md' $hits2 '提取自 xianyu110/awesome-nanobananapro-prompts (815 stars)，按条目标题匹配'
