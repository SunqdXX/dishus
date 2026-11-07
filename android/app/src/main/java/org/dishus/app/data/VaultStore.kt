package org.dishus.app.data

import android.content.Context
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Persists the Rust-sealed vault blob (already Argon2id + XChaCha20-Poly1305) wrapped once more
 * with an AES-256-GCM key that never leaves the Android Keystore (StrongBox when available).
 * Copying the file off the device is therefore useless without this device's keystore.
 *
 * Optional biometric unlock stores the vault password encrypted with a second Keystore key that
 * requires a strong biometric for every use and is invalidated when biometrics change.
 */
class VaultStore(context: Context) {
    private val dir = context.noBackupFilesDir
    private val vaultFile = File(dir, "vault.bin")
    private val bioFile = File(dir, "bio.bin")
    private val ks = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }

    fun exists(): Boolean = vaultFile.exists()

    fun save(blob: ByteArray) {
        val c = Cipher.getInstance(TRANSFORM)
        c.init(Cipher.ENCRYPT_MODE, key(WRAP_ALIAS, biometric = false))
        writeAtomic(vaultFile, c.iv + c.doFinal(blob))
    }

    fun load(): ByteArray {
        val raw = vaultFile.readBytes()
        require(raw.size > IV_LEN + 16) { "vault file corrupt" }
        val c = Cipher.getInstance(TRANSFORM)
        c.init(Cipher.DECRYPT_MODE, key(WRAP_ALIAS, biometric = false), GCMParameterSpec(128, raw, 0, IV_LEN))
        return c.doFinal(raw, IV_LEN, raw.size - IV_LEN)
    }

    /** Delete everything: vault, biometric blob and both keys. */
    fun wipe() {
        vaultFile.delete()
        bioFile.delete()
        runCatching { ks.deleteEntry(WRAP_ALIAS) }
        runCatching { ks.deleteEntry(BIO_ALIAS) }
    }

    // ---- biometric ----

    fun biometricEnabled(): Boolean = bioFile.exists() && ks.containsAlias(BIO_ALIAS)

    /** Cipher to hand to BiometricPrompt when enrolling. */
    fun biometricEncryptCipher(): Cipher {
        runCatching { ks.deleteEntry(BIO_ALIAS) }
        return Cipher.getInstance(TRANSFORM).apply { init(Cipher.ENCRYPT_MODE, key(BIO_ALIAS, biometric = true)) }
    }

    fun biometricStore(authenticated: Cipher, password: String) {
        val pw = password.toByteArray(Charsets.UTF_8)
        try {
            writeAtomic(bioFile, authenticated.iv + authenticated.doFinal(pw))
        } finally {
            pw.fill(0)
        }
    }

    /** Cipher to hand to BiometricPrompt when unlocking, or null if not set up / invalidated. */
    fun biometricDecryptCipher(): Cipher? = runCatching {
        val raw = bioFile.readBytes()
        Cipher.getInstance(TRANSFORM).apply {
            init(Cipher.DECRYPT_MODE, ks.getKey(BIO_ALIAS, null) as SecretKey, GCMParameterSpec(128, raw, 0, IV_LEN))
        }
    }.getOrNull()

    fun biometricLoad(authenticated: Cipher): String {
        val raw = bioFile.readBytes()
        val pw = authenticated.doFinal(raw, IV_LEN, raw.size - IV_LEN)
        return try {
            String(pw, Charsets.UTF_8)
        } finally {
            pw.fill(0)
        }
    }

    fun disableBiometric() {
        bioFile.delete()
        runCatching { ks.deleteEntry(BIO_ALIAS) }
    }

    // ---- internals ----

    private fun key(alias: String, biometric: Boolean): SecretKey {
        (ks.getKey(alias, null) as? SecretKey)?.let { return it }
        fun spec(strongBox: Boolean) = KeyGenParameterSpec.Builder(
            alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        ).apply {
            setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            setKeySize(256)
            setRandomizedEncryptionRequired(true)
            if (strongBox) setIsStrongBoxBacked(true)
            if (biometric) {
                setUserAuthenticationRequired(true)
                setInvalidatedByBiometricEnrollment(true)
                if (Build.VERSION.SDK_INT >= 30) {
                    setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG)
                } else {
                    @Suppress("DEPRECATION")
                    setUserAuthenticationValidityDurationSeconds(-1)
                }
            }
        }.build()
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        return try {
            gen.init(spec(strongBox = true)); gen.generateKey()
        } catch (_: StrongBoxUnavailableException) {
            gen.init(spec(strongBox = false)); gen.generateKey()
        }
    }

    private fun writeAtomic(f: File, data: ByteArray) {
        val tmp = File(f.parentFile, f.name + ".tmp")
        tmp.outputStream().use { it.write(data); it.fd.sync() }
        if (!tmp.renameTo(f)) error("could not write ${f.name}")
    }

    companion object {
        private const val ANDROID_KEYSTORE = "AndroidKeyStore"
        private const val WRAP_ALIAS = "dishus_vault_wrap"
        private const val BIO_ALIAS = "dishus_vault_bio"
        private const val TRANSFORM = "AES/GCM/NoPadding"
        private const val IV_LEN = 12
    }
}
