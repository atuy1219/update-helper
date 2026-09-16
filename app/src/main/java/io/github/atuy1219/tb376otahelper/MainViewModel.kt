package io.github.atuy1219.tb376otahelper

import android.app.Application
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.jsonPrimitive

data class UiState(
    val busy: Boolean = false,
    val rootAvailable: Boolean? = null,
    val device: JsonObject? = null,
    val fdt: JsonObject? = null,
    val operation: JsonObject? = null,
    val recoveryJournal: JsonObject? = null,
    val status: String = "端末を検査してください",
    val error: String? = null,
    val kernelsuPackages: List<String> = emptyList(),
    val exportTree: Uri? = null,
    val otaReady: Boolean = false,
    val ksuStockPartition: String? = null,
    val ksuStockSha256: String? = null,
)

class MainViewModel(application: Application) : AndroidViewModel(application) {
    private val native = NativeClient(application)
    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state.asStateFlow()

    init {
        viewModelScope.launch {
            val journal = native.journal()
            val unfinished = journal?.let(::isRecoveryRequired) == true
            _state.value = _state.value.copy(
                recoveryJournal = journal.takeIf { unfinished },
                kernelsuPackages = findKernelSuManagers(),
            )
        }
    }

    fun inspect() = launchOperation("端末を検査中") {
        native.install().getOrThrow()
        val result = native.inspect()
        check(result.isSuccess) { result.error ?: "inspect failed" }
        val root = result.result!!
        _state.value = _state.value.copy(
            rootAvailable = true,
            device = root.obj("device"),
            fdt = root.obj("fdt"),
            operation = root.obj("journal"),
            otaReady = false,
            ksuStockPartition = null,
            ksuStockSha256 = null,
            status = "端末検査が完了しました",
            error = null,
        )
    }

    fun prepareIncrementalOta() = launchOperation("差分OTA準備: KernelSU stockを検証中") {
        val device = _state.value.device ?: error("先に端末を検査してください")
        val slot = device.string("current_slot") ?: error("current slot不明")
        val next = device.string("next_boot_slot") ?: error("next boot slot不明")
        check(slot == next) { "OTA再起動待ち状態では差分OTA準備を実行できません" }
        check(isUpdateEngineIdle(device)) {
            "update_engineがIDLEではありません。OTAのダウンロード/適用中は現在slotを変更できません"
        }
        val fingerprint = device.string("build_fingerprint") ?: error("build fingerprint不明")
        check(device.bool("supported_device") == true && device.bool("bootloader_unlocked") == true) {
            "対応端末・Unlocked条件を満たしていません"
        }
        val battery = device.string("battery_percent")?.toIntOrNull() ?: error("battery不明")
        val charging = device.bool("charging") == true
        check(battery >= 30) { "battery below 30%; writing is prohibited" }
        check(battery >= 50 || charging) { "battery below 50% and not charging" }

        native.install().getOrThrow()
        val ksu = native.restoreKernelSuCurrentStock(slot, fingerprint).getOrThrow()
        _state.value = _state.value.copy(
            status = "差分OTA準備: ${ksu.partition}はstock検証済み。vendor_bootを復元中",
            ksuStockPartition = ksu.partition,
            ksuStockSha256 = ksu.stockSha256,
            otaReady = false,
        )

        val vendor = native.restoreCurrentStock()
        check(vendor.isSuccess) { vendor.error ?: "current vendor_boot stock restore failed" }

        val verification = native.inspect()
        check(verification.isSuccess) { verification.error ?: "post-restore inspect failed" }
        val data = verification.result!!
        val verifiedDevice = data.obj("device") ?: error("device verification missing")
        val verifiedFdt = data.obj("fdt") ?: error("vendor_boot FDT verification missing")
        check(verifiedDevice.string("current_slot") == verifiedDevice.string("next_boot_slot")) {
            "処理中にnext boot slotが変更されました"
        }
        check(isUpdateEngineIdle(verifiedDevice)) {
            "処理中にupdate_engineがIDLE以外へ遷移しました。OTAを開始せず状態を確認してください"
        }
        check(verifiedFdt.string("region") == "ROW") {
            "vendor_bootがstock ROWとして確認できません"
        }

        _state.value = _state.value.copy(
            rootAvailable = true,
            device = verifiedDevice,
            fdt = verifiedFdt,
            operation = data.obj("journal") ?: vendor.result?.obj("operation") ?: _state.value.operation,
            recoveryJournal = null,
            otaReady = true,
            ksuStockPartition = ksu.partition,
            ksuStockSha256 = ksu.stockSha256,
            status = "差分OTA準備完了。${ksu.partition}とvendor_bootをstockとして全体検証しました。再起動せずOTAを開始してください",
            error = null,
        )
    }

