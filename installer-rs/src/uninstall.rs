/// The uninstall script is the same `uninstall.ps1` the old installer wrote: it lives next to the launcher
/// and is started from "Apps & features". No extra exe is needed.
pub struct UninstallParams<'a> {
    pub product_name: &'a str,
    pub exe: &'a str,
    /// name of the HKCU\...\Uninstall key
    pub key_name: &'a str,
}

/// Escapes a string for PowerShell single quotes.
pub fn psq(value: &str) -> String {
    value.replace('\'', "''")
}

pub fn build_uninstall_script(params: &UninstallParams<'_>) -> String {
    let proc_name = params
        .exe
        .strip_suffix(".exe")
        .or_else(|| params.exe.strip_suffix(".EXE"))
        .unwrap_or(params.exe);
    let body = r#"param(
  [string]$InstallDir = '',
  [switch]$Relocated
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$Product  = '__PRODUCT__'
$ProcName = '__PROCNAME__'
$RegKey   = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\__KEYNAME__'
$Title    = 'Удаление ' + $Product
$nl       = [Environment]::NewLine

function Show([string]$text, [string]$buttons = 'OK', [string]$icon = 'Information', [string]$default = 'Button1') {
  return [System.Windows.Forms.MessageBox]::Show($text, $Title, $buttons, $icon, $default)
}

if (-not $Relocated) {
  # Скрипт удаляет свою же папку, поэтому продолжаем с копии во временной папке.
  $here = Split-Path -Parent $MyInvocation.MyCommand.Path
  $tmp  = Join-Path $env:TEMP ('chald-uninstall-' + [guid]::NewGuid().ToString('N') + '.ps1')
  Copy-Item -LiteralPath $MyInvocation.MyCommand.Path -Destination $tmp
  Start-Process -FilePath 'powershell.exe' -WindowStyle Hidden -ArgumentList @(
    '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"' + $tmp + '"'),
    '-Relocated', '-InstallDir', ('"' + $here + '"')
  )
  exit 0
}

# Удаляем только папку, которую создал наш установщик.
if (-not $InstallDir -or -not (Test-Path -LiteralPath (Join-Path $InstallDir '.chald-install.json'))) {
  Show ('Не найдена установленная копия ' + $Product + '.') 'OK' 'Error' | Out-Null
  exit 1
}

$ask = Show ('Удалить ' + $Product + ' с этого компьютера?' + $nl + $nl + $InstallDir) 'YesNo' 'Question' 'Button1'
if ($ask -ne 'Yes') { exit 0 }

$data = Join-Path $env:APPDATA $Product
$wipeData = $false
if (Test-Path -LiteralPath $data) {
  $a = Show ('Удалить также ваши данные (инстансы, миры, настройки, аккаунты)?' + $nl + $nl + $data + $nl + $nl + 'Если нажать «Нет», данные останутся, и после повторной установки всё будет на месте.') 'YesNoCancel' 'Warning' 'Button2'
  if ($a -eq 'Cancel') { exit 0 }
  $wipeData = ($a -eq 'Yes')
}

try {
  Get-Process -Name $ProcName -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) } |
    Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 700

  Remove-Item -LiteralPath (Join-Path ([Environment]::GetFolderPath('DesktopDirectory')) ($Product + '.lnk')) -Force -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath (Join-Path ([Environment]::GetFolderPath('Programs')) $Product) -Recurse -Force -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $RegKey -Recurse -Force -ErrorAction SilentlyContinue
  if ($wipeData) { Remove-Item -LiteralPath $data -Recurse -Force }
  Remove-Item -LiteralPath $InstallDir -Recurse -Force
} catch {
  Show ('Не удалось удалить всё до конца:' + $nl + $_.Exception.Message + $nl + $nl + 'Закройте ' + $Product + ' и запустите удаление ещё раз.') 'OK' 'Error' | Out-Null
  exit 1
}

Show ($Product + ' удалён.') | Out-Null
Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
"#;
    let script = body
        .replace("__PRODUCT__", &psq(params.product_name))
        .replace("__PROCNAME__", &psq(proc_name))
        .replace("__KEYNAME__", &psq(params.key_name));
    // Windows PowerShell 5 reads a .ps1 without a BOM as ANSI; Cyrillic needs UTF-8 BOM. Line endings: CRLF.
    let crlf = script.replace("\r\n", "\n").replace('\n', "\r\n");
    format!("\u{feff}{crlf}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_has_bom_crlf_and_escaped_quotes() {
        let script = build_uninstall_script(&UninstallParams { product_name: "Chald's Launcher", exe: "ChaldLauncher.exe", key_name: "ChaldLauncher" });
        assert!(script.starts_with('\u{feff}'));
        assert!(script.contains("Chald''s Launcher"));
        assert!(script.contains("-Relocated"));
        assert!(script.contains("uninstall"));
        assert!(!script.replace("\r\n", "").contains('\n'));
    }
}
