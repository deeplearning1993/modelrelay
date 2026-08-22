; This flag lives for the duration of one installer process. A fresh install
; leaves it at zero, while an upgrade sets it only after the old sidecar has
; safely removed an owned, previously registered service.
Var CmrUpgradeServiceWasInstalled
Var CmrUpgradeHadServiceBinary
; Set when the renamed "Codex Model Router" product was detected in its old
; install directory and its scheduled task was removed by that old CLI.
Var CmrLegacyProductServiceRemoved

; Stop and unregister an existing service before NSIS replaces cmr.exe. The
; installed CLI queries the exact `ModelRelay` task and verifies its
; ownership marker, executable and arguments. An unknown or concurrently
; replaced task therefore aborts the upgrade instead of being deleted.
!macro NSIS_HOOK_PREINSTALL
  StrCpy $CmrUpgradeServiceWasInstalled "0"
  StrCpy $CmrUpgradeHadServiceBinary "0"
  StrCpy $CmrLegacyProductServiceRemoved "0"

  ; An existing install must classify its own task through the old CLI before
  ; NSIS overwrites any executable. The CLI already performs localized OEM
  ; decoding plus strict owner/action verification, so an access error or an
  ; unknown same-name task cannot be mistaken for a fresh install.
  IfFileExists "$INSTDIR\cmr.exe" cmr_upgrade_probe_service cmr_upgrade_probe_legacy_product

cmr_upgrade_probe_legacy_product:
  ; This installer may upgrade the renamed "Codex Model Router" product,
  ; whose files live in a different per-user directory. Its old CLI must
  ; remove the old scheduled task before the new service can bind the port.
  IfFileExists "$LOCALAPPDATA\Codex Model Router\cmr.exe" cmr_legacy_probe_service cmr_upgrade_probe_orphan_task

cmr_legacy_probe_service:
  ClearErrors
  nsExec::ExecToStack '"$LOCALAPPDATA\Codex Model Router\cmr.exe" service status'
  Pop $0
  Pop $1
  IfErrors cmr_upgrade_probe_failed
  StrCmp $0 "0" cmr_legacy_classify_service cmr_upgrade_probe_failed

cmr_legacy_classify_service:
  StrCpy $2 $1 9
  StrCmp $2 "installed" cmr_legacy_remove_service 0
  StrCpy $2 $1 13
  StrCmp $2 "not-installed" cmr_upgrade_probe_orphan_task cmr_upgrade_probe_failed

cmr_legacy_remove_service:
  ClearErrors
  ExecWait '"$LOCALAPPDATA\Codex Model Router\cmr.exe" service uninstall' $0
  IfErrors cmr_legacy_remove_failed
  StrCmp $0 "0" cmr_legacy_service_removed cmr_legacy_remove_failed

cmr_legacy_service_removed:
  StrCpy $CmrLegacyProductServiceRemoved "1"
  Goto cmr_upgrade_probe_orphan_task

cmr_legacy_remove_failed:
  Abort "ModelRelay could not safely stop the previous Codex Model Router background service. The upgrade was not started."

cmr_upgrade_probe_service:
  ClearErrors
  nsExec::ExecToStack '"$INSTDIR\cmr.exe" service status'
  Pop $0
  Pop $1
  IfErrors cmr_upgrade_probe_failed
  StrCmp $0 "0" cmr_upgrade_classify_service cmr_upgrade_probe_failed

cmr_upgrade_classify_service:
  ; The CLI's first stable output line is one of these two ASCII values.
  ; Reject all other output instead of guessing across localized systems.
  StrCpy $2 $1 9
  StrCmp $2 "installed" cmr_upgrade_remove_service
  StrCpy $2 $1 13
  StrCmp $2 "not-installed" cmr_upgrade_preinstall_done cmr_upgrade_probe_failed

cmr_upgrade_probe_orphan_task:
  ; A first install has no old CLI. Refuse an exact pre-existing task when it is
  ; visible to Task Scheduler; otherwise leave service creation to one-click
  ; setup after the files are installed.
  ClearErrors
  nsExec::ExecToStack '"$SYSDIR\schtasks.exe" /Query /TN "ModelRelay" /XML'
  Pop $0
  Pop $1
  IfErrors cmr_upgrade_probe_failed
  StrCmp $0 "0" cmr_upgrade_sidecar_missing cmr_upgrade_preinstall_done

