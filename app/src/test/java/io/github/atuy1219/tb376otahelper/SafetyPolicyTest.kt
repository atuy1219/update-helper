package io.github.atuy1219.tb376otahelper

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SafetyPolicyTest {
    private val json = Json

    @Test
    fun noRootDisablesWrite() {
        assertFalse(canPatch(state(root = false)))
        assertFalse(canPrepareIncrementalOta(state(root = false, current = "a", next = "a")))
        assertFalse(canRestoreCurrentStock(state(root = false, current = "a", next = "a")))
    }

    @Test
    fun lockedBootloaderDisablesWrite() {
        assertFalse(canPatch(state(unlocked = false)))
        assertFalse(canPrepareIncrementalOta(state(unlocked = false, current = "a", next = "a")))
        assertFalse(canRestoreCurrentStock(state(unlocked = false, current = "a", next = "a")))
    }

    @Test
    fun sameNextBootSlotDisablesPatch() {
        assertFalse(canPatch(state(current = "a", next = "a")))
    }

    @Test
    fun incrementalOtaPrepRequiresCurrentSlotAndKernelSu() {
        assertTrue(canPrepareIncrementalOta(state(current = "a", next = "a", region = "PRC")))
        assertTrue(canPrepareIncrementalOta(state(current = "a", next = "a", region = "ROW")))
        assertFalse(canPrepareIncrementalOta(state(current = "a", next = "b", region = "PRC")))
        assertFalse(canPrepareIncrementalOta(state(current = "a", next = "a", region = "PRC", kernelsu = false)))
    }

    @Test
    fun currentStockRestoreRequiresNoPendingOtaAndPrcRegion() {
        assertTrue(canRestoreCurrentStock(state(current = "a", next = "a", region = "PRC")))
        assertFalse(canRestoreCurrentStock(state(current = "a", next = "b", region = "PRC")))
        assertFalse(canRestoreCurrentStock(state(current = "a", next = "a", region = "ROW")))
    }

    @Test
    fun operationInProgressDisablesReentry() {
        assertFalse(canPatch(state(busy = true)))
        assertFalse(canPrepareIncrementalOta(state(busy = true, current = "a", next = "a")))
        assertFalse(canRestoreCurrentStock(state(busy = true, current = "a", next = "a")))
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
        assertTrue(isRecoveryRequired(objectOf("""{"status":"current_stock_restore_writing"}""")))
        assertFalse(isRecoveryRequired(objectOf("""{"status":"success"}""")))
        assertFalse(isRecoveryRequired(objectOf("""{"status":"current_stock_restore_success"}""")))
    }

    @Test
    fun safExportAcceptsOnlyFixedRootBackupFolder() {
        assertTrue(isSafeBackupDir("/data/adb/tb376-ota-helper/backups/123-slot-b"))
        assertFalse(isSafeBackupDir("/data/local/tmp/123-slot-b"))
        assertFalse(isSafeBackupDir("/data/adb/tb376-ota-helper/backups/../state"))
    }

    @Test
    fun suResolutionUsesAbsoluteAndroidPathsBeforePathAndFallback() {
        assertEquals(
            listOf(
                "/system/bin/su",
                "/system/xbin/su",
                "/sbin/su",
                "/debug_ramdisk/su",
                "/vendor/bin/su",
                "/product/bin/su",
                "su",
            ),
            suCandidates("/vendor/bin:/product/bin"),
        )
    }

    @Test
    fun suResolutionRemovesDuplicatePathCandidates() {
        val candidates = suCandidates("/system/bin:/system/bin")
        assertEquals(1, candidates.count { it == "/system/bin/su" })
    }

    private fun state(
        root: Boolean = true,
        unlocked: Boolean = true,
        current: String = "a",
        next: String = "b",
        busy: Boolean = false,
        region: String = "PRC",
        kernelsu: Boolean = true,
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
                "kernelsu_next_present":$kernelsu,
                "current_slot":"$current",
                "next_boot_slot":"$next"
            }""",
        ),
        fdt = objectOf("""{"region":"$region"}"""),
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
