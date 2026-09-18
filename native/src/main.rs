use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tb376_ota_helper_native::{
    atomic_json, block_device_size, ensure_write_target, inspect_image, inspect_image_sized,
    partition_name, partition_path, patch_image, reverse_patch_image_to_row, sha256_file,
    stream_copy_exact, stream_hash, validate_block_device, validate_partition_size, PatchReport,
    Region, BACKUP_DIR, EXPECTED_HWBOARD_ID, EXPECTED_PARTITION_SIZE_335, EXPECTED_PRODUCT,
    KNOWN_335_PRC_SHA256, LOCK_PATH, ROOT_DIR, STATE_PATH,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
const OTA_IDLE: &str = "UPDATE_STATUS_IDLE";
const OTA_UPDATED_NEED_REBOOT: &str = "UPDATE_STATUS_UPDATED_NEED_REBOOT";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceInfo {
    root: bool,
    product: String,
    hwboardid: String,
    system_model: String,
    bootloader_unlocked: bool,
    verified_boot_state: String,
    current_slot: char,
    next_boot_slot: char,
    target_partition: String,
    target_path: String,
    partition_size: u64,
    build_fingerprint: String,
    kernel_release: String,
    serial: String,
    battery_percent: u8,
    charging: bool,
    ota_status: Option<String>,
    kernelsu_next_present: bool,
    supported_device: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Operation {
    tool_version: String,
    timestamp: String,
    serial: String,
    product: String,
    hwboardid: String,
    current_slot: String,
    next_boot_slot: String,
    target_partition: String,
    build_fingerprint: String,
    kernel_release: String,
    input_size: u64,
    input_sha256: String,
    output_size: u64,
    output_sha256: String,
    changed_byte_count: usize,
    changed_offsets: Vec<u64>,
    supported_fdt_count: usize,
    write_started: bool,
    write_completed: bool,
    readback_verified: bool,
    restore_attempted: bool,
    restore_verified: bool,
    already_prc: bool,
    status: String,
    backup_dir: String,
    error: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        emit("error", json!({"message": format!("{error:#}")}));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().context("missing subcommand")?;
    let mut slot = None;
    let mut backup_dir = None;
    let mut backup = None;
    let mut image = None;
    let mut json_output = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--slot" => slot = Some(parse_slot(&args.next().context("missing --slot value")?)?),
            "--backup-dir" => {
                backup_dir = Some(PathBuf::from(args.next().context("missing backup directory")?))
            }
            "--backup" => backup = Some(PathBuf::from(args.next().context("missing backup path")?)),
            "--image" => image = Some(PathBuf::from(args.next().context("missing image path")?)),
            "--json" => json_output = true,
            other => bail!("unknown argument: {other}"),
        }
    }
    if !json_output {
        bail!("--json is required");
    }
    match command.as_str() {
        "inspect" => {
            let device = inspect_device(None)?;
            let fdt = inspect_image_sized(Path::new(&device.target_path), device.partition_size)?;
            emit("result", json!({"device": device, "fdt": fdt, "journal": read_journal()}));
        }
        "dry-run" => dry_run(slot.context("--slot is required")?, backup_dir)?,
        "patch" => patch(slot.context("--slot is required")?, backup_dir)?,
        "restore" => restore(
            slot.context("--slot is required")?,
            backup.context("--backup is required")?,
        )?,
        "restore-current-stock" => restore_current_stock()?,
        "verify" => verify(
            slot.context("--slot is required")?,
            image.context("--image is required")?,
        )?,
        _ => bail!("unknown subcommand: {command}"),
    }
    Ok(())
}

fn dry_run(slot: char, requested_dir: Option<PathBuf>) -> Result<()> {
    let _lock = operation_lock()?;
    emit("progress", json!({"step": "device_validation"}));
    let device = inspect_device(Some(slot))?;
    let backup_dir = requested_dir.unwrap_or_else(|| new_backup_dir(slot));
    validate_backup_dir(&backup_dir, false)?;
    fs::create_dir_all(&backup_dir)?;
    let (operation, _) = prepare_artifacts(&device, &backup_dir)?;
    write_operation_files(&device, &operation)?;
    emit("result", json!({"mode": "dry-run", "device": device, "operation": operation}));
    Ok(())
}

