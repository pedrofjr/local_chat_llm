<#
.SYNOPSIS
  Builds, signs and publishes a release that `/update` will accept.

.DESCRIPTION
  Three things have to line up or the group cannot install the result:
  the version in Cargo.toml, the git tag, and the signature over the exe.
  This does all three from one place so they cannot drift apart.

  The signing key never reaches the console, the repository, or the release.
  It is read from a file, used, and dropped.

.PARAMETER KeyFile
  Path to the release private key (64 hex characters, one line).

.PARAMETER MinVersion
  Oldest build that can still talk to this one. Set it whenever the wire
  format changes, so older builds say "you must update" instead of silently
  failing to connect.

.EXAMPLE
  .\scripts\publish-release.ps1 -KeyFile $HOME\local-llm-release.key
  .\scripts\publish-release.ps1 -KeyFile $HOME\k.key -MinVersion 0.5.0
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$KeyFile,
    [string]$MinVersion,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

function Step($text) { Write-Output "==> $text" }

# --- version, from the one place that defines it -------------------------
$cargo = Get-Content 'Cargo.toml' -Raw
if ($cargo -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'no version in Cargo.toml' }
$version = $Matches[1]
$tag = "v$version"
Step "publishing $tag"

if (git tag --list $tag) { throw "tag $tag already exists - bump the version in Cargo.toml first" }
if (git status --porcelain) { throw 'working tree is dirty - commit before publishing' }

# --- key, read but never echoed ------------------------------------------
if (-not (Test-Path $KeyFile)) { throw "key file not found: $KeyFile" }
$keyHex = (Get-Content $KeyFile -Raw).Trim()
if ($keyHex -notmatch '^[0-9a-fA-F]{64}$') { throw 'key file must hold 64 hex characters' }

# --- notes, from the CHANGELOG -------------------------------------------
# O texto que o grupo lê ao atualizar sai do mesmo lugar onde o repositório
# guarda o histórico, e é conferido aqui em cima -- antes do build -- para uma
# versão sem seção parar em um segundo, e não depois de cinco minutos de cargo.
#
# Falhar em vez de publicar um texto genérico é o ponto: era o genérico que
# fazia o release não dizer o que mudou.
Step 'reading the release notes'
$changelog = Get-Content 'CHANGELOG.md' -Raw
$heading = [regex]::Escape($version)
$section = [regex]::Match($changelog, "(?ms)^##\s+$heading(?![\d.]).*?(?=^##\s|\z)")
if (-not $section.Success) { throw "CHANGELOG.md has no section for $version - write it first" }
# Fora o cabeçalho: o título do release já diz a versão.
$body = ($section.Value -replace '(?m)\A##\s.*\r?\n', '').Trim()
if (-not $body) { throw "the CHANGELOG section for $version is empty" }
$footer = (Get-Content (Join-Path $PSScriptRoot 'release-footer.md') -Raw).Trim()
$notesPath = Join-Path ([IO.Path]::GetTempPath()) "local-llm-notes-$version.md"
Set-Content -LiteralPath $notesPath -Value "$body`n`n$footer" -Encoding utf8NoBOM

# --- build ----------------------------------------------------------------
Step 'cargo test'
cargo test --quiet
if ($LASTEXITCODE -ne 0) { throw 'tests failed' }

Step 'cargo clippy'
cargo clippy --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { throw 'clippy failed' }

# Windows will not let cargo replace an exe that is running, and somebody is
# usually using the app while a release is being cut. An alternative target
# directory sidesteps that without asking anyone to close anything.
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
Step "building into $target"
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

$exe = Join-Path $target 'release\local-llm.exe'
if (-not (Test-Path $exe)) { throw "no exe at $exe" }
$size = [math]::Round((Get-Item $exe).Length / 1MB, 2)
Step "exe is $size MB"

# --- sign -----------------------------------------------------------------
# Python does the Ed25519 because it is already here; the app verifies the
# result with iroh's implementation, which is the same standard curve.
Step 'signing'
$out = Join-Path $target 'release'
$py = @'
import sys, hashlib
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
key_hex, exe_path, out_dir = sys.argv[1], sys.argv[2], sys.argv[3]
key = Ed25519PrivateKey.from_private_bytes(bytes.fromhex(key_hex))
data = open(exe_path, "rb").read()
open(out_dir + "/sha256.txt", "w").write(hashlib.sha256(data).hexdigest())
open(out_dir + "/sig.txt", "w").write(key.sign(data).hex())
open(out_dir + "/pub.txt", "w").write(key.public_key().public_bytes_raw().hex())
'@
$pyFile = Join-Path $env:TEMP 'll-sign.py'
Set-Content -Path $pyFile -Value $py -Encoding utf8
python $pyFile $keyHex $exe $out
if ($LASTEXITCODE -ne 0) { throw 'signing failed' }
Remove-Item $pyFile -Force
$keyHex = $null

$sha = (Get-Content "$out\sha256.txt" -Raw).Trim()
$sig = (Get-Content "$out\sig.txt" -Raw).Trim()
$pub = (Get-Content "$out\pub.txt" -Raw).Trim()

# The app only runs binaries signed by the key baked into it. Catching a
# mismatch here beats shipping a release nobody can install.
$src = Get-Content 'src\update.rs' -Raw
if ($src -match '(?s)RELEASE_PUBKEY:\s*\[u8;\s*32\]\s*=\s*\[(.*?)\];') {
    $bytes = [regex]::Matches($Matches[1], '0x([0-9a-fA-F]{2})') | ForEach-Object { $_.Groups[1].Value }
    $embeddedHex = ($bytes -join '').ToLower()
    if ($embeddedHex -ne $pub) {
        throw "this key does not match RELEASE_PUBKEY in src/update.rs`n  key:      $pub`n  embedded: $embeddedHex"
    }
    Step 'key matches the one built into the app'
} else {
    throw 'could not read RELEASE_PUBKEY from src/update.rs'
}

# --- manifest -------------------------------------------------------------
$url = "https://github.com/pedrofjr/local_chat_llm/releases/download/$tag/local-llm.exe"
$manifest = @"
version = "$version"
url = "$url"
sha256 = "$sha"
sig = "$sig"
"@
if ($MinVersion) { $manifest += "`nmin_version = `"$MinVersion`"" }
$manifestPath = "$out\latest.toml"
Set-Content -Path $manifestPath -Value $manifest -Encoding ascii
Step 'manifest:'
Get-Content $manifestPath | ForEach-Object { Write-Output "    $_" }

if ($DryRun) {
    Step 'dry run - nothing tagged or uploaded'
    return
}

# --- publish --------------------------------------------------------------
Step "tagging $tag"
git tag -a $tag -m "local-llm $version"
git push origin $tag 2>$null
if ($LASTEXITCODE -ne 0) { git push upstream $tag }

Step 'creating the release'
gh release create $tag $exe $manifestPath `
    --repo pedrofjr/local_chat_llm `
    --title "local-llm $version" `
    --notes-file $notesPath
if ($LASTEXITCODE -ne 0) { throw 'gh release create failed' }
Remove-Item -LiteralPath $notesPath -ErrorAction SilentlyContinue

Step "done - $tag is live, /update will find it"
