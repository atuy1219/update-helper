# TB376 OTA Helper design

## Supported device

Writes are enabled only when all of the following can be verified:

- root helper has effective UID 0 through KernelSU Next;
- `product=malbec`;
- `hwboardid` contains `SM8735P_8+128_22`, obtained from boot properties,
  `/proc/bootconfig`, or `/proc/cmdline`;
- Android model is `TB390FU` or `TB390FU_PRC`;
- `ro.boot.flash.locked=0` and verified-boot state is `orange` or vbmeta device
  state is `unlocked`;
- current slot is reported by `ro.boot.slot_suffix` and/or
  `bootctl get-current-slot`;
- next boot slot is reported by `bootctl get-active-boot-slot`;
- the allow-listed `/dev/block/by-name/vendor_boot_a` or
  `/dev/block/by-name/vendor_boot_b` resolves to a block device;
- its size is 64–256 MiB, 4096-byte aligned, and matches the complete image;
- the target has exactly the known Tuna/Tunap FDT set and a uniform ROW or PRC
  root region.

For normal OTA patching, the next boot slot must differ from the current slot.
For the dedicated pre-OTA stock restore, the two slots must be equal: the app
refuses that operation once an OTA-pending slot switch exists.

Any missing or contradictory value fails closed. Serial numbers are recorded
only in local operation metadata and are never hardcoded or transmitted.

## Process split

The Compose application never opens a block device through JNI. The APK carries
one arm64-v8a Rust executable under the native library directory. On first use
it is copied, as root, to:

```text
/data/adb/tb376-ota-helper/bin/tb376-ota-helper-native
```

Kotlin launches `su`, UID `0`, the executable path, and each argument as a
separate `ProcessBuilder` element. User text is never concatenated into a shell
command. The helper accepts:

```text
inspect --json
dry-run --slot a|b [--backup-dir FIXED_ROOT_CHILD] --json
patch --slot a|b [--backup-dir FIXED_ROOT_CHILD] --json
restore --slot a|b --backup FIXED_STOCK_PATH --json
restore-current-stock --json
verify --slot a|b --image FIXED_ARTIFACT_PATH --json
```

Partition names and paths are code-owned. There is no option for an arbitrary
block device. `vbmeta`, `boot`, `init_boot`, and `super` are never write
targets. The active `vendor_boot` remains inexpressible through the normal
`patch`/`restore` path; it is writable only through `restore-current-stock`,
which requires all of the dedicated checks below.

## Patch implementation

The parser is adapted from LTBox's
`patch_vendor_boot_region_country(...)`. It validates the FDT header, reserve,
structure, and strings bounds and walks structure tokens. Only properties at
root depth are considered. It does not perform a byte-string global replace.

The stock partition is copied with a 1 MiB buffer. SHA-256 is calculated while
streaming. The separate output file is memory-mapped; only nine known property
bytes are dirtied. The output is reparsed, its size is checked, and its SHA-256
is streamed. Input and output are never both held in memory. Typical resident
memory remains well below the complete 96 MiB partition size.

For ZUI 18.0.10.335, the known patched image is:

```text
size    100663296 bytes
SHA-256 C41CE9D1F33D11275972FFC1A3AB532069476049D3AC589AA0E54F86DAEDAA5C
```

The ROM image is not committed. Synthetic FDT fixtures exercise the exact
2-tuna/1-tunap contract. When the private 18.0.10.335 stock fixture is supplied
out of tree, its output can be compared with the known digest.

## Current OS stock restore

`restore-current-stock` exists for the pre-OTA case where the currently running
slot still contains the nine-byte PRC transformation from the previous cycle.
It does not download or guess a ROM image.

The restore is allowed only when:

- the device profile and unlocked state pass the normal allow-list;
- `current_slot == next_boot_slot`, so no OTA-pending slot switch exists;
- the current `vendor_boot` parses as the supported PRC Tuna/Tunap image;
- a direct child of `/data/adb/tb376-ota-helper/backups/` contains the exact
  `vendor_boot_<current>-prc.img` SHA-256 currently present on the block device;
- that directory's immutable OTA artifact metadata describes exactly nine
  ROW→PRC changed bytes for the current slot;
- the paired `vendor_boot_<current>-stock.img` SHA-256 matches the stored input
  digest and parses as the supported ROW image;
- the PRC artifact SHA-256 also matches its stored output digest;
- the usual battery policy passes.

Only then is the stock image streamed to the current slot, followed by `fsync`
and a full-partition read-back SHA-256. If a write or read-back fails, the
journal remains in `current_stock_restore_failed_do_not_reboot` and the app's
recovery screen retries the same validated stock artifact. The backup
`operation.json` is not rewritten, so it continues to describe the original
OTA transformation used to prove the stock/PRC pair.

If the current partition already parses as ROW, the command is a no-op and no
partition write occurs.

## Transaction and recovery

Operations take an exclusive lock and keep
`/data/adb/tb376-ota-helper/state.json` atomically updated. A normal OTA-target
write is preceded by the full stock backup and all patch validation. After
write and `fsync`, the entire target partition is read and hashed. A mismatch
immediately triggers a full stock restore, `fsync`, and another complete
read-back hash.

The dedicated current-slot stock restore also journals before its first write.
It intentionally does not use the normal inactive-slot write primitive, so the
existing `ensure_write_target` rule continues to reject active slots everywhere
else.

The reboot button requires:

- completed OTA-target operation (including the no-op already-PRC case);
- complete read-back SHA-256 verification;
- operation target still equals the next boot slot;
- no restore in progress;
- journal status `success` or `success_already_prc`.

A successful current-slot stock restore does not enable the app's OTA reboot
button. The app never reboots automatically.

## Battery policy

- below 30%: writing prohibited;
- 30–49%: writing prohibited unless external power is detected;
- 50% or higher: writing allowed.

Battery policy does not weaken any slot, model, block-device, FDT, or artifact
identity check.

## Build

CI installs stable Rust, cargo-ndk, Android SDK, and Gradle 8.11.1:

```text
cd native
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cd ..
scripts/build-native-android.sh
gradle lint testDebugUnitTest assembleDebug assembleRelease
```

The release APK uses the generated local debug signing identity for CI
installability only. No keystore is committed. Production distribution should
provide its signing identity through a protected build environment.
