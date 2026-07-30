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
            status = "端末検査が完了しました",
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
            status = if (data.obj("operation")?.bool("already_prc") == true) {
                "既にPRC化済みです。全体SHA-256を検証しました"
            } else {
                "書込みとパーティション全体の読戻し検証に成功しました"
            },
            error = null,
        )
    }

    fun restore() = launchOperation("stockバックアップを復元中") {
        val operation = _state.value.recoveryJournal ?: _state.value.operation
            ?: error("復元journalがありません")
        val slot = operation.string("next_boot_slot") ?: error("slot不明")
        val dir = operation.string("backup_dir") ?: error("backup不明")
        val result = native.restore(slot, "$dir/vendor_boot_${slot}-stock.img")
        check(result.isSuccess) { result.error ?: "restore failed" }
        val data = result.result!!
        _state.value = _state.value.copy(
            operation = data.obj("operation"),
            recoveryJournal = null,
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

    private fun launchOperation(label: String, block: suspend () -> Unit) {
        if (_state.value.busy) return
        viewModelScope.launch {
            _state.value = _state.value.copy(busy = true, status = label, error = null)
            runCatching { block() }.onFailure { error ->
                _state.value = _state.value.copy(
                    rootAvailable = if (error.message?.contains("root", true) == true) false else _state.value.rootAvailable,
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
    )

fun JsonObject.bool(name: String): Boolean? =
    this[name]?.jsonPrimitive?.booleanOrNull

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
