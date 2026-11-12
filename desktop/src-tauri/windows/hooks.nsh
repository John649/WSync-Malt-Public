; NSIS hooks for the Windows installer (bundle.windows.nsis.installerHooks).
;
; The app ships the engine as its sidecar ($INSTDIR\wsync.exe). Agents and
; terminals call `wsync` by name, so after the files are in place the sidecar
; installs itself to %USERPROFILE%\.wsync\bin and adds that to PATH — the same
; thing `wsync install` does by hand. --yes because there is no terminal to
; answer the prompt. A failure here is reported, never fatal: the app works
; without the CLI on PATH.

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Adding wsync to PATH..."
  nsExec::ExecToLog '"$INSTDIR\wsync.exe" --yes install'
  Pop $0
  ${If} $0 != 0
    DetailPrint "wsync could not be added to PATH (exit $0). Run `wsync install` from the install folder later."
  ${EndIf}
!macroend
