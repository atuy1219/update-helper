package io.github.atuy1219.tb376otahelper

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.io.File
import java.io.IOException

class NativeClient(private val context: Context) {
    companion object {
        const val ROOT = "/data/adb/tb376-ota-helper"
        const val BINARY = "$ROOT/bin/tb376-ota-helper-native"
        const val STATE = "$ROOT/state.json"
        private const val KSU_PREP_DIR = "$ROOT/ksu-ota-prep"
        private const val OTA_IDLE = "UPDATE_STATUS_IDLE"
        private val KSU_DAEMON_CANDIDATES = listOf(
            "/data/adb/ksud",
            "/data/adb/ksu/bin/ksud",
        )
        private val KSU_BACKUP_LINE = Regex(
            "(?m)^- Using backup file (/data/adb/ksu/ksun_backup_([0-9a-fA-F]{40}))\\s*$",
        )
        private val OTA_STATUS_LINE = Regex(
            "onStatusUpdate\\((UPDATE_STATUS_[A-Z0-9_]+)\\s",
        )
        private val HEX64 = Regex("^[0-9a-fA-F]{64}$")
        private val HEX40 = Regex("^[0-9a-fA-F]{40}$")
    }

    private val json = Json { ignoreUnknownKeys = true }

    suspend fun install(): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching {
            val source = File(context.applicationInfo.nativeLibraryDir, "libtb376_ota_helper_native.so")
            require(source.isFile) { "APKにarm64-v8a rootヘルパーが同梱されていません" }
            rootExec("/system/bin/mkdir", "-p", "$ROOT/bin").requireSuccess()
            rootExec("/system/bin/cp", source.absolutePath, BINARY).requireSuccess()
            rootExec("/system/bin/chmod", "0700", BINARY).requireSuccess()
        }
    }

    suspend fun inspect(): NativeResult = execute("inspect", "--json")

    suspend fun dryRun(slot: String, backupDir: String? = null): NativeResult {
        val args = mutableListOf("dry-run", "--slot", slot, "--json")
        backupDir?.let { args += listOf("--backup-dir", it) }
        return execute(*args.toTypedArray())
    }

    suspend fun patch(slot: String, backupDir: String): NativeResult =
        execute("patch", "--slot", slot, "--backup-dir", backupDir, "--json")

    suspend fun restore(slot: String, backup: String): NativeResult =
        execute("restore", "--slot", slot, "--backup", backup, "--json")

    suspend fun restoreCurrentStock(): NativeResult =
        execute("restore-current-stock", "--json")

    suspend fun restoreKernelSuCurrentStock(
        slot: String,
        buildFingerprint: String,
    ): Result<KernelSuStockResult> = withContext(Dispatchers.IO) {
        runCatching {
            require(slot in setOf("a", "b")) { "slot must be a or b" }
            require(buildFingerprint.isNotBlank()) { "build fingerprintが取得できません" }
            rootExec("/system/bin/mkdir", "-p", KSU_PREP_DIR).requireSuccess()

            val ksud = KSU_DAEMON_CANDIDATES.firstOrNull(::rootExecutableExists)
                ?: error("KernelSU Nextのksudを /data/adb から検出できません")
            val partitions = listOf("init_boot", "boot")
            var lastFailure: String? = null

            for (baseName in partitions) {
                val partitionName = "${baseName}_$slot"
                val partition = "/dev/block/by-name/$partitionName"
                if (!rootBlockExists(partition)) continue

                val candidate = "$KSU_PREP_DIR/$partitionName-stock.img"
                val metaPath = "$KSU_PREP_DIR/$partitionName.meta.json"
                val currentHash = rootDigest("sha256sum", partition, HEX64)

                readKsuPrepMeta(metaPath)?.takeIf {
                    it.string("slot") == slot &&
                        it.string("partition") == partitionName &&
                        it.string("build_fingerprint") == buildFingerprint &&
                        it.string("stock_sha256")?.matches(HEX64) == true &&
                        rootFileExists(candidate)
                }?.let { meta ->
                    val stockHash = meta.string("stock_sha256")!!.uppercase()
                    if (rootDigest("sha256sum", candidate, HEX64) == stockHash) {
                        if (currentHash == stockHash) {
                            return@runCatching KernelSuStockResult(
                                partition = partitionName,
                                stockSha256 = stockHash,
                                alreadyStock = true,
                            )
                        }
                        val sourceHash = meta.string("source_sha256")?.uppercase()
                        val status = meta.string("status")
                        val recoverableInterruptedWrite = status in setOf("writing", "failed")
                        if (currentHash == sourceHash || recoverableInterruptedWrite) {
                            writeKsuPrepMeta(
                                metaPath,
                                meta.toMutableMap().let { values ->
                                    buildJsonObject {
                                        values.forEach { (key, value) -> put(key, value) }
                                        put("status", "writing")
                                    }
                                },
                            )
                            val restored = flashAndVerify(candidate, partition, stockHash)
                            writeKsuPrepMeta(
                                metaPath,
                                buildJsonObject {
                                    meta.forEach { (key, value) -> put(key, value) }
                                    put("status", "success")
                                },
                            )
                            return@runCatching KernelSuStockResult(
                                partition = partitionName,
                                stockSha256 = restored,
                                alreadyStock = false,
                            )
                        }
                    }
                }

                rootExec("/system/bin/rm", "-f", candidate).requireSuccess()
                val restore = rootExec(
                    ksud,
                    "boot-restore",
                    "--boot", partition,
                    "--out", KSU_PREP_DIR,
                    "--out-name", File(candidate).name,
                )
                if (restore.exitCode != 0) {
                    lastFailure = (restore.stderr + "\n" + restore.stdout).trim()
                    continue
                }

                val backupMatch = KSU_BACKUP_LINE.find(restore.stdout)
                    ?: error(
                        "KernelSU Nextは${partitionName}を復元可能と判定しましたが、" +
                            "元のstockバックアップを使用していません。差分OTA用なので再構築イメージは書き込みません",
                    )
                val backup = backupMatch.groupValues[1]
                val expectedSha1 = backupMatch.groupValues[2]
                require(backup == "/data/adb/ksu/ksun_backup_$expectedSha1") {
                    "KernelSU backup path is outside the fixed allowlist"
                }
                require(rootFileExists(backup) && rootFileExists(candidate)) {
                    "KernelSU stock backup/candidateが見つかりません"
                }
                val actualSha1 = rootDigest("sha1sum", backup, HEX40)
                require(actualSha1.equals(expectedSha1, ignoreCase = true)) {
                    "KernelSU stock backup SHA-1が埋め込みIDと一致しません"
                }
                val backupSha256 = rootDigest("sha256sum", backup, HEX64)
                val candidateSha256 = rootDigest("sha256sum", candidate, HEX64)
                require(candidateSha256 == backupSha256) {
                    "KernelSU restore candidateとstock backupのSHA-256が一致しません"
                }

                val preparedMeta = buildJsonObject {
                    put("slot", slot)
                    put("partition", partitionName)
                    put("build_fingerprint", buildFingerprint)
                    put("source_sha256", currentHash)
                    put("stock_sha256", backupSha256)
                    put("backup_path", backup)
                    put("status", "prepared")
                }
                writeKsuPrepMeta(metaPath, preparedMeta)
                writeKsuPrepMeta(
                    metaPath,
                    buildJsonObject {
                        preparedMeta.forEach { (key, value) -> put(key, value) }
                        put("status", "writing")
                    },
                )

                val restored = runCatching {
                    flashAndVerify(candidate, partition, backupSha256)
                }.onFailure {
                    writeKsuPrepMeta(
                        metaPath,
                        buildJsonObject {
                            preparedMeta.forEach { (key, value) -> put(key, value) }
                            put("status", "failed")
                        },
                    )
                }.getOrThrow()

                require(rootDigest("sha256sum", backup, HEX64) == backupSha256) {
                    "KernelSU stock backupが処理中に変更されました"
                }
                writeKsuPrepMeta(
                    metaPath,
                    buildJsonObject {
                        preparedMeta.forEach { (key, value) -> put(key, value) }
                        put("status", "success")
                    },
                )
                return@runCatching KernelSuStockResult(
                    partition = partitionName,
                    stockSha256 = restored,
                    alreadyStock = false,
                )
            }

            error(
                "KernelSU Nextでpatchされたinit_boot/bootを特定できません。" +
                    "既にstockへ戻した場合はHelperが作成した検証済みcandidateが必要です" +
                    (lastFailure?.let { ": $it" } ?: ""),
            )
        }
    }

    suspend fun verify(slot: String, image: String): NativeResult =
        execute("verify", "--slot", slot, "--image", image, "--json")

    suspend fun journal(): JsonObject? = withContext(Dispatchers.IO) {
        val result = runCatching { rootExec("/system/bin/cat", STATE) }.getOrNull()
            ?: return@withContext null
        if (result.exitCode == 0) runCatching { json.parseToJsonElement(result.stdout).jsonObject }.getOrNull()
        else null
    }

    suspend fun reboot(): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { rootExec("/system/bin/reboot").requireSuccess() }
    }

    private fun flashAndVerify(candidate: String, partition: String, expectedSha256: String): String {
        val before = rootDigest("sha256sum", candidate, HEX64)
        require(before == expectedSha256) { "書込み直前にstock candidateが変更されました" }
        requireUpdateEngineIdle()
        return withWritableBlockDevice(partition) {
            rootExec(
                "/system/bin/toybox",
                "dd",
                "if=$candidate",
                "of=$partition",
                "bs=1048576",
            ).requireSuccess()
            rootExec("/system/bin/sync").requireSuccess()
            val candidateAfter = rootDigest("sha256sum", candidate, HEX64)
            require(candidateAfter == expectedSha256) { "書込み中にstock candidateが変更されました" }
            val readback = rootDigest("sha256sum", partition, HEX64)
            require(readback == expectedSha256) {
                "${File(partition).name}全体のSHA-256読戻しがstockと一致しません。再起動しないでください"
            }
            readback
        }
    }

    private fun blockDeviceReadOnly(partition: String): Boolean {
        val result = rootExec("/system/bin/toybox", "blockdev", "--getro", partition)
        result.requireSuccess()
        return when (val value = result.stdout.trim()) {
            "0" -> false
            "1" -> true
            else -> error("blockdev --getroの出力が不正です (" + partition + "): " + value)
        }
    }

    private fun setBlockDeviceReadOnly(partition: String, readOnly: Boolean) {
        val flag = if (readOnly) "--setro" else "--setrw"
        rootExec("/system/bin/toybox", "blockdev", flag, partition).requireSuccess()
        val action = if (readOnly) "復元" else "解除"
        check(blockDeviceReadOnly(partition) == readOnly) {
            File(partition).name + "のread-only状態を" + action + "できません"
        }
    }

    private fun <T> withWritableBlockDevice(partition: String, block: () -> T): T {
        val wasReadOnly = blockDeviceReadOnly(partition)
        if (wasReadOnly) setBlockDeviceReadOnly(partition, false)

        val operation = runCatching(block)
        val restore = if (wasReadOnly) {
            runCatching { setBlockDeviceReadOnly(partition, true) }
        } else {
            Result.success(Unit)
        }

        operation.exceptionOrNull()?.let { operationError ->
            restore.exceptionOrNull()?.let(operationError::addSuppressed)
            throw operationError
        }
        restore.getOrThrow()
        return operation.getOrThrow()
    }

    private fun requireUpdateEngineIdle() {
        val result = rootExec(
            "/system/bin/toybox",
            "timeout",
            "2",
            "/system/bin/update_engine_client",
            "--follow",
        )
        val combined = result.stdout + "\n" + result.stderr
        val status = OTA_STATUS_LINE.find(combined)?.groupValues?.get(1)
        require(status == OTA_IDLE) {
            if (status == null) {
                "update_engineの現在状態を取得できないためactive slotへの書込みを拒否しました"
            } else {
                "update_engineが${status}のためactive slotへの書込みを拒否しました"
            }
        }
    }

    private fun readKsuPrepMeta(path: String): JsonObject? {
        if (!rootFileExists(path)) return null
        val result = rootExec("/system/bin/cat", path)
        if (result.exitCode != 0) return null
        return runCatching { json.parseToJsonElement(result.stdout).jsonObject }.getOrNull()
    }

    private fun writeKsuPrepMeta(path: String, value: JsonObject) {
        val temp = File.createTempFile("tb376-ksu-prep-", ".json", context.cacheDir)
        try {
            temp.writeText(value.toString())
            rootExec("/system/bin/cp", temp.absolutePath, path).requireSuccess()
            rootExec("/system/bin/chmod", "0600", path).requireSuccess()
        } finally {
            temp.delete()
        }
    }

    private fun rootDigest(applet: String, path: String, format: Regex): String {
        val result = rootExec("/system/bin/toybox", applet, path)
        result.requireSuccess()
        val digest = result.stdout.trim().split(Regex("\\s+"), limit = 2).firstOrNull().orEmpty()
        require(digest.matches(format)) { "$applet output is invalid for $path" }
        return digest.uppercase()
    }

    private fun rootExecutableExists(path: String): Boolean =
        rootExec("/system/bin/toybox", "test", "-x", path).exitCode == 0

    private fun rootFileExists(path: String): Boolean =
        rootExec("/system/bin/toybox", "test", "-f", path).exitCode == 0

    private fun rootBlockExists(path: String): Boolean =
        rootExec("/system/bin/toybox", "test", "-b", path).exitCode == 0

    private suspend fun execute(vararg args: String): NativeResult = withContext(Dispatchers.IO) {
        val process = startRootProcess(listOf("0", BINARY) + args)
        val stdout = process.inputStream.bufferedReader().readLines()
        val stderr = process.errorStream.bufferedReader().readText()
        val exit = process.waitFor()
        val events = stdout.mapNotNull { line ->
            runCatching { json.parseToJsonElement(line).jsonObject }.getOrNull()
        }
        val error = events.lastOrNull { it["type"]?.jsonPrimitive?.content == "error" }
            ?.get("data")
            ?.let { it as? JsonObject }
            ?.get("message")
            ?.jsonPrimitive
            ?.content
        NativeResult(
            exitCode = exit,
            events = events,
            result = events.lastOrNull { it["type"]?.jsonPrimitive?.content == "result" }
                ?.get("data")
                ?.let { it as? JsonObject },
            error = error ?: stderr.ifBlank { null },
        )
    }

    private fun rootExec(vararg args: String): ProcessResult {
        val process = startRootProcess(listOf("0") + args)
        val stdout = process.inputStream.bufferedReader().readText()
        val stderr = process.errorStream.bufferedReader().readText()
        return ProcessResult(process.waitFor(), stdout, stderr)
    }

    private fun startRootProcess(arguments: List<String>): Process {
        var lastNotFound: IOException? = null
        for (su in suCandidates(System.getenv("PATH"))) {
            try {
                return ProcessBuilder(listOf(su) + arguments)
                    .redirectErrorStream(false)
                    .start()
            } catch (error: IOException) {
                if (!isCommandNotFound(error)) throw error
                lastNotFound = error
            }
        }
        throw RootUnavailableException(
            "KernelSUのsuを検出できません。KernelSU Nextの設定で「SU compatibility」を有効にし、" +
                "本アプリへのroot許可を確認してください",
            lastNotFound,
        )
    }

    private fun ProcessResult.requireSuccess() {
        check(exitCode == 0) { stderr.ifBlank { "root command failed ($exitCode)" } }
    }
}

