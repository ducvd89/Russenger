# Builds Russenger for Windows and assembles dist\Russenger\ (app + login window + Go helper).
# Needs: Rust (GNU toolchain + MinGW on PATH) and Go.
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User') + ";$env:USERPROFILE\.cargo\bin;C:\Program Files\Go\bin"

# icon (assets\icon.ico / icon.png) is checked in; regenerate with assets\make-icon.ps1

Push-Location "$root\relay-helper"
$env:CGO_ENABLED = '0'
go build -ldflags '-s -w' -o "$root\bin\relay-helper.exe" .
if ($LASTEXITCODE -ne 0) { throw "go build failed" }
Pop-Location

Push-Location "$root\relay-win"
cmd /c "cargo build --release 2>&1"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
Pop-Location

$dist = "$root\dist\Russenger"
New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item "$root\relay-win\target\release\russenger.exe", "$root\relay-win\target\release\russenger-login.exe", "$root\relay-win\target\release\russenger-player.exe", "$root\relay-win\target\release\russenger-call.exe","$root\bin\relay-helper.exe" $dist -Force
# GNU-target builds load WebView2 through this DLL at runtime (needed by russenger-login.exe)
$loader = Get-ChildItem "$root\relay-win\target\release\build\webview2-com-sys\*\out\x64\WebView2Loader.dll" | Select-Object -First 1
Copy-Item $loader.FullName $dist -Force
Copy-Item "$root\LICENSE", "$root\THIRD-PARTY-NOTICES.md" $dist -Force
Write-Host "Done: $dist"
