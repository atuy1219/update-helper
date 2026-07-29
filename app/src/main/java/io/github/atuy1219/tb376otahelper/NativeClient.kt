package io.github.atuy1219.tb376otahelper

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.io.File

class NativeClient(private val context: Context) {
    companion object {
        const val ROOT = "/data/adb/tb376-ota-helper"
        const val BINARY = "$ROOT/bin/tb376-ota-helper-native"
        const val STATE = "$ROOT/state.json"
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

    suspend fun verify(slot: String, image: String): NativeResult =
        execute("verify", "--slot", slot, "--image", image, "--json")

    suspend fun journal(): JsonObject? = withContext(Dispatchers.IO) {
        val result = rootExec("/system/bin/cat", STATE)
        if (result.exitCode == 0) runCatching { json.parseToJsonElement(result.stdout).jsonObject }.getOrNull()
        else null
    }

    suspend fun reboot(): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { rootExec("/system/bin/reboot").requireSuccess() }
    }

    private suspend fun execute(vararg args: String): NativeResult = withContext(Dispatchers.IO) {
        val command = mutableListOf("su", "0", BINARY)
        command.addAll(args)
        val process = ProcessBuilder(command)
            .redirectErrorStream(false)
            .start()
        val stdout = process.inputStream.bufferedReader().readLines()
        val stderr = process.errorStream.bufferedReader().readText()
        val exit = process.waitFor()
        val events = stdout.mapNotNull { line ->
            runCatching { json.parseToJsonElement(line).jsonObject }.getOrNull()
        }
        val error = events.lastOrNull { it["type"]?.jsonPrimitive?.content == "error" }
            ?.get("data")?.jsonObject?.get("message")?.jsonPrimitive?.content
        NativeResult(
            exitCode = exit,
            events = events,
            result = events.lastOrNull { it["type"]?.jsonPrimitive?.content == "result" }
                ?.get("data")?.jsonObject,
            error = error ?: stderr.ifBlank { null },
        )
    }

    private fun rootExec(vararg args: String): ProcessResult {
        val command = mutableListOf("su", "0")
        command.addAll(args)
        val process = ProcessBuilder(command).start()
        val stdout = process.inputStream.bufferedReader().readText()
        val stderr = process.errorStream.bufferedReader().readText()
        return ProcessResult(process.waitFor(), stdout, stderr)
    }

    private fun ProcessResult.requireSuccess() {
        check(exitCode == 0) { stderr.ifBlank { "root command failed ($exitCode)" } }
    }
}

data class NativeResult(
    val exitCode: Int,
    val events: List<JsonObject>,
    val result: JsonObject?,
    val error: String?,
) {
    val isSuccess: Boolean get() = exitCode == 0 && result != null
}

private data class ProcessResult(val exitCode: Int, val stdout: String, val stderr: String)

fun JsonObject.string(path: String): String? =
    this[path]?.jsonPrimitive?.content

fun JsonObject.obj(path: String): JsonObject? =
    this[path]?.let { it.jsonObject }
