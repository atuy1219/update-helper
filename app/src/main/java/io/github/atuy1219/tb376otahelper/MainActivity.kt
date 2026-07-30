package io.github.atuy1219.tb376otahelper

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(Modifier.fillMaxSize()) {
                    HelperApp()
                }
            }
        }
    }
}

@Composable
private fun HelperApp(vm: MainViewModel = viewModel()) {
    val state by vm.state.collectAsStateWithLifecycle()
    if (state.recoveryJournal != null) {
        RecoveryScreen(state, vm::restore)
        return
    }

    var patchConfirm by remember { mutableStateOf(false) }
    var rebootConfirm by remember { mutableStateOf(false) }
    val exportLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocumentTree(),
    ) { uri -> uri?.let(vm::exportBackup) }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("TB376 OTA Helper", style = MaterialTheme.typography.headlineMedium)
        Text(
            "OTA適用後、「再起動してください」で止めた状態の更新先vendor_bootだけをPRC化します。vbmeta・boot・init_boot・superには触れません。",
            style = MaterialTheme.typography.bodyMedium,
        )

        StatusCard(state)

        if (state.busy) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                CircularProgressIndicator(Modifier.padding(end = 12.dp))
                Text(state.status)
            }
        } else {
            Text(state.status, fontWeight = FontWeight.Medium)
        }
        state.error?.let {
            Card(Modifier.fillMaxWidth()) {
                Text(
                    it,
                    color = MaterialTheme.colorScheme.error,
                    modifier = Modifier.padding(12.dp),
                )
            }
        }

        Button(
            onClick = vm::inspect,
            enabled = !state.busy,
            modifier = Modifier.fillMaxWidth(),
        ) { Text("端末を検査") }

        Button(
            onClick = vm::dryRun,
            enabled = !state.busy && state.device != null &&
                state.device?.bool("supported_device") == true &&
                state.device?.string("current_slot") != state.device?.string("next_boot_slot"),
            modifier = Modifier.fillMaxWidth(),
        ) { Text("Dry Run") }

        Button(
            onClick = { patchConfirm = true },
            enabled = canPatch(state),
            modifier = Modifier.fillMaxWidth(),
        ) { Text("更新先vendor_bootをバックアップしてPRC化") }

        OutlinedButton(
            onClick = { exportLauncher.launch(null) },
            enabled = !state.busy && state.operation?.string("backup_dir") != null,
            modifier = Modifier.fillMaxWidth(),
        ) { Text("バックアップを書き出す") }

        OutlinedButton(
            onClick = { vm.openKernelSu() },
            enabled = !state.busy && state.kernelsuPackages.isNotEmpty(),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(
                if (state.kernelsuPackages.isEmpty()) {
                    "KernelSU Next Managerを検出できません"
                } else {
                    "KernelSU Nextを開く（非アクティブスロットにインストール）"
                },
            )
        }

        Button(
            onClick = { rebootConfirm = true },
            enabled = canReboot(state),
            modifier = Modifier.fillMaxWidth(),
        ) { Text("安全に再起動") }

        Text(
            "ブートローダーは絶対に再ロックしないでください。失敗表示または読戻し未検証の状態では再起動しないでください。",
            color = MaterialTheme.colorScheme.error,
            fontWeight = FontWeight.Bold,
        )
    }

    if (patchConfirm) {
        AlertDialog(
            onDismissRequest = { patchConfirm = false },
            title = { Text("更新先vendor_bootへ書き込みます") },
            text = {
                Text("stockバックアップとDry Run結果を再検証後、非アクティブな更新先スロットだけへ書き込み、パーティション全体をSHA-256検証します。")
            },
            confirmButton = {
                TextButton(onClick = {
                    patchConfirm = false
                    vm.patch()
                }) { Text("書込みを実行") }
            },
            dismissButton = {
                TextButton(onClick = { patchConfirm = false }) { Text("キャンセル") }
            },
        )
    }
    if (rebootConfirm) {
        AlertDialog(
            onDismissRequest = { rebootConfirm = false },
            title = { Text("再起動の最終確認") },
            text = { Text("KernelSU Nextの「非アクティブスロットにインストール」も完了しましたか？本アプリはinit_bootを確認・変更しません。") },
            confirmButton = {
                TextButton(onClick = {
                    rebootConfirm = false
                    vm.reboot()
                }) { Text("確認して再起動") }
            },
            dismissButton = {
                TextButton(onClick = { rebootConfirm = false }) { Text("戻る") }
            },
        )
    }
}

