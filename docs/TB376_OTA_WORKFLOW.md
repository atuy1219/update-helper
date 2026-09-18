# TB376FC ZUI OTA workflow

This procedure applies to a TB376FC that already boots TB390FU ROW firmware and
has an officially unlocked bootloader plus KernelSU Next LKM. The normal
steady-state assumption is that the Qualcomm A/B firmware partitions are also
TB390FU ROW. The helper does not intentionally roll those low-level partitions
back to TB376FC after an OTA.

## Before starting

- Export the previous operation backup through Android's Storage Access
  Framework.
- Keep a separate full Fastboot/EDL recovery set.
- Charge to at least 50%, or keep external power connected above 30%.
- Do not relock the bootloader.
- KernelSU Next's original stock backup in `/data/adb/ksu/` must still exist.
  Differential OTA preparation deliberately refuses KernelSU's reconstructed
  fallback image because byte-for-byte stock equality cannot be proven from it.
- Treat TB390FU ROW Qualcomm firmware as part of the stock source baseline. If
  any low-level partition was manually changed, restore that exact build before
  starting a differential OTA; the normal Helper flow does not convert a mixed
  TB376FC/TB390FU low-level layout into ROW.

## Differential/incremental ZUI OTA

1. Before downloading/applying the OTA, open TB376 OTA Helper and select
   **Inspect device**.
2. Confirm current slot equals next boot slot. If they differ, an OTA is already
   pending and current-slot stock preparation is refused.
3. Select **Prepare incremental OTA (restore stock)**.
4. The helper asks KernelSU Next to process the current `init_boot_<slot>` first
   (falling back to current `boot_<slot>` only when appropriate). The operation
   is accepted only when KernelSU reports that it used the exact
   `/data/adb/ksu/ksun_backup_<sha1>` backup embedded in the patched image.
5. The helper verifies the backup SHA-1 identity, verifies candidate and backup
   SHA-256 equality, writes the exact stock image, and verifies the whole target
   partition by SHA-256 read-back.
6. The helper then restores current `vendor_boot_<slot>` only from the exact
   Helper-generated ROW/PRC backup pair matching the currently installed PRC
   partition, followed by full-partition SHA-256 read-back verification.
7. Confirm the UI reports **Incremental OTA preparation complete** and
   `vendor_boot` region is `ROW`.
8. Do **not** reboot. Start/apply the ZUI differential OTA immediately.
9. When ZUI asks to restart, stop. Do **not** reboot.
10. Open KernelSU Next Manager and select **Install to inactive slot**. Wait for
    success.
11. Return to TB376 OTA Helper and select **Inspect device**.
12. Confirm current slot and next boot slot differ, and the target is only
    `vendor_boot_<next slot>`.
13. Run **Dry Run**. Review the three changed FDT offsets and both hashes.
14. Approve **Back up and patch update-target vendor_boot**.
15. Wait for full-partition read-back SHA-256 verification.
16. Export the backup to a user-selected SAF folder.
17. Keep the Qualcomm firmware produced by the TB390FU ROW OTA. Do not restore
    the previous TB376FC low-level firmware as part of the normal post-OTA flow.
18. Only after both KernelSU and vendor_boot post-OTA steps succeeded, explicitly
    approve **Safe reboot**.

## Why KernelSU's rebuilt fallback is rejected

KernelSU Next can remove its ramdisk files and repack a patched image even when
the original stock backup is missing. That is useful for ordinary recovery, but
a differential OTA may validate or consume source partition bytes. Repacking
can produce an image that is logically equivalent without being byte-identical
to Lenovo's original source. TB376 OTA Helper therefore proceeds only when the
real KernelSU stock backup exists and the candidate is SHA-256 identical to it.

The helper stores its verified KernelSU candidate and metadata under
`/data/adb/tb376-ota-helper/ksu-ota-prep/` so an interrupted write can be retried
without selecting a different source image.

## Already stock / missing backup

If a previously verified Helper candidate for the same slot and build
fingerprint already matches the current `init_boot`/`boot` SHA-256, the KernelSU
part of preparation is accepted as already stock. Otherwise, if KernelSU's exact
stock backup cannot be proven, preparation fails closed.

If current `vendor_boot` is already a supported ROW image, no vendor_boot write
is necessary. If it is PRC, its complete SHA-256 must match a previous
Helper-generated PRC artifact with the corresponding ROW stock backup.

Some Lenovo partitions are exposed by the running kernel with the block-device
read-only bit set. For Helper-owned `vendor_boot` and KernelSU stock-restore
writes, the original RO state is recorded, temporarily cleared only around the
verified write, and restored afterward. A failure to restore that state is
treated as an operation failure rather than ignored.

## Already PRC after OTA

If the next-slot `vendor_boot` image already has all three root
`region,country=PRC` values, the post-OTA helper does not rewrite it. It still
validates the complete FDT set and records the full partition SHA-256 as
`success_already_prc`.

## What must never happen

- Do not change or flash modified vbmeta. Hardware testing showed flags=3,
  damaged-signature, and unsigned algorithm-NONE vbmeta all mark the slot
  unbootable.
- Do not reboot between successful pre-OTA stock preparation and starting the
  ZUI OTA.
- Do not manually select another block device or stock image.
- Do not reboot while a write/restore operation is incomplete or after a
  read-back verification failure.

Google Play system updates normally do not update `vendor_boot`, so this helper
is not normally required for them.
