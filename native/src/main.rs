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
    partition_name, partition_path, patch_image, sha256_file, stream_copy_exact, stream_hash,
    validate_block_device, validate_partition_size, PatchReport, BACKUP_DIR,
    EXPECTED_HWBOARD_ID, EXPECTED_PRODUCT, LOCK_PATH, ROOT_DIR, STATE_PATH,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

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
        if report.region != tb376_ota_helper_native::Region::Prc
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
    let next_boot_slot = next_boot_slot()?;
    if let Some(slot) = requested_slot {
        if slot != next_boot_slot {
            bail!("requested slot {slot} is not bootctl active boot slot {next_boot_slot}");
        }
    }
    if requested_slot.is_some() && current_slot == next_boot_slot {
        bail!("next boot slot equals current slot; OTA-pending state not detected");
    }
    let target_partition = partition_name(next_boot_slot)?.to_string();
    if requested_slot.is_some() {
        ensure_write_target(current_slot, next_boot_slot, &target_partition)?;
    }
    let target_path = partition_path(next_boot_slot)?;
    validate_block_device(&target_path, &target_partition)?;
    let partition_size = block_device_size(&File::open(&target_path)?)?;
    if !(64 * 1024 * 1024..=256 * 1024 * 1024).contains(&partition_size)
        || partition_size % 4096 != 0
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
    let product = product_candidates
        .iter()
        .filter_map(|key| prop(&props, key))
        .find(|value| value.eq_ignore_ascii_case(EXPECTED_PRODUCT))
        .unwrap_or_default();
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
    let supported_device = product.eq_ignore_ascii_case(EXPECTED_PRODUCT)
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
        ota_status: command_output("/system/bin/update_engine_client", &["--status"]).ok(),
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
    fs::write(image.with_file_name(format!("{file_name}.sha256")), format!("{hash}  {file_name}\n"))?;
    Ok(())
}

fn operation_lock() -> Result<File> {
    fs::create_dir_all(ROOT_DIR)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
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
    let online = ["usb", "ac", "wireless"]
        .iter()
        .any(|name| fs::read_to_string(format!("/sys/class/power_supply/{name}/online"))
            .map(|v| v.trim() == "1")
            .unwrap_or(false));
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
    let prop_slot = prop(props, "ro.boot.slot_suffix").and_then(|v| normalize_slot(&v));
    let bootctl_slot =
        command_output("/system/bin/bootctl", &["get-current-slot"]).ok().and_then(|v| normalize_slot(&v));
    match (prop_slot, bootctl_slot) {
        (Some(a), Some(b)) if a != b => bail!("slot sources disagree: {a}/{b}"),
        (Some(slot), _) | (_, Some(slot)) => Ok(slot),
        _ => bail!("cannot determine current slot"),
    }
}

fn next_boot_slot() -> Result<char> {
    let value = command_output("/system/bin/bootctl", &["get-active-boot-slot"])
        .context("bootctl get-active-boot-slot failed")?;
    normalize_slot(&value).context("cannot determine next boot slot")
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
        key.to_ascii_lowercase().contains("board")
            && value.contains(EXPECTED_HWBOARD_ID)
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
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
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
