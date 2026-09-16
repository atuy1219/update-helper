# TB376 OTA Helper

TB376 OTA Helper is an offline Android application for one fixed device
profile: an officially unlocked Lenovo TB376FC (`product=malbec`,
`hwboardid=SM8735P_8+128_22`) running TB390FU/TB390FU_PRC ROW ZUI with KernelSU
Next LKM. KernelSU Next's **SU compatibility** setting must be enabled so the
app can request its explicitly approved root process.

Before starting a differential/incremental ZUI A/B OTA, use **Prepare incremental
OTA (restore stock)**. The app first asks KernelSU Next to restore the currently
running slot's patched `init_boot` (or `boot` when that is the selected KernelSU
partition). It accepts the result only when KernelSU identifies a real
`/data/adb/ksu/ksun_backup_<sha1>` stock backup and the generated candidate is
byte-identical to that backup by SHA-256. KernelSU's rebuild-without-KSU fallback
is deliberately rejected because it is not guaranteed to match the stock source
bytes required by a differential OTA.

The same preparation then restores the currently running slot's stock
`vendor_boot` only when that slot exactly matches a PRC image that this Helper
previously generated and the paired stock backup still exists. Both restored
partitions are read back and SHA-256 verified. Once an OTA is pending, these
active-slot restore paths are refused.

After the OTA has finished installing and the system is waiting for a reboot,
the normal flow backs up and patches only the next boot slot's
`vendor_boot_<slot>`. It never writes `vbmeta` or `super`. The current-slot
`init_boot`/`boot` is writable only by the dedicated pre-OTA KernelSU stock
restore path, and the active `vendor_boot` is writable only by the dedicated,
hash-matched pre-OTA stock-restore path.

The vendor_boot patch is the fixed LTBox Tuna/Tunap transformation:

- exactly 3 supported FDTs;
- exactly 2 root `compatible` lists containing `qcom,tuna`;
- exactly 1 containing `qcom,tunap`;
- only root `region,country` changes from `ROW\0` to `PRC\0`;
- exactly 9 changed bytes and identical image size.

See [docs/TB376_OTA_HELPER.md](docs/TB376_OTA_HELPER.md) for design and build
details, [docs/TB376_OTA_WORKFLOW.md](docs/TB376_OTA_WORKFLOW.md) before every
OTA, and [docs/TB376_RECOVERY.md](docs/TB376_RECOVERY.md) before recovery.

## Correct differential OTA order

1. Inspect the device while the current slot is still the next boot slot.
2. Run **Prepare incremental OTA (restore stock)**. It must verify the KernelSU
   stock backup and restore/verify current `init_boot` or `boot`, then restore
   and verify current `vendor_boot` as ROW.
3. Do **not** reboot. Start the ZUI OTA immediately after the helper reports the
   incremental-OTA-ready state.
4. When ZUI asks to restart, stop. Do **not** reboot.
5. In KernelSU Next Manager, install to the inactive slot.
6. Return to TB376 OTA Helper, inspect, run Dry Run, and patch only the update
   target `vendor_boot`.
7. Confirm full-partition read-back SHA-256 success and export the backup.
8. Only after both KernelSU and vendor_boot post-OTA steps succeeded, explicitly
   approve reboot in the app.

If KernelSU's exact stock backup is missing, or the current PRC `vendor_boot`
does not match a Helper-generated backup pair, preparation fails closed rather
than guessing or writing a reconstructed image.

Google Play system updates normally do not replace `vendor_boot` and do not
require this flow.

## Non-negotiable warnings

- Never modify vbmeta flags and never use unsigned or signature-damaged vbmeta.
- Never relock the bootloader after cross-model firmware has been installed.
- Never reboot after a write/restore error or an incomplete journal.
- Keep a user-exported backup and a separate full EDL recovery set.

The app requests no Internet, location, contact, phone, advertising ID, or
analytics permission. It sends no device information or logs off the tablet.
