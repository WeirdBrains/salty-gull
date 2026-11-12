# Per-user MSI end-to-end test. Runs as a standard (non-admin) Windows user.
$ErrorActionPreference = 'Continue'
$T = 'C:\SaltyMsiTest'
$O = "$T\out"; New-Item -ItemType Directory -Force $O | Out-Null
$ResultFile = "$O\result.txt"; [IO.File]::WriteAllText($ResultFile, '')
function Log($m) { [IO.File]::AppendAllText($ResultFile, "$m`r`n") }
Start-Transcript -Path "$O\transcript.txt" | Out-Null
function Ok($m) { Log "ok - $m" }
function Fail($m) { Log "not ok - $m"; Stop-Server; exit 1 }
$Srv = $null
function Stop-Server { if ($script:Srv) { Stop-Process -Id $script:Srv.Id -Force -ErrorAction SilentlyContinue } }
# Installed products through the Windows Installer API. Context 2 = per user (unmanaged), 4 = per machine.
function Products {
  $i = New-Object -ComObject WindowsInstaller.Installer
  foreach ($p in $i.ProductsEx('', '', 7)) {
    if ($p.InstallProperty('InstalledProductName') -eq 'Salty Demo') {
      $t = [System.__ComObject]
      [pscustomobject]@{
        Code = $t.InvokeMember('ProductCode', 'GetProperty', $null, $p, $null)
        Context = $t.InvokeMember('Context', 'GetProperty', $null, $p, $null)
        Version = $p.InstallProperty('VersionString')
      }
    }
  }
}
function Entry { Products | Where-Object Context -eq 2 | Select-Object -First 1 }
function MachineEntry { Products | Where-Object Context -eq 4 }
function Msi([string[]]$a) { (Start-Process msiexec.exe -ArgumentList $a -Wait -PassThru).ExitCode }
$App = "$env:LOCALAPPDATA\Programs\SaltyDemo\SaltyDemo.exe"
function RunApp([string]$a) {
  $p = Start-Process -FilePath $App -ArgumentList $a -Wait -PassThru -NoNewWindow -RedirectStandardOutput "$O\o.txt" -RedirectStandardError "$O\e.txt"
  $t = ((Get-Content "$O\o.txt" -Raw) + (Get-Content "$O\e.txt" -Raw)).Trim()
  return @{ code = $p.ExitCode; out = $t }
}
function WaitVersion([string]$v) {
  $sid = (Get-Process -Id $PID).SessionId
  for ($i = 0; $i -lt 240; $i++) {
    $e = Entry
    $busy = @(Get-Process msiexec -ErrorAction SilentlyContinue | Where-Object SessionId -eq $sid).Count
    if ($e -and $e.Version -eq $v -and $busy -eq 0) { return $true }
    Start-Sleep -Milliseconds 500
  }
  return $false
}

$groups = whoami /groups
Log "# user: $(whoami); admin group: $([bool]($groups | Select-String 'S-1-5-32-544')); $(($groups | Select-String 'Mandatory Label').ToString().Split(' ')[1..2] -join ' ')"
if ($groups | Select-String 'S-1-5-32-544') { Fail 'running with the Administrators group' }
Ok 'running as a standard user (not in Administrators)'
foreach ($old in @(Entry)) { if ($old) { Msi @('/x', $old.Code, '/qn', '/norestart') | Out-Null } }
Remove-Item -Recurse -Force "$env:TEMP\salty-msi-test" -ErrorAction SilentlyContinue

# Control: a per-machine MSI must fail without elevation.
$c = Msi @('/i', "$T\control-permachine.msi", '/qn', '/norestart', '/l*v', "$O\control.log")
if ($c -eq 0 -or (MachineEntry)) { Fail "per-machine control MSI installed without admin (exit $c)" }
Ok "per-machine control MSI refused without admin (msiexec exit $c)"

# Install 1.0.0 per user.
$c = Msi @('/i', "$T\demo-1.0.0.msi", 'MSIINSTALLPERUSER=1', 'ALLUSERS=2', '/qn', '/norestart', '/l*v', "$O\i100.log")
if ($c -ne 0) { Fail "per-user install exited $c" }
$e = Entry
if (-not $e -or $e.Version -ne '1.0.0' -or (MachineEntry)) { Fail 'no per-user uninstall entry for 1.0.0' }
if (-not (Test-Path $App)) { Fail "not installed at $App" }
Ok "1.0.0 installed per user without admin at $App (Windows Installer per-user context, nothing per machine)"
$r = RunApp 'run'; if ($r.code -ne 0 -or $r.out -ne 'salty-demo 1.0.0 ready') { Fail "1.0.0 start: $($r.code) $($r.out)" }
Ok 'launcher runs 1.0.0'

