; Tauri's NSIS template skips the desktop shortcut when `$UpdateMode = 1`, so an
; in-place upgrade leaves the user without the icon they had before. The skip is
; meant to avoid re-adding something the user deliberately deleted, but for an
; upgrade over our own previous install the expectation is the opposite.
;
; This only runs in update mode: a fresh install already gets the shortcut, via
; the finish page checkbox (checked by default) or the silent-install path.

!macro NSIS_HOOK_POSTINSTALL
  ${If} $UpdateMode = 1
    CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
  ${EndIf}
!macroend