internal fun suCandidates(path: String?): List<String> =
    (
        listOf(
            "/system/bin/su",
            "/system/xbin/su",
            "/sbin/su",
            "/debug_ramdisk/su",
        ) +
            path.orEmpty()
                .split(File.pathSeparatorChar)
                .filter { it.isNotBlank() }
                .map { "$it/su" } +
            "su"
        ).distinct()

private fun isCommandNotFound(error: IOException): Boolean =
    error.message?.let { "error=2" in it || "No such file or directory" in it } == true

private class RootUnavailableException(message: String, cause: Throwable?) :
    IOException(message, cause)

data class NativeResult(
    val exitCode: Int,
    val events: List<JsonObject>,
    val result: JsonObject?,
    val error: String?,
) {
    val isSuccess: Boolean get() = exitCode == 0 && result != null
}

data class KernelSuStockResult(
    val partition: String,
    val stockSha256: String,
    val alreadyStock: Boolean,
)

private data class ProcessResult(val exitCode: Int, val stdout: String, val stderr: String)

fun JsonObject.string(path: String): String? =
    this[path]
        ?.takeUnless { it is kotlinx.serialization.json.JsonNull }
        ?.jsonPrimitive
        ?.content

fun JsonObject.obj(path: String): JsonObject? =
    this[path] as? JsonObject