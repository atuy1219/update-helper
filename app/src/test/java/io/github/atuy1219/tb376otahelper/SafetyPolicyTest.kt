package io.github.atuy1219.tb376otahelper

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SafetyPolicyTest {
    private val json = Json

    @Test
    fun noRootDisablesWrite() {
        assertFalse(canPatch(state(root = false)))
    }

    @Test
    fun lockedBootloaderDisablesWrite() {
        assertFalse(canPatch(state(unlocked = false)))
    }

    @Test
    fun sameNextBootSlotDisablesWrite() {
        assertFalse(canPatch(state(current = "a", next = "a")))
    }

    @Test
    fun operationInProgressDisablesReentry() {
        assertFalse(canPatch(state(busy = true)))
    }

    @Test
    fun rebootDisabledBeforeFullReadbackSuccess() {
        assertFalse(canReboot(state(operationStatus = "prepared", writeComplete = false, verified = false)))
        assertFalse(canReboot(state(operationStatus = "success", writeComplete = true, verified = false)))
    }

    @Test
    fun rebootEnabledOnlyForMatchingVerifiedSuccess() {
        assertTrue(canReboot(state(operationStatus = "success", writeComplete = true, verified = true)))
    }

    @Test
    fun unfinishedJournalRequiresRecoveryScreen() {
        assertTrue(isRecoveryRequired(objectOf("""{"status":"writing"}""")))
        assertTrue(isRecoveryRequired(objectOf("""{"status":"restore_failed_do_not_reboot"}""")))
        assertFalse(isRecoveryRequired(objectOf("""{"status":"success"}""")))
    }

    @Test
    fun safExportAcceptsOnlyFixedRootBackupFolder() {
        assertTrue(isSafeBackupDir("/data/adb/tb376-ota-helper/backups/123-slot-b"))
        assertFalse(isSafeBackupDir("/data/local/tmp/123-slot-b"))
        assertFalse(isSafeBackupDir("/data/adb/tb376-ota-helper/backups/../state"))
    }

    private fun state(
        root: Boolean = true,
        unlocked: Boolean = true,
        current: String = "a",
        next: String = "b",
        busy: Boolean = false,
        operationStatus: String = "dry_run_success",
        writeComplete: Boolean = false,
        verified: Boolean = false,
    ): UiState = UiState(
        busy = busy,
        rootAvailable = root,
        device = objectOf(
            """{
                "bootloader_unlocked":$unlocked,
                "supported_device":true,
                "current_slot":"$current",
                "next_boot_slot":"$next"
            }""",
        ),
        operation = objectOf(
            """{
                "status":"$operationStatus",
                "next_boot_slot":"$next",
                "write_completed":$writeComplete,
                "readback_verified":$verified
            }""",
        ),
    )

    private fun objectOf(source: String): JsonObject =
        json.parseToJsonElement(source).jsonObject
}
