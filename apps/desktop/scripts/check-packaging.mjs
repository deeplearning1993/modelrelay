import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const desktopDirectory = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const tauriDirectory = resolve(desktopDirectory, "src-tauri");
const config = JSON.parse(
  readFileSync(resolve(tauriDirectory, "tauri.conf.json"), "utf8"),
);
const hooks = readFileSync(resolve(tauriDirectory, "nsis-hooks.nsh"), "utf8");
const preparation = readFileSync(
  resolve(desktopDirectory, "scripts", "prepare-sidecar.mjs"),
  "utf8",
);

assert.equal(config.build.beforeBuildCommand, "npm run build:bundle");
assert.deepEqual(config.bundle.externalBin, [
  "binaries/cmr",
  "binaries/cmr-service",
]);
assert.equal(config.bundle.windows.nsis.installerHooks, "nsis-hooks.nsh");
assert.equal(config.productName, "ModelRelay");
assert.match(preparation, /process\.env\.TAURI_ENV_TARGET_TRIPLE/);
assert.match(preparation, /process\.env\.CARGO/);
assert.match(preparation, /"--locked"/);
assert.match(preparation, /\["cmr", "cmr-service"\]/);
assert.match(preparation, /`\$\{binary\}-\$\{triple\}\$\{extension\}`/);

function macroBody(name) {
  const match = hooks.match(
    new RegExp(`!macro ${name}\\r?\\n([\\s\\S]*?)!macroend`),
  );
  assert.ok(match, `${name} must be defined`);
  return match[1];
}

const preinstall = macroBody("NSIS_HOOK_PREINSTALL");
const postinstall = macroBody("NSIS_HOOK_POSTINSTALL");
const preuninstall = macroBody("NSIS_HOOK_PREUNINSTALL");
const exactTaskProbe =
  'nsExec::ExecToStack \'"$SYSDIR\\schtasks.exe" /Query /TN "ModelRelay" /XML\'';
const legacyProductStatus =
  'nsExec::ExecToStack \'"$LOCALAPPDATA\\Codex Model Router\\cmr.exe" service status\'';
const legacyServiceCleanup =
  'ExecWait \'"$LOCALAPPDATA\\Codex Model Router\\cmr.exe" service uninstall\'';
const legacyServiceRestore =
  'ExecWait \'"$LOCALAPPDATA\\Codex Model Router\\cmr.exe" service install\'';
const ownedServiceProbe =
  'nsExec::ExecToStack \'"$INSTDIR\\cmr.exe" service status\'';
const serviceCleanup = 'ExecWait \'"$INSTDIR\\cmr.exe" service uninstall\'';
const serviceRestore = 'ExecWait \'"$INSTDIR\\cmr.exe" service install\'';
const codexCleanup = 'ExecWait \'"$INSTDIR\\cmr.exe" codex uninstall\'';

assert.match(hooks, /Var CmrUpgradeServiceWasInstalled/);
assert.match(hooks, /Var CmrUpgradeHadServiceBinary/);
assert.match(hooks, /Var CmrLegacyProductServiceRemoved/);
assert.match(preinstall, /StrCpy \$CmrUpgradeServiceWasInstalled "0"/);
assert.match(preinstall, /StrCpy \$CmrLegacyProductServiceRemoved "0"/);
assert.match(
  preinstall,
  /IfFileExists "\$INSTDIR\\cmr\.exe" cmr_upgrade_probe_service cmr_upgrade_probe_legacy_product/,
);
assert.ok(
  preinstall.includes(ownedServiceProbe),
  "an existing install must classify its task through the ownership-checking CLI",
);
assert.ok(
  preinstall.includes(legacyProductStatus),
  "the renamed product must be classified through its own old CLI",
);
assert.match(preinstall, /StrCmp \$2 "installed" cmr_legacy_remove_service/);
assert.ok(
  preinstall.includes(legacyServiceCleanup),
  "the legacy product service must be removed by its own old CLI",
);
assert.match(
  preinstall,
  /StrCpy \$CmrLegacyProductServiceRemoved "1"/,
);
assert.match(preinstall, /StrCmp \$2 "installed" cmr_upgrade_remove_service/);
assert.match(
  preinstall,
  /StrCmp \$2 "not-installed" cmr_upgrade_preinstall_done cmr_upgrade_probe_failed/,
);
assert.ok(
  preinstall.includes(exactTaskProbe),
  "a first install must refuse a visible orphaned exact-name task",
);
assert.ok(
  preinstall.includes(serviceCleanup),
  "an existing owned service must be removed before replacing cmr.exe",
);
assert.match(preinstall, /IfErrors cmr_upgrade_remove_failed/);
assert.match(
  preinstall,
  /Rename "\$INSTDIR\\cmr\.exe" "\$INSTDIR\\cmr\.exe\.cmr-upgrade-backup"/,
);
assert.match(
  preinstall,
  /Rename "\$INSTDIR\\cmr-service\.exe" "\$INSTDIR\\cmr-service\.exe\.cmr-upgrade-backup"/,
);
assert.match(preinstall, /IntCmp \$3 40 cmr_upgrade_lock_timeout/);
assert.match(preinstall, /StrCpy \$CmrUpgradeServiceWasInstalled "1"/);
assert.match(preinstall, /cmr_upgrade_probe_failed:\r?\n\s+Abort /);
assert.match(preinstall, /cmr_upgrade_sidecar_missing:\r?\n\s+Abort /);
assert.match(preinstall, /cmr_upgrade_remove_failed:\r?\n\s+Abort /);
assert.match(preinstall, /cmr_legacy_remove_failed:\r?\n\s+Abort /);