fn patch(slot: char, requested_dir: Option<PathBuf>) -> Result<()> {
    let _lock = operation_lock()?;
    emit("progress", json!({"step": "device_validation"}));
    let device = inspect_device(Some(slot))?;
    enforce_battery(&device)?;
    let backup_dir = requested_dir.unwrap_or_else(|| new_backup_dir(slot));
    validate_backup_dir(&backup_dir, backup_dir.exists())?;
    fs::create_dir_all(&backup_dir)?;
    let (mut operation, patched_path) = prepare_artifacts(&device, &backup_dir)?;
    operation.status = "prepared".to_string();
    atomic_json(Path::new(STATE_PATH), &operation)?;
    write_operation_files(&device, &operation)?;

    if operation.already_prc {
        operation.write_started = false;
        operation.write_completed = true;
        operation.readback_verified = true;
        operation.status = "success_already_prc".to_string();
        atomic_json(Path::new(STATE_PATH), &operation)?;
        write_operation_files(&device, &operation)?;
        emit("result", json!({"device": device, "operation": operation}));
        return Ok(());
    }

    operation.write_started = true;
    operation.status = "writing".to_string();
    atomic_json(Path::new(STATE_PATH), &operation)?;
    emit("progress", json!({"step": "write_partition"}));
    let write_result = write_and_verify(&device, &patched_path, &operation.output_sha256);
    match write_result {
        Ok(()) => {
            operation.write_completed = true;
            operation.readback_verified = true;
            operation.status = "success".to_string();
            atomic_json(Path::new(STATE_PATH), &operation)?;
            write_operation_files(&device, &operation)?;
            emit("result", json!({"device": device, "operation": operation}));
            Ok(())
        }
        Err(write_error) => {
            operation.error = Some(format!("{write_error:#}"));
            operation.status = "restore_required".to_string();
            operation.restore_attempted = true;
            atomic_json(Path::new(STATE_PATH), &operation)?;
            emit("progress", json!({"step": "automatic_restore"}));
            let stock = backup_dir.join(stock_name(slot));
            match write_and_verify(&device, &stock, &operation.input_sha256) {
                Ok(()) => {
                    operation.restore_verified = true;
                    operation.status = "restored_after_failure".to_string();
                    atomic_json(Path::new(STATE_PATH), &operation)?;
                    write_operation_files(&device, &operation)?;
                    bail!("write failed; stock image was restored and verified: {write_error:#}")
                }
                Err(restore_error) => {
                    operation.restore_verified = false;
                    operation.status = "restore_failed_do_not_reboot".to_string();
                    operation.error = Some(format!(
                        "write: {write_error:#}; restore: {restore_error:#}"
                    ));
                    atomic_json(Path::new(STATE_PATH), &operation)?;
                    write_operation_files(&device, &operation)?;
                    bail!(
                        "復元に失敗しました 再起動しないでください FastbootまたはEDLで復旧が必要です: {restore_error:#}"
                    )
                }
            }
        }
    }
}

fn restore(slot: char, backup: PathBuf) -> Result<()> {
    let _lock = operation_lock()?;
    validate_backup_file(&backup, slot, &stock_name(slot))?;
    let device = inspect_device(Some(slot))?;
    let expected = sha256_file(&backup)?;
    let mut operation = read_journal().context("no journal to restore")?;
    operation.restore_attempted = true;
    operation.status = "restoring".to_string();
    atomic_json(Path::new(STATE_PATH), &operation)?;
    match write_and_verify(&device, &backup, &expected) {
        Ok(()) => {
            operation.restore_verified = true;
            operation.status = "restore_success".to_string();
            atomic_json(Path::new(STATE_PATH), &operation)?;
            emit("result", json!({"device": device, "operation": operation}));
            Ok(())
        }
        Err(error) => {
            operation.restore_verified = false;
            operation.status = "restore_failed_do_not_reboot".to_string();
            operation.error = Some(format!("{error:#}"));
            atomic_json(Path::new(STATE_PATH), &operation)?;
            bail!("復元に失敗しました 再起動しないでください: {error:#}")
        }
    }
}