    fun dryRun() = launchOperation("Dry Run中") {
        val slot = requiredTargetSlot()
        val result = native.dryRun(slot)
        check(result.isSuccess) { result.error ?: "dry-run failed" }
        val data = result.result!!
        _state.value = _state.value.copy(
            device = data.obj("device"),
            operation = data.obj("operation"),
            status = "Dry Run成功。パーティションには書き込んでいません",
            error = null,
        )
    }

    fun patch() = launchOperation("バックアップ後に更新先vendor_bootを書込み中") {
        val slot = requiredTargetSlot()
        val backupDir = _state.value.operation?.string("backup_dir")
            ?: error("先にDry Runを実行してください")
        val result = native.patch(slot, backupDir)
        check(result.isSuccess) { result.error ?: "patch failed" }
        val data = result.result!!
        _state.value = _state.value.copy(
            device = data.obj("device"),
            operation = data.obj("operation"),
            recoveryJournal = null,
            otaReady = false,
            status = if (data.obj("operation")?.bool("already_prc") == true) {
                "既にPRC化済みです。全体SHA-256を検証しました"
            } else {
                "書込みとパーティション全体の読戻し検証に成功しました"
            },
            error = null,
        )
    }

    fun restoreCurrentStock() = launchOperation("現在OSのstock vendor_bootを復元中") {
        performCurrentStockRestore()
    }

    fun restore() = launchOperation("stockバックアップを復元中") {
        val operation = _state.value.recoveryJournal ?: _state.value.operation
            ?: error("復元journalがありません")
        if (operation.string("status")?.startsWith("current_stock_restore_") == true) {
            performCurrentStockRestore()
            return@launchOperation
        }
        val slot = operation.string("next_boot_slot") ?: error("slot不明")
        val dir = operation.string("backup_dir") ?: error("backup不明")
        val result = native.restore(slot, "$dir/vendor_boot_${slot}-stock.img")
        check(result.isSuccess) { result.error ?: "restore failed" }
        val data = result.result!!
        _state.value = _state.value.copy(
            operation = data.obj("operation"),
            recoveryJournal = null,
            otaReady = false,
            status = "stockを復元し、全体SHA-256を検証しました",
            error = null,
        )
    }

    fun exportBackup(tree: Uri) = launchOperation("バックアップを書き出し中") {
        val operation = _state.value.operation ?: error("バックアップがありません")
        val dir = operation.string("backup_dir") ?: error("バックアップパス不明")
        val slot = operation.string("next_boot_slot") ?: error("slot不明")
        BackupExporter(getApplication<Application>().contentResolver)
            .export(tree, dir, slot)
            .getOrThrow()
        _state.value = _state.value.copy(exportTree = tree, status = "SAFフォルダへ書き出しました")
    }

    fun reboot() = launchOperation("再起動を要求中") {
        check(canReboot(_state.value)) { "安全な再起動条件を満たしていません" }
        native.reboot().getOrThrow()
    }

    fun openKernelSu(): Boolean {
        val context = getApplication<Application>()
        val packageName = _state.value.kernelsuPackages.firstOrNull() ?: return false
        val intent = context.packageManager.getLaunchIntentForPackage(packageName) ?: return false
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        context.startActivity(intent)
        return true
    }

    private suspend fun performCurrentStockRestore() {
        val device = _state.value.device ?: error("先に端末を検査してください")
        check(isUpdateEngineIdle(device)) {
            "update_engineがIDLEではありません。OTA適用中は現在slotのvendor_bootを変更できません"
        }
        val result = native.restoreCurrentStock()
        check(result.isSuccess) { result.error ?: "current stock restore failed" }
        val data = result.result!!
        val alreadyStock = data.bool("already_stock") == true
        _state.value = _state.value.copy(
            device = data.obj("device") ?: _state.value.device,
            fdt = data.obj("fdt") ?: _state.value.fdt,
            operation = data.obj("operation") ?: _state.value.operation,
            recoveryJournal = null,
            otaReady = false,
            status = if (alreadyStock) {
                "現在OSのvendor_bootは既にstock ROWです。書き込みは行っていません"
            } else {
                "現在OSのstock vendor_bootを復元し、全体SHA-256を検証しました"
            },
            error = null,
        )
    }

