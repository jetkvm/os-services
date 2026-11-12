; The installer registers and starts the service for the user who runs it,
; and the uninstaller removes that registration. Before files change, the
; installed program stops its own service, so the installer does not find
; it running.

!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    nsExec::Exec '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-service'
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --install-service'
  Pop $0
  ${If} $0 != 0
    DetailPrint "The service did not start. Open IP-KVM OS Services from the Start menu to try again."
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-service'
  Pop $0
  ${If} $0 != 0
    DetailPrint "The service registration was not removed. Delete the IP-KVM OS Services task in Task Scheduler."
  ${EndIf}
!macroend