fn restore_current_stock() -> Result<()> {
    let _lock = operation_lock()?;
    emit("progress", json!({"step": "current_slot_validation"}));
    let device = inspect_device(None)?;
    validate_current_restore_device(&device)?;
    enforce_battery(&device)?;

    let current_hash = hash_partition(&device)?;
    let recovery = read_journal().filter(|operation| {
        operation.status.starts_with("current_stock_restore_")
            && operation.status != "current_stock_restore_success"
    });

    let mut operation = if let Some(mut operation) = recovery {
        let backup_dir = PathBuf::from(&operation.backup_dir);
        validate_current_restore_backup(&device, &backup_dir, &operation, None)?;
        if current_hash == operation.input_sha256 {
            operation.write_started = true;
            operation.write_completed = true;
            operation.readback_verified = true;
            operation.restore_attempted = true;
            operation.restore_verified = true;
            operation.status = "current_stock_restore_success".to_string();
            operation.error = None;
            atomic_json(Path::new(STATE_PATH), &operation)?;
            let fdt = inspect_image_sized(Path::new(&device.target_path), device.partition_size)?;
            emit(
                "result",
                json!({"device": device, "fdt": fdt, "operation": operation, "readback_sha256": current_hash}),
            );
            return Ok(());
        }
        operation
    } else {
        let fdt = inspect_image_sized(Path::new(&device.target_path), device.partition_size)?;
        if fdt.region == Region::Row {
            emit(
                "result",
                json!({"device": device, "fdt": fdt, "already_stock": true, "readback_sha256": current_hash}),
            );
            return Ok(());
        }
        if fdt.region != Region::Prc {
            bail!("current vendor_boot is not a supported PRC image");
        }
        let mut operation = match find_current_stock_backup(&device, &current_hash) {
            Ok(operation) => operation,
            Err(lookup_error)
                if current_hash.eq_ignore_ascii_case(KNOWN_335_PRC_SHA256)
                    && device.partition_size == EXPECTED_PARTITION_SIZE_335 =>
            {
                prepare_known_335_current_stock_backup(&device, &current_hash).with_context(|| {
                    format!(
                        "known 18.0.10.335 PRC vendor_boot matched, but verified ROW derivation failed after backup lookup error: {lookup_error:#}"
                    )
                })?
            }
            Err(error) => return Err(error),
        };
        operation.tool_version = TOOL_VERSION.to_string();
        operation.timestamp = now();
        operation.serial = device.serial.clone();
        operation.product = device.product.clone();
        operation.hwboardid = device.hwboardid.clone();
        operation.current_slot = device.current_slot.to_string();
        operation.next_boot_slot = device.current_slot.to_string();
        operation.target_partition = partition_name(device.current_slot)?.to_string();
        operation.build_fingerprint = device.build_fingerprint.clone();
        operation.kernel_release = device.kernel_release.clone();
        operation.write_started = false;
        operation.write_completed = false;
        operation.readback_verified = false;
        operation.restore_attempted = false;
        operation.restore_verified = false;
        operation.status = "current_stock_restore_prepared".to_string();
        operation.error = None;
        operation
    };

    let backup_dir = PathBuf::from(&operation.backup_dir);
    validate_current_restore_backup(&device, &backup_dir, &operation, None)?;
    let stock = backup_dir.join(stock_name(device.current_slot));

    operation.write_started = true;
    operation.write_completed = false;
    operation.readback_verified = false;
    operation.restore_attempted = true;
    operation.restore_verified = false;
    operation.status = "current_stock_restore_writing".to_string();
    operation.error = None;
    atomic_json(Path::new(STATE_PATH), &operation)?;
    emit("progress", json!({"step": "restore_current_vendor_boot"}));

    match write_current_and_verify(&device, &stock, &operation.input_sha256) {
        Ok(()) => {
            operation.write_completed = true;
            operation.readback_verified = true;
            operation.restore_verified = true;
            operation.status = "current_stock_restore_success".to_string();
            operation.error = None;
            atomic_json(Path::new(STATE_PATH), &operation)?;
            let fdt = inspect_image_sized(Path::new(&device.target_path), device.partition_size)?;
            emit(
                "result",
                json!({"device": device, "fdt": fdt, "operation": operation, "readback_sha256": operation.input_sha256}),
            );
            Ok(())
        }
        Err(error) => {
            operation.write_completed = false;
            operation.readback_verified = false;
            operation.restore_verified = false;
            operation.status = "current_stock_restore_failed_do_not_reboot".to_string();
            operation.error = Some(format!("{error:#}"));
            atomic_json(Path::new(STATE_PATH), &operation)?;
            bail!("現在slotのstock vendor_boot復元に失敗しました 再起動しないでください: {error:#}")
        }
    }
}

