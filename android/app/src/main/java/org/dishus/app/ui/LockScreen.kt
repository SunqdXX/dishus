package org.dishus.app.ui

import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricPrompt
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.dishus.app.DishusApp
import uniffi.dishus_core.DishusException
import javax.crypto.Cipher

@Composable
fun LockScreen(app: DishusApp, activity: FragmentActivity) {
    val creating = !app.vault.exists()
    var pw by remember { mutableStateOf("") }
    var pw2 by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    var offerBiometric by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val canBio = BiometricManager.from(activity).canAuthenticate(BIOMETRIC_STRONG) == BiometricManager.BIOMETRIC_SUCCESS

    fun finishUnlock(password: String) {
        busy = true; error = null
        scope.launch {
            val res = withContext(Dispatchers.Default) {
                runCatching {
                    if (creating) app.vault.save(app.core.createVault(password))
                    else app.core.unlock(password, app.vault.load())
                }
            }
            busy = false
            res.onSuccess {
                if (creating && canBio) offerBiometric = true else app.markUnlocked()
            }.onFailure { e ->
                error = when (e) {
                    is DishusException.WrongPassword -> "Wrong password"
                    is DishusException -> e.message ?: "Error"
                    else -> "Could not open the key store: ${e.message}"
                }
            }
        }
    }

    fun biometricUnlock() {
        val cipher = app.vault.biometricDecryptCipher()
        if (cipher == null) {
            error = "Biometric key was invalidated (enrolment changed). Use your password."
            app.vault.disableBiometric()
            return
        }
        prompt(activity, cipher, "Unlock DiShus") { c -> finishUnlock(app.vault.biometricLoad(c)) }
    }

    LaunchedEffect(Unit) {
        if (!creating && app.vault.biometricEnabled()) biometricUnlock()
    }

    Column(
        Modifier.fillMaxSize().safeDrawingPadding().padding(28.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("DiShus", style = MaterialTheme.typography.headlineLarge, color = MaterialTheme.colorScheme.primary)
        Spacer(Modifier.height(8.dp))
        if (offerBiometric) {
            Text("Also unlock with your fingerprint or face?", style = MaterialTheme.typography.bodyLarge)
            Spacer(Modifier.height(16.dp))
            Button(onClick = {
                val enc = app.vault.biometricEncryptCipher()
                prompt(activity, enc, "Enable biometric unlock") { c ->
                    app.vault.biometricStore(c, pw); pw = ""; pw2 = ""; app.markUnlocked()
                }
            }) { Text("Enable biometric unlock") }
            TextButton(onClick = { pw = ""; pw2 = ""; app.markUnlocked() }) { Text("Not now") }
            return@Column
        }
        Text(
            if (creating) "Choose an app password. It encrypts this phone's key; there is no recovery."
            else "Enter your app password",
            style = MaterialTheme.typography.bodyMedium,
        )
        Spacer(Modifier.height(20.dp))
        OutlinedTextField(
            value = pw, onValueChange = { pw = it }, label = { Text("Password") }, singleLine = true,
            visualTransformation = PasswordVisualTransformation(),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false),
            modifier = Modifier.fillMaxWidth(),
        )
        if (creating) {
            Spacer(Modifier.height(8.dp))
            OutlinedTextField(
                value = pw2, onValueChange = { pw2 = it }, label = { Text("Repeat password") }, singleLine = true,
                visualTransformation = PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false),
                modifier = Modifier.fillMaxWidth(),
            )
        }
        error?.let { Spacer(Modifier.height(8.dp)); Text(it, color = MaterialTheme.colorScheme.error) }
        Spacer(Modifier.height(20.dp))
        Button(
            enabled = !busy && pw.length >= 8 && (!creating || pw == pw2),
            onClick = { finishUnlock(pw) },
            modifier = Modifier.fillMaxWidth(),
        ) { Text(if (busy) "Deriving key…" else if (creating) "Create" else "Unlock") }
        if (creating && pw.isNotEmpty() && pw.length < 8) Text("At least 8 characters", style = MaterialTheme.typography.bodySmall)
        if (!creating && app.vault.biometricEnabled()) {
            OutlinedButton(onClick = { biometricUnlock() }, modifier = Modifier.fillMaxWidth()) { Text("Use biometrics") }
        }
    }
}

private fun prompt(activity: FragmentActivity, cipher: Cipher, title: String, onOk: (Cipher) -> Unit) {
    val p = BiometricPrompt(
        activity, ContextCompat.getMainExecutor(activity),
        object : BiometricPrompt.AuthenticationCallback() {
            override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                result.cryptoObject?.cipher?.let(onOk)
            }
        },
    )
    val info = BiometricPrompt.PromptInfo.Builder()
        .setTitle(title)
        .setNegativeButtonText("Use password")
        .setAllowedAuthenticators(BIOMETRIC_STRONG)
        .build()
    p.authenticate(info, BiometricPrompt.CryptoObject(cipher))
}