# Server and publishing.
$env:SALTY_TOKEN = (& "$T\salty-server.exe" --home "$O\srv" admin-token 2>$null | Select-Object -First 1)
$script:Srv = Start-Process -FilePath "$T\salty-server.exe" -ArgumentList '--home', "$O\srv", 'serve', '--org', 'demo', '--bind', '127.0.0.1:18471' -PassThru -WindowStyle Hidden -RedirectStandardError "$O\server.err" -RedirectStandardOutput "$O\server.out"
$up = $false
for ($i = 0; $i -lt 50; $i++) { & "$T\saltyctl.exe" publish-root "$T\root.cbor" --server http://127.0.0.1:18471 *> $null; if ($LASTEXITCODE -eq 0) { $up = $true; break }; Start-Sleep -Milliseconds 200 }
if (-not $up) { Fail 'server did not start' }
& "$T\saltyctl.exe" publish "$T\m1.cbor" --dir "$T\pkg-1.1.0" --server http://127.0.0.1:18471 *> $null
if ($LASTEXITCODE -ne 0) { Fail 'publish 1.1.0' }
Ok 'server up; 1.1.0 MSI published'

# Update to 1.1.0: the app records the install, starts msiexec detached and exits.
$r = RunApp "update --installer-arg /qn --installer-arg /l*v --installer-arg $O\u110.log"
if ($r.code -ne 0 -or $r.out -notmatch 'started msiexec') { Fail "update to 1.1.0: $($r.code) $($r.out)" }
if (-not (WaitVersion '1.1.0')) { Fail 'msiexec did not upgrade to 1.1.0' }
$r = RunApp 'run'; if ($r.out -ne 'salty-demo 1.1.0 ready (committed demo-msi-1)') { Fail "1.1.0 trial start: $($r.code) $($r.out)" }
$r = RunApp 'run'; if ($r.out -ne 'salty-demo 1.1.0 ready') { Fail "1.1.0 second start: $($r.out)" }
Ok '1.1.0 MSI downloaded, verified, installed per user by msiexec, trial-booted and committed'

# Broken 1.2.0: two failed starts, then the launcher reinstalls the kept 1.1.0 MSI.
& "$T\saltyctl.exe" publish "$T\m2.cbor" --dir "$T\pkg-1.2.0" --server http://127.0.0.1:18471 *> $null
$r = RunApp "update --installer-arg /qn --installer-arg /l*v --installer-arg $O\u120.log"
if ($r.code -ne 0) { Fail "update to 1.2.0: $($r.out)" }
if (-not (WaitVersion '1.2.0')) { Fail 'msiexec did not upgrade to 1.2.0' }
foreach ($n in 1, 2) {
  $r = RunApp 'run'
  if ($r.code -eq 0 -or $r.out -notmatch '1.2.0 is a broken build') { Fail "broken start $n`: $($r.code) $($r.out)" }
}
$r = RunApp 'run'
if ($r.code -ne 0 -or $r.out -notmatch 'reinstalling the previous version') { Fail "rollback start: $($r.code) $($r.out)" }
if (-not (WaitVersion '1.1.0')) { Fail 'the kept 1.1.0 MSI was not reinstalled' }
$r = RunApp 'run'; if ($r.out -ne 'salty-demo 1.1.0 ready') { Fail "after rollback: $($r.code) $($r.out)" }
$r = RunApp 'update'; if ($r.code -eq 0 -or $r.out -notmatch 'bad') { Fail "rolled-back update offered again: $($r.out)" }
Ok 'broken 1.2.0 failed twice; the launcher reinstalled the kept 1.1.0 MSI (downgrade) and 1.2.0 is refused'

# Uninstall and check nothing is left.
$pc = (Entry).Code
$c = Msi @('/x', $pc, '/qn', '/norestart', '/l*v', "$O\x.log")
if ($c -ne 0) { Fail "uninstall exited $c" }
if ((Entry) -or (Test-Path "$env:LOCALAPPDATA\Programs\SaltyDemo") -or (Test-Path 'HKCU:\Software\WeirdBrains\SaltyDemo')) { Fail 'uninstall left files or keys' }
Ok 'uninstalled per user: no product registration, files or registry keys left'
Stop-Server
Remove-Item -Recurse -Force "$env:TEMP\salty-msi-test" -ErrorAction SilentlyContinue
Log 'DONE'