fn prepare_known_335_current_stock_backup(
    device: &DeviceInfo,
    current_hash: &str,
) -> Result<Operation> {
    if !current_hash.eq_ignore_ascii_case(KNOWN_335_PRC_SHA256) {
        bail!("current vendor_boot does not match the known 18.0.10.335 PRC SHA-256");
    }
    if device.partition_size != EXPECTED_PARTITION_SIZE_335 {
        bail!(
            "known 18.0.10.335 vendor_boot size mismatch: {} != {}",
            device.partition_size,
            EXPECTED_PARTITION_SIZE_335
        );
    }

    fs::create_dir_all(BACKUP_DIR)?;
    let backup_dir = new_backup_dir(device.current_slot);
    validate_backup_dir(&backup_dir, false)?;
    fs::create_dir(&backup_dir)?;

    let patched = backup_dir.join(patched_name(device.current_slot));
    let stock = backup_dir.join(stock_name(device.current_slot));
    let roundtrip = backup_dir.join("vendor_boot-roundtrip-prc.img");

    let patched_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&patched)?;
    let copied_hash = stream_copy_exact(
        File::open(&device.target_path)?,
        &patched_file,
        device.partition_size,
    )?;
    patched_file.sync_all()?;
    if !copied_hash.eq_ignore_ascii_case(current_hash) {
        bail!("current vendor_boot changed while capturing the known PRC image");
    }
    if !sha256_file(&patched)?.eq_ignore_ascii_case(KNOWN_335_PRC_SHA256) {
        bail!("captured PRC backup does not match the known 18.0.10.335 digest");
    }

    let reverse = reverse_patch_image_to_row(&patched, &stock)?;
    if reverse.changed_byte_count != 9
        || reverse.changed_offsets.len() != 9
        || reverse.supported_fdt_count != 3
        || reverse.input_size != EXPECTED_PARTITION_SIZE_335
        || reverse.output_size != EXPECTED_PARTITION_SIZE_335
    {
        bail!("known PRC reverse patch did not satisfy the fixed 9-byte transformation");
    }

    let report = patch_image(&stock, &roundtrip)?;
    if report.already_prc
        || report.changed_byte_count != 9
        || report.changed_offsets.len() != 9
        || report.supported_fdt_count != 3
        || !report.output_sha256.eq_ignore_ascii_case(KNOWN_335_PRC_SHA256)
        || sha256_file(&roundtrip)? != sha256_file(&patched)?
    {
        bail!("derived ROW image failed exact ROW->PRC round-trip verification");
    }
    fs::remove_file(&roundtrip)?;

    let mut operation = operation_from(device, &backup_dir, &report);
    operation.status = "current_stock_restore_prepared".to_string();
    operation.next_boot_slot = device.current_slot.to_string();
    operation.target_partition = partition_name(device.current_slot)?.to_string();
    write_operation_files(device, &operation)?;
    Ok(operation)
}