cmr_upgrade_remove_service:
  ; Refuse an ambiguous residue from an interrupted older upgrade before
  ; changing the running service. The normal success path removes both files.
  IfFileExists "$INSTDIR\cmr.exe.cmr-upgrade-backup" cmr_upgrade_backup_exists
  IfFileExists "$INSTDIR\cmr-service.exe.cmr-upgrade-backup" cmr_upgrade_backup_exists

  ClearErrors
  ExecWait '"$INSTDIR\cmr.exe" service uninstall' $0
  IfErrors cmr_upgrade_remove_failed
  StrCmp $0 "0" cmr_upgrade_remove_succeeded cmr_upgrade_remove_failed

cmr_upgrade_remove_succeeded:
  ; Task Scheduler can return before Windows releases the executable image.
  ; Renaming is the hard lock-release gate: NSIS must never continue to File
  ; replacement while either old sidecar is still mapped by the old service.
  StrCpy $3 "0"

cmr_upgrade_stage_cli:
  ClearErrors
  Rename "$INSTDIR\cmr.exe" "$INSTDIR\cmr.exe.cmr-upgrade-backup"
  IfErrors cmr_upgrade_retry_cli cmr_upgrade_stage_service_if_present

cmr_upgrade_retry_cli:
  IntOp $3 $3 + 1
  IntCmp $3 40 cmr_upgrade_lock_timeout cmr_upgrade_retry_cli_delay cmr_upgrade_lock_timeout

cmr_upgrade_retry_cli_delay:
  Sleep 100
  Goto cmr_upgrade_stage_cli

cmr_upgrade_stage_service_if_present:
  IfFileExists "$INSTDIR\cmr-service.exe" cmr_upgrade_prepare_service_stage cmr_upgrade_old_sidecars_staged

cmr_upgrade_prepare_service_stage:
  StrCpy $CmrUpgradeHadServiceBinary "1"
  StrCpy $3 "0"

cmr_upgrade_stage_service:
  ClearErrors
  Rename "$INSTDIR\cmr-service.exe" "$INSTDIR\cmr-service.exe.cmr-upgrade-backup"
  IfErrors cmr_upgrade_retry_service cmr_upgrade_old_sidecars_staged

cmr_upgrade_retry_service:
  IntOp $3 $3 + 1
  IntCmp $3 40 cmr_upgrade_lock_timeout cmr_upgrade_retry_service_delay cmr_upgrade_lock_timeout

cmr_upgrade_retry_service_delay:
  Sleep 100
  Goto cmr_upgrade_stage_service

cmr_upgrade_old_sidecars_staged:
  StrCpy $CmrUpgradeServiceWasInstalled "1"
  Goto cmr_upgrade_preinstall_done

cmr_upgrade_backup_exists:
  Abort "ModelRelay found an unresolved executable backup from an earlier upgrade. No service or application files were changed."

cmr_upgrade_lock_timeout:
  ; Put back every file that was successfully staged, then restore the old
  ; owned task. This keeps a transient image lock from becoming an outage.
  IfFileExists "$INSTDIR\cmr.exe.cmr-upgrade-backup" 0 +2
    Rename "$INSTDIR\cmr.exe.cmr-upgrade-backup" "$INSTDIR\cmr.exe"
  IfFileExists "$INSTDIR\cmr-service.exe.cmr-upgrade-backup" 0 +2
    Rename "$INSTDIR\cmr-service.exe.cmr-upgrade-backup" "$INSTDIR\cmr-service.exe"
  ClearErrors
  ExecWait '"$INSTDIR\cmr.exe" service install' $0
  Abort "ModelRelay could not obtain an exclusive executable lock. The previous service was restored and the upgrade was stopped."

cmr_upgrade_probe_failed:
  Abort "ModelRelay could not safely inspect the existing background service. The upgrade was not started."

cmr_upgrade_sidecar_missing:
  Abort "The existing ModelRelay background service has no installed manager. The upgrade was not started."

cmr_upgrade_remove_failed:
  Abort "ModelRelay could not safely stop and unregister its existing background service. The upgrade was not started."

cmr_upgrade_preinstall_done:
!macroend

; Restore only a service that PREINSTALL actually removed. At this point NSIS
; has copied the new cmr.exe, so the recreated task always uses the new binary.
!macro NSIS_HOOK_POSTINSTALL
  StrCmp $CmrUpgradeServiceWasInstalled "1" cmr_upgrade_verify_new_sidecars 0
  StrCmp $CmrLegacyProductServiceRemoved "1" cmr_upgrade_verify_new_sidecars cmr_upgrade_postinstall_done