    private fun launchOperation(label: String, block: suspend () -> Unit) {
        if (_state.value.busy) return
        viewModelScope.launch {
            _state.value = _state.value.copy(busy = true, status = label, error = null)
            runCatching { block() }.onFailure { error ->
                _state.value = _state.value.copy(
                    rootAvailable = if (error.message?.contains("root", true) == true) false else _state.value.rootAvailable,
                    otaReady = false,
                    error = error.message ?: error.toString(),
                    status = "処理に失敗しました。再起動しないでください",
                )
            }
            _state.value = _state.value.copy(busy = false)
        }
    }

    private fun requiredTargetSlot(): String =
        _state.value.device?.string("next_boot_slot") ?: error("先に端末を検査してください")

    private fun findKernelSuManagers(): List<String> {
        val pm = getApplication<Application>().packageManager
        val intent = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
        return pm.queryIntentActivities(intent, PackageManager.MATCH_ALL)
            .filter {
                val label = it.loadLabel(pm).toString().lowercase()
                val name = it.activityInfo.packageName.lowercase()
                ("kernelsu" in label || "kernel su" in label || "ksu next" in label) &&
                    ("kernel" in name || "ksu" in name)
            }
            .map { it.activityInfo.packageName }
            .distinct()
    }
}

fun isRecoveryRequired(journal: JsonObject): Boolean =
    journal.string("status") !in setOf(
        "success",
        "success_already_prc",
        "restore_success",
        "dry_run_success",
        "current_stock_restore_success",
    )

fun JsonObject.bool(name: String): Boolean? =
    this[name]?.jsonPrimitive?.booleanOrNull

fun isUpdateEngineIdle(device: JsonObject): Boolean =
    device.string("ota_status")
        ?.uppercase()
        ?.contains("UPDATE_STATUS_IDLE") == true

fun canPatch(state: UiState): Boolean {
    val device = state.device ?: return false
    return !state.busy &&
        state.rootAvailable == true &&
        device.bool("bootloader_unlocked") == true &&
        device.bool("supported_device") == true &&
        device.string("current_slot") != device.string("next_boot_slot") &&
        device.string("next_boot_slot") in setOf("a", "b") &&
        state.operation?.string("status") == "dry_run_success"
}

fun canPrepareIncrementalOta(state: UiState): Boolean {
    val device = state.device ?: return false
    return !state.busy &&
        state.rootAvailable == true &&
        device.bool("bootloader_unlocked") == true &&
        device.bool("supported_device") == true &&
        device.bool("kernelsu_next_present") == true &&
        isUpdateEngineIdle(device) &&
        device.string("current_slot") in setOf("a", "b") &&
        device.string("current_slot") == device.string("next_boot_slot") &&
        state.fdt?.string("region") in setOf("ROW", "PRC")
}

fun canRestoreCurrentStock(state: UiState): Boolean {
    val device = state.device ?: return false
    return !state.busy &&
        state.rootAvailable == true &&
        device.bool("bootloader_unlocked") == true &&
        device.bool("supported_device") == true &&
        isUpdateEngineIdle(device) &&
        device.string("current_slot") in setOf("a", "b") &&
        device.string("current_slot") == device.string("next_boot_slot") &&
        state.fdt?.string("region") == "PRC"
}

fun canReboot(state: UiState): Boolean {
    val operation = state.operation ?: return false
    val device = state.device ?: return false
    return !state.busy &&
        state.rootAvailable == true &&
        device.bool("supported_device") == true &&
        operation.bool("write_completed") == true &&
        operation.bool("readback_verified") == true &&
        operation.string("next_boot_slot") == device.string("next_boot_slot") &&
        operation.string("status") in setOf("success", "success_already_prc") &&
        operation.string("status") != "restoring"
}