fn find_current_stock_backup(device: &DeviceInfo, current_hash: &str) -> Result<Operation> {
    let backup_root = Path::new(BACKUP_DIR);
    if !backup_root.is_dir() {
        bail!("stock backup directory does not exist: {BACKUP_DIR}");
    }
    for entry in fs::read_dir(backup_root)? {
        let Ok(entry) = entry else {
            continue;
        };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let operation: Operation = match fs::read(dir.join("operation.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(operation) => operation,
            None => continue,
        };
        if validate_current_restore_backup(device, &dir, &operation, Some(current_hash)).is_ok() {
            return Ok(operation);
        }
    }
    bail!(
        "現在のvendor_boot SHA-256に一致するHelper生成済みPRCバックアップがありません。stockを推測して書き込むことはしません"
    )
}

fn validate_current_restore_backup(
    device: &DeviceInfo,
    backup_dir: &Path,
    operation: &Operation,
    expected_patched_hash: Option<&str>,
) -> Result<()> {
    validate_backup_dir(backup_dir, true)?;
    if Path::new(&operation.backup_dir) != backup_dir {
        bail!("operation backup_dir does not match its containing directory");
    }
    let slot = device.current_slot;
    let partition = partition_name(slot)?;
    if operation.next_boot_slot != slot.to_string()
        || operation.target_partition != partition
        || operation.input_size != device.partition_size
        || operation.output_size != device.partition_size
        || operation.already_prc
        || operation.changed_byte_count != 9
        || operation.changed_offsets.len() != 9
        || operation.supported_fdt_count != 3
    {
        bail!("backup metadata does not describe the current slot's ROW-to-PRC transformation");
    }
    if let Some(expected) = expected_patched_hash
        && operation.output_sha256 != expected
    {
        bail!("backup PRC hash does not match the current vendor_boot");
    }

    let stock = backup_dir.join(stock_name(slot));
    let patched = backup_dir.join(patched_name(slot));
    validate_backup_file(&stock, slot, &stock_name(slot))?;
    validate_backup_file(&patched, slot, &patched_name(slot))?;
    if sha256_file(&stock)? != operation.input_sha256 {
        bail!("stock backup SHA-256 does not match operation metadata");
    }
    if sha256_file(&patched)? != operation.output_sha256 {
        bail!("PRC backup SHA-256 does not match operation metadata");
    }
    let stock_fdt = inspect_image(&stock)?;
    let patched_fdt = inspect_image(&patched)?;
    if stock_fdt.region != Region::Row
        || patched_fdt.region != Region::Prc
        || stock_fdt.supported_fdt_count != 3
        || patched_fdt.supported_fdt_count != 3
    {
        bail!("backup artifact FDT validation failed");
    }
    Ok(())
}

fn validate_current_restore_device(device: &DeviceInfo) -> Result<()> {
    if !device.supported_device {
        bail!(
            "unsupported device: product={}, model={}, hwboardid={}, unlocked={}",
            device.product,
            device.system_model,
            device.hwboardid,
            device.bootloader_unlocked
        );
    }
    if device.ota_status.as_deref() != Some(OTA_IDLE) {
        bail!(
            "update_engine is not IDLE; current stock restore is prohibited while OTA is active: {}",
            device.ota_status.as_deref().unwrap_or("unknown")
        );
    }
    if device.current_slot != device.next_boot_slot {
        bail!("OTA-pending state detected; current stock restore must be completed before starting the OTA");
    }
    let partition = partition_name(device.current_slot)?;
    if device.target_partition != partition {
        bail!("current slot partition mapping is inconsistent");
    }
    let expected_path = partition_path(device.current_slot)?;
    if Path::new(&device.target_path) != expected_path {
        bail!("current slot partition path is inconsistent");
    }
    validate_block_device(&expected_path, partition)?;
    Ok(())
}

fn write_current_and_verify(device: &DeviceInfo, image: &Path, expected_hash: &str) -> Result<()> {
    validate_current_restore_device(device)?;
    let image_size = fs::metadata(image)?.len();
    validate_partition_size(device.partition_size, image_size)?;
    let partition = partition_name(device.current_slot)?;
    validate_block_device(Path::new(&device.target_path), partition)?;
    let mut source = File::open(image)?;
    let mut target = OpenOptions::new().write(true).open(&device.target_path)?;
    let copied_hash = stream_copy_exact(&mut source, &mut target, image_size)?;
    target.sync_all()?;
    if copied_hash != expected_hash {
        bail!("stock backup changed while writing");
    }
    let readback = hash_partition(device)?;
    if readback != expected_hash {
        bail!("full partition SHA-256 mismatch: {readback} != {expected_hash}");
    }
    Ok(())
}

fn verify(slot: char, image: PathBuf) -> Result<()> {
    let _lock = operation_lock()?;
    validate_backup_file(&image, slot, "")?;
    let device = inspect_device(Some(slot))?;
    let expected = sha256_file(&image)?;
    let actual = hash_partition(&device)?;
    if expected != actual {
        bail!("full partition SHA-256 mismatch: {actual} != {expected}");
    }
    emit("result", json!({"device": device, "image_sha256": expected, "readback_sha256": actual}));
    Ok(())
}

fn prepare_artifacts(device: &DeviceInfo, backup_dir: &Path) -> Result<(Operation, PathBuf)> {
    let slot = device.next_boot_slot;
    let stock = backup_dir.join(stock_name(slot));
    let patched = backup_dir.join(patched_name(slot));
    if stock.exists() || patched.exists() {
        if !stock.is_file() || !patched.is_file() {
            bail!("backup directory contains an incomplete artifact set");
        }
        let operation: Operation =
            serde_json::from_slice(&fs::read(backup_dir.join("operation.json"))?)?;
        if operation.status != "dry_run_success"
            || operation.current_slot != device.current_slot.to_string()
            || operation.next_boot_slot != device.next_boot_slot.to_string()
            || operation.target_partition != device.target_partition
            || operation.input_sha256 != sha256_file(&stock)?
            || operation.output_sha256 != sha256_file(&patched)?
            || operation.input_size != device.partition_size
            || operation.output_size != device.partition_size
        {
            bail!("existing dry-run artifacts do not match the current device state");
        }
        let report = inspect_image(&patched)?;
        if report.region != Region::Prc
            || report.supported_fdt_count != 3
            || report.tuna_count != 2
            || report.tunap_count != 1
        {
            bail!("existing patched image failed FDT validation");
        }
        return Ok((operation, patched));
    }
    emit("progress", json!({"step": "stream_stock_backup"}));
    let partition = File::open(&device.target_path)?;
    let stock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&stock)?;
    let stock_sha = stream_copy_exact(partition, &stock_file, device.partition_size)?;
    stock_file.sync_all()?;
    write_hash_file(&stock, &stock_sha)?;

    emit("progress", json!({"step": "parse_and_patch_fdt"}));
    let report = patch_image(&stock, &patched)?;
    if report.input_sha256 != stock_sha {
        bail!("stock hash changed between backup and patch");
    }
    write_hash_file(&patched, &report.output_sha256)?;
    let operation = operation_from(device, backup_dir, &report);
    Ok((operation, patched))
}

fn write_and_verify(device: &DeviceInfo, image: &Path, expected_hash: &str) -> Result<()> {
    let image_size = fs::metadata(image)?.len();
    validate_partition_size(device.partition_size, image_size)?;
    let partition_name = partition_name(device.next_boot_slot)?;
    ensure_write_target(device.current_slot, device.next_boot_slot, partition_name)?;
    validate_block_device(Path::new(&device.target_path), partition_name)?;
    let mut source = File::open(image)?;
    let mut target = OpenOptions::new().write(true).open(&device.target_path)?;
    let copied_hash = stream_copy_exact(&mut source, &mut target, image_size)?;
    target.sync_all()?;
    if copied_hash != expected_hash {
        bail!("source changed while writing");
    }
    let readback = hash_partition(device)?;
    if readback != expected_hash {
        bail!("full partition SHA-256 mismatch: {readback} != {expected_hash}");
    }
    Ok(())
}

