; Uninstalling puts folders back in Explorer's hands (Pink Cloud Files was opening them).
!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /IM pink-cloud.exe /F'
  DeleteRegKey HKCU "Software\Classes\Directory\shell\PinkCloud"
  DeleteRegKey HKCU "Software\Classes\Drive\shell\PinkCloud"
  ReadRegStr $0 HKCU "Software\Classes\Directory\shell" ""
  StrCmp $0 "PinkCloud" 0 +2
    DeleteRegValue HKCU "Software\Classes\Directory\shell" ""
  ReadRegStr $0 HKCU "Software\Classes\Drive\shell" ""
  StrCmp $0 "PinkCloud" 0 +2
    DeleteRegValue HKCU "Software\Classes\Drive\shell" ""
  nsExec::Exec 'schtasks /delete /tn "Pink Cloud" /f'
  ; the version kept for going back after a bad update (safe start)
  Delete "$INSTDIR\pink-cloud-previous.exe"
  Delete "$INSTDIR\pink-cloud-broken.exe"
  Delete "$INSTDIR\previous-version.txt"
  Delete "$INSTDIR\skip-version.txt"
!macroend
