# TB376 OTA Helper

TB376 OTA Helper is an offline Android application for one fixed device
profile: an officially unlocked Lenovo TB376FC (`product=malbec`,
`hwboardid=SM8735P_8+128_22`) running TB390FU/TB390FU_PRC ROW ZUI with KernelSU
Next LKM. KernelSU Next's **SU compatibility** setting must be enabled so the
app can request its explicitly approved root process.

Before starting a normal ZUI A/B OTA, the app can restore the currently running
slot's stock `vendor_boot` only when that slot exactly matches a PRC image that
this Helper previously generated and the paired stock backup still exists.
Once an OTA is pending, that active-slot restore is refused.

After the OTA has finished installing and the system is waiting for a reboot,
the normal flow backs up and patches only the next boot slot's
`vendor_boot_<slot>`. It never writes `vbmeta`, `boot`, `init_boot`, or `super`.
The active `vendor_boot` is writable only by the dedicated, hash-matched
pre-OTA stock-restore path.

The patch is the fixed LTBox Tuna/Tunap transformation:

- exactly 3 supported FDTs;
- exactly 2 root `compatible` lists containing `qcom,tuna`;
- exactly 1 containing `qcom,tunap`;
- only root `region,country` changes from `ROW\0` to `PRC\0`;
- exactly 9 changed bytes and identical image size.

See [docs/TB376_OTA_HELPER.md](docs/TB376_OTA_HELPER.md) for design and build
details, [docs/TB376_OTA_WORKFLOW.md](docs/TB376_OTA_WORKFLOW.md) before every
OTA, and [docs/TB376_RECOVERY.md](docs/TB376_RECOVERY.md) before recovery.

## Correct OTA order

1. Inspect the device. If the current `vendor_boot` is PRC from the previous
   Helper cycle, use **Restore current OS stock vendor_boot** first.
2. Install the ZUI OTA.
3. Stop at “Restart required”; do not reboot.
4. In KernelSU Next Manager, install to the inactive slot.
5. Run Dry Run and patch the update target with TB376 OTA Helper.
6. Confirm full-partition read-back SHA-256 success.
7. Explicitly approve reboot in the app.

Google Play system updates normally do not replace `vendor_boot` and do not
require this flow.

## Non-negotiable warnings

- Never modify vbmeta flags and never use unsigned or signature-damaged vbmeta.
- Never relock the bootloader after cross-model firmware has been installed.
- Never reboot after a write/restore error or an incomplete journal.
- Keep a user-exported backup and a separate full EDL recovery set.

The app requests no Internet, location, contact, phone, advertising ID, or
analytics permission. It sends no device information or logs off the tablet.