fn hash_partition(device: &DeviceInfo) -> Result<String> {
    let file = File::open(&device.target_path)?;
    stream_hash(file.take(device.partition_size), device.partition_size)
}

fn inspect_device(requested_slot: Option<char>) -> Result<DeviceInfo> {
    if !is_root()? {
        bail!("root uid=0 is required");
    }
    let props = getprop_all()?;
    let current_slot = current_slot(&props)?;
    let ota_status = update_engine_status()?;
    let next_boot_slot = next_boot_slot_from_ota(current_slot, &ota_status);
    if let Some(slot) = requested_slot
        && slot != next_boot_slot
    {
        bail!(
            "requested slot {slot} does not match OTA target slot {next_boot_slot} derived from update_engine={ota_status}"
        );
    }
    if requested_slot.is_some() && ota_status != OTA_UPDATED_NEED_REBOOT {
        bail!(
            "inactive-slot operation requires {OTA_UPDATED_NEED_REBOOT}; current update_engine status is {ota_status}"
        );
    }
    if requested_slot.is_some() && current_slot == next_boot_slot {
        bail!("next boot slot equals current slot; completed A/B OTA is not detected");
    }
    let target_partition = partition_name(next_boot_slot)?.to_string();
    if requested_slot.is_some() {
        ensure_write_target(current_slot, next_boot_slot, &target_partition)?;
    }
    let target_path = partition_path(next_boot_slot)?;
    validate_block_device(&target_path, &target_partition)?;
    let partition_size = block_device_size(&File::open(&target_path)?)?;
    if !(64 * 1024 * 1024..=256 * 1024 * 1024).contains(&partition_size)
        || !partition_size.is_multiple_of(4096)
    {
        bail!("implausible vendor_boot size: {partition_size}");
    }

    let product_candidates = [
        "ro.build.product",
        "ro.product.device",
        "ro.product.product.device",
        "ro.boot.product",
        "ro.hardware",
    ];
    let product_values: Vec<String> = product_candidates
        .iter()
        .filter_map(|key| prop(&props, key))
        .filter(|value| !value.is_empty())
        .collect();
    let product = product_values
        .iter()
        .find(|value| is_supported_product_identity(value))
        .cloned()
        .or_else(|| product_values.first().cloned())
        .unwrap_or_default();
    let product_matches = product_values
        .iter()
        .any(|value| is_supported_product_identity(value));
    let hwboardid = detect_hwboardid(&props).unwrap_or_default();
    let system_model = [
        "ro.product.model",
        "ro.product.system.model",
        "ro.product.product.model",
        "ro.product.vendor.model",
    ]
        .iter()
        .filter_map(|key| prop(&props, key))
        .find(|value| {
            matches!(
                value.to_ascii_uppercase().as_str(),
                "TB390FU" | "TB390FU_PRC"
            )
        })
        .unwrap_or_default();
    let flash_locked = prop(&props, "ro.boot.flash.locked").unwrap_or_default();
    let verified_boot_state =
        prop(&props, "ro.boot.verifiedbootstate").unwrap_or_default();
    let vbmeta_state = prop(&props, "ro.boot.vbmeta.device_state").unwrap_or_default();
    let bootloader_unlocked = flash_locked == "0"
        && (verified_boot_state.eq_ignore_ascii_case("orange")
            || vbmeta_state.eq_ignore_ascii_case("unlocked"));
    let supported_device = product_matches
        && hwboardid.contains(EXPECTED_HWBOARD_ID)
        && matches!(
            system_model.to_ascii_uppercase().as_str(),
            value if value == "TB390FU" || value == "TB390FU_PRC"
        )
        && bootloader_unlocked;
    if requested_slot.is_some() && !supported_device {
        bail!(
            "unsupported device: product={product}, model={system_model}, hwboardid={hwboardid}, unlocked={bootloader_unlocked}"
        );
    }

    let (battery_percent, charging) = battery_state()?;
    Ok(DeviceInfo {
        root: true,
        product,
        hwboardid,
        system_model,
        bootloader_unlocked,
        verified_boot_state,
        current_slot,
        next_boot_slot,
        target_partition,
        target_path: target_path.display().to_string(),
        partition_size,
        build_fingerprint: prop(&props, "ro.build.fingerprint").unwrap_or_default(),
        kernel_release: fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .to_string(),
        serial: prop(&props, "ro.serialno").unwrap_or_default(),
        battery_percent,
        charging,
        ota_status: Some(ota_status),
        kernelsu_next_present: Path::new("/sys/kernel/ksu").exists()
            || Path::new("/data/adb/ksu").exists(),
        supported_device,
    })
}