@Composable
private fun StatusCard(state: UiState) {
    val d = state.device
    val op = state.operation
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Status("Root", when (state.rootAvailable) {
                true -> "利用可能"
                false -> "不可"
                null -> "未検査"
            })
            Status("Bootloader", d?.bool("bootloader_unlocked")?.let { if (it) "Unlocked" else "Locked" } ?: "未検査")
            Status("対応端末", d?.bool("supported_device")?.let { if (it) "Yes" else "No" } ?: "未検査")
            Status("現在のビルド", d?.string("build_fingerprint") ?: "—")
            Status("現在のスロット", d?.string("current_slot")?.uppercase() ?: "—")
            Status("次回起動スロット", d?.string("next_boot_slot")?.uppercase() ?: "—")
            Status("処理対象", d?.string("target_partition") ?: "—")
            Status("OTA再起動待ち", if (d != null && d.string("current_slot") != d.string("next_boot_slot")) "Yes" else "No/未検査")
            Status("KernelSU Next", if (state.kernelsuPackages.isNotEmpty()) "Manager検出" else "確認できません")
            Status("KernelSUカーネル状態", d?.bool("kernelsu_next_present")?.let { if (it) "検出" else "確認できません" } ?: "—")
            Status("バッテリー", d?.string("battery_percent")?.let { "$it% / ${if (d.bool("charging") == true) "充電中" else "未充電"}" } ?: "—")
            Status("update_engine", d?.string("ota_status") ?: "利用不可")
            Status("vendor_bootリージョン", state.fdt?.string("region") ?: if (op?.bool("already_prc") == true) "PRC" else "—")
            Status("vendor_bootサイズ", d?.string("partition_size") ?: "—")
            Status("stock SHA-256", op?.string("input_sha256") ?: "—")
            Status("patched SHA-256", op?.string("output_sha256") ?: "—")
            Status("変更オフセット", op?.get("changed_offsets")?.toString() ?: "—")
            Status("バックアップ保存先", op?.string("backup_dir") ?: "—")
            Status("最終処理結果", op?.string("status") ?: "—")
        }
    }
}

@Composable
private fun Status(label: String, value: String) {
    Column {
        Text(label, style = MaterialTheme.typography.labelMedium, color = Color.Gray)
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

@Composable
private fun RecoveryScreen(state: UiState, restore: () -> Unit) {
    Column(
        Modifier
            .fillMaxSize()
            .padding(24.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        Text(
            "未完了の処理を検出しました",
            style = MaterialTheme.typography.headlineMedium,
            color = MaterialTheme.colorScheme.error,
        )
        Spacer(Modifier.height(16.dp))
        Text("再起動しないでください。journalの状態: ${state.recoveryJournal?.string("status")}")
        state.error?.let {
            Spacer(Modifier.height(12.dp))
            Text(it, color = MaterialTheme.colorScheme.error)
        }
        Spacer(Modifier.height(24.dp))
        Button(onClick = restore, enabled = !state.busy, modifier = Modifier.fillMaxWidth()) {
            Text(if (state.busy) "復元中…" else "stockバックアップを復元して全体検証")
        }
        Spacer(Modifier.height(12.dp))
        Text("復元に失敗した場合はFastbootまたはEDLで復旧が必要です。")
    }
}