cmr_upgrade_verify_new_sidecars:
  IfFileExists "$INSTDIR\cmr.exe" 0 cmr_upgrade_restore_failed
  IfFileExists "$INSTDIR\cmr-service.exe" 0 cmr_upgrade_restore_failed
  ClearErrors
  nsExec::ExecToStack '"$INSTDIR\cmr.exe" --version'
  Pop $0
  Pop $1
  IfErrors cmr_upgrade_restore_failed
  StrCmp $0 "0" 0 cmr_upgrade_restore_failed
  StrCpy $2 "cmr ${VERSION}"
  StrLen $3 $2
  StrCpy $4 $1 $3
  StrCmp $4 $2 cmr_upgrade_restore_service cmr_upgrade_restore_failed

cmr_upgrade_restore_service:
  ClearErrors
  ExecWait '"$INSTDIR\cmr.exe" service install' $0
  IfErrors cmr_upgrade_restore_failed
  StrCmp $0 "0" cmr_upgrade_restore_succeeded cmr_upgrade_restore_failed

cmr_upgrade_restore_succeeded:
  Delete /REBOOTOK "$INSTDIR\cmr.exe.cmr-upgrade-backup"
  Delete /REBOOTOK "$INSTDIR\cmr-service.exe.cmr-upgrade-backup"
  Goto cmr_upgrade_postinstall_done

cmr_upgrade_restore_failed:
  ; The legacy-product migration has no staged backups in the new directory;
  ; its rollback re-registers the old service from the old install directory.
  StrCmp $CmrLegacyProductServiceRemoved "1" cmr_legacy_restore_old_service 0
  ; Best-effort removal covers a partially created new task. Restore the exact
  ; old binaries and ask the old ownership-checking CLI to recreate its task.
  IfFileExists "$INSTDIR\cmr.exe" 0 cmr_upgrade_restore_old_files
    ExecWait '"$INSTDIR\cmr.exe" service uninstall' $0

cmr_upgrade_restore_old_files:
  Delete "$INSTDIR\cmr.exe"
  Delete "$INSTDIR\cmr-service.exe"
  IfFileExists "$INSTDIR\cmr.exe.cmr-upgrade-backup" 0 cmr_upgrade_restore_irrecoverable
  Rename "$INSTDIR\cmr.exe.cmr-upgrade-backup" "$INSTDIR\cmr.exe"
  IfErrors cmr_upgrade_restore_irrecoverable
  StrCmp $CmrUpgradeHadServiceBinary "1" 0 cmr_upgrade_restore_old_service
  IfFileExists "$INSTDIR\cmr-service.exe.cmr-upgrade-backup" 0 cmr_upgrade_restore_irrecoverable
  Rename "$INSTDIR\cmr-service.exe.cmr-upgrade-backup" "$INSTDIR\cmr-service.exe"
  IfErrors cmr_upgrade_restore_irrecoverable

cmr_upgrade_restore_old_service:
  ClearErrors
  ExecWait '"$INSTDIR\cmr.exe" service install' $0
  Abort "ModelRelay could not activate the updated background service. The previous version was restored."

cmr_legacy_restore_old_service:
  ClearErrors
  ExecWait '"$LOCALAPPDATA\Codex Model Router\cmr.exe" service install' $0
  Abort "ModelRelay could not activate the updated background service. The previous Codex Model Router version was restored."

cmr_upgrade_restore_irrecoverable:
  Abort "ModelRelay could not activate the update or automatically restore the previous executable. Application data and credentials were not changed."

cmr_upgrade_postinstall_done:
!macroend

; Formal uninstall keeps the existing service-then-Codex cleanup order.
!macro NSIS_HOOK_PREUNINSTALL
  ClearErrors
  ExecWait '"$INSTDIR\cmr.exe" service uninstall' $0
  IfErrors cmr_service_uninstall_failed
  IntCmp $0 0 cmr_service_uninstall_succeeded cmr_service_uninstall_failed cmr_service_uninstall_failed

cmr_service_uninstall_failed:
  Abort "ModelRelay could not safely unregister its background service. No application files were removed."

cmr_service_uninstall_succeeded:
  ClearErrors
  ExecWait '"$INSTDIR\cmr.exe" codex uninstall' $0
  IfErrors cmr_codex_uninstall_failed
  IntCmp $0 0 cmr_uninstall_cleanup_done cmr_codex_uninstall_failed cmr_codex_uninstall_failed

cmr_codex_uninstall_failed:
  Abort "ModelRelay could not safely restore the Codex configuration. No application files were removed."

cmr_uninstall_cleanup_done:
!macroend
