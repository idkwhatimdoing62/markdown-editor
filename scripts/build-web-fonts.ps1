param(
    [string]$OutputDirectory = "fonts/web"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$outputRoot = Join-Path $repo $OutputDirectory
New-Item -ItemType Directory -Force -Path $outputRoot | Out-Null

$program = @'
from pathlib import Path
from fontTools.ttLib import TTFont
import sys

repo = Path(sys.argv[1])
output = Path(sys.argv[2])
fonts = [
    "JetBrainsMono-Regular.ttf",
    "JetBrainsMono-Bold.ttf",
    "LXGWWenKaiLite-Regular.ttf",
    "LXGWWenKaiLite-Medium.ttf",
]

for name in fonts:
    font = TTFont(repo / "fonts" / name, recalcTimestamp=False)
    font.flavor = "woff"
    font.save(output / (Path(name).stem + ".woff"))
'@

$tempScript = Join-Path ([System.IO.Path]::GetTempPath()) (
    "markdown-editor-build-web-fonts-{0}.py" -f [guid]::NewGuid().ToString("N")
)
try {
    Set-Content -LiteralPath $tempScript -Value $program -Encoding utf8
    & uv run --with "fonttools[woff]" python $tempScript $repo $outputRoot
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to generate WebView WOFF fonts"
    }
}
finally {
    Remove-Item -LiteralPath $tempScript -Force -ErrorAction SilentlyContinue
}

Get-ChildItem -LiteralPath $outputRoot -Filter *.woff |
    Sort-Object Name |
    Select-Object Name, Length
