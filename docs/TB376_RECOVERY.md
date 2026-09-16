# TB376 OTA recovery

## App shows an unfinished journal

Do not reboot. The recovery screen appears before the normal UI. Choose the
stock restore action. It writes only the recorded update-slot `vendor_boot`,
calls `fsync`, and hashes the complete partition. Reboot remains disabled until
the restore is fully verified.

Root backups are stored under:

```text
/data/adb/tb376-ota-helper/backups/<timestamp>-slot-<a|b>/
```

Each folder contains stock and PRC images with SHA-256 sidecars plus
`operation.json`, `operation.log`, and `device-info.json`. User exports are
written to `TB376-OTA-Backups/<timestamp>-slot-<a|b>/` in the SAF-selected
folder.

## Restore failed

If the screen says:

```text
復元に失敗しました
再起動しないでください
FastbootまたはEDLで復旧が必要です
```

leave the tablet powered and do not request a normal reboot.

### Fastboot recovery

If Fastboot is reachable, flash the exact stock `vendor_boot` backup to the
failed inactive slot only. Confirm the slot letter from the saved
`operation.json`; never assume it is B.

```text
fastboot getvar current-slot
fastboot getvar active-slot
fastboot flash vendor_boot_<recorded-target> vendor_boot_<target>-stock.img
```

Verify that the image belongs to the recorded build and slot before flashing.
Do not flash vbmeta from a generated or patched artifact. The required vbmeta is
the official, byte-for-byte unmodified ROW image for that target build.

### EDL recovery

Use the existing LTBox TB376FC globalizer documentation and your previously
validated Firehose/programmer and full device backup. Restore only with a plan
whose GPT geometry and partition mapping match this TB376FC. Keep device-owned
partitions (`persist`, `proinfo`, modem state, FRP, lock/calibration data)
untouched.

For the fixed 18.0.10.335 cross-flash arrangement:

```text
vendor_boot_a  PRC-patched ROW vendor_boot
vbmeta_a       official unmodified ROW vbmeta from the ROM directory
```

Never use flags=3, unsigned, or signature-damaged vbmeta. Never relock the
bootloader.

