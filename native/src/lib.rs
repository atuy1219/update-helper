use anyhow::{bail, Context, Result};
use memmap2::{Mmap, MmapMut, MmapOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const ROOT_DIR: &str = "/data/adb/tb376-ota-helper";
pub const BACKUP_DIR: &str = "/data/adb/tb376-ota-helper/backups";
pub const STATE_PATH: &str = "/data/adb/tb376-ota-helper/state.json";
pub const LOCK_PATH: &str = "/data/adb/tb376-ota-helper/operation.lock";
pub const EXPECTED_PRODUCT: &str = "malbec";
pub const EXPECTED_HWBOARD_ID: &str = "SM8735P_8+128_22";
pub const KNOWN_335_PRC_SHA256: &str =
    "C41CE9D1F33D11275972FFC1A3AB532069476049D3AC589AA0E54F86DAEDAA5C";
pub const EXPECTED_PARTITION_SIZE_335: u64 = 100_663_296;

const FDT_MAGIC: u32 = 0xD00D_FEED;
const FDT_MAGIC_BYTES: [u8; 4] = [0xD0, 0x0D, 0xFE, 0xED];
const FDT_HEADER_SIZE: usize = 40;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Region {
    Row,
    Prc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdtReport {
    pub region: Region,
    pub supported_fdt_count: usize,
    pub tuna_count: usize,
    pub tunap_count: usize,
    pub region_value_offsets: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchReport {
    pub source_region: Region,
    pub target_region: Region,
    pub supported_fdt_count: usize,
    pub tuna_count: usize,
    pub tunap_count: usize,
    pub changed_byte_count: usize,
    pub changed_offsets: Vec<u64>,
    pub input_size: u64,
    pub output_size: u64,
    pub input_sha256: String,
    pub output_sha256: String,
    pub already_prc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Board {
    Tuna,
    Tunap,
}

#[derive(Debug, Clone, Copy)]
struct SupportedFdt {
    board: Board,
    region: Region,
    region_value_start: usize,
}

#[derive(Debug, Clone, Copy)]
struct FdtBounds {
    base: usize,
    total_end: usize,
    struct_start: usize,
    struct_end: usize,
    strings_start: usize,
    strings_end: usize,
}

pub fn partition_name(slot: char) -> Result<&'static str> {
    match slot {
        'a' => Ok("vendor_boot_a"),
        'b' => Ok("vendor_boot_b"),
        _ => bail!("slot must be a or b"),
    }
}

pub fn partition_path(slot: char) -> Result<PathBuf> {
    Ok(Path::new("/dev/block/by-name").join(partition_name(slot)?))
}

pub fn ensure_write_target(current_slot: char, target_slot: char, name: &str) -> Result<()> {
    if target_slot == current_slot {
        bail!("refusing to write active slot {current_slot}");
    }
    if name != partition_name(target_slot)? {
        bail!("partition {name} is not allow-listed for target slot {target_slot}");
    }
    Ok(())
}

pub fn validate_partition_size(size: u64, image_size: u64) -> Result<()> {
    if !(64 * 1024 * 1024..=256 * 1024 * 1024).contains(&size)
        || !size.is_multiple_of(4096)
    {
        bail!("vendor_boot partition size is implausible: {size}");
    }
    if size != image_size {
        bail!("block device size {size} differs from image size {image_size}");
    }
    Ok(())
}

pub fn block_device_size(file: &File) -> Result<u64> {
    let metadata_len = file.metadata()?.len();
    if metadata_len > 0 {
        return Ok(metadata_len);
    }
    let mut size = 0_u64;
    // BLKGETSIZE64 is Linux's read-only block-size ioctl.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), 0x8008_1272_u64, &mut size) };
    if rc != 0 || size == 0 {
        bail!("BLKGETSIZE64 failed");
    }
    Ok(size)
}

pub fn validate_block_device(path: &Path, expected_name: &str) -> Result<PathBuf> {
    let expected = Path::new("/dev/block/by-name").join(expected_name);
    if path != expected {
        bail!("partition path is outside the fixed allowlist");
    }
    let resolved = fs::canonicalize(path).context("resolve block-device symlink")?;
    let metadata = fs::metadata(&resolved)?;
    if !std::os::unix::fs::FileTypeExt::is_block_device(&metadata.file_type()) {
        bail!("resolved target is not a block device: {}", resolved.display());
    }
    Ok(resolved)
}

pub fn stream_copy_exact<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    expected_size: u64,
) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("stream byte count overflow")?;
        if total > expected_size {
            bail!("source exceeds expected size {expected_size}");
        }
        writer.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    if total != expected_size {
        bail!("copied {total} bytes, expected {expected_size}");
    }
    writer.flush()?;
    Ok(hex_upper(hasher.finalize()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    stream_hash(file, size)
}

pub fn stream_hash<R: Read>(mut reader: R, expected_size: u64) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > expected_size {
            bail!("input exceeds expected size");
        }
        hasher.update(&buffer[..read]);
    }
    if total != expected_size {
        bail!("hashed {total} bytes, expected {expected_size}");
    }
    Ok(hex_upper(hasher.finalize()))
}

pub fn inspect_image(path: &Path) -> Result<FdtReport> {
    let file = File::open(path)?;
    let size = usize::try_from(file.metadata()?.len())?;
    inspect_open_file(&file, size)
}

pub fn inspect_image_sized(path: &Path, size: u64) -> Result<FdtReport> {
    let file = File::open(path)?;
    inspect_open_file(&file, usize::try_from(size)?)
}

fn inspect_open_file(file: &File, size: usize) -> Result<FdtReport> {
    if size == 0 {
        bail!("cannot inspect an empty image");
    }
    let map = unsafe { MmapOptions::new().len(size).map(file)? };
    validate_supported_fdt_set(&map)
}

pub fn patch_image(input: &Path, output: &Path) -> Result<PatchReport> {
    let input_file = File::open(input)?;
    let input_size = input_file.metadata()?.len();
    let input_map = unsafe { Mmap::map(&input_file)? };
    let source = validate_supported_fdt_set(&input_map)?;
    let input_sha256 = sha256_file(input)?;

    if source.region == Region::Prc {
        fs::copy(input, output)?;
        return Ok(PatchReport {
            source_region: Region::Prc,
            target_region: Region::Prc,
            supported_fdt_count: 3,
            tuna_count: 2,
            tunap_count: 1,
            changed_byte_count: 0,
            changed_offsets: vec![],
            input_size,
            output_size: input_size,
            input_sha256: input_sha256.clone(),
            output_sha256: input_sha256,
            already_prc: true,
        });
    }
    drop(input_map);

    let output_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(output)?;
    stream_copy_exact(File::open(input)?, &output_file, input_size)?;
    output_file.sync_all()?;

    let mut output_map = unsafe { MmapMut::map_mut(&output_file)? };
    let before = validate_supported_fdt_set(&output_map)?;
    let mut changed_offsets = Vec::with_capacity(9);
    for value_offset in &before.region_value_offsets {
        let start = usize::try_from(*value_offset)?;
        if output_map.get(start..start + 4) != Some(b"ROW\0") {
            bail!("region,country changed before patch at 0x{start:X}");
        }
        for delta in 0..3 {
            if b"ROW"[delta] != b"PRC"[delta] {
                changed_offsets.push((start + delta) as u64);
            }
        }
        output_map[start..start + 4].copy_from_slice(b"PRC\0");
    }
    output_map.flush()?;
    drop(output_map);
    output_file.sync_all()?;

    let output_size = output_file.metadata()?.len();
    if input_size != output_size || changed_offsets.len() != 9 {
        bail!(
            "unsafe patch: size {input_size}->{output_size}, changed bytes {}",
            changed_offsets.len()
        );
    }
    let verified = inspect_image(output)?;
    if verified.region != Region::Prc
        || verified.supported_fdt_count != 3
        || verified.tuna_count != 2
        || verified.tunap_count != 1
    {
        bail!("patched image failed FDT revalidation");
    }
    let output_sha256 = sha256_file(output)?;
    Ok(PatchReport {
        source_region: Region::Row,
        target_region: Region::Prc,
        supported_fdt_count: 3,
        tuna_count: 2,
        tunap_count: 1,
        changed_byte_count: 9,
        changed_offsets,
        input_size,
        output_size,
        input_sha256,
        output_sha256,
        already_prc: false,
    })
}

fn validate_supported_fdt_set(data: &[u8]) -> Result<FdtReport> {
    let mut supported = Vec::new();
    let mut cursor = 0_usize;
    while let Some(relative) = find_subslice(&data[cursor..], &FDT_MAGIC_BYTES) {
        let base = cursor + relative;
        let Some(bounds) = parse_fdt_bounds(data, base) else {
            cursor = base.saturating_add(4);
            continue;
        };
        if let Some(fdt) = parse_supported_fdt(data, bounds)? {
            supported.push(fdt);
        }
        cursor = bounds.total_end;
    }
    if supported.len() != 3 {
        bail!(
            "expected exactly 3 supported Tuna FDTs, found {}",
            supported.len()
        );
    }
    let tuna_count = supported.iter().filter(|f| f.board == Board::Tuna).count();
    let tunap_count = supported
        .iter()
        .filter(|f| f.board == Board::Tunap)
        .count();
    if tuna_count != 2 || tunap_count != 1 {
        bail!("expected qcom,tuna=2 and qcom,tunap=1, found {tuna_count}/{tunap_count}");
    }
    let region = supported[0].region;
    if supported.iter().any(|f| f.region != region) {
        bail!("supported FDT region,country values are mixed");
    }
    Ok(FdtReport {
        region,
        supported_fdt_count: supported.len(),
        tuna_count,
        tunap_count,
        region_value_offsets: supported
            .iter()
            .map(|f| f.region_value_start as u64)
            .collect(),
    })
}

fn parse_fdt_bounds(data: &[u8], base: usize) -> Option<FdtBounds> {
    let header_end = base.checked_add(FDT_HEADER_SIZE)?;
    if header_end > data.len() || be32(data.get(base..base + 4)?) != FDT_MAGIC {
        return None;
    }
    let total_size = be32(data.get(base + 4..base + 8)?) as usize;
    let struct_offset = be32(data.get(base + 8..base + 12)?) as usize;
    let strings_offset = be32(data.get(base + 12..base + 16)?) as usize;
    let strings_size = be32(data.get(base + 32..base + 36)?) as usize;
    let struct_size = be32(data.get(base + 36..base + 40)?) as usize;
    if total_size < FDT_HEADER_SIZE {
        return None;
    }
    let total_end = base.checked_add(total_size)?;
    let struct_start = base.checked_add(struct_offset)?;
    let struct_end = struct_start.checked_add(struct_size)?;
    let strings_start = base.checked_add(strings_offset)?;
    let strings_end = strings_start.checked_add(strings_size)?;
    if total_end > data.len()
        || struct_start < header_end
        || struct_end > total_end
        || strings_start < header_end
        || strings_end > total_end
    {
        return None;
    }
    Some(FdtBounds {
        base,
        total_end,
        struct_start,
        struct_end,
        strings_start,
        strings_end,
    })
}

fn parse_supported_fdt(data: &[u8], bounds: FdtBounds) -> Result<Option<SupportedFdt>> {
    let mut pos = bounds.struct_start;
    let mut depth = 0_usize;
    let mut compatible: Option<Board> = None;
    let mut region: Option<(Region, usize)> = None;
    let mut saw_end = false;
    while pos + 4 <= bounds.struct_end {
        let token = be32(&data[pos..pos + 4]);
        pos += 4;
        match token {
            FDT_BEGIN_NODE => {
                let end = find_nul(data, pos, bounds.struct_end).context("unterminated node")?;
                pos = align_fdt_offset(end + 1, bounds.base).context("node alignment overflow")?;
                depth += 1;
            }
            FDT_END_NODE => depth = depth.checked_sub(1).context("unexpected FDT_END_NODE")?,
            FDT_PROP => {
                if pos + 8 > bounds.struct_end {
                    bail!("truncated FDT property");
                }
                let value_len = be32(&data[pos..pos + 4]) as usize;
                let name_offset = be32(&data[pos + 4..pos + 8]) as usize;
                pos += 8;
                let value_start = pos;
                let value_end = value_start
                    .checked_add(value_len)
                    .context("property length overflow")?;
                if value_end > bounds.struct_end {
                    bail!("property outside structure block");
                }
                if depth == 1 {
                    let name_start = bounds
                        .strings_start
                        .checked_add(name_offset)
                        .context("name offset overflow")?;
                    if name_start >= bounds.strings_end {
                        bail!("property name outside strings block");
                    }
                    let name_end = find_nul(data, name_start, bounds.strings_end)
                        .context("unterminated property name")?;
                    let name = &data[name_start..name_end];
                    let value = &data[value_start..value_end];
                    if name == b"compatible" {
                        if compatible.is_some() {
                            bail!("duplicate root compatible");
                        }
                        compatible = classify_compatible(value);
                    } else if name == b"region,country" {
                        if region.is_some() {
                            bail!("duplicate root region,country");
                        }
                        region = Some((
                            match value {
                                b"ROW\0" => Region::Row,
                                b"PRC\0" => Region::Prc,
                                _ => bail!("unsupported root region,country value"),
                            },
                            value_start,
                        ));
                    }
                }
                pos = align_fdt_offset(value_end, bounds.base)
                    .context("property alignment overflow")?;
            }
            FDT_NOP => {}
            FDT_END => {
                saw_end = true;
                break;
            }
            other => bail!("unknown FDT token {other}"),
        }
    }
    if !saw_end || depth != 0 {
        bail!("malformed FDT structure");
    }
    match (compatible, region) {
        (Some(board), Some((region, region_value_start))) => Ok(Some(SupportedFdt {
            board,
            region,
            region_value_start,
        })),
        (Some(_), None) => bail!("supported Tuna FDT lacks root region,country"),
        _ => Ok(None),
    }
}

fn classify_compatible(value: &[u8]) -> Option<Board> {
    let mut board = None;
    for item in value.split(|byte| *byte == 0).filter(|item| !item.is_empty()) {
        let candidate = match item {
            b"qcom,tuna" => Some(Board::Tuna),
            b"qcom,tunap" => Some(Board::Tunap),
            _ => None,
        };
        if candidate.is_some() {
            if board.is_some() {
                return None;
            }
            board = candidate;
        }
    }
    board
}

fn align_fdt_offset(value: usize, base: usize) -> Option<usize> {
    let relative = value.checked_sub(base)?;
    base.checked_add(relative.checked_add(3)? & !3)
}

fn find_nul(data: &[u8], start: usize, end: usize) -> Option<usize> {
    data.get(start..end)?
        .iter()
        .position(|byte| *byte == 0)
        .map(|relative| start + relative)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("validated 4-byte slice"))
}

fn hex_upper(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

pub fn create_new_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?)
}

pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state")
    ));
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(&temporary, bytes)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