fn operation_from(device: &DeviceInfo, backup_dir: &Path, report: &PatchReport) -> Operation {
    Operation {
        tool_version: TOOL_VERSION.to_string(),
        timestamp: now(),
        serial: device.serial.clone(),
        product: device.product.clone(),
        hwboardid: device.hwboardid.clone(),
        current_slot: device.current_slot.to_string(),
        next_boot_slot: device.next_boot_slot.to_string(),
        target_partition: device.target_partition.clone(),
        build_fingerprint: device.build_fingerprint.clone(),
        kernel_release: device.kernel_release.clone(),
        input_size: report.input_size,
        input_sha256: report.input_sha256.clone(),
        output_size: report.output_size,
        output_sha256: report.output_sha256.clone(),
        changed_byte_count: report.changed_byte_count,
        changed_offsets: report.changed_offsets.clone(),
        supported_fdt_count: report.supported_fdt_count,
        write_started: false,
        write_completed: false,
        readback_verified: false,
        restore_attempted: false,
        restore_verified: false,
        already_prc: report.already_prc,
        status: "dry_run_success".to_string(),
        backup_dir: backup_dir.display().to_string(),
        error: None,
    }
}

fn write_operation_files(device: &DeviceInfo, operation: &Operation) -> Result<()> {
    let dir = Path::new(&operation.backup_dir);
    atomic_json(&dir.join("operation.json"), operation)?;
    atomic_json(&dir.join("device-info.json"), device)?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("operation.log"))?;
    writeln!(log, "{} {}", now(), operation.status)?;
    log.sync_all()?;
    Ok(())
}

fn write_hash_file(image: &Path, hash: &str) -> Result<()> {
    let file_name = image.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    fs::write(
        image.with_file_name(format!("{file_name}.sha256")),
        format!("{hash}  {file_name}\n"),
    )?;
    Ok(())
}

fn operation_lock() -> Result<File> {
    fs::create_dir_all(ROOT_DIR)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(LOCK_PATH)?;
    lock.try_lock_exclusive()
        .context("another operation is already running")?;
    Ok(lock)
}

fn enforce_battery(device: &DeviceInfo) -> Result<()> {
    match device.battery_percent {
        0..=29 => bail!("battery below 30%; writing is prohibited"),
        30..=49 if !device.charging => bail!("battery below 50% and not charging"),
        _ => Ok(()),
    }
}

fn battery_state() -> Result<(u8, bool)> {
    let capacity = fs::read_to_string("/sys/class/power_supply/battery/capacity")?
        .trim()
        .parse::<u8>()?;
    let status = fs::read_to_string("/sys/class/power_supply/battery/status")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let online = ["usb", "ac", "wireless"].iter().any(|name| {
        fs::read_to_string(format!("/sys/class/power_supply/{name}/online"))
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
    });
    Ok((
        capacity,
        online || status.contains("charging") || status.contains("full"),
    ))
}

fn is_root() -> Result<bool> {
    let status = fs::read_to_string("/proc/self/status")?;
    let euid = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .and_then(|line| line.split_whitespace().nth(2))
        .context("cannot read effective uid")?;
    Ok(euid == "0")
}

fn getprop_all() -> Result<Vec<(String, String)>> {
    let output = command_output("/system/bin/getprop", &[])?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once("]: [")?;
            Some((
                key.trim_start_matches('[').to_string(),
                value.trim_end_matches(']').to_string(),
            ))
        })
        .collect())
}

fn prop(props: &[(String, String)], key: &str) -> Option<String> {
    props
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
}

fn current_slot(props: &[(String, String)]) -> Result<char> {
    ["ro.boot.slot_suffix", "ro.boot.slot"]
        .iter()
        .filter_map(|key| prop(props, key))
        .find_map(|value| normalize_slot(&value))
        .context("cannot determine current slot from ro.boot.slot_suffix/ro.boot.slot")
}

fn next_boot_slot_from_ota(current_slot: char, ota_status: &str) -> char {
    if ota_status == OTA_UPDATED_NEED_REBOOT {
        opposite_slot(current_slot)
    } else {
        current_slot
    }
}

fn opposite_slot(slot: char) -> char {
    match slot {
        'a' => 'b',
        'b' => 'a',
        _ => slot,
    }
}

fn update_engine_status() -> Result<String> {
    let output = Command::new("/system/bin/toybox")
        .args([
            "timeout",
            "2",
            "/system/bin/update_engine_client",
            "--follow",
        ])
        .output()
        .context("cannot run update_engine_client --follow")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    parse_update_engine_status(&combined).with_context(|| {
        format!(
            "update_engine status callback was not received (exit={:?}): {}",
            output.status.code(),
            combined.trim()
        )
    })
}

