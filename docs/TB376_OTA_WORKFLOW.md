# TB376FC ZUI OTA workflow

This procedure applies to a TB376FC that already boots TB390FU ROW firmware and
has an officially unlocked bootloader plus KernelSU Next LKM.

## Before starting

- Export the previous operation backup through Android's Storage Access
  Framework.
- Keep a separate full Fastboot/EDL recovery set.
- Charge to at least 50%, or keep external power connected above 30%.
- Do not relock the bootloader.

## Every full ZUI OTA

1. Install the ZUI OTA normally.
2. When ZUI asks to restart, stop. Do **not** reboot.
3. Open KernelSU Next Manager.
4. Select **Install to inactive slot** and wait for success.
5. Return to TB376 OTA Helper and select **Inspect device**.
6. Confirm current slot and next boot slot differ.
7. Confirm the target is only `vendor_boot_<next slot>`.
8. Run **Dry Run**. Review the three changed FDT offsets and both hashes.
9. Approve **Back up and patch update-target vendor_boot**.
10. Wait for full-partition read-back SHA-256 verification.
11. Export the backup to a user-selected SAF folder.
12. Only after both KernelSU and vendor_boot steps succeeded, explicitly approve
    **Safe reboot**.

The app does not patch or verify `init_boot`. If KernelSU's inactive-slot state
cannot be determined reliably, the UI reports “cannot confirm”; it never writes
`init_boot`.

## Already PRC

If the next-slot image already has all three root `region,country=PRC` values,
the helper does not rewrite it. It still validates the complete FDT set and
records the full partition SHA-256 as `success_already_prc`.

## What must never happen

- Do not change or flash modified vbmeta. Hardware testing showed flags=3,
  damaged-signature, and unsigned algorithm-NONE vbmeta all mark the slot
  unbootable.
- Do not patch the currently running slot.
- Do not select a block device manually.
- Do not reboot while the journal is incomplete or after a restore failure.

Google Play system updates normally do not update `vendor_boot`, so this helper
is not normally required for them.

