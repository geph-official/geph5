# Hard recovery: restore internet even if geph5 broke it (kill switch / dead tun).
# Safe to run anytime. Does NOT rely on the daemon being responsive.
Write-Output 'killing geph processes...'
taskkill /IM geph5.exe /T /F 2>$null | Out-Null
taskkill /IM geph5-client.exe /T /F 2>$null | Out-Null
Start-Sleep -Milliseconds 500

# Delete the /1 split default routes that funnel all traffic into the (now dead) tun.
foreach ($p in '0.0.0.0/1','128.0.0.0/1') {
    netsh interface ipv4 delete route prefix=$p interface=Geph 2>$null | Out-Null
}
foreach ($p in '::/1','8000::/1') {
    netsh interface ipv6 delete route prefix=$p interface=Geph 2>$null | Out-Null
}

# Bring the Geph adapter down so any of its leftover routes/DNS stop attracting traffic.
try { Disable-NetAdapter -Name 'Geph' -Confirm:$false -ErrorAction Stop } catch {}

# Dynamic WFP filters auto-remove when the daemon dies; nothing to do there.
Write-Output 'recovery done. testing connectivity:'
& curl.exe -s --max-time 10 https://api.ipify.org
Write-Output ''