fn parse_update_engine_status(text: &str) -> Option<String> {
    const MARKER: &str = "onStatusUpdate(";
    text.lines().find_map(|line| {
        let start = line.find(MARKER)? + MARKER.len();
        let status = line[start..].split_whitespace().next()?;
        status
            .starts_with("UPDATE_STATUS_")
            .then(|| status.to_string())
    })
}

fn is_supported_product_identity(value: &str) -> bool {
    value.eq_ignore_ascii_case(EXPECTED_PRODUCT)
        || matches!(
            value.to_ascii_uppercase().as_str(),
            "TB390FU" | "TB390FU_PRC"
        )
}

fn normalize_slot(value: &str) -> Option<char> {
    match value.trim().to_ascii_lowercase().as_str() {
        "0" | "a" | "_a" => Some('a'),
        "1" | "b" | "_b" => Some('b'),
        _ => None,
    }
}

fn parse_slot(value: &str) -> Result<char> {
    normalize_slot(value).context("slot must be a or b")
}

fn detect_hwboardid(props: &[(String, String)]) -> Result<String> {
    for key in [
        "ro.boot.hwboardid",
        "ro.boot.hardware.boardid",
        "ro.vendor.hwboardid",
        "ro.boot.board_id",
    ] {
        if let Some(value) = prop(props, key)
            && value.contains(EXPECTED_HWBOARD_ID)
        {
            return Ok(value);
        }
    }
    if let Some((_, value)) = props.iter().find(|(key, value)| {
        key.to_ascii_lowercase().contains("board") && value.contains(EXPECTED_HWBOARD_ID)
    }) {
        return Ok(value.clone());
    }
    for path in ["/proc/bootconfig", "/proc/cmdline"] {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(EXPECTED_HWBOARD_ID) {
            return Ok(EXPECTED_HWBOARD_ID.to_string());
        }
    }
    bail!("cannot verify hwboardid={EXPECTED_HWBOARD_ID}")
}

fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        bail!("{program} exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn new_backup_dir(slot: char) -> PathBuf {
    Path::new(BACKUP_DIR).join(format!(
        "{}-slot-{slot}",
        OffsetDateTime::now_utc().unix_timestamp()
    ))
}

fn validate_backup_dir(path: &Path, must_exist: bool) -> Result<()> {
    if !path.starts_with(BACKUP_DIR) || path == Path::new(BACKUP_DIR) {
        bail!("backup directory must be a child of {BACKUP_DIR}");
    }
    if path.components().count() != Path::new(BACKUP_DIR).components().count() + 1 {
        bail!("nested or escaped backup directories are not accepted");
    }
    if must_exist && !path.is_dir() {
        bail!("backup directory does not exist");
    }
    Ok(())
}

fn validate_backup_file(path: &Path, slot: char, expected_name: &str) -> Result<()> {
    let parent = path.parent().context("backup has no parent")?;
    validate_backup_dir(parent, true)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if (!expected_name.is_empty() && name != expected_name)
        || (expected_name.is_empty() && name != stock_name(slot) && name != patched_name(slot))
    {
        bail!("backup file is not an allow-listed vendor_boot artifact");
    }
    if !path.is_file() {
        bail!("backup image does not exist");
    }
    Ok(())
}

fn stock_name(slot: char) -> String {
    format!("vendor_boot_{slot}-stock.img")
}

fn patched_name(slot: char) -> String {
    format!("vendor_boot_{slot}-prc.img")
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().unix_timestamp().to_string())
}

fn read_journal() -> Option<Operation> {
    fs::read(STATE_PATH)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn emit(kind: &str, value: Value) {
    println!("{}", json!({"type": kind, "data": value}));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tb390fu_update_engine_follow_output() {
        let output = "[INFO:update_engine_client_android.cc(96)] onStatusUpdate(UPDATE_STATUS_IDLE (0), 0)\n";
        assert_eq!(parse_update_engine_status(output).as_deref(), Some(OTA_IDLE));
    }

    #[test]
    fn accepts_crossflashed_tb390fu_product_identity() {
        assert!(is_supported_product_identity("TB390FU"));
        assert!(is_supported_product_identity("tb390fu_prc"));
        assert!(is_supported_product_identity(EXPECTED_PRODUCT));
        assert!(!is_supported_product_identity("qssi_64"));
        assert!(!is_supported_product_identity("qcom"));
    }

    #[test]
    fn derives_ota_target_without_bootctl() {
        assert_eq!(next_boot_slot_from_ota('a', OTA_IDLE), 'a');
        assert_eq!(next_boot_slot_from_ota('a', OTA_UPDATED_NEED_REBOOT), 'b');
        assert_eq!(next_boot_slot_from_ota('b', OTA_UPDATED_NEED_REBOOT), 'a');
        assert_eq!(
            next_boot_slot_from_ota('b', "UPDATE_STATUS_DOWNLOADING"),
            'b'
        );
    }
}
