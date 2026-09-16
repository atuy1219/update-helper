package io.github.atuy1219.tb376otahelper

import android.content.ContentResolver
import android.net.Uri
import android.provider.DocumentsContract
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.IOException

class BackupExporter(private val resolver: ContentResolver) {
    suspend fun export(tree: Uri, backupDir: String, slot: String): Result<Uri> =
        withContext(Dispatchers.IO) {
            runCatching {
                require(isSafeBackupDir(backupDir))
                require(slot == "a" || slot == "b")
                val rootId = DocumentsContract.getTreeDocumentId(tree)
                val root = DocumentsContract.buildDocumentUriUsingTree(tree, rootId)
                val top = ensureDirectory(root, "TB376-OTA-Backups")
                val folder = ensureDirectory(top, backupDir.substringAfterLast('/'))
                val names = listOf(
                    "vendor_boot_${slot}-stock.img",
                    "vendor_boot_${slot}-stock.img.sha256",
                    "vendor_boot_${slot}-prc.img",
                    "vendor_boot_${slot}-prc.img.sha256",
                    "operation.json",
                    "operation.log",
                    "device-info.json",
                )
                names.forEach { name ->
                    val mime = if (name.endsWith(".img")) "application/octet-stream" else "text/plain"
                    val target = DocumentsContract.createDocument(resolver, folder, mime, name)
                        ?: throw IOException("SAF document creation failed: $name")
                    val process = ProcessBuilder(
                        "su", "0", "/system/bin/cat", "$backupDir/$name",
                    ).start()
                    resolver.openOutputStream(target, "w").use { output ->
                        requireNotNull(output)
                        process.inputStream.use { input -> input.copyTo(output, 1024 * 1024) }
                    }
                    val stderr = process.errorStream.bufferedReader().readText()
                    check(process.waitFor() == 0) { stderr.ifBlank { "root read failed: $name" } }
                }
                folder
            }
        }

    private fun ensureDirectory(parent: Uri, name: String): Uri {
        val children = DocumentsContract.buildChildDocumentsUriUsingTree(
            parent,
            DocumentsContract.getDocumentId(parent),
        )
        resolver.query(
            children,
            arrayOf(DocumentsContract.Document.COLUMN_DOCUMENT_ID, DocumentsContract.Document.COLUMN_DISPLAY_NAME),
            null,
            null,
            null,
        )?.use { cursor ->
            while (cursor.moveToNext()) {
                if (cursor.getString(1) == name) {
                    return DocumentsContract.buildDocumentUriUsingTree(parent, cursor.getString(0))
                }
            }
        }
        return DocumentsContract.createDocument(
            resolver,
            parent,
            DocumentsContract.Document.MIME_TYPE_DIR,
            name,
        ) ?: throw IOException("SAF directory creation failed: $name")
    }
}

internal fun isSafeBackupDir(path: String): Boolean =
    path.matches(Regex("^/data/adb/tb376-ota-helper/backups/[0-9]+-slot-[ab]$"))