// An upgrade verifies the freshly installed sidecars before a removed service
// is recreated: the new cmr.exe must exist, run `--version`, and report the
// installer's own version. Only then is the flag-gated service restored. The
// legacy-product flag drives the same verification so the migrated service is
// recreated immediately after the files land.
assert.match(
  postinstall,
  /StrCmp \$CmrUpgradeServiceWasInstalled "1" cmr_upgrade_verify_new_sidecars 0/,
);
assert.match(
  postinstall,
  /StrCmp \$CmrLegacyProductServiceRemoved "1" cmr_upgrade_verify_new_sidecars cmr_upgrade_postinstall_done/,
);
assert.match(
  postinstall,
  /cmr_upgrade_verify_new_sidecars:\r?\n\s+IfFileExists "\$INSTDIR\\cmr\.exe" 0 cmr_upgrade_restore_failed/,
);
assert.match(postinstall, /IfFileExists "\$INSTDIR\\cmr-service\.exe" 0 cmr_upgrade_restore_failed/);
assert.match(postinstall, /nsExec::ExecToStack '\"\$INSTDIR\\cmr\.exe\" --version'/);
assert.match(postinstall, /StrCmp \$0 "0" 0 cmr_upgrade_restore_failed/);
assert.match(postinstall, /StrCpy \$2 "cmr \$\{VERSION\}"/);
assert.match(
  postinstall,
  /StrCmp \$4 \$2 cmr_upgrade_restore_service cmr_upgrade_restore_failed/,
);

assert.match(
  postinstall,
  /cmr_upgrade_restore_service:\r?\n\s+ClearErrors\r?\n\s+ExecWait '\"\$INSTDIR\\cmr\.exe\" service install'/,
);
assert.ok(
  postinstall.includes(serviceRestore),
  "post-install must restore a service removed by pre-install",
);
assert.match(
  postinstall,
  /cmr_upgrade_restore_succeeded:\r?\n\s+Delete \/REBOOTOK "\$INSTDIR\\cmr\.exe\.cmr-upgrade-backup"/,
);
assert.match(postinstall, /Delete \/REBOOTOK "\$INSTDIR\\cmr-service\.exe\.cmr-upgrade-backup"/);
assert.match(postinstall, /IfErrors cmr_upgrade_restore_failed/);
// The restore-failed label begins with best-effort cleanup before its Abort, so
// the abort may be preceded by other statements under the same label.
assert.match(postinstall, /cmr_upgrade_restore_failed:\r?\n[\s\S]*?Abort /);
assert.match(postinstall, /cmr_legacy_restore_old_service:\r?\n[\s\S]*?Abort /);
assert.ok(
  postinstall.includes(legacyServiceRestore),
  "the legacy migration rollback must re-register the old service",
);
// `service install` appears in PREINSTALL only in the lock-timeout rollback
// (never on the first-install path), and in POSTINSTALL in the flag-gated
// restore and the staged-upgrade rollback; the legacy rollback re-registers
// the old service from the old directory instead.
assert.equal(
  preinstall.split(serviceRestore).length - 1,
  1,
  "pre-install may only run service install during the lock-timeout rollback",
);
assert.equal(
  postinstall.split(serviceRestore).length - 1,
  2,
  "post-install runs service install once for the restore and once for the rollback",
);
assert.equal(
  preinstall.split(legacyServiceRestore).length - 1,
  0,
  "pre-install must not re-register the legacy service",
);
assert.equal(
  postinstall.split(legacyServiceRestore).length - 1,
  1,
  "post-install re-registers the legacy service only in the rollback",
);

const serviceIndex = preuninstall.indexOf(serviceCleanup);
const codexIndex = preuninstall.indexOf(codexCleanup);
assert.ok(serviceIndex >= 0, "pre-uninstall must invoke the installed cmr sidecar");
assert.ok(codexIndex > serviceIndex, "Codex cleanup must run after service cleanup");
assert.match(preuninstall, /IfErrors cmr_service_uninstall_failed/);
assert.match(preuninstall, /IfErrors cmr_codex_uninstall_failed/);
assert.match(preuninstall, /cmr_service_uninstall_failed:\r?\n\s+Abort /);
assert.match(preuninstall, /cmr_codex_uninstall_failed:\r?\n\s+Abort /);
assert.ok(
  !preinstall.includes(codexCleanup) && !postinstall.includes(codexCleanup),
  "an in-place upgrade must preserve the Codex integration",
);
assert.ok(
  !preinstall.includes("Codex Model Router\\\" codex uninstall"),
  "the legacy product cleanup must never run codex uninstall",
);

console.log("Desktop sidecar and NSIS install/upgrade/uninstall hooks are internally consistent.");
